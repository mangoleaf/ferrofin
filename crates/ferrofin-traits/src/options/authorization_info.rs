//! Port of `MediaBrowser.Controller.Net.AuthorizationInfo`.

use std::sync::Arc;

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::secret::Secret;
use ferrofin_model::users::UserPolicy;
use uuid::Uuid;

/// The authorization context resolved for an incoming request.
///
/// Mirrors C# `AuthorizationInfo`. Port rule applied: the C# `User` domain
/// property becomes an [`Option`]`<`[`Arc`]`<`[`UserEntity`]`>>` (the
/// persistence row, shared with the auth cache and the queries built from it).
/// This is a server-side request-context value, never serialized over the wire,
/// so it carries no serde derives (and [`UserEntity`] has none either).
#[derive(Debug, Clone, PartialEq, Default)]
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
    ///
    /// Shared, not owned: the auth cache, every copy of this info and every
    /// query a handler builds from it point at the one row.
    pub user: Option<Arc<UserEntity>>,

    /// Policy resolved with the user token, sharing its cache invalidation.
    /// Absent for API keys and requests without a resolved user.
    pub user_policy: Option<Arc<UserPolicy>>,

    /// Whether the token authenticated successfully.
    pub is_authenticated: bool,

    /// The caller's normalized remote IP, when the transport knew one.
    ///
    /// C# has no such field: `HttpContext` is ambient, so every controller
    /// reaches `HttpContext.GetNormalizedRemoteIP()` directly — notably
    /// `RequestHelpers.GetSession`, which re-logs session activity with it on
    /// every request. Ferrofin's handlers receive the resolved
    /// [`AuthorizationInfo`] instead of the request, so the address rides along
    /// here. Without it `SessionInfo.RemoteEndPoint` is blank and every
    /// activity-log `ShortOverview` ("IP address: …") built from it is null.
    pub remote_endpoint: Option<String>,
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
pub(crate) mod tests {
    use std::sync::Arc;

    use super::{AuthorizationInfo, UserEntity};
    use uuid::Uuid;

    /// Builds a minimal [`UserEntity`] carrying only the id we care about.
    /// [`UserEntity`] has no `Default`, so every field is spelled out.
    pub(crate) fn user_with_id(id: &str) -> UserEntity {
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
            normalized_username: String::new(),
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
            user: Some(Arc::new(user_with_id(&id.to_string()))),
            ..Default::default()
        };
        assert_eq!(info.user_id(), id);
    }

    /// A handler copies the auth info out of the request (`auth.rs`) and the
    /// user rides along into every query it builds: the copy must share the
    /// row, not duplicate each of its `String` columns.
    #[test]
    fn a_copy_shares_the_user_row() {
        let info = AuthorizationInfo {
            user: Some(Arc::new(user_with_id("u"))),
            ..Default::default()
        };
        let copy = info.clone();
        assert!(Arc::ptr_eq(
            info.user.as_ref().expect("user"),
            copy.user.as_ref().expect("user")
        ));
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
