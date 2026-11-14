//! Port of `MediaBrowser.Controller.SystemBackupService`'s DTOs: the manifest
//! a backup archive carries and the options a backup is created with.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The manifest entry stored inside each archive (as `manifest.json`) and returned
/// by the list/create/inspect routes. Port of `BackupManifestDto`.
///
/// The fields keep `BackupManifestDto`'s declaration order, which is the order
/// System.Text.Json writes them in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BackupManifest {
    /// The server version that wrote it.
    pub server_version: String,
    /// The archive-layout version (the engine's `BACKUP_ENGINE_VERSION`).
    /// Defaulted on read so archives written before the field existed (same
    /// layout) still list.
    #[serde(default)]
    pub backup_engine_version: String,
    /// When the backup was created: a `DateTimeOffset` upstream, so written
    /// with an explicit offset (`date_time_offset`).
    #[serde(with = "date_time_offset")]
    pub date_created: DateTime<Utc>,
    /// The archive's absolute path (Jellyfin reports the full path; its file
    /// name is the handle Restore / Manifest accept). Always re-derived from the
    /// on-disk location on read — and defaulted, because a Jellyfin manifest
    /// carries no `Path` at all and must still parse so the engine can refuse
    /// it (and the listing can show it, as Jellyfin lists foreign archives).
    #[serde(default)]
    pub path: String,
    /// The options the backup was created with (echoed back).
    pub options: BackupOptions,
}

/// System.Text.Json's own `DateTimeOffset` format (Jellyfin registers no
/// converter for it) over a UTC value: the fraction loses its trailing zeros,
/// and goes when it is zero, and the offset is `+00:00`, never `Z`. Reads any
/// ISO-8601 form, so archives written with the `Z` form still list.
mod date_time_offset {
    use chrono::{DateTime, Utc};
    use serde::Serializer;

    pub(super) use crate::json::datetime::deserialize;

    /// Writes `value` as `Utf8JsonWriter.WriteStringValue(DateTimeOffset)` does.
    pub(super) fn serialize<S: Serializer>(
        value: &DateTime<Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut text = value.format("%Y-%m-%dT%H:%M:%S").to_string();
        let ticks = value.timestamp_subsec_nanos() / 100;
        if ticks != 0 {
            text.push('.');
            text.push_str(format!("{ticks:07}").trim_end_matches('0'));
        }
        text.push_str("+00:00");
        serializer.serialize_str(&text)
    }
}

/// The `POST /Backup/Create` body — which sections to include. Port of
/// `BackupOptionsDto` with its defaults: only the database is on unless asked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)] // the contract's DTO is four flags
pub struct BackupOptions {
    /// Whether the internal metadata trees are archived.
    pub metadata: bool,
    /// Whether the trickplay tree is archived.
    pub trickplay: bool,
    /// Whether the subtitle cache is archived.
    pub subtitles: bool,
    /// Whether the database is archived.
    pub database: bool,
}

impl Default for BackupOptions {
    fn default() -> Self {
        Self {
            metadata: false,
            trickplay: false,
            subtitles: false,
            database: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BackupManifest, BackupOptions};

    /// `BackupManifestDto.DateCreated` is a `DateTimeOffset`: written with
    /// `+00:00` and System.Text.Json's trimmed fraction, read back from that
    /// form and from the `Z` form older Ferrofin archives carry.
    #[test]
    fn manifest_date_created_is_written_as_a_date_time_offset() {
        let manifest = |date: &str| BackupManifest {
            path: String::new(),
            date_created: date.parse().unwrap(),
            server_version: String::new(),
            backup_engine_version: String::new(),
            options: BackupOptions::default(),
        };
        for (stored, written) in [
            (
                "2026-10-23T22:07:24.2746138Z",
                "2026-10-23T22:07:24.2746138+00:00",
            ),
            ("2026-10-23T22:07:24.5Z", "2026-10-23T22:07:24.5+00:00"),
            ("2024-01-02T03:04:05Z", "2024-01-02T03:04:05+00:00"),
        ] {
            let json = serde_json::to_value(manifest(stored)).unwrap();
            assert_eq!(json["DateCreated"], written);
            let back: BackupManifest = serde_json::from_value(json).unwrap();
            assert_eq!(back.date_created, manifest(stored).date_created);
        }
        let old = r#"{"DateCreated":"2024-01-02T03:04:05.0000000Z","ServerVersion":"1.0.0","Options":{}}"#;
        let read: BackupManifest = serde_json::from_str(old).unwrap();
        assert_eq!(
            read.date_created,
            manifest("2024-01-02T03:04:05Z").date_created
        );
    }

    /// `BackupManifestDto` declares `ServerVersion, BackupEngineVersion,
    /// DateCreated, Path, Options`, and System.Text.Json writes that order
    /// (Jellyfin 12.2's `POST /Backup/Create` answers it).
    #[test]
    fn manifest_members_come_out_in_upstreams_order() {
        let manifest = BackupManifest {
            path: "/b/x.zip".to_owned(),
            date_created: "2024-01-02T03:04:05Z".parse().unwrap(),
            server_version: "1.4.0".to_owned(),
            backup_engine_version: "1.0.0".to_owned(),
            options: BackupOptions::default(),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let at: Vec<usize> = [
            "\"ServerVersion\"",
            "\"BackupEngineVersion\"",
            "\"DateCreated\"",
            "\"Path\"",
            "\"Options\"",
        ]
        .iter()
        .map(|key| json.find(key).expect(key))
        .collect();
        assert!(at.is_sorted(), "{json}");
    }
}
