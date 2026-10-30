//! The two virtual-path tokens Jellyfin persists in place of the server's own
//! directories, and their expansion on read.
//!
//! `XmlSerializer` documents, `BaseItems.Path` and `BaseItemImageInfos.Path`
//! all go through `IApplicationHost.ReverseVirtualPath` on the way in and
//! `ExpandVirtualPath` on the way out (`BaseItemRepository.Map`), so a stored
//! image path reads `%MetadataPath%/library/ab/abcd…/poster.jpg`. That is what
//! makes a Jellyfin database portable between data directories — and what
//! makes an adopted one unreadable until the same expansion is applied on
//! Ferrofin's side. Ferrofin writes resolved paths, so expanding on read is
//! the identity for rows it wrote itself.

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

    /// The stored path with both tokens replaced, case-insensitively, as
    /// `string.Replace(…, StringComparison.OrdinalIgnoreCase)` does upstream.
    #[must_use]
    pub fn expand(&self, path: &str) -> String {
        let Some(paths) = &self.paths else {
            return path.to_owned();
        };
        let data = replace_ignore_ascii_case(
            path,
            FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH,
            &paths.data_path(),
        );
        replace_ignore_ascii_case(
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

    /// [`Self::expand`] without a copy when there is nothing to expand: `path` itself, borrowed,
    /// when it holds neither token in any case (every path Ferrofin writes, every media path),
    /// else [`Self::expand`]'s string. The text is [`Self::expand`]'s either way.
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
    /// this server's data directory, then its internal metadata directory,
    /// replaced case-insensitively by their tokens.
    #[must_use]
    pub fn reverse(&self, path: &str) -> String {
        let Some(paths) = &self.paths else {
            return path.to_owned();
        };
        let data = replace_ignore_ascii_case(
            path,
            &paths.data_path(),
            FerrofinServerApplicationPaths::VIRTUAL_DATA_PATH,
        );
        replace_ignore_ascii_case(
            &data,
            &paths.internal_metadata_path(),
            FerrofinServerApplicationPaths::VIRTUAL_INTERNAL_METADATA_PATH,
        )
    }
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
