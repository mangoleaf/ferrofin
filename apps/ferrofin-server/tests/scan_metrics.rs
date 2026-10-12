//! The library-scan metrics end to end, through the real composition root and
//! the real Prometheus exporter.
//!
//! Its own binary: `ferrofin_metrics::init` and the subsystem `install`s set
//! process globals (once per process). The test boots the composition root
//! ([`build_app_state`]) over a temp data dir and a two-movie library whose
//! remote fetchers are off (no network), with `ffprobe` pointed at a stub that
//! answers a minimal probe and logs each call. It then mirrors what the
//! composition root does when `EnableMetrics` is set and proves on the rendered
//! `/metrics`:
//!
//! - with metrics disabled a scan runs normally and records nothing: the
//!   route is absent, and once metrics are enabled afterwards every scan and
//!   probe series still reads zero although ffprobe really ran;
//! - a scheduled scan counts one completed `schedule` pass, its created and
//!   unchanged items, a duration observation (in the configured
//!   `metrics_scan_duration_buckets`), its completion time, its closing passes
//!   and one `ok` probe per new movie (equal to the stub's own count);
//! - an unchanged rescan counts only `unchanged` items and **no** probe;
//! - a webhook report runs as `trigger="webhook"` and creates exactly the new
//!   movie.

// The counters render as exact integers, so comparing their f64 samples
// exactly is correct.
#![allow(clippy::float_cmp)]

use std::path::Path;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ferrofin_db::Database;
use ferrofin_model::configuration::{LibraryOptions, MediaPathInfo, TypeOptions};
use ferrofin_model::entities::CollectionTypeOptions;
use ferrofin_server::config::Config;
use ferrofin_server::metrics_wiring;
use ferrofin_server::state::build_app_state;
use ferrofin_traits::library::ScanTrigger;
use tower::ServiceExt as _;

/// A minimal ffprobe answer: one video stream, a format with a duration.
const PROBE_JSON: &str = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":640,"height":360}],"format":{"filename":"stub","format_name":"matroska,webm","duration":"60.000000","size":"1024","bit_rate":"1000"}}"#;

/// Writes an executable `ffprobe` stub into `dir` that appends one line to
/// `calls` per invocation and prints [`PROBE_JSON`].
fn write_ffprobe_stub(dir: &Path, calls: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let script = dir.join("ffprobe");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho x >> '{}'\ncat <<'EOF'\n{PROBE_JSON}\nEOF\n",
            calls.display()
        ),
    )
    .expect("write ffprobe stub");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod ffprobe stub");
    script
}

/// Creates `Title (Year)/Title (Year).mkv` under `root`.
fn write_movie(root: &Path, title: &str) -> std::path::PathBuf {
    let dir = root.join(title);
    std::fs::create_dir_all(&dir).expect("movie dir");
    let file = dir.join(format!("{title}.mkv"));
    std::fs::write(&file, [0u8; 1024]).expect("movie file");
    file
}

/// The value of the one series of `name` carrying every `labels` pair.
fn sample(exposition: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    exposition.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?;
        let (label_set, value) = match rest.strip_prefix('{') {
            Some(rest) => rest.split_once("} ")?,
            None => ("", rest.strip_prefix(' ')?),
        };
        labels
            .iter()
            .all(|(k, v)| label_set.contains(&format!("{k}=\"{v}\"")))
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

async fn scrape(router: &axum::Router) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&body).into_owned()
}

fn probe_calls(calls: &Path) -> usize {
    std::fs::read_to_string(calls).map_or(0, |s| s.lines().count())
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn scans_record_trigger_outcomes_passes_and_probes() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = Config {
        server_name: "ferrofin-scan-metrics".to_owned(),
        ..Config::test_stub(temp.path())
    };
    std::fs::create_dir_all(&config.config_dir).expect("config dir");
    std::fs::create_dir_all(&config.data_dir).expect("data dir");
    let media = temp.path().join("media");
    write_movie(&media, "Film Alpha (1999)");
    let calls = temp.path().join("ffprobe.calls");
    let ffprobe = write_ffprobe_stub(temp.path(), &calls);

    let db = Database::connect(&config.database_url())
        .await
        .expect("open db");
    db.run_migrations().await.expect("migrations");
    let ffmpeg = ferrofin_server::bootstrap::FfmpegPaths {
        encoder_app_path_display: None,
        ffmpeg: "ffmpeg".into(),
        ffprobe,
        capabilities: ferrofin_mediaencoding::FfmpegCapabilities::default(),
        chromaprint_muxer: false,
    };
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let wired = build_app_state(&db, &config, &ffmpeg, None, tx)
        .await
        .expect("wire app state");

    // A movies library with every remote fetcher off (no network).
    wired
        .state
        .virtual_folders
        .add_virtual_folder(
            "Movies",
            Some(CollectionTypeOptions::movies),
            &LibraryOptions {
                path_infos: vec![MediaPathInfo {
                    path: media.to_string_lossy().into_owned(),
                }],
                type_options: vec![TypeOptions {
                    type_: Some("Movie".to_owned()),
                    ..TypeOptions::default()
                }],
                ..LibraryOptions::default()
            },
        )
        .await
        .expect("add library");

    // ---- metrics disabled: the scan runs, nothing is mounted or recorded ----
    assert!(
        wired
            .state
            .library
            .run_library_scan(ScanTrigger::Schedule, None)
            .await
            .expect("scan"),
        "the scan completed with metrics disabled"
    );
    assert_eq!(probe_calls(&calls), 1, "ffprobe ran for the movie");
    let plain = ferrofin_api::create_router(wired.state.clone());
    let response = plain
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // ---- enable: what the composition root does when EnableMetrics is set ----
    let handle = ferrofin_metrics::init(
        ferrofin_metrics::RouteLabels::default(),
        tokio::runtime::Handle::current(),
    )
    .expect("metrics init");
    let _gauges = metrics_wiring::register_gauges(&handle);
    // A configured bucket list (`metrics_scan_duration_buckets`).
    metrics_wiring::install_subsystem_instruments(Some(&[0.01, 1.0, 100.0]));
    let router = metrics_wiring::mount(ferrofin_api::create_router(wired.state.clone()), &handle);

    // Every series exists at zero: the disabled scan (and its real ffprobe
    // run) left no trace.
    let before = scrape(&router).await;
    for trigger in ScanTrigger::ALL {
        for result in ["completed", "stopped", "failed"] {
            assert_eq!(
                sample(
                    &before,
                    "ferrofin_library_scans_total",
                    &[("trigger", trigger.as_str()), ("result", result)]
                ),
                Some(0.0),
                "{trigger} {result}:\n{before}"
            );
        }
        assert_eq!(
            sample(
                &before,
                "ferrofin_library_scan_items_total",
                &[("trigger", trigger.as_str()), ("outcome", "created")]
            ),
            Some(0.0)
        );
    }
    for result in ["ok", "failed", "cancelled"] {
        assert_eq!(
            sample(&before, "ferrofin_media_probe_total", &[("result", result)]),
            Some(0.0),
            "{result}"
        );
    }
    for result in ["completed", "stopped", "failed"] {
        assert_eq!(
            sample(
                &before,
                "ferrofin_library_scan_lane_refreshes_total",
                &[("result", result)]
            ),
            Some(0.0),
            "{result}"
        );
    }
    for provider in ferrofin_providers::metrics::PROVIDERS {
        assert_eq!(
            sample(
                &before,
                "ferrofin_metadata_provider_retries_total",
                &[("provider", provider)]
            ),
            Some(0.0),
            "{provider}"
        );
        for result in ["ok", "not_found", "failed", "skipped"] {
            assert_eq!(
                sample(
                    &before,
                    "ferrofin_metadata_provider_requests_total",
                    &[("provider", provider), ("result", result)]
                ),
                Some(0.0),
                "{provider} {result}"
            );
        }
    }
    assert_eq!(
        sample(&before, "ferrofin_library_scan_in_progress", &[]),
        Some(0.0)
    );
    assert!(!before.contains("ferrofin_library_scan_duration_seconds_count"));

    // ---- a scheduled scan: the new movie created, the old one unchanged ----
    write_movie(&media, "Film Beta (2004)");
    assert!(
        wired
            .state
            .library
            .run_library_scan(ScanTrigger::Schedule, None)
            .await
            .expect("scan")
    );
    // A scan waiter is released when its pass finishes. The queue worker
    // drops ScanInProgress only after draining the queue, so its gauge may
    // still be 1 immediately after run_library_scan returns.
    let first = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let metrics = scrape(&router).await;
            if sample(&metrics, "ferrofin_library_scan_in_progress", &[]) == Some(0.0) {
                break metrics;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("scan queue worker became idle after the completed pass");
    let schedule = ("trigger", "schedule");
    let items = |text: &str, outcome: &str| {
        sample(
            text,
            "ferrofin_library_scan_items_total",
            &[schedule, ("outcome", outcome)],
        )
        .unwrap_or_else(|| panic!("no {outcome} series:\n{text}"))
    };
    assert_eq!(
        sample(
            &first,
            "ferrofin_library_scans_total",
            &[schedule, ("result", "completed")]
        ),
        Some(1.0),
        "{first}"
    );
    let created = items(&first, "created");
    assert!(created >= 1.0, "the new movie was created: {created}");
    assert!(
        items(&first, "unchanged") >= 1.0,
        "the movie scanned before is unchanged:\n{first}"
    );
    assert_eq!(
        sample(
            &first,
            "ferrofin_library_scan_duration_seconds_count",
            &[schedule]
        ),
        Some(1.0)
    );
    // The histograms carry the configured boundaries.
    let bounds: Vec<&str> = first
        .lines()
        .filter(|l| {
            l.starts_with("ferrofin_library_scan_duration_seconds_bucket")
                && l.contains(r#"trigger="schedule""#)
        })
        .filter_map(|l| l.split(r#"le=""#).nth(1)?.split('"').next())
        .collect();
    assert_eq!(bounds, vec!["0.01", "1", "100", "+Inf"], "{first}");
    assert!(
        sample(
            &first,
            "ferrofin_library_scan_last_completed_timestamp_seconds",
            &[schedule]
        )
        .is_some_and(|t| t > 1.7e9),
        "{first}"
    );
    assert!(
        sample(
            &first,
            "ferrofin_library_scan_last_duration_seconds",
            &[schedule]
        )
        .is_some()
    );
    for pass in ferrofin_core::scan_metrics::SCAN_PASSES {
        assert_eq!(
            sample(
                &first,
                "ferrofin_library_scan_pass_duration_seconds_count",
                &[("pass", pass)]
            ),
            Some(1.0),
            "{pass}:\n{first}"
        );
    }
    let probes = |text: &str, result: &str| {
        sample(text, "ferrofin_media_probe_total", &[("result", result)])
            .unwrap_or_else(|| panic!("no {result} probe series:\n{text}"))
    };
    assert_eq!(probes(&first, "ok"), 1.0, "only the new movie was probed");
    assert_eq!(probes(&first, "failed"), 0.0);
    assert_eq!(
        probe_calls(&calls),
        2,
        "the probe counter matches the real ffprobe spawns since enabling"
    );
    assert_eq!(
        sample(&first, "ferrofin_library_scan_in_progress", &[]),
        Some(0.0)
    );

    // ---- unchanged rescan: nothing created or updated, no probe ----
    assert!(
        wired
            .state
            .library
            .run_library_scan(ScanTrigger::Schedule, None)
            .await
            .expect("rescan")
    );
    let second = scrape(&router).await;
    assert_eq!(
        sample(
            &second,
            "ferrofin_library_scans_total",
            &[schedule, ("result", "completed")]
        ),
        Some(2.0)
    );
    assert_eq!(items(&second, "created"), created, "nothing new created");
    assert_eq!(items(&second, "updated"), items(&first, "updated"));
    assert!(
        items(&second, "unchanged") >= items(&first, "unchanged") + 2.0,
        "both movies are unchanged:\n{second}"
    );
    assert_eq!(
        probes(&second, "ok"),
        1.0,
        "an unchanged rescan never probes"
    );
    assert_eq!(probe_calls(&calls), 2);

    // ---- a webhook's report: one webhook pass creating the new movie ----
    let mut server = (*wired.state.config.configuration().await.expect("config")).clone();
    server.library_monitor_delay = 0;
    wired
        .state
        .config
        .update_configuration(&server)
        .await
        .expect("no debounce");
    let added = write_movie(&media, "Film Gamma (2011)");
    wired
        .state
        .library_monitor
        .report_webhook_change(&added.to_string_lossy())
        .await
        .expect("reported");
    let webhook = ("trigger", "webhook");
    let mut third = String::new();
    for _ in 0..500 {
        third = scrape(&router).await;
        if sample(
            &third,
            "ferrofin_library_scans_total",
            &[webhook, ("result", "completed")],
        ) == Some(1.0)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        sample(
            &third,
            "ferrofin_library_scans_total",
            &[webhook, ("result", "completed")]
        ),
        Some(1.0),
        "the webhook's scan ran as trigger=webhook:\n{third}"
    );
    assert_eq!(
        sample(
            &third,
            "ferrofin_library_scan_items_total",
            &[webhook, ("outcome", "created")]
        ),
        Some(1.0),
        "{third}"
    );
    assert_eq!(
        sample(
            &third,
            "ferrofin_library_scans_total",
            &[("trigger", "watcher"), ("result", "completed")]
        ),
        Some(0.0),
        "nothing ran as the watcher"
    );
    assert_eq!(probes(&third, "ok"), 2.0, "only the new movie was probed");
    assert_eq!(probe_calls(&calls), 3);
    // A webhook refresh runs only the touched items' closing passes.
    assert_eq!(
        sample(
            &third,
            "ferrofin_library_scan_pass_duration_seconds_count",
            &[("pass", "music")]
        ),
        Some(2.0)
    );
}
