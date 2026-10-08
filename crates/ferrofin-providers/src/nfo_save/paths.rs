//! BaseNfoSaver uses LibraryManager.GetPathAfterNetworkSubstitution for artwork.
use ferrofin_model::configuration::PathSubstitution;

pub(super) fn substitute(path: &str, substitutions: &[PathSubstitution]) -> String {
    if path
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http"))
    {
        return path.to_owned();
    }
    for map in substitutions {
        if map.from.is_empty() || map.to.is_empty() || map.from.len() > path.len() {
            continue;
        }
        let separator = if map.from.contains('/') { '/' } else { '\\' };
        let normalize = |value: &str| {
            value
                .chars()
                .map(|c| {
                    if matches!(c, '/' | '\\') {
                        separator
                    } else {
                        c
                    }
                })
                .collect::<String>()
        };
        let from = normalize(&map.from);
        let normalized = normalize(path);
        let Some(prefix) = normalized.get(..from.len()) else {
            continue;
        };
        if !prefix.eq_ignore_ascii_case(&from) {
            continue;
        }
        let tail = &normalized[from.len()..];
        if !from.ends_with(separator) && !tail.is_empty() && !tail.starts_with(separator) {
            continue;
        }
        let start = if from.ends_with(separator) {
            from.len() - 1
        } else {
            from.len()
        };
        return format!(
            "{}{}",
            map.to.trim_end_matches(separator),
            &normalized[start..]
        );
    }
    path.to_owned()
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
