//! The Ferrofin settings page's wire types: what `GET /Ferrofin/Settings`
//! reports about each setting (CLI flags, `FERROFIN_*` environment variables,
//! `config.toml`, and the server configuration the Dashboard edits), and what
//! `POST /Ferrofin/Settings` saves.
//!
//! Not a Jellyfin contract type: the route is Ferrofin's own, read by the
//! Ferrofin plugin's settings page. Its JSON is PascalCase, like the rest of
//! the API.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Every Ferrofin bootstrap setting, with where the `config.toml` they may be
/// saved to lives.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ServerSettingsDto {
    /// The `config.toml` the server reads, whether or not it exists.
    pub config_file: String,

    /// Whether a save could write [`config_file`](Self::config_file): its
    /// directory accepts a new file, which is then renamed over it, and the
    /// file, when it exists, is not read-only.
    pub config_file_writable: bool,

    /// Whether a restart is pending (`GET /System/Info`'s `HasPendingRestart`).
    pub restart_pending: bool,

    /// One entry per setting, grouped as the settings page shows them.
    pub settings: Vec<ServerSettingDto>,
}

/// One bootstrap setting: its running value, its default, where the value
/// comes from, and whether the settings page may change it.
///
/// `Debug` leaves out the values of a [`ServerSettingKind::Secret`] setting,
/// so logging one cannot leak a key or a password.
#[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ServerSettingDto {
    /// The setting's `config.toml` key; for a setting `config.toml` has no key
    /// for (an OpenTelemetry variable), that variable's name.
    pub key: String,

    /// The setting's readable name, which heads its row on the settings page,
    /// e.g. "Subtitle extraction limit".
    pub label: String,

    /// The tab the settings page lists it on.
    pub group: ServerSettingGroup,

    /// The kind of value it holds, which picks the page's control.
    pub kind: ServerSettingKind,

    /// The unit its value is in, which the page shows beside it. Absent for a
    /// value that has none, such as an on/off, a path or a URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<ServerSettingUnit>,

    /// What the setting does, in one line.
    pub description: String,

    /// What the setting does, at more length than its description, which the
    /// page shows under it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,

    /// The environment variable that sets it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_var: Option<String>,

    /// The command-line flag that sets it, e.g. `--port`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,

    /// The value that applies when nothing sets it, as the page shows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    /// The value in use, secrets included (admins see them, as on Jellyfin's
    /// plugin pages). Absent when the setting is unset and its default
    /// applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,

    /// The value `config.toml` holds now, when it differs from the one in use:
    /// the file changed since the server started, or a variable, a flag or
    /// the Networking page outranks it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_value: Option<serde_json::Value>,

    /// Where the value in use comes from.
    pub source: ServerSettingSource,

    /// Whether the settings page may change it.
    pub editable: bool,

    /// Why the settings page may not change it, and where it is changed
    /// instead. Present exactly when [`editable`](Self::editable) is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked_reason: Option<String>,

    /// What a change needs before it applies.
    pub applies: ServerSettingApplies,

    /// The smallest value an [`ServerSettingKind::Integer`] setting takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,

    /// The largest value an [`ServerSettingKind::Integer`] setting takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,

    /// The options of a [`ServerSettingKind::Choice`] setting, in the order
    /// the page offers them. Empty, and left out, for any other kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ServerSettingOptionDto>,

    /// Where a save writes it.
    pub store: ServerSettingStore,

    /// Whether the stored value of a [`ServerSettingKind::Choice`] setting
    /// fits none of its options: [`value`](Self::value) is then the stored
    /// value as text, which a save keeps until it names the setting. Left out
    /// when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
}

/// One option of a [`ServerSettingKind::Choice`] setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ServerSettingOptionDto {
    /// The value a save sends and the report gives when it is picked.
    pub value: String,

    /// Its readable name.
    pub label: String,

    /// What picking it does, which the page shows while it is picked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Where a setting is stored, which decides when a change to it applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ServerSettingStore {
    /// `config.toml`, read when the server starts.
    ConfigFile,
    /// The server configuration that the Dashboard edits
    /// (`GET /System/Configuration`), which applies at once.
    ServerConfiguration,
}

impl std::fmt::Debug for ServerSettingDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secret = self.kind == ServerSettingKind::Secret;
        let shown = |value: &Option<serde_json::Value>| -> Option<String> {
            value.as_ref().map(|value| {
                if secret {
                    "[REDACTED]".to_owned()
                } else {
                    value.to_string()
                }
            })
        };
        f.debug_struct("ServerSettingDto")
            .field("key", &self.key)
            .field("label", &self.label)
            .field("group", &self.group)
            .field("kind", &self.kind)
            .field("unit", &self.unit)
            .field("description", &self.description)
            .field("note", &self.note)
            .field("env_var", &self.env_var)
            .field("flag", &self.flag)
            .field("default", &self.default)
            .field("value", &shown(&self.value))
            .field("saved_value", &shown(&self.saved_value))
            .field("source", &self.source)
            .field("editable", &self.editable)
            .field("locked_reason", &self.locked_reason)
            .field("applies", &self.applies)
            .field("min", &self.min)
            .field("max", &self.max)
            .field("options", &self.options)
            .field("store", &self.store)
            .field("custom", &self.custom)
            .finish()
    }
}

/// The tabs the settings page lists the settings on, in the page's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ServerSettingGroup {
    /// Listening, the advertised addresses, the server's identity and the
    /// limits on signing in.
    Network,
    /// Library scans.
    Library,
    /// Trickplay image generation.
    Trickplay,
    /// Image decoding, resizing and downloading.
    Images,
    /// The remote metadata providers: keys, mirrors and request limits.
    Providers,
    /// The database, the caches, the transcode caps and the metrics queue.
    Performance,
    /// Logging, metrics and trace export.
    Logging,
    /// The limits of sandboxed WASM plugins.
    Plugins,
    /// The directories, the executables, first-boot seeding, shutdown and the
    /// extensions switch.
    System,
}

/// The unit a setting's value is in, as the settings page shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ServerSettingUnit {
    /// Mebibytes.
    #[serde(rename = "MiB")]
    Mebibytes,
    /// Seconds.
    #[serde(rename = "seconds")]
    Seconds,
    /// Minutes.
    #[serde(rename = "minutes")]
    Minutes,
    /// Hours.
    #[serde(rename = "hours")]
    Hours,
    /// Days.
    #[serde(rename = "days")]
    Days,
    /// Megapixels: an image's width times its height, in millions.
    #[serde(rename = "megapixels")]
    Megapixels,
    /// A count of items, records or entries.
    #[serde(rename = "items")]
    Items,
    /// A count of queued events.
    #[serde(rename = "events")]
    Events,
}

/// The kind of value a setting holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ServerSettingKind {
    /// On or off.
    Bool,
    /// A whole number from `Min` to `Max`.
    Integer,
    /// A whole number, or `auto`.
    CountOrAuto,
    /// Free text.
    Text,
    /// A URL.
    Url,
    /// A filesystem path.
    Path,
    /// A list of filesystem paths.
    PathList,
    /// A list of numbers.
    NumberList,
    /// A key, a PIN or a password: shown to admins, never logged.
    Secret,
    /// One of the setting's `Options`, by its `Value`.
    Choice,
}

/// Where a setting's value in use comes from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ServerSettingSource {
    /// A command-line flag.
    Flag,
    /// An environment variable.
    Environment,
    /// jellyfin-web's Dashboard → Networking page, the saved network
    /// configuration, for a setting it covers when it holds a value: it
    /// outranks `config.toml`, and a variable or a flag outranks it.
    NetworkConfiguration,
    /// `config.toml`.
    ConfigFile,
    /// The server configuration, for a setting stored there
    /// ([`ServerSettingStore::ServerConfiguration`]), when it holds another
    /// value than a new server's. Nothing outranks it.
    ServerConfiguration,
    /// Nothing sets it: the default applies.
    Default,
}

/// What a change to a setting needs before it applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ServerSettingApplies {
    /// A restart, the dashboard's in-process restart included.
    Restart,
    /// A restart of the server process: the in-process restart keeps the
    /// value it was started with.
    ProcessRestart,
    /// Nothing: a setting stored in the server configuration applies as it is
    /// saved.
    Immediately,
}

/// The changes `POST /Ferrofin/Settings` saves: to `config.toml`, and to the
/// server configuration for a setting stored there.
///
/// `Debug` lists the keys only: a value may be a key or a password.
#[derive(Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ServerSettingsUpdateDto {
    /// The settings to change, by key ([`ServerSettingDto::key`]). A value
    /// sets the key; `null` removes it from `config.toml`, so the setting goes
    /// back to its default, and puts a setting stored in the server
    /// configuration back to a new server's value.
    pub changes: BTreeMap<String, Option<serde_json::Value>>,
}

impl std::fmt::Debug for ServerSettingsUpdateDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSettingsUpdateDto")
            .field("changes", &self.changes.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// What a `POST /Ferrofin/Settings` saved.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ServerSettingsUpdateResultDto {
    /// The keys whose entry in `config.toml` the save set or removed, in key
    /// order. Removing a key the file does not hold changes nothing.
    pub changed: Vec<String>,

    /// Whether a restart is needed before the saved values apply: whenever a
    /// `config.toml` key changed. The server then reports a pending restart
    /// (`GET /System/Info`'s `HasPendingRestart`).
    pub restart_required: bool,

    /// The changed keys only a restart of the server process applies
    /// ([`ServerSettingApplies::ProcessRestart`]): the in-process restart keeps
    /// their running value.
    pub process_restart_required: Vec<String>,

    /// The keys stored in the server configuration that the save changed, in
    /// key order: they applied at once, with no restart.
    pub applied_now: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(kind: ServerSettingKind) -> ServerSettingDto {
        ServerSettingDto {
            key: "omdb_api_key".to_owned(),
            label: "OMDb API key".to_owned(),
            group: ServerSettingGroup::Providers,
            kind,
            unit: None,
            description: "OMDb API key".to_owned(),
            note: None,
            env_var: Some("FERROFIN_OMDB_KEY".to_owned()),
            flag: None,
            default: None,
            value: Some(serde_json::json!("hunter2-running")),
            saved_value: Some(serde_json::json!("hunter2-saved")),
            source: ServerSettingSource::ConfigFile,
            editable: true,
            locked_reason: None,
            applies: ServerSettingApplies::Restart,
            min: None,
            max: None,
            options: Vec::new(),
            store: ServerSettingStore::ConfigFile,
            custom: false,
        }
    }

    #[test]
    fn debug_leaves_out_a_secrets_values_only() {
        let secret = format!("{:?}", setting(ServerSettingKind::Secret));
        assert!(!secret.contains("hunter2"), "{secret}");
        assert!(secret.contains("[REDACTED]"), "{secret}");

        let text = format!("{:?}", setting(ServerSettingKind::Text));
        assert!(text.contains("hunter2-running"), "{text}");
        assert!(text.contains("hunter2-saved"), "{text}");
    }

    #[test]
    fn serializes_pascal_case_and_leaves_out_what_is_unset() {
        let json = serde_json::to_value(setting(ServerSettingKind::Secret)).expect("serialize");
        assert_eq!(json["Key"], "omdb_api_key");
        assert_eq!(json["Label"], "OMDb API key");
        assert_eq!(json["Group"], "Providers");
        assert_eq!(json["EnvVar"], "FERROFIN_OMDB_KEY");
        assert_eq!(json["Kind"], "Secret");
        assert_eq!(json["Source"], "ConfigFile");
        assert_eq!(json["Applies"], "Restart");
        assert_eq!(json["Value"], "hunter2-running");
        assert_eq!(json["SavedValue"], "hunter2-saved");
        assert_eq!(json["Store"], "ConfigFile");
        for absent in [
            "Unit",
            "Flag",
            "Default",
            "LockedReason",
            "Min",
            "Max",
            "Note",
            "Options",
            "Custom",
        ] {
            assert!(json.get(absent).is_none(), "{absent} in {json}");
        }

        let all = ServerSettingsDto {
            config_file: "/data/config.toml".to_owned(),
            config_file_writable: true,
            restart_pending: false,
            settings: vec![setting(ServerSettingKind::Text)],
        };
        let json = serde_json::to_value(&all).expect("serialize");
        assert_eq!(json["ConfigFile"], "/data/config.toml");
        assert_eq!(json["ConfigFileWritable"], true);
        assert_eq!(json["RestartPending"], false);
        assert_eq!(json["Settings"][0]["Key"], "omdb_api_key");
        let back: ServerSettingsDto = serde_json::from_value(json).expect("round trip");
        assert_eq!(back, all);
    }

    #[test]
    fn a_choice_serializes_its_options_note_store_and_custom_flag() {
        let choice = ServerSettingDto {
            key: "image_saving_convention".to_owned(),
            kind: ServerSettingKind::Choice,
            note: Some("A longer note.".to_owned()),
            value: Some(serde_json::json!("7")),
            saved_value: None,
            source: ServerSettingSource::ServerConfiguration,
            applies: ServerSettingApplies::Immediately,
            options: vec![
                ServerSettingOptionDto {
                    value: "Legacy".to_owned(),
                    label: "Legacy".to_owned(),
                    note: Some("Jellyfin's own names.".to_owned()),
                },
                ServerSettingOptionDto {
                    value: "Compatible".to_owned(),
                    label: "Compatible".to_owned(),
                    note: None,
                },
            ],
            store: ServerSettingStore::ServerConfiguration,
            custom: true,
            ..setting(ServerSettingKind::Text)
        };
        let json = serde_json::to_value(&choice).expect("serialize");
        assert_eq!(json["Kind"], "Choice");
        assert_eq!(json["Note"], "A longer note.");
        assert_eq!(json["Source"], "ServerConfiguration");
        assert_eq!(json["Applies"], "Immediately");
        assert_eq!(json["Store"], "ServerConfiguration");
        assert_eq!(json["Custom"], true);
        assert_eq!(json["Value"], "7");
        assert_eq!(
            json["Options"],
            serde_json::json!([
                { "Value": "Legacy", "Label": "Legacy", "Note": "Jellyfin's own names." },
                { "Value": "Compatible", "Label": "Compatible" }
            ])
        );
        let back: ServerSettingDto = serde_json::from_value(json).expect("round trip");
        assert_eq!(back, choice);

        let result = ServerSettingsUpdateResultDto {
            applied_now: vec!["image_saving_convention".to_owned()],
            ..ServerSettingsUpdateResultDto::default()
        };
        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            json["AppliedNow"],
            serde_json::json!(["image_saving_convention"])
        );
        assert_eq!(json["RestartRequired"], false);
    }

    #[test]
    fn a_unit_serializes_as_the_page_shows_it() {
        let units = [
            (ServerSettingUnit::Mebibytes, "MiB"),
            (ServerSettingUnit::Seconds, "seconds"),
            (ServerSettingUnit::Minutes, "minutes"),
            (ServerSettingUnit::Hours, "hours"),
            (ServerSettingUnit::Days, "days"),
            (ServerSettingUnit::Megapixels, "megapixels"),
            (ServerSettingUnit::Items, "items"),
            (ServerSettingUnit::Events, "events"),
        ];
        for (unit, shown) in units {
            let setting = ServerSettingDto {
                unit: Some(unit),
                ..setting(ServerSettingKind::Integer)
            };
            let json = serde_json::to_value(&setting).expect("serialize");
            assert_eq!(json["Unit"], shown, "{unit:?}");
            let back: ServerSettingDto = serde_json::from_value(json).expect("round trip");
            assert_eq!(back.unit, Some(unit));
        }
    }

    #[test]
    fn an_update_reads_its_changes_and_debug_shows_only_their_keys() {
        let update: ServerSettingsUpdateDto = serde_json::from_value(serde_json::json!({
            "Changes": { "omdb_api_key": "hunter2", "db_pool": 4, "tvdb_token_days": null }
        }))
        .expect("deserialize");
        assert_eq!(
            update.changes.get("omdb_api_key"),
            Some(&Some(serde_json::json!("hunter2")))
        );
        assert_eq!(
            update.changes.get("db_pool"),
            Some(&Some(serde_json::json!(4)))
        );
        assert_eq!(
            update.changes.get("tvdb_token_days"),
            Some(&None),
            "null removes"
        );

        let debug = format!("{update:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("omdb_api_key"), "{debug}");
    }

    #[test]
    fn an_update_result_serializes_pascal_case() {
        let result = ServerSettingsUpdateResultDto {
            changed: vec!["log_level".to_owned(), "scan_probe_concurrency".to_owned()],
            restart_required: true,
            process_restart_required: vec!["log_level".to_owned()],
            applied_now: Vec::new(),
        };
        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "Changed": ["log_level", "scan_probe_concurrency"],
                "RestartRequired": true,
                "ProcessRestartRequired": ["log_level"],
                "AppliedNow": []
            })
        );
        let back: ServerSettingsUpdateResultDto = serde_json::from_value(json).expect("round trip");
        assert_eq!(back, result);
    }
}
