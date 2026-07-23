//! [`HermitLyricManager`] — a **stub** [`LyricManager`] for the deferred lyrics
//! subsystem.
//!
//! Port of the disabled shape of
//! `Emby.Server.Implementations.Lyrics.LyricManager`. Lyrics are not part of the
//! Hermit v1 feature set, so the `ILyricProvider` per-backend strategy interface
//! and on-disk `.lrc` sidecar handling are not ported. No stored lyrics exist,
//! remote search yields nothing, downloads/saves are rejected as unsupported,
//! and no providers are advertised.
//!
//! The seam exists so the DI graph can name an `Arc<dyn LyricManager>`; a real
//! lyrics host is a future wave.

use async_trait::async_trait;
use hermit_model::lyrics::{LyricDto, RemoteLyricInfoDto};
use hermit_model::providers::LyricProviderInfo;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::stubs::LyricManager;

/// The stub lyrics manager for the deferred lyrics subsystem.
///
/// Read paths return empty results; the download/save mutators return
/// [`ServiceError::InvalidInput`] ("lyrics are not enabled").
#[derive(Debug, Clone, Copy, Default)]
pub struct HermitLyricManager;

impl HermitLyricManager {
    /// Creates the stub lyrics manager.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// The shared rejection for the mutating operations while lyrics are off.
    fn disabled() -> ServiceError {
        ServiceError::invalid_input("lyrics are not enabled on this server")
    }
}

#[async_trait]
impl LyricManager for HermitLyricManager {
    async fn get_lyrics(&self, _item_id: Uuid) -> Result<Option<LyricDto>, ServiceError> {
        // Lyrics are deferred: nothing is stored.
        Ok(None)
    }

    async fn search_lyrics(&self, _item_id: Uuid) -> Result<Vec<RemoteLyricInfoDto>, ServiceError> {
        Ok(Vec::new())
    }

    async fn download_lyrics(
        &self,
        _item_id: Uuid,
        _lyric_id: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        Err(Self::disabled())
    }

    async fn save_lyric(
        &self,
        _item_id: Uuid,
        _format: &str,
        _lyrics: &str,
    ) -> Result<Option<LyricDto>, ServiceError> {
        Err(Self::disabled())
    }

    async fn delete_lyrics(&self, _item_id: Uuid) -> Result<(), ServiceError> {
        // Deleting is idempotent: with nothing stored, this is a no-op.
        Ok(())
    }

    async fn get_supported_providers(
        &self,
        _item_id: Uuid,
    ) -> Result<Vec<LyricProviderInfo>, ServiceError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use hermit_traits::error::ServiceError;
    use hermit_traits::stubs::LyricManager;

    use super::HermitLyricManager;

    #[tokio::test]
    async fn reads_empty_mutations_rejected() {
        let mgr = HermitLyricManager::new();
        let id = Uuid::new_v4();
        assert!(mgr.get_lyrics(id).await.expect("get").is_none());
        assert!(mgr.search_lyrics(id).await.expect("search").is_empty());
        assert!(matches!(
            mgr.download_lyrics(id, "x").await,
            Err(ServiceError::InvalidInput(_))
        ));
        assert!(matches!(
            mgr.save_lyric(id, "lrc", "text").await,
            Err(ServiceError::InvalidInput(_))
        ));
        mgr.delete_lyrics(id).await.expect("delete is a no-op");
        assert!(
            mgr.get_supported_providers(id)
                .await
                .expect("providers")
                .is_empty()
        );
    }
}
