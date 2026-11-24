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
//!   transformations from the plugin configuration (kept in memory and read
//!   again after each save, so a config save applies without a restart);
//! - [`FileTransformationExtension`] — the `/Plugins` presentation + the
//!   vendored upstream settings page;
//! - [`SkipButtonTransformer`] — Intro Skipper's `Injector.FileTransformer`,
//!   its one in-process consumer (registered by the composition root).
//!
//! Not ported: .NET assembly-reflection callbacks and named-pipe callbacks
//! (both are .NET-process-specific; compiled-in extensions register natively
//! through the [`FileTransformationService`] trait instead).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use ferrofin_core::event_manager::FerrofinEventManager;
use ferrofin_core::plugin_manager::PLUGIN_CONFIGURATION_CHANGED;
use ferrofin_core::{PluginConfigPage, ScheduledTask};
use ferrofin_traits::plugins::{
    FileTransformationService, FileTransformer, PluginDescriptor, PluginManager,
};
use ferrofin_traits::system::LocalApiAccess;
use ferrofin_util::http_client::HTTP_CLIENT_DEFAULT_TIMEOUT;
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
    /// `pattern` compiled once at registration (`None`: not a regex, so it
    /// matches only exactly), instead of on every served `/web` file.
    regex: Option<regex::Regex>,
    kind: TransformKind,
}

impl Registration {
    fn new(id: Uuid, pattern: String, kind: TransformKind) -> Self {
        let regex = regex::Regex::new(&pattern).ok();
        Self {
            id,
            pattern,
            regex,
            kind,
        }
    }

    /// Whether this registration applies to `path` ([`pattern_matches`]).
    fn matches(&self, path: &str) -> bool {
        pattern_matches(&self.pattern, self.regex.as_ref(), path)
    }
}

/// Whether `pattern` matches `path`: exact (case-sensitive, like the
/// upstream dictionary key) or as `regex`, the pattern compiled (`None`
/// when it is not a regex). Port of the exact-`ContainsKey`-then-regex match
/// in `NeedsTransformation`.
fn pattern_matches(pattern: &str, regex: Option<&regex::Regex>, path: &str) -> bool {
    pattern == path || regex.is_some_and(|r| r.is_match(path))
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

/// The most the pipeline serves for one file, in bytes: a callback's answer
/// is read no further, and a step whose output would pass it is skipped,
/// leaving the contents as they were.
///
/// The pipeline runs on every anonymous `/web` GET of a matching file, so
/// this bounds what one request can make the server allocate. jellyfin-web's
/// bundles are a few MiB, far below it. Owner decision (gl-933, 2026-10-25):
/// 16 MiB, one cap for the callback body and the pipeline's output, not a
/// setting. An accepted hardening divergence: the plugin reads a callback's
/// whole answer and applies every search/replace.
const MAX_TRANSFORMED_BYTES: usize = 16 * 1024 * 1024;

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
    /// `filename_pattern` compiled once, when the configuration is read
    /// (`None`: not a regex, so it matches only exactly).
    #[serde(skip)]
    regex: Option<regex::Regex>,
}

impl Default for ConfiguredTransformation {
    fn default() -> Self {
        Self {
            id: Uuid::nil(),
            filename_pattern: String::new(),
            search_text: String::new(),
            replace_text: String::new(),
            regex: None,
        }
    }
}

impl ConfiguredTransformation {
    /// Whether this transformation applies to `path` ([`pattern_matches`]).
    fn matches(&self, path: &str) -> bool {
        pattern_matches(&self.filename_pattern, self.regex.as_ref(), path)
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
/// first use and kept, each pattern compiled, until the next save ([`subscribe`]),
/// as upstream re-registers them from its `UpdateConfiguration` hook.
pub struct WebFileTransformationService {
    registrations: RwLock<Vec<Registration>>,
    plugins: Arc<dyn PluginManager>,
    /// Where this server's API is reached locally (`GetApiUrlForLocalAccess()`),
    /// for resolving relative HTTP-callback endpoints.
    local_api: Arc<dyn LocalApiAccess>,
    http: reqwest::Client,
    /// How long an HTTP-callback exchange may take, body included: the
    /// plugin's `new HttpClient()` keeps .NET's default `Timeout`
    /// ([`HTTP_CLIENT_DEFAULT_TIMEOUT`]; a test shortens it).
    endpoint_timeout: Duration,
    /// The registrations, by `(id, pattern)`, whose last callback failed.
    /// The pipeline runs on every matching `/web` request, so a failure
    /// warns the first time and logs at `debug` while it repeats; a success,
    /// or removing the registration, clears it.
    failing: Mutex<HashSet<(Uuid, String)>>,
    /// The admin-defined transformations, read on first use and forgotten
    /// on a configuration save ([`subscribe`]). Every `/web` request asks for
    /// them, and a save must reach the next one, so a load holds the lock: a
    /// save landing during it is forgotten after it, never before.
    configured: tokio::sync::Mutex<Option<Arc<[ConfiguredTransformation]>>>,
}

impl WebFileTransformationService {
    /// Builds the service over the plugin manager (for the enabled gate and
    /// the admin-defined transformations) and the host that says where this
    /// server's API is reached locally.
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginManager>, local_api: Arc<dyn LocalApiAccess>) -> Self {
        Self {
            registrations: RwLock::new(Vec::new()),
            plugins,
            local_api,
            // The plugin's `new HttpClient()` decodes no compressed body.
            http: reqwest::Client::builder()
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .build()
                .unwrap_or_default(),
            endpoint_timeout: HTTP_CLIENT_DEFAULT_TIMEOUT,
            failing: Mutex::default(),
            configured: tokio::sync::Mutex::new(None),
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

    /// The admin-defined transformations from the plugin configuration, kept
    /// from the first read until the next save; none when it cannot be read,
    /// which is not kept, so a transient failure does not hide them.
    async fn configured_transformations(&self) -> Arc<[ConfiguredTransformation]> {
        let mut configured = self.configured.lock().await;
        if let Some(transformations) = &*configured {
            return Arc::clone(transformations);
        }
        let Ok(bytes) = self.plugins.get_plugin_configuration(EXTENSION_ID).await else {
            return Arc::from([]);
        };
        let mut transformations =
            serde_json::from_slice::<ServiceConfig>(&bytes).map_or_default(|c| c.transformations);
        for t in &mut transformations {
            t.regex = regex::Regex::new(&t.filename_pattern).ok();
        }
        let transformations: Arc<[_]> = transformations.into();
        *configured = Some(Arc::clone(&transformations));
        transformations
    }

    /// Forgets the kept admin-defined transformations, so the next request
    /// reads the saved configuration.
    async fn configuration_saved(&self) {
        *self.configured.lock().await = None;
    }

    /// Strips the leading `/` (upstream `NormalizePath`).
    fn normalize(path: &str) -> &str {
        path.trim_start_matches('/')
    }

    /// Invokes an HTTP-callback endpoint with `contents`, returning the
    /// response body, or why it failed (the caller leaves the contents
    /// unchanged). Port of the endpoint branch of
    /// `TransformationHelper.ApplyTransformation`.
    ///
    /// A relative endpoint is a path on this server. The plugin sends it to
    /// the server's `PublishedServerUrl` when one is set, else to
    /// `http://localhost:{HttpPort}`; Ferrofin resolves it against the local
    /// access URL over HTTP instead, which carries the base URL (without it
    /// the request is redirected to the web client) and reaches a listener
    /// pinned to one address. Plain HTTP, like the plugin's, because the
    /// server's own certificate need not be one this client trusts. With
    /// Require HTTPS on, the callback is redirected to HTTPS like any request,
    /// as the plugin's is.
    async fn call_endpoint(&self, endpoint: &str, contents: &str) -> Result<String, String> {
        let url = if endpoint.starts_with("http") {
            endpoint.to_owned()
        } else {
            let base = self
                .local_api
                .get_http_api_url_for_local_access()
                .await
                .map_err(|e| format!("no local api url: {e}"))?;
            format!("{base}/{}", endpoint.trim_start_matches('/'))
        };
        // `PostAsync` reads the whole response before it returns, so the
        // plugin's `HttpClient.Timeout` bounds the body read too.
        let response = self
            .http
            .post(&url)
            .timeout(self.endpoint_timeout)
            .json(&serde_json::json!({ "contents": contents }))
            .send()
            .await;
        match response {
            // The plugin writes the body whatever the status; an error
            // answer's body never replaces the file here (a plugin bug, not
            // ported).
            Ok(resp) if !resp.status().is_success() => {
                Err(format!("answered {}", resp.status().as_u16()))
            }
            Ok(resp) => read_capped(resp).await,
            Err(e) => Err(e.to_string()),
        }
    }

    /// Clears step `(id, pattern)`'s failure ([`Self::first_failure`]).
    fn step_worked(&self, id: Uuid, pattern: &str) {
        let mut failing = self.failing.lock().unwrap_or_else(PoisonError::into_inner);
        if !failing.is_empty() {
            failing.remove(&(id, pattern.to_owned()));
        }
    }

    /// Records a failure of step `(id, pattern)` (a registration, or an
    /// admin-defined search/replace), answering whether it is the first since
    /// the step last worked: that one is a `warn`, a repeat a `debug`
    /// (LOGGING.md §1).
    fn first_failure(&self, id: Uuid, pattern: &str) -> bool {
        self.failing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((id, pattern.to_owned()))
    }

    /// Logs once ([`Self::first_failure`]) that step `(id, pattern)` was
    /// skipped because it would grow the file to `len` bytes, past
    /// [`MAX_TRANSFORMED_BYTES`].
    fn step_over_cap(&self, id: Uuid, pattern: &str, len: usize) {
        if self.first_failure(id, pattern) {
            tracing::warn!(
                %id,
                pattern,
                len,
                max = MAX_TRANSFORMED_BYTES,
                "transformation would grow the file past the size cap; serving it without this step"
            );
        } else {
            tracing::debug!(%id, pattern, len, "transformation skipped: past the size cap");
        }
    }

    /// Logs registration `(id, pattern)`'s callback failure once
    /// ([`Self::first_failure`]).
    fn endpoint_failed(&self, id: Uuid, pattern: &str, endpoint: &str, error: &str) {
        if self.first_failure(id, pattern) {
            tracing::warn!(
                %id,
                pattern,
                endpoint,
                error,
                "transformation endpoint failed; serving the file unchanged until it works"
            );
        } else {
            tracing::debug!(%id, pattern, endpoint, error, "transformation endpoint failed");
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
            .any(|r| r.matches(path))
        {
            return true;
        }
        self.configured_transformations()
            .await
            .iter()
            .any(|t| t.matches(path))
    }

    async fn would_transform(&self, path: &str) -> bool {
        if !self.plugin_enabled().await {
            return false;
        }
        let path = Self::normalize(path);
        // A matching in-process step that would change nothing (the Intro
        // Skipper's patch while its opt-in is off, the default) does not
        // count. An endpoint's answer is unknown until it is called.
        let builtins: Vec<Arc<dyn FileTransformer>> = {
            let regs = self
                .registrations
                .read()
                .expect("registrations lock poisoned");
            let mut builtins = Vec::new();
            for r in regs.iter().filter(|r| r.matches(path)) {
                match &r.kind {
                    TransformKind::Builtin(t) => builtins.push(Arc::clone(t)),
                    TransformKind::Endpoint(_) => return true,
                }
            }
            builtins
        };
        for t in builtins {
            if t.would_transform(path).await {
                return true;
            }
        }
        self.configured_transformations()
            .await
            .iter()
            .any(|t| t.matches(path))
    }

    async fn run_transformation(&self, path: &str, contents: String) -> String {
        if !self.plugin_enabled().await {
            return contents;
        }
        let path = Self::normalize(path).to_owned();

        // Snapshot the matching registered pipeline steps (in registration
        // order — the upstream per-pattern pipeline), then the admin-defined
        // search/replace steps.
        let steps: Vec<(Uuid, String, TransformStep)> = {
            let regs = self
                .registrations
                .read()
                .expect("registrations lock poisoned");
            regs.iter()
                .filter(|r| r.matches(&path))
                .map(|r| {
                    let step = match &r.kind {
                        TransformKind::Builtin(t) => TransformStep::Builtin(Arc::clone(t)),
                        TransformKind::Endpoint(url) => TransformStep::Endpoint(url.clone()),
                    };
                    (r.id, r.pattern.clone(), step)
                })
                .collect()
        };

        let mut contents = contents;
        for (id, pattern, step) in steps {
            contents = match step {
                TransformStep::Builtin(t) => match t.transform(&path, &contents).await {
                    Some(transformed)
                        if transformed.len() > MAX_TRANSFORMED_BYTES
                            && transformed.len() > contents.len() =>
                    {
                        self.step_over_cap(id, &pattern, transformed.len());
                        contents
                    }
                    transformed => {
                        self.step_worked(id, &pattern);
                        transformed.unwrap_or(contents)
                    }
                },
                TransformStep::Endpoint(url) => match self.call_endpoint(&url, &contents).await {
                    Ok(transformed) => {
                        self.step_worked(id, &pattern);
                        transformed
                    }
                    Err(error) => {
                        self.endpoint_failed(id, &pattern, &url, &error);
                        contents
                    }
                },
            };
        }

        // Admin-defined search/replace (skipped when either side is blank,
        // matching `HandlePluginConfigTransformation`). A common search with
        // a long replace multiplies the file, so one that would grow it past
        // the cap is skipped.
        for t in self.configured_transformations().await.iter() {
            if t.matches(&path)
                && !t.search_text.trim().is_empty()
                && !t.replace_text.trim().is_empty()
            {
                let growth = t.replace_text.len().saturating_sub(t.search_text.len());
                let grown = if growth == 0 {
                    0
                } else {
                    contents
                        .matches(t.search_text.as_str())
                        .count()
                        .saturating_mul(growth)
                        .saturating_add(contents.len())
                };
                if grown > MAX_TRANSFORMED_BYTES {
                    self.step_over_cap(t.id, &t.filename_pattern, grown);
                } else {
                    self.step_worked(t.id, &t.filename_pattern);
                    contents = contents.replace(&t.search_text, &t.replace_text);
                }
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
        regs.push(Registration::new(
            id,
            pattern,
            TransformKind::Builtin(transformer),
        ));
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
        regs.push(Registration::new(
            id,
            pattern,
            TransformKind::Endpoint(endpoint.to_owned()),
        ));
    }

    async fn remove_transformation(&self, id: Uuid) {
        self.registrations
            .write()
            .expect("registrations lock poisoned")
            .retain(|r| r.id != id);
        self.failing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(failed, _)| *failed != id);
    }
}

/// Reads a callback's answer, giving up before buffering more than
/// [`MAX_TRANSFORMED_BYTES`]: at once for a `Content-Length` over it, else as
/// the body streams in. Decoded as UTF-8, lossily, as `text()` decodes in
/// this build (reqwest's `charset` feature is off); each invalid byte becomes
/// a 3-byte U+FFFD, so the decoded text is held to the cap as well.
async fn read_capped(mut resp: reqwest::Response) -> Result<String, String> {
    let over_cap = || format!("answered more than the {MAX_TRANSFORMED_BYTES}-byte cap");
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_TRANSFORMED_BYTES as u64)
    {
        return Err(over_cap());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("body read failed: {e}"))?
    {
        if body.len() + chunk.len() > MAX_TRANSFORMED_BYTES {
            return Err(over_cap());
        }
        body.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(body)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    if text.len() > MAX_TRANSFORMED_BYTES {
        return Err(over_cap());
    }
    Ok(text)
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
/// (`Injector.TimeoutAssignmentPattern`, copied verbatim), compiled once.
static TIMEOUT_ASSIGNMENT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\(t\.hideTimeout=setTimeout\(t\.hideSkipButton\.bind\(t\)\,8e3\)\)")
        .expect("valid timeout pattern")
});

/// The timeout check in `hideSkipButton` (`Injector.TimeoutOsdChangePattern`).
static TIMEOUT_OSD_CHANGE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"\:this\.hideTimeout\|\|this\.hideSkipButton\(\)")
        .expect("valid osd pattern")
});

/// The focusability check (`Injector.FocusabilityAssignmentPattern`).
static FOCUSABILITY_ASSIGNMENT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?:(?:var)\s+)?r\s*=\s*document\.activeElement\s*&&\s*[A-Za-z_$][\w$]*\.A\.isCurrentlyFocusable\(document\.activeElement\)")
        .expect("valid focus pattern")
});

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

        let updated = TIMEOUT_ASSIGNMENT.replace_all(
            contents,
            format!("{persist}||(t.hideTimeout=setTimeout(t.hideSkipButton.bind(t),{delay}))")
                .as_str(),
        );

        let updated = TIMEOUT_OSD_CHANGE.replace_all(
            &updated,
            format!(":{persist}||this.hideTimeout||this.hideSkipButton()").as_str(),
        );

        FOCUSABILITY_ASSIGNMENT
            .replace_all(
                &updated,
                format!("${{0}}&&t.playbackManager.currentTime()>{MS_PER_SECOND}").as_str(),
            )
            .into_owned()
    }

    /// The configured `SkipbuttonHideDelay`, or `None` while the Intro Skipper
    /// is disabled or not opted in (`UseFileTransformationPlugin` off), when
    /// the upstream callback returns the contents untouched.
    async fn hide_delay(&self) -> Option<i64> {
        let enabled = self
            .plugins
            .get_plugin(INTRO_SKIPPER_ID)
            .await
            .ok()
            .flatten()
            .is_some_and(|p| p.enabled);
        if !enabled {
            return None;
        }
        let bytes = self
            .plugins
            .get_plugin_configuration(INTRO_SKIPPER_ID)
            .await
            .ok()?;
        let config = serde_json::from_slice::<SkipButtonConfig>(&bytes).unwrap_or_default();
        config
            .use_file_transformation_plugin
            .then_some(config.skipbutton_hide_delay)
    }
}

#[async_trait]
impl FileTransformer for SkipButtonTransformer {
    async fn transform(&self, _path: &str, contents: &str) -> Option<String> {
        Some(Self::apply(contents, self.hide_delay().await?))
    }

    async fn would_transform(&self, _path: &str) -> bool {
        self.hide_delay().await.is_some()
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

/// Has `service` forget its admin-defined transformations on every plugin
/// configuration save (`PluginConfigurationChanged`), the port of the
/// plugin's `UpdateConfiguration` re-registering them. The save awaits it, so
/// what it saved applies from the next `/web` request. Any plugin's save
/// does: saves are rare, and the next request reads the file once. Called
/// once by the composition root.
pub fn subscribe(events: &FerrofinEventManager, service: &Arc<WebFileTransformationService>) {
    let service = Arc::clone(service);
    events.subscribe(
        PLUGIN_CONFIGURATION_CHANGED,
        Arc::new(move |_payload: &str| {
            let service = Arc::clone(&service);
            Box::pin(async move {
                service.configuration_saved().await;
                Ok(())
            })
        }),
    );
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

    /// A host whose local API URL is fixed.
    struct LocalApi(String);

    #[async_trait]
    impl LocalApiAccess for LocalApi {
        /// The HTTPS address a server listening with HTTPS gives; nothing
        /// listens there, so a callback sent to it fails.
        async fn get_api_url_for_local_access(&self) -> Result<String, ServiceError> {
            Ok("https://127.0.0.1:1".to_owned())
        }

        async fn get_http_api_url_for_local_access(&self) -> Result<String, ServiceError> {
            Ok(self.0.clone())
        }
    }

    fn service_with(ft_config: &str, enabled: bool) -> WebFileTransformationService {
        service_at(ft_config, enabled, "http://127.0.0.1:0")
    }

    /// [`service_with`] on a server reached locally at `local_api_url`.
    fn service_at(
        ft_config: &str,
        enabled: bool,
        local_api_url: &str,
    ) -> WebFileTransformationService {
        WebFileTransformationService::new(
            Arc::new(FakePlugins {
                ft_config: ft_config.as_bytes().to_vec(),
                is_config: b"{}".to_vec(),
                enabled,
            }),
            Arc::new(LocalApi(local_api_url.to_owned())),
        )
    }

    /// A transformer that upper-cases the contents.
    struct Upper;
    #[async_trait]
    impl FileTransformer for Upper {
        async fn transform(&self, _path: &str, contents: &str) -> Option<String> {
            Some(contents.to_uppercase())
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
        // A pattern that is not a regex still matches its exact path, and
        // only that.
        svc.add_transformation(Uuid::from_u128(3), "a+(b.js", Arc::new(Upper))
            .await;
        assert!(svc.needs_transformation("a+(b.js").await);
        assert!(!svc.needs_transformation("aa+(b.js").await);
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
        // A configured pattern is a regex; one that does not compile matches
        // only its exact path.
        let patterns = r#"{"Transformations":[{"Id":"00000000-0000-0000-0000-000000000003","FilenamePattern":".*\\.chunk\\.js","SearchText":"a","ReplaceText":"b"},{"Id":"00000000-0000-0000-0000-000000000004","FilenamePattern":"a+(b.js","SearchText":"a","ReplaceText":"b"}]}"#;
        let svc = service_with(patterns, true);
        assert!(svc.needs_transformation("123.chunk.js").await);
        assert!(!svc.needs_transformation("123.chunk.css").await);
        assert!(svc.needs_transformation("a+(b.js").await);
        assert!(!svc.needs_transformation("aa+(b.js").await);
        assert_eq!(
            svc.run_transformation("123.chunk.js", "xay".to_owned())
                .await,
            "xby"
        );
    }

    /// The admin-defined transformations are read once and kept, as upstream
    /// keeps a plugin's configuration: an edit of `config.json` behind the
    /// server's back is not seen, and a save through the plugin manager (its
    /// `PluginConfigurationChanged`) applies from the next request.
    #[tokio::test]
    async fn configured_transformations_are_kept_until_a_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let events = ferrofin_core::event_manager::FerrofinEventManager::new();
        let plugins: Arc<dyn PluginManager> = Arc::new(
            ferrofin_core::FerrofinPluginManager::new(
                crate::registered_plugins(&crate::builtin_extensions()),
                dir.path().to_path_buf(),
            )
            .with_events(Arc::new(events.clone())),
        );
        let svc = Arc::new(WebFileTransformationService::new(
            Arc::clone(&plugins),
            Arc::new(LocalApi("http://127.0.0.1:0".to_owned())),
        ));
        subscribe(&events, &svc);
        let config = |file: &str| {
            format!(
                r#"{{"Transformations":[{{"Id":"00000000-0000-0000-0000-000000000001","FilenamePattern":"{file}","SearchText":"a","ReplaceText":"b"}}]}}"#
            )
        };
        plugins
            .set_plugin_configuration(EXTENSION_ID, config("a.js").into_bytes())
            .await
            .expect("save");
        assert!(svc.needs_transformation("a.js").await);

        std::fs::write(
            dir.path()
                .join(EXTENSION_ID.to_string())
                .join("config.json"),
            config("b.js"),
        )
        .expect("edit on disk");
        assert!(svc.needs_transformation("a.js").await, "kept");
        assert!(!svc.needs_transformation("b.js").await, "kept");

        plugins
            .set_plugin_configuration(EXTENSION_ID, config("c.js").into_bytes())
            .await
            .expect("save");
        assert!(!svc.needs_transformation("a.js").await, "saved");
        assert!(svc.would_transform("c.js").await, "saved");
        assert_eq!(
            svc.run_transformation("c.js", "xay".to_owned()).await,
            "xby"
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
            .transform("main.jellyfin.bundle.js", TIMEOUT_SNIPPET)
            .await;
        assert_eq!(out, None, "left unchanged");

        // Opted in → patched.
        let plugins = Arc::new(FakePlugins {
            ft_config: b"{}".to_vec(),
            is_config: br#"{"UseFileTransformationPlugin":true,"SkipbuttonHideDelay":5}"#.to_vec(),
            enabled: true,
        });
        let t = SkipButtonTransformer::new(plugins);
        let out = t
            .transform("main.jellyfin.bundle.js", TIMEOUT_SNIPPET)
            .await
            .expect("patched");
        assert!(out.contains("5000"), "expected patched timeout: {out}");
    }

    /// The bundle's skip-button step would change it only while the Intro
    /// Skipper is enabled and opted in, read live from its saved settings:
    /// off (the default), the bundle is left to the static server. A no-op
    /// step never hides an admin-defined transformation of the same file.
    #[tokio::test]
    async fn the_skip_button_step_would_transform_only_while_opted_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let events = ferrofin_core::event_manager::FerrofinEventManager::new();
        let plugins: Arc<dyn PluginManager> = Arc::new(
            ferrofin_core::FerrofinPluginManager::new(
                crate::registered_plugins(&crate::builtin_extensions()),
                dir.path().to_path_buf(),
            )
            .with_events(Arc::new(events.clone())),
        );
        let svc = Arc::new(WebFileTransformationService::new(
            Arc::clone(&plugins),
            Arc::new(LocalApi("http://127.0.0.1:0".to_owned())),
        ));
        subscribe(&events, &svc);
        register_skip_button_transformer(svc.as_ref(), Arc::clone(&plugins)).await;
        let bundle = "main.jellyfin.bundle.js";
        let opt_in = |on: bool| {
            format!(r#"{{"UseFileTransformationPlugin":{on},"SkipbuttonHideDelay":5}}"#)
                .into_bytes()
        };

        // The registration matches, but the opt-in is off by default.
        assert!(svc.needs_transformation(bundle).await);
        assert!(!svc.would_transform(bundle).await, "default");
        plugins
            .set_plugin_configuration(INTRO_SKIPPER_ID, opt_in(true))
            .await
            .expect("save");
        assert!(svc.would_transform(bundle).await, "opted in");
        plugins
            .disable_plugin(INTRO_SKIPPER_ID)
            .await
            .expect("disable");
        assert!(!svc.would_transform(bundle).await, "Intro Skipper disabled");
        plugins
            .enable_plugin(INTRO_SKIPPER_ID)
            .await
            .expect("enable");
        assert!(svc.would_transform(bundle).await, "enabled again");
        plugins
            .set_plugin_configuration(INTRO_SKIPPER_ID, opt_in(false))
            .await
            .expect("save");
        assert!(!svc.would_transform(bundle).await, "opted out");

        // An unreadable configuration: the step leaves the file alone.
        let config = dir
            .path()
            .join(INTRO_SKIPPER_ID.to_string())
            .join("config.json");
        std::fs::remove_file(&config).expect("remove the saved settings");
        std::fs::create_dir(&config).expect("a directory in their place");
        assert!(!svc.would_transform(bundle).await, "unreadable");

        // A step that cannot tell keeps the bundle in the pipeline: an
        // in-process one with the default, and an endpoint.
        let other = WebFileTransformationService::new(
            Arc::clone(&plugins),
            Arc::new(LocalApi("http://127.0.0.1:0".to_owned())),
        );
        register_skip_button_transformer(&other, Arc::clone(&plugins)).await;
        other
            .add_transformation(Uuid::from_u128(1), bundle, Arc::new(Upper))
            .await;
        assert!(other.would_transform(bundle).await, "default step");
        let other = WebFileTransformationService::new(
            Arc::clone(&plugins),
            Arc::new(LocalApi("http://127.0.0.1:0".to_owned())),
        );
        register_skip_button_transformer(&other, Arc::clone(&plugins)).await;
        other
            .add_endpoint_transformation(Uuid::from_u128(2), bundle, "/patch")
            .await;
        assert!(other.would_transform(bundle).await, "endpoint");

        // An admin-defined search/replace on the bundle still counts.
        plugins
            .set_plugin_configuration(
                EXTENSION_ID,
                br#"{"Transformations":[{"Id":"00000000-0000-0000-0000-000000000001","FilenamePattern":"main.jellyfin.bundle.js","SearchText":"a","ReplaceText":"b"}]}"#.to_vec(),
            )
            .await
            .expect("save");
        assert!(svc.would_transform(bundle).await, "admin transformation");

        // The File Transformation plugin disabled: no pipeline at all.
        plugins.disable_plugin(EXTENSION_ID).await.expect("disable");
        assert!(!svc.needs_transformation(bundle).await);
        assert!(!svc.would_transform(bundle).await);
    }

    #[test]
    fn extension_default_config_round_trips() {
        let bytes = FileTransformationExtension.default_config();
        let v: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(v["DebugLoggingState"], "Disabled");
        let transformations = v["Transformations"].as_array().expect("array");
        assert!(transformations.is_empty(), "{transformations:?}");
    }

    /// A callback endpoint answering each request with `status` and `body`,
    /// recording each request's head and body.
    fn spawn_callback(
        status: &'static str,
        body: &'static str,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        spawn_responder(vec![answer(status, body)])
    }

    /// A `Content-Length` response with `status` and `body`.
    fn answer(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// A callback endpoint answering its n-th request with the raw response
    /// `answers[n]` (the last one from then on), recording each request's
    /// head and body.
    fn spawn_responder<A: AsRef<[u8]> + Send + 'static>(
        answers: Vec<A>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read as _, Write as _};
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 8192];
                let read = stream.read(&mut buf).unwrap_or(0);
                log.lock()
                    .expect("lock")
                    .push(String::from_utf8_lossy(&buf[..read]).into_owned());
                let response = &answers[n.min(answers.len() - 1)];
                let _ = stream.write_all(response.as_ref());
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// A callback that answers with an error status leaves the file as it
    /// was: its error body never replaces a served web file. The plugin at
    /// `f4f01c3` writes the body whatever the status
    /// (`TransformationHelper.ApplyTransformation`); serving a callback's error
    /// page as a bundle is a plugin bug, not ported.
    #[tokio::test]
    async fn an_endpoint_error_leaves_the_contents_unchanged() {
        let (base, seen) = spawn_callback("500 Internal Server Error", "oops");
        let svc = service_with("{}", true);
        svc.add_endpoint_transformation(Uuid::from_u128(1), "main.js", &format!("{base}/cb"))
            .await;

        let out = svc
            .run_transformation("main.js", "original".to_owned())
            .await;

        assert_eq!(out, "original");
        assert_eq!(
            seen.lock().expect("lock").len(),
            1,
            "the endpoint was called"
        );
    }

    /// An in-memory sink for a scoped `fmt` subscriber.
    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLogs {
        /// How many lines were logged at `level` (`"WARN"`, `"DEBUG"`).
        fn count(&self, level: &str) -> usize {
            String::from_utf8_lossy(&self.0.lock().expect("logs"))
                .lines()
                .filter(|line| line.split_whitespace().nth(1) == Some(level))
                .count()
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("logs").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The pipeline runs on every matching `/web` request, so a callback
    /// that keeps failing warns once, then logs at `debug` until it has
    /// worked again; a failure after that warns again.
    #[tokio::test]
    async fn a_failing_endpoint_warns_once_until_it_recovers() {
        let logs = CapturedLogs::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .with_env_filter("ferrofin_extensions=debug")
                .finish(),
        );
        let fail = answer("500 Internal Server Error", "oops");
        let (base, _) = spawn_responder(vec![
            fail.clone(),
            fail.clone(),
            fail.clone(),
            answer("200 OK", "patched"),
            fail,
        ]);
        let svc = service_with("{}", true);
        svc.add_endpoint_transformation(Uuid::from_u128(1), "main.js", &format!("{base}/cb"))
            .await;
        let serve = || svc.run_transformation("main.js", "original".to_owned());

        for _ in 0..3 {
            serve().await;
        }
        assert_eq!((logs.count("WARN"), logs.count("DEBUG")), (1, 2));
        assert_eq!(serve().await, "patched");
        serve().await;
        assert_eq!(
            (logs.count("WARN"), logs.count("DEBUG")),
            (2, 2),
            "a failure after a success warns again"
        );
    }

    /// Past the output cap, a callback's answer is not read on: with a
    /// `Content-Length` over it, or streaming past it without one, the file
    /// is served as it was.
    #[tokio::test]
    async fn an_endpoint_answer_over_the_cap_leaves_the_contents_unchanged() {
        let body = "x".repeat(MAX_TRANSFORMED_BYTES + 1);
        for head in [format!("Content-Length: {}\r\n", body.len()), String::new()] {
            let (base, seen) = spawn_responder(vec![format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n{head}Connection: close\r\n\r\n{body}"
            )]);
            let svc = service_with("{}", true);
            svc.add_endpoint_transformation(Uuid::from_u128(1), "main.js", &format!("{base}/cb"))
                .await;

            let out = svc
                .run_transformation("main.js", "original".to_owned())
                .await;

            assert_eq!(out, "original", "{head:?}");
            assert_eq!(seen.lock().expect("lock").len(), 1);
        }
    }

    /// An answer under the cap in bytes that decodes past it: every invalid
    /// UTF-8 byte becomes a 3-byte U+FFFD.
    #[tokio::test]
    async fn an_endpoint_answer_that_decodes_past_the_cap_leaves_the_contents_unchanged() {
        let body = vec![0xFF_u8; MAX_TRANSFORMED_BYTES / 2];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let (base, _) = spawn_responder(vec![response]);
        let svc = service_with("{}", true);
        svc.add_endpoint_transformation(Uuid::from_u128(1), "main.js", &format!("{base}/cb"))
            .await;

        let out = svc
            .run_transformation("main.js", "original".to_owned())
            .await;

        assert_eq!(out, "original");
    }

    /// A transformer that answers more than the output cap.
    struct Huge;
    #[async_trait]
    impl FileTransformer for Huge {
        async fn transform(&self, _path: &str, _contents: &str) -> Option<String> {
            Some("x".repeat(MAX_TRANSFORMED_BYTES + 1))
        }
    }

    /// A step whose output would pass the cap is skipped and the rest still
    /// run: an in-process transformer's answer, and an admin search/replace
    /// whose replace text multiplies the file.
    #[tokio::test]
    async fn a_step_over_the_output_cap_is_skipped() {
        let cfg = format!(
            r#"{{"Transformations":[{{"Id":"00000000-0000-0000-0000-000000000001","FilenamePattern":"main.js","SearchText":"A","ReplaceText":"{}"}},{{"Id":"00000000-0000-0000-0000-000000000002","FilenamePattern":"main.js","SearchText":"B","ReplaceText":"c"}}]}}"#,
            "y".repeat(1024)
        );
        let svc = service_with(&cfg, true);
        svc.add_transformation(Uuid::from_u128(3), "main.js", Arc::new(Huge))
            .await;
        svc.add_transformation(Uuid::from_u128(4), "main.js", Arc::new(Upper))
            .await;
        // After `Upper`, 20 000 matches of a 1 KiB replace: about 20 MiB.
        let input = format!("{}b", "a".repeat(20_000));

        let out = svc.run_transformation("main.js", input).await;

        assert_eq!(out, format!("{}c", "A".repeat(20_000)));
    }

    /// A transformer that leaves every file unchanged.
    struct Unchanged;
    #[async_trait]
    impl FileTransformer for Unchanged {
        async fn transform(&self, _path: &str, _contents: &str) -> Option<String> {
            None
        }
    }

    /// A step answering `None` passes the contents on as they were, and the
    /// steps after it still run.
    #[tokio::test]
    async fn an_unchanged_step_passes_the_contents_on() {
        let svc = service_with(r#"{"Transformations":[]}"#, true);
        svc.add_transformation(Uuid::from_u128(5), "main.js", Arc::new(Unchanged))
            .await;
        assert_eq!(
            svc.run_transformation("main.js", "original".to_owned())
                .await,
            "original"
        );
        svc.add_transformation(Uuid::from_u128(6), "main.js", Arc::new(Upper))
            .await;
        assert_eq!(
            svc.run_transformation("main.js", "original".to_owned())
                .await,
            "ORIGINAL"
        );
    }

    /// A callback endpoint that accepts each connection and reads the request,
    /// then sends `head` (nothing when it is empty) and holds the connection
    /// open without another byte.
    async fn spawn_stalled_callback(head: &'static str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(head.as_bytes()).await;
                    std::future::pending::<()>().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// The plugin's `new HttpClient()` keeps .NET's 100 s `Timeout` over the
    /// whole `PostAsync` (the body is read before it returns), so a callback
    /// that never answers, or stops part-way through its body, leaves the file
    /// as it was instead of holding the request for ever.
    #[tokio::test]
    async fn a_callback_that_never_finishes_leaves_the_contents_unchanged() {
        for head in [
            "",
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 7\r\n\r\npat",
        ] {
            let base = spawn_stalled_callback(head).await;
            let mut svc = service_with("{}", true);
            svc.endpoint_timeout = Duration::from_millis(200);
            svc.add_endpoint_transformation(Uuid::from_u128(1), "main.js", &format!("{base}/cb"))
                .await;

            let out = tokio::time::timeout(
                Duration::from_secs(5),
                svc.run_transformation("main.js", "original".to_owned()),
            )
            .await;

            assert_eq!(
                out.as_deref().ok(),
                Some("original"),
                "the callback must give up ({head:?})"
            );
        }
    }

    /// A relative endpoint is a path on this server, called through its local
    /// access URL — base URL included — and its answer replaces the file.
    #[tokio::test]
    async fn a_relative_endpoint_is_called_through_the_local_api_url() {
        let (base, seen) = spawn_callback("200 OK", "patched");
        let svc = service_at("{}", true, &format!("{base}/jellyfin"));
        svc.add_endpoint_transformation(Uuid::from_u128(1), "main.js", "/Plugin/Transform")
            .await;

        let out = svc
            .run_transformation("main.js", "original".to_owned())
            .await;

        assert_eq!(out, "patched");
        let seen = seen.lock().expect("lock");
        assert!(
            seen[0].starts_with("POST /jellyfin/Plugin/Transform HTTP/1.1"),
            "{}",
            seen[0]
        );
        assert!(
            seen[0].contains(r#"{"contents":"original"}"#),
            "{}",
            seen[0]
        );
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
