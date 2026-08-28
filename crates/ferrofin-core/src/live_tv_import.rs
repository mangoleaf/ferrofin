//! Adoption import for Jellyfin's `config/livetv.xml`.
//!
//! Every other Jellyfin configuration document is carried across when a data
//! directory is adopted (see [`crate::config_import`]), but the Live TV one was
//! not — and Ferrofin keeps tuners and listing providers in the database rather
//! than in a config file, so nothing else would ever pick them up.
//!
//! The cost of skipping it is not cosmetic: with no tuner configured, Live TV
//! is *off* (C# `LiveTvManager.IsLiveTvEnabled`), so adopting a server that had
//! Live TV loses the guide, the tuner and the view — until an admin notices and
//! re-adds it by hand.

use ferrofin_db::Database;
use ferrofin_model::live_tv::LiveTvOptions;

use crate::config_import;
use ferrofin_traits::error::ServiceError;

/// The `FerrofinMeta` key recording that the import has run.
///
/// Once only: an operator who deletes an imported tuner means it, and a boot
/// that re-created it would be arguing with them.
const META_KEY: &str = "livetv_config_imported_v1";

/// Fields of `livetv.xml` that must not be carried over: all three are
/// recording paths inside the Jellyfin container's filesystem.
const LIVETV_XML_DENY: &[&str] = &["RecordingPath", "MovieRecordingPath", "SeriesRecordingPath"];

/// Imports the tuners and listing providers from `{config}/livetv.xml`, once.
///
/// Returns how many rows were written. A missing file, or a file that cannot be
/// read, still marks the import done — the marker means "we have looked", and
/// re-reading an unparseable file every boot only repeats the warning.
///
/// # Errors
///
/// Returns a [`ServiceError`] if the database rejects a write.
pub async fn import_live_tv_config(
    db: &Database,
    config_dir: &std::path::Path,
) -> Result<usize, ServiceError> {
    let done = db
        .meta_get(META_KEY)
        .await
        .map_err(|e| ServiceError::Backend(e.to_string()))?;
    if done.as_deref() == Some("1") {
        return Ok(0);
    }

    let written = match tokio::fs::read_to_string(config_dir.join("livetv.xml")).await {
        Ok(xml) => write_options(db, &xml).await?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            tracing::warn!(%e, "could not read livetv.xml; Live TV starts unconfigured");
            0
        }
    };
    db.meta_set(META_KEY, "1")
        .await
        .map_err(|e| ServiceError::Backend(e.to_string()))?;
    Ok(written)
}

/// Parses `xml` and writes what it names. Split out so the parse and the writes
/// are testable without a filesystem.
async fn write_options(db: &Database, xml: &str) -> Result<usize, ServiceError> {
    let options = match config_import::import_over(
        &LiveTvOptions::default(),
        xml,
        "LiveTvOptions",
        LIVETV_XML_DENY,
    ) {
        Ok(options) => options,
        Err(e) => {
            tracing::warn!(%e, "livetv.xml could not be read; Live TV starts unconfigured");
            return Ok(0);
        }
    };

    let mut written = 0;
    for tuner in &options.tuner_hosts {
        // A tuner with no URL is not a tuner: the column is NOT NULL, and
        // upstream's own `SaveTunerHost` rejects it.
        let (Some(id), Some(url)) = (tuner.id.as_deref(), tuner.url.as_deref()) else {
            tracing::warn!(
                tuner = ?tuner.friendly_name,
                "skipping a tuner from livetv.xml with no id or url"
            );
            continue;
        };
        let data = serde_json::to_string(tuner)
            .map_err(|e| ServiceError::backend(format!("serialize tuner host: {e}")))?;
        db.upsert_live_tv_tuner_host(id, url, tuner.type_.as_deref().unwrap_or("m3u"), &data)
            .await
            .map_err(ServiceError::from)?;
        written += 1;
    }
    for provider in &options.listing_providers {
        let Some(id) = provider.id.as_deref() else {
            tracing::warn!("skipping a listing provider from livetv.xml with no id");
            continue;
        };
        let data = serde_json::to_string(provider)
            .map_err(|e| ServiceError::backend(format!("serialize listing provider: {e}")))?;
        db.upsert_live_tv_listing_provider(
            id,
            provider.type_.as_deref().unwrap_or("xmltv"),
            provider.path.as_deref().unwrap_or_default(),
            &data,
        )
        .await
        .map_err(ServiceError::from)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;

    /// A `livetv.xml` in the shape Jellyfin writes it — the `TunerHostInfo` /
    /// `ListingsProviderInfo` element wrappers and all.
    const REAL_LIVETV_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<LiveTvOptions xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <GuideDays>7</GuideDays>
  <RecordingPath>/config/recordings</RecordingPath>
  <EnableRecordingSubfolders>true</EnableRecordingSubfolders>
  <TunerHosts>
    <TunerHostInfo>
      <Id>4a1f2b</Id>
      <Url>http://tuner.lan/playlist.m3u</Url>
      <Type>m3u</Type>
      <FriendlyName>Attic aerial</FriendlyName>
      <AllowHWTranscoding>true</AllowHWTranscoding>
      <TunerCount>2</TunerCount>
    </TunerHostInfo>
  </TunerHosts>
  <ListingProviders>
    <ListingsProviderInfo>
      <Id>9c0d1e</Id>
      <Type>xmltv</Type>
      <Path>/config/guide.xml</Path>
    </ListingsProviderInfo>
  </ListingProviders>
  <PrePaddingSeconds>60</PrePaddingSeconds>
</LiveTvOptions>"#;

    /// The tuner and the guide come across, so an adopted server has Live TV
    /// rather than silently losing it.
    #[tokio::test]
    async fn the_tuner_and_the_listing_provider_are_imported() {
        let db = test_db().await;
        assert_eq!(
            write_options(&db, REAL_LIVETV_XML).await.expect("import"),
            2
        );

        let tuners = db.live_tv_tuner_hosts().await.expect("tuners");
        let [(id, url, kind, data)] = tuners.as_slice() else {
            panic!("expected exactly one tuner, got {tuners:?}");
        };
        assert_eq!(id, "4a1f2b");
        assert_eq!(url, "http://tuner.lan/playlist.m3u");
        assert_eq!(kind, "m3u");
        // …and the whole DTO round-trips, not just the columns.
        let tuner: ferrofin_model::live_tv::TunerHostInfo =
            serde_json::from_str(data).expect("parses");
        assert_eq!(tuner.friendly_name.as_deref(), Some("Attic aerial"));
        assert!(tuner.allow_hw_transcoding, "a true element is carried");
        assert_eq!(tuner.tuner_count, 2);

        let providers = db.live_tv_listing_providers().await.expect("providers");
        assert_eq!(
            providers,
            [(
                "9c0d1e".to_owned(),
                "xmltv".to_owned(),
                "/config/guide.xml".to_owned()
            )]
        );
    }

    /// Once only: an operator who removes an imported tuner is not overruled on
    /// the next boot.
    #[tokio::test]
    async fn the_import_runs_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("livetv.xml"), REAL_LIVETV_XML).expect("write");
        let db = test_db().await;

        assert_eq!(
            import_live_tv_config(&db, dir.path())
                .await
                .expect("import"),
            2
        );
        db.delete_all_live_tv_tuner_hosts()
            .await
            .expect("operator removes it");
        assert_eq!(
            import_live_tv_config(&db, dir.path())
                .await
                .expect("import"),
            0,
            "the second boot does not put it back"
        );
        assert_eq!(db.live_tv_tuner_count().await.expect("count"), 0);
    }

    /// A server that never had Live TV is not a failure, and is not retried
    /// forever either.
    #[tokio::test]
    async fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = test_db().await;
        assert_eq!(
            import_live_tv_config(&db, dir.path())
                .await
                .expect("import"),
            0
        );
    }
}
