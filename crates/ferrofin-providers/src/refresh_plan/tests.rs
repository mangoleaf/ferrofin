//! The refresh decision, table by table: the "Target behaviour" rows of
//! `PLAN_SCAN_CHANGE_DETECTION`, the change monitors' edges, and the
//! `MetadataServiceRefreshTests` cases (`tests/Jellyfin.Providers.Tests/
//! Manager/MetadataServiceRefreshTests.cs`) that exercise the decision rather
//! than the merge — their C# expected values are the oracle. Its
//! `RefreshWithProviders_*` cases exercise the merge and sit beside it
//! (`metadata_merge/tests.rs`, and `ferrofin-core`'s `library_scan/music.rs`
//! for the lookup-info one).

use chrono::{DateTime, TimeDelta, TimeZone as _, Utc};
use ferrofin_model::configuration::LibraryOptions;
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use rstest::rstest;

use super::{
    FileFacts, ImageFetch, ItemRefreshPlan, LocalMetadataFile, LocalMetadataFormat, PassOutcome,
    ProbeKind, RefreshReason, RefreshRequest, SaveDecision, StoredState, decide_save, file_changed,
    plan_item_refresh,
};

use MetadataRefreshMode::{Default, FullRefresh, None as NoRefresh, ValidationOnly};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 24, 12, 0, 0).unwrap()
}

/// The file's mtime on disk.
fn mtime() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 1, 8, 30, 0).unwrap()
}

/// An item a previous scan fully refreshed and saved yesterday.
fn current() -> StoredState {
    StoredState {
        date_last_refreshed: Some(now() - TimeDelta::days(1)),
        date_last_saved: Some(now() - TimeDelta::days(1)),
        date_modified: Some(mtime()),
        run_time_ticks: Some(72_000_000_000),
        total_bitrate: Some(8_000_000),
        is_virtual_item: false,
        is_locked: false,
    }
}

/// A plain video file, unchanged on disk.
fn video() -> FileFacts {
    FileFacts {
        mtime: Some(mtime()),
        probe: ProbeKind::Video { file_or_iso: true },
        sidecars_changed: false,
        lyrics_changed: false,
        local_metadata: None,
        supports_cumulative_run_time: false,
        is_shortcut: false,
        is_file_protocol: true,
        is_placeholder: false,
    }
}

fn options(meta: MetadataRefreshMode, image: MetadataRefreshMode) -> MetadataRefreshOptions {
    MetadataRefreshOptions {
        metadata_refresh_mode: meta,
        image_refresh_mode: image,
        ..MetadataRefreshOptions::default()
    }
}

fn plan(
    stored: Option<&StoredState>,
    fs: &FileFacts,
    opts: &MetadataRefreshOptions,
) -> ItemRefreshPlan {
    plan_with(stored, fs, opts, None, false)
}

fn plan_with(
    stored: Option<&StoredState>,
    fs: &FileFacts,
    opts: &MetadataRefreshOptions,
    library: Option<&LibraryOptions>,
    backfill: bool,
) -> ItemRefreshPlan {
    let request = RefreshRequest {
        options: opts,
        force_save: false,
    };
    plan_item_refresh(stored, fs, &request, library, now(), backfill)
}

fn save(plan: ItemRefreshPlan, opts: &MetadataRefreshOptions, changed: bool) -> SaveDecision {
    decide_save(
        plan,
        &RefreshRequest {
            options: opts,
            force_save: false,
        },
        PassOutcome {
            changed,
            failed: false,
        },
    )
}

/// `(probe, local, remote, remote images)` of a plan.
fn runs(plan: ItemRefreshPlan) -> (bool, bool, bool, ImageFetch) {
    (
        plan.probe,
        plan.local_metadata,
        plan.remote_metadata,
        plan.remote_images,
    )
}

// --- The "Target behaviour" table, row by row (Default/Default scan) ---

#[test]
fn a_new_item_runs_everything_and_is_saved() {
    let opts = MetadataRefreshOptions::default();
    let p = plan(None, &video(), &opts);
    assert!(p.is_first_refresh);
    assert_eq!(runs(p), (true, true, true, ImageFetch::MissingOnly));
    assert!(save(p, &opts, false).save);
}

#[test]
fn a_never_refreshed_item_runs_everything_once_and_is_saved() {
    let opts = MetadataRefreshOptions::default();
    let stored = StoredState {
        date_last_refreshed: None,
        ..current()
    };
    let p = plan(Some(&stored), &video(), &opts);
    assert!(p.is_first_refresh);
    assert!(p.run_all_providers);
    assert_eq!(runs(p), (true, true, true, ImageFetch::MissingOnly));
    assert!(save(p, &opts, false).save, "the stamp is saved");
}

#[rstest]
#[case::drift_2s(2_000, true)]
#[case::drift_back_2s(-2_000, true)]
#[case::drift_exactly_1s(1_000, false)]
#[case::drift_half_a_second(500, false)]
fn file_mtime_drift_over_one_second_requires_a_refresh(
    #[case] drift_ms: i64,
    #[case] requires: bool,
) {
    let opts = MetadataRefreshOptions::default();
    let fs = FileFacts {
        mtime: Some(mtime() + TimeDelta::milliseconds(drift_ms)),
        ..video()
    };
    let p = plan(Some(&current()), &fs, &opts);
    assert_eq!(p.requires_refresh, requires);
    if requires {
        // Every provider, but no remote images: the item was refreshed before.
        assert_eq!(runs(p), (true, true, true, ImageFetch::None));
        assert!(save(p, &opts, false).save);
    } else {
        assert_eq!(runs(p), (false, false, false, ImageFetch::None));
        assert!(!save(p, &opts, false).save);
    }
}

#[rstest]
#[case::elapsed(30, 31, true)]
#[case::exactly_elapsed(30, 30, true)]
#[case::not_elapsed(30, 29, false)]
#[case::disabled(0, 400, false)]
fn automatic_refresh_interval_elapsed_requires_a_refresh(
    #[case] interval_days: i32,
    #[case] refreshed_days_ago: i64,
    #[case] requires: bool,
) {
    let opts = MetadataRefreshOptions::default();
    let library = LibraryOptions {
        automatic_refresh_interval_days: interval_days,
        ..LibraryOptions::default()
    };
    let stored = StoredState {
        date_last_refreshed: Some(now() - TimeDelta::days(refreshed_days_ago)),
        ..current()
    };
    let p = plan_with(Some(&stored), &video(), &opts, Some(&library), false);
    assert_eq!(p.requires_refresh, requires);
    assert_eq!(p.remote_metadata, requires);
    assert_eq!(p.probe, requires, "ProbeProvider runs with every provider");
    assert_eq!(p.remote_images, ImageFetch::None);
    assert_eq!(save(p, &opts, false).save, requires);
}

#[test]
fn an_elapsed_interval_requires_a_refresh_even_in_none_mode() {
    // The interval arm of `requiresRefresh` is evaluated before, and
    // regardless of, the metadata mode; it still forces the save.
    let opts = options(NoRefresh, NoRefresh);
    let library = LibraryOptions {
        automatic_refresh_interval_days: 7,
        ..LibraryOptions::default()
    };
    let stored = StoredState {
        date_last_refreshed: Some(now() - TimeDelta::days(8)),
        ..current()
    };
    let p = plan_with(Some(&stored), &video(), &opts, Some(&library), false);
    assert!(p.requires_refresh);
    assert_eq!(runs(p), (false, false, false, ImageFetch::None));
    let decision = save(p, &opts, false);
    assert!(decision.save);
    assert!(!decision.stamp_refreshed, "no fetch was attempted");
}

#[test]
fn an_unchanged_item_runs_nothing_and_is_not_saved() {
    let opts = MetadataRefreshOptions::default();
    let p = plan(Some(&current()), &video(), &opts);
    assert!(!p.is_first_refresh);
    assert!(!p.requires_refresh);
    assert_eq!(runs(p), (false, false, false, ImageFetch::None));
    let decision = save(p, &opts, false);
    assert!(!decision.save);
    // Stamped in memory like upstream, but never persisted on its own.
    assert!(decision.stamp_refreshed);
}

#[test]
fn an_unchanged_item_is_saved_when_the_pass_changed_something() {
    let opts = MetadataRefreshOptions::default();
    let p = plan(Some(&current()), &video(), &opts);
    assert!(save(p, &opts, true).save);
}

#[rstest]
#[case::no_runtime_no_bitrate(None, None, false, true)]
#[case::runtime_only(Some(1), None, false, false)]
#[case::bitrate_only(None, Some(1), false, false)]
#[case::virtual_item(None, None, true, false)]
fn missing_media_info_reprobes(
    #[case] run_time_ticks: Option<i64>,
    #[case] total_bitrate: Option<i64>,
    #[case] is_virtual_item: bool,
    #[case] probes: bool,
) {
    let opts = MetadataRefreshOptions::default();
    let stored = StoredState {
        run_time_ticks,
        total_bitrate,
        is_virtual_item,
        ..current()
    };
    let p = plan(Some(&stored), &video(), &opts);
    assert_eq!(p.probe, probes);
    // A probe is a pre-refresh provider: the local readers run with it.
    assert_eq!(p.local_metadata, probes);
    assert!(
        !p.remote_metadata,
        "no remote provider has a change monitor"
    );
    assert!(!p.requires_refresh);
}

/// `IsMissingMediaInfo`'s exclusions: a `.strm` shortcut, a stream URL and
/// a disc placeholder have no media info to find on disk, so a missing
/// runtime does not re-probe them on every scan.
#[rstest]
#[case::shortcut(true, true, false, false)]
#[case::not_a_file(false, false, false, false)]
#[case::placeholder(false, true, true, false)]
#[case::plain_file(false, true, false, true)]
fn missing_media_info_excludes_what_has_none_on_disk(
    #[case] is_shortcut: bool,
    #[case] is_file_protocol: bool,
    #[case] is_placeholder: bool,
    #[case] probes: bool,
) {
    let opts = MetadataRefreshOptions::default();
    let stored = StoredState {
        run_time_ticks: None,
        total_bitrate: None,
        ..current()
    };
    let fs = FileFacts {
        is_shortcut,
        is_file_protocol,
        is_placeholder,
        ..video()
    };
    assert_eq!(plan(Some(&stored), &fs, &opts).probe, probes);
}

#[test]
fn a_placeholder_audio_item_is_still_missing_media_info() {
    // `IsPlaceHolder` is a `Video` property; audio has no such exclusion.
    let opts = MetadataRefreshOptions::default();
    let stored = StoredState {
        run_time_ticks: None,
        total_bitrate: None,
        ..current()
    };
    let fs = FileFacts {
        probe: ProbeKind::Audio,
        is_placeholder: true,
        ..video()
    };
    assert!(plan(Some(&stored), &fs, &opts).probe);
}

#[test]
fn a_changed_sidecar_set_reprobes_without_remote_providers() {
    let opts = MetadataRefreshOptions::default();
    let fs = FileFacts {
        sidecars_changed: true,
        ..video()
    };
    let p = plan(Some(&current()), &fs, &opts);
    assert_eq!(runs(p), (true, true, false, ImageFetch::None));
    assert!(!p.requires_refresh);
}

#[test]
fn a_changed_lyric_set_reprobes_audio_only() {
    let opts = MetadataRefreshOptions::default();
    let audio = FileFacts {
        probe: ProbeKind::Audio,
        lyrics_changed: true,
        ..video()
    };
    assert!(plan(Some(&current()), &audio, &opts).probe);
    let video_with_lyric_flag = FileFacts {
        lyrics_changed: true,
        ..video()
    };
    assert!(!plan(Some(&current()), &video_with_lyric_flag, &opts).probe);
}

/// The external-file arms' gates (`ProbeProvider.cs:151-179`): a changed
/// sidecar set re-probes a video only when it `SupportsLocalMetadata` (a
/// file path) and is no disc placeholder (`!video.IsPlaceHolder`); a changed
/// lyric set re-probes audio only when it `SupportsLocalMetadata` —
/// `IsPlaceHolder` is a `Video` property.
#[rstest]
#[case::video_file(ProbeKind::Video { file_or_iso: true }, true, false, true)]
#[case::video_placeholder(ProbeKind::Video { file_or_iso: true }, true, true, false)]
#[case::video_not_a_file(ProbeKind::Video { file_or_iso: true }, false, false, false)]
#[case::audio_file(ProbeKind::Audio, true, false, true)]
#[case::audio_placeholder_flag(ProbeKind::Audio, true, true, true)]
#[case::audio_not_a_file(ProbeKind::Audio, false, false, false)]
fn the_external_file_arms_need_local_metadata_support(
    #[case] probe: ProbeKind,
    #[case] is_file_protocol: bool,
    #[case] is_placeholder: bool,
    #[case] probes: bool,
) {
    let opts = MetadataRefreshOptions::default();
    let fs = FileFacts {
        probe,
        is_file_protocol,
        is_placeholder,
        sidecars_changed: true,
        lyrics_changed: true,
        ..video()
    };
    let p = plan(Some(&current()), &fs, &opts);
    assert_eq!(p.probe, probes);
    let reason = match probe {
        ProbeKind::Audio => RefreshReason::Lyrics,
        _ => RefreshReason::Sidecars,
    };
    assert_eq!(
        p.reason,
        if probes {
            reason
        } else {
            RefreshReason::Unchanged
        }
    );
}

#[test]
fn a_disc_folder_does_not_compare_its_mtime_for_the_probe() {
    // `ProbeProvider.HasChanged` checks the mtime only for a VideoFile/Iso.
    // With no stored DateModified, `RequiresRefresh` stays false (it guards
    // `DateTime.MinValue`) while the probe's `HasChanged` would not — so a
    // file re-probes and a disc folder does not.
    let opts = MetadataRefreshOptions::default();
    let stored = StoredState {
        date_modified: None,
        ..current()
    };
    let file = plan(Some(&stored), &video(), &opts);
    assert!(!file.requires_refresh);
    assert!(file.probe);
    let disc = FileFacts {
        probe: ProbeKind::Video { file_or_iso: false },
        ..video()
    };
    let disc = plan(Some(&stored), &disc, &opts);
    assert!(!disc.requires_refresh);
    assert!(!disc.probe);
}

#[rstest]
#[case::nfo_61s_newer(LocalMetadataFormat::Nfo, 61_000, true)]
#[case::nfo_59s_newer(LocalMetadataFormat::Nfo, 59_000, false)]
#[case::nfo_older(LocalMetadataFormat::Nfo, -3_600_000, false)]
#[case::xml_1s_newer(LocalMetadataFormat::Xml, 1_000, true)]
#[case::xml_same_time(LocalMetadataFormat::Xml, 0, false)]
fn a_local_sidecar_newer_than_the_last_save_rereads_local_metadata_only(
    #[case] format: LocalMetadataFormat,
    #[case] newer_ms: i64,
    #[case] rereads: bool,
) {
    let opts = MetadataRefreshOptions::default();
    let stored = current();
    let saved = stored.date_last_saved.unwrap();
    let fs = FileFacts {
        local_metadata: Some(LocalMetadataFile {
            mtime: saved + TimeDelta::milliseconds(newer_ms),
            format,
        }),
        ..video()
    };
    let p = plan(Some(&stored), &fs, &opts);
    assert_eq!(runs(p), (false, rereads, false, ImageFetch::None));
}

#[test]
fn an_nfo_with_no_last_save_is_always_newer() {
    let opts = MetadataRefreshOptions::default();
    let stored = StoredState {
        date_last_saved: None,
        ..current()
    };
    let fs = FileFacts {
        local_metadata: Some(LocalMetadataFile {
            mtime: mtime(),
            format: LocalMetadataFormat::Nfo,
        }),
        ..video()
    };
    assert!(plan(Some(&stored), &fs, &opts).local_metadata);
}

#[test]
fn a_cumulative_runtime_folder_without_runtime_requires_a_refresh() {
    let opts = MetadataRefreshOptions::default();
    let album = FileFacts {
        probe: ProbeKind::None,
        supports_cumulative_run_time: true,
        ..video()
    };
    let without = StoredState {
        run_time_ticks: None,
        ..current()
    };
    let p = plan(Some(&without), &album, &opts);
    assert!(p.requires_refresh);
    assert!(!p.probe, "a folder is never probed");
    assert!(
        plan(Some(&current()), &album, &opts)
            .requires_refresh
            .eq(&false)
    );
}

#[rstest]
#[case::full_refresh(false, ImageFetch::MissingOnly)]
#[case::full_refresh_replacing_images(true, ImageFetch::All)]
fn a_full_refresh_runs_every_provider(#[case] replace_images: bool, #[case] images: ImageFetch) {
    let opts = MetadataRefreshOptions {
        replace_all_images: replace_images,
        ..options(FullRefresh, FullRefresh)
    };
    let p = plan(Some(&current()), &video(), &opts);
    assert!(p.run_all_providers);
    assert_eq!(runs(p), (true, true, true, images));
    // Saved even when nothing changed: the stamp needs saving.
    assert!(save(p, &opts, false).save);
}

#[test]
fn replace_all_metadata_runs_every_provider_and_saves() {
    let opts = MetadataRefreshOptions {
        replace_all_metadata: true,
        ..options(FullRefresh, FullRefresh)
    };
    let p = plan(Some(&current()), &video(), &opts);
    assert_eq!(runs(p), (true, true, true, ImageFetch::MissingOnly));
    assert!(save(p, &opts, false).save);
}

#[test]
fn replace_all_metadata_runs_every_provider_even_in_validation_only_mode() {
    // `runAllProviders` reads `ReplaceAllMetadata` before the mode.
    let opts = MetadataRefreshOptions {
        replace_all_metadata: true,
        ..options(ValidationOnly, ValidationOnly)
    };
    let p = plan(Some(&current()), &video(), &opts);
    assert_eq!(runs(p), (true, true, true, ImageFetch::None));
    let decision = save(p, &opts, false);
    assert!(decision.save);
    assert!(!decision.stamp_refreshed);
}

// --- Locked, ValidationOnly, None ---

#[rstest]
#[case::default_mode(Default, ImageFetch::None)]
#[case::image_full_refresh(FullRefresh, ImageFetch::MissingOnly)]
fn a_locked_item_never_runs_remote_metadata(
    #[case] image_mode: MetadataRefreshMode,
    #[case] images: ImageFetch,
) {
    let opts = options(FullRefresh, image_mode);
    let stored = StoredState {
        is_locked: true,
        ..current()
    };
    let p = plan_with(Some(&stored), &video(), &opts, None, true);
    assert!(!p.remote_metadata);
    assert!(!p.run_all_providers);
    // The (forced, pre-refresh) probe still runs; the local readers do not:
    // `RefreshWithProviders` returns on `IsLocked` before them.
    assert!(p.probe);
    assert!(!p.local_metadata);
    assert_eq!(p.remote_images, images);
}

/// A locked item whose NFO changed: its change monitor does not bring the
/// reader back (`MetadataService.cs:785-788` returns before any local
/// provider runs), and nothing else runs either.
#[test]
fn a_locked_item_never_reads_a_changed_nfo() {
    let opts = MetadataRefreshOptions::default();
    let stored = StoredState {
        is_locked: true,
        ..current()
    };
    let fs = FileFacts {
        local_metadata: Some(LocalMetadataFile {
            mtime: now(),
            format: LocalMetadataFormat::Nfo,
        }),
        ..video()
    };
    let p = plan(Some(&stored), &fs, &opts);
    assert_eq!(runs(p), (false, false, false, ImageFetch::None));
    assert!(!p.local_monitor_fired);
    assert!(!save(p, &opts, false).save);
}

/// `ProviderManagerTests.GetMetadataProviders_CanRefreshMetadataLocked_
/// WhenLocalOrForced` and `GetImageProviders_CanRefreshImagesLocked_
/// WhenLocalOrFullRefresh`, as far as the decision models providers: on a
/// locked item the remote metadata provider is refused, the forced probe is
/// not, and the remote image providers run only on an image full refresh.
#[rstest]
#[case::remote_image_default(Default, ImageFetch::None)]
#[case::remote_image_full_refresh(FullRefresh, ImageFetch::MissingOnly)]
fn provider_manager_locked_gates(
    #[case] image_mode: MetadataRefreshMode,
    #[case] images: ImageFetch,
) {
    let opts = options(Default, image_mode);
    let stored = StoredState {
        date_last_refreshed: None,
        is_locked: true,
        ..current()
    };
    let p = plan(Some(&stored), &video(), &opts);
    // `IRemoteMetadataProvider`, not forced: refused.
    assert!(!p.remote_metadata);
    // `ProbeProvider` is an `IForcedProvider`: allowed.
    assert!(p.probe);
    assert_eq!(p.remote_images, images);
}

/// A locked item keeps its local and forced providers, so a pass that runs
/// every provider — or one whose local change monitor fired — has
/// providers for `BeforeMetadataRefresh` to run ahead of, though none of
/// them reads anything for it. An unlocked item's providers are counted
/// where they run.
#[test]
fn a_locked_items_local_providers_count_for_the_refill() {
    let stored = StoredState {
        is_locked: true,
        ..current()
    };
    let folder = FileFacts {
        probe: ProbeKind::None,
        ..video()
    };
    let default = MetadataRefreshOptions::default();
    assert!(plan(Some(&stored), &folder, &options(FullRefresh, Default)).locked_local);
    assert!(!plan(Some(&stored), &folder, &default).locked_local);
    let nfo = FileFacts {
        local_metadata: Some(LocalMetadataFile {
            mtime: now(),
            format: LocalMetadataFormat::Nfo,
        }),
        ..folder
    };
    assert!(plan(Some(&stored), &nfo, &default).locked_local);
    assert!(!plan(Some(&stored), &folder, &options(NoRefresh, NoRefresh)).locked_local);
    assert!(!plan(Some(&current()), &folder, &options(FullRefresh, Default)).locked_local);
}

#[test]
fn a_new_locked_item_still_probes() {
    let opts = MetadataRefreshOptions::default();
    let stored = StoredState {
        date_last_refreshed: None,
        is_locked: true,
        ..current()
    };
    let p = plan(Some(&stored), &video(), &opts);
    assert_eq!(runs(p), (true, false, false, ImageFetch::None));
    assert!(save(p, &opts, false).save);
}

#[test]
fn validation_only_runs_changed_local_providers_but_nothing_remote() {
    let opts = options(ValidationOnly, ValidationOnly);
    let never = StoredState {
        date_last_refreshed: None,
        ..current()
    };
    // A first refresh runs everything only from `Default` up.
    let p = plan_with(Some(&never), &video(), &opts, None, true);
    assert_eq!(runs(p), (false, false, false, ImageFetch::None));
    // A changed sidecar set still re-probes.
    let fs = FileFacts {
        sidecars_changed: true,
        ..video()
    };
    let p = plan_with(Some(&current()), &fs, &opts, None, true);
    assert_eq!(runs(p), (true, true, false, ImageFetch::None));
    assert!(!save(p, &opts, false).stamp_refreshed);
}

#[test]
fn none_mode_runs_no_provider() {
    let opts = options(NoRefresh, NoRefresh);
    let fs = FileFacts {
        mtime: Some(mtime() + TimeDelta::hours(1)),
        sidecars_changed: true,
        ..video()
    };
    let p = plan_with(Some(&current()), &fs, &opts, None, true);
    assert!(
        !p.requires_refresh,
        "RequiresRefresh is skipped in None mode"
    );
    assert_eq!(runs(p), (false, false, false, ImageFetch::None));
    let decision = save(p, &opts, false);
    assert!(!decision.save);
    assert!(!decision.stamp_refreshed);
}

// --- The scan's backfill trigger (owner decision D2) ---

#[rstest]
#[case::default_unlocked(Default, false, true)]
#[case::default_locked(Default, true, false)]
#[case::validation_only(ValidationOnly, false, false)]
#[case::none(NoRefresh, false, false)]
fn backfill_only_adds_remote_metadata(
    #[case] mode: MetadataRefreshMode,
    #[case] is_locked: bool,
    #[case] remote: bool,
) {
    let opts = options(mode, Default);
    let stored = StoredState {
        is_locked,
        ..current()
    };
    let p = plan_with(Some(&stored), &video(), &opts, None, true);
    assert_eq!(p.remote_metadata, remote);
    // A remote provider "reporting a change" runs every local reader too
    // (`MetadataService.cs:689-693`), so an NFO's values survive the merge;
    // a locked item runs neither.
    assert_eq!(p.local_metadata, remote);
    assert!(!p.run_all_providers, "backfill is not upstream's run-all");
    assert!(!p.probe, "the backfill asks nothing of the file");
    assert_eq!(p.remote_images, ImageFetch::None);
    assert!(!p.requires_refresh);
}

// --- Provider failed vs returned empty (owner decision D1) ---

#[rstest]
#[case::found_nothing(false, true)]
#[case::provider_failed(true, false)]
fn a_failed_provider_leaves_the_stamp_but_an_empty_one_stamps(
    #[case] failed: bool,
    #[case] stamps: bool,
) {
    let opts = MetadataRefreshOptions::default();
    let p = plan(None, &video(), &opts);
    let decision = decide_save(
        p,
        &RefreshRequest {
            options: &opts,
            force_save: false,
        },
        PassOutcome {
            changed: false,
            failed,
        },
    );
    assert!(decision.save, "a first refresh is saved either way");
    assert_eq!(decision.stamp_refreshed, stamps);
}

#[test]
fn force_save_saves_an_unchanged_item() {
    let opts = MetadataRefreshOptions::default();
    let p = plan(Some(&current()), &video(), &opts);
    let decision = decide_save(
        p,
        &RefreshRequest {
            options: &opts,
            force_save: true,
        },
        PassOutcome::default(),
    );
    assert!(decision.save);
}

// --- MetadataServiceRefreshTests.cs, transliterated ---

/// `NewStampedTestItem`: refreshed and saved 60 days ago; `TestItem`
/// overrides `RequiresRefresh` to false, which a path-less item with no
/// stored `DateModified` gives here too.
fn stamped_test_item() -> StoredState {
    StoredState {
        date_last_refreshed: Some(now() - TimeDelta::days(60)),
        date_last_saved: Some(now() - TimeDelta::days(60)),
        date_modified: None,
        run_time_ticks: None,
        total_bitrate: None,
        is_virtual_item: false,
        is_locked: false,
    }
}

/// `TestItem` is a plain `BaseItem`: no path, no prober.
fn test_item_fs() -> FileFacts {
    FileFacts {
        mtime: None,
        probe: ProbeKind::None,
        sidecars_changed: false,
        lyrics_changed: false,
        local_metadata: None,
        supports_cumulative_run_time: false,
        is_shortcut: false,
        is_file_protocol: true,
        is_placeholder: false,
    }
}

/// `RefreshMetadata_ProvidersFoundNothing_PersistsRefreshDateOnFullRefresh`:
/// `[InlineData(FullRefresh, true)]`, `[InlineData(Default, false)]`.
#[rstest]
#[case(FullRefresh, true)]
#[case(Default, false)]
fn refresh_metadata_providers_found_nothing_persists_refresh_date_on_full_refresh(
    #[case] mode: MetadataRefreshMode,
    #[case] expect_saved: bool,
) {
    let opts = options(mode, mode);
    let stored = stamped_test_item();
    let p = plan(Some(&stored), &test_item_fs(), &opts);
    // The one remote provider has no change monitor: it runs only on the
    // full refresh.
    assert_eq!(p.remote_metadata, mode == FullRefresh);
    // It returned `HasMetadata = false`: nothing changed, nothing failed.
    let decision = save(p, &opts, false);
    assert_eq!(decision.save, expect_saved);
    if expect_saved {
        assert!(decision.stamp_refreshed, "the advanced stamp is saved");
    }
}

/// `RefreshMetadata_CustomProviderThrew_LeavesRefreshDateAlone`
/// (`[InlineData(true)]`, `[InlineData(false)]`), full refresh: a throwing
/// custom provider (the probe's stand-in) leaves `DateLastRefreshed`.
#[rstest]
#[case(true)]
#[case(false)]
fn refresh_metadata_custom_provider_threw_leaves_refresh_date_alone(#[case] provider_throws: bool) {
    let opts = options(FullRefresh, FullRefresh);
    let p = plan(Some(&stamped_test_item()), &test_item_fs(), &opts);
    let decision = decide_save(
        p,
        &RefreshRequest {
            options: &opts,
            force_save: false,
        },
        PassOutcome {
            changed: false,
            failed: provider_throws,
        },
    );
    // `Assert.Equal(providerThrows, item.DateLastRefreshed == stampBefore)`.
    assert_eq!(provider_throws, !decision.stamp_refreshed);
}

/// `RefreshMetadata_ImageProviderThrew_LeavesRefreshDateAlone`
/// (`[InlineData(true)]`, `[InlineData(false)]`): the remote image stage
/// runs on an image full refresh, and its failure leaves the stamp.
#[rstest]
#[case(true)]
#[case(false)]
fn refresh_metadata_image_provider_threw_leaves_refresh_date_alone(#[case] provider_throws: bool) {
    let opts = options(FullRefresh, FullRefresh);
    let p = plan(Some(&stamped_test_item()), &test_item_fs(), &opts);
    assert_eq!(p.remote_images, ImageFetch::MissingOnly);
    let decision = decide_save(
        p,
        &RefreshRequest {
            options: &opts,
            force_save: false,
        },
        PassOutcome {
            changed: false,
            failed: provider_throws,
        },
    );
    assert_eq!(provider_throws, !decision.stamp_refreshed);
}

/// `RefreshMetadata_LocalImageValidationThrew_LeavesRefreshDateAlone`.
#[test]
fn refresh_metadata_local_image_validation_threw_leaves_refresh_date_alone() {
    let opts = options(FullRefresh, FullRefresh);
    let p = plan(Some(&stamped_test_item()), &test_item_fs(), &opts);
    let decision = decide_save(
        p,
        &RefreshRequest {
            options: &opts,
            force_save: false,
        },
        PassOutcome {
            changed: false,
            failed: true,
        },
    );
    assert!(!decision.stamp_refreshed);
}

// --- The reason a path-scoped scan logs for each item ---

/// The first trigger that fired, one case per trigger, in the decision's
/// ranking: no stored row, the options, the first and the required
/// refresh, then the change monitors, then the backfill rule.
#[rstest]
#[case::new_item(
    None,
    video(),
    MetadataRefreshOptions::default(),
    false,
    RefreshReason::New
)]
#[case::full_refresh(
    Some(current()),
    video(),
    options(FullRefresh, FullRefresh),
    false,
    RefreshReason::Requested
)]
#[case::never_refreshed(
    Some(StoredState { date_last_refreshed: None, ..current() }),
    video(),
    MetadataRefreshOptions::default(),
    false,
    RefreshReason::FirstRefresh
)]
#[case::moved_mtime(
    Some(current()),
    FileFacts { mtime: Some(mtime() + TimeDelta::seconds(5)), ..video() },
    MetadataRefreshOptions::default(),
    false,
    RefreshReason::Modified
)]
#[case::folder_without_runtime(
    Some(StoredState { run_time_ticks: None, ..current() }),
    FileFacts { probe: ProbeKind::None, supports_cumulative_run_time: true, ..video() },
    MetadataRefreshOptions::default(),
    false,
    RefreshReason::NoRuntime
)]
#[case::newer_nfo(
    Some(current()),
    FileFacts {
        local_metadata: Some(LocalMetadataFile {
            mtime: now(),
            format: LocalMetadataFormat::Nfo,
        }),
        ..video()
    },
    MetadataRefreshOptions::default(),
    false,
    RefreshReason::LocalMetadata
)]
#[case::changed_sidecars(
    Some(current()),
    FileFacts { sidecars_changed: true, ..video() },
    MetadataRefreshOptions::default(),
    false,
    RefreshReason::Sidecars
)]
#[case::missing_media_info(
    Some(StoredState { run_time_ticks: None, total_bitrate: None, ..current() }),
    video(),
    MetadataRefreshOptions::default(),
    false,
    RefreshReason::MissingMediaInfo
)]
#[case::backfill(
    Some(current()),
    video(),
    MetadataRefreshOptions::default(),
    true,
    RefreshReason::Backfill
)]
#[case::unchanged(
    Some(current()),
    video(),
    MetadataRefreshOptions::default(),
    false,
    RefreshReason::Unchanged
)]
#[case::context_only(
    Some(current()),
    FileFacts { mtime: Some(mtime() + TimeDelta::seconds(5)), ..video() },
    options(NoRefresh, NoRefresh),
    false,
    RefreshReason::Unchanged
)]
fn the_reason_names_the_first_trigger_that_fired(
    #[case] stored: Option<StoredState>,
    #[case] fs: FileFacts,
    #[case] opts: MetadataRefreshOptions,
    #[case] backfill: bool,
    #[case] reason: RefreshReason,
) {
    let p = plan_with(stored.as_ref(), &fs, &opts, None, backfill);
    assert_eq!(p.reason, reason);
}

#[test]
fn an_elapsed_interval_is_the_reason_even_in_none_mode() {
    let library = LibraryOptions {
        automatic_refresh_interval_days: 7,
        ..LibraryOptions::default()
    };
    let stored = StoredState {
        date_last_refreshed: Some(now() - TimeDelta::days(8)),
        ..current()
    };
    let p = plan_with(
        Some(&stored),
        &video(),
        &options(NoRefresh, NoRefresh),
        Some(&library),
        false,
    );
    assert_eq!(p.reason, RefreshReason::Interval);
    assert_eq!(p.reason.as_str(), "interval");
}

#[test]
fn every_reason_has_a_distinct_log_name() {
    let all = [
        RefreshReason::New,
        RefreshReason::Requested,
        RefreshReason::FirstRefresh,
        RefreshReason::Interval,
        RefreshReason::Modified,
        RefreshReason::NoRuntime,
        RefreshReason::LocalMetadata,
        RefreshReason::Sidecars,
        RefreshReason::Lyrics,
        RefreshReason::MissingMediaInfo,
        RefreshReason::Backfill,
        RefreshReason::Unchanged,
    ];
    let names: std::collections::HashSet<&str> = all.iter().map(|r| r.as_str()).collect();
    assert_eq!(names.len(), all.len());
    assert_eq!(RefreshReason::New.as_str(), "created");
    assert_eq!(RefreshReason::Modified.as_str(), "mtime");
    assert_eq!(RefreshReason::LocalMetadata.as_str(), "nfo");
    assert_eq!(RefreshReason::Sidecars.as_str(), "sidecar");
    assert_eq!(ItemRefreshPlan::IDLE.reason, RefreshReason::Unchanged);
}

/// `BaseItemExtensions.HasChanged`: more than a second either way, and an
/// unset stored date (`DateTime.MinValue`) always.
#[rstest]
#[case::same(Some(0), false)]
#[case::within_a_second(Some(1_000), false)]
#[case::past_a_second(Some(1_001), true)]
#[case::earlier(Some(-1_001), true)]
#[case::never_saved(None, true)]
fn a_file_changed_when_its_mtime_drifted_past_a_second(
    #[case] stored_offset_ms: Option<i64>,
    #[case] changed: bool,
) {
    let stored = stored_offset_ms.map(|ms| mtime() + TimeDelta::milliseconds(ms));
    assert_eq!(file_changed(stored, mtime()), changed);
}
