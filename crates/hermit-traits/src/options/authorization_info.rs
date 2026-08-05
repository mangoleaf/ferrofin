//! Port of `MediaBrowser.Controller.Net.AuthorizationInfo`.

use hermit_db::entities::users::UserEntity;
use hermit_model::secret::Secret;
use uuid::Uuid;

/// The authorization context resolved for an incoming request.
///
/// Mirrors C# `AuthorizationInfo`. Port rule applied: the C# `User` domain
/// property becomes an [`Option`]`<`[`UserEntity`]`>` (the persistence row).
/// This is a server-side request-context value, never serialized over the wire,
/// so it carries no serde derives (and [`UserEntity`] has none either).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthorizationInfo {
    /// The device id supplied by the client.
    pub device_id: Option<String>,

    /// The human-readable device name.
    pub device: Option<String>,

    /// The client/app name.
    pub client: Option<String>,

    /// The client/app version.
    pub version: Option<String>,

    /// The access token presented, if any.
    pub token: Option<Secret>,

    /// Whether the authorization came from an API key rather than a user token.
    pub is_api_key: bool,

    /// The authenticated user, if the token resolved to one.
    pub user: Option<UserEntity>,

    /// Whether the token authenticated successfully.
    pub is_authenticated: bool,
}

impl AuthorizationInfo {
    /// The id of the authenticated [`user`](Self::user), or [`Uuid::nil`] when
    /// there is none. Mirrors C# `UserId => User?.Id ?? Guid.Empty`.
    ///
    /// The user entity stores its id as the hyphenated `Guid` string; an
    /// unparseable id also yields [`Uuid::nil`].
    #[must_use]
    pub fn user_id(&self) -> Uuid {
        self.user
            .as_ref()
            .and_then(|u| Uuid::parse_str(&u.id).ok())
            .unwrap_or_else(Uuid::nil)
    }

    /// Whether a non-empty token is present. Mirrors C# `HasToken`.
    #[must_use]
    pub fn has_token(&self) -> bool {
        self.token
            .as_ref()
            .is_some_and(|t| !t.expose().trim().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthorizationInfo, UserEntity};
    use uuid::Uuid;

    /// Builds a minimal [`UserEntity`] carrying only the id we care about.
    /// [`UserEntity`] has no `Default`, so every field is spelled out.
    fn user_with_id(id: &str) -> UserEntity {
        UserEntity {
            id: id.to_owned(),
            audio_language_preference: None,
            authentication_provider_id: String::new(),
            cast_receiver_id: None,
            display_collections_view: false,
            display_missing_episodes: false,
            enable_auto_login: false,
            enable_local_password: false,
            enable_next_episode_auto_play: false,
            enable_user_preference_access: false,
            hide_played_in_latest: false,
            internal_id: 0,
            invalid_login_attempt_count: 0,
            last_activity_date: None,
            last_login_date: None,
            login_attempts_before_lockout: None,
            max_active_sessions: 0,
            max_parental_rating_score: None,
            max_parental_rating_sub_score: None,
            must_update_password: false,
            normalized_username: String::new(),
            password: None,
            password_reset_provider_id: String::new(),
            play_default_audio_track: false,
            remember_audio_selections: false,
            remember_subtitle_selections: false,
            remote_client_bitrate_limit: None,
            row_version: 0,
            subtitle_language_preference: None,
            subtitle_mode: 0,
            sync_play_access: 0,
            username: String::new(),
        }
    }

    #[test]
    fn user_id_is_nil_without_user() {
        assert_eq!(AuthorizationInfo::default().user_id(), Uuid::nil());
    }

    #[test]
    fn user_id_parses_entity_id() {
        let id = Uuid::from_u128(0x1234);
        let info = AuthorizationInfo {
            user: Some(user_with_id(&id.to_string())),
            ..Default::default()
        };
        assert_eq!(info.user_id(), id);
    }

    #[test]
    fn has_token_ignores_blank() {
        let mut info = AuthorizationInfo::default();
        assert!(!info.has_token());
        info.token = Some("   ".into());
        assert!(!info.has_token());
        info.token = Some("abc".into());
        assert!(info.has_token());
    }
}
