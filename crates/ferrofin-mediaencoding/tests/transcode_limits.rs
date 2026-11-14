//! The admin caps on concurrent re-encoding jobs, through
//! [`TranscodeManagerImpl::start_ffmpeg`] with a fake ffmpeg.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_mediaencoding::transcoding::manager::StartFfMpegRequest;
use ferrofin_mediaencoding::transcoding::{
    FakeScript, FakeSegmentTranscoder, SegmentTranscoder, SessionReporter, SpawnRequest,
    StartFfmpegError, TranscodeChild, TranscodeLimits, TranscodeRefused,
};
use ferrofin_mediaencoding::{
    BaseEncodingJobOptions, EncodingJobInfo, TranscodeDisplayNames, TranscodeManagerImpl,
};
use ferrofin_model::dlna::SubtitleDeliveryMethod;
use ferrofin_model::dto::MediaSourceInfo;
use ferrofin_model::entities::HardwareAccelerationType;
use ferrofin_model::entities_media::MediaStream;
use ferrofin_model::session::TranscodingInfo;
use ferrofin_traits::media_encoding::{
    TranscodingJobHandle, TranscodingJobType, TranscodingProgress,
};
use rstest::rstest;
use uuid::Uuid;

const ALICE: Option<Uuid> = Some(Uuid::from_u128(0xA11CE));
const BOB: Option<Uuid> = Some(Uuid::from_u128(0xB0B));
/// An API key has no user.
const API_KEY: Option<Uuid> = None;
/// The device every start comes from unless a test names another, as one
/// player's "next episode" does.
const TV: Option<&str> = Some("living-room-tv");

/// What the job does to each stream of the source.
#[derive(Clone, Copy)]
struct Streams {
    video: Option<&'static str>,
    audio: Option<&'static str>,
}

/// H.264 to H.264 at a lower size, audio re-encoded too: a full transcode.
const TRANSCODE: Streams = Streams {
    video: Some("h264"),
    audio: Some("aac"),
};
/// A remux or direct stream: every stream copied.
const FULL_COPY: Streams = Streams {
    video: Some("copy"),
    audio: Some("copy"),
};

/// Records what the manager reports.
#[derive(Default)]
struct Reports {
    killed: Mutex<Vec<(String, bool, bool)>>,
    started: Mutex<usize>,
}

struct Reporter(Arc<Reports>);

#[async_trait]
impl SessionReporter for Reporter {
    async fn report_progress(&self, _job: &TranscodingJobHandle, _progress: TranscodingProgress) {}
    async fn report_started(&self, _job: &TranscodingJobHandle, _info: TranscodingInfo) {
        *self.0.started.lock().unwrap() += 1;
    }
    async fn on_job_failed_to_start(&self, _job: &TranscodingJobHandle) {}
    async fn on_job_killed(
        &self,
        job: &TranscodingJobHandle,
        delete_files: bool,
        close_live_stream: bool,
    ) {
        self.0.killed.lock().unwrap().push((
            job.play_session_id.clone().unwrap_or_default(),
            delete_files,
            close_live_stream,
        ));
    }
}

struct Fixture {
    root: tempfile::TempDir,
    fake: FakeSegmentTranscoder,
    manager: TranscodeManagerImpl<Reporter>,
    reports: Arc<Reports>,
}

impl Fixture {
    fn new(server: u32, per_user: u32) -> Self {
        Self::scripted(
            server,
            per_user,
            FakeScript {
                segment_files: vec!["out0.ts".to_owned()],
                extra_files: vec!["out.m3u8".to_owned()],
                ..FakeScript::default()
            },
        )
    }

    fn scripted(server: u32, per_user: u32, script: FakeScript) -> Self {
        let reports = Arc::new(Reports::default());
        Self {
            root: tempfile::tempdir().unwrap(),
            fake: FakeSegmentTranscoder::new(script),
            manager: TranscodeManagerImpl::new(Reporter(Arc::clone(&reports)))
                .with_transcode_limits(TranscodeLimits { server, per_user }),
            reports,
        }
    }

    fn playlist(&self, session: &str) -> PathBuf {
        self.root.path().join(session).join("out.m3u8")
    }

    fn log_dir(&self) -> PathBuf {
        self.root.path().join("log")
    }

    async fn start(
        &self,
        session: &str,
        user: Option<Uuid>,
        streams: Streams,
    ) -> Result<TranscodingJobHandle, StartFfmpegError> {
        self.start_from(session, user, TV, streams).await
    }

    /// [`Self::start`] from `device`, the request's `DeviceId`.
    async fn start_from(
        &self,
        session: &str,
        user: Option<Uuid>,
        device: Option<&str>,
        streams: Streams,
    ) -> Result<TranscodingJobHandle, StartFfmpegError> {
        let playlist = self.playlist(session);
        let mut job = state(&playlist, session, streams);
        job.device_id = device.map(str::to_owned);
        self.manager
            .start_ffmpeg(&self.fake, request(&job, &playlist, self.log_dir(), user))
            .await
    }

    /// A job someone is reading from, which eviction must leave alone.
    async fn start_watched(
        &self,
        session: &str,
        user: Option<Uuid>,
    ) -> ferrofin_mediaencoding::transcoding::manager::TranscodeRequestGuard<
        '_,
        Reporter,
        ferrofin_mediaencoding::transcoding::FsFileCleaner,
    > {
        let handle = self
            .start(session, user, TRANSCODE)
            .await
            .expect("admitted");
        self.manager
            .begin_request_guard(&handle.path, handle.job_type)
            .expect("the job is registered")
    }

    fn spawned(&self) -> usize {
        self.fake.requests.lock().unwrap().len()
    }

    fn killed(&self) -> Vec<String> {
        let killed = self.reports.killed.lock().unwrap();
        killed.iter().map(|(session, ..)| session.clone()).collect()
    }
}

fn stream() -> MediaStream {
    MediaStream {
        index: 0,
        ..MediaStream::default()
    }
}

fn state(output_path: &Path, session: &str, streams: Streams) -> EncodingJobInfo {
    EncodingJobInfo {
        display: TranscodeDisplayNames::default(),
        base_request: BaseEncodingJobOptions::default(),
        video_stream: streams.video.map(|_| stream()),
        audio_stream: streams.audio.map(|_| stream()),
        subtitle_stream: None,
        media_source: MediaSourceInfo::default(),
        output_video_codec: streams.video.map(str::to_owned),
        output_audio_codec: streams.audio.map(str::to_owned),
        output_video_bitrate: None,
        output_audio_bitrate: None,
        output_audio_channels: None,
        output_container: None,
        output_video_sync: None,
        output_file_path: output_path.to_string_lossy().into_owned(),
        input_container: None,
        is_input_video: streams.video.is_some(),
        subtitle_delivery_method: SubtitleDeliveryMethod::Encode,
        trimmed_subtitle_path: None,
        transcode_reasons: ferrofin_model::session::TranscodeReasons::empty(),
        run_time_ticks: None,
        transcoding_type: TranscodingJobType::Hls,
        supported_video_codecs: Vec::new(),
        supported_audio_codecs: Vec::new(),
        supported_subtitle_codecs: Vec::new(),
        segment_length_secs: 6,
        wait_for_path: Some(output_path.to_path_buf()),
        segment_container: Some("ts".to_owned()),
        play_session_id: Some(session.to_owned()),
        device_id: TV.map(str::to_owned),
    }
}

fn request<'a>(
    state: &'a EncodingJobInfo,
    output_path: &'a Path,
    log_dir: PathBuf,
    user_id: Option<Uuid>,
) -> StartFfMpegRequest<'a> {
    StartFfMpegRequest {
        program: "ffmpeg",
        state,
        output_path,
        arguments: Vec::new(),
        log_dir,
        working_dir: None,
        env: Vec::new(),
        hardware_acceleration_type: HardwareAccelerationType::none,
        user_id,
    }
}

fn refused_by(result: Result<TranscodingJobHandle, StartFfmpegError>) -> TranscodeRefused {
    match result {
        Err(StartFfmpegError::Refused(refused)) => refused,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[rstest]
#[case::full_transcode(Some("h264"), Some("aac"), true)]
#[case::copied_video_reencoded_audio(Some("copy"), Some("aac"), true)]
#[case::reencoded_video_copied_audio(Some("h264"), Some("copy"), true)]
#[case::full_copy(Some("copy"), Some("copy"), false)]
#[case::copy_is_case_insensitive(Some("COPY"), Some("Copy"), false)]
#[case::audio_only_transcode(None, Some("mp3"), true)]
#[case::audio_only_copy(None, Some("copy"), false)]
#[case::video_only_copy(Some("copy"), None, false)]
#[case::video_only_transcode(Some("h264"), None, true)]
fn what_counts_is_any_stream_re_encoded(
    #[case] video: Option<&'static str>,
    #[case] audio: Option<&'static str>,
    #[case] counts: bool,
) {
    let job = state(Path::new("/t/out.m3u8"), "s", Streams { video, audio });
    assert_eq!(job.encodes_any_stream(), counts);
}

/// The planner names an output audio codec for a source with no audio, and the
/// argument builder ignores it there: a copied silent video is a full copy.
#[test]
fn an_audio_codec_named_for_a_silent_source_does_not_count() {
    let mut job = state(Path::new("/t/out.m3u8"), "s", FULL_COPY);
    job.audio_stream = None;
    job.output_audio_codec = Some("aac".to_owned());
    assert!(!job.encodes_any_stream());
}

/// With no stream known ffmpeg picks streams and encoders itself, so the job
/// cannot be called a copy.
#[test]
fn a_job_with_no_known_stream_counts() {
    let job = state(
        Path::new("/t/out.m3u8"),
        "s",
        Streams {
            video: None,
            audio: None,
        },
    );
    assert!(job.encodes_any_stream());
}

#[tokio::test]
async fn the_caps_are_off_by_default() {
    let fixture = Fixture::new(0, 0);
    let mut watched = Vec::new();
    for index in 0..8 {
        watched.push(fixture.start_watched(&format!("s{index}"), ALICE).await);
    }
    assert_eq!(fixture.spawned(), 8);
    assert_eq!(fixture.manager.active_job_count(), 8);
}

#[tokio::test]
async fn a_user_past_the_cap_is_refused_and_others_still_start() {
    let fixture = Fixture::new(0, 2);
    let _first = fixture.start_watched("a1", ALICE).await;
    let _second = fixture.start_watched("a2", ALICE).await;
    let spawned = fixture.spawned();

    let refused = fixture.start("a3", ALICE, TRANSCODE).await;
    assert_eq!(refused_by(refused), TranscodeRefused::User);
    assert_eq!(fixture.spawned(), spawned, "nothing spawned");
    assert_eq!(fixture.manager.active_job_count(), 2);

    fixture
        .start("b1", BOB, TRANSCODE)
        .await
        .expect("Bob's cap");
    fixture
        .start("key", API_KEY, TRANSCODE)
        .await
        .expect("no user, no user cap");
}

#[tokio::test]
async fn a_killed_job_frees_its_slot() {
    let fixture = Fixture::new(0, 1);
    let handle = fixture.start("a1", ALICE, TRANSCODE).await.unwrap();
    {
        let _watching = fixture
            .manager
            .begin_request_guard(&handle.path, handle.job_type);
        refused_by(fixture.start("a2", ALICE, TRANSCODE).await);
    }
    fixture.manager.kill_and_remove(&handle, false, false).await;
    assert_eq!(fixture.manager.active_job_count(), 0, "out of the registry");
    fixture
        .start("a3", ALICE, TRANSCODE)
        .await
        .expect("a killed job frees its slot");
}

/// A refusal is cheap and leaves nothing behind: no directory, no log file, no
/// session report, no process.
#[tokio::test]
async fn a_refusal_touches_nothing() {
    let fixture = Fixture::new(1, 0);
    let _first = fixture.start_watched("a1", ALICE).await;
    let started = *fixture.reports.started.lock().unwrap();

    refused_by(fixture.start("b1", BOB, TRANSCODE).await);

    assert!(
        !fixture.root.path().join("b1").exists(),
        "no output directory"
    );
    assert!(!fixture.log_dir().join("b1").exists());
    assert_eq!(std::fs::read_dir(fixture.log_dir()).unwrap().count(), 0);
    assert_eq!(*fixture.reports.started.lock().unwrap(), started);
}

/// An API key has no user: outside the per-user cap, inside the server-wide one.
#[tokio::test]
async fn an_api_key_is_outside_the_user_cap_and_inside_the_server_cap() {
    let fixture = Fixture::new(3, 1);
    let _first = fixture.start_watched("key1", API_KEY).await;
    let _second = fixture.start_watched("key2", API_KEY).await;
    let _third = fixture.start_watched("key3", API_KEY).await;
    assert_eq!(fixture.spawned(), 3, "the per-user cap of 1 does not apply");

    assert_eq!(
        refused_by(fixture.start("key4", API_KEY, TRANSCODE).await),
        TranscodeRefused::Server
    );
    assert_eq!(
        refused_by(fixture.start("a1", ALICE, TRANSCODE).await),
        TranscodeRefused::Server,
        "the API key's jobs count against everyone"
    );
}

/// With no user there are no idle jobs of "theirs": a key's idle job waits for
/// the reaper, and another key's start is refused.
#[tokio::test]
async fn an_api_key_evicts_nothing() {
    let fixture = Fixture::new(1, 0);
    fixture.start("key1", API_KEY, TRANSCODE).await.unwrap();
    assert_eq!(
        refused_by(fixture.start("key2", API_KEY, TRANSCODE).await),
        TranscodeRefused::Server
    );
    assert_eq!(fixture.killed(), Vec::<String>::new());
}

#[tokio::test]
async fn a_full_copy_is_not_counted() {
    let fixture = Fixture::new(1, 1);
    for session in ["c1", "c2", "c3"] {
        fixture
            .start(session, ALICE, FULL_COPY)
            .await
            .expect("a copy is free");
    }
    let _encoding = fixture.start_watched("a1", ALICE).await;
    assert_eq!(fixture.manager.active_job_count(), 4);
    assert_eq!(
        fixture.killed(),
        Vec::<String>::new(),
        "a copy holds no slot to free"
    );

    fixture
        .start("c4", ALICE, FULL_COPY)
        .await
        .expect("a copy needs no slot, at the caps too");
}

#[rstest]
#[case::copied_video_reencoded_audio(Some("copy"), Some("aac"))]
#[case::music_transcode(None, Some("mp3"))]
#[tokio::test]
async fn a_job_re_encoding_one_stream_is_counted(
    #[case] video: Option<&'static str>,
    #[case] audio: Option<&'static str>,
) {
    let fixture = Fixture::new(0, 1);
    let _watched = fixture.start_watched("a1", ALICE).await;
    let refused = fixture.start("a2", ALICE, Streams { video, audio }).await;
    assert_eq!(refused_by(refused), TranscodeRefused::User);

    let fixture = Fixture::new(0, 1);
    let handle = fixture
        .start("a1", ALICE, Streams { video, audio })
        .await
        .unwrap();
    let _watching = fixture
        .manager
        .begin_request_guard(&handle.path, handle.job_type);
    assert_eq!(
        refused_by(fixture.start("a2", ALICE, TRANSCODE).await),
        TranscodeRefused::User,
        "the job itself holds the slot"
    );
}

/// "Next episode": the last episode's job has no consumer but waits for its
/// ping timeout, and a cap of 1 must not make the next one wait with it.
#[tokio::test]
async fn next_episode_evicts_the_users_idle_job() {
    let fixture = Fixture::new(0, 1);
    fixture.start("ep1", ALICE, TRANSCODE).await.unwrap();

    fixture
        .start("ep2", ALICE, TRANSCODE)
        .await
        .expect("the idle job gives up its slot");

    assert_eq!(fixture.manager.active_job_count(), 1);
    assert_eq!(fixture.spawned(), 2);
    assert_eq!(
        *fixture.reports.killed.lock().unwrap(),
        [("ep1".to_owned(), true, true)],
        "torn down as the idle reaper would"
    );
}

/// As on a seek restart, a job taking over the same live stream re-opens
/// ffmpeg on the tuner, so the evicted job must not close it. Another live
/// stream's tuner goes back, as the reaper would send it.
#[rstest]
#[case::same_live_stream("next", false)]
#[case::another_live_stream("other", true)]
#[tokio::test]
async fn eviction_keeps_a_live_stream_the_next_job_reads(
    #[case] next_stream: &str,
    #[case] closed: bool,
) {
    let fixture = Fixture::new(0, 1);
    let live = |session: &str, stream: &str| {
        let playlist = fixture.playlist(session);
        let mut job = state(&playlist, session, TRANSCODE);
        job.base_request.live_stream_id = Some(stream.to_owned());
        (playlist, job)
    };
    let (playlist, job) = live("ch1", "next");
    fixture
        .manager
        .start_ffmpeg(
            &fixture.fake,
            request(&job, &playlist, fixture.log_dir(), ALICE),
        )
        .await
        .unwrap();
    let (playlist, job) = live("ch2", next_stream);
    fixture
        .manager
        .start_ffmpeg(
            &fixture.fake,
            request(&job, &playlist, fixture.log_dir(), ALICE),
        )
        .await
        .expect("the idle job gives up its slot");

    assert_eq!(
        *fixture.reports.killed.lock().unwrap(),
        [("ch1".to_owned(), true, closed)]
    );
}

/// A client that hangs up on a start leaves a job nobody waits for. Its slot
/// goes to the same user's next start from the same device, not to the ping
/// timeout, whether or not the process had started yet.
#[rstest]
#[case::while_spawning(true)]
#[case::while_waiting_for_output(false)]
#[tokio::test]
async fn a_start_the_client_hung_up_on_gives_up_its_slot(#[case] spawning: bool) {
    /// Never finishes spawning.
    struct HangingSpawn;

    #[async_trait]
    impl SegmentTranscoder for HangingSpawn {
        async fn start_transcode(
            &self,
            _request: &SpawnRequest,
        ) -> Result<Box<dyn TranscodeChild>, String> {
            std::future::pending().await
        }
    }

    let fixture = Fixture::new(0, 1);
    // A process that runs and never writes its output in this test's time.
    let silent = FakeSegmentTranscoder::new(FakeScript {
        write_after_polls: usize::MAX,
        ..FakeScript::default()
    });
    let playlist = fixture.playlist("gone");
    let job = state(&playlist, "gone", TRANSCODE);
    let start = request(&job, &playlist, fixture.log_dir(), ALICE);
    let hanging: &dyn SegmentTranscoder = if spawning { &HangingSpawn } else { &silent };
    let started = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        fixture.manager.start_ffmpeg(hanging, start),
    )
    .await;
    assert!(started.is_err(), "the start is still waiting when dropped");
    assert_eq!(
        fixture.manager.active_job_count(),
        1,
        "its job stays behind"
    );

    fixture
        .start("next", ALICE, TRANSCODE)
        .await
        .expect("the abandoned job gives up its slot");
    assert_eq!(fixture.killed(), ["gone"]);
    assert_eq!(fixture.manager.active_job_count(), 1);
}

#[tokio::test]
async fn a_job_being_read_is_not_evicted() {
    let fixture = Fixture::new(0, 1);
    let _watched = fixture.start_watched("a1", ALICE).await;
    refused_by(fixture.start("a2", ALICE, TRANSCODE).await);
    assert_eq!(fixture.killed(), Vec::<String>::new());
    assert_eq!(fixture.manager.active_job_count(), 1);
}

/// One job is enough, and it is the one idle longest: the newer one may be a
/// player between two segments.
#[tokio::test]
async fn eviction_takes_only_the_longest_idle_job() {
    let fixture = Fixture::new(0, 2);
    let older = fixture.start("older", ALICE, TRANSCODE).await.unwrap();
    let newer = fixture.start("newer", ALICE, TRANSCODE).await.unwrap();
    // Start order is not activity order: a read of `older` makes `newer` the
    // one idle longest.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    drop(
        fixture
            .manager
            .begin_request_guard(&older.path, older.job_type),
    );

    fixture.start("third", ALICE, TRANSCODE).await.unwrap();

    assert_eq!(fixture.killed(), [newer.play_session_id.unwrap()]);
    assert_eq!(fixture.manager.active_job_count(), 2);
}

#[tokio::test]
async fn another_users_idle_job_is_left_alone() {
    let fixture = Fixture::new(1, 0);
    fixture.start("a1", ALICE, TRANSCODE).await.unwrap();
    assert_eq!(
        refused_by(fixture.start("b1", BOB, TRANSCODE).await),
        TranscodeRefused::Server
    );
    assert_eq!(fixture.killed(), Vec::<String>::new());
}

#[tokio::test]
async fn the_server_cap_also_evicts_the_users_own_idle_job() {
    let fixture = Fixture::new(1, 0);
    fixture.start("a1", ALICE, TRANSCODE).await.unwrap();
    fixture
        .start("a2", ALICE, TRANSCODE)
        .await
        .expect("evicts a1");
    assert_eq!(fixture.killed(), ["a1"]);
}

/// Two devices on one account. A job with no request is also a player between
/// two segment requests, so a start from another device past a cap is refused:
/// ending that job would have its next segment restart it and end this one in
/// turn, the two devices ending each other's jobs.
#[rstest]
#[case::per_user_cap(0, 1, TranscodeRefused::User)]
#[case::server_cap(1, 0, TranscodeRefused::Server)]
#[tokio::test]
async fn another_device_of_the_user_is_refused_and_its_stream_kept(
    #[case] server: u32,
    #[case] per_user: u32,
    #[case] refusal: TranscodeRefused,
) {
    let fixture = Fixture::new(server, per_user);
    let playing = fixture.start("tv", ALICE, TRANSCODE).await.unwrap();

    let refused = fixture
        .start_from("tablet", ALICE, Some("bedroom-tablet"), TRANSCODE)
        .await;

    assert_eq!(refused_by(refused), refusal);
    assert_eq!(fixture.killed(), Vec::<String>::new());
    assert_eq!(fixture.spawned(), 1, "nothing spawned for the tablet");
    assert!(
        fixture
            .manager
            .begin_request_guard(&playing.path, playing.job_type)
            .is_some(),
        "the TV's next segment request finds its job"
    );
}

/// The job evicted is one of the same device, its `DeviceId` compared ignoring
/// ASCII case, as upstream's `KillTranscodingJobs` ignores case. Unless both
/// sides carry a device there is no telling a player of the same device, so
/// nothing is evicted.
#[rstest]
#[case::same_device(Some("tv"), Some("tv"), true)]
#[case::same_device_in_another_case(Some("Living-Room-TV"), Some("living-room-tv"), true)]
#[case::another_device(Some("tv"), Some("tablet"), false)]
#[case::no_device_on_either(None, None, false)]
#[case::empty_device_on_both(Some(""), Some(""), false)]
#[case::no_device_on_the_new_start(Some("tv"), None, false)]
#[case::no_device_on_the_idle_job(None, Some("tv"), false)]
#[tokio::test]
async fn only_a_job_of_the_same_device_is_evicted(
    #[case] idle_device: Option<&str>,
    #[case] next_device: Option<&str>,
    #[case] evicted: bool,
) {
    let fixture = Fixture::new(0, 1);
    fixture
        .start_from("ep1", ALICE, idle_device, TRANSCODE)
        .await
        .unwrap();

    let next = fixture
        .start_from("ep2", ALICE, next_device, TRANSCODE)
        .await;

    if evicted {
        next.expect("the same device's idle job gives up its slot");
        assert_eq!(fixture.killed(), ["ep1"]);
    } else {
        assert_eq!(refused_by(next), TranscodeRefused::User);
        assert_eq!(fixture.killed(), Vec::<String>::new());
    }
    assert_eq!(fixture.manager.active_job_count(), 1);
}

/// A job still starting holds a slot and has its start as a consumer, so a
/// concurrent start can neither pass the count nor take it for idle.
#[tokio::test]
async fn a_starting_job_counts_and_is_not_idle() {
    let fixture = Fixture::scripted(
        0,
        1,
        FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            // The first start waits for its output across several polls.
            write_after_polls: 3,
            ..FakeScript::default()
        },
    );
    let (first, second) = tokio::join!(
        fixture.start("a1", ALICE, TRANSCODE),
        fixture.start("a2", ALICE, TRANSCODE),
    );
    first.expect("the first start is admitted");
    assert_eq!(refused_by(second), TranscodeRefused::User);
    assert_eq!(fixture.killed(), Vec::<String>::new());
    assert_eq!(fixture.spawned(), 1);
}

/// A job whose process finished stays registered for its segments, and holds
/// no slot: nothing is left to end.
#[tokio::test]
async fn a_finished_job_holds_no_slot() {
    let fixture = Fixture::scripted(
        0,
        1,
        FakeScript {
            segment_files: vec!["out0.ts".to_owned()],
            extra_files: vec!["out.m3u8".to_owned()],
            exits_immediately: true,
            ..FakeScript::default()
        },
    );
    fixture.start("a1", ALICE, TRANSCODE).await.unwrap();
    fixture.start("a2", ALICE, TRANSCODE).await.expect("room");
    assert_eq!(fixture.manager.active_job_count(), 2);
    assert_eq!(
        fixture.killed(),
        Vec::<String>::new(),
        "nothing needed evicting"
    );
}

/// A job whose ffmpeg exits non-zero at startup stays registered (gl-1373)
/// and holds no slot either.
#[tokio::test]
async fn a_job_whose_ffmpeg_failed_at_startup_holds_no_slot() {
    let fixture = Fixture::scripted(
        0,
        1,
        FakeScript {
            exit_code: 1,
            exits_immediately: true,
            ..FakeScript::default()
        },
    );
    let failed = fixture.start("a1", ALICE, TRANSCODE).await;
    assert!(
        matches!(failed, Err(StartFfmpegError::Failed(_))),
        "{failed:?}"
    );
    assert_eq!(fixture.manager.active_job_count(), 1);
    let admitted = fixture.start("a2", ALICE, TRANSCODE).await;
    assert!(
        matches!(admitted, Err(StartFfmpegError::Failed(_))),
        "admitted, then failed like the first: {admitted:?}"
    );
    assert_eq!(fixture.manager.active_job_count(), 2);
    assert_eq!(
        fixture.killed(),
        Vec::<String>::new(),
        "nothing needed evicting"
    );
}

/// An admitted job that fails before it spawns gives its slot back.
#[tokio::test]
async fn a_start_failing_after_admission_gives_its_slot_back() {
    let fixture = Fixture::new(0, 1);
    let root = Path::new("/");
    let job = state(root, "bad", TRANSCODE);
    let failed = fixture
        .manager
        .start_ffmpeg(&fixture.fake, request(&job, root, fixture.log_dir(), ALICE))
        .await;
    assert!(
        matches!(failed, Err(StartFfmpegError::Failed(_))),
        "{failed:?}"
    );
    assert_eq!(fixture.manager.active_job_count(), 0);
    fixture.start("a1", ALICE, TRANSCODE).await.expect("room");
}
