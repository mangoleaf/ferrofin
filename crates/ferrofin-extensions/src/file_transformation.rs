//! The **File Transformation** extension — a compiled-in port of
//! [`jellyfin-plugin-file-transformation`](https://github.com/IAmParadox27/jellyfin-plugin-file-transformation)
//! (GUID `5e87cc92-571a-4d8d-8d98-d2d4147f9f90`).
//!
//! The plugin lets other plugins (and admins, via its settings page) register
//! **transformations of served web-client files**: a file-name pattern (exact
//! web-root-relative path or regex) plus a transformer. The static `/web`
//! server routes matching files through the pipeline, so e.g. the Intro
//! Skipper can patch `main.jellyfin.bundle.js` to honor its skip-button
//! hide-delay setting.
//!
//! Ported surface:
//! - [`WebFileTransformationService`] — the pattern→pipeline registry
//!   (`WebFileTransformationService` upstream), including HTTP-endpoint
//!   callbacks (`TransformationHelper`) and the admin-defined search/replace
//!   transformations from the plugin configuration (read live, so a config
//!   save applies without a restart);
//! - [`FileTransformationExtension`] — the `/Plugins` presentation + the
//!   vendored upstream settings page;
//! - [`SkipButtonTransformer`] — Intro Skipper's `Injector.FileTransformer`,
//!   its one in-process consumer (registered by the composition root).
//!
//! Not ported: .NET assembly-reflection callbacks and named-pipe callbacks
//! (both are .NET-process-specific; compiled-in extensions register natively
//! through the [`FileTransformationService`] trait instead).

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use ferrofin_core::{PluginConfigPage, ScheduledTask};
use ferrofin_traits::plugins::{
    FileTransformationService, FileTransformer, PluginDescriptor, PluginManager,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{Extension, ExtensionContext};

/// The File Transformation plugin's stable id — the **upstream plugin's GUID**
/// (Intro Skipper detects the plugin's presence by this exact id).
pub const EXTENSION_ID: Uuid = Uuid::from_u128(0x5e87_cc92_571a_4d8d_8d98_d2d4_147f_9f90);

/// The Intro Skipper's id, under which its skip-button transformer registers.
const INTRO_SKIPPER_ID: Uuid = Uuid::from_u128(0xc83d_86bb_a1e0_4c35_a113_e210_1cf4_ee6b);

/// The `/Plugins` surface of the File Transformation extension.
pub struct FileTransformationExtension;

impl Extension for FileTransformationExtension {
    fn id(&self) -> Uuid {
        EXTENSION_ID
    }

    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: EXTENSION_ID,
            name: "File Transformation".to_owned(),
            // The upstream plugin version the port (and its vendored settings
            // page) tracks.
            version: "2.5.0.0".to_owned(),
            description: "Provides a pipeline that transforms served web-client files, so \
                          plugins can patch the web UI without modifying it on disk."
                .to_owned(),
            enabled: true,
            has_image: false,
            can_uninstall: false,
            configuration_file_name: None,
        }
    }

    fn default_config(&self) -> Vec<u8> {
        // Port of the upstream `PluginConfiguration` defaults; the enum is
        // serialized by name, which is what the settings page's `<select>`
        // reads and writes.
        br#"{"DebugLoggingState":"Disabled","Transformations":[]}"#.to_vec()
    }

    fn config_pages(&self) -> Vec<PluginConfigPage> {
        // The upstream plugin's own settings page (vendored by `build.rs`),
        // main-menu-enabled exactly as its `GetPages` declares.
        vec![PluginConfigPage {
            name: "File Transformation".to_owned(),
            bytes: include_bytes!(concat!(env!("OUT_DIR"), "/filetransformation/config.html"))
                .to_vec(),
            enable_in_main_menu: true,
        }]
    }

    fn tasks(&self, _cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>> {
        Vec::new()
    }
}

/// One registered transformation: an id, its file-name pattern, and how to
/// invoke it.
struct Registration {
    id: Uuid,
    pattern: String,
    kind: TransformKind,
}

/// Hard cap on the number of live registrations — a runaway bound, not a
/// working-set tuner.
///
/// The registry has the lifetime of the process and nothing sweeps it: an
/// entry survives until its owner calls `remove_transformation`, which no
/// caller of `POST /FileTransformation/RegisterTransformation` is obliged to
/// do. Registrations are keyed by an id **and** a pattern the caller supplies,
/// so varying either defeats the idempotence check and appends a fresh entry
/// per request. Real deployments register a handful (one per plugin that
/// patches a web file); this cap is orders of magnitude above that and exists
/// only so a caller in a loop cannot grow the process without limit.
///
/// FLAGGED as a candidate setting — plausible range 32…4096, default 256.
const MAX_REGISTRATIONS: usize = 256;

/// Hard cap on one registration's pattern length.
///
/// A pattern is matched (and regex-compiled) against every served `/web` path,
/// so an over-long one is both retained forever and re-scanned per request.
/// Web-root-relative paths and the regexes that match them are tens of bytes;
/// 1 KiB is an abuse guard, not a limit anyone reaches.
///
/// FLAGGED as a candidate setting — plausible range 256…8192, default 1024.
const MAX_PATTERN_LEN: usize = 1024;

/// Hard cap on one registration's callback endpoint length. Endpoints are URLs;
/// 2 KiB is the conventional practical URL ceiling.
///
/// FLAGGED as a candidate setting — plausible range 256…8192, default 2048.
const MAX_ENDPOINT_LEN: usize = 2048;

/// Whether a registration may be admitted, given the registry's current
/// contents. Refusals are logged and dropped (the upstream controller always
/// answers `Ok()`, so refusing must not change the HTTP status).
///
/// The length checks come first so an over-long string is never stored, and
/// the count check ignores re-registrations of an existing `(id, pattern)` —
/// those replace nothing and add nothing, and are handled by the callers'
/// idempotence check.
fn admits(regs: &[Registration], id: Uuid, pattern: &str, endpoint_len: usize) -> bool {
    if pattern.len() > MAX_PATTERN_LEN {
        tracing::warn!(
            %id,
            len = pattern.len(),
            max = MAX_PATTERN_LEN,
            "file-transformation registration refused: pattern too long"
        );
        return false;
    }
    if endpoint_len > MAX_ENDPOINT_LEN {
        tracing::warn!(
            %id,
            len = endpoint_len,
            max = MAX_ENDPOINT_LEN,
            "file-transformation registration refused: endpoint too long"
        );
        return false;
    }
    if regs.len() >= MAX_REGISTRATIONS {
        tracing::warn!(
            %id,
            pattern,
            max = MAX_REGISTRATIONS,
            "file-transformation registration refused: registry is full"
        );
        return false;
    }
    true
}

/// How a registered transformation is invoked.
enum TransformKind {
    /// An in-process transformer (a compiled-in extension's callback).
    Builtin(Arc<dyn FileTransformer>),
    /// An HTTP callback: POST `{"contents": …}` to the endpoint, response body
    /// is the transformed contents.
    Endpoint(String),
}

/// An admin-defined transformation from the plugin configuration
/// (`PluginDefinedTransformation` upstream).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ConfiguredTransformation {
    id: Uuid,
    filename_pattern: String,
    search_text: String,
    replace_text: String,
}

impl Default for ConfiguredTransformation {
    fn default() -> Self {
        Self {
            id: Uuid::nil(),
            filename_pattern: String::new(),
            search_text: String::new(),
            replace_text: String::new(),
        }
    }
}

/// The subset of the plugin configuration the service reads.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ServiceConfig {
    transformations: Vec<ConfiguredTransformation>,
}

/// The concrete [`FileTransformationService`] — the pattern→pipeline registry.
///
/// Port of the upstream `WebFileTransformationService`. Admin-defined
/// search/replace transformations are read from the plugin configuration on
/// every match check rather than registered at startup, so a dashboard config
/// save applies immediately (upstream re-registers via its
/// `UpdateConfiguration` hook, which Ferrofin's plugin manager doesn't have).
// ponytail: config read per matching request — add an mtime cache if /web
// request volume ever makes it measurable.
pub struct WebFileTransformationService {
    registrations: RwLock<Vec<Registration>>,
    plugins: Arc<dyn PluginManager>,
    /// The server's own base URL (e.g. `http://127.0.0.1:8096`), for resolving
    /// relative HTTP-callback endpoints — upstream resolves them against the
    /// server's published URL.
    base_url: String,
    http: reqwest::Client,
}

impl WebFileTransformationService {
    /// Builds the service over the plugin manager (for the enabled gate and
    /// the admin-defined transformations) and the server's own base URL.
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginManager>, base_url: String) -> Self {
        Self {
            registrations: RwLock::new(Vec::new()),
            plugins,
            base_url,
            http: reqwest::Client::new(),
        }
    }

    /// Whether the File Transformation plugin itself is enabled — a disabled
    /// plugin means no pipeline at all (upstream: a disabled plugin's
    /// middleware is never loaded).
    async fn plugin_enabled(&self) -> bool {
        self.plugins
            .get_plugin(EXTENSION_ID)
            .await
            .ok()
            .flatten()
            .is_some_and(|p| p.enabled)
    }

    /// The admin-defined transformations from the live plugin configuration.
    async fn configured_transformations(&self) -> Vec<ConfiguredTransformation> {
        let Ok(bytes) = self.plugins.get_plugin_configuration(EXTENSION_ID).await else {
            return Vec::new();
        };
        serde_json::from_slice::<ServiceConfig>(&bytes)
            .map(|c| c.transformations)
            .unwrap_or_default()
    }

    /// Whether `pattern` matches `path`: exact (case-sensitive, like the
    /// upstream dictionary key) or as a regex. Port of the
    /// exact-`ContainsKey`-then-regex match in `NeedsTransformation`.
    fn pattern_matches(pattern: &str, path: &str) -> bool {
        if pattern == path {
            return true;
        }
        regex::Regex::new(pattern).is_ok_and(|r| r.is_match(path))
    }

    /// Strips the leading `/` (upstream `NormalizePath`).
    fn normalize(path: &str) -> &str {
        path.trim_start_matches('/')
    }

    /// Invokes an HTTP-callback endpoint with `contents`, returning the
    /// response body, or `None` (leaving the contents unchanged) on any
    /// failure. Port of the endpoint branch of
    /// `TransformationHelper.ApplyTransformation`.
    async fn call_endpoint(&self, endpoint: &str, contents: &str) -> Option<String> {
        let url = if endpoint.starts_with("http") {
            endpoint.to_owned()
        } else {
            format!("{}/{}", self.base_url, endpoint.trim_start_matches('/'))
        };
        let response = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "contents": contents }))
            .send()
            .await;
        match response {
            Ok(resp) => match resp.text().await {
                Ok(text) => Some(text),
                Err(e) => {
                    tracing::warn!(url, error = %e, "transformation endpoint body read failed");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(url, error = %e, "transformation endpoint call failed");
                None
            }
        }
    }
}

#[async_trait]
impl FileTransformationService for WebFileTransformationService {
    async fn needs_transformation(&self, path: &str) -> bool {
        if !self.plugin_enabled().await {
            return false;
        }
        let path = Self::normalize(path);
        if self
            .registrations
            .read()
            .expect("registrations lock poisoned")
            .iter()
            .any(|r| Self::pattern_matches(&r.pattern, path))
        {
            return true;
        }
        self.configured_transformations()
            .await
            .iter()
            .any(|t| Self::pattern_matches(&t.filename_pattern, path))
    }

    async fn run_transformation(&self, path: &str, contents: String) -> String {
        if !self.plugin_enabled().await {
            return contents;
        }
        let path = Self::normalize(path).to_owned();

        // Snapshot the matching registered pipeline steps (in registration
        // order — the upstream per-pattern pipeline), then the admin-defined
        // search/replace steps.
        let steps: Vec<(Uuid, TransformStep)> = {
            let regs = self
                .registrations
                .read()
                .expect("registrations lock poisoned");
            regs.iter()
                .filter(|r| Self::pattern_matches(&r.pattern, &path))
                .map(|r| {
                    let step = match &r.kind {
                        TransformKind::Builtin(t) => TransformStep::Builtin(Arc::clone(t)),
                        TransformKind::Endpoint(url) => TransformStep::Endpoint(url.clone()),
                    };
                    (r.id, step)
                })
                .collect()
        };

        let mut contents = contents;
        for (id, step) in steps {
            contents = match step {
                TransformStep::Builtin(t) => t.transform(&path, contents).await,
                TransformStep::Endpoint(url) => {
                    if let Some(transformed) = self.call_endpoint(&url, &contents).await {
                        transformed
                    } else {
                        tracing::warn!(%id, "transformation left contents unchanged");
                        contents
                    }
                }
            };
        }

        // Admin-defined search/replace (skipped when either side is blank,
        // matching `HandlePluginConfigTransformation`).
        for t in self.configured_transformations().await {
            if Self::pattern_matches(&t.filename_pattern, &path)
                && !t.search_text.trim().is_empty()
                && !t.replace_text.trim().is_empty()
            {
                contents = contents.replace(&t.search_text, &t.replace_text);
            }
        }

        contents
    }

    async fn add_transformation(
        &self,
        id: Uuid,
        file_name_pattern: &str,
        transformer: Arc<dyn FileTransformer>,
    ) {
        let pattern = Self::normalize(file_name_pattern).to_owned();
        let mut regs = self
            .registrations
            .write()
            .expect("registrations lock poisoned");
        // Idempotent per (id, pattern), like the upstream pipeline insert.
        if regs.iter().any(|r| r.id == id && r.pattern == pattern) {
            return;
        }
        if !admits(&regs, id, &pattern, 0) {
            return;
        }
        tracing::info!(%id, pattern, "registered file transformation");
        regs.push(Registration {
            id,
            pattern,
            kind: TransformKind::Builtin(transformer),
        });
    }

    async fn add_endpoint_transformation(&self, id: Uuid, file_name_pattern: &str, endpoint: &str) {
        let pattern = Self::normalize(file_name_pattern).to_owned();
        let mut regs = self
            .registrations
            .write()
            .expect("registrations lock poisoned");
        if regs.iter().any(|r| r.id == id && r.pattern == pattern) {
            return;
        }
        if !admits(&regs, id, &pattern, endpoint.len()) {
            return;
        }
        tracing::info!(%id, pattern, endpoint, "registered endpoint file transformation");
        regs.push(Registration {
            id,
            pattern,
            kind: TransformKind::Endpoint(endpoint.to_owned()),
        });
    }

    async fn remove_transformation(&self, id: Uuid) {
        self.registrations
            .write()
            .expect("registrations lock poisoned")
            .retain(|r| r.id != id);
    }
}

/// A pipeline step snapshotted out of the registration lock (so the async
/// transformer calls run without holding it).
enum TransformStep {
    /// An in-process transformer.
    Builtin(Arc<dyn FileTransformer>),
    /// An HTTP-callback endpoint URL.
    Endpoint(String),
}

// ---------------------------------------------------------------------------
// Intro Skipper's skip-button transformer (its Injector.FileTransformer)
// ---------------------------------------------------------------------------

/// The timeout assignment in jellyfin-web's `showSkipButton`
/// (`Injector.TimeoutAssignmentPattern`, copied verbatim).
const TIMEOUT_ASSIGNMENT_PATTERN: &str =
    r"\(t\.hideTimeout=setTimeout\(t\.hideSkipButton\.bind\(t\)\,8e3\)\)";

/// The timeout check in `hideSkipButton` (`Injector.TimeoutOsdChangePattern`).
const TIMEOUT_OSD_CHANGE_PATTERN: &str = r"\:this\.hideTimeout\|\|this\.hideSkipButton\(\)";

/// The focusability check (`Injector.FocusabilityAssignmentPattern`).
const FOCUSABILITY_ASSIGNMENT_PATTERN: &str = r"(?:(?:var)\s+)?r\s*=\s*document\.activeElement\s*&&\s*[A-Za-z_$][\w$]*\.A\.isCurrentlyFocusable\(document\.activeElement\)";

/// Milliseconds per second.
const MS_PER_SECOND: i64 = 1000;

/// The Intro Skipper configuration fields the transformer reads.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct SkipButtonConfig {
    use_file_transformation_plugin: bool,
    skipbutton_hide_delay: i64,
}

/// Intro Skipper's `main.jellyfin.bundle.js` patch — a faithful port of its
/// `Injector.FileTransformer`: replaces the hardcoded 8-second skip-button
/// auto-hide with the configured `SkipbuttonHideDelay` (0 ⇒ the button
/// persists) and gates the button's focus grab on playback having started.
///
/// Reads the Intro Skipper configuration on every run (the upstream callback
/// does the same via `Plugin.Instance.Configuration`), so toggling
/// `UseFileTransformationPlugin` or the delay applies without a restart.
pub struct SkipButtonTransformer {
    plugins: Arc<dyn PluginManager>,
}

impl SkipButtonTransformer {
    /// Builds the transformer over the plugin manager (for the live Intro
    /// Skipper configuration).
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginManager>) -> Self {
        Self { plugins }
    }

    /// Applies the three regex patches for `hide_delay_secs` (0 or invalid ⇒
    /// persist). Pure — the oracle for the unit tests.
    fn apply(contents: &str, hide_delay_secs: i64) -> String {
        // Port of `TryGetValidTimeoutMs`: a non-positive (or overflowing)
        // delay means the button persists instead of auto-hiding.
        let hide_delay_ms = hide_delay_secs
            .checked_mul(MS_PER_SECOND)
            .filter(|_| hide_delay_secs > 0);
        let persist = if hide_delay_ms.is_none() {
            "true"
        } else {
            "false"
        };
        let delay = hide_delay_ms.unwrap_or(0);

        let timeout_assignment =
            regex::Regex::new(TIMEOUT_ASSIGNMENT_PATTERN).expect("valid timeout pattern");
        let updated = timeout_assignment.replace_all(
            contents,
            format!("{persist}||(t.hideTimeout=setTimeout(t.hideSkipButton.bind(t),{delay}))")
                .as_str(),
        );

        let osd_change = regex::Regex::new(TIMEOUT_OSD_CHANGE_PATTERN).expect("valid osd pattern");
        let updated = osd_change.replace_all(
            &updated,
            format!(":{persist}||this.hideTimeout||this.hideSkipButton()").as_str(),
        );

        let focusability =
            regex::Regex::new(FOCUSABILITY_ASSIGNMENT_PATTERN).expect("valid focus pattern");
        focusability
            .replace_all(
                &updated,
                format!("${{0}}&&t.playbackManager.currentTime()>{MS_PER_SECOND}").as_str(),
            )
            .into_owned()
    }
}

#[async_trait]
impl FileTransformer for SkipButtonTransformer {
    async fn transform(&self, _path: &str, contents: String) -> String {
        // Self-gate on the Intro Skipper being enabled and opted in — the
        // upstream callback returns the contents untouched when
        // `UseFileTransformationPlugin` is off.
        let enabled = self
            .plugins
            .get_plugin(INTRO_SKIPPER_ID)
            .await
            .ok()
            .flatten()
            .is_some_and(|p| p.enabled);
        if !enabled {
            return contents;
        }
        let config = match self
            .plugins
            .get_plugin_configuration(INTRO_SKIPPER_ID)
            .await
        {
            Ok(bytes) => serde_json::from_slice::<SkipButtonConfig>(&bytes).unwrap_or_default(),
            Err(_) => return contents,
        };
        if !config.use_file_transformation_plugin {
            return contents;
        }
        Self::apply(&contents, config.skipbutton_hide_delay)
    }
}

/// The Intro Skipper's transformation registration: patches jellyfin-web's
/// `main.jellyfin.bundle.js` (the upstream `InitializeWebInjector` payload).
/// Called by the composition root after the service is built.
pub async fn register_skip_button_transformer(
    service: &dyn FileTransformationService,
    plugins: Arc<dyn PluginManager>,
) {
    service
        .add_transformation(
            INTRO_SKIPPER_ID,
            "main.jellyfin.bundle.js",
            Arc::new(SkipButtonTransformer::new(plugins)),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrofin_model::updates::{PackageInfo, RepositoryInfo};
    use ferrofin_traits::error::ServiceError;

    /// A plugin manager whose FT + Intro Skipper plugins are enabled with the
    /// given configurations.
    struct FakePlugins {
        ft_config: Vec<u8>,
        is_config: Vec<u8>,
        enabled: bool,
    }

    #[async_trait]
    impl PluginManager for FakePlugins {
        async fn list_plugins(&self) -> Result<Vec<PluginDescriptor>, ServiceError> {
            Ok(Vec::new())
        }
        async fn get_plugin(&self, id: Uuid) -> Result<Option<PluginDescriptor>, ServiceError> {
            Ok(Some(PluginDescriptor {
                id,
                enabled: self.enabled,
                ..PluginDescriptor::default()
            }))
        }
        async fn enable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn disable_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn remove_plugin(&self, _id: Uuid) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn get_plugin_configuration(&self, id: Uuid) -> Result<Vec<u8>, ServiceError> {
            Ok(if id == EXTENSION_ID {
                self.ft_config.clone()
            } else {
                self.is_config.clone()
            })
        }
        async fn set_plugin_configuration(
            &self,
            _id: Uuid,
            _config: Vec<u8>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn plugin_image(
            &self,
            _id: Uuid,
        ) -> Result<Option<ferrofin_traits::plugins::PluginImage>, ServiceError> {
            Ok(None)
        }
        async fn get_repositories(&self) -> Result<Vec<RepositoryInfo>, ServiceError> {
            Ok(Vec::new())
        }
        async fn set_repositories(
            &self,
            _repositories: Vec<RepositoryInfo>,
        ) -> Result<(), ServiceError> {
            Ok(())
        }
        async fn list_packages(&self) -> Result<Vec<PackageInfo>, ServiceError> {
            Ok(Vec::new())
        }
    }

    fn service_with(ft_config: &str, enabled: bool) -> WebFileTransformationService {
        WebFileTransformationService::new(
            Arc::new(FakePlugins {
                ft_config: ft_config.as_bytes().to_vec(),
                is_config: b"{}".to_vec(),
                enabled,
            }),
            "http://127.0.0.1:0".to_owned(),
        )
    }

    /// A transformer that upper-cases the contents.
    struct Upper;
    #[async_trait]
    impl FileTransformer for Upper {
        async fn transform(&self, _path: &str, contents: String) -> String {
            contents.to_uppercase()
        }
    }

    #[tokio::test]
    async fn exact_and_regex_patterns_match() {
        let svc = service_with(r#"{"Transformations":[]}"#, true);
        svc.add_transformation(
            Uuid::from_u128(1),
            "main.jellyfin.bundle.js",
            Arc::new(Upper),
        )
        .await;
        svc.add_transformation(Uuid::from_u128(2), r".*\.chunk\.js", Arc::new(Upper))
            .await;
        assert!(svc.needs_transformation("main.jellyfin.bundle.js").await);
        assert!(svc.needs_transformation("/main.jellyfin.bundle.js").await);
        assert!(svc.needs_transformation("123.chunk.js").await);
        assert!(!svc.needs_transformation("index.html").await);
        assert_eq!(
            svc.run_transformation("main.jellyfin.bundle.js", "abc".to_owned())
                .await,
            "ABC"
        );
        // A non-matching path passes through untouched.
        assert_eq!(
            svc.run_transformation("index.html", "abc".to_owned()).await,
            "abc"
        );
    }

    #[tokio::test]
    async fn disabled_plugin_disables_the_pipeline() {
        let svc = service_with(r#"{"Transformations":[]}"#, false);
        svc.add_transformation(Uuid::from_u128(1), "a.js", Arc::new(Upper))
            .await;
        assert!(!svc.needs_transformation("a.js").await);
        assert_eq!(
            svc.run_transformation("a.js", "abc".to_owned()).await,
            "abc"
        );
    }

    #[tokio::test]
    async fn remove_transformation_unregisters_by_id() {
        let svc = service_with(r#"{"Transformations":[]}"#, true);
        svc.add_transformation(Uuid::from_u128(1), "a.js", Arc::new(Upper))
            .await;
        svc.remove_transformation(Uuid::from_u128(1)).await;
        assert!(!svc.needs_transformation("a.js").await);
    }

    #[tokio::test]
    async fn config_defined_search_replace_applies_live() {
        let cfg = r#"{"Transformations":[{"Id":"00000000-0000-0000-0000-000000000001","FilenamePattern":"index.html","SearchText":"</body>","ReplaceText":"<script></script></body>"}]}"#;
        let svc = service_with(cfg, true);
        assert!(svc.needs_transformation("index.html").await);
        let out = svc
            .run_transformation("index.html", "<body></body>".to_owned())
            .await;
        assert_eq!(out, "<body><script></script></body>");
        // Blank search/replace entries are skipped, not applied as empties.
        let blank = r#"{"Transformations":[{"Id":"00000000-0000-0000-0000-000000000002","FilenamePattern":"index.html","SearchText":"","ReplaceText":"x"}]}"#;
        let svc = service_with(blank, true);
        assert!(svc.needs_transformation("index.html").await);
        assert_eq!(
            svc.run_transformation("index.html", "abc".to_owned()).await,
            "abc"
        );
    }

    // ---- SkipButtonTransformer: the C# Injector as the oracle ---------------

    /// The exact snippet `TimeoutAssignmentPattern` targets in jellyfin-web.
    const TIMEOUT_SNIPPET: &str = "(t.hideTimeout=setTimeout(t.hideSkipButton.bind(t),8e3))";
    /// The exact snippet `TimeoutOsdChangePattern` targets.
    const OSD_SNIPPET: &str = ":this.hideTimeout||this.hideSkipButton()";
    /// A snippet `FocusabilityAssignmentPattern` matches.
    const FOCUS_SNIPPET: &str =
        "var r=document.activeElement&&o.A.isCurrentlyFocusable(document.activeElement)";

    #[test]
    fn skip_button_patch_replaces_the_hardcoded_timeout() {
        let input = format!("{TIMEOUT_SNIPPET};{OSD_SNIPPET};{FOCUS_SNIPPET}");
        let out = SkipButtonTransformer::apply(&input, 5);
        assert!(
            out.contains("false||(t.hideTimeout=setTimeout(t.hideSkipButton.bind(t),5000))"),
            "timeout patch missing: {out}"
        );
        assert!(
            out.contains(":false||this.hideTimeout||this.hideSkipButton()"),
            "osd patch missing: {out}"
        );
        assert!(
            out.contains("&&t.playbackManager.currentTime()>1000"),
            "focusability patch missing: {out}"
        );
    }

    #[test]
    fn skip_button_zero_delay_persists_the_button() {
        let out = SkipButtonTransformer::apply(TIMEOUT_SNIPPET, 0);
        assert!(
            out.contains("true||(t.hideTimeout=setTimeout(t.hideSkipButton.bind(t),0))"),
            "persist patch missing: {out}"
        );
    }

    #[tokio::test]
    async fn skip_button_transformer_gates_on_opt_in() {
        // UseFileTransformationPlugin=false → contents untouched.
        let plugins = Arc::new(FakePlugins {
            ft_config: b"{}".to_vec(),
            is_config: br#"{"UseFileTransformationPlugin":false,"SkipbuttonHideDelay":5}"#.to_vec(),
            enabled: true,
        });
        let t = SkipButtonTransformer::new(plugins);
        let out = t
            .transform("main.jellyfin.bundle.js", TIMEOUT_SNIPPET.to_owned())
            .await;
        assert_eq!(out, TIMEOUT_SNIPPET);

        // Opted in → patched.
        let plugins = Arc::new(FakePlugins {
            ft_config: b"{}".to_vec(),
            is_config: br#"{"UseFileTransformationPlugin":true,"SkipbuttonHideDelay":5}"#.to_vec(),
            enabled: true,
        });
        let t = SkipButtonTransformer::new(plugins);
        let out = t
            .transform("main.jellyfin.bundle.js", TIMEOUT_SNIPPET.to_owned())
            .await;
        assert!(out.contains("5000"), "expected patched timeout: {out}");
    }

    #[test]
    fn extension_default_config_round_trips() {
        let bytes = FileTransformationExtension.default_config();
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(v["DebugLoggingState"], "Disabled");
        assert!(v["Transformations"].as_array().expect("array").is_empty());
    }

    /// The registry's length in a test (the field is private to this module).
    fn len_of(svc: &WebFileTransformationService) -> usize {
        svc.registrations.read().expect("lock").len()
    }

    /// The registry is process-lifetime state with no sweeper, and both halves
    /// of its idempotence key come from the caller — so an unbounded registry
    /// grows by one permanent entry per request. Measured before this cap
    /// existed: 150 `POST /FileTransformation/RegisterTransformation` calls
    /// carrying 1 MB of strings each raised the server's RssAnon by 157 MB,
    /// linearly and with no plateau.
    #[tokio::test]
    async fn registry_stops_growing_at_the_cap() {
        let svc = service_with("{}", true);
        for i in 0..(MAX_REGISTRATIONS + 50) {
            svc.add_endpoint_transformation(
                Uuid::from_u128(i as u128),
                &format!("pattern-{i}"),
                "http://127.0.0.1:1/cb",
            )
            .await;
        }
        assert_eq!(
            len_of(&svc),
            MAX_REGISTRATIONS,
            "an unbounded registry would hold every registration"
        );
    }

    /// The same cap must hold for in-process registrations, so a compiled-in
    /// extension registering in a loop cannot outgrow it either.
    #[tokio::test]
    async fn builtin_registrations_share_the_cap() {
        let svc = service_with("{}", true);
        let plugins: Arc<dyn PluginManager> = Arc::new(FakePlugins {
            ft_config: b"{}".to_vec(),
            is_config: b"{}".to_vec(),
            enabled: true,
        });
        for i in 0..(MAX_REGISTRATIONS + 10) {
            svc.add_transformation(
                Uuid::from_u128(i as u128),
                &format!("builtin-{i}"),
                Arc::new(SkipButtonTransformer::new(Arc::clone(&plugins))),
            )
            .await;
        }
        assert_eq!(len_of(&svc), MAX_REGISTRATIONS);
    }

    /// A single registration can carry megabytes of caller-supplied string and
    /// keep them for the life of the process, so the strings are capped too —
    /// an over-long one is refused outright rather than stored truncated.
    #[tokio::test]
    async fn over_long_pattern_and_endpoint_are_refused() {
        let svc = service_with("{}", true);
        svc.add_endpoint_transformation(
            Uuid::from_u128(1),
            &"p".repeat(MAX_PATTERN_LEN + 1),
            "http://127.0.0.1:1/cb",
        )
        .await;
        assert_eq!(len_of(&svc), 0, "an over-long pattern must not be stored");

        svc.add_endpoint_transformation(
            Uuid::from_u128(2),
            "ok-pattern",
            &"e".repeat(MAX_ENDPOINT_LEN + 1),
        )
        .await;
        assert_eq!(len_of(&svc), 0, "an over-long endpoint must not be stored");

        // A registration inside both limits still lands.
        svc.add_endpoint_transformation(Uuid::from_u128(3), "ok-pattern", "http://127.0.0.1:1/cb")
            .await;
        assert_eq!(len_of(&svc), 1);
    }

    /// Refusing at the cap must not evict what is already registered: the
    /// Intro Skipper's compiled-in patch has to keep working while a noisy
    /// caller is being refused.
    #[tokio::test]
    async fn a_full_registry_still_serves_its_existing_registrations() {
        let svc = service_with("{}", true);
        svc.add_endpoint_transformation(Uuid::from_u128(0), "keep.me.js", "http://127.0.0.1:1/cb")
            .await;
        for i in 1..(MAX_REGISTRATIONS + 20) {
            svc.add_endpoint_transformation(
                Uuid::from_u128(i as u128),
                &format!("noise-{i}"),
                "http://127.0.0.1:1/cb",
            )
            .await;
        }
        assert!(
            svc.needs_transformation("keep.me.js").await,
            "the first registration must survive the flood"
        );
    }
}
