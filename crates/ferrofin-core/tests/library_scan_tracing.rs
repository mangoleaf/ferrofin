//! Scan tracing needs a process-wide subscriber: SQLite's worker threads can
//! close the last child span, and tracing's registry uses the closing thread's
//! default dispatcher to close its parent. A thread-local subscriber can leave
//! the root unexported. Keep this test in its own binary so the global subscriber
//! does not interfere with the other tracing tests.

use std::sync::Arc;
use std::time::Duration;

use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_core::library_scan::LibraryScanner;
use ferrofin_core::{
    FerrofinFileSystem, FerrofinItemCountService, FerrofinItemPersistenceService,
    FerrofinItemRepository, FerrofinLibraryManager, FerrofinPeopleRepository,
    FerrofinVirtualFolderManager,
};
use ferrofin_db::Database;
use ferrofin_traits::library::LibraryManager;
use opentelemetry::trace::{SpanId, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt as _;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_library_scan_exports_a_library_scan_span_tagged_with_trigger() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_simple_exporter(exporter.clone())
        .build();
    let layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("ferrofin"));
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
        .expect("global tracing subscriber");

    let db = Database::connect_in_memory().await.expect("database");
    db.run_migrations().await.expect("migrations");
    let tmp = tempfile::tempdir().expect("tempdir");
    let persistence = Arc::new(FerrofinItemPersistenceService::new(db.clone()));
    // No virtual folders: exercise the real scanner and SQLite without media.
    let folders = Arc::new(
        FerrofinVirtualFolderManager::new(tmp.path().join("default"))
            .with_item_store(persistence.clone()),
    );
    let scanner = Arc::new(LibraryScanner::new(
        folders,
        Arc::new(FerrofinFileSystem::new()),
        persistence.clone(),
    ));
    let mgr = FerrofinLibraryManager::new(
        Arc::new(FerrofinItemRepository::new(
            db.clone(),
            Arc::new(ItemTypeLookup::new()),
        )),
        Arc::new(FerrofinItemCountService::new(db.clone())),
        persistence,
        Arc::new(FerrofinPeopleRepository::new(db.clone())),
    )
    .with_scanner(scanner);

    mgr.queue_library_scan().await.expect("queued");
    // Queue completion can precede the last span reference being dropped on a
    // SQLite worker. Wait for the export itself; force_flush cannot close spans.
    let span = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(span) = exporter
                .get_finished_spans()
                .expect("spans")
                .into_iter()
                .find(|s| s.name == "library_scan")
            {
                break span;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("library_scan span exported");
    assert_eq!(span.parent_span_id, SpanId::INVALID, "scan is a root span");
    let trigger = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "trigger")
        .map(|kv| kv.value.to_string());
    assert_eq!(trigger.as_deref(), Some("api"));

    mgr.shutdown_scans().await;
    db.close().await;
    provider.shutdown().expect("tracer shutdown");
}
