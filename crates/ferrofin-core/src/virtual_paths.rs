//! The two virtual-path tokens Jellyfin persists in place of the server's own
//! directories, and their expansion on read.
//!
//! `XmlSerializer` documents, `BaseItems.Path` and `BaseItemImageInfos.Path`
//! all go through `IApplicationHost.ReverseVirtualPath` on the way in and
//! `ExpandVirtualPath` on the way out (`BaseItemRepository.Map`), so a stored
//! image path reads `%MetadataPath%/library/ab/abcd…/poster.jpg`. That is what
//! makes a Jellyfin database portable between data directories — and what
//! makes an adopted one unreadable until the same expansion is applied on
//! Ferrofin's side. Ferrofin writes every `BaseItems.Path` the same way
//! ([`VirtualPathExpander::reverse`]: `FerrofinItemPersistenceService`'s
//! saves, the by-name, person, playlists-folder and user-view writers), so its
//! rows follow a move of those directories as upstream's do.

use std::borrow::Cow;
use std::sync::Arc;

use ferrofin_traits::system::ServerApplicationPaths;

use crate::app_paths::FerrofinServerApplicationPaths;

/// Expands `%AppDataPath%` and `%MetadataPath%` to this server's directories.
///
/// Port of the read half of the virtual-path pair (`ExpandVirtualPath`). A
/// [`VirtualPathExpander::identity`] never rewrites, for callers (tests, mostly)
/// that have no paths and store none of the tokens.
#[derive(Debug, Clone, Default)]
pub struct VirtualPathExpander {
    /// Shared paths so metadata-directory changes take effect without a restart.
    paths: Option<Arc<FerrofinServerApplicationPaths>>,
}

impl VirtualPathExpander {
    /// An expander that follows this server's current directories.
    #[must_use]
    pub fn from_paths(paths: Arc<FerrofinServerApplicationPaths>) -> Self {
        Self { paths: Some(paths) }
    }

    /// An expander that leaves every path alone.
    #[must_use]
    pub fn identity() -> Self {
        Self::default()
    }

    /// This server's internal metadata directory, `None` for
    /// [`Self::identity`].
    #[must_use]
    pub fn internal_metadata_path(&self) -> Option<String> {
        self.paths
            .as_ref()
            .map(|paths| paths.internal_metadata_path())
    }

    /// The stored path with both tokens replaced, case-insensitively, as
    /// `string.Replace(…, StringComparison.OrdinalIgnoreCase)` does upstream.
    #[must_use]
    pub fn expand(&self, path: &str) -> String {
        let Some(paths) = &self.paths else {
            return path.to_owned();
        };
        let data = substitute(
            path,
            FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH,
            &paths.data_path(),
        );
        substitute(
            &data,
            FerrofinServerApplicationPaths::VIRTUAL_INTERNAL_METADATA_PATH,
            &paths.internal_metadata_path(),
        )
    }

    /// [`Self::expand`] for an optional path.
    #[must_use]
    pub fn expand_opt(&self, path: Option<&str>) -> Option<String> {
        path.map(|p| self.expand(p))
    }

    /// [`Self::expand`] on a row's stored path, in place: `BaseItemMapper.Map`'s
    /// `dto.Path = appHost.ExpandVirtualPath(entity.Path)`. A path holding
    /// neither token is left as it is, without a copy ([`Self::expand_cow`]).
    pub fn expand_in_place(&self, path: &mut Option<String>) {
        if let Some(stored) = path.as_deref()
            && let Cow::Owned(expanded) = self.expand_cow(stored)
        {
            *path = Some(expanded);
        }
    }

    /// [`Self::expand`] without a copy when there is nothing to expand: `path` itself, borrowed,
    /// when it holds neither token in any case (every path outside this server's data and
    /// metadata directories: media, the root folders, libraries), else [`Self::expand`]'s
    /// string. The text is [`Self::expand`]'s either way.
    #[must_use]
    pub fn expand_cow<'p>(&self, path: &'p str) -> Cow<'p, str> {
        let holds = |token: &str| {
            path.as_bytes()
                .windows(token.len())
                .any(|window| window.eq_ignore_ascii_case(token.as_bytes()))
        };
        if self.paths.is_some()
            && (holds(FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH)
                || holds(FerrofinServerApplicationPaths::VIRTUAL_INTERNAL_METADATA_PATH))
        {
            Cow::Owned(self.expand(path))
        } else {
            Cow::Borrowed(path)
        }
    }

    /// The write half, `ReverseVirtualPath` (`ApplicationHost.cs:299-305`):
    /// a path at or under this server's data directory, else its internal
    /// metadata directory, gets that directory's token in its place, compared
    /// case-insensitively.
    ///
    /// ACCEPTED DIVERGENCE (Ferrofin is the more-correct side): upstream's
    /// `string.Replace` swaps the directory wherever it occurs. Its directories
    /// are absolute, so that is a prefix but for a path that merely contains
    /// one (`/media/config/data/…` beside a `/config/data`); a relative
    /// `--data-dir` (`data/data`) would match inside any path holding the
    /// text. Only a whole leading directory is replaced here, so every stored
    /// token starts the path, as upstream's do (gl-1581).
    #[must_use]
    pub fn reverse(&self, path: &str) -> String {
        let Some(paths) = &self.paths else {
            return path.to_owned();
        };
        [
            (
                paths.data_path(),
                FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH,
            ),
            (
                paths.internal_metadata_path(),
                FerrofinServerApplicationPaths::VIRTUAL_INTERNAL_METADATA_PATH,
            ),
        ]
        .into_iter()
        .find_map(|(dir, token)| under(path, &dir).map(|rest| format!("{token}{rest}")))
        .unwrap_or_else(|| path.to_owned())
    }
}

/// `path` with `token` replaced by `dir`, case-insensitively. A `dir`
/// configured with a trailing separator (`/mnt/meta/`) gives it once: the
/// stored path already carries one after the token, as
/// [`VirtualPathExpander::reverse`] keeps it.
fn substitute(path: &str, token: &str, dir: &str) -> String {
    let trimmed = dir.trim_end_matches(['/', '\\']);
    if trimmed.len() == dir.len() || trimmed.is_empty() {
        return replace_ignore_ascii_case(path, token, dir);
    }
    let path = replace_ignore_ascii_case(path, &format!("{token}/"), &format!("{trimmed}/"));
    let path = replace_ignore_ascii_case(&path, &format!("{token}\\"), &format!("{trimmed}\\"));
    replace_ignore_ascii_case(&path, token, dir)
}

/// What follows `dir` in `path` when `path` is `dir` or lies under it, the
/// directory compared case-insensitively: empty, or the rest from its
/// separator on.
fn under<'p>(path: &'p str, dir: &str) -> Option<&'p str> {
    let dir = dir.trim_end_matches(['/', '\\']);
    if dir.is_empty() {
        return None;
    }
    let rest = path.get(dir.len()..)?;
    (path[..dir.len()].eq_ignore_ascii_case(dir)
        && (rest.is_empty() || rest.starts_with(['/', '\\'])))
    .then_some(rest)
}

pub(crate) use ferrofin_util::string_extensions::replace_ignore_ascii_case;

#[cfg(test)]
mod tests {
    use super::*;

    fn expander() -> VirtualPathExpander {
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            "/var/lib/ferrofin",
            "log",
            "config",
            "cache",
            "web",
        ));
        paths.set_internal_metadata_path(Some("/var/lib/ferrofin/data/metadata"));
        VirtualPathExpander::from_paths(paths)
    }

    #[test]
    fn cloned_expanders_follow_metadata_directory_changes() {
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            "/data", "log", "config", "cache", "web",
        ));
        let original = VirtualPathExpander::from_paths(Arc::clone(&paths));
        let cloned = original.clone();
        assert_eq!(
            original.expand("%MetadataPath%/poster.jpg"),
            "/data/metadata/poster.jpg"
        );
        paths.set_internal_metadata_path(Some("/new-metadata"));
        for expander in [&original, &cloned] {
            assert_eq!(
                expander.expand("%MetadataPath%/poster.jpg"),
                "/new-metadata/poster.jpg"
            );
        }
        paths.set_internal_metadata_path(None);
        assert_eq!(
            cloned.expand("%MetadataPath%/poster.jpg"),
            "/data/metadata/poster.jpg"
        );
    }

    #[test]
    fn expands_the_metadata_token_as_jellyfin_stores_image_paths() {
        assert_eq!(
            expander().expand("%MetadataPath%/library/ab/abcdef/poster.jpg"),
            "/var/lib/ferrofin/data/metadata/library/ab/abcdef/poster.jpg"
        );
    }

    /// `ReverseVirtualPath` replaces the data directory first, so a metadata
    /// directory inside it comes out under `%AppDataPath%`, as upstream's
    /// does; a separate one gets `%MetadataPath%`; both case-insensitively.
    #[test]
    fn reverses_this_servers_directories_into_their_tokens() {
        let nested = expander();
        assert_eq!(
            nested.reverse("/var/lib/ferrofin/data/collections"),
            "%AppDataPath%/collections"
        );
        assert_eq!(
            nested.reverse("/VAR/lib/ferrofin/Data/x"),
            "%AppDataPath%/x"
        );
        assert_eq!(
            nested.reverse("/var/lib/ferrofin/data/metadata/a"),
            "%AppDataPath%/metadata/a"
        );
        assert_eq!(nested.reverse("/srv/media"), "/srv/media");
        // the directory itself, and a whole leading directory only (gl-1581)
        assert_eq!(nested.reverse("/var/lib/ferrofin/data"), "%AppDataPath%");
        assert_eq!(
            nested.reverse("/var/lib/ferrofin/data2/x"),
            "/var/lib/ferrofin/data2/x"
        );
        assert_eq!(
            nested.reverse("/media/var/lib/ferrofin/data/x.mkv"),
            "/media/var/lib/ferrofin/data/x.mkv"
        );
        assert_eq!(nested.reverse(""), "");
        // a directory configured with a trailing separator round-trips
        let slashed = Arc::new(FerrofinServerApplicationPaths::new(
            "/data", "log", "config", "cache", "web",
        ));
        slashed.set_internal_metadata_path(Some("/mnt/meta/"));
        let slashed = VirtualPathExpander::from_paths(slashed);
        let stored = slashed.reverse("/mnt/meta/Genre/Action");
        assert_eq!(stored, "%MetadataPath%/Genre/Action");
        assert_eq!(slashed.expand(&stored), "/mnt/meta/Genre/Action");
        assert_eq!(slashed.reverse("/mnt/meta"), "%MetadataPath%");
        let relative = VirtualPathExpander::from_paths(Arc::new(
            FerrofinServerApplicationPaths::new("data", "log", "config", "cache", "web"),
        ));
        assert_eq!(
            relative.reverse("data/data/playlists"),
            "%AppDataPath%/playlists"
        );
        assert_eq!(
            relative.reverse("/mnt/data/data/movies/a.mkv"),
            "/mnt/data/data/movies/a.mkv",
            "a relative data directory never matches inside a path"
        );
        let paths = Arc::new(FerrofinServerApplicationPaths::new(
            "/data", "log", "config", "cache", "web",
        ));
        paths.set_internal_metadata_path(Some("/meta"));
        assert_eq!(
            VirtualPathExpander::from_paths(paths).reverse("/meta/library/x.jpg"),
            "%MetadataPath%/library/x.jpg"
        );
        assert_eq!(
            VirtualPathExpander::identity().reverse("/data/x"),
            "/data/x"
        );
    }

    #[test]
    fn expands_the_data_token_and_is_case_insensitive() {
        assert_eq!(
            expander().expand("%appdatapath%/collections/Favourites"),
            "/var/lib/ferrofin/data/collections/Favourites"
        );
    }

    #[test]
    fn resolved_paths_pass_through_unchanged() {
        let media = "/srv/fastmedia/shows/S01E01-thumb.jpg";
        assert_eq!(expander().expand(media), media);
        assert_eq!(expander().expand(""), "");
    }

    #[test]
    fn identity_keeps_the_tokens() {
        let stored = "%MetadataPath%/library/x.jpg";
        assert_eq!(VirtualPathExpander::identity().expand(stored), stored);
    }

    #[test]
    fn expand_in_place_rewrites_only_a_stored_token() {
        let mut stored = Some("%MetadataPath%/Genre/Action".to_owned());
        expander().expand_in_place(&mut stored);
        assert_eq!(
            stored.as_deref(),
            Some("/var/lib/ferrofin/data/metadata/Genre/Action")
        );
        let mut media = Some("/srv/media/a.mkv".to_owned());
        expander().expand_in_place(&mut media);
        assert_eq!(media.as_deref(), Some("/srv/media/a.mkv"));
        let mut none = None;
        expander().expand_in_place(&mut none);
        assert_eq!(none, None);
        let mut kept = Some("%AppDataPath%/playlists".to_owned());
        VirtualPathExpander::identity().expand_in_place(&mut kept);
        assert_eq!(kept.as_deref(), Some("%AppDataPath%/playlists"));
        let mut percent = Some("/media/100%/a.mkv".to_owned());
        expander().expand_in_place(&mut percent);
        assert_eq!(percent.as_deref(), Some("/media/100%/a.mkv"));
    }

    #[test]
    fn expand_cow_borrows_for_the_identity_and_expands_a_by_name_path() {
        assert!(matches!(
            expander().expand_cow("/srv/media/a.mkv"),
            Cow::Borrowed("/srv/media/a.mkv")
        ));
        assert!(matches!(
            VirtualPathExpander::identity().expand_cow("%MetadataPath%/x"),
            Cow::Borrowed("%MetadataPath%/x")
        ));
        assert_eq!(
            expander().expand_cow("/media/100%/a.mkv"),
            "/media/100%/a.mkv"
        );
        assert_eq!(
            expander().expand_cow("%MetadataPath%/People/A/Amanda Adams"),
            "/var/lib/ferrofin/data/metadata/People/A/Amanda Adams"
        );
    }

    #[test]
    fn expand_opt_maps_through() {
        assert_eq!(expander().expand_opt(None), None);
        assert_eq!(
            expander().expand_opt(Some("%MetadataPath%/x")).as_deref(),
            Some("/var/lib/ferrofin/data/metadata/x")
        );
    }

    /// `expand_cow` reads as `expand` and copies only a path holding a token, in any case; the
    /// identity expander copies nothing (gl-1001).
    #[test]
    fn expand_cow_copies_only_a_path_with_a_token() {
        let server = expander();
        for path in [
            "/srv/media/a.mkv",
            "",
            "%AppData%/x",
            "/a/100%/b",
            "%Metadata",
        ] {
            let expanded = server.expand_cow(path);
            assert!(matches!(expanded, Cow::Borrowed(p) if p == path), "{path}");
            assert_eq!(expanded, server.expand(path), "{path}");
        }
        for path in [
            "%AppDataPath%/collections/x",
            "%appdatapath%/x",
            "%MetadataPath%/library/x.jpg",
            "/x/%METADATAPATH%",
            "%AppDataPath%/%MetadataPath%",
        ] {
            let expanded = server.expand_cow(path);
            assert!(matches!(expanded, Cow::Owned(_)), "{path}");
            assert_eq!(expanded, server.expand(path), "{path}");
        }
        let stored = "%MetadataPath%/library/x.jpg";
        assert!(matches!(
            VirtualPathExpander::identity().expand_cow(stored),
            Cow::Borrowed(p) if p == stored
        ));
    }
}
