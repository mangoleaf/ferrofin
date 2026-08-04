//! The source-fetch seam.
//!
//! `refresh_guide` needs to read the configured M3U and XMLTV URLs. That network
//! I/O sits behind [`SourceFetcher`] so the manager's channel/guide-population
//! logic can be unit-tested with an in-memory fake instead of a live server.

use async_trait::async_trait;

use hermit_traits::error::ServiceError;

use crate::error::LiveTvError;

/// Fetches the text body of a tuner/guide source URL (or local file path).
#[async_trait]
pub trait SourceFetcher: Send + Sync {
    /// Fetches `url` and returns its body as text.
    async fn fetch(&self, url: &str) -> Result<String, ServiceError>;
}

/// The real fetcher: `reqwest` for `http(s)://`, a filesystem read otherwise.
#[derive(Debug, Clone, Default)]
pub struct ReqwestFetcher {
    client: reqwest::Client,
}

impl ReqwestFetcher {
    /// Creates a fetcher over a fresh `reqwest` client.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SourceFetcher for ReqwestFetcher {
    async fn fetch(&self, url: &str) -> Result<String, ServiceError> {
        if url.starts_with("http://") || url.starts_with("https://") {
            let resp = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|e| LiveTvError::http(format!("fetch {url}"), e))?;
            let resp = resp
                .error_for_status()
                .map_err(|e| LiveTvError::http(format!("fetch {url}"), e))?;
            resp.text()
                .await
                .map_err(|e| LiveTvError::http(format!("read {url}"), e).into())
        } else {
            let path = url.strip_prefix("file://").unwrap_or(url);
            tokio::fs::read_to_string(path)
                .await
                .map_err(|e| LiveTvError::io(format!("read {path}"), e).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReqwestFetcher, SourceFetcher};

    #[tokio::test]
    async fn reads_a_local_file_path() {
        let dir = std::env::temp_dir();
        let path = dir.join("hermit_livetv_fetch_test.m3u");
        tokio::fs::write(&path, "#EXTM3U\n").await.expect("write");
        let body = ReqwestFetcher::new()
            .fetch(path.to_str().unwrap())
            .await
            .expect("fetch");
        assert_eq!(body, "#EXTM3U\n");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let err = ReqwestFetcher::new()
            .fetch("/nonexistent/hermit/livetv/nope.xml")
            .await;
        assert!(err.is_err());
    }
}
