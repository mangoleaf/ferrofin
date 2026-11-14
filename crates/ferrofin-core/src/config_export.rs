//! Ferrofin's configuration documents as the XML Jellyfin reads: the reverse
//! of [`crate::config_import`], for the export of a data directory back to
//! Jellyfin.
//!
//! Each document is read as the server reads it (a named document never
//! saved, or one that does not parse, is its default) and written in the
//! shape .NET's `XmlSerializer` writes and reads (`config_export::xml`). A Jellyfin
//! that boots on the result serves the same `GET /System/Configuration*`
//! values Ferrofin served.
//!
//! A setting the import refuses to carry in (a path in the other server's
//! filesystem, a plugin repository, Jellyfin's own bookkeeping) is not carried
//! out either: Jellyfin is given the default instead. The default, not a gap:
//! Jellyfin's own repository reaches a server through a one-time migration
//! that does not run again, so a document without `PluginRepositories` would
//! leave Jellyfin with none to reinstall a plugin from. A Ferrofin-only setting
//! has nowhere to go and is left out. Both are listed in
//! [`ConfigExport::not_exported`] whenever Ferrofin's value is not the default,
//! for the export's loss report, except `EncoderAppPathDisplay`, which each
//! server fills in at startup from its own ffmpeg. The list names settings,
//! never their values, as a certificate password is one of them.
//!
//! `database.xml` is not written: Ferrofin is SQLite-only, and Jellyfin writes
//! its SQLite default itself.
//!
//! Each library's options go the same way ([`export_library_options`]): its
//! `options.json`, read as the server reads it, as the `options.xml` beside it,
//! with its media paths stored as Jellyfin stores them.

mod branding;
mod encoding;
mod library;
mod metadata;
mod network;
mod system;
mod xbmcmetadata;
mod xml;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use ferrofin_model::branding::BrandingOptions;
use ferrofin_model::configuration::{
    EncodingOptions, LibraryOptions, MetadataConfiguration, XbmcMetadataOptions,
};
use ferrofin_networking::NetworkConfiguration;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::system::ServerApplicationPaths;

use crate::app_paths::FerrofinServerApplicationPaths;
use crate::configuration_manager::FerrofinServerConfigurationManager as Manager;
use crate::virtual_folder_manager::FerrofinVirtualFolderManager;
use crate::virtual_paths::VirtualPathExpander;

/// One file of the export, relative to Jellyfin's program data directory
/// (`config/system.xml`, `data/device.txt`, `root/default/Movies/options.xml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedFile {
    /// Where the file goes, relative to Jellyfin's program data directory.
    pub path: String,
    /// The file's contents.
    pub contents: String,
}

/// A setting the export leaves to Jellyfin's default, for the loss report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotExported {
    /// The document it belongs to (`config/network.xml`).
    pub file: String,
    /// The setting's name (`CertificatePath`).
    pub setting: String,
    /// Why Jellyfin is not given Ferrofin's value.
    pub reason: &'static str,
}

/// The configuration export: the files to write, and what they leave out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigExport {
    /// The files to write: the XML documents and the server id file, or each
    /// library's `options.xml`.
    pub files: Vec<ExportedFile>,
    /// The settings whose Ferrofin value Jellyfin is not given.
    pub not_exported: Vec<NotExported>,
}

/// How one document is written.
struct Document {
    /// The document element (`ServerConfiguration`).
    root: &'static str,
    /// Settings the import does not carry in, so the export does not carry
    /// them out either and writes the default: the import's deny list for the
    /// document.
    written_as_default: &'static [&'static str],
    /// Settings Jellyfin's class does not have, as paths
    /// (`TrickplayOptions.FullChroma`): left out.
    ferrofin_only: &'static [&'static str],
    /// The element `XmlSerializer` names each entry of a collection of
    /// objects, keyed by the collection.
    entries: &'static [(&'static str, &'static str)],
}

/// Exports the configuration of the Ferrofin data directory whose config
/// directory (`{data}/config`) is `config_dir`. Nothing in it is changed.
///
/// # Errors
///
/// Returns [`ServiceError::Backend`] when `system.json` or `system_id` is
/// missing (Ferrofin has never served the directory) or does not parse, when a
/// file cannot be read, or when a value cannot be written as XML.
pub fn export_configuration(config_dir: &Path) -> Result<ConfigExport, ServiceError> {
    // `{config}/users/named/`, where `FerrofinServerApplicationPaths` puts the
    // user configuration directory and the manager the named documents.
    let named = config_dir.join("users").join(Manager::NAMED_SUBDIR);
    let mut export = ConfigExport::default();
    export_document(
        system::FILE,
        &system::DOCUMENT,
        &system::read(config_dir)?,
        &crate::configuration_manager::default_server_configuration(),
        &mut export,
    )?;
    export_named::<EncodingOptions>(
        encoding::FILE,
        &encoding::DOCUMENT,
        &named,
        Manager::ENCODING_FILE_NAME,
        &mut export,
    )?;
    export_named::<BrandingOptions>(
        branding::FILE,
        &branding::DOCUMENT,
        &named,
        Manager::BRANDING_FILE_NAME,
        &mut export,
    )?;
    export_named::<NetworkConfiguration>(
        network::FILE,
        &network::DOCUMENT,
        &named,
        Manager::NETWORK_FILE_NAME,
        &mut export,
    )?;
    export_named::<MetadataConfiguration>(
        metadata::FILE,
        &metadata::DOCUMENT,
        &named,
        Manager::METADATA_FILE_NAME,
        &mut export,
    )?;
    export_named::<XbmcMetadataOptions>(
        xbmcmetadata::FILE,
        &xbmcmetadata::DOCUMENT,
        &named,
        Manager::XBMC_METADATA_FILE_NAME,
        &mut export,
    )?;
    export.files.push(server_id(config_dir)?);
    Ok(export)
}

/// Exports the options of every library of the Ferrofin data directory
/// `program_data_dir`, whose config directory is `config_dir`, as the
/// `root/default/<library>/options.xml` Jellyfin reads. Nothing in it is
/// changed.
///
/// A library is a directory under `root/default`, or a link to one, as the
/// server lists them. Its options are read as the server reads them
/// (`FerrofinVirtualFolderManager::read_options`): its `options.json`, else
/// the adopted `options.xml` Ferrofin has not converted yet, else the
/// defaults. Each media path is stored as `CollectionFolder.SaveLibraryOptions`
/// stores it, through `ReverseVirtualPath`. A path in the data or metadata
/// directory becomes `%AppDataPath%/…` or `%MetadataPath%/…` (the Collections
/// library's own folder is one), so it follows Jellyfin's directories.
///
/// `program_data_dir` must be the path the server ran with (its `--data-dir`),
/// as Jellyfin's own `ReverseVirtualPath` uses its running paths: Ferrofin
/// stores media paths resolved, so a data directory read from another mount
/// point leaves the Collections folder as an absolute path Jellyfin may not
/// have.
///
/// # Errors
///
/// Returns [`ServiceError::Backend`] when `system.json` (which names the
/// metadata directory) is missing or does not parse, when the library root
/// cannot be listed, when a library folder's name is not UTF-8, or when a
/// value cannot be written as XML.
pub async fn export_library_options(
    program_data_dir: &Path,
    config_dir: &Path,
) -> Result<ConfigExport, ServiceError> {
    // Only the data, metadata and library directories are read; the log,
    // cache and web directories are there because the constructor takes them.
    let paths = FerrofinServerApplicationPaths::new(
        program_data_dir,
        program_data_dir.join("log"),
        config_dir,
        program_data_dir.join("cache"),
        program_data_dir.join("web"),
    );
    // `%MetadataPath%` stands for the configured metadata directory, as the
    // server's load sets it.
    paths.set_internal_metadata_path(Some(&system::read(config_dir)?.metadata_path));
    let root = PathBuf::from(paths.default_user_views_path());
    let expander = VirtualPathExpander::from_paths(Arc::new(paths));
    let list_error =
        |e: std::io::Error| ServiceError::Backend(format!("list {}: {e}", root.display()));
    let mut libraries = Vec::new();
    match tokio::fs::read_dir(&root).await {
        Ok(mut entries) => {
            while let Some(entry) = entries.next_entry().await.map_err(list_error)? {
                // `metadata` follows a link, as the server's listing does.
                if tokio::fs::metadata(entry.path())
                    .await
                    .is_ok_and(|m| m.is_dir())
                {
                    libraries.push(entry.file_name());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(list_error(e)),
    }
    libraries.sort();
    let mut export = ConfigExport::default();
    for name in libraries {
        let folder = root.join(&name);
        let name = name.into_string().map_err(|name| {
            ServiceError::Backend(format!(
                "the library folder {} has a name that is not UTF-8",
                Path::new(&name).display()
            ))
        })?;
        let mut options = FerrofinVirtualFolderManager::read_options(&folder, &expander).await;
        for info in &mut options.path_infos {
            if !info.path.is_empty() {
                info.path = expander.reverse(&info.path);
            }
        }
        export_document(
            &format!("root/default/{name}/options.xml"),
            &library::DOCUMENT,
            &options,
            &LibraryOptions::default(),
            &mut export,
        )
        .map_err(|e| match e {
            ServiceError::Backend(message) => {
                ServiceError::Backend(format!("library {name}: {message}"))
            }
            other => other,
        })?;
    }
    Ok(export)
}

/// Exports the named document stored as `named/{file}`.
fn export_named<T: Serialize + DeserializeOwned + Default>(
    path: &str,
    document: &Document,
    named: &Path,
    file: &str,
    export: &mut ConfigExport,
) -> Result<(), ServiceError> {
    export_document(
        path,
        document,
        &read_named::<T>(&named.join(file))?,
        &T::default(),
        export,
    )
}

/// Reads a named document as the server does
/// (`FerrofinServerConfigurationManager::read_named`): one never saved, or
/// one that does not parse, is the default.
fn read_named<T: DeserializeOwned + Default>(path: &Path) -> Result<T, ServiceError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "a saved configuration file is not valid; exporting its defaults, as the server serves them"
            );
            T::default()
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(ServiceError::Backend(format!("read {}: {e}", path.display()))),
    }
}

/// Writes `value` as `document` at `path`, listing what it leaves out against
/// `default`.
fn export_document<T: Serialize>(
    path: &str,
    document: &Document,
    value: &T,
    default: &T,
    export: &mut ConfigExport,
) -> Result<(), ServiceError> {
    let mut fields = object(value)?;
    let mut defaults = object(default)?;
    let mut left_out = |setting: &str, ours: Option<Value>, default: Option<&Value>| {
        if let Some(reason) = reason(document, setting)
            && ours.as_ref() != default
        {
            export.not_exported.push(NotExported {
                file: path.to_owned(),
                setting: setting.to_owned(),
                reason,
            });
        }
    };
    for member in document.ferrofin_only {
        let ours = take(&mut fields, member);
        let default = take(&mut defaults, member);
        left_out(member, ours, default.as_ref());
    }
    for setting in document.written_as_default {
        let default = defaults.get(*setting);
        let ours = match default {
            Some(value) => fields.insert((*setting).to_owned(), value.clone()),
            None => fields.remove(*setting),
        };
        left_out(setting, ours, default);
    }
    mark_cleared(&mut fields, &defaults);
    export.files.push(ExportedFile {
        path: path.to_owned(),
        contents: xml::document(document.root, &fields, document.entries)?,
    });
    Ok(())
}

/// Writes a member the default sets but Ferrofin's value clears (an `Option`
/// serialized as absent) as `null`, which the writer gives `xsi:nil`.
/// `XmlSerializer` keeps only a null it is told about: a member left out
/// reads as its constructor default, so `ActivityLogRetentionDays` cleared in
/// Ferrofin would come back as 30 days in Jellyfin. `XmlSerializer` honours
/// `xsi:nil` on a nullable value type; on a string it reads the empty element
/// as `""` (checked on Jellyfin 12.4 with `VaapiDevice`). Jellyfin cannot keep a
/// null in such a string itself: it leaves a null string out of the file, which
/// reads back as the constructor default.
///
/// Safe only while every `Option` in the six documents is a string or a
/// nullable value type upstream (today: strings and the `int?`
/// `ActivityLogRetentionDays`). `xsi:nil` on a non-nullable value type would
/// make Jellyfin refuse the whole document.
fn mark_cleared(fields: &mut Map<String, Value>, defaults: &Map<String, Value>) {
    for (name, default) in defaults {
        match (fields.get_mut(name), default) {
            (None, _) => {
                fields.insert(name.clone(), Value::Null);
            }
            (Some(Value::Object(ours)), Value::Object(default)) => mark_cleared(ours, default),
            (Some(_), _) => {}
        }
    }
}

/// Removes and returns the member at `path` (`TrickplayOptions.FullChroma`).
fn take(fields: &mut Map<String, Value>, path: &str) -> Option<Value> {
    match path.split_once('.') {
        None => fields.remove(path),
        Some((head, rest)) => match fields.get_mut(head) {
            Some(Value::Object(inner)) => take(inner, rest),
            _ => None,
        },
    }
}

/// Why Jellyfin is not given Ferrofin's value of `setting`, or `None` when
/// that is no loss at all.
fn reason(document: &Document, setting: &str) -> Option<&'static str> {
    if document.ferrofin_only.contains(&setting) {
        return Some("Jellyfin has no such setting");
    }
    match setting {
        // Each server fills it in at startup from the ffmpeg it finds.
        "EncoderAppPathDisplay" => None,
        "PluginRepositories" => {
            Some("Ferrofin's plugin repositories serve WASM plugins, which Jellyfin cannot load")
        }
        "PreviousVersion" | "PreviousVersionStr" => {
            Some("Jellyfin keeps its own version bookkeeping")
        }
        "CertificatePassword" => Some("it unlocks the certificate, whose path is not exported"),
        "MetadataPath"
        | "CachePath"
        | "TranscodingTempPath"
        | "EncoderAppPath"
        | "FallbackFontPath"
        | "SplashscreenLocation"
        | "CertificatePath" => {
            Some("a path in Ferrofin's environment; Jellyfin is given the default")
        }
        _ => Some("the import does not carry it in either; Jellyfin is given the default"),
    }
}

/// `value` as a JSON object.
fn object<T: Serialize>(value: &T) -> Result<Map<String, Value>, ServiceError> {
    match serde_json::to_value(value) {
        Ok(Value::Object(fields)) => Ok(fields),
        Ok(_) => Err(ServiceError::Backend(
            "configuration does not serialize to a JSON object".to_owned(),
        )),
        Err(e) => Err(ServiceError::Backend(format!(
            "serialize configuration: {e}"
        ))),
    }
}

/// The server id as Jellyfin's `{DataPath}/device.txt`, which Jellyfin reads
/// as its `SystemId` (`DeviceId.GetCachedId`), so paired clients still know
/// the server.
fn server_id(config_dir: &Path) -> Result<ExportedFile, ServiceError> {
    let path = config_dir.join("system_id");
    let id = std::fs::read_to_string(&path)
        .map_err(|e| ServiceError::Backend(format!("read {}: {e}", path.display())))?;
    // Trimmed, as the server reads it (`load_server_id`).
    let id = id.trim();
    // `Guid.TryParse` takes the N, D and B forms (32, 36 and 38 characters),
    // not the URN form `Uuid` also accepts.
    if uuid::Uuid::try_parse(id).is_err() || !matches!(id.len(), 32 | 36 | 38) {
        return Err(ServiceError::Backend(format!(
            "{} does not hold a server id",
            path.display()
        )));
    }
    Ok(ExportedFile {
        path: "data/device.txt".to_owned(),
        contents: id.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ferrofin_model::configuration::ServerConfiguration;
    use serde_json::json;

    use super::*;
    use crate::config_import::{self, Node};

    const SERVER_ID: &str = "3f25fe64f745407b88841fa775eef31b";

    /// A Ferrofin config directory holding `system.json` and `system_id`,
    /// and the named documents given.
    fn config_dir(system: &ServerConfiguration, named: &[(&str, Value)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("system.json"),
            serde_json::to_vec(system).expect("serialize"),
        )
        .expect("write system.json");
        std::fs::write(dir.path().join("system_id"), format!("{SERVER_ID}\n")).expect("write id");
        let named_dir = dir.path().join("users").join("named");
        std::fs::create_dir_all(&named_dir).expect("named dir");
        for (file, value) in named {
            std::fs::write(named_dir.join(file), value.to_string()).expect("write named");
        }
        dir
    }

    fn file<'a>(export: &'a ConfigExport, path: &str) -> &'a str {
        &export
            .files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("{path} is exported"))
            .contents
    }

    /// The documents Jellyfin 12.4 wrote itself for a fresh install, after
    /// saving each one once (`config_export/testdata/`, written by `jellyfin/jellyfin:12.4`).
    fn golden(name: &str) -> &'static str {
        match name {
            "system" => include_str!("config_export/testdata/system.xml"),
            "encoding" => include_str!("config_export/testdata/encoding.xml"),
            "branding" => include_str!("config_export/testdata/branding.xml"),
            "network" => include_str!("config_export/testdata/network.xml"),
            "metadata" => include_str!("config_export/testdata/metadata.xml"),
            "xbmcmetadata" => include_str!("config_export/testdata/xbmcmetadata.xml"),
            other => panic!("no golden document {other}"),
        }
    }

    /// A document's members by name, each as a comparable tree.
    fn members(xml: &str) -> (String, BTreeMap<String, Node>) {
        let root = config_import::parse(xml).expect("parses");
        let members = root
            .children
            .iter()
            .map(|c| (c.name.clone(), canonical(c)))
            .collect();
        (root.name, members)
    }

    /// `node` with its members in name order, which `XmlSerializer` does not
    /// care about. The sort is stable, so the entries of a collection, which
    /// share a name, keep the order that does matter.
    fn canonical(node: &Node) -> Node {
        let mut children: Vec<Node> = node.children.iter().map(canonical).collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        Node {
            children,
            ..node.clone()
        }
    }

    /// Ferrofin's default documents, exported, have the shape Jellyfin 12.4
    /// gives the same defaults: the same root, the same members, the same
    /// entry names and value spellings, the defaults written for the settings
    /// left out included (`PluginRepositories` is Jellyfin's own repository).
    /// The only member added is one `XmlSerializer` omits while it holds its
    /// `[DefaultValue]`. A setting Ferrofin adds that Jellyfin lacks, and that
    /// is written at its default, fails here until it is listed as
    /// Ferrofin-only.
    #[test]
    fn default_documents_have_the_shape_jellyfin_writes() {
        // The state the golden server was in: its wizard done, its port
        // authorized at startup (as Ferrofin's own load also sets it).
        let mut system = crate::configuration_manager::default_server_configuration();
        system.is_startup_wizard_completed = true;
        system.is_port_authorized = true;
        // Both servers default it to 100 per core; the golden server had 32.
        system.cache_size = 3200;
        let dir = config_dir(&system, &[]);
        let export = export_configuration(dir.path()).expect("exports");
        let documents = [
            ("system", system::FILE),
            ("encoding", encoding::FILE),
            ("branding", branding::FILE),
            ("network", network::FILE),
            ("metadata", metadata::FILE),
            ("xbmcmetadata", xbmcmetadata::FILE),
        ];
        for (name, path) in documents {
            let (root, ours) = members(file(&export, path));
            let (jellyfin_root, mut theirs) = members(golden(name));
            assert_eq!(root, jellyfin_root, "{name}: root element");
            let mut ours = ours;
            if name == "encoding" {
                // Each server fills it in at startup with the ffmpeg it found;
                // Ferrofin's default has none, so nothing is written.
                assert_eq!(ours.remove("EncoderAppPathDisplay"), None);
                theirs.remove("EncoderAppPathDisplay");
                // `[DefaultValue(TrimCopiedAudio)]`: written by us, omitted by
                // `XmlSerializer` while it holds that value. Both read the same.
                let seek = ours.remove("HlsAudioSeekStrategy").expect("written");
                assert_eq!(seek.text, "TrimCopiedAudio");
            }
            let missing: Vec<_> = theirs.keys().filter(|k| !ours.contains_key(*k)).collect();
            let extra: Vec<_> = ours.keys().filter(|k| !theirs.contains_key(*k)).collect();
            assert!(
                missing.is_empty(),
                "{name}: Jellyfin writes {missing:?}, the export does not"
            );
            assert!(
                extra.is_empty(),
                "{name}: the export writes {extra:?}, Jellyfin has no such member"
            );
            let differ: Vec<_> = ours
                .iter()
                .filter(|(member, node)| theirs.get(*member) != Some(*node))
                .map(|(member, node)| (member, node, theirs.get(member)))
                .collect();
            assert!(
                differ.is_empty(),
                "{name}: members that differ: {differ:#?}"
            );
        }
    }

    /// A changed document goes to XML and back through the import unchanged,
    /// apart from the settings the export leaves out, which come back as the
    /// defaults Jellyfin would use.
    #[test]
    fn a_changed_document_round_trips_through_the_import() {
        let mut system = crate::configuration_manager::default_server_configuration();
        system.server_name = "Living room".to_owned();
        system.metadata_path = "/media/metadata".to_owned();
        system.cache_path = Some("/media/cache".to_owned());
        system.sort_remove_words = vec!["the".to_owned(), "a <b> & c".to_owned()];
        system.metadata_options.truncate(2);
        system.metadata_options[0].disabled_metadata_fetchers = vec!["TheMovieDb".to_owned()];
        system.trickplay_options.width_resolutions = vec![320, 640];
        // Cleared where the default sets it: Jellyfin must read null, not 30.
        system.activity_log_retention_days = None;
        system.content_types =
            serde_json::from_value(json!([{"Name": "Films", "Value": "movies"}])).expect("pairs");
        system.path_substitutions =
            serde_json::from_value(json!([{"From": "/mnt/a", "To": "/media/a"}])).expect("pairs");
        let named = [
            (
                "encoding.json",
                json!({"HardwareAccelerationType": "nvenc", "DownMixAudioBoost": 1.5, "EncoderPreset": "veryslow", "TranscodingTempPath": "/transcode", "HardwareDecodingCodecs": ["h264"], "VaapiDevice": null}),
            ),
            (
                "branding.json",
                json!({"LoginDisclaimer": "Members only", "CustomCss": "body { color: red; }"}),
            ),
            (
                "network.json",
                json!({"KnownProxies": ["10.0.0.1"], "InternalHttpPort": 8097, "CertificatePath": "/certs/a.pfx", "CertificatePassword": "secret"}),
            ),
            (
                "metadata.json",
                json!({"UseFileCreationTimeForDateAdded": false}),
            ),
            (
                "xbmcmetadata.json",
                json!({"ReleaseDateFormat": "dd-MM-yyyy", "UserId": "6d0b2c3c3b8c4d5e8f9a0b1c2d3e4f50"}),
            ),
        ];
        let dir = config_dir(&system, &named);
        let export = export_configuration(dir.path()).expect("exports");

        let back: ServerConfiguration = config_import::import_over(
            &crate::configuration_manager::default_server_configuration(),
            file(&export, "config/system.xml"),
            "ServerConfiguration",
            &[],
        )
        .expect("imports");
        let mut expected = system.clone();
        let defaults = crate::configuration_manager::default_server_configuration();
        expected.metadata_path = defaults.metadata_path;
        expected.cache_path = defaults.cache_path;
        assert_eq!(back, expected, "system.xml round trip");

        round_trip::<EncodingOptions>(
            &export,
            "config/encoding.xml",
            "EncodingOptions",
            &named[0].1,
            &["TranscodingTempPath"],
        );
        round_trip::<BrandingOptions>(
            &export,
            "config/branding.xml",
            "BrandingOptions",
            &named[1].1,
            &[],
        );
        round_trip::<NetworkConfiguration>(
            &export,
            "config/network.xml",
            "NetworkConfiguration",
            &named[2].1,
            &["CertificatePath", "CertificatePassword"],
        );
        round_trip::<MetadataConfiguration>(
            &export,
            "config/metadata.xml",
            "MetadataConfiguration",
            &named[3].1,
            &[],
        );
        round_trip::<XbmcMetadataOptions>(
            &export,
            "config/xbmcmetadata.xml",
            "XbmcMetadataOptions",
            &named[4].1,
            &[],
        );
    }

    fn round_trip<T>(
        export: &ConfigExport,
        path: &str,
        root: &str,
        saved: &Value,
        left_out: &[&str],
    ) where
        T: Serialize + DeserializeOwned + Default + PartialEq + std::fmt::Debug,
    {
        let mut saved = saved.as_object().expect("object").clone();
        for setting in left_out {
            saved.remove(*setting);
        }
        let expected: T = serde_json::from_value(Value::Object(saved)).expect("a valid document");
        let back: T = config_import::import_over(&T::default(), file(export, path), root, &[])
            .expect("imports");
        assert_eq!(back, expected, "{path} round trip");
    }

    /// Every setting left out with a changed value is reported, by name and
    /// never by value; one at its default is not.
    #[test]
    fn settings_left_out_are_reported_by_name() {
        let mut system = crate::configuration_manager::default_server_configuration();
        system.cache_path = Some("/media/cache".to_owned());
        system.plugin_repositories[0].url =
            Some("https://plugins.example/wasm/manifest.json".to_owned());
        system.previous_version_str = Some("12.0.0".to_owned());
        system.trickplay_options.full_chroma = true;
        let named = [
            (
                "network.json",
                json!({"CertificatePath": "/certs/a.pfx", "CertificatePassword": "secret"}),
            ),
            // Not reported: Jellyfin fills it in from its own ffmpeg.
            (
                "encoding.json",
                json!({"EncoderAppPathDisplay": "/usr/bin/ffmpeg"}),
            ),
        ];
        let dir = config_dir(&system, &named);
        let export = export_configuration(dir.path()).expect("exports");
        let reported: Vec<_> = export
            .not_exported
            .iter()
            .map(|n| (n.file.as_str(), n.setting.as_str()))
            .collect();
        assert_eq!(
            reported,
            [
                ("config/system.xml", "TrickplayOptions.FullChroma"),
                ("config/system.xml", "CachePath"),
                ("config/system.xml", "PluginRepositories"),
                ("config/system.xml", "PreviousVersionStr"),
                ("config/network.xml", "CertificatePath"),
                ("config/network.xml", "CertificatePassword"),
            ]
        );
        assert!(export.not_exported.iter().all(|n| !n.reason.is_empty()));
        let everything = format!("{export:?}");
        assert!(
            !everything.contains("secret"),
            "the password is in the export"
        );
        // Left out means given the default: an empty path, and Jellyfin's own
        // plugin repository rather than none at all.
        assert!(file(&export, "config/network.xml").contains("<CertificatePath />"));
        let system_xml = file(&export, "config/system.xml");
        assert!(
            system_xml.contains("https://repo.jellyfin.org/files/plugin/manifest.json"),
            "{system_xml}"
        );
        assert!(!system_xml.contains("plugins.example"), "{system_xml}");
        assert!(!system_xml.contains("/media/cache"), "{system_xml}");
        assert!(!system_xml.contains("FullChroma"), "{system_xml}");
        assert!(!system_xml.contains("12.0.0"), "{system_xml}");
        assert!(!file(&export, "config/encoding.xml").contains("/usr/bin/ffmpeg"));
    }

    /// The exported server id is Ferrofin's `system_id`, written where
    /// Jellyfin reads its own.
    #[test]
    fn the_server_id_is_kept() {
        let dir = config_dir(
            &crate::configuration_manager::default_server_configuration(),
            &[],
        );
        let export = export_configuration(dir.path()).expect("exports");
        assert_eq!(file(&export, "data/device.txt"), SERVER_ID);
        // The six documents and the id, and no `database.xml`.
        let paths: Vec<_> = export.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "config/system.xml",
                "config/encoding.xml",
                "config/branding.xml",
                "config/network.xml",
                "config/metadata.xml",
                "config/xbmcmetadata.xml",
                "data/device.txt",
            ]
        );
    }

    /// An id in a form `Guid.TryParse` reads is kept as written. A URN, in
    /// either case, is refused rather than handed to a Jellyfin that would
    /// mint a new id; so is a BOM in front, which the server would serve but
    /// Jellyfin would drop, so the two ids would differ.
    #[test]
    fn the_server_id_must_be_a_form_jellyfin_reads() {
        let dir = config_dir(
            &crate::configuration_manager::default_server_configuration(),
            &[],
        );
        let id_file = dir.path().join("system_id");
        let braced = "{3f25fe64-f745-407b-8884-1fa775eef31b}";
        std::fs::write(&id_file, braced).expect("write");
        let export = export_configuration(dir.path()).expect("exports");
        assert_eq!(file(&export, "data/device.txt"), braced);
        for refused in [
            format!("urn:uuid:{}", &braced[1..37]),
            format!("URN:UUID:{}", &braced[1..37]),
            format!("\u{feff}{SERVER_ID}"),
        ] {
            std::fs::write(&id_file, &refused).expect("write");
            let error = export_configuration(dir.path()).expect_err("refused");
            assert!(
                error.to_string().contains("server id"),
                "{refused}: {error}"
            );
        }
    }

    /// A member the default sets and the value clears becomes null, at any
    /// depth; a member both lack stays out.
    #[test]
    fn cleared_members_become_null() {
        let mut fields = json!({"Kept": 1, "Nested": {"Kept": "a"}})
            .as_object()
            .expect("object")
            .clone();
        let defaults = json!({"Kept": 2, "Days": 30, "Nested": {"Kept": "b", "Device": "/dev/x"}});
        mark_cleared(&mut fields, defaults.as_object().expect("object"));
        assert_eq!(
            Value::Object(fields),
            json!({"Kept": 1, "Days": null, "Nested": {"Kept": "a", "Device": null}})
        );
    }

    /// A named document that does not parse is exported as its defaults, as
    /// the server serves it.
    #[test]
    fn a_named_document_that_does_not_parse_exports_its_defaults() {
        let dir = config_dir(
            &crate::configuration_manager::default_server_configuration(),
            &[("metadata.json", json!("not an object"))],
        );
        let export = export_configuration(dir.path()).expect("exports");
        assert!(
            file(&export, "config/metadata.xml").contains(
                "<UseFileCreationTimeForDateAdded>true</UseFileCreationTimeForDateAdded>"
            )
        );
    }

    /// A directory Ferrofin never served is refused, not exported as
    /// defaults: no `system.json`, or no server id.
    #[test]
    fn a_directory_ferrofin_never_served_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = export_configuration(dir.path()).expect_err("refused");
        assert!(error.to_string().contains("system.json"), "{error}");

        let dir = config_dir(
            &crate::configuration_manager::default_server_configuration(),
            &[],
        );
        std::fs::write(dir.path().join("system_id"), "not an id").expect("write");
        let error = export_configuration(dir.path()).expect_err("refused");
        assert!(error.to_string().contains("server id"), "{error}");
    }

    /// The server backfills an empty `MetadataOptions` at load, so the export
    /// gives Jellyfin the same list.
    #[test]
    fn empty_metadata_options_are_backfilled_as_the_server_does() {
        let mut system = crate::configuration_manager::default_server_configuration();
        system.metadata_options.clear();
        let dir = config_dir(&system, &[]);
        let export = export_configuration(dir.path()).expect("exports");
        assert!(file(&export, "config/system.xml").contains("<ItemType>Movie</ItemType>"));
    }

    /// A Ferrofin data directory with a default `system.json` and the given
    /// libraries, each written as its `options.json`.
    fn data_dir(libraries: &[(&str, &LibraryOptions)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).expect("config dir");
        std::fs::write(
            config.join("system.json"),
            serde_json::to_vec(&crate::configuration_manager::default_server_configuration())
                .expect("serialize"),
        )
        .expect("write system.json");
        for (name, options) in libraries {
            write_library(&dir, name, options);
        }
        dir
    }

    async fn export_libraries(dir: &tempfile::TempDir) -> ConfigExport {
        export_library_options(dir.path(), &dir.path().join("config"))
            .await
            .expect("exports")
    }

    fn with_paths(paths: &[&str]) -> LibraryOptions {
        LibraryOptions {
            path_infos: paths
                .iter()
                .map(|p| ferrofin_model::configuration::MediaPathInfo {
                    path: (*p).to_owned(),
                })
                .collect(),
            ..LibraryOptions::default()
        }
    }

    /// Ferrofin's default library options, exported, have the shape Jellyfin
    /// 12.4 writes for a library it created with its own defaults, and the
    /// Collections library's folder in the data directory is stored as
    /// `%AppDataPath%/collections`, as Jellyfin stores it
    /// (`testdata/library-options.xml`, `testdata/collections-options.xml`).
    #[tokio::test]
    async fn library_options_have_the_shape_jellyfin_writes() {
        let movies = with_paths(&["/media/movies"]);
        let dir = data_dir(&[("Movies", &movies)]);
        let collections_path = dir.path().join("data").join("collections");
        let mut collections = with_paths(&[collections_path.to_str().expect("utf-8")]);
        collections.save_local_metadata = true;
        write_library(&dir, "Collections", &collections);
        let export = export_libraries(&dir).await;
        let goldens = [
            (
                "root/default/Movies/options.xml",
                include_str!("config_export/testdata/library-options.xml"),
            ),
            (
                "root/default/Collections/options.xml",
                include_str!("config_export/testdata/collections-options.xml"),
            ),
        ];
        for (path, golden) in goldens {
            let (root, mut ours) = members(file(&export, path));
            let (jellyfin_root, theirs) = members(golden);
            assert_eq!(root, jellyfin_root, "{path}: root element");
            // `[DefaultValue(false)]`: written by us, omitted by `XmlSerializer`
            // while false. Both read the same.
            for member in [
                "SaveLyricsWithMedia",
                "SaveTrickplayWithMedia",
                "PreferNonstandardArtistsTag",
                "UseCustomTagDelimiters",
            ] {
                let node = ours.remove(member).expect("written");
                assert_eq!(node.text, "false", "{path}: {member}");
            }
            let missing: Vec<_> = theirs.keys().filter(|k| !ours.contains_key(*k)).collect();
            let extra: Vec<_> = ours.keys().filter(|k| !theirs.contains_key(*k)).collect();
            assert!(
                missing.is_empty(),
                "{path}: Jellyfin writes {missing:?}, the export does not"
            );
            assert!(
                extra.is_empty(),
                "{path}: the export writes {extra:?}, Jellyfin has no such member"
            );
            let differ: Vec<_> = ours
                .iter()
                .filter(|(member, node)| theirs.get(*member) != Some(*node))
                .map(|(member, node)| (member, node, theirs.get(member)))
                .collect();
            assert!(
                differ.is_empty(),
                "{path}: members that differ: {differ:#?}"
            );
        }
    }

    /// Writes `options` as the `options.json` of the library `name`.
    fn write_library(dir: &tempfile::TempDir, name: &str, options: &LibraryOptions) {
        let folder = dir.path().join("root").join("default").join(name);
        std::fs::create_dir_all(&folder).expect("library dir");
        std::fs::write(
            folder.join("options.json"),
            serde_json::to_vec(options).expect("serialize"),
        )
        .expect("write options.json");
    }

    /// Every library option, each changed from its default, goes to XML and
    /// back through the import as it was; a media path in the data directory
    /// comes back in Jellyfin's token form. The test checks that it changed
    /// every member with a default value, at the top level, in a type and in
    /// an image option, so one added later is covered too; the members with no
    /// default are checked by name.
    #[tokio::test]
    async fn every_library_option_round_trips_through_the_import() {
        let dir = data_dir(&[]);
        let in_data = dir.path().join("data").join("home-videos");
        let defaults = object(&LibraryOptions::default()).expect("object");
        let mut fields = Map::new();
        for (key, default) in &defaults {
            let changed = match (key.as_str(), default) {
                // An enum: any other of its names.
                ("AllowEmbeddedSubtitles", _) => json!("AllowText"),
                // Collections of objects, set below.
                ("PathInfos" | "TypeOptions", _) => continue,
                (_, Value::Bool(b)) => json!(!b),
                (_, Value::Number(n)) => json!(n.as_i64().expect("an integer") + 7),
                (_, Value::String(s)) => json!(format!("{s} & <changed>")),
                (_, Value::Array(items)) if items.is_empty() => json!(["first", "second"]),
                (_, Value::Array(_)) => json!(["#"]),
                (_, other) => panic!("no changed value for {key}: {other}"),
            };
            fields.insert(key.clone(), changed);
        }
        for (key, value) in [
            // The members whose default is no value at all.
            ("PreferredMetadataLanguage", json!("de")),
            ("MetadataCountryCode", json!("DE")),
            // Every saver unchecked: an empty list, not no list.
            ("MetadataSavers", json!([])),
            ("LocalMetadataReaderOrder", json!(["Nfo"])),
            ("SubtitleDownloadLanguages", json!(["deu", "eng"])),
            (
                "PathInfos",
                json!([{"Path": "/media/films"}, {"Path": in_data.to_str().expect("utf-8")}]),
            ),
            (
                "TypeOptions",
                json!([
                    {
                        "Type": "Movie",
                        "MetadataFetchers": ["TheMovieDb"],
                        "MetadataFetcherOrder": ["TheMovieDb", "The Open Movie Database"],
                        "ImageFetchers": ["TheMovieDb"],
                        "ImageFetcherOrder": ["TheMovieDb"],
                        "ImageOptions": [
                            {"Type": "Backdrop", "Limit": 3, "MinWidth": 1280},
                            {"Type": "Logo", "Limit": 1, "MinWidth": 0},
                        ],
                        "SimilarItemProviders": ["Local"],
                        "SimilarItemProviderOrder": ["Local"],
                    },
                    {"Type": "Series", "ImageFetchers": ["TheTVDB"], "ImageOptions": []},
                ]),
            ),
        ] {
            fields.insert(key.to_owned(), value);
        }
        let options: LibraryOptions =
            serde_json::from_value(Value::Object(fields)).expect("valid options");
        let changed = object(&options).expect("object");
        assert_every_member_changed(&changed, &defaults);
        write_library(&dir, "Films", &options);

        let export = export_libraries(&dir).await;
        let xml = file(&export, "root/default/Films/options.xml");
        assert!(
            xml.contains("<Path>%AppDataPath%/home-videos</Path>"),
            "{xml}"
        );
        assert!(xml.contains("<MetadataSavers />"), "{xml}");
        let back: LibraryOptions =
            config_import::import_over(&LibraryOptions::default(), xml, "LibraryOptions", &[])
                .expect("imports");
        let mut expected = options;
        expected.path_infos[1].path = "%AppDataPath%/home-videos".to_owned();
        assert_eq!(back, expected);
        assert!(export.not_exported.is_empty(), "{:?}", export.not_exported);
    }

    /// A media path in the metadata directory is stored as `%MetadataPath%/…`,
    /// whether that directory is the default (`{data_dir}/metadata`) or the one
    /// `system.json`'s `MetadataPath` names.
    #[tokio::test]
    async fn a_media_path_in_the_metadata_directory_takes_its_token() {
        let dir = data_dir(&[]);
        let in_default = dir.path().join("metadata").join("extras");
        write_library(
            &dir,
            "Extras",
            &with_paths(&[in_default.to_str().expect("utf-8")]),
        );
        let export = export_libraries(&dir).await;
        let xml = file(&export, "root/default/Extras/options.xml");
        assert!(xml.contains("<Path>%MetadataPath%/extras</Path>"), "{xml}");

        let custom = dir.path().join("elsewhere");
        let mut system = crate::configuration_manager::default_server_configuration();
        system.metadata_path = custom.to_str().expect("utf-8").to_owned();
        std::fs::write(
            dir.path().join("config").join("system.json"),
            serde_json::to_vec(&system).expect("serialize"),
        )
        .expect("write system.json");
        let in_custom = custom.join("trailers");
        write_library(
            &dir,
            "Trailers",
            &with_paths(&[in_custom.to_str().expect("utf-8")]),
        );
        let export = export_libraries(&dir).await;
        let xml = file(&export, "root/default/Trailers/options.xml");
        assert!(
            xml.contains("<Path>%MetadataPath%/trailers</Path>"),
            "{xml}"
        );
        // The default directory is no longer the metadata directory.
        let xml = file(&export, "root/default/Extras/options.xml");
        assert!(
            xml.contains(&format!("<Path>{}</Path>", in_default.display())),
            "{xml}"
        );
    }

    /// A library adopted from Jellyfin whose `options.xml` Ferrofin never
    /// converted is exported from that XML, as the server reads it, and
    /// nothing is written into the source.
    #[tokio::test]
    async fn an_unconverted_adopted_options_xml_is_exported_as_read() {
        let dir = data_dir(&[]);
        let folder = dir.path().join("root").join("default").join("Collections");
        std::fs::create_dir_all(&folder).expect("library dir");
        let golden = include_str!("config_export/testdata/collections-options.xml");
        std::fs::write(folder.join("options.xml"), golden).expect("write");
        let export = export_libraries(&dir).await;
        let xml = file(&export, "root/default/Collections/options.xml");
        assert!(
            xml.contains("<Path>%AppDataPath%/collections</Path>"),
            "{xml}"
        );
        assert!(
            xml.contains("<SaveLocalMetadata>true</SaveLocalMetadata>"),
            "{xml}"
        );
        assert!(
            !folder.join("options.json").exists(),
            "the source was changed"
        );
    }

    /// A data directory with no libraries exports none, and a file beside the
    /// library folders is not one.
    #[tokio::test]
    async fn only_library_folders_are_exported() {
        let dir = data_dir(&[]);
        let export = export_libraries(&dir).await;
        assert!(export.files.is_empty(), "{:?}", export.files);

        let dir = data_dir(&[("Music", &LibraryOptions::default())]);
        std::fs::write(
            dir.path().join("root").join("default").join("stray.txt"),
            "x",
        )
        .expect("write");
        // A link to a library folder is a library, as the server lists it; a
        // link that leads nowhere is not.
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("dir");
        let root = dir.path().join("root").join("default");
        std::os::unix::fs::symlink(&elsewhere, root.join("Linked")).expect("link");
        std::os::unix::fs::symlink(dir.path().join("gone"), root.join("Gone")).expect("link");
        let export = export_libraries(&dir).await;
        let paths: Vec<_> = export.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "root/default/Linked/options.xml",
                "root/default/Music/options.xml"
            ]
        );
    }

    /// Fails unless `changed` sets every library member away from `defaults`,
    /// and every member of its first type and that type's first image option.
    fn assert_every_member_changed(changed: &Map<String, Value>, defaults: &Map<String, Value>) {
        let mut unchanged = left_at_default(changed, defaults);
        let first_type = changed["TypeOptions"][0].as_object().expect("a type");
        let type_defaults =
            object(&ferrofin_model::configuration::TypeOptions::default()).expect("object");
        unchanged.extend(
            left_at_default(first_type, &type_defaults)
                .into_iter()
                .map(|k| format!("TypeOptions.{k}")),
        );
        let first_image = first_type["ImageOptions"][0]
            .as_object()
            .expect("an image option");
        let image_defaults =
            object(&ferrofin_model::configuration::ImageOption::default()).expect("object");
        unchanged.extend(
            left_at_default(first_image, &image_defaults)
                .into_iter()
                .map(|k| format!("ImageOptions.{k}")),
        );
        // serde drops a misspelled key, so the members with no default are
        // checked by name.
        for key in [
            "PreferredMetadataLanguage",
            "MetadataCountryCode",
            "MetadataSavers",
            "LocalMetadataReaderOrder",
            "SubtitleDownloadLanguages",
        ] {
            if !changed.contains_key(key) {
                unchanged.push(key.to_owned());
            }
        }
        if !first_type.contains_key("Type") {
            unchanged.push("TypeOptions.Type".to_owned());
        }
        assert!(
            unchanged.is_empty(),
            "members left at their default: {unchanged:?}"
        );
    }

    /// The members of `defaults` that `changed` leaves at their default, or
    /// leaves out.
    fn left_at_default(changed: &Map<String, Value>, defaults: &Map<String, Value>) -> Vec<String> {
        defaults
            .iter()
            .filter(|(key, default)| changed.get(*key).is_none_or(|value| value == *default))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// A value XML cannot carry stops the export, naming the library and the
    /// setting, so the operator knows which `options.json` to fix.
    #[tokio::test]
    async fn a_value_xml_cannot_carry_names_the_library() {
        let options = LibraryOptions {
            season_zero_display_name: "bad\u{1}name".to_owned(),
            ..LibraryOptions::default()
        };
        let dir = data_dir(&[("Films", &options)]);
        let error = export_library_options(dir.path(), &dir.path().join("config"))
            .await
            .expect_err("refused");
        let error = error.to_string();
        assert!(error.contains("library Films"), "{error}");
        assert!(error.contains("SeasonZeroDisplayName"), "{error}");
    }
}
