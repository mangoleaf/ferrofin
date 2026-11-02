//! BaseNfoSaver uses LibraryManager.GetPathAfterNetworkSubstitution for artwork.
use ferrofin_model::configuration::{PathSubstitution, path_after_network_substitution};

pub(crate) fn substitute(path: &str, substitutions: &[PathSubstitution]) -> String {
    // BaseNfoSaver leaves a remote image path as it is.
    if path
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http"))
    {
        return path.to_owned();
    }
    path_after_network_substitution(path, substitutions)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacements_observe_component_boundaries_and_first_match() {
        let maps = vec![
            PathSubstitution {
                from: "/media".into(),
                to: "/network/".into(),
            },
            PathSubstitution {
                from: "/media/Movie".into(),
                to: "/later".into(),
            },
        ];
        assert_eq!(
            substitute("/media/Movie/poster.jpg", &maps),
            "/network/Movie/poster.jpg"
        );
        assert_eq!(
            substitute("/Media/Movie/poster.jpg", &maps),
            "/network/Movie/poster.jpg"
        );
        assert_eq!(
            substitute("/media-other/poster.jpg", &maps),
            "/media-other/poster.jpg"
        );
        assert_eq!(substitute("/media", &maps), "/network");
        assert_eq!(
            substitute(
                "/media/poster.jpg",
                &[PathSubstitution {
                    from: "/media/".into(),
                    to: "/network".into()
                }]
            ),
            "/network/poster.jpg"
        );
    }
}
