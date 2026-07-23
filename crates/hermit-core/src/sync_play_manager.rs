//! [`HermitSyncPlayManager`] — a **no-op** [`SyncPlayManager`] for the deferred
//! SyncPlay subsystem.
//!
//! Port of the disabled shape of
//! `Emby.Server.Implementations.SyncPlay.SyncPlayManager`. SyncPlay (synchronized
//! group playback) is not part of the Hermit v1 feature set, so the group
//! coordinator, the `IGroupPlaybackRequest` strategy hierarchy and the
//! request/response envelopes are not ported. Group creation and joins are
//! rejected as unsupported, listings are empty, and no user is ever reported as
//! active.
//!
//! The seam exists only so the DI graph can name an `Arc<dyn SyncPlayManager>`;
//! a real implementation is a future wave.

use async_trait::async_trait;
use hermit_model::sync_play::GroupInfoDto;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::stubs::SyncPlayManager;

/// The no-op SyncPlay manager for the deferred SyncPlay subsystem.
///
/// Mutating group operations return [`ServiceError::InvalidInput`] ("SyncPlay is
/// not enabled") and read operations return empty/negative results.
#[derive(Debug, Clone, Copy, Default)]
pub struct HermitSyncPlayManager;

impl HermitSyncPlayManager {
    /// Creates the no-op SyncPlay manager.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// The shared rejection for the mutating operations while SyncPlay is off.
    fn disabled() -> ServiceError {
        ServiceError::invalid_input("SyncPlay is not enabled on this server")
    }
}

#[async_trait]
impl SyncPlayManager for HermitSyncPlayManager {
    async fn new_group(
        &self,
        _session_id: &str,
        _group_name: &str,
    ) -> Result<GroupInfoDto, ServiceError> {
        Err(Self::disabled())
    }

    async fn join_group(&self, _session_id: &str, _group_id: Uuid) -> Result<(), ServiceError> {
        Err(Self::disabled())
    }

    async fn leave_group(&self, _session_id: &str) -> Result<(), ServiceError> {
        // Leaving is idempotent: with no groups, this is a successful no-op.
        Ok(())
    }

    async fn list_groups(&self, _session_id: &str) -> Result<Vec<GroupInfoDto>, ServiceError> {
        Ok(Vec::new())
    }

    async fn is_user_active(&self, _user_id: Uuid) -> Result<bool, ServiceError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use hermit_traits::error::ServiceError;
    use hermit_traits::stubs::SyncPlayManager;

    use super::HermitSyncPlayManager;

    #[tokio::test]
    async fn mutations_rejected_reads_empty() {
        let mgr = HermitSyncPlayManager::new();
        assert!(matches!(
            mgr.new_group("s", "g").await,
            Err(ServiceError::InvalidInput(_))
        ));
        assert!(matches!(
            mgr.join_group("s", Uuid::new_v4()).await,
            Err(ServiceError::InvalidInput(_))
        ));
        mgr.leave_group("s").await.expect("leave is a no-op");
        assert!(mgr.list_groups("s").await.expect("list").is_empty());
        assert!(!mgr.is_user_active(Uuid::new_v4()).await.expect("active"));
    }
}
