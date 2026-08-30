//! Studio Images remote provider — a port of Jellyfin's core `StudioImages`
//! plugin (`MediaBrowser.Providers/Plugins/StudioImages`).
//!
//! For a `Studio` item it supplies a single `Thumb` image fetched from a
//! Jellyfin-compatible artwork repository on GitHub, matched purely by the
//! studio's name (no external id). The default repository is Jellyfin's
//! `emby-artwork`; the flow is:
//!
//! 1. download the manifest `{repo}/thumbs.txt` — the list of studio folder
//!    names the repository has artwork for;
//! 2. normalize the item's studio name and each manifest entry identically
//!    (strip ` . & ! , /`) and match case-insensitively (C#
//!    `GetComparableName` + `OrdinalIgnoreCase`);
//! 3. the thumb URL is `{repo}/images/{match}/thumb.jpg`.
//!
//! The provider is image-only and best-effort: an unmatched studio, or any
//! network failure, yields no image rather than an error — exactly like the
//! upstream provider returning `null`.

use std::sync::{Arc, Mutex};

use ferrofin_traits::plugins::PluginManager;

use crate::plugin_config::{ConfigSource, StudioImagesConfig};

/// The default artwork repository — Jellyfin's `emby-artwork` studios tree
/// (`PluginConfiguration.RepositoryUrl` default).
pub const DEFAULT_REPO_URL: &str =
    "https://raw.githubusercontent.com/jellyfin/emby-artwork/master/studios";

/// The provider name stamped on returned images (C# `Name`).
pub const PROVIDER_NAME: &str = "Artwork Repository";

/// A Studio Images client. Cheap to clone semantics via `Arc` at the call site;
/// holds a process-lifetime cache of the repository manifest.
pub struct StudiosClient {
    http: reqwest::Client,
    /// The operator's explicit repository override (`FERROFIN_STUDIOS_REPO_URL`
    /// / config `studios_repo_url`), trailing slashes trimmed. `None` when the
    /// operator set nothing, which is when the dashboard's `RepositoryUrl`
    /// governs.
    configured_repo_url: Option<String>,
    /// The Studio Images plugin's dashboard settings (`RepositoryUrl`).
    plugin: ConfigSource,
    /// The cached `thumbs.txt` studio-name list, keyed by the repository URL it
    /// was fetched from — the URL is now a runtime setting, so a cache that did
    /// not remember which repo it came from would serve the old repo's names
    /// after an admin changed it.
    // ponytail: cached for the process lifetime (studio artwork changes rarely,
    // a restart re-fetches). Upstream re-fetches after 1 day; add a TTL only if
    // a long-running server must pick up repo changes without a restart.
    manifest: Mutex<Option<(String, Vec<String>)>>,
}

impl std::fmt::Debug for StudiosClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StudiosClient")
            .field("configured_repo_url", &self.configured_repo_url)
            .field(
                "manifest_cached",
                &self.manifest.lock().is_ok_and(|m| m.is_some()),
            )
            .finish_non_exhaustive()
    }
}

impl Default for StudiosClient {
    fn default() -> Self {
        Self::new()
    }
}

impl StudiosClient {
    /// A client against the default `emby-artwork` repository.
    #[must_use]
    pub fn new() -> Self {
        Self::with_repo_url(DEFAULT_REPO_URL)
    }

    /// A client against a custom repository URL (trailing slashes trimmed); an
    /// empty URL leaves the repository to the dashboard setting, and then to
    /// [`DEFAULT_REPO_URL`].
    #[must_use]
    pub fn with_repo_url(repo_url: &str) -> Self {
        let trimmed = repo_url.trim_end_matches('/');
        Self {
            http: reqwest::Client::new(),
            configured_repo_url: (!trimmed.is_empty()).then(|| trimmed.to_owned()),
            plugin: ConfigSource::new(),
            manifest: Mutex::new(None),
        }
    }

    /// Binds the plugin manager, making the Studio Images settings page's
    /// `RepositoryUrl` live. Called once by the composition root.
    pub fn attach_plugin_manager(&self, plugins: Arc<dyn PluginManager>) {
        self.plugin.attach(plugins);
    }

    /// The repository base URL for this lookup.
    ///
    /// `StudiosImageProvider.RepositoryUrl` is
    /// `Plugin.Instance.Configuration.RepositoryUrl` read at call time
    /// (v10.11.8 `StudiosImageProvider.cs:190`), so the dashboard governs. An
    /// operator who set `FERROFIN_STUDIOS_REPO_URL` still wins: an explicit
    /// env/file setting is the more specific instruction, and that knob shipped
    /// before this plugin had an identity. With neither set, the C# default.
    async fn repo_url(&self) -> String {
        if let Some(configured) = &self.configured_repo_url {
            return configured.clone();
        }
        let cfg: StudioImagesConfig = self
            .plugin
            .load(crate::builtin_plugins::STUDIO_IMAGES.id)
            .await;
        let trimmed = cfg.repository_url.trim_end_matches('/');
        if trimmed.is_empty() {
            DEFAULT_REPO_URL.to_owned()
        } else {
            trimmed.to_owned()
        }
    }

    /// The comparable form of a studio name — the C# `GetComparableName`:
    /// remove space, `.`, `&`, `!`, `,`, `/`. Matching is done case-insensitively
    /// on this form, so casing is preserved here.
    fn comparable_name(name: &str) -> String {
        name.chars()
            .filter(|c| !matches!(c, ' ' | '.' | '&' | '!' | ',' | '/'))
            .collect()
    }

    /// The repository manifest (`thumbs.txt`) as a list of studio folder names,
    /// fetched once and cached. Any failure yields an empty list (best-effort).
    async fn manifest(&self, repo_url: &str) -> Vec<String> {
        if let Ok(guard) = self.manifest.lock()
            && let Some((cached_url, cached)) = guard.as_ref()
            && cached_url == repo_url
        {
            return cached.clone();
        }
        let url = format!("{repo_url}/thumbs.txt");
        let list = match self.http.get(&url).send().await {
            Ok(resp) => match resp.text().await {
                Ok(body) => body
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect(),
                Err(e) => {
                    tracing::warn!(url, error = %e, "studios: manifest body read failed");
                    Vec::new()
                }
            },
            Err(e) => {
                tracing::warn!(url, error = %e, "studios: manifest fetch failed");
                Vec::new()
            }
        };
        // Cache even an empty result: a repo that 404s should not be re-hit on
        // every studio; a restart clears it.
        if let Ok(mut guard) = self.manifest.lock() {
            *guard = Some((repo_url.to_owned(), list.clone()));
        }
        list
    }

    /// Seeds the in-memory manifest cache directly, so tests (in this crate and
    /// the manager's) exercise matching without touching the network.
    #[cfg(test)]
    pub(crate) fn seed_manifest(&self, entries: Vec<String>) {
        let url = self
            .configured_repo_url
            .clone()
            .unwrap_or_else(|| DEFAULT_REPO_URL.to_owned());
        *self.manifest.lock().expect("manifest lock") = Some((url, entries));
    }

    /// Downloads `url`, returning its bytes; `None` on any failure
    /// (best-effort, like the metadata clients' image downloads).
    pub async fn download(&self, url: &str) -> Option<Vec<u8>> {
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.bytes().await.ok().map(|b| b.to_vec())
    }

    /// The thumb URL for `studio_name`, or `None` when the repository has no
    /// matching studio. Port of `FindMatch` + `GetUrl`.
    pub async fn thumb_url(&self, studio_name: &str) -> Option<String> {
        let target = Self::comparable_name(studio_name);
        if target.is_empty() {
            return None;
        }
        let repo_url = self.repo_url().await;
        let manifest = self.manifest(&repo_url).await;
        let matched = manifest
            .into_iter()
            .find(|entry| Self::comparable_name(entry).eq_ignore_ascii_case(&target))?;
        Some(format!("{repo_url}/images/{matched}/thumb.jpg"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparable_name_strips_the_c_sharp_punctuation_set() {
        // Verbatim the six characters `GetComparableName` removes; case kept.
        assert_eq!(
            StudiosClient::comparable_name("Walt Disney Pictures"),
            "WaltDisneyPictures"
        );
        assert_eq!(
            StudiosClient::comparable_name("20th Century Fox!"),
            "20thCenturyFox"
        );
        assert_eq!(
            StudiosClient::comparable_name("Tom, Dick & Harry / Co."),
            "TomDickHarryCo"
        );
        // Characters outside the set (e.g. `-`) are preserved.
        assert_eq!(StudiosClient::comparable_name("A-B"), "A-B");
    }

    #[tokio::test]
    async fn with_repo_url_trims_and_falls_back() {
        let c = StudiosClient::with_repo_url("https://example.test/studios/");
        assert_eq!(c.repo_url().await, "https://example.test/studios");
        // No operator override and no attached plugin manager → the C# default.
        let c = StudiosClient::with_repo_url("");
        assert!(c.configured_repo_url.is_none());
        assert_eq!(c.repo_url().await, DEFAULT_REPO_URL);
    }

    /// The dashboard's `RepositoryUrl` governs when the operator set no
    /// override, and the manifest cache is re-keyed when it changes — a cache
    /// that ignored the URL would answer the new repo with the old repo's names.
    #[tokio::test]
    async fn the_dashboard_repository_url_is_live_and_rekeys_the_cache() {
        let plugins = crate::plugin_config::tests_support::manager_with(
            crate::builtin_plugins::STUDIO_IMAGES.id,
            br#"{"RepositoryUrl":"https://example.test/art/"}"#.to_vec(),
        );
        let client = StudiosClient::with_repo_url("");
        client.attach_plugin_manager(plugins);
        assert_eq!(client.repo_url().await, "https://example.test/art");

        // Seed the cache against the OLD repo; the new URL must not read it.
        *client.manifest.lock().unwrap() =
            Some((DEFAULT_REPO_URL.to_owned(), vec!["Netflix".to_owned()]));
        // The lookup goes to the configured repo, whose manifest is unreachable
        // (example.test does not resolve), so no stale match leaks through.
        assert!(client.thumb_url("Netflix").await.is_none());
    }

    /// An operator's explicit override still wins over the dashboard.
    #[tokio::test]
    async fn the_operator_override_beats_the_dashboard() {
        let plugins = crate::plugin_config::tests_support::manager_with(
            crate::builtin_plugins::STUDIO_IMAGES.id,
            br#"{"RepositoryUrl":"https://dashboard.test/art"}"#.to_vec(),
        );
        let client = StudiosClient::with_repo_url("https://operator.test/art");
        client.attach_plugin_manager(plugins);
        assert_eq!(client.repo_url().await, "https://operator.test/art");
    }

    /// The thumb URL is built from the *manifest entry's* casing (not the item's),
    /// matched case- and punctuation-insensitively. Seeds the cache directly so
    /// no network is touched.
    #[tokio::test]
    async fn thumb_url_matches_case_and_punctuation_insensitively() {
        let client = StudiosClient::new();
        client.seed_manifest(vec![
            "Walt Disney Pictures".to_owned(),
            "Netflix".to_owned(),
        ]);

        let url = client.thumb_url("walt disney pictures.").await;
        assert_eq!(
            url.as_deref(),
            Some(concat!(
                "https://raw.githubusercontent.com/jellyfin/emby-artwork/master/studios",
                "/images/Walt Disney Pictures/thumb.jpg"
            ))
        );
        // No manifest match → no image.
        assert!(client.thumb_url("A Nonexistent Studio").await.is_none());
        // A name that normalizes to empty → no lookup.
        assert!(client.thumb_url(" / ").await.is_none());
    }

    #[tokio::test]
    async fn empty_manifest_is_cached_and_yields_no_image() {
        let client = StudiosClient::new();
        client.seed_manifest(Vec::new());
        assert!(client.thumb_url("Netflix").await.is_none());
    }

    /// A fetch failure (here a refused connection — offline, deterministic) is
    /// swallowed to an empty manifest, and that empty result is cached so a
    /// second lookup does not re-hit the network.
    #[tokio::test]
    async fn manifest_fetch_failure_yields_empty_and_is_cached() {
        // Port 1 refuses immediately; no network egress.
        let client = StudiosClient::with_repo_url("http://127.0.0.1:1/studios");
        assert!(client.thumb_url("Netflix").await.is_none());
        assert_eq!(
            client
                .manifest
                .lock()
                .unwrap()
                .as_ref()
                .map(|(u, l)| (u.as_str(), l.len())),
            Some(("http://127.0.0.1:1/studios", 0))
        );
        // Cached empty → second call still None (and would not re-fetch).
        assert!(client.thumb_url("Netflix").await.is_none());
    }

    #[tokio::test]
    async fn default_and_debug_are_wired() {
        let client = StudiosClient::default();
        assert_eq!(client.repo_url().await, DEFAULT_REPO_URL);
        let rendered = format!("{client:?}");
        assert!(rendered.contains("StudiosClient"));
        assert!(rendered.contains("manifest_cached"));
    }
}
