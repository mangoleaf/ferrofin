//! Shared direct-play (static) file-serving helpers for the streaming
//! controllers (`Videos`, `Audio`, `UniversalAudio`).
//!
//! Every one of those controllers, on its non-transcoding path, resolves an
//! item's static [`MediaSourceInfo`](ferrofin_model::dto::MediaSourceInfo), takes
//! its on-disk path, and serves the file with HTTP `Range` support. The
//! transcoding / HLS / stream-copy machinery those controllers also expose is
//! deferred (no ffmpeg runner), so only this direct-stream slice is ported; it is
//! factored here so `videos`/`audio` don't each duplicate it.

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Resolves the direct-play file path for an item's first static media source.
///
/// Ports the direct-stream lookup shared by the streaming controllers: the item's
/// static (non-probed) source's on-disk `Path`. Returns `404` when the item has no
/// static media source, or that source has no on-disk path (nothing to
/// direct-stream). The transcoding parameters the controllers accept are ignored
/// here — only the static path is served.
pub(crate) async fn stream_path(state: &AppState, item_id: Uuid) -> Result<String, ApiError> {
    let sources = state
        .media_sources
        .get_static_media_sources(item_id, true, None)
        .await?;
    sources
        .into_iter()
        .find_map(|s| s.path)
        .ok_or_else(|| ApiError::NotFound(format!("no direct stream for item {item_id}")))
}

/// Serves the file at `path` for `request` **with range processing disabled**.
///
/// Mirrors ASP.NET Core's two-argument `PhysicalFile(path, contentType)`, the
/// overload that leaves `FileResult.EnableRangeProcessing` at its `false`
/// default. `FileResultExecutorBase.SetHeadersAndLog` only calls
/// `SetAcceptRangeHeader` inside the `enableRangeProcessing` branch, so such a
/// response advertises no `Accept-Ranges` **and** ignores an inbound `Range`:
/// the client gets `200` with the whole file, never a `206`.
///
/// Reproduced here by stripping `Range`/`If-Range` from the request before
/// [`serve_static_file`] sees them and dropping the `Accept-Ranges` the
/// underlying `ServeFile` adds. Use this only for routes the C# serves through
/// that overload — the trickplay tile is one; direct play and the HLS segments
/// are not, and must keep their range support.
pub(crate) async fn serve_static_file_without_ranges(
    path: &str,
    mut request: Request,
) -> Result<Response, ApiError> {
    request.headers_mut().remove(axum::http::header::RANGE);
    request.headers_mut().remove(axum::http::header::IF_RANGE);
    let mut response = serve_static_file(path, request).await?;
    response
        .headers_mut()
        .remove(axum::http::header::ACCEPT_RANGES);
    Ok(response)
}

/// Serves the file at `path` for `request`, honouring `Range`/`HEAD`.
///
/// Delegates to [`tower_http::services::ServeFile`], which performs the
/// `Range`/`HEAD`/`206 Partial Content`/`404` handling; its infallible response is
/// mapped into an axum body. A resolution/IO failure surfaces as `404`.
pub(crate) async fn serve_static_file(path: &str, request: Request) -> Result<Response, ApiError> {
    let response = ServeFile::new(path)
        .oneshot(request)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;
    // A path the database resolved but the filesystem rejects otherwise
    // surfaces as a bare 404 indistinguishable from a missing route — name the
    // fs-level cause (stale NFS handle, permissions, moved file) in the log.
    // Only on the 404 branch: this helper serves every HLS segment and every
    // Range request of a direct play, and an unconditional pre-`stat` doubled
    // the metadata syscalls on that path (two round trips per request on
    // network storage) to produce a diagnostic that never fired. A 206/416 is
    // about the Range header, not the file, so it is not interesting here.
    if response.status() == axum::http::StatusCode::NOT_FOUND {
        let reason = tokio::fs::metadata(path)
            .await
            .err()
            .map_or_else(|| "unreadable".to_owned(), |e| e.to_string());
        tracing::warn!(path, error = reason, "direct-stream file is not accessible");
    }
    Ok(response.map(Body::new))
}

/// Runs the response-completion callback when the body terminates, including
/// cancellation. ASP.NET Core v10.0.0 Kestrel HttpProtocol.ProcessRequests calls
/// FireOnCompleted after both normal and aborted responses.
struct ResponseCompletion(
    Option<std::sync::Arc<dyn ferrofin_traits::media_encoding::TranscodeStreamProgress>>,
);
impl Drop for ResponseCompletion {
    fn drop(&mut self) {
        if let Some(observer) = &self.0 {
            observer.complete();
        }
    }
}

/// Counts reads and advances the dynamic segment when its response terminates.
pub(crate) fn track_transcode_body(
    body: Body,
    observer: std::sync::Arc<dyn ferrofin_traits::media_encoding::TranscodeStreamProgress>,
) -> Body {
    use futures_util::StreamExt as _;
    Body::from_stream(futures_util::stream::unfold(
        (
            body.into_data_stream(),
            ResponseCompletion(Some(observer)),
            false,
        ),
        |(mut stream, completion, failed)| async move {
            if failed {
                return None;
            }
            match stream.next().await {
                Some(Ok(bytes)) => {
                    if let Some(observer) = &completion.0 {
                        observer.add_bytes(bytes.len());
                    }
                    Some((Ok::<_, axum::Error>(bytes), (stream, completion, false)))
                }
                Some(Err(error)) => Some((Err(error), (stream, completion, true))),
                None => None,
            }
        },
    ))
}

/// Reads a progressive output while ffmpeg is still extending it.
/// An empty read waits 50ms while the producing process lives, as upstream's
/// ProgressiveFileStream does. A body termination releases its request mark.
pub(crate) async fn serve_growing_transcode(
    path: &str,
    request: Request,
    observer: Option<std::sync::Arc<dyn ferrofin_traits::media_encoding::TranscodeStreamProgress>>,
) -> Result<Response, ApiError> {
    use tokio::io::AsyncReadExt as _;
    let completion = ResponseCompletion(observer);
    if request.method() == axum::http::Method::HEAD {
        let mut response = Response::new(Body::empty());
        response.headers_mut().insert(
            axum::http::header::ACCEPT_RANGES,
            axum::http::HeaderValue::from_static("none"),
        );
        return Ok(response);
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| ApiError::NotFound(error.to_string()))?;
    let body = Body::from_stream(futures_util::stream::unfold(
        (file, completion, false),
        |(mut file, completion, failed)| async move {
            if failed {
                return None;
            }
            let mut buffer = vec![0; 32_768];
            loop {
                match file.read(&mut buffer).await {
                    Ok(0)
                        if completion
                            .0
                            .as_ref()
                            .is_some_and(|observer| !observer.has_exited()) =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Ok(0) => return None,
                    Ok(length) => {
                        buffer.truncate(length);
                        if let Some(observer) = &completion.0 {
                            observer.add_bytes(length);
                        }
                        return Some((Ok::<_, std::io::Error>(buffer), (file, completion, false)));
                    }
                    Err(error) => return Some((Err(error), (file, completion, true))),
                }
            }
        },
    ));
    let mut response = Response::new(body);
    response.headers_mut().insert(
        axum::http::header::ACCEPT_RANGES,
        axum::http::HeaderValue::from_static("none"),
    );
    Ok(response)
}

#[cfg(test)]
mod transcode_body_tests {
    use super::*;
    use ferrofin_traits::media_encoding::TranscodeStreamProgress;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct Observer {
        bytes: AtomicUsize,
        completed: AtomicBool,
        exited: AtomicBool,
    }

    impl TranscodeStreamProgress for Observer {
        fn add_bytes(&self, bytes: usize) {
            self.bytes.fetch_add(bytes, Ordering::SeqCst);
        }
        fn complete(&self) {
            self.completed.store(true, Ordering::SeqCst);
        }
        fn has_exited(&self) -> bool {
            self.exited.load(Ordering::SeqCst)
        }
        fn is_progressive(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn completion_counts_actual_chunks_and_disconnect_runs_callback() {
        use futures_util::StreamExt as _;
        let observer = Arc::new(Observer::default());
        let body = track_transcode_body(Body::from("0123456789"), observer.clone());
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), b"0123456789");
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 10);
        assert!(observer.completed.load(Ordering::SeqCst));
        let observer = Arc::new(Observer::default());
        let mut body =
            track_transcode_body(Body::from("unfinished"), observer.clone()).into_data_stream();
        assert!(body.next().await.is_some());
        drop(body);
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 10);
        assert!(observer.completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn growing_output_streams_appended_bytes_until_process_exit() {
        use futures_util::StreamExt as _;
        use std::io::Write as _;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stream.ts");
        std::fs::write(&path, b"first").unwrap();
        let observer = Arc::new(Observer::default());
        let response = serve_growing_transcode(
            path.to_str().unwrap(),
            Request::new(Body::empty()),
            Some(observer.clone()),
        )
        .await
        .unwrap();
        assert!(
            !response
                .headers()
                .contains_key(axum::http::header::CONTENT_LENGTH)
        );
        let mut body = response.into_body().into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap().as_ref(), b"first");
        assert!(!observer.completed.load(Ordering::SeqCst));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"second")
            .unwrap();
        assert_eq!(body.next().await.unwrap().unwrap().as_ref(), b"second");
        observer.exited.store(true, Ordering::SeqCst);
        assert!(body.next().await.is_none());
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 11);
        assert!(observer.completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn growing_read_cancellation_and_missing_file_run_completion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stream.ts");
        std::fs::write(&path, b"").unwrap();
        let observer = Arc::new(Observer::default());
        let response = serve_growing_transcode(
            path.to_str().unwrap(),
            Request::new(Body::empty()),
            Some(observer.clone()),
        )
        .await
        .unwrap();
        let read = axum::body::to_bytes(response.into_body(), usize::MAX);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(75), read)
                .await
                .is_err()
        );
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 0);
        assert!(observer.completed.load(Ordering::SeqCst));
        let observer = Arc::new(Observer::default());
        assert!(
            serve_growing_transcode(
                directory.path().join("missing").to_str().unwrap(),
                Request::new(Body::empty()),
                Some(observer.clone())
            )
            .await
            .is_err()
        );
        assert!(observer.completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn failed_segment_body_still_runs_its_completion_callback() {
        let observer = Arc::new(Observer::default());
        let failed = Body::from_stream(futures_util::stream::once(async {
            Err::<Vec<u8>, _>(std::io::Error::other("fixture disconnected"))
        }));
        assert!(
            axum::body::to_bytes(track_transcode_body(failed, observer.clone()), usize::MAX)
                .await
                .is_err()
        );
        assert_eq!(observer.bytes.load(Ordering::SeqCst), 0);
        assert!(observer.completed.load(Ordering::SeqCst));
    }
}
