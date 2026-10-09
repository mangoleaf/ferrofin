//! Automatic artwork acquisition preferences from `LibraryOptions.TypeOptions`.
//!
//! Jellyfin's `TypeOptions.GetImageOptions` first uses the saved image entry,
//! then the per-item-type defaults, then limit 1 / minimum width 0. These
//! preferences do not filter local discovery or the manual image chooser.

use ferrofin_model::configuration::{
    ImageOption, LibraryOptions, TypeOptions, default_image_options,
};
use ferrofin_model::entities::ImageType;

/// The effective automatic image acquisition preferences for one item type.
#[derive(Clone, Copy, Debug)]
pub struct ImageAcquisitionPolicy<'a> {
    kind: &'a str,
    options: Option<&'a TypeOptions>,
}

impl<'a> ImageAcquisitionPolicy<'a> {
    /// Resolves the library's case-insensitive item-type entry. Server metadata provider
    /// preferences are not an image-options fallback in `ItemImageProvider`.
    #[must_use]
    pub fn new(library: Option<&'a LibraryOptions>, kind: &'a str) -> Self {
        Self {
            kind,
            options: library.and_then(|library| {
                library.type_options.iter().find(|option| {
                    option
                        .type_
                        .as_deref()
                        .is_some_and(|saved| saved.eq_ignore_ascii_case(kind))
                })
            }),
        }
    }

    /// The saved entry, type-specific default, or upstream default instance.
    #[must_use]
    pub fn option(self, image_type: ImageType) -> ImageOption {
        self.options
            .and_then(|options| {
                options
                    .image_options
                    .iter()
                    .find(|option| option.type_ == image_type)
            })
            .or_else(|| {
                default_image_options(
                    self.options
                        .and_then(|options| options.type_.as_deref())
                        .unwrap_or(self.kind),
                )
                .iter()
                .find(|option| option.type_ == image_type)
            })
            .copied()
            .unwrap_or(ImageOption {
                type_: image_type,
                ..Default::default()
            })
    }

    /// The maximum acquired count. All supported types except Backdrop are
    /// singular, even when their saved limit is greater than one.
    #[must_use]
    pub fn limit(self, image_type: ImageType) -> usize {
        let limit = usize::try_from(self.option(image_type).limit).unwrap_or(0);
        if image_type == ImageType::Backdrop {
            limit
        } else {
            limit.min(1)
        }
    }

    /// Whether a remote candidate qualifies. An unknown width is allowed,
    /// matching upstream's nullable-width check; dynamic providers only use
    /// the positive-limit check because they do not offer remote dimensions.
    #[must_use]
    pub fn accepts(self, image_type: ImageType, width: Option<i32>) -> bool {
        self.limit(image_type) > 0
            && width.is_none_or(|width| width >= self.option(image_type).min_width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("Movie", ImageType::Backdrop, 1, 1280)]
    #[case("Movie", ImageType::Disc, 0, 0)]
    #[case("Series", ImageType::Banner, 1, 0)]
    #[case("Season", ImageType::Backdrop, 0, 1280)]
    #[case("MusicAlbum", ImageType::Disc, 0, 0)]
    #[case("MusicArtist", ImageType::Banner, 0, 0)]
    #[case("Person", ImageType::Primary, 1, 0)]
    fn defaults_match_upstream(
        #[case] kind: &str,
        #[case] image_type: ImageType,
        #[case] limit: usize,
        #[case] min_width: i32,
    ) {
        let policy = ImageAcquisitionPolicy::new(None, kind);
        assert_eq!(policy.limit(image_type), limit);
        assert_eq!(policy.option(image_type).min_width, min_width);
    }

    #[test]
    fn missing_wire_members_use_constructor_defaults() {
        let option: ImageOption = serde_json::from_str(r#"{"Type":"Backdrop"}"#).unwrap();
        assert_eq!(option.limit, 1);
        assert_eq!(option.min_width, 0);
        let option: ImageOption = serde_json::from_str(r#"{"Type":"Primary","Limit":0}"#).unwrap();
        assert_eq!(option.limit, 0);
    }

    #[test]
    fn saved_settings_override_one_type_without_erasing_other_defaults() {
        let library = LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("Movie".to_owned()),
                image_options: vec![
                    ImageOption {
                        type_: ImageType::Backdrop,
                        limit: 3,
                        min_width: 1920,
                    },
                    ImageOption {
                        type_: ImageType::Primary,
                        limit: 8,
                        min_width: 600,
                    },
                    ImageOption {
                        type_: ImageType::Logo,
                        limit: -1,
                        min_width: 0,
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let policy = ImageAcquisitionPolicy::new(Some(&library), "Movie");
        assert_eq!(policy.limit(ImageType::Backdrop), 3);
        assert_eq!(policy.limit(ImageType::Primary), 1);
        assert_eq!(policy.limit(ImageType::Logo), 0);
        assert_eq!(policy.limit(ImageType::Disc), 0);
        assert!(!policy.accepts(ImageType::Primary, Some(599)));
        assert!(policy.accepts(ImageType::Primary, Some(600)));
        assert!(policy.accepts(ImageType::Primary, None));
        assert!(!policy.accepts(ImageType::Logo, None));
    }
}
