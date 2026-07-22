//! Minimal Live TV manager trait (deferred subsystem).
//!
//! Port of a representative slice of
//! `MediaBrowser.Controller.LiveTv.ILiveTvManager`. Live TV is deferred, so the
//! full timer/recording/tuner surface and the `ILiveTvService` per-backend
//! strategy interface are **not** ported; only enough to establish the seam.
//!
//! Port rules applied: DTO-shaped results reuse `hermit-model` DTOs
//! ([`LiveTvInfo`], `QueryResult<BaseItemDto>`); identity args are
//! [`uuid::Uuid`]; `Task<T>` → `async fn -> Result<T, ServiceError>`.

use async_trait::async_trait;
use hermit_model::dto::BaseItemDto;
use hermit_model::live_tv::LiveTvInfo;
use hermit_model::querying::QueryResult;

use crate::error::ServiceError;
use crate::options::{DtoOptions, InternalItemsQuery};

/// The (deferred) Live TV manager.
///
/// Port of `ILiveTvManager` (minimal slice).
#[async_trait]
pub trait LiveTvManager: Send + Sync {
    /// Gets top-level Live TV service/status information.
    async fn get_live_tv_info(&self) -> Result<LiveTvInfo, ServiceError>;

    /// Queries Live TV programs (EPG entries).
    async fn get_programs(
        &self,
        query: &InternalItemsQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError>;

    /// Resets the tuner backing the given channel/recording id.
    async fn reset_tuner(&self, id: &str) -> Result<(), ServiceError>;
}

fn _assert_object_safe_live_tv_manager(_: &dyn LiveTvManager) {}
