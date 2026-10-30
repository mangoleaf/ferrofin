//! The Ferrofin settings page's wire types: what `GET /Ferrofin/Settings`
//! reports about each bootstrap setting (CLI flags, `FERROFIN_*` environment
//! variables, `config.toml`).
//!
//! Not a Jellyfin contract type: the route is Ferrofin's own, read by the
//! Ferrofin plugin's settings page. Its JSON is PascalCase, like the rest of
//! the API.

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
    /// directory accepts a new file, which is then renamed over it.
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

    /// The group the settings page lists it under.
    pub group: ServerSettingGroup,

    /// The kind of value it holds, which picks the page's control.
    pub kind: ServerSettingKind,

    /// What the setting does, in one line.
    pub description: String,

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
    /// the file changed since the server started, or a variable or flag
    /// outranks it.
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
            .field("group", &self.group)
            .field("kind", &self.kind)
            .field("description", &self.description)
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
            .finish()
    }
}

/// The groups the settings page lists the settings in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ServerSettingGroup {
    /// The data, configuration, cache and web directories.
    Paths,
    /// Listening, the advertised addresses and the server's identity.
    Network,
    /// The ffmpeg and ffprobe executables and how they are used.
    Ffmpeg,
    /// Library scans, the database and first-boot seeding.
    Library,
    /// Logging, metrics, trace export and shutdown.
    Observability,
    /// The remote metadata providers: keys, mirrors and request limits.
    Providers,
    /// The limits of sandboxed WASM plugins.
    Wasm,
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
}

/// Where a setting's value in use comes from, highest precedence first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ServerSettingSource {
    /// A command-line flag.
    Flag,
    /// An environment variable.
    Environment,
    /// `config.toml`.
    ConfigFile,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(kind: ServerSettingKind) -> ServerSettingDto {
        ServerSettingDto {
            key: "omdb_api_key".to_owned(),
            group: ServerSettingGroup::Providers,
            kind,
            description: "OMDb API key".to_owned(),
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
        assert_eq!(json["EnvVar"], "FERROFIN_OMDB_KEY");
        assert_eq!(json["Kind"], "Secret");
        assert_eq!(json["Source"], "ConfigFile");
        assert_eq!(json["Applies"], "Restart");
        assert_eq!(json["Value"], "hunter2-running");
        assert_eq!(json["SavedValue"], "hunter2-saved");
        for absent in ["Flag", "Default", "LockedReason", "Min", "Max"] {
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
}
