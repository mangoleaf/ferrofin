//! Minimal SyncPlay manager trait (deferred subsystem).
//!
//! Port of a representative slice of
//! `MediaBrowser.Controller.SyncPlay.ISyncPlayManager`. SyncPlay is deferred, so
//! the `SessionInfo` domain object, the `IGroupPlaybackRequest` strategy
//! hierarchy and the full request/response envelopes are **not** ported; the
//! session is identified by id and the group by [`uuid::Uuid`].
//!
//! Port rules applied: group descriptors reuse the [`GroupInfoDto`] wire DTO;
//! synchronous C# methods become `async fn -> Result` (the impl coordinates
//! across sessions).

use async_trait::async_trait;
use hermit_model::sync_play::GroupInfoDto;
use uuid::Uuid;

use crate::error::ServiceError;

/// The (deferred) SyncPlay manager.
///
/// Port of `ISyncPlayManager` (minimal slice). `SessionInfo` becomes a session
/// id and group requests are reduced to their identifying arguments.
#[async_trait]
pub trait SyncPlayManager: Send + Sync {
    /// Creates a new SyncPlay group owned by the session, returning its info.
    async fn new_group(
        &self,
        session_id: &str,
        group_name: &str,
    ) -> Result<GroupInfoDto, ServiceError>;

    /// Adds the session to an existing group.
    async fn join_group(&self, session_id: &str, group_id: Uuid) -> Result<(), ServiceError>;

    /// Removes the session from its current group.
    async fn leave_group(&self, session_id: &str) -> Result<(), ServiceError>;

    /// Lists the groups visible to the session.
    async fn list_groups(&self, session_id: &str) -> Result<Vec<GroupInfoDto>, ServiceError>;

    /// Whether the user currently participates in any group.
    async fn is_user_active(&self, user_id: Uuid) -> Result<bool, ServiceError>;
}

fn _assert_object_safe_sync_play_manager(_: &dyn SyncPlayManager) {}
