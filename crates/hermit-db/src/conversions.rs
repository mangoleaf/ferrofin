//! `TryFrom<entity>` conversions from the [`crate::entities`] row structs into
//! their matching `hermit-model` DTOs.
//!
//! Only entities with a clean 1:1 `hermit-model` target are converted here;
//! richer types (`BaseItem`, `User`, `MediaStream`) that need joins or lack a
//! target DTO are deferred to a later port unit.
//!
//! ## Why `TryFrom`, not `From`
//! The storage shape keeps `Guid` columns as their hyphenated [`String`] form
//! and enum columns as `INTEGER`/`TEXT` discriminants (see
//! [`crate::entities`]). Turning those into the DTO's [`uuid::Uuid`] and enum
//! fields is fallible — a malformed `Guid` or an out-of-range discriminant
//! yields a [`DbError`], never a panic (per the workspace no-`unwrap` rule). So
//! the conversions are [`TryFrom`], with [`DbError`] as the error type.
//!
//! Enum discriminants are read through the [`crate::enums`] mapping helpers or
//! matched against the C# declaration order that the target `hermit-model`
//! enum mirrors.

pub mod base_items;
pub mod display_preferences;
pub mod playback;
pub mod security;
pub mod users;

use uuid::Uuid;

use crate::error::DbError;

/// Parses a hyphenated `Guid` string stored in `column` into a [`Uuid`].
///
/// # Errors
/// Returns [`DbError::InvalidGuid`] if `value` is not a valid UUID.
fn parse_guid(column: &'static str, value: &str) -> Result<Uuid, DbError> {
    Uuid::parse_str(value).map_err(|source| DbError::InvalidGuid { column, source })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use hermit_model::activity::{ActivityLogEntry, LogLevel};
    use hermit_model::data::PersonKind;
    use hermit_model::devices::DeviceInfo;
    use hermit_model::dto::{
        BaseItemPerson, DisplayPreferencesDto, ImageInfo, ScrollDirection, TrickplayInfoDto,
        UserItemDataDto,
    };
    use hermit_model::entities::ImageType;
    use hermit_model::media_segments::{MediaSegmentDto, MediaSegmentType};
    use uuid::Uuid;

    use super::base_items::PersonCredit;
    use crate::entities::base_items::{
        BaseItemImageInfoEntity, PeopleBaseItemMapEntity, PeopleEntity,
    };
    use crate::entities::display_preferences::DisplayPreferencesEntity;
    use crate::entities::playback::{MediaSegmentEntity, TrickplayInfoEntity, UserDataEntity};
    use crate::entities::security::DeviceEntity;
    use crate::entities::users::{ActivityLogEntity, ImageInfoEntity};
    use crate::error::DbError;

    /// A fixed timestamp used across the conversion fixtures.
    fn instant() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0)
            .single()
            .expect("valid instant")
    }

    #[test]
    fn user_data_maps_to_dto() {
        let id = Uuid::from_u128(0x11);
        let entity = UserDataEntity {
            item_id: id.to_string(),
            user_id: Uuid::from_u128(0x12).to_string(),
            custom_data_key: "default".to_owned(),
            audio_stream_index: Some(2),
            is_favorite: true,
            last_played_date: Some(instant()),
            likes: Some(true),
            play_count: 4,
            playback_position_ticks: 987_654,
            played: true,
            rating: Some(9.5),
            retention_date: None,
            subtitle_stream_index: None,
        };
        let dto = UserItemDataDto::try_from(entity).expect("convert");
        assert_eq!(dto.item_id, id);
        assert_eq!(dto.key, "default");
        assert_eq!(dto.rating, Some(9.5));
        assert_eq!(dto.played_percentage, None);
        assert_eq!(dto.unplayed_item_count, None);
        assert!(dto.is_favorite);
        assert_eq!(dto.play_count, 4);
        assert_eq!(dto.playback_position_ticks, 987_654);
    }

    #[test]
    fn user_data_rejects_bad_guid() {
        let entity = UserDataEntity {
            item_id: "not-a-guid".to_owned(),
            user_id: Uuid::from_u128(1).to_string(),
            custom_data_key: "default".to_owned(),
            audio_stream_index: None,
            is_favorite: false,
            last_played_date: None,
            likes: None,
            play_count: 0,
            playback_position_ticks: 0,
            played: false,
            rating: None,
            retention_date: None,
            subtitle_stream_index: None,
        };
        assert!(matches!(
            UserItemDataDto::try_from(entity),
            Err(DbError::InvalidGuid {
                column: "UserData.ItemId",
                ..
            })
        ));
    }

    #[test]
    fn trickplay_maps_field_for_field() {
        let entity = TrickplayInfoEntity {
            item_id: Uuid::from_u128(1).to_string(),
            width: 320,
            bandwidth: 500_000,
            height: 180,
            interval: 10_000,
            thumbnail_count: 240,
            tile_height: 10,
            tile_width: 12,
        };
        let dto = TrickplayInfoDto::try_from(entity).expect("convert");
        assert_eq!(dto.width, 320);
        assert_eq!(dto.tile_width, 12);
        assert_eq!(dto.tile_height, 10);
        assert_eq!(dto.bandwidth, 500_000);
        assert_eq!(dto.interval, 10_000);
    }

    #[test]
    fn media_segment_maps_type_and_guids() {
        let id = Uuid::from_u128(0x52);
        let item = Uuid::from_u128(0x50);
        let entity = MediaSegmentEntity {
            id: id.to_string(),
            end_ticks: 6_000_000,
            item_id: item.to_string(),
            segment_provider_id: "chapter-provider".to_owned(),
            start_ticks: 0,
            type_: 5,
        };
        let dto = MediaSegmentDto::try_from(entity).expect("convert");
        assert_eq!(dto.id, id);
        assert_eq!(dto.item_id, item);
        assert_eq!(dto.type_, MediaSegmentType::Intro);
        assert_eq!(dto.end_ticks, 6_000_000);
    }

    #[test]
    fn media_segment_rejects_bad_type() {
        let entity = MediaSegmentEntity {
            id: Uuid::from_u128(1).to_string(),
            end_ticks: 0,
            item_id: Uuid::from_u128(2).to_string(),
            segment_provider_id: "p".to_owned(),
            start_ticks: 0,
            type_: 99,
        };
        assert!(matches!(
            MediaSegmentDto::try_from(entity),
            Err(DbError::InvalidEnumValue {
                enum_name: "MediaSegmentType",
                value: 99,
            })
        ));
    }

    #[test]
    fn device_maps_to_dto() {
        let user = Uuid::from_u128(0x60);
        let entity = DeviceEntity {
            id: 1,
            access_token: "atk".to_owned(),
            app_name: "app".to_owned(),
            app_version: "1.0".to_owned(),
            date_created: instant(),
            date_last_activity: instant(),
            date_modified: instant(),
            device_id: "dev".to_owned(),
            device_name: "Phone".to_owned(),
            is_active: true,
            user_id: user.to_string(),
        };
        let dto = DeviceInfo::try_from(entity).expect("convert");
        assert_eq!(dto.name.as_deref(), Some("Phone"));
        assert_eq!(dto.id.as_deref(), Some("dev"));
        assert_eq!(dto.access_token.as_deref(), Some("atk"));
        assert_eq!(dto.app_name.as_deref(), Some("app"));
        assert_eq!(dto.last_user_id, Some(user));
        assert_eq!(dto.custom_name, None);
        assert_eq!(dto.date_last_activity, Some(instant()));
    }

    #[test]
    fn activity_log_maps_severity_and_user() {
        let user = Uuid::from_u128(0x70);
        let entity = ActivityLogEntity {
            id: 1,
            date_created: instant(),
            item_id: None,
            log_severity: 2,
            name: "Login".to_owned(),
            overview: None,
            row_version: 1,
            short_overview: None,
            type_: "AuthenticationSucceeded".to_owned(),
            user_id: user.to_string(),
        };
        let dto = ActivityLogEntry::try_from(entity).expect("convert");
        assert_eq!(dto.id, 1);
        assert_eq!(dto.name, "Login");
        assert_eq!(dto.type_, "AuthenticationSucceeded");
        assert_eq!(dto.date, instant());
        assert_eq!(dto.user_id, user);
        assert_eq!(dto.severity, LogLevel::Information);
    }

    #[test]
    fn user_image_maps_to_profile_image() {
        let entity = ImageInfoEntity {
            id: 1,
            last_modified: instant(),
            path: "/img.png".to_owned(),
            user_id: Some(Uuid::from_u128(1).to_string()),
        };
        let dto = ImageInfo::try_from(entity).expect("convert");
        assert_eq!(dto.image_type, ImageType::Profile);
        assert_eq!(dto.path.as_deref(), Some("/img.png"));
        assert_eq!(dto.width, None);
        assert_eq!(dto.size, 0);
    }

    #[test]
    fn base_item_image_maps_type_and_dimensions() {
        let entity = BaseItemImageInfoEntity {
            id: Uuid::from_u128(0x22).to_string(),
            blurhash: Some(b"LEHV6".to_vec()),
            date_modified: Some(instant()),
            height: 1080,
            image_type: 2,
            item_id: Uuid::from_u128(0x20).to_string(),
            path: "/poster.jpg".to_owned(),
            width: 1920,
        };
        let dto = ImageInfo::try_from(entity).expect("convert");
        assert_eq!(dto.image_type, ImageType::Backdrop);
        assert_eq!(dto.path.as_deref(), Some("/poster.jpg"));
        assert_eq!(dto.height, Some(1080));
        assert_eq!(dto.width, Some(1920));
        assert_eq!(dto.blur_hash.as_deref(), Some("LEHV6"));
        assert_eq!(dto.size, 0);
    }

    #[test]
    fn display_preferences_maps_enums_and_flags() {
        let user = Uuid::from_u128(0x80);
        let entity = DisplayPreferencesEntity {
            id: 1,
            chromecast_version: 2,
            client: "web".to_owned(),
            dashboard_theme: Some("dark".to_owned()),
            enable_next_video_info_overlay: true,
            index_by: Some(2),
            item_id: Uuid::from_u128(0x99).to_string(),
            scroll_direction: 1,
            show_backdrop: false,
            show_sidebar: true,
            skip_backward_length: 10,
            skip_forward_length: 30,
            tv_home: None,
            user_id: user.to_string(),
        };
        let dto = DisplayPreferencesDto::try_from(entity).expect("convert");
        assert_eq!(dto.id.as_deref(), Some(user.to_string().as_str()));
        assert_eq!(dto.client.as_deref(), Some("web"));
        assert_eq!(dto.index_by.as_deref(), Some("CommunityRating"));
        assert_eq!(dto.scroll_direction, ScrollDirection::Vertical);
        assert!(!dto.show_backdrop);
        assert!(dto.show_sidebar);
        assert_eq!(
            dto.primary_image_height,
            DisplayPreferencesDto::default().primary_image_height
        );
    }

    #[test]
    fn person_credit_maps_to_base_item_person() {
        let id = Uuid::from_u128(0x32);
        let person = PeopleEntity {
            id: id.to_string(),
            name: "Harrison Ford".to_owned(),
            person_type: Some("Actor".to_owned()),
            ..Default::default()
        };
        let credit = PeopleBaseItemMapEntity {
            item_id: Uuid::from_u128(0x30).to_string(),
            people_id: id.to_string(),
            role: "Deckard".to_owned(),
            list_order: Some(0),
            sort_order: Some(0),
        };
        let dto = BaseItemPerson::try_from(PersonCredit { person, credit }).expect("convert");
        assert_eq!(dto.id, id);
        assert_eq!(dto.name.as_deref(), Some("Harrison Ford"));
        assert_eq!(dto.role.as_deref(), Some("Deckard"));
        assert_eq!(dto.type_, PersonKind::Actor);
    }

    #[test]
    fn person_credit_unknown_type_maps_to_unknown() {
        let id = Uuid::from_u128(1);
        let person = PeopleEntity {
            id: id.to_string(),
            name: "Someone".to_owned(),
            person_type: Some("Choreographer".to_owned()),
            ..Default::default()
        };
        let credit = PeopleBaseItemMapEntity {
            item_id: Uuid::from_u128(2).to_string(),
            people_id: id.to_string(),
            role: String::new(),
            list_order: None,
            sort_order: None,
        };
        let dto = BaseItemPerson::try_from(PersonCredit { person, credit }).expect("convert");
        assert_eq!(dto.type_, PersonKind::Unknown);
    }

    /// Builds a `BaseItemImageInfoEntity` carrying the given `ImageType`
    /// discriminant, for exercising `image_type_from_i32`.
    fn base_item_image(image_type: i32) -> BaseItemImageInfoEntity {
        BaseItemImageInfoEntity {
            id: Uuid::from_u128(0x22).to_string(),
            blurhash: None,
            date_modified: None,
            height: 100,
            image_type,
            item_id: Uuid::from_u128(0x20).to_string(),
            path: "/x.jpg".to_owned(),
            width: 100,
        }
    }

    #[test]
    fn base_item_image_maps_every_image_type_discriminant() {
        let expected = [
            ImageType::Primary,
            ImageType::Art,
            ImageType::Backdrop,
            ImageType::Banner,
            ImageType::Logo,
            ImageType::Thumb,
            ImageType::Disc,
            ImageType::Box,
            ImageType::Screenshot,
            ImageType::Menu,
            ImageType::Chapter,
            ImageType::BoxRear,
            ImageType::Profile,
        ];
        for (value, want) in expected.into_iter().enumerate() {
            let value = i32::try_from(value).expect("small index fits i32");
            let dto = ImageInfo::try_from(base_item_image(value)).expect("convert");
            assert_eq!(dto.image_type, want, "discriminant {value}");
        }
    }

    #[test]
    fn base_item_image_rejects_bad_image_type() {
        assert!(matches!(
            ImageInfo::try_from(base_item_image(13)),
            Err(DbError::InvalidEnumValue {
                enum_name: "ImageType",
                value: 13,
            })
        ));
    }

    /// Builds an `ActivityLogEntity` with the given `LogSeverity` discriminant,
    /// for exercising `log_level_from_i32`.
    fn activity_log(log_severity: i32) -> ActivityLogEntity {
        ActivityLogEntity {
            id: 1,
            date_created: instant(),
            item_id: None,
            log_severity,
            name: "n".to_owned(),
            overview: None,
            row_version: 1,
            short_overview: None,
            type_: "t".to_owned(),
            user_id: Uuid::from_u128(1).to_string(),
        }
    }

    #[test]
    fn activity_log_maps_every_severity_discriminant() {
        let expected = [
            LogLevel::Trace,
            LogLevel::Debug,
            LogLevel::Information,
            LogLevel::Warning,
            LogLevel::Error,
            LogLevel::Critical,
            LogLevel::None,
        ];
        for (value, want) in expected.into_iter().enumerate() {
            let value = i32::try_from(value).expect("small index fits i32");
            let dto = ActivityLogEntry::try_from(activity_log(value)).expect("convert");
            assert_eq!(dto.severity, want, "discriminant {value}");
        }
    }

    #[test]
    fn activity_log_rejects_bad_severity() {
        assert!(matches!(
            ActivityLogEntry::try_from(activity_log(7)),
            Err(DbError::InvalidEnumValue {
                enum_name: "LogLevel",
                value: 7,
            })
        ));
    }

    /// Builds a `DisplayPreferencesEntity` with the given `IndexBy` and
    /// `ScrollDirection` discriminants, exercising `indexing_kind_name` and
    /// `scroll_direction_from_i32`.
    fn display_preferences(
        index_by: Option<i32>,
        scroll_direction: i32,
    ) -> DisplayPreferencesEntity {
        DisplayPreferencesEntity {
            id: 1,
            chromecast_version: 0,
            client: "web".to_owned(),
            dashboard_theme: None,
            enable_next_video_info_overlay: false,
            index_by,
            item_id: Uuid::from_u128(0x99).to_string(),
            scroll_direction,
            show_backdrop: false,
            show_sidebar: false,
            skip_backward_length: 0,
            skip_forward_length: 0,
            tv_home: None,
            user_id: Uuid::from_u128(0x80).to_string(),
        }
    }

    #[test]
    fn display_preferences_maps_every_index_by_name() {
        let cases = [
            (0, "PremiereDate"),
            (1, "ProductionYear"),
            (2, "CommunityRating"),
        ];
        for (value, name) in cases {
            let dto = DisplayPreferencesDto::try_from(display_preferences(Some(value), 0))
                .expect("convert");
            assert_eq!(dto.index_by.as_deref(), Some(name), "discriminant {value}");
        }
    }

    #[test]
    fn display_preferences_none_index_by_stays_none() {
        let dto = DisplayPreferencesDto::try_from(display_preferences(None, 0)).expect("convert");
        assert_eq!(dto.index_by, None);
    }

    #[test]
    fn display_preferences_maps_both_scroll_directions() {
        let horizontal =
            DisplayPreferencesDto::try_from(display_preferences(None, 0)).expect("convert");
        assert_eq!(horizontal.scroll_direction, ScrollDirection::Horizontal);
        let vertical =
            DisplayPreferencesDto::try_from(display_preferences(None, 1)).expect("convert");
        assert_eq!(vertical.scroll_direction, ScrollDirection::Vertical);
    }

    #[test]
    fn display_preferences_rejects_bad_scroll_direction() {
        assert!(matches!(
            DisplayPreferencesDto::try_from(display_preferences(None, 9)),
            Err(DbError::InvalidEnumValue {
                enum_name: "ScrollDirection",
                value: 9,
            })
        ));
    }

    #[test]
    fn display_preferences_rejects_bad_index_by() {
        assert!(matches!(
            DisplayPreferencesDto::try_from(display_preferences(Some(9), 0)),
            Err(DbError::InvalidEnumValue {
                enum_name: "IndexingKind",
                value: 9,
            })
        ));
    }

    /// Builds a `MediaSegmentEntity` with the given `Type` discriminant, for
    /// exercising `media_segment_type_from_i32`.
    fn media_segment(type_: i32) -> MediaSegmentEntity {
        MediaSegmentEntity {
            id: Uuid::from_u128(0x52).to_string(),
            end_ticks: 0,
            item_id: Uuid::from_u128(0x50).to_string(),
            segment_provider_id: "p".to_owned(),
            start_ticks: 0,
            type_,
        }
    }

    #[test]
    fn media_segment_maps_every_type_discriminant() {
        let expected = [
            MediaSegmentType::Unknown,
            MediaSegmentType::Commercial,
            MediaSegmentType::Preview,
            MediaSegmentType::Recap,
            MediaSegmentType::Outro,
            MediaSegmentType::Intro,
        ];
        for (value, want) in expected.into_iter().enumerate() {
            let value = i32::try_from(value).expect("small index fits i32");
            let dto = MediaSegmentDto::try_from(media_segment(value)).expect("convert");
            assert_eq!(dto.type_, want, "discriminant {value}");
        }
    }
}
