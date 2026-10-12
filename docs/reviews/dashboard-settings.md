# Dashboard settings review findings

The per-finding reports of the Jellyfin Web dashboard settings review on `fix/dashboard-settings`, grouped by dashboard page. Each section records the upstream behaviour, what changed, and the evidence it was checked with.

## Contents

- [Configuration plumbing](#configuration-plumbing)
  - [First implementation batch](#first-implementation-batch)
  - [Second implementation batch](#second-implementation-batch)
  - [Typed dashboard configuration documents](#typed-dashboard-configuration-documents)
  - [Validate and atomically save named configurations](#validate-and-atomically-save-named-configurations)
  - [Publish successful configuration updates](#publish-successful-configuration-updates)
  - [Current Web payload compatibility](#current-web-payload-compatibility)
- [General and overview](#general-and-overview)
  - [Advertise saved server names immediately](#advertise-saved-server-names-immediately)
  - [Verify the live server UI culture](#verify-the-live-server-ui-culture)
  - [Apply the dashboard cache directory](#apply-the-dashboard-cache-directory)
  - [Apply metadata directory changes consistently](#apply-metadata-directory-changes-consistently)
  - [Verify the live Quick Connect toggle](#verify-the-live-quick-connect-toggle)
  - [Apply the library scan fanout limit](#apply-the-library-scan-fanout-limit)
  - [Honor the parallel image encoding limit](#honor-the-parallel-image-encoding-limit)
  - [Verify restart and shutdown](#verify-restart-and-shutdown)
  - [Verify dashboard overview widgets](#verify-dashboard-overview-widgets)
- [Branding](#branding)
  - [The splashscreen preview, formats and first upload](#the-splashscreen-preview-formats-and-first-upload)
- [Users: profile and permissions](#users-profile-and-permissions)
  - [Account management and password changes](#account-management-and-password-changes)
  - [Administrator, disabled and hidden accounts](#administrator-disabled-and-hidden-accounts)
  - [Saved lockout thresholds and concurrent failures](#saved-lockout-thresholds-and-concurrent-failures)
  - [Maximum active sessions](#maximum-active-sessions)
  - [Selected authentication provider](#selected-authentication-provider)
  - [Built-in password recovery and reset-provider selection](#built-in-password-recovery-and-reset-provider-selection)
  - [Per-user remote access](#per-user-remote-access)
  - [Allow media playback](#allow-media-playback)
  - [Audio/video transcoding and remuxing permissions](#audiovideo-transcoding-and-remuxing-permissions)
  - [Force transcoding of remote sources](#force-transcoding-of-remote-sources)
  - [Internet streaming bitrate limit](#internet-streaming-bitrate-limit)
  - [Download permission and file eligibility](#download-permission-and-file-eligibility)
  - [Media deletion — open dependency](#media-deletion--open-dependency)
  - [Collection management](#collection-management)
  - [Subtitle-management permission](#subtitle-management-permission)
  - [Live TV access and management](#live-tv-access-and-management)
  - [Remote control and shared devices](#remote-control-and-shared-devices)
  - [SyncPlay access](#syncplay-access)
- [Users: access and parental controls](#users-access-and-parental-controls)
  - [Library selection and direct item access](#library-selection-and-direct-item-access)
  - [Owned extras on adopted databases (follow-up)](#owned-extras-on-adopted-databases-follow-up)
  - [Channel selection](#channel-selection)
  - [Device selection](#device-selection)
  - [Parental ratings and subratings](#parental-ratings-and-subratings)
  - [Block unrated content](#block-unrated-content)
  - [Allowed and blocked tags](#allowed-and-blocked-tags)
  - [Access schedules](#access-schedules)
- [Libraries: management and basic options](#libraries-management-and-basic-options)
  - [Library management and media paths](#library-management-and-media-paths)
  - [Rename honors the refresh setting](#rename-honors-the-refresh-setting)
  - [Disabled libraries stay out of listings and counts](#disabled-libraries-stay-out-of-listings-and-counts)
  - [Disabling photos removes existing catalog entries on scan](#disabling-photos-removes-existing-catalog-entries-on-scan)
  - [Realtime monitoring follows saved library options](#realtime-monitoring-follows-saved-library-options)
  - [Automatic series grouping uses invariant Unicode casing](#automatic-series-grouping-uses-invariant-unicode-casing)
  - [Metadata language and country reach the providers](#metadata-language-and-country-reach-the-providers)
  - [Automatic metadata refresh interval](#automatic-metadata-refresh-interval)
  - [Season zero uses the saved display name](#season-zero-uses-the-saved-display-name)
  - [Embedded video titles and episode information](#embedded-video-titles-and-episode-information)
  - [Automatic collections after library validation](#automatic-collections-after-library-validation)
- [Libraries: metadata and image providers](#libraries-metadata-and-image-providers)
  - [Metadata provider selection and order](#metadata-provider-selection-and-order)
  - [Image provider selection and order](#image-provider-selection-and-order)
  - [Automatic artwork limits and minimum widths](#automatic-artwork-limits-and-minimum-widths)
  - [Local metadata reader order](#local-metadata-reader-order)
  - [Automatic NFO saver selection](#automatic-nfo-saver-selection)
  - [Artwork saved beside media](#artwork-saved-beside-media)
  - [Similar-item provider selection and order](#similar-item-provider-selection-and-order)
- [Libraries: subtitles, lyrics, music analysis and media segments](#libraries-subtitles-lyrics-music-analysis-and-media-segments)
  - [Allowed embedded subtitles](#allowed-embedded-subtitles)
  - [Automatic subtitle languages and matching controls](#automatic-subtitle-languages-and-matching-controls)
  - [Subtitle provider selection and order](#subtitle-provider-selection-and-order)
  - [Subtitle destinations and collision handling](#subtitle-destinations-and-collision-handling)
  - [Live lyric destinations](#live-lyric-destinations)
  - [Lyric provider selection and order](#lyric-provider-selection-and-order)
  - [Configured embedded music tags](#configured-embedded-music-tags)
  - [Audio normalization analysis](#audio-normalization-analysis)
  - [Registered media-segment providers](#registered-media-segment-providers)
- [Libraries: chapters and trickplay](#libraries-chapters-and-trickplay)
  - [Enable chapter images](#enable-chapter-images)
  - [Chapter images during library scans](#chapter-images-during-library-scans)
  - [Trickplay extraction enablement](#trickplay-extraction-enablement)
  - [Trickplay extraction during scans](#trickplay-extraction-during-scans)
  - [Trickplay storage beside media](#trickplay-storage-beside-media)
- [Libraries: display](#libraries-display)
  - [Display folder view](#display-folder-view)
  - [Group movies and shows into collections](#group-movies-and-shows-into-collections)
  - [Display specials within seasons](#display-specials-within-seasons)
  - [External content in suggestions](#external-content-in-suggestions)
  - [Date added from file creation time](#date-added-from-file-creation-time)
- [Libraries: metadata defaults](#libraries-metadata-defaults)
  - [Default metadata language (`PreferredMetadataLanguage`)](#default-metadata-language-preferredmetadatalanguage)
  - [Default metadata country (`MetadataCountryCode`)](#default-metadata-country-metadatacountrycode)
  - [Dummy chapter duration (`DummyChapterDuration`)](#dummy-chapter-duration-dummychapterduration)
  - [Chapter image resolution (`ChapterImageResolution`)](#chapter-image-resolution-chapterimageresolution)
- [Libraries: NFO](#libraries-nfo)
  - [Save selected-user watch data in NFO (`xbmcmetadata.UserId`)](#save-selected-user-watch-data-in-nfo-xbmcmetadatauserid)
  - [Save image paths in NFO (`SaveImagePathsInNfo`)](#save-image-paths-in-nfo-saveimagepathsinnfo)
  - [NFO path substitution (`EnablePathSubstitution`)](#nfo-path-substitution-enablepathsubstitution)
  - [Extra thumbnail duplication (`EnableExtraThumbsDuplication`)](#extra-thumbnail-duplication-enableextrathumbsduplication)
- [Playback: transcoding](#playback-transcoding)
  - [Resolved encoder path display (`EncoderAppPathDisplay`)](#resolved-encoder-path-display-encoderapppathdisplay)
  - [Effective transcode directory (`TranscodingTempPath`)](#effective-transcode-directory-transcodingtemppath)
  - [Encoding threads and `CpuCoreLimit`](#encoding-threads-and-cpucorelimit)
  - [Configured encoder preset and CRF](#configured-encoder-preset-and-crf)
  - [Allowed HEVC and AV1 encoder selection](#allowed-hevc-and-av1-encoder-selection)
  - [Live transcode throttling](#live-transcode-throttling)
  - [Rolling HLS segment retention](#rolling-hls-segment-retention)
  - [Maximum HLS muxing queue](#maximum-hls-muxing-queue)
- [Networking](#networking)
  - [The listener follows the network settings](#the-listener-follows-the-network-settings)
- [Live TV](#live-tv)
  - [The M3U playlist is fetched with the tuner's user agent](#the-m3u-playlist-is-fetched-with-the-tuners-user-agent)
  - [Tuner stream settings reach the HLS transcode](#tuner-stream-settings-reach-the-hls-transcode)
- [Scheduled tasks](#scheduled-tasks)
  - [A trigger's maximum runtime applies to the runs it starts](#a-triggers-maximum-runtime-applies-to-the-runs-it-starts)
  - [A trigger save that cannot be written is an error](#a-trigger-save-that-cannot-be-written-is-an-error)
- [Backups](#backups)
  - [A restored backup's metadata follows its restored configuration](#a-restored-backups-metadata-follows-its-restored-configuration)
  - [A failed restore is reported, not announced as a success](#a-failed-restore-is-reported-not-announced-as-a-success)
- [Scanning and library structure](#scanning-and-library-structure)
  - [Plain subfolders of a movie library are `Folder`s](#plain-subfolders-of-a-movie-library-are-folders)
  - [Unlocked metadata after a library rename and an explicit scan — owner decision needed](#unlocked-metadata-after-a-library-rename-and-an-explicit-scan--owner-decision-needed)
  - [Repair the complete scan-change fixture](#repair-the-complete-scan-change-fixture)
  - [Filesystem-root library shortcuts](#filesystem-root-library-shortcuts)
  - [A video being recorded is keyed under Live TV](#a-video-being-recorded-is-keyed-under-live-tv)
  - [Home-video libraries resolve plain videos](#home-video-libraries-resolve-plain-videos)
  - [By-name identity settings and long folder names](#by-name-identity-settings-and-long-folder-names)
- [Metadata providers and local metadata](#metadata-providers-and-local-metadata)
  - [Path-less episode metadata providers](#path-less-episode-metadata-providers)
  - [TVDB metadata follows the series display order](#tvdb-metadata-follows-the-series-display-order)
  - [The fanart.tv season image provider](#the-fanarttv-season-image-provider)
  - [Book covers are image providers, not a side effect of the metadata read](#book-covers-are-image-providers-not-a-side-effect-of-the-metadata-read)
  - [Local NFO readers beyond movies, series and episodes](#local-nfo-readers-beyond-movies-series-and-episodes)
  - [Book local-metadata change monitors](#book-local-metadata-change-monitors)
  - [Custom NFO release-date formats](#custom-nfo-release-date-formats)
  - [Embedded tags become an audio item's credits](#embedded-tags-become-an-audio-items-credits)
  - [Generic NFO ordering follows the culture, and duplicates fold Unicode](#generic-nfo-ordering-follows-the-culture-and-duplicates-fold-unicode)
  - [The metadata refresh queue](#the-metadata-refresh-queue)
- [Artwork](#artwork)
  - [Embedded images from videos](#embedded-images-from-videos)
  - [Indexed artwork deletion](#indexed-artwork-deletion)
  - [Artwork replacement cleanup failures](#artwork-replacement-cleanup-failures)
  - [Indexed image uploads](#indexed-image-uploads)
  - [Screenshot upload replacement](#screenshot-upload-replacement)
  - [Artwork mutation metadata lifecycle](#artwork-mutation-metadata-lifecycle)
  - [Backdrop reorder exchanges the files](#backdrop-reorder-exchanges-the-files)
- [Subtitles and chapters](#subtitles-and-chapters)
  - [Subtitle provider namespaces](#subtitle-provider-namespaces)
  - [Saved item metadata in subtitle searches](#saved-item-metadata-in-subtitle-searches)
  - [Blank and time-formatted chapter names](#blank-and-time-formatted-chapter-names)
- [Playback and encoding](#playback-and-encoding)
  - [Remote `.strm` shortcut playback](#remote-strm-shortcut-playback)
  - [Audio-specific device-profile negotiation](#audio-specific-device-profile-negotiation)
  - [Encoder selection honours the saved path, and ffprobe is found like Jellyfin finds it](#encoder-selection-honours-the-saved-path-and-ffprobe-is-found-like-jellyfin-finds-it)
- [Browsing and ordering](#browsing-and-ordering)
  - [Artist DTO links name real artist items](#artist-dto-links-name-real-artist-items)
  - [The user's saved view order (`OrderedViews`)](#the-users-saved-view-order-orderedviews)
  - [Recursive browse ordering — `Studio`, `Artist`, `AlbumArtist`, `IsPlayed`](#recursive-browse-ordering--studio-artist-albumartist-isplayed)
  - [A box set's `DisplayOrder`](#a-box-sets-displayorder)
- [Localization](#localization)
  - [Localization dropdown resource catalog](#localization-dropdown-resource-catalog)
  - [Ambient UI culture vs configured server culture](#ambient-ui-culture-vs-configured-server-culture)
- [Task queue](#task-queue)
  - [Busy scheduled-task queue and completion context](#busy-scheduled-task-queue-and-completion-context)

## Configuration plumbing

### First implementation batch

This batch covers the first 20 findings, **[Typed dashboard configuration documents](#typed-dashboard-configuration-documents)–[Current Web payload compatibility](#current-web-payload-compatibility), [Advertise saved server names immediately](#advertise-saved-server-names-immediately)–[Verify dashboard overview widgets](#verify-dashboard-overview-widgets) and [Account management and password changes](#account-management-and-password-changes)–[Per-user remote access](#per-user-remote-access)**,
in order on `fix/dashboard-settings`, based on
`782ca9f28cc938bf15b5a55b425c435673d2f392`. Each finding has its own signed-off
commit and validation record in this directory, without a co-author trailer.
The canonical living checklist is
`brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md`; it remains ignored
and shared with the main checkout through the worktree's `brain` link.

The changes connect saved configuration to its runtime consumers, validate and
atomically publish updates, honor cache/metadata paths and concurrency limits,
and correct account, session and authentication-provider behavior. Findings that
were already supported received targeted verification and regression coverage.
[Per-user remote access](#per-user-remote-access)'s original "Supported" assessment proved too broad: ordinary requests and
password login bypassed the user remote-access setting. That gap is fixed here.
The remaining dashboard findings retain their individual status in the checklist.

The UI reference is local Jellyfin Web
`1e507c588f353482a00333f84e36ddb7c8fc8221`; the behavior oracle is the repository's
pinned Jellyfin `v12.0-rc7`, `4910aafa1a`. Live checks used disposable data and
accounts. The primary checkout's code and build targets were left untouched.

#### Final checks

All required checks pass on the final implementation:

- `cargo fmt --all --check`.
- Strict workspace Clippy with all targets and features.
- `cargo nextest run --workspace`: **7,772 passed, 5 skipped**.
- `cargo test --workspace --doc`: **3 passed**, no failures.
- Debug and release server builds, plus the individual live HTTP checks recorded
  beside each finding.

Each coverage gate ran one crate at a time and excluded other workspace crates
from its report. All ten changed, non-exempt crates exceed the 80% line gate:

| Crate | Line coverage |
|---|---:|
| `ferrofin-util` | 96.10% |
| `ferrofin-common` | 87.18% |
| `ferrofin-model` | 86.86% |
| `ferrofin-drawing` | 91.41% |
| `ferrofin-mediaencoding` | 93.74% |
| `ferrofin-providers` | 93.20% |
| `ferrofin-livetv` | 93.83% |
| `ferrofin-extensions` | 94.87% |
| `ferrofin-api` | 87.19% |
| `ferrofin-core` | 93.85% |

Traits and the server composition root have the repository's documented coverage
exemption. The final workspace run includes the SQL-boundary check, real-server
restart/restore tests, and contract route checks. Two issues caught during the
batch run were corrected before the passing rerun: the remote-login fixture now
explicitly grants remote access after checking rejection, and the provider-ID
write lives in a persistence module.

#### Combined browse verification

The repository's `bench/screens.js` functional shape pass ran against both native
release binaries built with Rust 1.98.1. Docker daemon access is unavailable to
this account. Separate disposable configuration copies shared a private,
read-only copy of the generated benchmark media; original benchmark data was
preserved. Servers used CPUs 8–15 and k6 used CPUs 16–19. Plugins were disabled,
startup tasks drained, and a 30-second settle preceded the functional pass.

The baseline is the branch base; the after build includes all 20 findings. Both
returned **146 successful responses** (144 HTTP 200 and two HTTP 204), with
identical statuses, item counts, field sets and image byte counts in the shape
pass. Library counts also matched: 3,001 movies, 250 series, 7,490 episodes,
296 artists, 800 albums and 8,000 songs. This verifies consistency across the
combined changes on the repository's realistic browse fixture.

A combined latency window could **not** be measured: three attempts failed the
unchanged 90% initial-idle requirement at **83.8%, 80.1% and 23.7% idle**.
The functional pass deliberately evaluates no latency results. Docker/cgroup
memory measurements were also unavailable. Per-finding before/after HTTP timings
and resource-limit observations remain in the individual records; they are
shared-host observations, not optimization claims. The pending combined latency
comparison needs a quiet host; its prepared method is the same pinned cores,
separate 30-second warmups and 120-second windows at five screens per second.

The [validation summary](dashboard-settings-batch-01-validation.json) preserves
coverage, test totals, binary hashes and the functional comparison. Local raw
logs are `/tmp/ferrofin-dashboard-final-*.log`; disposable shape results are under
`target/dashboard-native-bench/{before-shape,after-shape}/`. This record closes
the earlier batch-end test and coverage notes and explicitly records the
remaining performance-measurement limitation.

#### Validation record

- Findings:
  - [Typed dashboard configuration documents](#typed-dashboard-configuration-documents), [Validate and atomically save named configurations](#validate-and-atomically-save-named-configurations), [Publish successful configuration updates](#publish-successful-configuration-updates), [Current Web payload compatibility](#current-web-payload-compatibility)
  - [Advertise saved server names immediately](#advertise-saved-server-names-immediately), [Verify the live server UI culture](#verify-the-live-server-ui-culture), [Apply the dashboard cache directory](#apply-the-dashboard-cache-directory), [Apply metadata directory changes consistently](#apply-metadata-directory-changes-consistently), [Verify the live Quick Connect toggle](#verify-the-live-quick-connect-toggle), [Apply the library scan fanout limit](#apply-the-library-scan-fanout-limit), [Honor the parallel image encoding limit](#honor-the-parallel-image-encoding-limit), [Verify restart and shutdown](#verify-restart-and-shutdown), [Verify dashboard overview widgets](#verify-dashboard-overview-widgets)
  - [Account management and password changes](#account-management-and-password-changes), [Administrator, disabled and hidden accounts](#administrator-disabled-and-hidden-accounts), [Saved lockout thresholds and concurrent failures](#saved-lockout-thresholds-and-concurrent-failures), [Maximum active sessions](#maximum-active-sessions), [Selected authentication provider](#selected-authentication-provider), [Built-in password recovery and reset-provider selection](#built-in-password-recovery-and-reset-provider-selection), [Per-user remote access](#per-user-remote-access)
- Base commit: 782ca9f28cc938bf15b5a55b425c435673d2f392
- Rust version: 1.98.1
- Checks:
  - Fmt: yes
  - Strict workspace clippy all targets all features: yes
  - Workspace tests:
    - Passed: 7772
    - Skipped: 5
    - Failed: 0
  - Doctests:
    - Passed: 3
    - Failed: 0
  - Per crate line coverage percent:
    - Ferrofin-util: 96.1
    - Ferrofin-common: 87.18
    - Ferrofin-model: 86.86
    - Ferrofin-drawing: 91.41
    - Ferrofin-mediaencoding: 93.74
    - Ferrofin-providers: 93.2
    - Ferrofin-livetv: 93.83
    - Ferrofin-extensions: 94.87
    - Ferrofin-api: 87.19
    - Ferrofin-core: 93.85
- Native browse consistency:
  - Workload: bench/screens.js SHAPE=1
  - Server cpus: 8-15
  - Client cpus: 16-19
  - Counts:
    - MovieCount: 3001
    - SeriesCount: 250
    - EpisodeCount: 7490
    - ArtistCount: 296
    - ProgramCount: 0
    - TrailerCount: 0
    - SongCount: 8000
    - AlbumCount: 800
    - MusicVideoCount: 0
    - BoxSetCount: 0
    - BookCount: 0
    - ItemCount: 0
  - Responses per build: 146
  - Statuses per build:
    - 200: 144
    - 204: 2
  - Identical response shapes: yes
  - Plugins disabled both: yes
  - Latency measured: no
  - Idle rejections percent:
    - 83.8
    - 80.1
    - 23.7
  - Required idle percent: 90
  - Memory measured: no
  - Docker available: no
- Binaries:
  - Before:
    - Sha256: 15584e754010b2eb1a785133d260b0de9b280a3944cfd4cba61b700bb9bdf96f
    - Profile: release
  - After:
    - Sha256: db803c7501101cd6e7a68bd4e96e52a7589fb6d9aebe31c5513a3d04b30cdc24
    - Profile: release

### Second implementation batch

This batch completes **[Allow media playback](#allow-media-playback)–[Download permission and file eligibility](#download-permission-and-file-eligibility) and [Collection management](#collection-management)–[SyncPlay access](#syncplay-access)**, ten more findings after the first
20, on `fix/dashboard-settings`. Each has one signed-off commit without a
co-author trailer. [Media deletion — open dependency](#media-deletion--open-dependency) has a separate dependency review and remains open. The
canonical checklist is `brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md`.

| Findings | Result |
|---|---|
| [[Allow media playback](#allow-media-playback)](#allow-media-playback) | Saved playback permission reaches item PlayAccess; session Play enforcement verified. |
| [[Audio/video transcoding and remuxing permissions](#audiovideo-transcoding-and-remuxing-permissions)](#audiovideo-transcoding-and-remuxing-permissions) | Trusted transcode permissions reach stream planning and separate cached jobs. |
| [[Force transcoding of remote sources](#force-transcoding-of-remote-sources)](#force-transcoding-of-remote-sources) | Remote-source transcoding policy reaches negotiation and stream-copy decisions. |
| [[Internet streaming bitrate limit](#internet-streaming-bitrate-limit)](#internet-streaming-bitrate-limit) | User/server bitrate caps use trusted network classification and affect actual FFmpeg output. |
| [[Download permission and file eligibility](#download-permission-and-file-eligibility)](#download-permission-and-file-eligibility) | Download eligibility agrees with DTOs, and alternate versions download their own bytes. |
| [[Collection management](#collection-management)](#collection-management) | Existing collection-management policy verified over real HTTP. |
| [[Subtitle-management permission](#subtitle-management-permission)](#subtitle-management-permission) | Subtitle management enforces the saved permission; deletion retains upstream administrator restriction. |
| [[Live TV access and management](#live-tv-access-and-management)](#live-tv-access-and-management) | Existing independent Live TV access/management policies verified. |
| [[Remote control and shared devices](#remote-control-and-shared-devices)](#remote-control-and-shared-devices) | Command delivery and session mutations enforce control rights; cast lists honor shared-device and device-access rules. |
| [[SyncPlay access](#syncplay-access)](#syncplay-access) | SyncPlay preserves inherited administrator access and matches empty-user-ID errors. |

The behavior oracle is Jellyfin `v12.0-rc7`, `4910aafa1a`; the Web reference
remains `1e507c588f353482a00333f84e36ddb7c8fc8221`. Several original audit claims
were corrected after tracing upstream authorization and playback code: these
settings do not all impose a universal prohibition on direct stream URLs.

#### Validation

Formatting, strict workspace Clippy with all targets/features, the SQL boundary,
the server build and all **three doctests** pass. Focused tests and native HTTP,
WebSocket and FFmpeg checks are recorded per finding. Final API validation is
**889 tests passed**, with **87.65% line coverage**. Core line coverage is
**90.69%**. Each report excludes other workspace crates and exceeds the 80%
threshold. These are the two non-exempt crates changed in this batch; traits and
the server composition root have the repository's coverage exemption.

**The workspace gate is not green.** Its latest run executed 7,786 tests:
7,781 passed, five failed, and five additional tests were skipped. One failure
was an older malformed-subtitle fixture lacking [Subtitle-management permission](#subtitle-management-permission)'s newly required permission.
That fixture is corrected in [Subtitle-management permission](#subtitle-management-permission)'s commit; all 14 video tests and the entire
889-test API suite subsequently pass. No production code changed after that
workspace run. The remaining four failures are:

- Core `notify_watcher::tests::a_metadata_only_change_of_a_root_is_not_reported`.
- Mediaencoding `transcoding::fs_wait::tests::wakes_on_file_creation_well_before_the_fallback_tick`.
- Mediaencoding `transcoding::fs_wait::tests::change_before_wait_is_not_lost`.
- Server `scan_change_detection::a_scan_reprocesses_only_what_changed`.

The host has exhausted its inotify watch allowance. Direct `inotify_add_watch`
calls on `/tmp` and a fresh temporary directory return `ENOSPC`, including
outside the sandbox. The core and server logs explicitly report the watch
limit; the mediaencoding tests reach their fallback timer instead of receiving
watch events. The separate core coverage run passed 2,072 tests and failed four
watcher tests. Its percentage threshold passes, but its combined test/coverage
command does not. No limits were changed, tests weakened or unrelated processes
stopped. Rerun the workspace and core coverage gates when watch capacity is
available. The earlier [Allow media playback](#allow-media-playback)–[Internet streaming bitrate limit](#internet-streaming-bitrate-limit) checkpoint had seven failures from the same
watcher dependency; variation reflects shared-host availability.

All live checks used disposable accounts, databases and media. Per-finding
before/after timings are recorded as shared-host observations, with no speedup
claim. Docker access remains unavailable and the host cannot supply the quiet
benchmark conditions required for a combined performance result. The previous
batch's functional browse comparison does not measure this batch's latency.

#### Open work

[[Media deletion — open dependency](#media-deletion--open-dependency)](#media-deletion--open-dependency) depends on the existing physical-deletion
workstream and its scanner prerequisites; permission checks alone do not close
it. [Library selection and direct item access](#library-selection-and-direct-item-access) is next: integrate main's visibility merge `f366059b` (PR #43), preserving
the settings fixes and validating their interactions. Its parent `ba619e65` is
itself a JSON-binding merge, not the visibility implementation to cherry-pick.

New findings [Remote `.strm` shortcut playback](#remote-strm-shortcut-playback) (STRM source resolution/probing) and [Audio-specific device-profile negotiation](#audio-specific-device-profile-negotiation) (audio-specific device
profile planning) remain explicit work items in the living document. “Internet streaming bitrate `RemoteClientBitrateLimit`” (open)'s
server bitrate consumer is fixed by [Internet streaming bitrate limit](#internet-streaming-bitrate-limit), but its separate dashboard-section
review is still pending. No other setting is marked complete by implication.

The [validation summary](dashboard-settings-batch-02-validation.json) preserves
test totals, coverage outcomes, native probe counts and timings. Raw local logs
use `/tmp/ferrofin-dashboard-batch02-*`; per-finding live results use
`/tmp/ferrofin-dashboard-u*-results.json`.

#### Validation record

- Completed findings:
  - [Allow media playback](#allow-media-playback)
  - [Audio/video transcoding and remuxing permissions](#audiovideo-transcoding-and-remuxing-permissions)
  - [Force transcoding of remote sources](#force-transcoding-of-remote-sources)
  - [Internet streaming bitrate limit](#internet-streaming-bitrate-limit)
  - [Download permission and file eligibility](#download-permission-and-file-eligibility)
  - [Collection management](#collection-management)
  - [Subtitle-management permission](#subtitle-management-permission)
  - [Live TV access and management](#live-tv-access-and-management)
  - [Remote control and shared devices](#remote-control-and-shared-devices)
  - [SyncPlay access](#syncplay-access)
- Open dependency: [Media deletion — open dependency](#media-deletion--open-dependency)
- Base commit: 785a2423
- Rust version: 1.98.1
- Jellyfin behavior oracle: 4910aafa1a
- Jellyfin web reference: 1e507c588f353482a00333f84e36ddb7c8fc8221
- Checks:
  - Fmt: yes
  - Strict workspace clippy all targets all features: yes
  - Sql boundary: yes
  - Server build: yes
  - Workspace tests:
    - Passed: 7781
    - Failed: 5
    - Skipped: 5
    - Exit code: 100
    - Gate passed: no
    - Before test fixture correction: yes
    - Subsequently corrected failure: ferrofin-api::videos::subtitle_upload_bad_base64_is_400
    - Unresolved watch dependent failures:
      - ferrofin-core::notify_watcher::tests::a_metadata_only_change_of_a_root_is_not_reported
      - ferrofin-mediaencoding::transcoding::fs_wait::tests::wakes_on_file_creation_well_before_the_fallback_tick
      - ferrofin-mediaencoding::transcoding::fs_wait::tests::change_before_wait_is_not_lost
      - ferrofin-server::scan_change_detection::a_scan_reprocesses_only_what_changed
  - Doctests:
    - Passed: 3
    - Failed: 0
  - Final api suite:
    - Passed: 889
    - Failed: 0
    - Skipped: 0
  - Corrected video target:
    - Passed: 14
    - Failed: 0
  - Coverage:
    - Ferrofin-api:
      - Line percent: 87.65
      - Threshold percent: 80
      - Test and coverage exit code: 0
    - Ferrofin-core:
      - Line percent: 90.69
      - Threshold percent: 80
      - Test and coverage exit code: 1
      - Report exit code: 0
      - Tests passed: 2072
      - Tests failed: 4
      - Failure cause: inotify watch capacity exhausted
  - Inotify probe:
    - Outside sandbox: yes
    - Paths:
      - /tmp
      - fresh temporary directory
    - Errno: 28
    - Error: ENOSPC
    - System limits changed: no
- Native checks:
  - [Allow media playback](#allow-media-playback):
    - Before:
      - Probe records: 6
      - Timings:
        - Items play access:
          - Samples: 50
          - Median ms: 1.927
          - P95 ms: 2.91
      - Source: /tmp/ferrofin-dashboard-u08-before-results.json
    - After:
      - Probe records: 6
      - Timings:
        - Items play access:
          - Samples: 50
          - Median ms: 1.998
          - P95 ms: 2.98
      - Source: /tmp/ferrofin-dashboard-u08-after-results.json
  - [Audio/video transcoding and remuxing permissions](#audiovideo-transcoding-and-remuxing-permissions):
    - Before:
      - Probe records: 9
      - Timings:
        - Posted playback info:
          - Samples: 50
          - Median ms: 1.538
          - P95 ms: 1.798
      - Source: /tmp/ferrofin-dashboard-u09-before-results.json
    - After:
      - Probe records: 9
      - Timings:
        - Posted playback info:
          - Samples: 50
          - Median ms: 1.716
          - P95 ms: 2.192
      - Source: /tmp/ferrofin-dashboard-u09-after-results.json
  - [Force transcoding of remote sources](#force-transcoding-of-remote-sources):
    - Before:
      - Probe records: 12
      - Timings:
        - Remote playback info:
          - Samples: 50
          - Median ms: 1.515
          - P95 ms: 2.663
      - Source: /tmp/ferrofin-dashboard-u10-before-results.json
    - After:
      - Probe records: 12
      - Timings:
        - Remote playback info:
          - Samples: 50
          - Median ms: 1.672
          - P95 ms: 2.721
      - Source: /tmp/ferrofin-dashboard-u10-after-results.json
  - [Internet streaming bitrate limit](#internet-streaming-bitrate-limit):
    - Before:
      - Probe records: 10
      - Timings:
        - Remote bitrate playback info:
          - Samples: 50
          - Median ms: 2.492
          - P95 ms: 5.752
      - Source: /tmp/ferrofin-dashboard-u11-before-results.json
    - After:
      - Probe records: 10
      - Timings:
        - Remote bitrate playback info:
          - Samples: 50
          - Median ms: 2.178
          - P95 ms: 5.759
      - Source: /tmp/ferrofin-dashboard-u11-after-results.json
  - [Download permission and file eligibility](#download-permission-and-file-eligibility):
    - Before:
      - Probe records: 12
      - Timings:
        - Download head:
          - Samples: 50
          - Median ms: 1.246
          - P95 ms: 2.075
      - Source: /tmp/ferrofin-dashboard-u12-before-results.json
    - After:
      - Probe records: 12
      - Timings:
        - Download head:
          - Samples: 50
          - Median ms: 0.686
          - P95 ms: 0.758
      - Source: /tmp/ferrofin-dashboard-u12-after-results.json
  - [Collection management](#collection-management):
    - Verified:
      - Probe records: 18
      - Timings:
        - Add existing member:
          - Samples: 50
          - Median ms: 0.762
          - P95 ms: 1.084
      - Source: /tmp/ferrofin-dashboard-u14-verified-results.json
  - [Subtitle-management permission](#subtitle-management-permission):
    - Before:
      - Probe records: 30
      - Timings:
        - Search:
          - Samples: 50
          - Median ms: 0.787
          - P95 ms: 1.255
      - Source: /tmp/ferrofin-dashboard-u15-before-results.json
    - After:
      - Probe records: 30
      - Timings:
        - Search:
          - Samples: 50
          - Median ms: 0.867
          - P95 ms: 1.063
      - Source: /tmp/ferrofin-dashboard-u15-after-results.json
  - [Live TV access and management](#live-tv-access-and-management):
    - Verified:
      - Probe records: 80
      - Timings:
        - Livetv info:
          - Samples: 50
          - Median ms: 0.463
          - P95 ms: 0.795
      - Source: /tmp/ferrofin-dashboard-u16-verified-results.json
  - [Remote control and shared devices](#remote-control-and-shared-devices):
    - Before:
      - Probe records: 52
      - Timings:
        - Pause:
          - Samples: 50
          - Median ms: 0.565
          - P95 ms: 0.698
      - Source: /tmp/ferrofin-dashboard-u17-before-results.json
    - After:
      - Probe records: 52
      - Timings:
        - Pause:
          - Samples: 50
          - Median ms: 0.775
          - P95 ms: 0.952
      - Source: /tmp/ferrofin-dashboard-u17-after-results.json
  - [SyncPlay access](#syncplay-access):
    - Before:
      - Probe records: 21
      - Timings:
        - List:
          - Samples: 50
          - Median ms: 0.593
          - P95 ms: 0.763
      - Source: /tmp/ferrofin-dashboard-u18-before-results.json
    - After:
      - Probe records: 21
      - Timings:
        - List:
          - Samples: 50
          - Median ms: 0.691
          - P95 ms: 1.303
      - Source: /tmp/ferrofin-dashboard-u18-after-results.json
- Performance claim: no
- Combined latency measured: no
- New open findings:
  - [Remote `.strm` shortcut playback](#remote-strm-shortcut-playback)
  - [Audio-specific device-profile negotiation](#audio-specific-device-profile-negotiation)
- Next finding: [Library selection and direct item access](#library-selection-and-direct-item-access)

### Typed dashboard configuration documents

Reviewed and implemented on 2026-10-07, based on main
`782ca9f28cc938bf15b5a55b425c435673d2f392`.

Older partial named configurations could make dashboard fields disappear or show
the wrong state. Malformed stored values were handled differently by section:
encoding returned an error while networking echoed the unusable value.

Core named configuration reads now deserialize through their DTOs, supply defaults
for omitted members, and report malformed documents before returning defaults.
Reading never rewrites the stored file. The existing network aliases, Live TV
tuner/provider projection, non-finite encoding response checks, and plugin-owned
JSON documents retain their behavior.

Main already contained typed named-configuration writes when this worktree was
created. This change completes the typed read path. It closes the transport/default
handling portion of this finding; it does not claim that every persisted setting has a runtime
consumer. Store validation ([Validate and atomically save named configurations](#validate-and-atomically-save-named-configurations)), update propagation ([Publish successful configuration updates](#publish-successful-configuration-updates)), DTO coverage ([Current Web payload compatibility](#current-web-payload-compatibility)), and
the individual settings findings remain separate work.

The reference behavior is Jellyfin v12.0-rc7 (`4910aafa1a`):
`ConfigurationController.UpdateNamedConfiguration` deserializes to the store's
type, and `BaseConfigurationManager.LoadConfiguration` logs deserialization
failures and constructs the type's default instance. Ferrofin keeps its JSON
storage and existing handling of unreadable files.

#### Verification

All 17 tests in `ferrofin-api::config` pass. Regression coverage includes partial
documents for encoding, networking, metadata, NFO, and Live TV; malformed stored
values; canonical network aliases; case-insensitive section names; preservation
of original files; and plugin-owned document shapes. Existing tests also cover
typed writes, rejected writes preserving previous content, syntax/read failures,
and the non-finite-number serialization contract.

Real HTTP checks ran both binaries on loopback against the same disposable data
directory. The fixture contains one administrator and no media. Each probe restored
its original file after reading it. Both binaries also rejected malformed writes
without replacing the previous good document, and round-tripped a plugin-owned
document.

| Stored document / requested field | Before | After |
|---|---|---|
| Metadata `{}` / `UseFileCreationTimeForDateAdded` | 200, field absent | 200, `true` |
| NFO with only `UserId` / `SaveImagePathsInNfo` | 200, field absent | 200, `true` |
| Live TV with only `PrePaddingSeconds` / `SaveRecordingNFO` | 200, field absent | 200, `true` |
| Encoding with a string in `EnableThrottling` | 500 | 200, default `false`, warning |
| Networking with a nonnumeric `InternalHttpPort` | 200, unusable string echoed | 200, default `8096`, warning |

All five stored files remained byte-for-byte unchanged by GET.

Repository gates passed:

- `cargo fmt --all --check`.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- `cargo nextest run --workspace`: 7,722 passed, 5 skipped.
- `cargo test --workspace --doc`.
- `cargo llvm-cov nextest -p ferrofin-api --fail-under-lines 80 --summary-only`,
  with the contributor guide's filename exclusion for other crates/apps:
  871 tests passed; **87.04% API line coverage**.

Cargo builds used `--offline`, two build jobs, and four nextest test threads.
The live checks cover configuration HTTP behavior; no GPU pipelines were exercised.

#### Before/after timings

Authenticated `curl` GETs of `/System/Configuration/{key}`, dev builds of the
baseline and changed server, same host and data directory. Each endpoint received
20 warm-up requests followed by 100 measured requests; `curl`'s `time_total`
includes the loopback connection. Other builds were running, so these numbers
are a local regression check, not a stable performance comparison.

| Section | Median before / after (ms) | p95 before / after (ms) |
|---|---|---|
| Metadata | 1.014 / 1.069 | 2.060 / 1.624 |
| Networking | 1.441 / 1.168 | 3.181 / 1.829 |
| Live TV | 1.549 / 1.484 | 4.337 / 2.330 |
| Encoding | 1.125 / 1.186 | 2.140 / 1.993 |

### Validate and atomically save named configurations

The dashboard's generic named save bypassed `EncodingConfigurationStore`.
It accepted nonexistent transcoding directories and changes to the encoder path,
and truncated the destination before knowing that the replacement would succeed.

The composition root now registers the existing encoding validator. The API
normalizes types, validates against the previous typed configuration, and writes
a private temporary file beside the destination. It flushes the completed file
before renaming it over the destination. Failed writes and renames clean up the
temporary file and preserve the previous destination. Saves share a lock through
the live network update, so concurrent requests cannot leave disk and runtime
policy in different orders. Accepted saves finish even if the client disconnects.

This follows Jellyfin v12.0-rc7 (`4910aafa1a`),
`BaseConfigurationManager.SaveConfiguration`, `EncodingConfigurationStore.Validate`,
and `ExceptionMiddleware.GetStatusCode`: a changed nonblank transcoding path must
exist (404 otherwise); a changed nonblank encoder path is rejected (500).
Unchanged paths and blank resets retain upstream behavior. Atomic replacement
adds failure protection to Ferrofin's JSON storage. This does not wire individual
encoding settings to their runtime consumers; those findings remain separate.

#### Verification

The 19 API configuration tests pass, including validator dispatch, error categories,
unchanged bytes after rejection, temporary-file cleanup after rename failure,
unchanged live policy on failure, and agreement between persisted and effective
network configuration after 20 concurrent saves. The file utility also injects a
mid-write failure and verifies the original bytes survive.

Real HTTP checks used the [Typed dashboard configuration documents](#typed-dashboard-configuration-documents) binary and this change on the same disposable
one-admin, no-media fixture. Before the change, both invalid path changes returned
204 and replaced the file. Afterward, missing directories return 404 and encoder
path changes return 500, with byte-for-byte preservation in both cases. Valid
existing directories, unchanged paths that no longer exist, and blank resets still
return 204. A forced network-file write failure returns 500 and leaves the running
remote-access denial in place (503 for a forwarded remote client).

Formatting, strict workspace Clippy, and the server build pass. Per-crate line
coverage: util **96.22%** (178 tests), common **87.18%** (74 tests),
mediaencoding **93.71%** (917 tests), and API **87.12%** (873 tests).
The API coverage run required socket access for its WebSocket integration test
and rebuilding one malformed instrumented test binary; the final run includes
all tests and profiles. The complete workspace
test suite and doctests will also run after the first 20 findings.

#### Before/after timings

Authenticated curl POSTs on loopback, dev builds, same disposable data directory:
10 warmups followed by 50 samples for each named section. The shared host had
concurrent builds, so these are local regression measurements. Saves now include
semantic validation where registered and a file flush before atomic replacement.

| Section | Median before / after (ms) | p95 before / after (ms) |
|---|---|---|

| Encoding | 1.058 / 1.325 | 3.628 / 1.841 |
| Metadata | 0.917 / 1.502 | 1.608 / 2.917 |
| Network | 1.966 / 2.281 | 3.552 / 5.792 |

### Publish successful configuration updates

The configuration manager now supports synchronous observers for main, branding,
and generic named saves. Observers receive the exact committed configuration;
main settings use an immutable shared document and named settings carry their
normalized key and serialized body. Validation and persistence failures emit no
update. Listeners run outside the document/registry read locks and before the
save returns.

Main and branding writes now use atomic replacement and serialize persistence,
publication, and observer delivery. Their worker owns the writer lock and completes
these steps if the caller disconnects. This extends [Validate and atomically save named configurations](#validate-and-atomically-save-named-configurations)'s failure protection to
the manager-owned stores and prevents concurrent main saves from publishing in
a different order than they reach disk.

Jellyfin v12.0-rc7 (`4910aafa1a`) provides `ConfigurationUpdated` and
`NamedConfigurationUpdated` in `BaseConfigurationManager`. Ferrofin exposes these
through the configuration seam without making API handlers depend on core types.
Individual service subscriptions and their setting behavior are verified in the
subsequent G findings. This infrastructure does not close unrelated consumer gaps.

#### Verification

The 28 configuration-manager tests pass. New regression cases verify observer
snapshots against both the persisted file and current memory during 20 concurrent
saves, no event or live-state replacement on failed writes, normalized named keys,
and the exact persisted branding body. API tests verify successful named writes
notify the manager and rejected or failed saves do not.

The real HTTP check submits 20 concurrent full-document saves, compares disk with
GET, and rejects a missing metadata directory without changing the saved bytes.
Both the before and after builds passed those live checks; this probe did not
reproduce the ordering race in the baseline. The observer regression tests exercise
the new ordering guarantee directly. All 20 API configuration tests, formatting,
strict workspace Clippy, and the server build pass. The complete workspace suite,
doctests, and changed-crate coverage run at the end of the batch.

Authenticated curl POSTs of the full main document used dev builds and the same
one-admin, no-media fixture, with 10 warmups and 50 measured requests. Median
latency was **1.542 → 1.558 ms**, p95 **2.285 → 2.583 ms**. Other builds shared the
host; this is a local regression check, not a stable performance comparison.

### Current Web payload compatibility

The library provider registry computed similarity-provider choices, but the wire
DTO omitted `SimilarItemProviders`. The current Web library editor therefore had
no choices to display. The response now includes the registered providers and
their default-enabled state, matching `LibraryTypeOptionsDto` in Jellyfin
v12.0-rc7 (`4910aafa1a`) and the reviewed Web checkout.

The old suppression was tied to the vendored 10.11.8 response schema, which predates
this property. The server's behavioral parity pin is v12.0-rc7; this response now
follows it. The vendored contract files and route-superset requirements are intact.

Eight payload fixtures use the member names, nesting, and form value types emitted
by Web `1e507c588f353482a00333f84e36ddb7c8fc8221`. The API regression tests post and
read back server, branding, encoding, network, metadata, NFO, Live TV and library
settings. Library tests cover both creation and ID-based updates, including nested
provider choices and image options. Fixture provenance and maintenance instructions
are in `crates/ferrofin-api/tests/fixtures/dashboard-settings.md`.

Web still emits one obsolete constant, `EnableArchiveMediaFiles: false`; there is
no control for it and no property in Jellyfin v12's `LibraryOptions`. The fixture
keeps that actual input, while its readback check explicitly excludes the discarded
constant. No new capability is implied by storing a compatibility field.

This finding covers transport and choice visibility. Individual settings whose
values persist but lack consumers remain open in the living review.

#### Verification

All 21 configuration and 18 library-structure API tests, 15 library-options
provider tests, and 21 model configuration tests pass. Formatting, strict workspace
Clippy, and the server build pass. Compilation now uses a worktree-local target
cache after mixed system/rustup artifacts were found in the shared cache.

Real HTTP before/after checks used the same disposable fixture. Before the change,
Movie's `SimilarItemProviders` was absent. Afterward, it contains `Local Genre/Tag`
(default enabled) and `TheMovieDb` (default disabled). Both builds round-trip all
40 checked current library fields through actual creation/update and readback.
Workspace tests, doctests, and per-crate coverage run at the end of the batch.

Authenticated curl GETs of `/Libraries/AvailableOptions?libraryContentType=movies&isNewLibrary=true`
used dev builds, 10 warmups and 50 samples. Median latency was **0.545 → 0.595 ms**,
p95 **1.181 → 1.366 ms**. These are local regression measurements on a shared host.

## General and overview

### Advertise saved server names immediately

The application host now subscribes to committed server-configuration updates.
Saving `ServerName` updates the cached friendly name before the save completes;
public/private system information and the next discovery response use it without
a restart. The subscription holds a weak host reference so restarting can release
the old host and its configuration manager.

Jellyfin v12.0-rc7 (`4910aafa1a`), `ApplicationHost.FriendlyName`, falls back to the
machine name for an empty name and preserves whitespace-only names. Ferrofin now
matches that distinction instead of trimming whitespace to empty.

#### Verification

All 14 application-host tests and five real UDP discovery tests pass. The discovery
regression now saves through the configuration manager without manually refreshing
the host. Host tests cover new names, empty-name fallback, whitespace preservation,
unrelated named updates, and releasing a stopped host. Formatting, strict workspace
Clippy, and the server build pass.

Real HTTP before/after checks used the same disposable fixture. The baseline kept
advertising `ferrofin` after saving a new name. The changed server immediately
returns the new name from both `/System/Info/Public` and `/System/Info`, preserves
whitespace exactly, and returns to the fallback when the saved name is empty.

Authenticated curl GETs used dev builds, 10 warmups and 50 samples per endpoint.
The shared host was running other work, so these local measurements are noisy:

| Endpoint | Median before / after (ms) | p95 before / after (ms) |
|---|---|---|
| `/System/Info/Public` | 1.033 / 1.590 | 2.107 / 2.422 |
| `/System/Info` | 1.909 / 1.233 | 4.193 / 2.821 |

The complete workspace suite, doctests and per-crate coverage run at the end of
the first 20 findings.

### Verify the live server UI culture

`UICulture` was already connected to a live configuration reader. This change adds
regression coverage showing that an existing localization service observes a
saved culture immediately and recomputes media-stream labels/display titles.
No production behavior changed.

The tested server-default lookup uses the configured culture, then English, then
the phrase key. This follows the fallback rules in Jellyfin v12.0-rc7's
`LocalizationManager` (`4910aafa1a`). Explicit culture lookups retain precedence
over the server default. Web's own client-language preference is a separate
surface from this server setting.

#### Verification

The new media-stream regression and all 38 localization tests pass. Formatting,
strict workspace Clippy, and the server build pass.

Real HTTP checks generated and scanned a two-second video with one default audio
stream, with remote metadata/image providers disabled. Without restarting, saving
`UICulture=de` produces `Standard`, `fr` produces `Par défaut`, and an unknown or
empty culture produces `Default`. Both `LocalizedDefault` and the composed
`DisplayTitle` change on the next playback-info request. The before and after
builds both pass, confirming the original supported classification.

For the same disposable media fixture, authenticated curl playback-info GETs used
10 warmups and 50 samples. Median latency was **1.623 → 1.542 ms**, p95
**2.637 → 3.115 ms**. These are noisy shared-host measurements of unchanged
production code, not a performance improvement claim. Full workspace tests,
doctests and per-crate coverage run at the end of the first 20 findings.

### Apply the dashboard cache directory

Saved `CachePath` now overrides the startup cache directory when loading the
configuration and when replacing an override. Existing services resolve the
current directory at the start of an operation: resized images, OMDb records,
similarity responses, Schedules Direct country files, frame-extraction scratch
files, Live TV buffers, and extension fingerprints/scratch files. Path-manager,
playback-planner, storage and maintenance consumers already read shared paths.
An operation retains its chosen destination while it runs; existing files are
not moved or removed.

This follows Jellyfin v12.0-rc7 (`4910aafa1a`),
`BaseConfigurationManager.UpdateCachePath` / `ValidateCachePath` and
`BaseApplicationPaths.CreateAndCheckMarker` / `CreateCacheDirTag`.
Changed nonblank paths must exist and accept a temporary write. Conflicting
`.jellyfin-*` markers are rejected and successful roots receive `.jellyfin-cache`
and the standard `CACHEDIR.TAG` signature. Validation and preparation happen
before configuration persistence/publication, so failures preserve saved and live
settings.

Upstream retains the live cache root when an override is cleared; restarting then
restores the startup CLI/environment/default root. Ferrofin preserves this rule
and additionally sets `HasPendingRestart` when clearing requires a restart.

#### Verification

All 208 focused tests pass across configuration/path management, image processing,
provider caches, frame extraction, Live TV and extensions. Regressions cover live
cache replacement, persisted overrides, clearing/reloading, rejected paths,
backup tags, real image writes and a live OMDb cache switch. Formatting, strict
workspace Clippy and the server build pass.

Real HTTP checks on the same disposable scanned-video fixture confirm:

- Both successive cache overrides change `/System/Info.CachePath` immediately;
  the next resized profile-image request creates a file beneath the new root.
  Before the fix both overrides were ignored and neither root received a file.
- Missing paths return **404**; read-only paths and conflicting directory markers
  return **500**. All three preserve the previous `system.json` bytes. Previously
  all returned **204** and replaced the saved configuration.
- Clearing retains the second override and sets `HasPendingRestart=true`.

Cached resized-image GETs used 10 curl warmups and 50 measured requests. Median
latency was **2.068 → 1.502 ms**, p95 **8.574 → 3.228 ms**. These are noisy local
measurements, not an improvement claim. The batch's final validation will include
workspace tests, doctests, per-crate coverage and the browse benchmark for the
combined changes to request paths.

### Apply metadata directory changes consistently

`MetadataPath` now reaches existing scanner artwork and external-stream readers,
dynamic images, user-profile uploads, item-image uploads, lyrics, subtitle
fallback uploads and user-view creation through the live application path.
Existing path-manager consumers already resolve the current root. Changing a root
controls subsequent operations; it does not relocate previously saved files.

Path validation now follows Jellyfin v12.0-rc7 (`4910aafa1a`),
`ServerConfigurationManager.ValidateMetadataPath` / `UpdateMetadataPath`:
changed nonblank directories must exist and accept a temporary write, while
startup/default/unchanged paths are created if absent. Missing changed paths
return 404; write failures return 500. Preparation occurs before configuration
persistence and publication. Clearing resets the metadata directory immediately
to its default, unlike clearing the cache override in [Apply the dashboard cache directory](#apply-the-dashboard-cache-directory).

The image-deletion guard also recognizes shared album artwork in an earlier
metadata root. A root change must not make deleting one track's image delete an
asset still referenced by other tracks.

#### Verification

All 394 focused configuration, metadata-service and scanner tests pass. Changed
regressions exercise a live profile-image directory switch, subtitle fallback
writes, lyric downloads/readback, read-only directory rejection, clearing to the
default, and retaining shared album art after changing roots. Formatting, strict
workspace Clippy and the server build pass.

The same disposable scanned-video fixture was exercised over real HTTP on both
builds. Before the fix, two successive metadata overrides changed system
information but received **zero** profile-image, item-image or subtitle files.
After the fix, each received all three uploads immediately. Clearing sent the
next uploads to the default root. A missing path now returns **404** instead of
400; a read-only directory returns **500** instead of a successful 204. Both
rejections preserve the previous configuration file.

Saving an identical server-configuration document, on the same fixture, used 10
curl warmups and 50 measured POSTs per build. Median latency was **3.447 →
1.260 ms**, p95 **11.163 → 2.331 ms**. Shared-host noise is substantial; these
numbers are not an improvement claim. Playback-info timings taken during the
upload probes are not used for comparison because each probe added subtitle
streams. Final workspace tests, doctests, coverage and the combined browse
benchmark remain batch-end gates.

### Verify the live Quick Connect toggle

`QuickConnectAvailable` already has a correct live reader. Added a regression
using the real configuration manager to save the setting while a Quick Connect
manager holds pending and authorized requests. No production code changed.

Compared with Jellyfin v12.0-rc7 (`4910aafa1a`),
`QuickConnectManager.IsEnabled` / `AssertActive`: disabling refuses initiation,
status polling, authorization and secret exchange immediately. Re-enabling allows
unexpired pending/authorized requests again. The setting gates pairing operations;
it does not revoke sessions already issued through Quick Connect.

#### Verification

All six Quick Connect manager tests, formatting, strict workspace Clippy and
the server build pass. Real HTTP checks use separate administrator and pairing
devices. Both builds return **401** for all disabled pairing operations,
including exchange after authorization, and **200** after re-enabling. A token
issued before disabling still authenticates `/Users/Me`.

For the same disposable fixture, `GET /QuickConnect/Enabled` used 10 curl warmups
and 50 samples: median **0.408 → 0.550 ms**, p95 **0.761 → 2.187 ms**. Production
code is unchanged; this spread reflects shared-host noise. Final workspace tests,
doctests and coverage remain batch-end gates.

### Apply the library scan fanout limit

`LibraryScanFanoutConcurrency` now controls a shared scanner budget for folder
resolution and speculative media probes. Positive values are exact ceilings;
nonpositive values use `max(1, usable processors - 3)`, matching Jellyfin
v12.0-rc7 (`4910aafa1a`), `LimitedConcurrencyLibraryScheduler`. The production
reader observes committed configuration updates. Existing work drains after a
limit reduction; subsequent admission uses the new ceiling.

Nested folder work shares the same budget and proceeds inline when saturated.
Each worker owns its naming/context state; results and deletion guards merge in
input order. Workers inherit series/season ownership so extras retain their
owners. Cancellation joins folder workers and stops before writes or pruning.
The ordered foreground metadata/refresh lane reserves one slot, while speculative
probes acquire additional slots for their actual lifetime. The separate ffprobe
ceiling still applies. Metadata persistence remains ordered; this change does not
add parallel remote-provider refresh or close the separate
`LibraryMetadataRefreshConcurrency` finding.

#### Verification

All 280 scanner-related tests, formatting, strict workspace Clippy and the server build pass.
Regressions cover live limit changes, processor-count defaults, shared nested
limits, cancellation, exact serial/parallel plans across library types, and
probe-to-item matching at limits 1, 2 and 4. The original serial planner and
unconfigured probe-window regressions also pass.

A real HTTP save followed by scanning 16 fresh videos at each setting on the
same disposable fixture changed the observed ffprobe peak from **8 → 1** at
limit 1, and **8 → 3** at limit 4. All 16 items in each pass had valid playback
metadata. Limit 4 leaves room for the ordered foreground work. Scan wall times
(including polling/readback) were **0.422 → 0.810 s** and **0.391 → 0.529 s**:
restricting concurrency intentionally trades throughput for resource usage.
The after fixture also retained the earlier scanned items; these are smoke
measurements, not a throughput benchmark. Final workspace tests, doctests,
per-crate coverage and the combined browse benchmark are batch-end gates.

### Honor the parallel image encoding limit

`ParallelImageEncodingLimit` now creates the two startup pools used by Jellyfin
v12.0-rc7 (`4910aafa1a`): resized-image encodes have their own pool, while audio
artwork, video thumbnails and trickplay extraction share another. Positive
values are exact limits; zero and negative values use the usable CPU count.
Cached images and passthrough responses do not acquire an encoding slot.

The pools retain their startup limits, matching the upstream constructors.
Saving a different value flags `HasPendingRestart`; restarting reconstructs the
services with the saved value. A disconnected resized-image request retains
its permit until blocking pixel work actually finishes. Cancelled media jobs
use the existing kill-on-drop process runner and release their pool permit.

#### Verification

All 837 drawing/media-encoding tests and the startup default/explicit-limit test
pass, as do formatting, strict workspace Clippy and the server build. New
regressions verify limits 1 and 2 during blocking pixel work after request
cancellation, plus audio/video/trickplay sharing and cancellation release.
Existing cache-hit, fallback, frame extraction and trickplay retry cases pass.

Real HTTP saves now flag a pending restart (**false → true** compared with the
baseline); `POST /System/Restart` reloads the saved limit and clears the flag.
Sixteen uncached resizes of the same 1600×1000 JPEG, using eight HTTP clients,
all returned the requested dimensions. Wall time at limit 1 was
**1.748 → 12.403 s**, and at limit 4 **1.905 → 3.148 s**. The baseline ignored
both values and ran all requests concurrently; the intended resource restriction
now changes throughput. These debug-build smoke timings are not production
capacity estimates. Final workspace tests, doctests, coverage and the combined
browse benchmark are batch-end gates.

### Verify restart and shutdown

The existing lifecycle implementation is supported. `POST /System/Restart`
drains HTTP, tears down lifetime-owned background work, cancels scheduled jobs,
stops/joins the scanner and closes the database before constructing another
host. `POST /System/Shutdown` follows the same teardown and exits. HTTP draining
has a configured deadline so a long-lived connection cannot prevent restart
indefinitely. Restart-required state alone does not cause a signal or shutdown
to restart the process.

Expanded the real-server restart regression to save UI culture, Quick Connect
availability and the image encoding limit. The next lifetime preserves the full
configuration, applies Quick Connect's saved value, clears the restart flag,
and accepts the existing session token. The existing test also verifies stable
server identity, working metrics, discovery socket release/rebinding and a
clean shutdown. The backup-restore test verifies that teardown completes before
the next lifetime opens the restored database.

Both real HTTP/UDP integration tests pass, along with formatting and strict
workspace Clippy. Production code is unchanged. Final workspace tests, doctests
and coverage remain batch-end gates.

### Verify dashboard overview widgets

The overview services are supported. Added a regression proving both system-info
and storage DTOs resolve effective cache, image-cache, metadata and default
transcode paths after saved directory changes. Clearing metadata restores its
default immediately; clearing cache continues reporting the current root until
restart. This closes the widget verification; encoding-specific destination
controls remain in their own findings.

All 56 focused system/session/activity tests, formatting, strict workspace Clippy
and the server build pass. Real HTTP checks on both builds verify:

- The fixture's authenticated device appears in `/Sessions` with its user and
  effective client address.
- A real scan reports Running, progress from 0 through approximately 99%, then
  Idle with a Completed execution record; all 16 new videos have playback data.
- The activity widget contains actual authentication/session entries.
- `/System/Info` and `/System/Info/Storage` agree on changed paths. Storage includes
  the configured library location and real nonzero filesystem usage.

Production code is unchanged. On the same disposable fixture, 10 warmups and
50 storage requests measured median **1.728 → 1.464 ms**, p95
**2.328 → 3.018 ms**. This is ordinary shared-host spread, not an optimization.
Final workspace tests, doctests and per-crate coverage remain batch-end gates.

## Branding

### The splashscreen preview, formats and first upload

`GET /Branding/Splashscreen` had three defects around the dashboard's custom
splashscreen page, and a fresh install could not upload one at all.

Source oracle: Jellyfin `4910aafa1a`, `ImageController.GetSplashscreen`,
`UploadCustomSplashscreen` and `GetImageResult`.

#### Behavior

- **Administrator preview.** Upstream is `!SplashscreenEnabled &&
  !User.IsInRole(Administrator)` → 404. Ferrofin refused everyone while the
  splashscreen was disabled, so the dashboard could not preview an uploaded
  image before enabling it. The route now reads the optional caller; an API key
  or a user whose policy is administrator gets the file, anyone else (signed in
  or anonymous) still gets 404. The role test is shared with `RequireAdmin`.
- **`format` and `tag`.** The route used to serve the stored bytes untouched,
  ignoring both. It now goes through the same tail as every image route: the
  image processor converts to the requested `format` (an explicit format wins,
  otherwise the client's `Accept` list), and a `tag` earns
  `Cache-Control: public, max-age=31536000, immutable` and an `ETag`
  (`GetImageResult` with a 365-day cache duration), including `304` on a
  matching `If-None-Match`.
- **First upload.** The upload wrote to `<data>/splashscreen-upload.<ext>`
  without creating `<data>`, which does not exist in a fresh Ferrofin data
  directory, so the first upload failed with a 500 (`No such file or
  directory`). The directory is now created first (Jellyfin's data path exists
  from startup).

#### Verification

- API tests (`branding.rs`, with caller stubs for key, regular user and
  anonymous): a disabled splashscreen is served to the key and 404s for a
  regular user and an anonymous caller; enabled, everyone gets it; an untagged
  response is `public`, a tagged one immutable with its ETag. The existing
  upload/get/delete, disabled, missing-file and bad-content-type tests pass.
- Production binary, previous baseline (data directory pre-created so its upload
  works) vs updated, a 16×16 red PNG uploaded with the splashscreen disabled:

  | | admin GET | anonymous GET | `?format=jpg` | `?tag=abc` caching |
  |---|---|---|---|---|
  | before | **404** | 404 | 404 | 404 |
  | after | **200** | 404 | **200 `image/jpeg`** (`ffd8ff…`) | **immutable** + `"abc"` |

  Before, the disabled flag hid format and caching behavior too; the code ignored
  both regardless. On a fresh data directory the previous binary's upload
  returned 500; the updated one returns 204.

#### Limitations

- The upload still writes the decoded bytes without validating that they are an
  image, as upstream does.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on api passed: api 931, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
the field order of a struct constructor; it was fixed and Clippy and the
branding tests rerun clean, while the package and coverage results were taken
just before. Line coverage over api's own sources: **87.81%**, above the 80%
gate; traits and server composition are coverage-exempt. The instrumented build
took `target` to 36.7 GiB, above the 30 GiB cap; its cache was removed right
after the numbers were taken, leaving 24.2 GiB with about 494 GiB free.

## Users: profile and permissions

### Account management and password changes

Password changes now preserve the requesting session while revoking other
sessions, matching Jellyfin's UserController. Previously the current token was
revoked too. Account name, configuration and password edits now enforce
EnableUserPreferenceAccess for ordinary users, and accept elevated API keys.
Administrator reset and deletion keep their upstream behavior. Forgotten-password
provider selection and recovery are covered separately by [Built-in password recovery and reset-provider selection](#built-in-password-recovery-and-reset-provider-selection).

All 16 API user integration tests, formatting, strict workspace Clippy and the
server build pass. Real HTTP checks on the same disposable fixture verify account
creation, rename, case-insensitive duplicate rejection, wrong-current-password
rejection, password change, admin reset to an empty password and deletion.
The current session changes from erroneous 401 to 200 after its password change;
other sessions still return 401. With preferences disabled, all three self-edit
operations now return 403 instead of 204. API-key account edits now return 204
instead of 403. Deleting the account invalidates its token.

Ten warmups and 50 `/Users/Me` requests measured median **0.458 → 0.391 ms**,
p95 **0.592 → 0.613 ms**; this is shared-host noise, not a performance claim.
Final workspace tests, doctests and per-crate coverage remain batch-end gates.

### Administrator, disabled and hidden accounts

Disabled users are now rejected when resolving existing device tokens. Previously
the dashboard disable action revoked tokens, but automatic lockout left those
tokens usable. The shared authorization context now carries the saved user policy
and rejects disabled users on both cold and cached resolutions. Policy changes
invalidate it through the existing generation-protected cache. Warm requests share
the policy by Arc and require no additional database reads; cold resolutions
assemble the policy through UserManager.

All 57 focused authorization/cache/user-manager tests, formatting, strict workspace
Clippy and the server build pass. Real HTTP on the same disposable fixture verifies
hidden users disappear from public listings but can log in by name, administrator
promotion/demotion immediately changes access to `/Devices`, and manual disable
rejects both tokens and new logins. Automatic lockout now changes the existing
token's `/Users/Me` response from erroneous **200 to 401**, without a restart.

Ten warmups and 50 `/Users/Me` requests measured median **0.530 → 0.493 ms**,
p95 **0.692 → 0.659 ms**. These small differences are shared-host noise. Final
workspace tests, doctests and per-crate coverage remain batch-end gates.

### Saved lockout thresholds and concurrent failures

Authentication now serializes each user's credential check and counter update,
reloading the user inside the lock as Jellyfin does. Concurrent policy saves share
that lock; unrelated users have separate locks, and idle lock entries are weak.
Counter changes invalidate cached user data. Threshold -1 remains unlimited and
0 remains the default of 3; other negative thresholds now trigger lockout on the
first failure instead of silently becoming unlimited.

All 63 focused authorization/cache/user-manager tests, formatting, strict workspace
Clippy and the build pass. Real HTTP on the same fixture verifies saved thresholds
0, -1, -2 and 2, rejection after lockout, and successful administrator unlocks.
Most decisively, 24 simultaneous failures now record **24 attempts instead of 3**.
The burst took **19.343 → 36.005 ms**; serialization performs the previously lost
updates and intentionally prevents same-account authentication races. These are
empty-password fixture accounts, isolating bookkeeping from password hashing.

Final workspace tests, doctests and per-crate coverage remain batch-end gates.

### Maximum active sessions

Session admission already correctly enforces positive limits across concurrent
logins. Its rejection now returns **403**, matching Jellyfin's SecurityException,
instead of incorrectly treating the valid credentials as a 401.

All 46 session-manager tests, formatting, strict workspace Clippy and the build
pass. New regressions and real HTTP verify that -1 and 0 admit all eight attempted
sessions; limits 1 and 2 admit exactly one and two. At capacity, even logging in
again on the same device is rejected before token replacement, matching upstream.
Lowering a cap keeps existing sessions usable and rejects additional logins.

On the same disposable fixture, eight-login bursts measured **11.874 → 12.713 ms**
at -1, **8.225 → 8.477 ms** at 0, **6.994 → 8.991 ms** at 1, and
**6.630 → 8.822 ms** at 2. The only production change is the error category;
these small shared-host differences are not a performance claim. Final workspace
tests, doctests and per-crate coverage remain batch-end gates.

### Selected authentication provider

The saved provider now governs password changes and resets as well as login.
External providers own their credential store; an unavailable provider no longer
silently overwrites the local password. Enabled-provider selection remains
case-insensitive and unknown IDs fail authentication. Successful authentication
records the provider identity and reloads provider changes; newly provisioned
users are resolved by the returned canonical name and receive the provider's
new-user policy. The server currently registers the built-in provider; this does
not add native .NET plugin loading.

All 48 focused user/provider tests, formatting, strict workspace Clippy and the
build pass. Registered-provider fakes verify enabled/disabled choices, password
dispatch, provisioning and policy assignment. Real HTTP verifies the built-in
choice with case variants, an empty selection, unknown-ID rejection, and restoring
the built-in choice after a password operation against an unavailable provider.
An empty selected ID now becomes the successful provider's ID; the unavailable
provider no longer changes the local password.

Single cold login observations on the same fixture were **4.878 → 11.542 ms**
(case variant), **5.268 → 17.432 ms** (empty selection), and
**2.099 → 5.701 ms** (missing provider). Builds/coverage were running on the shared
host; these are functional-run timings, not stable latency estimates. The login
path now reloads provider changes and persists a newly selected provider. Final
workspace tests, doctests, per-crate coverage and the combined browse measurement
remain batch-end gates.

### Built-in password recovery and reset-provider selection

The reset-provider selector has one registered choice. Jellyfin also falls back
to its built-in provider for empty or unknown saved IDs, so adding a registry of
unavailable providers would not fix this setting. Verified all three selections
against the actual built-in recovery flow and corrected its behavioral gaps.

Only a known user on the effective local network now receives a PIN file. Unknown,
blank and remote requests receive the same PinCode response shape without creating
a file. Paths use the invariant-uppercase username hash under ProgramDataPath,
matching Jellyfin. Files are atomically replaced with mode 0600; PINs are no longer
logged. Comparison ignores dashes but preserves case. The exact entered PIN becomes
the password, a failed password update preserves the file, successful recovery
records password-change activity, and serialized redemption consumes it once.
Expired records are removed; unmatched/replayed PINs return 404.

All 21 focused API user tests, formatting, strict workspace Clippy and the build
pass. Tests exercise failed password storage, missing users, expiration, concurrent
redemption and file permissions. Real HTTP on the same fixture verifies provider
fallback, uniform unknown/blank responses, lowercase rejection, dashless-password
login, replay rejection and remote requests through a configured trusted proxy.
Remote requests previously created files; they no longer do.

One full PIN redemption (including password hashing) measured
**4088.463 → 2218.474 ms** while builds/coverage ran on the shared host. This is a
functional-run observation, not an optimization claim. Final workspace tests,
doctests and per-crate coverage remain batch-end gates.

### Per-user remote access

The original audit overstated support: ordinary authenticated requests and
password login ignored `EnableRemoteAccess`. Only selected permission policies
enforced it. Existing tokens also bypassed access schedules on ordinary routes.

Default authorization now checks the effective client address against the live
network configuration and enforces remote access before the administrator
exception. Password login shares that network manager, and public user listings
hide remote-disabled accounts from remote clients after setup. The policy uses
[Administrator, disabled and hidden accounts](#administrator-disabled-and-hidden-accounts)'s cached user snapshot, so warm authorization adds no policy database read.
Schedules are evaluated at request time rather than only when issuing a token.

Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`, particularly
`DefaultAuthorizationHandler`, `ApiServiceCollectionExtensions`,
`FirstTimeSetupRequirement`, `UserController` and `UserManager`. Preserved the
explicit schedule exemptions for user details and system information, setup
access, unrestricted API keys, and bare elevation/local-access policies.
Configuration and item-lookup actions combine their controller's default policy
with elevation, so those admin actions still enforce remote access.

All **881 API tests**, **71 focused core tests**, formatting, strict workspace
Clippy and the server build pass. Regressions cover administrator ordering,
schedule exemptions, trusted forwarding, live LAN changes, and password login.
Handler-only test fakes now carry an ordinary authenticated identity and policy;
userless credentials are refused before protected handlers run.

Real HTTP before/after on the same disposable fixture verified:

| Probe | Before | After |
|---|---|---|
| Remote-disabled user's existing token | 200 | 403 |
| Remote-disabled user's password login | 200 | 403 |
| Visible remote-disabled user in remote public list | Included | Omitted |
| Ordinary token outside saved access schedule | 200 | 403 |
| User details and system information outside schedule | 200 | 200 |
| Remote-disabled admin on a default-policy route | 200 | 403 |
| Same admin on bare elevation route (`/Devices`) | 200 | 200 |
| Saved LAN changes and ignored untrusted forwarding | Pass | Pass |

Fifty warm remote `GET /Users/Me` requests after ten warmups measured median
**0.835 → 0.792 ms**, p95 **1.705 → 1.772 ms**. Builds and coverage were running
on the shared host; these observations establish the exercised path, not an
optimization claim. See [batch verification](#first-implementation-batch)
for final workspace gates, per-crate coverage, browse consistency checks and the
combined latency-measurement limitation.

### Allow media playback

Item DTOs now derive requested `PlayAccess` from the saved `EnableMediaPlayback`
permission instead of always returning `Full`. The existing content-permissions
query supplies the flag once per page. Disabled playback yields `None`, including
for administrators, and saved changes take effect on the next request.

The original audit described a stronger boundary than Jellyfin implements.
Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`, `BaseItem.GetPlayAccess`,
`DtoService`, and `SessionManager.SendPlayCommand`: this permission controls item
play access and session Play commands. It does not itself reject PlaybackInfo,
direct stream or HLS requests. Ferrofin's session Play check was already present;
jellyfin-web also checks the user's flag before playing. This setting is not a
server-side ban on fetching media through known URLs.

Formatting, strict workspace Clippy, the server build, the SQL boundary check,
and **174 focused core tests** pass. Added real-database regressions cover single
and page DTOs, live saved policy changes, ordinary users and administrators,
multiple item types, omitted fields, and requests without a user.

Real HTTP before/after on the same generated-media fixture verified six saved
policy combinations: both user types toggled off/on/off. Previously every item
and page advertised `Full`; now they return `None`/`Full`/`None`. PlaybackInfo
remains HTTP 200, matching the upstream boundary.

Fifty warm item-page requests after ten warmups measured median **1.927 → 1.998 ms**,
p95 **2.910 → 2.980 ms** on the shared host. These observations verify the exercised
path and do not establish a performance change.

### Audio/video transcoding and remuxing permissions

PlaybackInfo now reapplies the resolved user's saved permissions after device
profile negotiation. Audio requires audio-transcoding permission; video can
advertise the negotiated HLS path when at least one of audio transcoding, video
transcoding or remuxing is allowed. A client transcoding veto still wins. An
administrator requesting playback for another user uses that user's policy.

The authenticated policy snapshot now reaches every HLS planning route,
including segment requests, and the progressive-stream fallback. Video requests
force video/audio copy independently when the corresponding permission is off,
even when a client requests encoding or incompatible output. The cache key
includes the saved permissions so the same URL cannot reuse an encoder job
created under a different policy.

Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`: `MediaSourceManager`,
`MediaInfoHelper.SetDeviceSpecificData`, `StreamingHelpers.GetStreamingState`
and `EncodingHelper.TryStreamCopy`. The original audit's proposed hard HTTP
boundary was too broad: upstream remux permission controls negotiation, and
`TryStreamCopy` runs only for video requests. A direct audio-only URL does not
invoke that policy check. Userless API keys retain their existing behavior;
there is no administrator exception to a user's encoding restrictions.

Regressions cover all eight saved permission combinations, client vetoes,
audio-versus-video negotiation, each planning route, query-policy spoofing,
actual encoder arguments, cache separation and the upstream audio-only exception.

Formatting, strict workspace Clippy, the SQL boundary gate, the server build and
**103 focused API/planner tests** pass. Real HTTP checks on the same generated
H.264/AAC fixture exercised all eight policies and a repeated all-disabled case:

- Before: every policy advertised a transcode URL and the same segment URL kept
  serving the original video/audio encoding job.
- After: all-disabled negotiation advertises neither transcoding nor a transcode
  URL; other combinations retain the permitted HLS path. Actual FFmpeg logs and
  served segments confirm independent video/audio copy restrictions, including
  remux-only users and reuse after toggling back to an earlier policy.

Fifty warm posted PlaybackInfo requests after ten warmups measured median
**1.538 → 1.716 ms**, p95 **1.798 → 2.192 ms**. These are shared-host observations,
not a performance claim. Negotiation reuses the cached caller policy and reads
the item's media type once; an administrator acting for another user resolves
that user's policy. HLS planning adds no policy database read.

### Force transcoding of remote sources

Playback negotiation now consumes `ForceRemoteSourceTranscoding`. A remote
source cannot advertise direct play/stream when the flag is enabled, and its
delivery URL disables both video and audio copying. Local sources retain their
normal decisions. Saved changes use the policy cache's existing invalidation.
The output cache also distinguishes copy vetoes so changed delivery requests
cannot reuse the opposite encoding mode.

Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`,
`MediaInfoHelper.SetDeviceSpecificData`: remote means the source's `IsRemote`,
not the client's network location. Upstream preserves the negotiated container
and protocol in its direct-play branch, including an HTTP `Static=true` URL.
That behavior is covered explicitly; this flag does not universally guarantee
encoding or authorize operations forbidden by [Audio/video transcoding and remuxing permissions](#audiovideo-transcoding-and-remuxing-permissions)'s permissions.

Formatting, strict workspace Clippy, the SQL boundary gate, the server build and
**104 focused API/planner tests** pass. Regressions cover local/remote sources,
off/on/off changes, direct and HLS decisions, encoding permission restrictions,
and separation of copied and encoded cache files.

The live fixture also exposed an independent source-resolution gap, recorded as
**[Remote `.strm` shortcut playback](#remote-strm-shortcut-playback) (open)** in the living review: scanned `.strm` files remain local sources
without playable remote metadata. The next implementation must resolve remote
shortcut targets, propagate their protocol/`IsRemote`, and probe them on demand,
while preserving upstream's rejection of local-file shortcut targets. This finding's
real HTTP verification uses the existing remote M3U tuner-source implementation.

Real HTTP before/after on the same tuner fixture verified twelve combinations:
remote/local sources, direct-play requested/vetoed, and the flag off/on/off.
Previously neither URL copy veto was emitted; now both appear only while the
remote source's flag is enabled, and disappear immediately when it is disabled.
Fifty warm requests after ten warmups measured median **1.515 → 1.672 ms**,
p95 **2.663 → 2.721 ms** on the shared host, without a performance claim.

Tracing also identified **[Audio-specific device-profile negotiation](#audio-specific-device-profile-negotiation) (open)**: audio-specific device-profile selection
has not been ported; negotiation always uses the video StreamBuilder entry point.
The living document records the required audio profile/URL port and validation.

### Internet streaming bitrate limit

PlaybackInfo now combines the client's explicit limit with the effective saved
remote limit. A positive user `RemoteClientBitrateLimit` replaces the server
default; zero/negative user values inherit it. The smaller client/remote limit
wins outside the LAN. LAN membership uses the live network configuration and
the effective client address normalized through trusted proxies. Saved user,
server, proxy and LAN changes take effect on the next request.

Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`,
`MediaInfoHelper.GetMaxBitrate`. This is negotiation, not traffic shaping or a
second permission gate on arbitrary stream URLs. Upstream preserves explicit
nonpositive client limits; those values are covered without changing their
meaning. Device-profile defaults remain the builder's fallback when negotiation
supplies no explicit limit. [Audio/video transcoding and remuxing permissions](#audiovideo-transcoding-and-remuxing-permissions)'s forced-copy restrictions still take precedence
over encoding a stream to a lower bitrate.

The negotiated video/audio limits already reach HLS and FFmpeg. Their values now
also distinguish cache files, preventing a lower-cap request from reusing output
encoded under an earlier cap with the same session/codec tuple.

Formatting, strict workspace Clippy, the SQL boundary gate, the server build and
**106 focused API/planner tests** pass. Real HTTP before/after on the same
generated high-bitrate fixture verified:

| Case | Before total negotiated bitrate | After |
|---|---:|---:|
| Inherit 1 Mbps server cap, client asks for 5 Mbps | 5 Mbps | 1 Mbps |
| User overrides server with 2 Mbps | 5 Mbps | 2 Mbps |
| Client asks for 0.5 Mbps | 0.5 Mbps | 0.5 Mbps |
| Negative user limit inherits server | 5 Mbps | 1 Mbps |
| No client limit, profile default 8 Mbps | 8 Mbps | 1 Mbps |
| Server cap disabled | 5 Mbps | 5 Mbps |
| LAN client with saved caps | 5 Mbps | 5 Mbps |
| Trusted forwarded IPv6 remote client, user cap 0.5 Mbps | 5 Mbps | 0.5 Mbps |
| Saved LAN expansion includes forwarded client | 5 Mbps | 5 Mbps |
| Forwarding header from an untrusted proxy is ignored | 5 Mbps | 5 Mbps |

Following the actual master/variant URLs produced real media segments. With a
0.5 Mbps saved cap and a 5 Mbps client request, FFmpeg's video `-maxrate` changed
from **4,930,306 to 430,306 bps** (the audio budget was 69,694 bps). First segment
size changed from **916,876 to 180,104 bytes**. Fifty warm PlaybackInfo requests
after ten warmups measured median **2.492 → 2.178 ms**, p95 **5.752 → 5.759 ms**;
the shared host makes these observations unsuitable for a performance claim.

This also implements the server-wide cap described by “Internet streaming bitrate `RemoteClientBitrateLimit`” (open); that row retains its
place for a separate dashboard-section review.

### Download permission and file eligibility

Download authorization already matched Jellyfin: ordinary users with
`EnableContentDownloading=false` receive 403; administrators pass the route
policy but fail the per-item check with 400. Saved off/on/off changes apply
immediately to GET, HEAD and the user-facing `CanDownload` field.

Fixed two additional defects found during verification:

- `CanDownload` and the endpoint now share the upstream item-kind/file-protocol
  predicate. DVD/BluRay videos, remote video/audio/book sources, folders,
  playlists and unsupported leaf types cannot be downloaded. ISO video remains
  eligible; Photo retains Jellyfin's unconditional file-eligibility override.
- Downloads serve the requested item's own path. Resolving playback sources
  previously promoted an alternate version to its primary and returned the
  wrong file.

Compared with Jellyfin `4910aafa1a` (`v12.0-rc7`), `LibraryController.GetDownload`
and `BaseItem`/`Video`/`Audio`/`AudioBook`/`Book`/`Photo.CanDownload`. The new DTO
service seam adds no repository query in production. The shared user-scoped item
visibility issue remains [Library selection and direct item access](#library-selection-and-direct-item-access).

**89 DTO tests and eight streaming tests** pass, including single/page DTO and
endpoint eligibility agreement, saved permission changes, download headers and
alternate-version bytes. Formatting, strict workspace Clippy, the SQL boundary
gate and the server build pass.

A disposable server verified 12 ordinary/admin off/on/off ranged GET/HEAD
requests, including exact bytes and attachment headers. Before/after fixture
rows reproduced both defects: DVD changed from `CanDownload=true`/206 to
false/400, and the alternate changed from the primary's bytes to its own bytes.
Fifty warm HEAD requests after ten warmups measured median **1.246 → 0.686 ms**,
p95 **2.075 → 0.758 ms** on the shared host; no performance claim is made.

The preceding [Allow media playback](#allow-media-playback)–[Internet streaming bitrate limit](#internet-streaming-bitrate-limit) workspace checkpoint ran 7,779 tests: 7,772 passed and
seven watcher-dependent tests failed, with five skipped. A minimal inotify watch
also fails with ENOSPC: the host's watch quota is exhausted. This is not a green
workspace gate. At that checkpoint separate line coverage was API **87.47%**
and core **92.30%**; this finding's new core/API code requires the next coverage run.

### Media deletion — open dependency

The saved deletion flags already reach the per-item permission predicate:
`EnableContentDeletion` permits eligible media generally, and
`EnableContentDeletionFromFolders` permits eligible items in selected libraries.
Box sets use collection-management rights and playlists use their ownership
rules. The endpoint and `CanDelete` DTO share that implementation.

This finding is **not complete**. The settings worktree still removes database
rows without implementing Jellyfin's physical deletion and sidecar/folder rules.
User-scoped item resolution is the separate [Library selection and direct item access](#library-selection-and-direct-item-access) dependency.

The existing `investigate/item-file-deletion` worktree owns this implementation,
with its canonical plan at `brain/plans/PLAN_ITEM_FILE_DELETION.md`. At review,
its clean HEAD was `c7a1034a`; scanner hierarchy, mixed-folder semantics, video
classification and alternate-version grouping must be correct before deletion
can safely choose the same files and directories as Jellyfin. The plan's
scanner/adoption work and staged deletion patch have not been integrated here.

Continue independent settings findings while preserving this finding as open. Completion
requires integrating the finished deletion work, the shared visibility checks,
and its real-filesystem/adopted-database validation. Do not mark this setting
complete merely because its permission predicate works, or cherry-pick the
stale deletion patch ahead of its prerequisites.

### Collection management

Verified supported against Jellyfin `v12.0-rc7` (`4910aafa1a`):
`CollectionController` uses the collection-management policy on create, add and
remove membership. The default authorization handler permits administrators;
ordinary users require `EnableCollectionManagement`. This is independent of
`EnableContentDeletion`. Box-set deletion uses the same collection permission
through the shared item-deletion predicate.

No production change is needed for the setting. A disposable native server
verified all three operations with saved **off → on → off** policy changes for
both ordinary users and administrators: **18 HTTP checks** pass. Denied ordinary
requests return 403; permitted creation returns 200 and membership edits return
204. Created collections were removed; their source media file remained intact.
Fifty warm duplicate-membership additions after ten warmups measured median
**0.762 ms**, p95 **1.084 ms** on the shared host, without a performance claim.

The pre-existing item-visibility dependency remains [Library selection and direct item access](#library-selection-and-direct-item-access). This verification does
not establish isolation for collections containing item IDs outside the user's
visible library, or complete [Media deletion — open dependency](#media-deletion--open-dependency)'s general physical-deletion implementation.

### Subtitle-management permission

Upload, remote search, remote download and remote-file retrieval now require
`EnableSubtitleManagement`, with the administrator/API-key overrides and
remote-access/schedule checks supplied by the shared user-permission policy.
They read the already-resolved authentication policy, so the added gate needs
no extra warm-request policy query. Saved changes apply on the next request.

Compared with Jellyfin `v12.0-rc7` (`4910aafa1a`), `SubtitleController` and the
`UserPermissionRequirement` handlers. The original audit overstated the deletion
gap: upstream intentionally requires administrator elevation for deletion even
when subtitle management is enabled. That rule remains in place. Subtitle
playback/conversion routes keep their separate upstream authorization rules.

**25 subtitle/Live TV permission tests** pass, including an ordinary denied user,
an enabled user, an administrator with the flag disabled and an API key across
all five management actions. Formatting, strict workspace Clippy, SQL boundary
and server build pass.

The batch run also caught an older malformed-subtitle test whose ordinary-user
fixture lacked the newly required permission. The fixture now explicitly grants
subtitle management so the request reaches input validation; all 14 video tests
pass, including the expected malformed-base64 400. The separate denied-user
matrix continues to verify that authorization happens first.

Thirty live HTTP probes verified saved off/on/off changes for ordinary and admin
accounts. Before the fix, disabled ordinary users could upload a real subtitle
(204), search (200), and reach remote download/fetch. Afterwards all four return
403; enabled users can upload/search/download, and deletion remains 403 for all
ordinary users. Administrators retain their override. No remote provider account
was used: an unknown remote subtitle ID exercises the provider error path after
authorization, not successful third-party delivery. [Library selection and direct item access](#library-selection-and-direct-item-access) item visibility remains
an independent dependency.

Fifty warm searches after ten warmups measured median **0.787 → 0.867 ms**, p95
**1.255 → 1.063 ms** on the shared host, without a performance claim.

### Live TV access and management

Verified supported against Jellyfin `v12.0-rc7` (`4910aafa1a`). Live TV read
routes use `LiveTvAccess`, recording/timer mutations use `LiveTvManagement`, and
tuner/provider configuration retains administrator elevation. Management does
not implicitly grant viewing, nor does viewing grant recording management.
Administrators and API keys retain the upstream overrides; the shared policy
still checks remote-access restrictions before the administrator override.

No production change is needed. All seven existing permission tests pass,
covering denied/allowed handlers, API keys, administrators, remote access,
schedules and anonymous callers. The [Subtitle-management permission](#subtitle-management-permission) check also passes strict workspace
Clippy, formatting, SQL boundary and build.

A disposable server passed **80 live HTTP checks** across both roles and all
four access/management combinations, then restored both flags to false. Info,
guide, channels and timer lists were tested separately from cancellation and
tuner configuration. Missing ordinary timers produce 204 and missing series
timers produce 404 after authorization; disallowed requests produce 403 before
lookup. An unknown tuner type remains 404 for an administrator and 403 for an
ordinary user regardless of either permission. No real recording was touched.
Fifty warm Info requests after ten warmups measured median **0.463 ms**, p95
**0.795 ms** on the shared host, without a performance claim.

This checks these dashboard flags at their controller boundaries. Generic item
and playback visibility remains [Library selection and direct item access](#library-selection-and-direct-item-access); selecting individual plugin channels is
[Channel selection](#channel-selection), and DVR settings have their own later findings.

### Remote control and shared devices

The manager now enforces Jellyfin's `AssertCanControl` before delivering a
command. Controlling one's own primary/additional-user session, a public target,
or a trusted userless/API-key operation remains allowed. Controlling another
user's session requires `EnableRemoteControlOfOtherUsers`, including for an
administrator with that permission disabled.

The same check now protects both capability endpoints, now-viewing reports and
guest-user add/remove. Attaching a different user also requires administrator
privileges, preventing changes to that user's playback history through guest
attachment. API keys can issue commands to an explicit target without resolving
a nonexistent user row. Trait implementations lacking control authorization
fail closed.

`EnableSharedDeviceControl` now filters public sessions from the controllable
list according to the **requested user's** preference, including administrator
and API-key requests on that user's behalf. The caller's device-access rules
also filter this list. Device permissions are resolved once per page, after
releasing the session mutex. Missing requested users return an empty list.

Compared with Jellyfin `v12.0-rc7` (`4910aafa1a`), `SessionManager.AssertCanControl`,
`AssertCanAttachUser`, `GetSessions` and the controller's mutating calls. Upstream
uses the shared-device setting as a list filter; it does not forbid commands to
public sessions. The implementation preserves that distinction.

**48 core session tests and 38 API session/playstate tests** pass, including
message delivery/absence, live permission changes, primary/additional/public
sessions, administrator and API-key cases, guest attachment and device filters.
Formatting, strict workspace Clippy, SQL boundary and the server build pass.

Real HTTP and WebSocket checks verified seven operations across ordinary/admin
saved off/on/off changes. Previously every disabled-user command/mutation
returned 204 and Pause/Mute reached the target socket; afterwards they return
403 with no command delivered. Enabled requests still deliver. An ordinary user
can no longer attach a different user; administrators can. A disallowed
persistent target device disappears from the list, and an API-key Pause changes
from 400 to 204. Public-session filtering is covered by the real-manager tests.

Fifty warm Pause requests after ten warmups measured median **0.565 → 0.775 ms**,
p95 **0.698 → 0.952 ms**. The disposable before/after servers ran on a shared
host; these are observations, not a performance claim.

### SyncPlay access

SyncPlay authorization now retains the administrator override inherited from
Jellyfin's default authorization requirement. An administrator can create and
join groups even with `JoinGroups` or `None` saved. Ordinary users keep the
existing create/join/no-access rules. Revoking access while a user is in a group
still permits group operations and leaving, but denies listing or joining other
groups; after leaving, group operations are denied too.

Compared with Jellyfin `v12.0-rc7` (`4910aafa1a`),
`SyncPlayAccessRequirement`, both authorization handlers and
`UserManager.GetUserById`. The specialized handler never issues an unconditional
failure that would cancel the default handler's administrator success. API keys
still reach user resolution: their empty user ID causes an argument exception,
so the matching response is 400 rather than the previous 403.

**39 API session/playstate tests** pass, including the administrator/API-key
matrix, ordinary policy modes and active membership. Formatting, strict
workspace Clippy, SQL boundary and the server build pass. Broader validation is
recorded in the batch checkpoint.

Twenty-one real HTTP probes verified saved mode changes for ordinary and
administrator accounts, active-member revocation and API-key requests. Admin
create with `JoinGroups` changed from 403 to 200; with `None`, create/join changed
from 403 to 200/204. Ordinary-user behavior remained correct. API-key List
changed from 403 to 400. Disposable groups and accounts were removed afterwards.

Fifty warm List requests after ten warmups measured median **0.593 → 0.691 ms**,
p95 **0.763 → 1.303 ms**. These shared-host observations are not a performance
claim.

## Users: access and parental controls

### Library selection and direct item access

Integrated main's visibility merge `f366059b` (PR #43), preserving the first
30 settings fixes. User-scoped item actions now evaluate the item, its parents
and its library membership before reading or mutating data. The evaluator
handles adopted physical folders, shared locations, owner extras, cached images,
private playlists and batched SyncPlay queues. Administrators remain subject to
their own item visibility; explicit target-user requests use that user's rules.

The integration retains [Download permission and file eligibility](#download-permission-and-file-eligibility)'s item download eligibility and requested-version
file selection, [Subtitle-management permission](#subtitle-management-permission)'s subtitle permission, and the earlier combined default and
administrator policies. It also corrects three interactions:

- Public image/attachment routes resolve authenticated identity without applying
  the default route policy. A denied access schedule must not erase identity and
  turn a restricted user's request into unrestricted anonymous image access.
- Userless PlaybackInfo can return source information. Negotiation with a device
  profile still requires a user, matching upstream's empty-ID 400 and missing
  user 404. Explicit API-key target users retain their playback policy.

- Posted PlaybackInfo `UserId` now selects the effective user; the query parameter
  takes precedence, and ordinary callers cannot select another account.

Compared with Jellyfin `v12.0-rc7` (`4910aafa1a`), `LibraryManager`, item-kind
visibility overrides, `ImageController`, `MediaInfoController` and
`MediaInfoHelper`. The existing [visibility investigation](../investigations/per-user-item-visibility.md)
records the route-by-route oracle and its separate live Jellyfin 12.1 evidence.

Preserved upstream exceptions include public userless images, raw streaming
lookups, explicit-ID browse queries and the 401 result for a hidden browse
parent. This is not a claim that every media URL returns 404 when hidden.
`BlockedMediaFolders` is a legacy stored preference that takes precedence over
the allow-list; upstream's current policy update does not write that obsolete
field, and the Web editor sends null. New selections use `EnableAllFolders` and
`EnabledFolders`. The independent unrated/tag browse and rating-dataset findings
remain [Parental ratings and subratings](#parental-ratings-and-subratings)–[Allowed and blocked tags](#allowed-and-blocked-tags); library enable behavior is [Disabled libraries stay out of listings and counts](#disabled-libraries-stay-out-of-listings-and-counts).

Validation: all 21 core visibility tests, four real-manager HTTP tests, 891 API
tests, SQL boundary checks, formatting, strict workspace Clippy and server build
pass. The public image/schedule and posted target-user regressions pass. The
native server matches all 33 observations in the recorded Jellyfin 12.1 fixture.
Hidden item reads/mutations return 404, cached images remain hidden, API-key
explicit users are checked, and a denied delete preserves the item.

Measured native debug-build median milliseconds (50 requests per route, identical
synthetic libraries) before → after: detail 3.281 → 3.571; PlaybackInfo
1.136 → 2.752; cached image 0.662 → 1.874; SyncPlay list 0.963 → 1.893. These
added visibility reads have a measurable cost. The shared host and debug builds
are unsuitable for a production performance claim; Docker benchmarking is
unavailable. Raw fixtures: `/tmp/ferrofin-visibility-ferrofin-ba3fp826` and
`/tmp/ferrofin-visibility-ferrofin-d8kmrtfs`; checks:
`/tmp/ferrofin-dashboard-u19-checks.json`. Broader coverage/workspace checks will
run at the batch checkpoint; the preceding checkpoint's host inotify exhaustion
remains recorded separately.

### Owned extras on adopted databases (follow-up)

[Parental ratings and subratings](#parental-ratings-and-subratings) adoption validation exposed a visibility integration regression:
`/Items/{id}/SpecialFeatures` returned an empty list for adopted extras with an
`OwnerId` but no `TopParentId`. Adding user context in [Library selection and direct item access](#library-selection-and-direct-item-access) exposed an older
query-scoping omission. Jellyfin `4910aafa1a`,
`LibraryManager.AddUserToQuery`, treats `OwnerIds` as an existing scope; Ferrofin
incorrectly added library IDs and excluded the extras.

Restored the eighth upstream scope exception. User parental restrictions still
apply to the returned extras, and the owner still has its [Library selection and direct item access](#library-selection-and-direct-item-access) visibility check.
The extended repository regression, formatting, strict workspace Clippy and
server build pass. Native HTTP returns the owned extra after the fix and hides
it when its rating exceeds the user's limit. The 10.11.8 adoption and restart
stages now pass where both previously reported missing extras. The remaining
adoption matrix is recorded with [Parental ratings and subratings](#parental-ratings-and-subratings).

Native debug-build median for SpecialFeatures: 2.245 → 3.252 ms, p95
2.787 → 4.811 ms, 50 measured requests after ten warmups. Before returned zero
extras; after returned one, so this is a cost observation, not a speed comparison
of equivalent work. Measurements: `/tmp/ferrofin-dashboard-u19-extras-{before,after}-results.json`.

This follow-up does not increment the completed-finding count.

### Channel selection

Verified the visibility implementation integrated in [Library selection and direct item access](#library-selection-and-direct-item-access) against Jellyfin
`4910aafa1a`, `MediaBrowser.Controller/Channels/Channel.cs::IsVisible` and
`IsChannelVisible`. `EnableAllChannels` and `EnabledChannels` control channel
entries and their content. A nonempty legacy `BlockedChannels` list replaces
the allow-list; administrators have no override. Live TV guide channel IDs are
not plugin channel references and keep their separate Live TV rules.

The current Web editor clears the legacy field; pinned upstream policy writes
do not update it. Adoption preserves that preference and visibility reads it.
Ferrofin's stock channel-provider list is empty: this verification uses stored
channel entries and does not claim support for loading native .NET providers.

Added a matrix regression for all/selected/none, deny-list precedence and both
administrator states. All 22 visibility tests, formatting and strict workspace
Clippy pass. A disposable native server passed 24 HTTP assertions on channel
entries and content, with policies changed through the API and legacy state
seeded only into the fixture database. Results:
`/tmp/ferrofin-dashboard-u20-results.json`; fixture:
`/tmp/ferrofin-dashboard-u20-4r228c72`.

No production code changed in this finding. [Library selection and direct item access](#library-selection-and-direct-item-access) records the measured cost of
the shared visibility implementation.

### Device selection

Compared the permission consumer, session admission, public-user picker and
user-update behavior with Jellyfin `4910aafa1a`: `DeviceManager.CanAccessDevice`,
`SessionManager.AuthenticateNewSession`, `UserController.Get`, and
`Users/DeviceAccessHost` plus `UserManager.RenameUser`.

Existing all-device, selected-device, case-insensitive matching, administrator
and nonpersistent-ID exceptions are correct. Fixed three missing details:

- Device admission failures return 403, matching the upstream security error.
- After setup, authenticated `/Users/Public` callers see only users allowed on
  their device. Anonymous headers do not create authenticated claims; anonymous
  callers and the startup wizard retain upstream behavior.
- Renaming a restricted user logs out existing disallowed devices and clears
  their cached tokens. In this pinned source, policy/configuration saves do not
  emit `OnUserUpdated`, so changing the device list alone retains existing tokens.
  New authentication and session-target filtering use the saved policy immediately.

Validation: 61 device/session tests, six real-manager HTTP tests, all 891 API
tests, SQL boundary, formatting, strict workspace Clippy and server build pass.
The account-update unit fixture now explicitly declares unrestricted device
access; real-manager regressions cover restricted accounts. A native server
passed all 12 live observations, including transient devices, both setup states,
case-insensitive admission, administrator override and actual token revocation.

Native debug-build `/Users/Public` median: 0.652 → 0.832 ms, p95:
0.811 → 1.591 ms (50 measured requests after ten warmups, two fixture users,
authenticated caller). Shared-host measurements, not a production performance
claim. Results: `/tmp/ferrofin-dashboard-u21-{before,after}-results.json`;
quality checks: `/tmp/ferrofin-dashboard-u21-checks.json`.

### Parental ratings and subratings

The maximum rating now uses all 45 upstream rating resources (44 country
tables; GB/UK share a country key), rather than the US table alone. The server's
default country is read live. Explicit item/library countries, country prefixes,
US fallback, custom ratings, inherited ratings and unrated values retain the
upstream precedence. Existing 12.1 parsing behavior is preserved, including
whole-value matching before slash-separated fallback.

Metadata edits and scans now persist the score used by the shared visibility
resolver. Clearing a rating clears its old numeric value. An idempotent boot
repair recalculates existing rows under the complete catalog, including databases
that already ran the older US-only repair. This changes no schema.

The query audit also found and fixed these bypasses:

- A rated collection/playlist uses its own rating. Only an unrated container
  consults linked children; the former conjunction incorrectly rejected rated
  containers because of an adult child.
- Genre/artist/studio aggregates and by-name search rows now check the ratings
  of related media. Counts on genre, person and year pages apply the same limit
  before aggregation.
- Direct-child, linked-child, descendant and whole-server played counts apply
  the limit. A hidden episode's progress cannot make its series resumable.

Source: Jellyfin `4910aafa1a`, `LocalizationManager.LoadAll/GetRatingScore`,
`BaseItem.GetParentalRatingScore/OnMetadataChanged`,
`BaseItemRepository.QueryBuilding.ApplyParentalRestrictions`,
`BuildMaxParentalRatingFilter`, `ApplyItemByNameAccessFiltering`, and
`ItemCountService`. Rating resources are copied verbatim; 17 upstream rating
cases are transliterated into tests. [Block unrated content](#block-unrated-content) and [Allowed and blocked tags](#allowed-and-blocked-tags) separately cover unrated kinds
and tags; their remaining query gaps are not closed by this change.

Validation: 228 focused core tests, 28 repository integration tests, seven
real-manager HTTP tests, SQL boundary, formatting, strict workspace Clippy and
server build pass. Native HTTP checks cover five country changes without a
restart, score/subscore boundaries, direct access, metadata edits, facets and
counts: 80 recorded observations across the two fixtures. The count baseline
exposed all genres and reported three movies even when the user could see none;
the corrected build reports only accessible media.

The boot-repair adoption matrix passes all 28 stages: adoption, restart, scan
and unchanged second scan for 10.11.8–10.11.11, 12.0, and both supported 12.1
upgrade routes. Fixtures are disposable copies. Adoption remains one-way;
returning to Jellyfin requires restoring its backup. This run also exposed the
owner-scoping regression fixed separately in `304683f3` ([Library selection and direct item access](#library-selection-and-direct-item-access)).

Native debug-build timings, 50 requests after ten warmups:

| Path | Before median / p95 ms | After median / p95 ms |
|---|---:|---:|
| Rating choices | 0.428 / 0.559 | 0.451 / 0.956 |
| Movie detail, catalog change | 2.369 / 2.757 | 2.712 / 4.110 |
| Movie browse, catalog change | 1.635 / 1.867 | 1.703 / 2.251 |
| Genres, count/facet fix | 1.918 / 2.133 | 1.820 / 2.472 |
| Genre detail, count/facet fix | 1.795 / 2.012 | 1.521 / 1.900 |
| Person detail, count/facet fix | 1.818 / 2.691 | 1.601 / 2.593 |
| Movie browse, count/facet fix | 1.815 / 2.758 | 1.726 / 2.523 |

These are small-fixture shared-host observations, not production performance
claims. Corrected facets return fewer rows, so their before/after work differs.
The initial three-movie scan was observed at 0.409 → 0.413 seconds with 0.2-second
polling, a coarse smoke check rather than a throughput benchmark.

Evidence: `/tmp/ferrofin-dashboard-u22-{before,after}-results.json`,
`/tmp/ferrofin-dashboard-u22-counts-{before,after}-results.json`,
`/tmp/ferrofin-dashboard-u22-query-checks.json`, and
`/tmp/ferrofin-dashboard-u22-adoption-fixed.log`.

The final workspace checkpoint passes **7,842 tests (five skipped) and three
doctests**. Earlier runs had host inotify `ENOSPC` failures: four core watcher
cases and one watcher-dependent server case. The host condition no longer
reproduces in this checkpoint, and the instrumented watcher subset also passes
on recheck. No host limits or unrelated processes were changed.

The per-crate cargo coverage threshold passes at 80.85%, but that aggregate
includes stale executables and warns about 144 mismatched functions. Restricting
the report to the exact 21 instrumented executables listed by this build's
nextest metadata reports **94.96% core line coverage** (100,718 / 106,069 lines in
123 core source/test files) and retains 18 mismatch warnings. Those profile
warnings remain a validation limitation; both percentage thresholds pass. The
current-binary report and list are
`/tmp/ferrofin-dashboard-u22-exact-coverage-summary.json` and
`/tmp/ferrofin-dashboard-u22-coverage-binaries.json`. The instrumented full core
run passed 2,120 of 2,123 tests before the host condition cleared; its three
watcher failures subsequently pass. Server wiring is exempt from the separate
coverage gate, and its integration tests pass.

Workspace and watcher records:
`/tmp/ferrofin-dashboard-u22-workspace-checks.json` and
`/tmp/ferrofin-dashboard-u22-final-watcher-recheck.log`.

### Block unrated content

Saved `BlockUnratedItems` now reaches browse, explicit-ID queries, Latest,
facets, counts, resumable folders and folder played-state queries. Preferences
are loaded once for each resolved query and shared by its page and total.
Previously the direct visibility resolver blocked an unrated movie while lists
and counts continued to expose it.

The query uses the inherited numeric rating: zero is a rating; an empty or
unrecognized classification is unrated. It respects the persisted `UnratedType`
on adopted rows and derives the class/source category for older Ferrofin rows
where that column is absent. Normal metadata writes and scans now stamp the
category. This needs no schema migration or destructive backfill.

User-policy persistence also preserves unnamed numeric enum values, rather than
writing Rust's debug representation. Reading and direct visibility accept the
same case-insensitive enum names and numeric values as upstream.

Source: Jellyfin `4910aafa1a`, `InternalItemsQuery.SetUser`,
`BaseItemRepository.TranslateQuery` and `ApplyParentalRestrictions`, plus the
`GetBlockUnratedType` overrides on media classes. Preserved source distinctions:

- Query construction excludes the canonical `Other` preference. Direct item
  checks retain it. Current Web presents seven specific categories, not `Other`.
- Plain folders and by-name items have direct-access exceptions; seasons leave
  direct blocking to their series/episodes. Query filtering uses the stored
  category, as upstream does.
- An explicit internal `HasParentalRating = true` selector skips unrated-kind
  filtering in `TranslateQuery`. Access-only predicates retain the user policy.
- Administrators do not receive an unrated-content exemption.

Validation: 390 focused core tests, 28 repository integration tests, eight
real-manager HTTP tests, SQL boundary, formatting, strict workspace Clippy and
server build pass. Regressions cover 15 class/source combinations on old rows,
score zero, explicit IDs, the query exception, folder played state, numeric enum
round-tripping and live saves. All 45 native HTTP observations pass for repeated
off/on/off changes across lists, Latest, genres, counts and direct access.

Native debug-build timings, 50 requests after ten warmups:

| Path | Before median / p95 ms | After median / p95 ms |
|---|---:|---:|
| Genres | 2.166 / 2.828 | 1.817 / 4.168 |
| Genre detail | 1.637 / 1.821 | 1.803 / 4.368 |
| Movie browse | 1.712 / 2.077 | 2.366 / 5.301 |

This is a three-movie fixture on a shared host with concurrent compilation,
not a production performance claim. The corrected browse returns one permitted
movie instead of all three and adds the preference lookup/filtering cost.

Evidence: `/tmp/ferrofin-dashboard-u23-checks.json` and
`/tmp/ferrofin-dashboard-u23-{before,after}-results.json`.
A clean coverage build passes all 2,138 core tests and reports **94.59% line
coverage**. LLVM still reports 18 mismatched-function warnings after cleaning;
the percentage passes the gate but retains that measurement limitation. Evidence:
`/tmp/ferrofin-dashboard-u23-coverage.json` and its referenced log.

[Allowed and blocked tags](#allowed-and-blocked-tags)'s tag predicates remain a separate finding; loading their preferences does
not implement them.

### Allowed and blocked tags

Saved tag restrictions now reach browse, explicit-ID queries, Latest, facets,
counts and descendant/played-state queries through the shared parental predicate.
The direct visibility evaluator already enforced them; lists previously exposed
items and counts that direct access rejected.

The query checks the item's own tags, SeriesId, AncestorIds and TopParentId.
Blocked tags take precedence over allowed tags; matching removes diacritics and
normalizes case. People retain upstream's allowed-tag exception, while blocked
tags still apply. A by-name row must also have accessible related media. No new
index, schema migration or data rewrite is needed. Each tag set uses four indexed
membership checks, matching the pinned source's query shape.

Root children retain the allowed-tag exception. Home views now share the batched
item visibility evaluator, so a library with a blocked tag disappears even when
`includeHidden=true`; its media also disappears through inherited tags. Hiding a
home view through display preferences remains a separate operation.

Compared against Jellyfin `4910aafa1a`: `InternalItemsQuery.SetUser`,
`BaseItemRepository.ApplyParentalRestrictions`, `ApplyItemByNameAccessFiltering`,
`BaseItem.IsVisibleViaTags`, `GetAncestorIds` and `UserViewManager.GetUserViews`.
Native Jellyfin 12.1.0 is supplementary runtime evidence, not the source pin.

Validation: 433 focused core tests, 28 repository tests and nine real-manager
HTTP tests pass. The regressions cover each inheritance source, Unicode,
block-list precedence, empty/blank policies, explicit IDs, Person exceptions,
live saves, counts and root views. An EXPLAIN QUERY PLAN regression verifies
indexed tag-value/map and ancestor searches. SQL boundary, formatting, strict
workspace Clippy and server build are checked alongside the change.

All **60 native HTTP observations match Jellyfin** after materializing the
shared genre before the matrix. UUIDs are normalized, and the reference's
pre-existing extra Playlists root is excluded from the name comparison; neither
normalization alters tag-policy outcomes. Twenty-one observations change from
the baseline. The fixture repeats empty, blocked, allowed, conflicting and reset
policies, then tags a library and verifies inheritance and root visibility.

Native debug-build timings, 50 requests after ten warmups:

| Path | Before median / p95 ms | After median / p95 ms |
|---|---:|---:|
| Movie browse | 2.066 / 2.697 | 2.355 / 5.045 |
| Genres | 1.589 / 2.343 | 2.084 / 4.379 |
| Genre detail | 1.691 / 2.244 | 1.951 / 4.477 |
| Library views | 2.033 / 2.769 | 4.030 / 5.134 |

This is a two-movie fixture on a shared host with other builds/probes running,
not a production performance claim. Corrected browse/count paths return one
movie rather than two; view filtering adds batched policy and hierarchy reads.
Docker benchmarking remains unavailable in this environment.

Evidence: `/tmp/ferrofin-dashboard-u24-checks.json`,
`/tmp/ferrofin-dashboard-u24-comparison.json` and the three fixture result paths
listed in that comparison. A clean core coverage run passes **2,144 tests** and
reports **94.59% line coverage**, with 18 LLVM mismatched-function warnings still
present. This passes the gate with the measurement limitation retained. See
`/tmp/ferrofin-dashboard-u24-coverage.json` and its referenced log.

### Access schedules

Current Jellyfin Web rebuilds schedule rows from three HTML attributes:
`DayOfWeek`, `StartHour` and `EndHour`. Both hours are strings; neither database
`Id` nor `UserId` is sent. Ferrofin required those IDs and returned HTTP 400 for
any nonempty schedule list, despite enforcing schedules sent by older/full DTO
clients. `AccessSchedule` now accepts upstream constructor defaults. Persistence
continues assigning the row identity and target user inside the policy transaction.
The existing numeric-body binder handles Web's quoted hours.

Login and ordinary authorization also stopped at whole-second precision. They
now include fractional seconds, matching `TimeOfDay.TotalHours`: the exact end
is allowed, but 100 ns after it is outside the window.

Source: Jellyfin `4910aafa1a`, `AccessSchedule`,
`UserEntityExtensions.IsParentalScheduleAllowed`, `UserManager.AuthenticateUser`,
`DefaultAuthorizationHandler`; Web `1e507c588f`,
`apps/dashboard/features/users/components/ParentalControl.tsx:getSchedulesFromPage`.
Native Jellyfin 12.1.0 confirms all 16 focused save/enforcement observations.

Existing [Per-user remote access](#per-user-remote-access) semantics remain: server-local time, no schedules means unrestricted,
any matching window permits access, day groups are honored, and overnight windows
are not implicitly split. Existing administrator tokens bypass the default-route
schedule check; password login still checks their schedule. User-detail and
system-info routes keep upstream's schedule exceptions. Policy saves take effect
on the next request without replacing the token.

Validation passes: 55 focused core tests, the model payload regression, all 892
API tests, ten real-manager HTTP tests, SQL boundary, formatting, strict workspace
Clippy and server build. Boundary regressions cover the exact start/end and the
first 100 ns past the end; HTTP verifies that saved rows receive real IDs and
that changing a schedule affects an already issued token immediately.

All **49 native observations pass** after the fix. Before it, all six nonempty
Web payloads returned 400; the baseline then supplied full legacy rows to check
authorization independently. After it, all eight saves (including clears)
return 204 without that fallback. Saved day groups, multiple windows, resets,
login, existing tokens and administrator exceptions match the pinned rules.

Fifty warm scheduled `GET /Users/Me` requests after ten warmups measured median
**0.460 → 0.360 ms**, p95 **0.625 → 0.501 ms**. These debug builds ran on a shared
host while other checks compiled; this is path verification, not a production
performance claim. Native fixtures use `TZ=America/Denver` for both client and
server. Evidence: `/tmp/ferrofin-dashboard-u25-{before,after}-results.json`,
`/tmp/ferrofin-dashboard-u25-checks.json`, and the reference fixture
`/tmp/ferrofin-dashboard-u25-reference-jellyfin-2rb2_mkh/results.json`.

Separate coverage gates pass: **core 94.61%** (2,149 tests), **model 88.58%**
(959 tests), **API 87.52%** (892 tests). All coverage test runs pass. LLVM reports
18, 39 and 81 mismatched-function warnings respectively; the threshold results
retain that measurement limitation. The private workspace coverage cache was
cleaned before this sequence. Evidence:
`/tmp/ferrofin-dashboard-u25-coverage.json` and its referenced logs.

## Libraries: management and basic options

### Library management and media paths

Library creation and path updates now match the dashboard contract. The physical
folder prerequisite preserves media when libraries share a location.

Source: Jellyfin `4910aafa1a`, `LibraryStructureController`,
`LibraryManager.AddVirtualFolder`, `AddMediaPath`, `UpdateMediaPath`,
`SyncLibraryOptionsToLocations`, `AggregateFolder` and `CollectionFolder`.
The approved D1 physical-folder work was ported from `59aaf532`; unrelated
item-file-deletion commits were not included. [Media deletion — open dependency](#media-deletion--open-dependency) remains open.

Creation rejects leading/trailing whitespace and embedded newlines. Both library
creation and AddMediaPath require directories, with upstream's respective 400 and
404 responses. UpdateMediaPath synchronizes existing shortcut locations into the
options instead of adding its request path as a disconnected option entry.
Duplicate names, collection type, locations and removal retain their contracts.

Scanned media now hangs from a physical location Folder below AggregateFolder.
The CollectionFolder records its physical locations and folder IDs; readers,
notifications, provider selection and encoding/library lookups follow that
hierarchy. Shared roots retain all referring library ancestors, and a first
path-scoped refresh provisions its physical parent. Removing one library leaves
another library's media IDs and watched state intact.

A library can be removed before an older database has had a new scan. Before
deleting its CollectionFolder, a transaction moves legacy children under their
physical location, retaining media IDs, metadata and user data. An unresolvable
legacy path rolls back and fails the removal rather than deleting media. The
same protection precedes a rename and orphan-library cleanup; [Rename honors the refresh setting](#rename-honors-the-refresh-setting) separately
addresses the rename controller's unconditional refresh.

Removing a library with refresh disabled preserves physical rows until a scan,
as upstream does. A successful full scan prunes roots no library references,
including after the last library is removed; an unrelated empty library does not
prevent cleanup. This reads stored parent links: normal browse queries project
AggregateFolder as user views and cannot be used to enumerate its physical
children. Source files are never deleted by these structural controls.

The 14 basic native observations match Jellyfin 12.1.0 exactly; six recorded
steps change from the parent build. Fresh and upgraded shared-directory probes
retain HTTP 200 and Played=true immediately after removal and after rescan;
the parent returned 404 until rescanning. The upgrade starts with the [Access schedules](#access-schedules) binary,
restarts the same fixture on this build and removes a library without a scan.
Evidence: `/tmp/ferrofin-dashboard-l01-ferrofin-3nirblr0/results.json`,
`/tmp/ferrofin-dashboard-l01-shared-ferrofin-z6bd9gyh/results.json` and
`/tmp/ferrofin-dashboard-l01-shared-ferrofin-7vbe81e1/results.json`.

Fifty warm GET /Library/VirtualFolders requests after ten warmups measured median
**0.950 → 0.980 ms**, p95 **1.311 → 1.316 ms**. These are debug binaries on a
shared host with concurrent checks, not production performance claims.

All **16** extended native observations now match the reference, including
immediate preservation and removal on the next scan after deleting the last
library. Evidence: `/tmp/ferrofin-dashboard-l01-ferrofin-_no9la8a/results.json`.
All **28 adoption stages pass** across the seven supported source/upgrade paths:
initial adoption, restart, scan and repeated scan preserve the fixture metadata,
user data and views. Adoption is one-way; returning to Jellyfin requires restoring
the preserved original backup. These checks use disposable copies.

The full workspace run passes **7,880 tests**, fails seven watcher-dependent
tests and skips five; all three doctests pass. A serial recheck retains those
seven failures (four core watch tests, two encoding file-notification tests,
one scan-change integration test). Core watch creation reports the host inotify
limit, and the scan test explicitly reports that its required watcher could not
start. The earlier full instrumented core run passes all 2,161 tests, including
its watchers. **The workspace gate is not green.** Host settings and unrelated
processes were not changed. Sixteen final physical-location/adoption regression
tests, strict workspace Clippy and the rebuilt native server checks pass.

The more general nested-location hierarchy remains an explicit open finding,
[Plain subfolders of a movie library are `Folder`s](#plain-subfolders-of-a-movie-library-are-folders): the existing planner flattens intermediate directories. Exact shared roots
are covered here; nested root/subfolder resolution needs its own implementation
and reference matrix. This change does not certify that separate case.

Separate line-coverage thresholds pass: **core 94.98%**, **model 88.73%**,
**API 87.52%**, **providers 93.15%**, **extensions 94.83%**. The final core run
passes 2,158 tests and retains four host watcher failures; its current-binary coverage
report passes the 80% threshold. The other crate runs pass all their tests
(960 model, 892 API, 754 providers with four skipped, 96 extensions). LLVM
mismatched-function warnings remain a measurement limitation; the initial five
reports log 19, 39, 83, 70 and 39 respectively. The ordinary final core report
also picked up stale instrumented binaries and logged 132 warnings. Restricting
LLVM to the 22 current binaries listed by nextest reports 101,858 / 107,241 lines
covered (94.98%), with 19 warnings. This is the core figure recorded above.
Commands and results are in `/tmp/ferrofin-dashboard-l01-{coverage,final-core-coverage}.json`
and `/tmp/ferrofin-dashboard-l01-exact-coverage-summary.json`.
The SQL-boundary gate, formatting and strict workspace Clippy also pass.

No active user library, configuration, database or media file was used for these
fixtures. The implementation is complete for the controls covered here; the
host-dependent workspace failures and [Plain subfolders of a movie library are `Folder`s](#plain-subfolders-of-a-movie-library-are-folders) remain explicitly open.

### Rename honors the refresh setting

Renaming with `refreshLibrary=false` (or omitted) no longer queues a scan. The
renamed collection folder is still materialized immediately, so its physical
locations can be browsed. `refreshLibrary=true` retains the scoped refresh.
The [Library management and media paths](#library-management-and-media-paths) hierarchy and legacy-conversion prerequisite preserve media identities,
metadata and user data across the collection-folder ID change.

Source: Jellyfin `4910aafa1a`,
`LibraryStructureController.RenameVirtualFolder`. The prior controller explicitly
ignored the flag to compensate for cascading deletion of media rows. That
compensation is no longer needed after [Library management and media paths](#library-management-and-media-paths).

The API regression uses a library double that panics if a scan is queued. Both
omitted and false flags rename successfully without a scan. All **893 API tests**,
formatting, strict workspace Clippy and server build pass. The separate API
coverage gate reports **87.55%**, with all 893 tests passing and 93 LLVM
mismatched-function warnings retained as a measurement limitation.

Native fixtures edit a movie's title/overview, mark it played, then add an
unscanned second movie before renaming. Before this change, the false flag
indexes that new movie. Afterwards it remains unindexed, matching native
Jellyfin 12.1; an explicit refresh then finds it. An explicit true flag still
indexes it during rename. Existing media retains HTTP 200, its edited metadata
and Played=true in fresh and upgraded databases. The upgrade starts with [Access schedules](#access-schedules),
restarts on this build and renames without rescanning first.

The reference also resets unlocked title/overview during the later explicit
scan, whereas Ferrofin retains them. That separate metadata-refresh observation
is recorded as [Unlocked metadata after a library rename and an explicit scan — owner decision needed](#unlocked-metadata-after-a-library-rename-and-an-explicit-scan--owner-decision-needed) for pinned-source verification; it is not counted as rename
parity or silently accepted as a compatibility exception.

Thirty warm rename requests after ten warmups measured median
**7.973 → 7.762 ms**, p95 **32.489 → 26.308 ms**, with 250 ms between requests.
Both builds use disposable two-movie libraries; these shared-host debug timings
are not a production performance claim. No requests failed.

Evidence: `/tmp/ferrofin-dashboard-l02-checks.json`,
`/tmp/ferrofin-dashboard-l02-coverage.json`, native results under
`/tmp/ferrofin-dashboard-l02-ferrofin-{1lua1urb,nz8qjr1o,xy7d6rvu}/results.json`,
and benchmark results under `...-{5tzqp7wk,gc7eplak}/results.json`.
The [Library management and media paths](#library-management-and-media-paths) workspace watcher limitations remain open; this change adds no watcher
or scan dependency to rename with refresh disabled.

### Disabled libraries stay out of listings and counts

Saving `LibraryOptions.Enabled=false` now hides that library from the user root,
unscoped item queries and their counts immediately, including for administrators.
Direct item visibility and home views already enforced the option; the repository
paths were missing it. They now read the same saved JSON/XML options without
creating files or mutating library rows. Re-enabling takes effect on the next
request, without a scan or restart.

Source: Jellyfin `4910aafa1a`, `CollectionFolder.IsVisible`,
`Folder.AddChildrenFromCollection` and `LibraryManager.AddUserToQuery`.
The original audit incorrectly suggested disabling should stop scans: upstream
uses this option for visibility. Explicit-scope browse and userless internal
root reads retain the upstream exceptions; per-user folder restrictions still
apply independently.

All **42 native observations match Jellyfin 12.1.0** across enabled/disabled/
re-enabled states and administrator/ordinary users. The six previously different
responses—root listings, movie lists and counts for both users—now match. Direct
library/movie lookup, home views and explicit library browse remain consistent
with the reference. Evidence: `/tmp/ferrofin-dashboard-l03-ferrofin-lwjz01x2/results.json`.

Validation passes: **223 core tests, 28 repository tests, ten real-manager HTTP
tests**, SQL boundary, formatting, strict workspace Clippy and server build.
Regressions verify live JSON toggles across pages, counts and visible-library
IDs, adopted XML reads without writeback, root filtering and scope exceptions.

The full core coverage run passes **2,160 tests** and retains the same four host
watcher failures. The separate 80% threshold passes; restricting LLVM to the 22
current test binaries reports **94.99%** (102,012 / 107,388 lines), with 19
mismatched-function warnings. The workspace watcher limitation documented in [Library management and media paths](#library-management-and-media-paths)
remains open; no host limits were changed.

Fifty warm ordinary-user movie queries after ten warmups measured median
**9.732 → 8.036 ms**, p95 **19.086 → 23.472 ms** against the immediate parent and
this build. Shared-host debug timings show substantial variance and are not a
production performance claim. Both matrices completed without request failures.
Evidence: `/tmp/ferrofin-dashboard-l03-checks.json`,
`/tmp/ferrofin-dashboard-l03-coverage.json`,
`/tmp/ferrofin-dashboard-l03-exact-coverage-summary.json`, and the parent fixture
`/tmp/ferrofin-dashboard-l03-ferrofin-qibe2ofs/results.json`.

### Disabling photos removes existing catalog entries on scan

`EnablePhotos=false` already stopped discovering new photos in home-video
libraries, but the stale-item safeguard retained existing photos because their
files still existed. After a scan, those photos remained browsable indefinitely.
The safeguard now recognizes that the saved option invalidates both Photo and
PhotoAlbum resolution. It removes their database rows while preserving files,
video entries and the existing unavailable-location/cascade protections.
Re-enabling photos discovers the same files again under their deterministic IDs.

Source: Jellyfin `4910aafa1a`, `PhotoResolver.Resolve`,
`PhotoAlbumResolver.Resolve` and folder child validation. This option applies on
the next scan; saving the setting does not itself request a scan.

All **eight native observations match Jellyfin 12.1.0**: off/on/off/on for flat
photos and for a nested album. Disabling now removes previously indexed entries,
a new file stays unindexed while disabled, all source files remain present,
and re-enabling restores photos/albums. Native fixtures:
`/tmp/ferrofin-dashboard-l04-ferrofin-kc67wh58/results.json` and
`/tmp/ferrofin-dashboard-l04-ferrofin-kimp8a3o/results.json`.

Validation passes: **279 scanner tests**, **43 location/adoption/watcher scan
tests**, SQL boundary, formatting, strict workspace Clippy and server build.
The new regressions cover album removal, video preservation, re-enabling and
an unavailable media root: disabling does not prune it until it is reachable.

Core's full coverage run passes **2,162 tests** and retains four host inotify
watcher failures. The separate threshold passes; the 22 current test binaries
report **95.00%** (102,114 / 107,493 lines), with 19 LLVM mismatched-function
warnings. These host-dependent failures remain the [Library management and media paths](#library-management-and-media-paths) workspace limitation.

The native server's measured complete scan times for initial-off, on, off with
a new file, and on again were respectively **177 → 188 ms**, **619 → 630 ms**,
**173 → 186 ms**, and **194 → 208 ms**. Each is one small-fixture debug scan on a
shared host; these observations verify the changed path and do not establish
production performance. The disabled scan now performs the required deletion
of catalog rows instead of retaining them.

Evidence: `/tmp/ferrofin-dashboard-l04-checks.json`,
`/tmp/ferrofin-dashboard-l04-coverage.json`,
`/tmp/ferrofin-dashboard-l04-exact-coverage-summary.json`, and parent fixture
`/tmp/ferrofin-dashboard-l04-ferrofin-l5dmrmg9/server.log`.

### Realtime monitoring follows saved library options

`EnableRealtimeMonitor` already filters virtual-folder locations, and the
LibraryOptions HTTP handler stops and restarts the monitor after saving. The
restart reads current options. This review additionally found that overlapping
and repeated locations were each recursively watched. The monitor now sorts
roots and suppresses roots covered by an earlier parent, using directory
boundaries and invariant ordinal casing, as Jellyfin does.

Source: Jellyfin `4910aafa1a`, `LibraryMonitor.Start`,
`IsLibraryMonitorEnabled`, and `ContainsParentFolder`.

The 50 focused monitor/virtual-folder tests pass, including new regressions for
saved on/off/on changes, a shared path still needed by another enabled library,
all libraries disabled, duplicate roots, descendants and similarly prefixed
siblings. Formatting, SQL boundary, strict workspace Clippy and build pass.

The rebuilt native server passed off/on/off through real HTTP: newly created
files stayed absent with monitoring disabled, appeared without a manual scan
after enabling it, and stayed absent after disabling it again. The initial
parent-build attempt could not create an inotify watch because the host quota
was exhausted; the rebuilt run succeeded after the host condition cleared.
No host limits or unrelated processes were changed.

Single-request option-save timings were off **3.43 → 3.77 ms**, on
**3.67 → 3.81 ms**, and off again **3.48 → 4.41 ms**. These small debug fixture
observations include different host watch availability and are not a production
performance comparison. Production code changes only reduce redundant watches.

Native evidence: `/tmp/ferrofin-dashboard-l05-ferrofin-35xrmhj_/results.json`
(parent) and `/tmp/ferrofin-dashboard-l05-ferrofin-jk2x7y9c/results.json` (after).
Checks: `/tmp/ferrofin-dashboard-l05-checks.json`.

Core coverage: **2,164 tests pass**, with the same four intermittent host
inotify failures. The current 22 binaries report **95.00%** line coverage
(102,182 / 107,563); the exact export retains LLVM mismatched-function warnings.
The broad stale-binary report was stopped after the current-binary export
passed. This does not make the outstanding workspace watcher gate green.
Evidence: `/tmp/ferrofin-dashboard-l05-exact-coverage-summary.json` and
`/tmp/ferrofin-dashboard-l05-ferrofin-core-coverage.log`.

### Automatic series grouping uses invariant Unicode casing

Grouping already follows `EnableAutomaticSeriesGrouping` when series identities
are persisted. Its name fallback used Rust full lowercase, which differs from
.NET invariant lowercase: `ΟΣ` became `ος` instead of `οσ`. Two locations holding
`ΟΣ` and `οσ` consequently appeared as separate series even with grouping on.
The fallback now uses the existing invariant-casing helper. Provider IDs, item
IDs, language/library suffixes and disabled-grouping behavior retain their rules.

Source: Jellyfin `4910aafa1a`, `Series.CreatePresentationUniqueKey`,
`GetNameBasedGroupingKey` and `AddLibrariesToPresentationUniqueKey`.

All ten native observations match Jellyfin 12.1.0: five phases each for ordinary
and Greek names. Initial grouping joins both episodes; saving false leaves
persisted grouping until a scan splits it; saving true leaves it split until a
scan joins it. `SeriesCount` counts the two physical copies in both servers,
even when the browse list contains one group. Existing libraries need a scan
to recompute these persisted keys.

Validation: 274 focused normal core tests and 285 focused instrumented tests
pass, including invariant sigma and dotted-I cases. Formatting, SQL boundary,
strict workspace Clippy and server build pass. The separate current-binary core
coverage threshold is **94.84%** (101,952 / 107,496 lines), combining the full [Realtime monitoring follows saved library options](#realtime-monitoring-follows-saved-library-options)
profiles with this change's focused run; 29 LLVM function mismatch warnings
remain. [Realtime monitoring follows saved library options](#realtime-monitoring-follows-saved-library-options)'s four host watcher failures remain an outstanding workspace gate.

Two alternating parent/updated native runs, each with three complete scans of
the same two-episode Unicode fixture, measured median **339.5 → 360 ms** (six
scans each; ranges **296–645 → 256–658 ms**). These are debug observations on a
shared host, not production performance numbers. The changed result is required:
the enabled phases now return one correctly merged series.

Evidence: `/tmp/ferrofin-dashboard-l06-checks.json`,
`/tmp/ferrofin-dashboard-l06-exact-coverage-summary.json`,
`/tmp/ferrofin-dashboard-l06-timings.json`,
`/tmp/ferrofin-dashboard-l06-unicode-ferrofin-tye6lpbq/results.json`, and
`/tmp/ferrofin-dashboard-l06-ferrofin-7vqiufnz/results.json`.

### Metadata language and country reach the providers

Library language and country now inherit from the item's nearest explicit
parent setting and then the live server defaults. Blank library preferences
inherit; changing the server setting takes effect on the next scan without a
restart. The scanner caches ancestor locale fields for that pass and keys
TMDB season responses by language, preventing one translated season response
from being reused for a different locale.

TMDB search, details, episodes, seasons and artwork receive the resolved
language. Movie and series ratings use the requested country's certification,
including Germany's FSK prefix, with the provider's US fallback. TVDB series
and episode names/overviews select translations and skip aliases; ratings use
the requested country's three-letter code. Fanart requests rank artwork using
the item's resolved language. Music metadata/artwork resolve ancestor settings
too. The pathless season/episode refresh shares the resolved TMDB locale.

Sources: Jellyfin `4910aafa1a`, `BaseItem.GetPreferredMetadataLanguage` and
`GetPreferredMetadataCountryCode`, and the TMDB providers; TVDB plugin
`5c4592f`, translated-name/overview selection and country ratings; fanart
`GetImages` ordering. TVDB uses the plugin's default empty fallback-language
list and disabled original-language fallback. Configuring those plugin-specific
options remains “Built-in provider configuration pages” (open); completing this finding does not certify the separate server
settings and by-name-item paths in [Default metadata language (`PreferredMetadataLanguage`)](#default-metadata-language-preferredmetadatalanguage)/M02.

Validation: 458 focused core/provider tests, the new native HTTP integration
test, SQL boundary, formatting, server build and strict workspace Clippy pass.
The HTTP test scans a pinned movie against a recording local provider:
server fr/FR produces French metadata and FR rating, library de/DE overrides
both, then clearing the library preferences and saving server es-419/AR
produces the normalized es-AR request and AR rating without restarting.
Mock provider regressions cover TVDB translation/country selection, country
rating fallback, ancestor precedence, language-separated season caches and
fanart language/width/rating priority and request isolation.

Fresh separate coverage meets both thresholds: core **94.99%**
(102,605 / 108,018 lines, 22 current binaries) and providers **93.71%**
(21,370 / 22,804 lines, four current binaries). The full core run passed
2,169 of 2,173 tests; four existing `notify_watcher` tests fail because the
host reports its OS file-watch limit. All 760 provider tests pass (four skips).
LLVM reports 19 core function-data mismatches; this is not a clean workspace
gate while the host watcher failures remain.

Measured native debug scans on the same two-episode fixture: parent median
**307 ms** (261–357), final build **353 ms** (281–409), three scans each.
Providers were enabled but outbound requests failed quickly through a local
closed proxy, exercising locale/ancestor lookup without external network
variance. These shared-host observations do not measure successful provider
latency; no production performance claim is made. The HTTP integration test
separately verifies successful provider responses.

Evidence: `/tmp/ferrofin-dashboard-l07-checks.json`,
`/tmp/ferrofin-dashboard-l07-coverage.json`,
`/tmp/ferrofin-dashboard-l07-timing-ferrofin-w2jnr0xe/server.log`, and
`target/dashboard-test-tmp/ferrofin-dashboard-l07-timing-ferrofin-3odltk_o/server.log`.

### Automatic metadata refresh interval

The existing implementation correctly consumes `AutomaticRefreshIntervalDays`.
A positive interval makes remote metadata due once the stored refresh timestamp
is old enough; zero and negative values disable this age trigger. A scan or
explicit refresh must still run: the field does not create a per-library timer.

Source: Jellyfin `4910aafa1a`, `MetadataService.RefreshMetadata` and
`RefreshWithProviders`; Ferrofin `refresh_plan::remote_due` and the scanner's
per-item refresh planning. No production change was needed.

Validation: all 83 provider refresh-plan tests pass. The existing scanner
integration test `an_elapsed_refresh_interval_refetches` verifies a new remote
request, probing and timestamp update. Two isolated native HTTP runs, Ferrofin
and Jellyfin 12.1.0, agree on all four observations: an item aged 60 days does
not refresh with zero or negative intervals, refreshes at 30 days, and does not
refresh again while fresh. Saved library options were changed over HTTP; only
the disposable fixture's timestamp was aged directly in SQLite.

Evidence: `/tmp/ferrofin-dashboard-l08-tests.log`,
`/tmp/ferrofin-dashboard-l08-ferrofin-85xet766/results.json`, and
`/tmp/ferrofin-dashboard-l08-jellyfin-ndjxeqvw/results.json`.

### Season zero uses the saved display name

Physical and virtual season-zero discovery now uses `SeasonZeroDisplayName`.
Subsequent scans and direct season refreshes apply the saved name, preserving
item locks, Name field locks and ordinal case-only differences. Empty configured
names are retained. Episodes take their finalized parent season's actual name
when they refresh, including a locked custom name; refreshing only the season
does not rewrite its episodes.

Source: Jellyfin `4910aafa1a`, `SeasonResolver`,
`SeriesMetadataService.GetValidSeasonNameForSeries`, and
`SeasonMetadataService.BeforeSaveInternal` / `EpisodeMetadataService.BeforeSaveInternal`.

Validation: all 449 focused core/provider tests pass, including physical and
virtual quiet rescans, name locks, Unicode casing and empty names. Fifty native
HTTP observations match Jellyfin 12.1.0: five series across initial discovery,
saved-but-unscanned options, scan, case-only change and direct virtual-season
refresh, checking both season names and episode labels. Formatting, SQL
boundary, server build and strict workspace Clippy pass.

Separate coverage using the full [Metadata language and country reach the providers](#metadata-language-and-country-reach-the-providers) baseline and 449 newly instrumented tests:
core **94.70%** (102,341 / 108,070 lines), providers **93.72%**
(21,396 / 22,830). Current binaries only; 19 core LLVM function-data mismatch
warnings remain. The four host file-watch-limit failures in the full [Metadata language and country reach the providers](#metadata-language-and-country-reach-the-providers) core
run remain an outstanding workspace gate.

Native parent/updated debug runs on identical five-episode fixtures measured
three complete scans each: median **333 → 390 ms**, ranges **316–376 → 342–396**.
These small shared-host samples include the required additional name updates;
they are observations, not production performance estimates.

Evidence: `/tmp/ferrofin-dashboard-l09-checks.json`,
`/tmp/ferrofin-dashboard-l09-coverage.json`, `/tmp/ferrofin-dashboard-l09-native.log`,
and `target/dashboard-test-tmp/ferrofin-dashboard-l09-{ferrofin-zp68uhfy,jellyfin-kp4x_3cc}/results.json`.

### Embedded video titles and episode information

The scanner now honors `EnableEmbeddedTitles`, the additional
`EnableEmbeddedExtrasTitles` gate for extras, and `EnableEmbeddedEpisodeInfos`.
Video probing updates the existing metadata before local-provider merging and
respects item and Name locks for titles. NFO episode numbers override probe
numbers. The generic video prober still fills missing episode numbers regardless
of the episode-info switch, matching upstream.

Source: Jellyfin `4910aafa1a`, `FFProbeVideoInfo.FetchEmbeddedInfo`,
`Episode.BeforeMetadataRefresh`, `MetadataService.RefreshMetadata`, and
`LibraryManager.FindExtras`. The episode pre-refresh gate requires a stored
container equal to `mp4`; a normal comma-separated ffprobe format list does not
qualify. Extras can return to their filename on a quiet scan when the prober
does not run. Both behaviors are confirmed against the native reference.

Validation: 412 focused core tests pass, including all change-detection
integration tests, title and episode gates, locks, NFO precedence, and missing
metadata refresh. Four native HTTP phases match Jellyfin 12.1.0 exactly across
five movie libraries and two episode libraries: initial scan, quiet scan,
changed-file missing-metadata refresh, and persisted `mp4` container refresh.
Formatting, SQL boundary, server build and strict workspace Clippy pass.

Separate core coverage using the [Season zero uses the saved display name](#season-zero-uses-the-saved-display-name) baseline and 412 newly instrumented tests:
**94.73%** (102,591 / 108,297 lines), using 22 current binaries. Existing LLVM
function-data mismatch warnings remain. The four host file-watch-limit failures
in the full [Metadata language and country reach the providers](#metadata-language-and-country-reach-the-providers) core run remain an outstanding workspace gate.

Native parent/updated debug runs on the same fixtures measured four completed
scan operations each: median **677 → 687.5 ms**, ranges **249–1,195 → 191–842**.
These small shared-host observations are not production performance estimates.

Evidence: `/tmp/ferrofin-dashboard-l10-checks.json`,
`/tmp/ferrofin-dashboard-l10-coverage.json`,
`/tmp/ferrofin-dashboard-l10-native-final.log`, and
`target/dashboard-test-tmp/ferrofin-dashboard-l10-{ferrofin-_nxqlyfb,jellyfin-_6wdidwh}/results.json`.

### Automatic collections after library validation

`AutomaticallyAddToCollection` now drives a post-scan pass. Eligible movies in
enabled libraries are grouped by exact collection name, excluding virtual items
and alternate versions. At least two distinct movies are required to create a
collection; one can join an existing collection. Membership is additive, so
turning the option off preserves collections and previous membership. Ordinary
item refreshes do not run the global collection task.

NFO `<set>` names now reach the stored `CollectionName`, alongside the existing
TMDB mapping. The server shares its real collection manager through a weak
reference, avoiding a scanner/collection/library ownership cycle. The existing
scan-pass histogram gains one bounded `pass` value, `collections`.

Source: Jellyfin `4910aafa1a`, `CollectionPostScanTask` and `MovieNfoParser`.
Validation: 466 focused core tests pass, including NFO-to-scan collection
creation, library scope, exact names, alternate/virtual exclusions, repeated
passes, cancellation, and paging beyond 1,000 movies. All five native HTTP
phases match Jellyfin 12.1.0: off, on, existing single-member collection,
off with a new movie, and on again. Formatting, SQL boundary, server build,
and strict workspace Clippy pass.

Separate core coverage: **94.72%** (102,961 / 108,701 lines), combining the [Embedded video titles and episode information](#embedded-video-titles-and-episode-information)
baseline with 466 newly instrumented tests and exporting 22 current binaries.
LLVM function-data mismatch warnings persist; the four host file-watch-limit
failures in the full [Metadata language and country reach the providers](#metadata-language-and-country-reach-the-providers) core run remain an outstanding workspace gate.

Native parent/updated debug fixtures each run five complete scans. Initial
medians were **342 → 487 ms** (ranges **330–459 → 376–6,613**); the updated
outlier spent 6,253 ms in collections while compilation was active. A repeat
measured **458 → 353 ms** (ranges **345–1,043 → 335–440**), with reference parity
unchanged. These shared-host observations do not establish production latency
or large-library collection throughput.

Evidence: `/tmp/ferrofin-dashboard-l11-checks.json`,
`/tmp/ferrofin-dashboard-l11-coverage.json`,
`/tmp/ferrofin-dashboard-l11-native.json`,
`/tmp/ferrofin-dashboard-l11-{parent,updated}-repeat.log`, and
`target/dashboard-test-tmp/ferrofin-dashboard-l11-{ferrofin-4_xq6o7t,jellyfin-u_dn91in}/results.json`.

## Libraries: metadata and image providers

### Metadata provider selection and order

The existing shared gates honor per-type enabled providers, including an empty
selection, and inherit server options only when the library has no per-type
entry. The scan folds enabled providers in configured/default/registration
order; Identify keeps the first result when providers share an external id.

Configured ranking now uses exact provider names, matching Jellyfin
`4910aafa1a`, `ProviderManager.GetConfiguredOrder` (`Array.IndexOf`). Enable
lists remain case-insensitive. Identify uses the same ranking helper as the
scanner and image selection instead of a separate case-insensitive lookup.

Validation: all **767 provider tests** pass (four environment-gated skips),
including selection, empty/absent inheritance, defaults, exact ranking and
Identify result precedence. Seven scanner metadata/artwork gate-and-order tests
pass. The real-server HTTP regression saves four live selections/orders and
verifies Identify: cleared selection, case-insensitive enabling, reversed
provider order, and an incorrectly cased order entry. Existing locale HTTP
checks also pass. Formatting, SQL boundary, build and strict workspace Clippy
pass. Separate provider coverage: **93.73%** (21,421 / 22,855 lines), from its
full instrumented suite and four current binaries.

This closes selection/ranking itself. Provider execution coverage is a separate
remaining gap: the path-less episode refresh implementation explicitly lacks
OMDb/TVDB metadata execution even though filesystem episode scanning supports
them. The living document records that as [Path-less episode metadata providers](#path-less-episode-metadata-providers); this finding does not certify
those unfinished provider implementations.

Evidence: `/tmp/ferrofin-dashboard-l12-checks.json`,
`/tmp/ferrofin-dashboard-l12-coverage.json`,
`/tmp/ferrofin-dashboard-l12-scan-gates.log`, and
`apps/ferrofin-server/tests/dashboard_metadata_locale.rs`.

The subsequent full scanner regression also corrected two older plugin-order
fixtures to distinguish exact configured ranks from case-insensitive enabling.
All 468 focused scanner/persistence tests and strict workspace Clippy pass with
those expectations.

### Image provider selection and order

Automatic image refresh now uses the providers enabled for the item's type,
independently of metadata selection. Remote, named WASM and embedded audio
providers participate in the same configured ordering. Default order depends
on item type: for example TMDB precedes FanArt for movies, while FanArt precedes
TMDB for series. Later providers fill image types the earlier ones did not
supply. This applies to the filesystem scan, music post-pass and refresh of
items without a media path. The server now attaches the registered WASM image
implementations to that last path too.

The scanner resolves TVDB series/episode artwork and OMDb posters when the
metadata provider did not run. TVDB season artwork uses the selected display
order with official-order fallback for an empty result. TMDB seasons use their
image listing. Episode TVDB artwork resolves through the parent series and
numbering, including absolute-order specials; an episode's own TVDB id does
not override that lookup. Existing metadata stills are reused only for the
same ordering.

Manual Choose Image deliberately includes disabled providers, matching
Jellyfin's controller, and retains configured provider order. TVDB series,
season and episode providers now appear in that dialog. Its language tags use
the localization culture table and TVDB's Chinese/Portuguese mappings. Dynamic
WASM providers return bytes during acquisition and are not remote URL choices.

Sources: Jellyfin `4910aafa1a`, `ProviderManager.GetImageProvidersInternal`,
`CanRefreshImages`, `GetAvailableRemoteImages`, `ItemImageProvider`,
`RemoteImageController`, and the individual image-provider order declarations;
TVDB plugin `5c4592f`, its image providers and `TvdbSdkExtensions`; FanArt plugin
`7c2f8599`, its movie, series, season, artist and album image providers.
The TVDB season reference type is also confirmed against
[TVDB's v4 schema](https://raw.githubusercontent.com/thetvdb/v4-api/master/docs/swagger.yml).

Validation passes: all **805 provider tests**, **479 focused core tests**, the
real-server HTTP regression, formatting, SQL boundary, build and strict
workspace Clippy. Tests exercise plugin-first, remote-first, disabled and
unspecified order in both item and music passes, audio extraction ordering,
and path-less plugin refresh. HTTP saves TVDB selection with metadata disabled,
checks manual listing while automatic providers are disabled, then downloads
and serves the expected bytes for physical and path-less seasons.

Native five-phase scans on identical disposable movie fixtures measured a
median **361 ms before** (351–482) and **357 ms after** (343–483), comparing
`29ee460d` with the completed ordering change. Both return identical collection
results. This is a baseline scan regression check with remote fetchers disabled,
not remote-download latency; Docker remains unavailable. The first remote
provider implementation step separately measured 388 ms before / 359 ms after.

Separate line coverage: providers **93.79%** (21,947/23,400); core **94.71%** (103,304/109,071).
The core run combines its [Automatic collections after library validation](#automatic-collections-after-library-validation) whole-crate baseline with current instrumented
focused tests and current binaries. LLVM function-data mismatch warnings remain
in the evidence logs; this is accumulated coverage, not a new full core run.

Image quantities, minimum widths and file placement remain findings [Automatic artwork limits and minimum widths](#automatic-artwork-limits-and-minimum-widths)/L17.
The living document explicitly keeps [TVDB metadata follows the series display order](#tvdb-metadata-follows-the-series-display-order)–[Embedded images from videos](#embedded-images-from-videos) open: TVDB metadata numbering,
the unregistered FanArt season provider, and video embedded-image extraction.
This finding completes the registered-provider selection and ordering repair; it does
not certify those missing provider implementations.

Evidence: `/tmp/ferrofin-dashboard-l13-{checks,coverage}.json`,
`/tmp/ferrofin-dashboard-l13-ordering-native.json`, matching logs, and
`apps/ferrofin-server/tests/dashboard_metadata_locale.rs`. The first remote
provider step's reports are preserved as `l13-remote-step-{checks,coverage,native}.json`.

### Automatic artwork limits and minimum widths

Automatic artwork acquisition now reads the saved per-type `ImageOptions`,
falling back to Jellyfin's per-item defaults. Zero or negative limits disable
acquisition; singular image types have one slot and Backdrop permits the saved
count. Known remote widths below `MinWidth` are rejected; unknown widths remain
eligible. Dynamic/plugin, audio and existing book-cover extraction use the
positive-limit check without applying a remote-image width constraint.

The filesystem scanner, music pass and path-less refresh apply the same policy.
TMDB acquisition uses complete image listings with dimensions; TVDB dimensions
also survive conversion. Normal refresh avoids providers whose supported slots
are all disabled/full. Local images count toward capacity and remain unaffected
by the size filter. The manual image chooser still lists all candidates.

New backdrops occupy unused files until acquisition succeeds; only then are old
internal backdrops pruned. Failed replacements retain existing images. Multiple
backdrops are persisted together, and singular image types have distinct file
stems. Downloads preserve the returned media type. Forbidden/missing candidates
permit fallback, other failures stop that provider's image-type pass, and the
next provider can still fill the slot. The duplicate-length backdrop check is
retained for acquisition without replacement.

Source: Jellyfin `4910aafa1a`, `ItemImageProvider`, `TypeOptions`, `ImageOption`,
`LibraryOptions`, and the TMDB image providers. Limits apply to automatic image
acquisition, not to local metadata readers that import image references.

Validation passes: **960 model tests**, **815 provider tests**, **488 focused
core tests**, real-server HTTP, SQL boundary, formatting, strict workspace Clippy
and the server build. Separate line coverage passes: model **90.15%** (8,841/9,807), providers
**93.60%** (22,240/23,761), core **94.70%** (103,632/109,436).
The core result combines the previous whole-crate profile with current focused
tests and 22 current binaries. LLVM profile mismatch warnings remain in the
evidence; it is accumulated coverage, not a new whole-core test run.
Results are in `/tmp/ferrofin-dashboard-l14-coverage.json`.
The HTTP regression saves two different limit/width configurations, disables
metadata providers, refreshes images and verifies counts and served bytes.
It compares the acquired backdrop set: existing persistence assigns random GUID
row IDs and exposes indices in ID order, not acquisition order.

Related work remains open: [Artwork saved beside media](#artwork-saved-beside-media) controls destinations; [Embedded images from videos](#embedded-images-from-videos) covers missing video
embedded extraction. Book image-provider registration, configured ordering and
image-only refresh are a separate open implementation gap: the existing book
cover path is coupled to its metadata read. This change gates that path's image
acquisition but does not certify those missing execution paths.

The native five-phase movie-scan fixture returned identical collection results
before and after. Median scan time was **390 ms before** (364–1,174) and
**461 ms after** (336–529). These unisolated runs overlap other host work and
show no performance improvement; this is a baseline regression measurement
with remote fetchers disabled, not image-download latency. Docker was unavailable.
Evidence: `/tmp/ferrofin-dashboard-l14-{checks,native}.json` and matching logs.

### Local metadata reader order

The book library editor now exposes Comic Provider, EPUB Metadata and Open
Packaging Format. Their saved `LocalMetadataReaderOrder` controls which source
wins when a book has competing metadata. The first successful local reader
supplies the metadata; subsequent local readers do not fill individual fields.
An empty or unsuccessful preferred reader falls through to the next one.

An omitted library order inherits the server's per-type order. An explicit
empty array restores registration order. Names match exactly, as upstream's
`Array.IndexOf` does. The existing disabled-reader list is also applied to the
book readers. Nfo remains the only registered local reader for the supported
movie/TV/music paths, so rearranging a singleton has no behavioral effect.

ComicProvider's internal comment/sidecar/archive order remains fixed. Its
external ComicInfo reader is now considered for every Book, including EPUB and
PDF, matching upstream. Book covers remain a separate open execution issue in
[Book covers are image providers, not a side effect of the metadata read](#book-covers-are-image-providers-not-a-side-effect-of-the-metadata-read); local metadata ordering does not close image-provider ordering.

Sources: Jellyfin `4910aafa1a`, `ProviderManager.GetOrCreateOrderedProviders`,
`GetConfiguredOrder`, `MetadataService.ExecuteMetadataProviders`, ComicProvider,
ExternalComicInfoProvider, EpubProvider and OpfProvider. A disposable native
Jellyfin **12.1.0** run corroborates six precedence cases; the source pin remains
12.0-rc7.

Validation passes: **823 provider tests**, **488 focused core tests**, real HTTP,
formatting, SQL boundary, server build and strict workspace Clippy. The HTTP
regression saves reader order, scans conflicting local files, changes precedence,
and checks omitted versus explicit empty library order without restarting.
A separate valid EPUB fixture checks all three readers, case-sensitive ranking
and server fallback against native Jellyfin: all six final names match. Before
the change Ferrofin chose embedded EPUB metadata in all six cases.

Coverage and native timing details are recorded in
`/tmp/ferrofin-dashboard-l15-{checks,coverage,native}.json` and matching logs.

Separate line coverage passes: providers **93.64%** (22,353/23,871), core
**94.70%** (103,638/109,442). The core result combines the [Automatic artwork limits and minimum widths](#automatic-artwork-limits-and-minimum-widths) profile with current
focused tests and 22 current binaries; LLVM reports 41 function-data mismatches.
All instrumented tests pass. This is accumulated coverage.

Native scan medians were **167 ms before** (156–173) and **184 ms after**
(161–244) over the initial scan and six refreshes. Refresh-only medians were
164 / 182 ms. These small, unisolated runs overlap other host work and make no
performance-improvement claim; the reported request-and-wait durations in the
raw fixture include a fixed one-second wait and are not API latency. Docker
was unavailable.

### Automatic NFO saver selection

The selected Nfo saver now runs after persisted metadata edits, automatic and
explicit refreshes, music refreshes, and artwork changes. The composition root
attaches the saver to the provider manager; the scanner invokes it only after
persisting the item's metadata, IDs, people, streams and images. Saver failures
are logged without reversing an otherwise successful edit.

An explicit `MetadataSavers` list takes precedence over legacy and server
settings. Names match without regard to case; an empty list disables writes.
With no list, server `DisabledMetadataSavers` applies, then `SaveLocalMetadata`.
An edit can update an existing sidecar even when the legacy flag is off.
Metadata imports normally do not rewrite NFOs; existing season sidecars retain
upstream's exception. Artwork updates write only when saving image paths is
enabled, apart from that season exception. Force-save and replace-all refreshes
also invoke selected savers when no remote provider answers.

The filesystem writer chooses the upstream destination for movies, mixed-folder
videos, episodes, series, seasons, albums and artists. Unsupported kinds,
channel items and video extras do not generate NFOs. Writes are serialized per
path, compare the existing bytes, and use atomic replacement. The serializer
includes persisted metadata and IDs, field locks, credits, music children,
stream details, artwork paths and the configured user's export data. Unknown
nested custom tags survive; stale owned tags are replaced or removed. Artwork
uses the server's path-substitution mappings as the pinned saver does.

Sources: Jellyfin `4910aafa1a`, ProviderManager.SaveMetadataAsync /
IsSaverEnabledForItem, MetadataService.SaveInternal, BaseNfoSaver and the
Movie/Episode/Series/Season/Album/Artist savers. The disposable native reference
run is Jellyfin **12.1.0**; it corroborates the seven edit-selection scenarios
without changing the source pin.

The separate NFO dashboard findings remain independently open: this finding wires the
export machinery but does not complete selected-user import or validate the
entire NFO settings page. [Artwork saved beside media](#artwork-saved-beside-media) covers artwork file destinations.

Validation passes: **845 provider tests**, **488 focused core tests**, **893 API
tests**, real HTTP, formatting, SQL boundary, server build and strict workspace
Clippy. The HTTP regression covers automatic refresh, case-insensitive selection,
empty selection, existing-file fallback, immediate configuration changes,
custom-tag preservation, image-path suppression and an unwritable sidecar.
Unit regressions also cover per-kind destinations, update thresholds, inherited
XML namespaces, media stream fields, actor thumbnails and user export data.

Separate line coverage passes: providers **92.54%** (23,201/25,070), core
**94.69%** (103,668/109,481), API **86.12%** (40,146/46,616). Core coverage combines
the [Local metadata reader order](#local-metadata-reader-order) profile with current focused tests and 22 current binaries; this is
accumulated coverage, and LLVM reports function-data mismatches. The final
provider/API runs are fresh and all instrumented tests pass. Initial API failures
were the old item-update test double's unimplemented saver; that fixture now
accepts and checks metadata-edit saves.

All seven native edit-selection/custom-tag outcomes match Jellyfin. Comparable
before/after edit timings wait for the initial scan to become idle. The medians
and ranges below measure database edits without/with the new filesystem work,
not an isolated benchmark or a performance improvement. Docker was unavailable.

Raw records: `/tmp/ferrofin-dashboard-l16-{checks,coverage,native}.json`, the
`*-settled.log` fixture logs and the final HTTP regression log.


Native edit median: **4.21 ms before** (3.27–6.20), **9.74 ms after** (4.37–16.11), seven edits each. Runs were unisolated and overlapped validation.

### Artwork saved beside media

`SaveLocalMetadata` now controls the shared artwork destination writer used by
manual uploads, provider-manager downloads, scanner artwork and music refreshes.
The writer resolves the owning library's current options, including physical
location IDs, and reads the current `ImageSavingConvention`. Existing local
artwork is retained; newly acquired managed images use the pinned upstream
media-adjacent filenames. Disabling the option keeps acquisitions internal.

The Legacy/Compatible policy covers mixed folders, movies, music videos,
episodes, physical/virtual seasons, artist/album folders, DVD/Blu-ray folders
and the individual artwork types. Tracks, photos, extras, channel items and
non-primary episode images remain internal. Compatible extra backdrops use
`extrafanart`; the named NFO option enables their `extrathumbs` copies.

Writes use complete staged files and atomic replacement. Adjacent artwork honors
the process umask for new files and preserves existing file permissions while
clearing the read-only owner flag, so shared readers retain access. Single-output media
write failures retain the internal image, while duplicate-output failures are
reported. Replacing an image removes its previous eligible file when the
filename or extension changes. Shared album-cover assets remain owned by all
of their references. Album inheritance publishes the album's selected local
cover while tracks retain their internal shared cover. Identical destination
bytes preserve the file's modification time on unchanged rescans. Indexed
backdrop uploads preserve the other images rather than replacing the whole type.
Image reads and swaps use insertion order, and an indexed replacement updates
its existing row so its identity and slot stay stable. This prevents random UUID
sorting or delete/reinsert from moving the image being addressed.

Indexed deletion remains a separate open work item, **[Indexed artwork deletion](#indexed-artwork-deletion)**: the existing delete
operation ignores the requested slot and removes the whole image type. This
finding establishes destinations and indexed saving, not deletion parity.

Sources: Jellyfin `4910aafa1a`, `ImageSaver.SaveImage`, `GetSavePaths`,
`GetStandardSavePath`, `GetCompatibleSavePaths`, `GetBackdropSaveFilename`, and
`BaseItem.SupportsLocalMetadata` / `Photo.SupportsLocalMetadata`. The native
reference is Jellyfin **12.1.0**, kept separate from the pinned source version.

Validation:

- Formatting, strict workspace Clippy, SQL-boundary checks and server build pass.
- 180 utility tests, 852 provider tests (4 environment skips), 490 focused core
  tests and all **2,218 full-core tests** and the real-server `dashboard_metadata_locale` regression pass. The
  HTTP scenario checks uploads, automatic downloads, live flags/convention,
  old-file removal, write failure, duplicated backdrops and indexed replacement.
- Native disposable-fixture checks use both pre-change `c7831614` and current
  binaries built with debug info and incremental compilation disabled, plus
  Jellyfin 12.1.0. All six after-change file/permission/count/order observations
  match the reference. Before-change files were always internal and the second
  backdrop replaced the first. After-change adjacent files are mode 0644 with
  the fixture's umask and both backdrop slots remain in upload order.
- Four settled Primary uploads measured median **7.36 ms before / 15.40 ms
  after**, ranges 7.20–10.46 / 11.71–26.76 ms. These are unisolated native
  observations on a busy host, including added file writes, not a benchmark
  improvement claim. Docker was unavailable for `bench/`.
- Fresh utility coverage is **96.13%**, provider coverage **91.88%**. A retained
  pre-cleanup core profile did not provide usable coverage with the reduced
  build settings. The complete fresh core run passes at **95.14%**, without LLVM
  export warnings. All three crates exceed their separate 80% line gates.
- The full-suite check exposed an outdated episode fixture: it counted
  `/episode/.../images` as episode metadata requests. Its route dispatch now
  distinguishes image-list requests; both normal regression tests pass without
  weakening their metadata request-count assertions. Strict workspace Clippy
  was rechecked after this fixture correction and passes.

Evidence is retained under `/tmp/ferrofin-dashboard-l17-`: `checks.json`,
`coverage.json`, `native-{before,after,reference}.json`, and
`bounded-{before,after,reference}.log`. A reused Cargo fingerprint after the
baseline snapshot produced a restore-build error; generated local-crate outputs
were cleaned and the current-source build then passed. This did not affect the
separately retained binaries used for the measurements.

The disk cleanup preserved source and history and removed only generated
`target/` artifacts. Subsequent validation uses one process at a time, no
incremental/debug output, a 30 GiB target budget, 512 GiB host free-space reserve
and pooled coverage profiles removed after export.

### Similar-item provider selection and order

Verified complete. The shared owning-library lookup introduced in [Library management and media paths](#library-management-and-media-paths) already
corrected the original physical-location lookup finding; this change adds
regressions without changing production similarity logic.

A real virtual-folder manager and SQLite store verify an adopted-style top
parent distinct from the collection folder, path fallback, live saved options,
case-insensitive enable/order names, explicit order, selection-order fallback
and clearing remote selection without restarting. Local similarity remains
available regardless of the remote selection, matching the pinned source.

The real HTTP scenario creates a second movie with a distinct TMDB ID. An
unchecked provider makes no remote request; enabling TMDB places its referenced
movie first; disabling it stops further remote/cache execution. Existing tests
cover local-versus-remote ordering, unchecking the local entry, cache behavior,
duplicates, visibility and provider failures.

Source: Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/Library/SimilarItems/SimilarItemsManager.cs`,
`GetSimilarItemsAsync` and `GetConfiguredSimilarProviderOrder`. Remote providers
are opt-in, local providers always participate, ranking ignores case and an
empty explicit order falls back to the enabled-provider list.

Validation: 34 similarity tests pass normally and instrumented; the extended
real-server HTTP test passes; formatting and strict workspace Clippy pass.
Separate core line coverage is **95.14%**, using the fresh [Artwork saved beside media](#artwork-saved-beside-media) full-core baseline
plus these current tests, with no LLVM export warnings.

Native disposable HTTP fixtures exercise saved disabled/enabled/disabled
selection on a movie without a remote provider ID. Fourteen settled requests
per phase measure median milliseconds before/after: **3.48/3.43**, **2.67/2.76**,
**2.70/4.18**. All responses contain the expected empty result. These unisolated
observations make no performance improvement claim; production logic did not
change and Docker was unavailable for `bench/`. The mock-backed HTTP scenario,
rather than this no-ID timing fixture, verifies remote result selection.

Evidence: `/tmp/ferrofin-dashboard-l18-{checks,coverage,native}.json`,
`native-{before,after}.json` and the corresponding logs. Builds remain serialized
with debug/incremental output disabled and the 30 GiB/512 GiB storage budgets.

## Libraries: subtitles, lyrics, music analysis and media segments

### Allowed embedded subtitles

The saved `AllowEmbeddedSubtitles` mode now filters embedded video subtitles on
the next successful probe. AllowText retains text, AllowImage retains image,
AllowNone removes both, and AllowAll or an unknown enum value retains both.
External subtitles, other stream types and audio probing remain unchanged.
Filtering preserves the already assigned stream indexes, including gaps, and
a successful probe that filters every stream clears the previous stored rows.
An absent or failed probe preserves them.

Automatic scan downloads use the raw embedded streams before this display
filter, matching Jellyfin's ordering. Hidden text still satisfies a language;
hidden image subtitles satisfy it only when the embedded-skip option is set.
External streams are read afresh for each language so subtitles attached since
the probe began, including earlier downloads, are not lost or downloaded twice.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs`, `Fetch` and
`AddExternalSubtitlesAsync`; filtering follows external merging and numbering.
The existing MediaStream classifier supplies text/image semantics.

Validation: all **348 focused core tests** pass instrumented. The initial normal
run passed 347 and found an invalid parent foreign key in the new SQLite fixture;
that fixture was corrected and its regression passes normally. Real-server HTTP
checks all five saved mode transitions with text/image/external subtitles.
Formatting, SQL boundary, server build and strict workspace Clippy pass.
Separate core line coverage is **94.90%**, using the fresh full-core baseline
plus current focused tests, with no LLVM export warnings. The normalizer drops
codec-less raw probe streams; the direct core cases additionally test their
classification when supplied in MediaInfo.

Native HTTP uses a tiny real MKV containing English text and an external English
SRT. After-change stream types, codecs, external flags and indexes match the
Jellyfin **12.1.0** reference in All/Text/Image/None/All. Before-change incorrectly
exposed the embedded text in every mode. Refresh completion is observed through
an advancing Etag. Before/after milliseconds for these five operations are
**237.96/246.61, 212.27/222.60, 218.62/225.22, 214.48/215.31,
218.27/241.20**. These unisolated local observations make no performance
improvement claim; Docker was unavailable for the isolated benchmark suite.

Evidence: `/tmp/ferrofin-dashboard-l19-{checks,coverage,native}.json`, native
before/after/reference JSON and corresponding logs. Builds remain serialized
with debug/incremental output disabled, pooled coverage profiles, a 30 GiB target
limit and a 512 GiB host reserve. Generated artifacts occupy about 10 GiB.

### Automatic subtitle languages and matching controls

Automatic downloads now distinguish matching text from image subtitles.
Matching text satisfies its language; a matching external image subtitle still
needs text, and the embedded-skip switch applies only to embedded subtitles.
The downloader retains default-audio matching, configured languages and perfect
hash matching, including live changes to each saved option.

Eligibility now excludes disc formats, channel livestreams and active DVR
captures. The DVR manager is held weakly to avoid a library ownership cycle.
The daily task resolves the owning library, including nested and adopted paths,
and applies upstream's language candidate preselection: any matching audio
track when audio skipping is enabled, and external/all matching subtitles
according to the embedded-skip flag. The downloader's final check still uses
default audio and text rules. The task configures SourceTypes=Library as in
upstream; neither repository currently consumes that query field, so this is
not a claim of an additional source-type filter.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/MediaInfo/SubtitleDownloader.cs`,
`SubtitleScheduledTask.cs`, `MediaBrowser.Controller/Entities/Video.cs` and
`MediaBrowser.Model/Entities/MediaStream.cs`.

Validation: **48 focused core tests** pass normally and instrumented, including
real SQLite/provider downloads and live saved nested-library options. Real
HTTP checks required/relaxed perfect matching, both skip toggles against the
raw filtered probe snapshot, external VobSub acquiring text, disabled providers
and empty languages. The existing subtitle scan helpers run independently to check existing sidecars,
quiet rescans, provider outages and failed probes; core tests cover cancellation. Formatting,
SQL boundary, server build and strict workspace Clippy pass. Separate core
line coverage is **94.32%**, with no LLVM export warnings.

Native before/after HTTP uses a tiny real MKV with French audio and Spanish PGS,
an external Portuguese PGS, and a local mock provider behind a temporary TLS
CA trusted only by the server child. All eleven phases complete. Before-change
skips the needed Portuguese text; after-change downloads it and preserves the
image subtitle. Other matching/perfect/skip/language cases retain their expected
outcomes, and manual search retains upstream's independent provider defaults.
The first six comparable refreshes have median milliseconds **239.04 before /
243.36 after**, ranges **214.89–262.34 / 215.10–282.08**. The external-image case
changes the work performed, so its **336.01 / 323.71 ms** is not a like-for-like
latency comparison. These unisolated observations make no improvement claim.
Docker was unavailable; the native Jellyfin distribution lacks the OpenSubtitles
plugin, so remote provider parity uses pinned source and mock-backed HTTP.

Evidence: `/tmp/ferrofin-dashboard-l20-{checks,coverage,native}.json`, native
before/after JSON and corresponding test/native logs. The initial TLS fixture
incorrectly used its CA as a leaf; that invalid run was replaced with a proper
CA-signed leaf before recording these measurements. Build storage remains
bounded to 30 GiB with a 512 GiB host reserve; current target is about 10 GiB.

The broader pre-existing scan matrix is not green: it expects a metadata-detail
poster URL to be downloaded without providing the separate image-list response.
A temporary corrected image fixture then exposed its stale mixed-folder watcher
counts before reaching subtitles. Those exploratory changes were removed; the
independent subtitle test supplies its own probe-outage setup. Repair the full
image-list/mixed-folder matrix before claiming the next workspace gate passes.

### Subtitle provider selection and order

Automatic subtitle searches now use an independent first-success flag. Manual
searches retain the source default of querying every provider concurrently and
returning candidates in selected order. Automation alone no longer changes the
search-all behavior. Provider errors and empty results allow ordered fallback.

Registered providers first sort by intrinsic order. Saved SubtitleFetcherOrder
uses exact display names; DisabledSubtitleFetchers remains case-insensitive.
Movie/Episode support filters both actual-item searches and dashboard provider
descriptors. Unsupported kinds and missing descriptor items return no providers;
actual library lookup errors propagate. Generic searches retain their requested
media type when no item resolves.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Subtitles/SubtitleManager.cs` and
`src/Jellyfin.Extensions/ReadOnlyListExtension.cs`. The interactive video overload
uses default provider selection independently of a library's automatic settings.
[Subtitle provider namespaces](#subtitle-provider-namespaces) records the existing readable provider-ID namespace difference from upstream
MD5 IDs; [Saved item metadata in subtitle searches](#saved-item-metadata-in-subtitle-searches) records incomplete ProviderIds/episode-range enrichment. These gaps
remain open and are not counted as fixed by this setting.

Validation: **52 focused core tests** pass normally and instrumented, including
a concurrency barrier that requires simultaneous searches, exact saved casing,
intrinsic order, failure/empty fallback, all-disabled silence, media-type
filtering and independent automation/search-all flags. Real HTTP saves provider
settings, checks disabled/re-enabled automatic calls and preserves manual
selection. The independent scan/rescan/probe-failure test also passes. Formatting,
server build and strict workspace all-target/all-feature Clippy pass. Separate
core line coverage is **94.33%** (103,992/110,246), with no LLVM export warnings;
unchanged coverage is seeded from [Automatic subtitle languages and matching controls](#automatic-subtitle-languages-and-matching-controls) and current affected tests are rerun.
The broader scan matrix remains the separately documented [Repair the complete scan-change fixture](#repair-the-complete-scan-change-fixture) validation repair.

Native before/after uses the eleven-phase mock-backed HTTP fixture from [Automatic subtitle languages and matching controls](#automatic-subtitle-languages-and-matching-controls).
The before observations are reused from the exact [Automatic subtitle languages and matching controls](#automatic-subtitle-languages-and-matching-controls) committed binary retained
as dashboard-l21-parent; after observations come from the rebuilt binary.
All configured provider/language/skip outcomes pass, and manual search returns
one candidate on both builds. Disabled-provider refresh is **364.08/338.66 ms**,
re-enabled saved-order refresh **364.83/372.25 ms**, and manual search
**5.71/5.49 ms**, before/after respectively. Single-provider native observations
cannot measure multi-provider fan-out; the concurrency regression covers that
contract. These unisolated numbers make no performance improvement claim.
Docker is unavailable and the native Jellyfin package lacks OpenSubtitles, so
remote provider parity uses the pinned source and local provider mock.

Evidence: `/tmp/ferrofin-dashboard-l21-{checks,coverage,native}.json`, native
before/after JSON, and corresponding logs. Builds remain serialized, with no
incremental/debug artifacts, a 30 GiB target cap and 512 GiB free-space reserve.

### Subtitle destinations and collision handling

Subtitle uploads and provider downloads now read the live owning library's
SaveSubtitlesWithMedia option. Disabled storage selects the item's internal
metadata directory; enabled storage selects the media directory. The selected
write failure propagates before recording a stream. There is no silent retry
in the pinned SubtitleManager. Nested/adopted physical parents resolve through
the shared owning-library helper, and internal paths follow the live metadata root.

Existing subtitle bytes survive repeated writes. Exclusive new-file opens use
numbered collision suffixes. Names lowercase language/format and include forced
then SDH flags. The nine supported formats match NamingOptions; unsupported
formats and path separators in language return invalid-input errors. Image
subtitle codecs stay image codecs; MKS containers await actual probing.

Source oracle: Jellyfin `4910aafa1a`,
MediaBrowser.Providers/Subtitles/SubtitleManager.cs (UploadSubtitle,
TrySaveSubtitle, TrySaveToFiles), Emby.Naming/Common/NamingOptions.cs,
MediaBrowser.Controller/Entities/BaseItem.cs and MediaInfoResolver.cs.

Validation: **56 focused core tests** pass normally and instrumented, including
live destination/root changes, owning-library selection, bytes preserved across
collisions, exclusive concurrent writes, selected-directory failure, formats
and stream codecs. Real HTTP uploads six subtitles across live switches and
checks both files and persisted streams; failed writes/invalid inputs add no
stream. That fixture first refreshes removed [Automatic subtitle languages and matching controls](#automatic-subtitle-languages-and-matching-controls) sidecars to reconcile its
previous persisted rows. The independent subtitle rescan/probe-failure test,
SQL boundary, formatting, server build and strict workspace Clippy pass.
Core coverage is **94.34%** (104,299/110,562), with no LLVM export warnings;
unchanged coverage is seeded from [Subtitle provider selection and order](#subtitle-provider-selection-and-order) and affected tests are rerun.

Native before/after/reference runs nine local HTTP scenarios. After-change
statuses, filenames and exact file contents match native Jellyfin **12.1.0**
through off/on/off, collisions, combined forced/SDH, blocked media, unsupported
format and hostile language. Before-change overwrites the same media file,
retries blocked media internally, and accepts invalid formats/languages.
After-change creates six files and retains six streams across all failures.
Jellyfin queues stream probing, so its immediate stream snapshots are not
settled equality evidence; the independent HTTP regression checks persistence.

First media upload is **5.87/3.83 ms** and subsequent media collision
**4.91/5.20 ms**, before/after. The five ordinary successful uploads range
**3.42–6.04/3.83–5.20 ms**. Work differs for collisions/destinations and newly
rejected cases; these unisolated observations make no improvement claim.
Docker is unavailable. The reference uses the existing temporary .NET 10 runtime,
not the host runtime; an initial host-runtime launch failure was replaced.

Evidence: `/tmp/ferrofin-dashboard-l22-{checks,coverage,native}.json`, native
before/after/reference JSON and logs. Builds remain serialized with a 30 GiB
target cap and 512 GiB host reserve; target is approximately 11 GiB.
[Repair the complete scan-change fixture](#repair-the-complete-scan-change-fixture) remains the separately tracked broader scan-matrix fixture repair.

### Live lyric destinations

Lyric uploads and provider downloads now resolve the item's owning library
through the shared resolver. This fixes nested locations and adopted physical
parent IDs that previously selected the first matching library. Each save reads
the live SaveLyricsWithMedia option and metadata root. Enabled media storage
tries the media directory first and falls back internally if that write fails;
disabled storage uses the internal directory directly.

The original supported classification was correct for a single ordinary
library, but missed the ownership defect. Source oracle: Jellyfin
`4910aafa1a`, MediaBrowser.Providers/Lyric/LyricManager.cs, TrySaveLyric and
TrySaveToFiles; the latter explicitly retries the internal destination.

Validation: 24 focused core tests pass, including nested/adopted ownership,
off/on/off changes, live root replacement, selected-target failure and existing
parser/provider/task regressions. Real HTTP verifies saved library options,
uploaded lyric contents and exact files through all four destination phases.
SQL boundary, formatting, server build and strict workspace Clippy pass.

Native before/after/reference checks agree on destination files and read-back
text in all four phases. The reference is Jellyfin 12.1.0, distinct from the
source pin. Its upload queues metadata refresh, so the fixture waits for the
lyric stream to settle before checking read-back or deleting the next fixture.
The initial immediate-read reference attempt raced that queue; its replacement
passes. The HTTP upload response alone is timed, excluding settlement polling.

Before/after upload observations are 4.23/4.50 ms (internal), 8.93/5.58 ms
(media), 4.88/4.80 ms (new internal root), and 6.25/4.28 ms (blocked media with
fallback). These are single unisolated local observations, with no performance
improvement claim; Docker is unavailable.

Evidence: `/tmp/ferrofin-dashboard-l23-{checks,coverage,native}.json` and the
corresponding focused-test, native before/after/reference and coverage logs.
Core coverage is **94.37%** (104,437/110,668), with all 24 affected tests
rerun instrumented, unchanged coverage seeded from [Subtitle destinations and collision handling](#subtitle-destinations-and-collision-handling), and no LLVM export
warnings. Builds remain serialized under the 30 GiB target cap and 512 GiB host reserve.

### Lyric provider selection and order

Scheduled lyric downloads now construct automated requests using the live
owning library's DisabledLyricFetchers and LyricFetcherOrder. Disabling ignores
case; configured order matches exact names. Providers first receive stable
intrinsic priorities, with zero as the default, and unranked ties retain that
order. First-success searches run sequentially; all-provider searches run
concurrently and return results in configured provider order.

Manual lyric search deliberately retains the Audio-overload defaults, including
its independent provider selection. Source oracle: Jellyfin `4910aafa1a`,
MediaBrowser.Providers/Lyric/LyricManager.cs constructor and SearchLyricsAsync
overloads, and MediaBrowser.Providers/Lyric/LyricScheduledTask.cs request
construction.

Validation: 28 focused core tests pass, including live nested/adopted ownership,
disabled-name casing, exact order, intrinsic priorities, stable ties and a
barrier that proves concurrent all-provider startup. The task regression checks
that it uses automated search. Real HTTP uses a local LrcLib endpoint: disabled
automatic work makes zero provider requests, manual search still succeeds,
re-enabling downloads actual lyrics, and disabling again stops requests.
The independent metadata HTTP test, complete scan matrix, SQL boundary,
formatting, server build and strict workspace Clippy pass.

Native before/after and Jellyfin 12.1.0 reference checks complete successfully
with three saved disabled-name casings and no lyric files. Before/after task
wall times are 26.93/28.01, 28.72/28.11 and 27.94/28.32 ms. These include
25 ms polling on an unisolated host and make no improvement claim; Docker is
unavailable. External provider access is blocked, so native disabled-task
outcomes alone cannot prove which providers were attempted. The local HTTP
mock supplies that evidence without relying on a public provider. Core coverage
is **94.33%** (104,628/110,918), with all 28 affected tests rerun instrumented
and unchanged coverage seeded from [Live lyric destinations](#live-lyric-destinations). LLVM export emits no warnings.

Evidence: `/tmp/ferrofin-dashboard-l24-{checks,coverage,native}.json`, focused
test logs and native before/after/reference logs. Builds remain serialized with
the 30 GiB target cap and 512 GiB host reserve.

### Configured embedded music tags

Audio probes retain an internal, serde/schema-skipped snapshot of unsplit tags.
The scanner interprets it using the owning library's live options instead of
accepting the normalizer's unconditional splitting. Defaults use standard
ARTIST/ALBUMARTIST fields; PreferNonstandardArtistsTag selects ARTISTS and
ALBUMARTISTS when supplied. Custom separators apply only when enabled, accept
one UTF-16 character each, and retain the configured whitelist spelling before
remaining split names. NUL sanitation, actual U+001F multivalues, per-value
distinct handling and MusicBrainz field/alias priority follow the pinned source.

Existing field locks and metadata merge rules are retained. As in upstream,
changing the option does not rewrite already populated artist metadata merely
on Save; replacement refresh is used to reinterpret those fields. Encoders that
supply already interpreted MediaInfo without raw tags retain their seam contract.

Source oracle: Jellyfin `4910aafa1a`,
MediaBrowser.Providers/MediaInfo/AudioFileProber.cs,
MediaBrowser.Model/Extensions/LibraryOptionsExtension.cs and NameExtensions.cs.

Validation: all **961 model**, **919 mediaencoding**, and **76 core/music**
tests pass. Regressions cover raw-tag wire exclusion, normalization precedence,
preferred/missing/empty/NUL tags, Unicode whitespace/case/diacritics, whitelist
order, multivalues, valid provider IDs and repeated live scanner policies.
Real HTTP performs six saved-option replacement refreshes and verifies actual
item fields. The metadata HTTP regression, full scan matrix, SQL boundary,
formatting, build and strict workspace Clippy pass. The prepared HTTP fixture
was adapted to [Lyric provider selection and order](#lyric-provider-selection-and-order)'s new LrcLib test endpoint; an empty-string test construction
was corrected to meet the strict lint gate.

Native before/after/reference uses real FLAC tags in seven saved configurations.
Before-change matches none; after-change matches all seven native Jellyfin
12.1.0 observations, including provider-ID retention and AlbumArtist. The source
pin and native runtime are distinct. The fixture separately settles the library's
named-artist catalog after track refresh: immediate reference AlbumArtists pairs
can be absent before that pass. Its time is excluded from track-refresh timings;
[Artist DTO links name real artist items](#artist-dto-links-name-real-artist-items) records the separate DTO identity/mapping gap. Public providers are blocked
through the local proxy, including reference by-name provider attempts.

Before/after refresh response ranges are **1.94–3.96/2.05–2.91 ms**; observed
time until Etag changes is **236.59–244.42/229.34–237.47 ms**, including 50 ms
polling. These single unisolated observations make no improvement claim.
Docker is unavailable. Independent model coverage is **90.16%** (8,856/9,822), and mediaencoding
coverage is **90.92%** (17,004/18,702), from their full instrumented suites.
Core coverage is **85.89%** (95,671/111,391), with all 76 affected tests
rerun instrumented and unchanged coverage seeded from [Lyric provider selection and order](#lyric-provider-selection-and-order). Model/core exports
emit no warnings; the fresh mediaencoding export reports nine functions with
mismatched profile data. Each changed crate independently passes 80%.

Audio credit persistence remains [Embedded tags become an audio item's credits](#embedded-tags-become-an-audio-items-credits). The real fixture verifies ordinary FLAC
tag strings; it does not establish ATL-equivalent repeated-container value
boundaries for every format. Actual U+001F values are guarded by resolver tests.

Evidence: `/tmp/ferrofin-dashboard-l25-{checks,coverage,native}.json` and matching
logs/exports. Builds remain serialized under the 30 GiB target cap and 512 GiB
host reserve; current generated artifacts are approximately 12 GiB.

### Audio normalization analysis

The scheduled task already consumed the owning library's saved EnableLUFSScan
flag. Album analysis now preserves either existing LUFS or NormalizationGain,
and both album concat input and individual analysis exclude remote protocols.
An ffmpeg start/run failure produces no measurement and lets later tracks run.
The summary parser requires leading whitespace and keeps the first integrated
summary, matching the pinned source's expression.

Source oracle: Jellyfin `4910aafa1a`,
Emby.Server.Implementations/ScheduledTasks/Tasks/AudioNormalizationTask.cs.

Real SQLite/filesystem regressions save the library flag off/on/off/on without
recreating the task, preserve embedded album and track gains, check actual concat
contents, exclude remote tracks, retain previously measured values and continue
after a failed start. The existing positive parser fixture now uses explicit
concatenation so Rust's line continuation cannot strip ffmpeg's indentation.

The native fixture uses two real FLAC tracks, then adds two more. It checks
disabled, enabled, cached repeat, disabled new tracks and enabled new tracks over
HTTP. Album catalog settlement is a separate pass, excluded from scheduled-task
timings: the reference can return stale AlbumNormalizationGain immediately after
analysis. Source pin and native Jellyfin 12.1.0 runtime are distinct. Using
multiple tracks avoids the source's independent single-pending-item batch-save
quirk. These checks establish analysis behavior and returned gains; they make no
claim about every player's use of normalization.

Validation: all five normal and instrumented normalization/parser tests pass,
along with the real lyrics HTTP regression, SQL boundary, formatting, server
build and strict workspace Clippy. Three new test-only lint violations were
corrected before the final passing run. Core independently reaches **85.73%**
line coverage (95,672/111,602), seeding unchanged coverage from [Configured embedded music tags](#configured-embedded-music-tags) and rerunning
all affected tests. LLVM export reports no warnings.

All five after-change native phases complete and match the reference's track
and album gains within 0.00001 after catalog settlement. Before/after task wall
times are **28.32/27.47 ms** disabled, **108.03/132.94 ms** enabled,
**27.90/27.88 ms** cached repeat, **28.22/26.78 ms** disabled with new tracks,
and **84.43/80.94 ms** enabled with new tracks. These single unisolated
observations include 25 ms polling and make no performance improvement claim.
Docker is unavailable. Native ordinary-file checks complement the injected
start-failure/remote-path regressions; they do not reproduce those failures.

Evidence: `/tmp/ferrofin-dashboard-l26-{checks,coverage}.json`,
`/tmp/ferrofin-dashboard-l26-native-{before,after,reference}.json`, and matching
logs/exports. Generated worktree artifacts are **12.39 GiB**; builds remain
serialized under the 30 GiB target cap and 512 GiB host reserve.

### Registered media-segment providers

Library choices now come from actual registered producers. Saved order uses
exact display names, disabling uses .NET ordinal case-insensitive comparison,
and unranked providers retain stable intrinsic order. New provider namespaces
use the pinned MD5/UTF-16LE name identity. The core MediaSegments endpoint now
applies its default provider filter; Intro Skipper management reads its own
producer data independently, as upstream does.

Intro Skipper detection remains in its season task. The core extraction task
replays complete persistent output without repeating season fingerprinting.
Ordinary core create/delete changes served rows; actual producer editors,
detectors and authenticated WASM writes publish their snapshots before updating
that projection. Overwrite adopts legacy output from every registered producer,
including disabled ones, before clearing served rows. Item erasure invokes
cleanup; producer bulk erasure also handles cache-only items.

One retained adapter serializes snapshot publication. Extraction callbacks run
outside the mutation gate; revisions prevent stale extraction from overwriting
a newer edit or resurrecting bulk-erased output. Snapshot publication failure
preserves prior source and rows, while later projection failure leaves complete
source available for replay.

WASM scan targets describe inputs. Only actual loaded writers with completed
snapshot/identity evidence or their exact legacy namespace become segment
providers. State-only analyzers remain independent. Host-side policy skips stay
pending even if the library is re-enabled before the guest returns, and state
publication preserves concurrent guest keys. No new WIT role or setting is
introduced, and unloaded namespaces never become advertised providers.

Source oracles: Jellyfin `4910aafa1a`, MediaSegmentManager,
MediaSegmentsController and LibraryController; Intro Skipper
`db09359a520dc91cf51336e508634513d1800fa8`, SegmentProvider, Plugin,
SegmentEditorController and MediaSegmentEditorService.

This finding establishes provider selection, ordering and source retention.
Full Intro Skipper plugin/API parity, its independent authorization/query,
exclusion/refiner and analyzed-season behavior remain separate work. The plain
native Jellyfin runtime has no loaded Intro Skipper plugin, so plugin-reference
HTTP parity is unmeasured. The opt-in compiled example WASM guest is also a
separate check; actual inline component guests exercise the changed host paths.

The actual-manager fixtures use canonical stored GUIDs and isolated real media
paths. Canonical lookup exposed the old lowercase-ID fixture, while using `/`
exposed a separate shortcut-reader defect: it strips the root and loses library
Locations before setting consumers run. [Filesystem-root library shortcuts](#filesystem-root-library-shortcuts) records that ingestion/ownership
gap; this finding does not close it by implication.

Normal validation passes: 15 focused core tests, 33 API integration tests,
44 extension tests, 44 WASM tests, two traits media-segment regressions, the
SQL-boundary ratchet, real HTTP dashboard persistence, formatting, a fresh
server build and strict workspace/all-target/all-feature Clippy. The WASM
suite includes actual component instantiation and a guest write interrupted by
an off/on library policy change. Separate crate coverage is recorded below.

Native before/after uses isolated media, the loaded built-in Intro Skipper,
its actual advertised provider name in each build, 10 GET samples per saved
flag state, and the scheduled-task endpoint. Across off/on/off/on/off states,
the baseline serves both Intro and an unloaded WASM Outro regardless of the
flag. The changed build serves only registered Intro while enabled, zero while
disabled, and retains the management Intro throughout. Extraction completes
and retains only the registered producer's served output.

GET median milliseconds before: 3.363, 1.534, 1.560, 3.131, 1.583;
after: 2.270, 2.579, 2.223, 2.175, 2.129. Task wall time before/after:
26.775/27.269 ms, including a 25 ms polling interval. This is one unisolated
host observation; it establishes no general latency improvement. The new
GET path necessarily resolves owning-library/provider policy.

Evidence: `/tmp/ferrofin-dashboard-l27-checks.json`,
`/tmp/ferrofin-dashboard-l27-native-before.json` and
`/tmp/ferrofin-dashboard-l27-native-after.json`. Native script:
`/tmp/ferrofin-dashboard-l27-live.py`. The baseline's earlier fixture attempt
rejected its old `IntroSkipper` spelling before any policy observation; the
successful baseline uses that actual advertised name, while the changed build
requires `Intro Skipper`.

Fresh full core coverage passes: **95.23%**, 107,525 / 112,906 lines, all
2,284 tests passing across 22 instrumented binaries, no LLVM diagnostics.
The attempted [Audio normalization analysis](#audio-normalization-analysis) profile reuse measured only 2.35% against this build and
was rejected; its record remains at
`/tmp/ferrofin-dashboard-l27-incompatible-seed-coverage.json`. The passing
run uses no prior profile. 

Fresh full API coverage passes its numeric threshold: **86.08%**, 40,153 /
46,648 lines, all 893 tests passing across 54 instrumented binaries. This
seed-free export reports **53 functions with mismatched data**; the diagnostic
is retained in the coverage record and qualifies the measurement. Normal
integration and actual HTTP/native proof independently pass; this percentage
alone does not establish behavioral correctness.

Fresh full extension coverage passes: **94.22%**, 2,854 / 3,029 lines,
all 96 tests passing, no LLVM diagnostics. Fresh WASM coverage passes:
**85.20%**, 4,811 / 5,647 lines, all 61 selected host/runtime tests passing,
no LLVM diagnostics. The compiled example guest binary is built/listed but
its opt-in test is excluded; actual inline WAT guests run. Traits and server
are the documented coverage exemptions.

All four gates use fresh full suites with no prior profiles. Records and
merged profiles: `/tmp/ferrofin-dashboard-l27-coverage.json` and
`/tmp/ferrofin-dashboard-l27-ferrofin-<crate>.profdata`. Raw profiles were
removed after successful exports. Stale generated target cleanup removed
7.39 GiB of artifacts; after coverage, target is about 25.6 GiB and host
available space about 682 GiB. Source/worktree/commits and the baseline binary
remain intact. Full workspace tests/doctests remain the batch completion gate.

## Libraries: chapters and trickplay

### Enable chapter images

The scheduled chapter task now reconciles disabled and previously failed
videos instead of leaving stale image references and files behind. It shares
one extractor with the upcoming scan-time consumer. Extraction uses the live
owning library, physical/complete-video eligibility, the persisted default
video-stream index, source chapter-density rules, and DateModified-based cache
filenames. Fresh video scans now persist that required default index; this
prerequisite belongs in this finding so the task works before [Chapter images during library scans](#chapter-images-during-library-scans).

The first frame uses min(15 seconds, runtime), and chapters below runtime can
extract near EOF. Existing images can be adopted without new extraction;
references and unused supported image files reconcile. Repository/service
errors propagate without poisoning extraction-failure history. Genuine earlier
failures still flush on a later error. Existing systemic write-permission
preflight remains, while disabled-only cleanup does not require encoder output.

Pinned source: Jellyfin `4910aafa1a`, ChapterManager, ChapterImagesTask,
PathManager and FFProbeVideoInfo. The shared helper has no strong library or
chapter-manager ownership and only weak DVR ownership. The task includes owned
videos. No production SQL query node is added; the scanner ratchet stays 24.

Native baseline: a fresh 36-second video with chapters at 0/26/35/runtime,
scanned with both flags off. Scheduled enable produced images at 0/26, skipped
35 and retained all four chapter rows. Both images were already blue, so this
finding does not claim a baseline first-frame color defect. Disabling retained
the two images and their references; re-enabling reused them. Timings:
initial scan 426.06 ms, enabled 107.73 ms, disabled cleanup 26.91 ms,
re-enabled 27.17 ms.

The separate during-scan flag and event-driven scheduling remain [Chapter images during library scans](#chapter-images-during-library-scans). This finding
native proof invokes only the scheduled task; it does not require [Chapter images during library scans](#chapter-images-during-library-scans)'s removal
of queued whole-library extraction or the old scanner's at-runtime chapter row.
Dummy marker generation and configured image dimensions remain [Dummy chapter duration (`DummyChapterDuration`)](#dummy-chapter-duration-dummychapterduration)/M04.

Evidence: `/tmp/ferrofin-dashboard-l28-native-before.json`; runners
`/tmp/ferrofin-dashboard-l28-checks.py` and
`/tmp/ferrofin-dashboard-l28-coverage.py`; native fixture
`/tmp/ferrofin-dashboard-l28-l29-live.py --finding l28`.

Final changed-binary HTTP proof produces all three below-runtime images, including
35 seconds, removes the runtime marker when saving, clears references and files
when disabled, and regenerates them when re-enabled. Cache paths include the
file's DateModified ticks. The independent Jellyfin 12.1.0 run agrees on these
scheduled-task outcomes; the behavioral source oracle remains the pinned
12.0-rc7 commit. Initial disabled-scan marker removal belongs to [Chapter images during library scans](#chapter-images-during-library-scans).

| Native phase | Before (ms) | After (ms) | Jellyfin reference (ms) |
| --- | ---: | ---: | ---: |
| Initial disabled scan | 426.06 | 476.56 | 455.17 |
| Scheduled enable | 107.73 | 129.59 | 178.02 |
| Scheduled disabled cleanup | 26.91 | 29.34 | 22.99 |
| Scheduled re-enable | 27.17 | 133.16 | 147.53 |

Each phase is one local observation on the shared host, including task polling.
The enabled and re-enabled tasks now perform the required extra encoding work;
these measurements are not a general performance improvement claim.

Validation: 45 focused core tests, 43 image API integration tests, the SQL
boundary ratchet, real-server metadata-locale HTTP, trait compilation, normal
server build, formatting and strict workspace/all-target/all-feature Clippy
pass. Fresh affected core execution merged with the verified [Registered media-segment providers](#registered-media-segment-providers) full-core
profile reports **83.09%** line coverage (94,639 / 113,903), above the separate
80% gate, with no LLVM diagnostics. Traits/server are coverage-exempt. The
final whole-workspace test and doctest gate remains required for this batch.

Final evidence: `/tmp/ferrofin-dashboard-l28-checks.json`,
`/tmp/ferrofin-dashboard-l28-coverage.json`,
`/tmp/ferrofin-dashboard-l28-native-after.json`,
`/tmp/ferrofin-dashboard-l28-native-reference.json`. The target directory was
23.29 GiB after validation, with about 682 GiB host space free.

### Chapter images during library scans

Successful video Default/FullRefresh now reconciles chapters during the video's
own refresh. The live library's ExtractChapterImagesDuringLibraryScan controls
new frames independently of EnableChapterImageExtraction. Existing valid images
can be reused with during-scan extraction off; disabling the owning library
clears stale references and unused supported files. Refresh replaces authoritative
chapter rows, including empty sets, and omits markers at or beyond runtime.
Failed probes, audio, None and ValidationOnly preserve the prior chapter set.

The shared extractor owns no library/chapter manager. The composition root
attaches it to the scanner and removes the LibraryChanged subscriber that
queued whole-library extraction irrespective of the during-scan flag. The
scheduled [Enable chapter images](#enable-chapter-images) task remains independently available. No new SQL query node or
ratchet ceiling is introduced.

Cancellation follows the pinned call placement: before a new extraction it
preserves stored rows and prior files, leaving any completed new frame on disk;
an interruption caught inside the encoder follows the ordinary partial-failure
save/prune path. Cached rows can reconcile without new encoding. Real SQLite
regressions check each case and the live off/on/disabled transitions.

Source oracle: Jellyfin 4910aafa1a (12.0-rc7),
MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs Fetch and
Emby.Server.Implementations/Chapters/ChapterManager.cs RefreshChapterImages and
SaveChapters. DummyChapterDuration and configured image resolution remain
independent [Dummy chapter duration (`DummyChapterDuration`)](#dummy-chapter-duration-dummychapterduration)/M04 findings.

The preserved [Enable chapter images](#enable-chapter-images) binary's real HTTP scan baseline produces no images in all
three flag combinations (enabled/off, enabled/on, disabled/on) and retains an
at-runtime marker. Initial scan: 430.50 ms; changed-file phases:
370.68 / 371.00 / 371.67 ms. This fixture did not observe the old queued task
executing; the event-handler removal is also established by source review.

The changed binary produces three images only for enabled/on, adopts no new
frames for enabled/off, clears references/files for disabled/on, and never queues
the separate chapter task. All phases now contain three below-runtime rows.
Changed-file phase times: 371.86 / 479.65 / 369.26 ms; initial scan 436.48 ms.
The enabled/on increase includes three required frame encodes. These are single
local observations on a shared host with polling, not a general latency claim.

52 focused core tests, 43 image API integration tests, SQL boundary, traits,
real-server metadata-locale HTTP, normal build, formatting and strict
workspace/all-target/all-feature Clippy pass. Composition-root attachment uses
the retained scanner Arc after construction; the stale local binding was caught
and corrected by the actual server build.

The independent Jellyfin 12.1.0 reference produces the same below-runtime
chapter sets and flag-controlled image outcomes. Initial scan 481.05 ms;
changed-file phases 286.60 / 425.10 / 283.55 ms. Source and runtime pins remain
separate.

Fresh affected core tests merged with the verified [Enable chapter images](#enable-chapter-images) core profile report
**81.99%** line coverage (93,780 / 114,377), above the separate 80% gate,
with 0 LLVM diagnostics. Traits/server are coverage-exempt.
Final whole-workspace tests/doctests remain required for the batch. Baseline evidence: /tmp/ferrofin-dashboard-l29-native-before.json.

Final evidence: `/tmp/ferrofin-dashboard-l29-checks.json`,
`/tmp/ferrofin-dashboard-l29-coverage.json`,
`/tmp/ferrofin-dashboard-l29-native-{before,after,reference}.json`.
Target: 23.29 GiB; host free space: about 682 GiB.

### Trickplay extraction enablement

The scheduled task now reads the live physical owning library for each candidate
video, includes owned extras and excludes nonvideo/virtual/folder rows.
The declared SourceTypes query field is inert in the pin and is not reinterpreted
as a channel exclusion: complete channel VOD remains a candidate. Shared eligibility uses the source's actual disc/placeholder/shortcut,
active recording/channel livestream, runtime and any-stream rules before any
discovery or pruning. Audio-only candidates can undergo outer cleanup but
cannot encode a video frame; no chapter-default-index requirement is invented.

Existing user-managed grids are discovered before the extraction flag. Valid
arbitrary width/grid dimensions, actual image size, inferred interval, padded
capacity and bandwidth are persisted. Raw configured interval is used during
discovery; generation's minimum interval is applied later. Existing rows with
no directory at either location are pruned. Disabled internal images are
removed, while disabled media-side user tiles are catalogued and preserved.
Missing media files and backdrops are checked per width after outer discovery
and disabled cleanup, matching their source call placement.

Pinned source oracle: Jellyfin 4910aafa1a,
Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs,
MediaBrowser.Providers/Trickplay/TrickplayImagesTask.cs and
MediaBrowser.Model/Dto/MediaSourceInfo.cs. Native Jellyfin 12.1.0 is separate
runtime evidence. During-scan and destination consumers remain [Trickplay extraction during scans](#trickplay-extraction-during-scans)/L32.

Validation: 27 focused core tests, SQL boundary, trait compilation, real-server
HTTP trickplay integration, normal build, formatting and strict workspace/
all-target/all-feature Clippy pass. Independent native before/after runs use the
same isolated four-second video and 160px 2x2 grid. The before binary cannot
serve the disabled user grid; the after binary catalogs and serves it without
modifying tile bytes or mtime. Generation, reuse and disabled internal cleanup
remain correct. All six after/reference observation gates pass. The independent
Jellyfin runtime is 12.1.0; the source oracle remains 12.0-rc7.

Source recheck preserves two non-obvious outcomes: SkiaEncoder returns zero
dimensions for an unreadable JPEG, and discovery catalogs it with minimum
height one; a real video-stream width zero skips generation, while a missing
Width does not cap it. Regression assertions were corrected to those source
outcomes. The obsolete first-location helper is removed. Existing persistence
helpers seed the new Data cases, retaining the trickplay SQL ceiling of seven.

| Native phase | Before (ms) | After (ms) | Jellyfin reference (ms) |
| --- | ---: | ---: | ---: |
| Disabled initial | 52.36 | 53.38 | 118.37 |
| Enabled generation | 107.89 | 113.62 | 130.61 |
| Enabled reuse | 52.13 | 52.67 | 62.07 |
| Disabled internal pruning | 52.05 | 52.46 | 27.69 |
| Disabled user-grid discovery | 52.07 | 54.52 | 64.59 |
| Disabled user-catalog reuse | 54.11 | 53.88 | 26.69 |

These are one observation per phase on a shared host, including 50ms task
polling; they are not a general performance improvement claim.

Separate core line coverage: **80.8%** (93,083 / 115,208), with 0 LLVM diagnostics. Fresh affected tests are mandatory; a verified earlier profile contributes compatible unchanged execution. The full batch workspace/doctest gate remains required. Traits/server are coverage-exempt.

Evidence: `/tmp/ferrofin-dashboard-l30-checks.json`,
`/tmp/ferrofin-dashboard-l30-coverage.json`,
`/tmp/ferrofin-dashboard-l30-native-{before,after,reference}.json`.
Target after validation: 23.59 GiB; host free space remains above 674 GiB.

### Trickplay extraction during scans

The scanner now invokes the shared trickplay manager after a selected video
refresh, using the live owning library's
`ExtractTrickplayImagesDuringLibraryScan`. This is independent of extraction
enablement: a selected refresh still discovers user grids or removes disabled
internal data. The manager shares eligibility, encoding limits and storage with
the scheduled task.

Live `TrickplayOptions.ScanBehavior` controls whether metadata completion waits
for extraction. Blocking waits; other enum values start a background operation.
`RegenerateTrickplay` forces replacement only in FullRefresh. A successful new
probe supplies its actual streams, including an empty result; a refresh without
a new probe reads stored streams. New videos are saved before the trickplay
foreign key is used. Existing blocking refreshes retain their metadata Etag
until extraction finishes.

Cancellation stops completion and drops extraction work. Temporary frame and
tile directories use drop guards, including cancellation while encoding.
Ordinary custom-provider errors are logged but do not prevent
`DateLastRefreshed`, matching `MetadataService.RunCustomProvider`. The HTTP
error fixture uses a real SQLite discovery failure; an unreadable JPEG is not
an error in the pinned image decoder.

Selection follows run-all and the actual media-file change monitor. Subtitle
changes and the separate metadata-backfill heuristic are not treated as that
monitor. Generic provider change-monitor dispatch remains open work: extend the
provider traits and refresh planner with actual per-provider `HasChanged`
results and feed custom-provider selection without reinterpreting backfill.
The pinned built-in remote providers do not implement that interface. This
setting does not establish a generic provider-monitor implementation (“General provider change-monitor dispatch” (open)).

Pinned source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Trickplay/TrickplayProvider.cs`,
`MediaBrowser.Providers/Manager/MetadataService.cs`, and
`Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs`.
Native Jellyfin 12.1.0 is separate runtime evidence.

Validation passes: 35 focused core tests and the fresh full 2,318-test core
suite, three real-server HTTP trickplay tests, SQL boundary checks, trait
compilation, normal server build, formatting and strict workspace/
all-target/all-feature Clippy.
The before binary produces no scan-time tiles when the setting is enabled.
All seven after/reference catalog and destination gates pass, including live
on/off changes, reuse, replacement and disabled cleanup. The HTTP held-encoder
fixture proves the different Blocking/NonBlocking save boundaries. Fresh full core line coverage is **95.23%** (110,332 / 115,862), with zero
LLVM diagnostics. The initial combined profile reached 78.32% and was
rejected; the passing run uses no earlier seed. Traits/server are coverage
exempt. The complete batch workspace/doctest gate remains required.

| Native phase | Before (ms) | After (ms) | Jellyfin reference (ms) |
| --- | ---: | ---: | ---: |

| scan flag disabled | 246.11 | 242.56 | 94.45 |
| blocking scan generates | 232.30 | 229.07 | 167.46 |
| blocking scan reuses | 229.84 | 233.68 | 99.94 |
| nonblocking scan replaces | 229.81 | 246.50 | 86.26 |
| scan flag disabled again | 222.98 | 235.55 | 84.36 |
| scan disabled extraction prunes | 230.40 | 235.30 | 82.71 |
| full refresh forces when disabled | 225.26 | 291.73 | 87.37 |

These are one observation per phase on a shared host, including 50ms
completion polling. They measure metadata completion and do not claim a
general performance improvement or a NonBlocking extraction-speed comparison.

Evidence: `/tmp/ferrofin-dashboard-l31-checks.json`,
`/tmp/ferrofin-dashboard-l31-coverage.json`, and
`/tmp/ferrofin-dashboard-l31-native-{before,after,reference}.json`.

Target after validation: 23.86 GiB; host free space: 671.44 GiB.

[Trickplay storage beside media](#trickplay-storage-beside-media) revalidation repaired the shared HTTP fixture’s internal data root and added
a successful cleanup control after removal of the injected database error.
All four trickplay HTTP tests now pass with discovery demonstrably reached;
see `#trickplay-storage-beside-media`. The earlier held-manager unit evidence remains
valid, while this replaces the earlier error-injection fixture evidence.

### Trickplay storage beside media

Generation, pruning and tile lookup now use the owning library's live
`SaveTrickplayWithMedia`. A failed write at the selected location does not
fall back to internal storage. Lookup switches immediately after a setting
change; existing files remain at their old location until migration runs.

The migration task reads live ownership, extraction enablement and destination.
It moves existing tiles in either direction without encoding or changing their
bytes. It includes owned physical videos and complete channel VOD, since the
declared SourceTypes field is inert in the pinned query. Nonvideo, virtual and
folder rows are excluded. Extraction disabled means no migration.

Source migration quirks are preserved: an existing empty destination prevents
movement, matching files in a populated destination are overwritten, unrelated
destination files remain, and directory enumeration/cleanup errors are reported.
Source-container removal is nonrecursive; an ordinary remaining file can cause
an error after tiles have moved. No unrelated file is deleted to hide that error.

Sidecar paths retain the full dotted basename. Leading-dot filenames also
match `Path.ChangeExtension`, including `.mkv` becoming `.trickplay`; the
internal GUID layout is unchanged.

Pinned source oracle: Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/Library/PathManager.cs`,
`Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs`, and the
migration scheduled task. Native Jellyfin 12.1.0 is separate runtime evidence.

The shared HTTP fixture now uses the actual internal data directory. Its
ordinary-error test also removes the injected database error and repeats the
same refresh, requiring successful disabled cleanup as a positive control.
This revalidates [Trickplay extraction during scans](#trickplay-extraction-during-scans)'s error behavior with a fixture that actually reaches
discovery. Migration's seeded video rows now include the normal MediaType.

Validation passed: 47 focused core tests, four real-server HTTP tests, the
trait build and SQL boundary, formatting and strict workspace Clippy across
all targets/features. Fresh this finding execution merged with the verified [Trickplay extraction during scans](#trickplay-extraction-during-scans) full
profile covers 110,397/116,301 core lines (94.92%); 22 current binaries and
98 fresh profiles were exported, with no LLVM diagnostics.

Native before/after used the same fixture and settings sequence. Jellyfin
12.1.0 independently passed the six destination/catalog checks. All migration
phases retain the original tile hash. Measured completion times in milliseconds:

| Phase | Before | After | Jellyfin 12.1 |
|---|---:|---:|---:|
| generate internal dotted name | 107.47 | 108.37 | 167.56 |
| migrate to sidecar | 53.11 | 53.18 | 58.4 |
| migrate back internal | 53.92 | 53.4 | 25.13 |
| disabled migration retains internal | 52.5 | 55.79 | 18.31 |
| reenabled migrates sidecar | 52.21 | 53.37 | 18.38 |
| blocked sidecar has no internal fallback | 53.03 | 69.86 | 77.77 |

These are shared-host observations with polling overhead, not a general
performance claim. Before migration chose the wrong destination and the blocked
sidecar operation incorrectly reused internal storage; after/reference match
the expected files and responses.

Evidence: `/tmp/ferrofin-dashboard-l32-checks.json`,
`/tmp/ferrofin-dashboard-l32-coverage.json`, and the three
`/tmp/ferrofin-dashboard-l32-native-*.json` records. The instrumented target and
normal target together use 23.79 GiB, with 671.44 GiB free. All owned native
children exited. Source, worktree and commits remain preserved.

## Libraries: display

### Display folder view

The live EnableFolderView switch now adds/removes the localized Folders home
view. Its identity uses the named metadata directory, surviving language changes;
adopted numeric ViewType and tokenized paths remain usable without warm writes.
Existing hidden-view preferences and physical library visibility apply.

Browsing that view selects visible immediate root children. The shared typed
filter implements the pinned GetResult whitelist, including own user-data key
precedence, nullable Likes/Rating and leaf/folder played semantics, literal name
prefix/ranges, image index zero and nonempty provider values. It preserves input
order without explicit sorting; explicit Name/SortName sorting is stable and
ordinal case-insensitive, including forced-name precedence. Filtering and
adjacency precede total-count calculation and paging. Direct IDs bypass the
view builder and retain the ordinary parent scope; nil adjacency is absent.

Pinned source is Jellyfin 4910aafa1a: UserViewManager, UserViewBuilder.GetResult,
Folder.GetChildren, BaseItem keys and registered Name/SortName comparers.
API consumers use traits; core implements the batched persistence algorithms.

The independent additional work remains concrete in the living document:
[The user's saved view order (`OrderedViews`)](#the-users-saved-view-order-orderedviews) general ordering and generated keys, [A video being recorded is keyed under Live TV](#a-video-being-recorded-is-keyed-under-live-tv) active-DVR key override, “Named UserView refresh and existing artwork lifecycle” (open) named
view artwork/refresh lifecycle, and “Grouped/preset and special UserView production/selection” (open) grouped/preset producers. This finding
verifies the display switch and browsing; it does not close those consumers.

The cold real-server fixture exposed a missing plugin-root provision: Folders
must include Playlists before any MediaFolders request or scan. The view now
settles the existing root/plugin stores on first use; warm reads remain
idempotent. The native fixture independently asserts this cold membership.

HTTP name boundaries reproduce ASP.NET10 blank-to-null string binding without
trimming nonblank input. Internal typed filters still treat whitespace
literally. Both /Items and user-scoped Items routes cover all three boundaries.

The executable .NET10 name oracle also rejected the initial supplementary
character ordering assumption. The corrected OrdinalIgnoreCase consumer uses
simple invariant casing and scalar order for valid Rust strings, retaining
null ordering and stable ties. BMP versus supplementary characters, Deseret
case equivalence, common prefixes and sharp-S are now explicit regressions.
The primary runtime implementation is OrdinalCasing.Icu.cs173–265 in dotnet
v10.0.0; this is distinct from ordinary Ordinal UTF-16 comparison.

Repository-only test helpers retain the adopted historical save date and stored
sort-key fixtures; the SQL boundary was not relaxed. Seeded CollectionFolder
fixtures now carry the real IsFolder fact.

The native rated-filter case exposed eight missing HTTP bindings. Both Items
routes now forward AdjacentTo, HasOverview, the three provider-presence flags,
HasParentalRating, and both premiere-date bounds. Nullable blank inputs, UTC
offsets, malformed inputs, and filtering before count/adjacency/paging have
real API and Folders regressions. The source-only internal inputs remain
unexposed.

Items premiere-date bounds now use their own binder. Offsetless dates are
interpreted in the server's local timezone, matching ItemsController's
ToUniversalTime conversion; explicit offsets are converted during binding.
DST gaps/folds select standard time, valid local conversions clamp at the
DateTime limits, and fractional seconds reproduce the source's floating-point
accumulation and rounding to 100ns. Invalid offsets, leap seconds and binding
range overflow are rejected. LiveTV retains its separate binder.

The .NET 10.0.12 reference matrix records 320 cases across eight zones with
host tzdata 2026c, including historical offsets, negative/half-hour DST,
precision, range and rejection cases. A real TCP test exercises both Items
routes in owned child processes, changing only each child's TZ. The production
fixture independently sets a collection's premiere date through HTTP and checks
five Folders date/count boundaries in America/New_York, including a single tick.
The measured platform is Linux x86_64; this establishes the tested ISO input
forms and does not establish all DateTime.Parse formats or historical timezone
parity on experimental Windows/macOS targets.

Reference implementation:
[ASP.NET DateTime binder](https://github.com/dotnet/aspnetcore/blob/v10.0.0/src/Mvc/Mvc.Core/src/ModelBinding/Binders/DateTimeModelBinder.cs),
[DateTimeParse](https://github.com/dotnet/runtime/blob/v10.0.0/src/libraries/System.Private.CoreLib/src/System/Globalization/DateTimeParse.cs),
and [TimeZoneInfo Unix conversion](https://github.com/dotnet/runtime/blob/v10.0.0/src/libraries/System.Private.CoreLib/src/System/TimeZoneInfo.Unix.cs).
The existing workspace libc 0.2.189 edge is reused. Latest upstream
[libc documentation](https://docs.rs/crate/libc/latest) lists 0.2.190; no unrelated
package upgrade was introduced.

The new direct dependency edges reuse locked versions. Maintainer-published
latest release documentation confirms [icu_collator 2.3.1](https://docs.rs/crate/icu_collator/latest)
and [unicode-normalization 0.1.25](https://docs.rs/crate/unicode-normalization/latest);
no package upgrade or new version resolution was needed.

Validation passed:

- fmt: passed.
- core: 346 tests run: 346 passed, 1999 skipped.
- api: 102 tests run: 102 passed, 797 skipped.
- traits: passed.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- oracle: passed.
- clippy: passed.
- build: passed.
- date-oracle-http: 1 test run: 1 passed, 37 skipped.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 113,203/118,796 lines (95.29%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-api: 40,691/47,265 lines (86.09%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
  Earlier attempt/seed diagnostics retained: warning: 53 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-d01-checks.json`,
`/tmp/ferrofin-dashboard-d01-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 19.02 GiB generated target, 672.55 GiB free on the host.
After final API coverage, generated target used 24.30 GiB with 671.23 GiB free.
The pre-coverage cleanup removed 8.63 GiB of stale generated artifacts.
Fresh full coverage suites passed all 2,345 core and 899 API tests.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| disabled / ordinary_parent_browse | 4.003 / 6.364 | 3.059 / 4.147 | 9.907 / 14.804 |
| enabled / ordinary_parent_browse_timing | 4.525 / 7.876 | 3.202 / 4.179 | 8.668 / 11.610 |
| localized / ordinary_parent_browse_timing | 4.709 / 6.899 | 3.101 / 3.917 | 7.784 / 11.277 |
| disabled_again / ordinary_parent_browse_timing | 3.438 / 5.028 | 3.020 / 3.415 | 8.468 / 12.268 |
| reenabled / ordinary_parent_browse_timing | 3.409 / 4.346 | 3.049 / 4.449 | 7.987 / 11.338 |
| enabled / folders_browse_timing | unavailable | 3.071 / 3.745 | 8.047 / 11.622 |
| localized / folders_browse_timing | unavailable | 2.834 / 4.209 | 6.196 / 10.114 |
| reenabled / folders_browse_timing | unavailable | 2.778 / 4.210 | 5.898 / 9.318 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the repaired server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

### Group movies and shows into collections

The two live global grouping switches now control the Folder consumers that
invoke automatic collection substitution. Movies and series can be enabled
independently; request overrides retain the pinned allow-list and parent-kind
exceptions. Disabled plain Folder browsing still applies the source filtering
and paging path. Series, Season and Channel overrides retain their own dispatch.

Recursive repository selection substitutes collections before sorting, adjacency,
counts and paging. Plain browsing loads physical children in stored order followed
by all linked child kinds, applies visibility/ownership and filters, then replaces
eligible Movie/Series members with visible collections. Local alternate-version
suppression and the three name-range rechecks match the selected source consumer.
The implementation batches membership reads and uses the existing trait seams.

Existing/adopted movie and TV UserViews choose their source default kinds and
recursive/direct dispatch. Explicit item IDs first validate the actual parent,
retain any collection/playlist ancestor redirect and user restrictions, then clear
parent selection and bypass view defaults. Requested IDs always suppress collapse.
The real HTTP fixture exercises four view variants, both settings and recursion
choices, blocked parents/items, requested kinds and ordered pages/counts.

The source oracle is Jellyfin 4910aafa1a: Folder.GetItemsInternal and
CollapseBoxSetItems, UserViewBuilder.GetUserItems, CollectionManager, ItemsController
and BaseItemRepository query translation. Native Jellyfin 12.1.0 is separate runtime
evidence. [The user's saved view order (`OrderedViews`)](#the-users-saved-view-order-orderedviews) general sorting/request-ID order, [A video being recorded is keyed under Live TV](#a-video-being-recorded-is-keyed-under-live-tv) active-DVR keys, “Grouped/preset and special UserView production/selection” (open) missing
view producers and [A box set's `DisplayOrder`](#a-box-sets-displayorder) BoxSet/music child overrides remain explicit work items.
[Home-video libraries resolve plain videos](#home-video-libraries-resolve-plain-videos) records the independently confirmed home-video resolver gap in the living audit;
the original failed native setup is retained as evidence. The reviewed native fixture
uses actual scanned MusicVideo items to distinguish recursive all-kind substitution
from the plain CollectionManager Movie/Series-only consumer.
This finding verifies the grouping switches on their actual consumers.
Recursive BoxSet/Playlist membership now follows the pinned linked-descendant
branch. Batched folder-root discovery preserves first-visit ancestry/link roles,
all direct link kinds, nested folders, cycles and root exclusion. It binds root
sets rather than materializing every leaf. Rows and counts reuse the resolved
scope; internal count queries use the same resolver. Physical ownership/deletion
keeps its separate scope. Real repository and HTTP cases cover direct/recursive
membership, Series/Episode descendants, user restrictions and pages/counts.
[A box set's `DisplayOrder`](#a-box-sets-displayorder) retains collection DisplayOrder, modern physical-child and music overrides.

Production helpers separate option construction, parent dispatch, child loading,
substitution and post-filter sorting/paging. Strict lint checks remain enabled;
only coherent, large fixture matrices have individual length allowances.
The unplayed-series fixture persists its Episode ancestry closure, as a real scan
does; scalar OrdinalIgnoreCase ordering uses the measured Linux .NET oracle.
An offline oracle executes the exact pinned LINQ with EF SQLite 10.0.11 on
.NET 10.0.12/SQLite 3.53.3. All 166 ordinary/collapsed observations agree with
the implementation. SQLite lowercases ASCII on the row; the .NET bound uses
ToLowerInvariant. Three older uppercase Unicode prefix expectations are corrected
from that measurement, with lowercase and literal punctuation controls. The committed
83-case golden matrix verifies the real repository results, identities and counts
in both shapes. This SQL operation remains distinct from in-memory Folder matching.
Full reference SHA256: `05e938b771303330f35d96d7241c4b9d387c535efc8665581f13062a27bc68d7`.

Validation passed:

- fmt: passed.
- clippy: passed.
- core: 415 tests run: 415 passed, 1953 skipped.
- api: 102 tests run: 102 passed, 797 skipped.
- traits: passed.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- http-folder-view: 1 test run: 1 passed, 0 skipped.
- date-oracle-http: 1 test run: 1 passed, 37 skipped.
- oracle: passed.
- build: passed.
- traits-descendant: 1 test run: 1 passed, 75 skipped.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 114,997/120,722 lines (95.26%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-api: 40,734/47,397 lines (85.94%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
  Earlier attempt/seed diagnostics retained: warning: 53 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-d02-checks.json`,
`/tmp/ferrofin-dashboard-d02-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 25.21 GiB generated target, 627.65 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| disabled / ordinary_parent_timing | 2.848 / 3.405 | 3.187 / 5.040 | 5.770 / 8.750 |
| disabled / plain_parent_timing | 3.199 / 5.969 | 4.806 / 10.193 | 5.330 / 7.560 |
| disabled / combined_timing | 3.302 / 5.920 | 4.028 / 8.101 | 8.757 / 11.523 |
| movies_only / ordinary_parent_timing | 3.529 / 5.419 | 3.754 / 4.704 | 7.642 / 10.867 |
| movies_only / plain_parent_timing | 3.943 / 4.387 | 7.200 / 9.098 | 5.980 / 8.200 |
| movies_only / combined_timing | 4.173 / 5.030 | 4.195 / 7.401 | 9.741 / 12.965 |
| shows_only / ordinary_parent_timing | 3.156 / 6.386 | 2.984 / 4.787 | 5.703 / 7.684 |
| shows_only / plain_parent_timing | 3.417 / 5.427 | 5.273 / 6.387 | 5.335 / 7.907 |
| shows_only / combined_timing | 3.833 / 5.010 | 3.693 / 4.226 | 9.834 / 13.580 |
| both / ordinary_parent_timing | 3.096 / 6.005 | 2.657 / 5.576 | 6.537 / 7.938 |
| both / plain_parent_timing | 3.370 / 6.113 | 6.815 / 12.079 | 5.933 / 7.739 |
| both / combined_timing | 2.969 / 3.485 | 3.410 / 4.868 | 8.946 / 15.640 |
| disabled_again / ordinary_parent_timing | 2.768 / 3.127 | 3.030 / 5.754 | 5.733 / 10.609 |
| disabled_again / plain_parent_timing | 2.906 / 3.500 | 4.617 / 7.971 | 5.234 / 8.223 |
| disabled_again / combined_timing | 2.884 / 4.402 | 3.168 / 6.337 | 8.472 / 14.904 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

Final source provenance: `/tmp/ferrofin-d02-source22-freeze-v2/freeze.json`
records the exact applied source and closed passing normal/native runs.
The 47-piece continuation queue retains earlier source and evidence snapshots;
all 25 successor patches apply strictly over this verified change.

Target-only cleanup reclaimed 7.11 GiB before coverage, leaving 18.18 GiB
at that point. The preserved parent binary, source worktree and commits remained
intact. Evidence: `/tmp/ferrofin-dashboard-d02-target-cleanup.json`.

### Display specials within seasons

The original support classification covered NextUp but missed episode lists.
The reviewed implementation consumes the live saved
`DisplaySpecialsWithinSeasons` value in `/Shows/{series}/Episodes` and ordinary
Season `/Items` browsing, including default UserViews that delegate to a Season.
Numbered requests resolve an actual visible Season; unknown numbers return an
empty list, and a supplied Season ID uses that Season's actual Series.

Positioned specials follow the shared aired-order comparer. Season zero keeps
physical membership and SortName ordering. Full-series selection preserves
source season order and removes repeated IDs at their last season occurrence,
so the same special is counted once. Filtering and adjacency precede count and
paging. Nil relation/adjacency IDs use the source fallback or absent-input
behavior. Domain LocationType follows physical path/source facts rather than
substituting the stored IsVirtualItem bit.

Season `/Items` uses the pinned missing-episode inclusion before the shared
Folder filter/sort tail; `/Shows` retains its different user missing preference.
Both recursion modes, the selected user's parent visibility, explicit-ID
bypass, unplayed/name filters, descending aired order and pages have
regressions. `/Items` uses the Season's own `IsVisible` contract: library grants
alone do not deny a Season browse, while an own-parent tag block returns 401.
`/Shows` resolves the Season through standalone visibility and returns 404 in
both denied cases. The real HTTP fixture authenticates with an admin user
session and selects the restricted user; clearing the selected user's tag block
restores both routes without restarting. API consumers use traits and the pure model comparer; API does not
depend on core. Existing NextUp behavior remains covered.

The real scan exposed a separate missing input: the NFO reader parsed the
three nullable `AirsBeforeSeasonNumber`, `AirsAfterSeasonNumber` and
`AirsBeforeEpisodeNumber` fields but never copied them into provider metadata.
They now reach the existing merge policy. Real scan/refresh regressions cover
zero and negative values, retained stored values when positions are omitted,
clearing with replacement plus removal, and local NFO precedence over remote
TVDB positions. Resolver facts, title and physical Season zero survive.

Source contract: Jellyfin `4910aafa1a`, `Series.GetSeasonEpisodes`,
`Series.GetEpisodes`, `Season.GetItemsInternal`, `TvShowsController.GetEpisodes`,
`AiredEpisodeOrderComparer`, and `TVSeriesManager`. Native Jellyfin 12.1.0 is
separate runtime evidence. The production fixture scans real MKVs/NFO,
requires a known task's new nonempty EndTimeUtc plus Completed status and all
four expected titles, then checks false → true → false membership/order/count.
It includes blocked-tag and selected-user parent controls; raw before results
are observations, while after/reference phases assert their expected results.

[[Plain subfolders of a movie library are `Folder`s](#plain-subfolders-of-a-movie-library-are-folders), [The user's saved view order (`OrderedViews`)](#the-users-saved-view-order-orderedviews), [A video being recorded is keyed under Live TV](#a-video-being-recorded-is-keyed-under-live-tv), “Grouped/preset and special UserView production/selection” (open) and [A box set's `DisplayOrder`](#a-box-sets-displayorder)](../../brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md#additional-source-resolution-finding-from-implementation)
remain separate: physical hierarchy, broader comparers, active-DVR user-data
keys, special view producers and other domain child selection are not established
by this setting's episode-list checks.

Required validation includes core/API/model tests and separate coverage >=80%
each, SQL boundary, real HTTP, production/native comparisons, build, final-milestone doctests
and strict workspace/all-targets/all-features Clippy. Frozen provenance and
runner commands are recorded under
`/tmp/ferrofin-d03-independent-review/`. The failed count-type builds and
HTTP assertions are preserved under `/tmp/ferrofin-d03-root-count-types/` and
`/tmp/ferrofin-d03-http-selection-failure/`; corrected source and pinned-source
visibility evidence are retained in the additive this finding freeze.

The final source and closed normal gates are bound in
`/tmp/ferrofin-d03-source26-freeze-v6/final-freeze.json`
(`8af86b4e27fa92477ca735b8ea37302adcdc5a2aa1cfd52c41a488aa0c6c934a`).
All 32 source/fixture files match the reconstructed prefix and all 22
successor patches apply strictly. The released runner-list proof is
`/tmp/ferrofin-d03-count-types-manifest-preparation/final-v6-runner-list-checks.json`.
Fresh full-suite coverage ran without prior seeds: core 2,371/2,371,
API 908/908, and model 980/980 tests passed. The API export reports
53 functions with mismatched data; that diagnostic is retained in the raw
export and coverage JSON rather than suppressed. Core and model exports
have no LLVM diagnostics.

Validation passed:

- fmt: passed.
- core: 161 tests run: 161 passed, 2210 skipped.
- api: 66 tests run: 66 passed, 842 skipped.
- model: 19 tests run: 19 passed, 961 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 115,686/121,357 lines (95.33%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-api: 41,359/47,941 lines (86.27%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
- ferrofin-model: 8,934/9,901 lines (90.23%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.

Evidence: `/tmp/ferrofin-dashboard-d03-checks.json`,
`/tmp/ferrofin-dashboard-d03-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 21.72 GiB generated target, 534.97 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| False / season-number / get_ms | 2.741 | 3.803 | 12.516 |
| False / season-id / get_ms | 2.739 | 3.605 | 16.707 |
| False / specials-season / get_ms | 2.531 | 3.123 | 13.370 |
| False / all / get_ms | 2.904 | 3.287 | 12.591 |
| False / page / get_ms | 2.460 | 2.676 | 12.159 |
| False / unknown-season / get_ms | 2.363 | 2.289 | 9.816 |
| False / nil-adjacency / get_ms | 2.632 | 4.026 | 11.317 |
| True / season-number / get_ms | 2.633 | 3.192 | 11.819 |
| True / season-id / get_ms | 3.005 | 3.158 | 10.122 |
| True / specials-season / get_ms | 2.300 | 2.751 | 12.816 |
| True / all / get_ms | 2.689 | 3.227 | 10.404 |
| True / page / get_ms | 2.275 | 2.947 | 11.160 |
| True / unknown-season / get_ms | 1.781 | 2.122 | 10.592 |
| True / nil-adjacency / get_ms | 2.567 | 3.741 | 15.530 |
| False / season-number / get_ms (occurrence 2) | 3.440 | 3.522 | 10.900 |
| False / season-id / get_ms (occurrence 2) | 3.216 | 3.499 | 10.071 |
| False / specials-season / get_ms (occurrence 2) | 2.668 | 2.784 | 11.085 |
| False / all / get_ms (occurrence 2) | 2.953 | 3.642 | 9.936 |
| False / page / get_ms (occurrence 2) | 2.268 | 2.706 | 11.289 |
| False / unknown-season / get_ms (occurrence 2) | 1.836 | 2.172 | 9.615 |
| False / nil-adjacency / get_ms (occurrence 2) | 2.602 | 3.166 | 11.736 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

### External content in suggestions

Ordinary LiveTV-program similarity now uses the pinned source's public Program type alias. The storage lookup has no mapping for that alias, so the fallback returns no rows. It previously queried the stored LiveTvProgram kind and returned guide candidates. The movie-like and series-like Program branches keep their source-specific behavior.

The saved `EnableExternalContentInSuggestions` switch already updated all three movie candidate pools live: movie similarity, movie-like Program similarity and movie recommendations. With the setting enabled, those pools include Movie, Trailer and LiveTvProgram. Movie similarity excludes played candidates and applies selected-user access. The pinned query translator omits its IsMovie=true predicate when Movie or Trailer is included, so ordinary and series-like Program candidates can also qualify. Movie-like Program lookup uses a flat genre/tag query without the played predicate. Recent-played baselines remain Movies; liked baselines and people candidate pools expand. Configured remote similarity providers remain callable with either flag value.

Real-manager regressions use false → true → false on one manager and database, all three movie consumers, source-specific Program branches, a Trailer liked baseline, remote-provider execution and denied-library candidates with a grant-access positive control. The main matrix explicitly adopts distinct Program presentation keys to assert each candidate's eligibility. A separate regression keeps real guide NULL keys and verifies that Movie-containing user queries produce one keyless guide representative. Neither fixture changes guide key production.

Source contract: Jellyfin `4910aafa1a`. Native Jellyfin12.1 is separate runtime evidence. The native fixture requires a fresh Completed scan of real movie media/NFO and then adopts isolated Trailer/Program rows with explicit distinct keys. Those keys differ from the actual guide writer's NULL keys. The fixture verifies row consumers and does not establish guide/listings producer or plugin integration.

[“Grouped/preset and special UserView production/selection” (open) and [Home-video libraries resolve plain videos](#home-video-libraries-resolve-plain-videos)](../../brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md#additional-source-resolution-finding-from-implementation) remain separate view-production and home-video-resolution work. Earlier this finding verification-only preparation is superseded by this narrow default-branch correction.

Required validation includes affected core tests and coverage >=80%, SQL boundary, real HTTP, native before/after/reference, build, final-milestone doctests and strict workspace/all-targets/all-features Clippy. The final provenance below binds root's actual gates.

The initial verification-only run exposed two fixture errors: selecting the wrong database filename and expecting stored LiveTvProgram rows where the source query uses the unmapped Program alias. Correcting the native fixture demonstrated the production mismatch. The first corrected core matrix also used NULL presentation keys while expecting distinct rows; distinct adopted keys and a separate real-guide grouping regression now test both contracts. The original failing observations and corrections remain in `/tmp/ferrofin-d04-root-failure-evidence/` and the independent review records.

Target-only cleanup reduced generated build data from 22.07 to 17.89 GiB and preserved the worktree, commits and parent binary. Another build on this host reduced available space below the validation helper's original 512 GiB reserve, so the helper stopped before running tests. The reviewed reserve is now 416 GiB, above 11% of this filesystem's capacity; the 30 GiB target cap, serialized builds and all validation gates remain in force. `/tmp/ferrofin-dashboard-d04-target-cleanup.json` and `/tmp/ferrofin-d04-storage-guard-review/change.json` retain the measurements and exact helper change.

Final provenance is `/tmp/ferrofin-d04-source29-freeze/final-freeze.json` (SHA-256 `78fbc8bac963d228b232c3d86e5edce3337d721e09cc912884dffeecf8d442c4`). It binds all 33 source files to the root-tested tree, the source29 correction, 20 strictly applying unchanged successors, nine passing normal gates, and owned before/reference/after runs with no surviving children. The original checks record retains its earlier source28 manifest; the final snapshot supplies the separately reviewed correction without rewriting that history. Coverage is established by the actual run below.

The final eight-manifest routing proof is `/tmp/ferrofin-d04-source49-manifest-preparation/final-runner-list-checks.json` (SHA-256 `c1b7262bc0a175ab5b9cfbba92d95852772a26994156e0479f2c7e69e187b46f`): all 16 read-only checks/coverage list modes and 249 source/support artifact hashes passed. The independent NFO manifests use the same source49 queue. The this finding's native roles select the source-correct fixture and owned-process wrapper for every phase.

Validation passed:

- fmt: passed.
- core: 37 tests run: 37 passed, 2337 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 116,042/121,705 lines (95.35%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.

Evidence: `/tmp/ferrofin-dashboard-d04-checks.json`,
`/tmp/ferrofin-dashboard-d04-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 19.22 GiB generated target, 507.08 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| False / movie / get_ms | 11.789 | 6.112 | 113.718 |
| False / movie-program / get_ms | 6.763 | 4.767 | 51.567 |
| False / series-program / get_ms | 6.083 | 4.285 | 65.649 |
| False / plain-program / get_ms | 8.482 | 4.282 | 12.718 |
| True / movie / get_ms | 12.976 | 5.463 | 62.304 |
| True / movie-program / get_ms | 7.492 | 5.384 | 42.780 |
| True / series-program / get_ms | 6.225 | 3.921 | 14.240 |
| True / plain-program / get_ms | 7.826 | 3.140 | 8.147 |
| False / movie / get_ms (occurrence 2) | 9.561 | 5.470 | 13.660 |
| False / movie-program / get_ms (occurrence 2) | 7.061 | 5.616 | 14.192 |
| False / series-program / get_ms (occurrence 2) | 4.506 | 5.102 | 17.059 |
| False / plain-program / get_ms (occurrence 2) | 4.595 | 3.683 | 24.602 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

### Date added from file creation time

Linux file-creation dating now matches the source runtime's filesystem clock.
Ferrofin previously preferred statx birth time, while .NET 10.0.12 on Linux uses
native stat without birth-time support and selects the older of inode change
time and modification time. A copied file with a preserved old modification
time was therefore dated by its new birth time instead of the preserved date.
`FileTimes::of` now supplies the Linux source backend's inputs; other operating
systems retain their native creation-time input. The pure birth/fallback
selection helper is unchanged.

The live named metadata option `UseFileCreationTimeForDateAdded` applies to both
new imports and existing physical files whose modification time changes. The
living document's earlier “newly dated items” restriction also needs correction.

New imports use file creation time when enabled, with the source fallback for
an unusable creation date; disabled imports use current import time. On an
existing file change, enabled refresh replaces DateCreated with a valid creation
time, while disabled refresh preserves its saved date. Saving configuration or
refreshing an unchanged file does not itself re-date existing rows. The source
change threshold is an absolute modification-time difference greater than one
second. Folder filesystem inputs do not supply the same file timestamps.

Core cases cover changed/unchanged files, the exact one-second boundary,
missing timestamps, folders and disabled behavior. HTTP/native cases import
under false → true → false and compare existing changed/unchanged refreshes.
Import-time dates must fall within the actual fresh scan interval; creation-time
dates must match a fresh filesystem stat. An editor-seeded
`2001-02-03T04:05:06.1234567Z` must survive unchanged refreshes at 100ns precision.
Every explicit refresh requires both the new NFO title and a new Etag; scans
require a known task's new EndTimeUtc plus Completed status. Time comparisons use
integer ticks, and creation time is re-statted after each file edit.

The Linux regression uses a real file whose modification time is preserved at
2020-01-01 and independently asserts that exact timestamp. The real scan
integration suite distinguishes preserved modification time from birth time
and is selected explicitly for normal tests and fresh affected coverage. The
native fixture checks Ferrofin against Linux stat inputs and Jellyfin against
a small offline BCL oracle reading `FileInfo.CreationTimeUtc` directly. The
oracle's exact 100ns result matched the failed reference observation; neither
comparison uses a tolerance. Raw birth time remains a diagnostic.

The initial fixture posted only DateCreated through the full metadata editor,
which correctly cleared Name in both implementations. It now submits the
current DTO with only DateCreated changed and independently verifies that Name
and all seven fractional date digits survive. Each refresh uses
`replaceAllMetadata=true`: source FullRefresh without replacement only fills
missing metadata and cannot replace the existing title used as its completion
signal. Title-only NFOs have no dateadded value, so replacement retains the
date-policy assertions. A test-only undeclared chrono reference was replaced
with the standard clock, and a redundant struct-default update was removed
after strict Clippy rejected it. The original failures are retained under
`/tmp/ferrofin-d05-root-failure-evidence/`.

Source contract: Jellyfin `4910aafa1a`, `ResolverHelper.SetDateCreated`,
`MetadataService.BeforeSaveInternal`, `BaseItemExtensions.HasChanged`,
`ManagedFileSystem` and `ItemUpdateController`. The measured runtime is .NET
10.0.12, with its exact CoreLib version recorded by the oracle. Its Linux
[native stat backend](https://github.com/dotnet/runtime/blob/v10.0.12/src/native/libs/System.Native/pal_io.c#L185)
and [creation-time fallback](https://github.com/dotnet/runtime/blob/v10.0.12/src/libraries/System.Private.CoreLib/src/System/IO/FileStatus.Unix.cs#L267)
establish the platform clock. Native Jellyfin 12.1.0 is separate runtime
evidence. The fresh import/refresh fixture does not measure the pinned
one-off RefreshInternalDateModified migration. Filesystem creation-time behavior
is evaluated on the host platform, including its available birth-time/fallback
semantics.

[[Plain subfolders of a movie library are `Folder`s](#plain-subfolders-of-a-movie-library-are-folders) and [Unlocked metadata after a library rename and an explicit scan — owner decision needed](#unlocked-metadata-after-a-library-rename-and-an-explicit-scan--owner-decision-needed)](../../brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md#additional-source-resolution-finding-from-implementation)
remain separate physical-hierarchy and general rescan-metadata work. These date
assignment checks do not establish every resolver or unlocked metadata merge.
The fill-only title observation is source behavior and does not establish a
general metadata merge defect.

Required validation includes core tests/coverage >=80%, SQL boundary, real HTTP,
production/native comparisons, build, final-milestone doctests and strict workspace/all-targets/
all-features Clippy. Final provenance is
`/tmp/ferrofin-d05-source31-freeze-v4/final-freeze.json` (SHA-256
`8eb93a97abc39681742693895fce28a2752acf0b7aefabe1ab2257ee6957604f`). It
binds all 34 source files to the root-tested tree, the source31 Linux correction,
19 strictly applying unchanged successors, nine passing normal gates including
all 32 expanded core tests, and owned before/reference/after runs with no
surviving children. It also retains the actual offline oracle build and measured
.NET 10.0.12 creation-time observation. The normal checks record keeps its raw
source30 manifest; the final snapshot supplies the independently reviewed
correction without rewriting that history. The actual coverage run is recorded
below.

Final runner routing is verified in
`/tmp/ferrofin-d05-source50-manifest-preparation/final-complete-runner-list-checks.json`
(SHA-256 `b079a6aff2737d2ea317ebf398db98de4fe567cc04957d48193ddd77f7c43cf1`):
all 16 read-only checks/coverage list modes and 323 source/support artifact
hashes passed. Both this finding filters include `binary(library_scan_date_added)`.
The independent NFO manifests use the same source50 queue. This finding native roles
select the final Linux fixture and the owned wrapper, which binds the exact
published BCL oracle and adds it only to Jellyfin invocations.

Validation passed:

- fmt: passed.
- core: 32 tests run: 32 passed, 2351 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 106,022/121,730 lines (87.10%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.

Evidence: `/tmp/ferrofin-dashboard-d05-checks.json`,
`/tmp/ferrofin-dashboard-d05-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 20.54 GiB generated target, 497.99 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| Existing date phase 0 / refresh_ms | 244.395 | 229.514 | 62.374 |
| Existing date phase 1 / refresh_ms | 240.592 | 217.852 | 60.200 |
| Existing date phase 2 / refresh_ms | 224.360 | 216.083 | 59.448 |
| Existing date phase 3 / refresh_ms | 229.667 | 225.747 | 57.847 |
| Existing date phase 4 / refresh_ms | 229.275 | 224.199 | 56.120 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

## Libraries: metadata defaults

### Default metadata language (`PreferredMetadataLanguage`)

The change carries the live server language/country fallback into named Person and BoxSet metadata and artwork consumers, while retaining item and library overrides. It also corrects the shared TMDB request contract: normalized preferred image language, `null`, and English unless already English; Movie/Series/Season/Episode detail requests append Images. Person requests append Images/ExternalIds/TvCredits/MovieCredits without `include_image_language`. Collection metadata supplies the fallback list; collection artwork requests all languages.

The oracle is explicit source pin `4910aafa1a`, principally `TmdbUtils.GetImageLanguagesParam`, `TmdbClientManager` and the Person/BoxSet providers. The real-router/native fixture checks actual stored metadata, downloaded Primary images, manual candidate order, overrides and regional `es-419` resolution. A changed Etag alone cannot satisfy readiness. Native Jellyfin 12.1.0 is separate supporting evidence, not a substitute for the pin; the actual results are recorded below.

Pinned Person/Collection caches omit country from their raw-language/ID key, so cold-ID phases require real requests while source-consistent warm omissions are allowed. Initial Person biography is observation only because PeopleValidator uses ValidationOnly; explicit FullRefresh establishes the consumer. [Path-less episode metadata providers](#path-less-episode-metadata-providers)/S06 provider execution/order and broader cache, identification and whitespace behavior remain independent work.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-m01-independent-review/m01-checks.py --list
python3 /tmp/ferrofin-m01-independent-review/m01-coverage.py --list
```

Coverage must independently reach 80% for core and providers, with providers freshly measured in full. The locale fixture uses an isolated child-trusted offline TMDB mock and bounded owned-process cleanup. Evidence: `/tmp/ferrofin-m01-independent-review/review.md`; current manifest: `m01-validation-manifest.json` in that directory. 

#### Failed runs and fixture corrections

The original native reference run timed out on the first French Person refresh. The stored biography was already French, but the refresh emitted no TMDB request. The pin caches Person and Collection metadata by ID and raw language for one hour; the initial scan had already warmed that entry. Existing central Primary images can also be protected as local images, and ProviderManager caches downloaded bytes by image URL for ten seconds. These are fixture prerequisites, not additional product defects established by this run.

The corrected this finding fixture uses the normal metadata editor to set a fresh provider ID for each language phase, confirms the stored ID, deletes only the isolated fixture item's existing Primary through the normal image route, and confirms its absence before refresh. Its mock artwork URLs include that provider ID to avoid the separate image-byte cache. It still requires a changed Etag, the exact saved biography/overview and provider ID, a new Primary tag, real selected-language image traffic, three correctly ranked manual candidates, and the source request parameters. [Default metadata country (`MetadataCountryCode`)](#default-metadata-country-metadatacountrycode)'s native fixture remains unchanged. No readiness or wire assertion was weakened.

The first normal validation passed core tests but failed providers test compilation because the new Movie Find regression lacked its function-local TmdbKind/TmdbSearchProvider imports. The correction adds only those imports. The original logs and check records are preserved under `/tmp/ferrofin-m01-root-failure-evidence/`; the failed locale fixture emitted no result JSON, which is explicitly recorded rather than treated as cleanup proof. Fixture preparation and source review are in `/tmp/ferrofin-m01-native-cache-review/`; the applied import patch and Q51 continuation proof are in `/tmp/ferrofin-m01-test-import-followup/`. All sixteen subsequent pieces still apply after that correction.

The next HTTP run reached the new movie-language assertion and failed because the list request did not ask for `Overview`. Both the pinned DtoService and Ferrofin project that field only when requested. The fixture now requests `fields=Overview`; its expected biography, name/rating readiness predicates and timeout remain unchanged. The failed HTTP log and the successful core/providers/reference checks are preserved as `movie-overview-field-*` in the same failure-evidence directory. This follow-up changes only that fixture request, so the completed crate and native-reference gates remain applicable; formatting, the corrected HTTP suite, build, native-after and strict lint are rerun. The exact added patch is recorded by the root-only proof adapter and then incorporated into the frozen continuation sequence.

The subsequent strict Clippy run rejected the offline HTTP mock's case-sensitive URL suffix check as a file-extension comparison. Replacing `ends_with(".png")` with `strip_suffix(".png").is_some()` preserves the exact accepted URL set and case sensitivity; no production or assertion changes are involved. The original lint failure is preserved as `mock-url-lint-*`. Formatting, HTTP and strict lint are rerun, with both exact fixture follow-ups included in the source provenance. Independent review is in `/tmp/ferrofin-m01-http-url-lint-independent-review/`.

Rust 1.98's subsequent strict lint also requires fixed-size array chunks in the TMDB detail-request regression. The test now uses `as_chunks::<4>().0.iter()` instead of `chunks_exact(4)`, with the same complete groups and unchanged wire assertions. The failed log/check record is retained as `request-chunks-lint-*`; formatting, all providers tests and strict lint are rerun after the correction. This affects test iteration only, and is recorded as the third fixture/test follow-up after the original Q51 stack.

Validation passed:

- fmt: passed.
- core: 76 tests run: 76 passed, 2308 skipped.
- providers: 859 tests run: 859 passed, 4 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 116,285/121,954 lines (95.35%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
  Earlier attempt/seed diagnostics retained: warning: 22 functions have mismatched data.
- ferrofin-providers: 24,396/26,491 lines (92.09%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 6 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-m01-checks.json`,
`/tmp/ferrofin-dashboard-m01-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 21.14 GiB generated target, 519.10 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| fr/AR/Person / refresh_ms | 36.970 | 33.480 | 67.870 |
| fr/AR/BoxSet / refresh_ms | 38.000 | 35.430 | 57.470 |
| de/AR/Person / refresh_ms | 37.420 | 33.600 | 74.850 |
| de/AR/BoxSet / refresh_ms | 38.960 | 34.780 | 55.610 |
| es-419/AR/Person / refresh_ms | 36.470 | 33.470 | 60.370 |
| es-419/AR/BoxSet / refresh_ms | 40.880 | 33.410 | 19.530 |
| es-419/AR/Movie / refresh_ms | 211.260 | 201.460 | 62.340 |
| run / setup/configuration / timings.initial_scan_ms | 315.070 | 268.230 | 475.090 |
| run / setup/configuration / timings.config_read | 0.569 / 0.733 | 0.567 / 0.754 | 5.535 / 8.471 |
| run / setup/configuration / timings.ratings_read | 0.452 / 0.549 | 0.359 / 0.470 | 5.355 / 8.340 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

### Default metadata country (`MetadataCountryCode`)

The existing live country inheritance remains the production path: item/library country takes precedence, and a blank library value falls back to the saved server country. This finding independently verifies the actual country consumers after [Default metadata language (`PreferredMetadataLanguage`)](#default-metadata-language-preferredmetadatalanguage)'s shared corrections: TMDB certification selection, live parental-rating tables, and regional language resolution for named Person/BoxSet metadata and artwork. `es-419` resolves to `es-AR` for Argentina and `es-MX` for the other tested country.

The oracle is source pin `4910aafa1a`, including `TmdbUtils.MakeParentalRating` and the language/country resolution passed to TMDB consumers. The real-router and native cases switch DE/FR/AR, clear library overrides and require the corresponding saved movie rating and country-specific `/Localization/ParentalRatings` output. Cold named-item IDs prove actual requests despite the pinned Person/Collection cache's omitted-country key. This finding adds verification rather than duplicating [Default metadata language (`PreferredMetadataLanguage`)](#default-metadata-language-preferredmetadatalanguage)'s production helper changes.

Native Jellyfin 12.1.0 remains a separate runtime comparison; the actual results are recorded below. [Path-less episode metadata providers](#path-less-episode-metadata-providers)/S06 provider execution/order and the inherited response-cache machinery remain open independently. The fixture does not infer cache parity from fresh requests.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-m01-independent-review/m02-checks.py --list
python3 /tmp/ferrofin-m01-independent-review/m02-coverage.py --list
```

The changed nonexempt crate is providers, measured with a fresh full suite and an independent 80% gate. The same isolated offline TMDB fixture owns and bounds its server, media and TLS children; no host trust/cache/account is reused. Evidence: `/tmp/ferrofin-m01-independent-review/review.md`; current manifest: `m02-validation-manifest.json` in that directory.

Validation passed:

- fmt: passed.
- providers: 860 tests run: 860 passed, 4 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-providers: 24,504/26,599 lines (92.12%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 6 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-m02-checks.json`,
`/tmp/ferrofin-dashboard-m02-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 21.82 GiB generated target, 519.00 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| fr/DE/Movie / refresh_ms | 243.840 | 211.560 | 75.000 |
| fr/FR/Movie / refresh_ms | 243.990 | 207.790 | 67.000 |
| fr/AR/Movie / refresh_ms | 239.770 | 239.960 | 65.790 |
| es-419/AR/Person / refresh_ms | 33.830 | 36.120 | 22.760 |
| es-419/AR/BoxSet / refresh_ms | 34.420 | 39.660 | 29.220 |
| es-419/FR/Person / refresh_ms | 32.870 | 37.060 | 21.020 |
| es-419/FR/BoxSet / refresh_ms | 34.330 | 40.690 | 21.550 |
| run / setup/configuration / timings.initial_scan_ms | 320.200 | 313.570 | 518.380 |
| run / setup/configuration / timings.config_read | 0.504 / 0.690 | 0.469 / 0.678 | 6.486 / 8.576 |
| run / setup/configuration / timings.ratings_read | 0.429 / 0.534 | 0.397 / 0.549 | 5.618 / 8.352 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

### Dummy chapter duration (`DummyChapterDuration`)

The change reads the live duration in the actual successful video-probe/scan flow. Source pin `4910aafa1a`, `FFProbeVideoInfo.cs:277–298,605–641`, generates dummy chapters only on Default/FullRefresh, with a positive interval, at most one original chapter and a Video stream. It produces at least one marker for a positive valid runtime, rejects negative or over-12-hour runtimes, permits exactly 12 hours, and uses localized generated names. Multiple original chapters survive.

Successful null-duration video probes now clear stale runtime before chapter construction. The source custom-provider exception path contains an ordinary chapter-construction error: fresh probe fields/streams and the normal completed refresh can persist, while old chapters/images survive and extraction is skipped. Cancellation remains distinct. This does not repair the independent general probe-failure fold.

Core regressions use actual item/stream/chapter repositories for corrupt and null runtimes. The native fixture uses real 36-second video, zero/negative/10/18/90-second settings, single-marker replacement, multiple-marker preservation and French generated names. Fresh Completed task history, Etag and persisted Video/runtime checks prevent stale observations. Jellyfin 12.1.0 supplies a separate runtime comparison; the actual results are recorded below. [Blank and time-formatted chapter names](#blank-and-time-formatted-chapter-names) blank/time-formatted original chapter-name normalization remains open.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-m03-m04-independent-review/m03-checks.py --list
python3 /tmp/ferrofin-m03-m04-independent-review/m03-coverage.py --list
```

Canonical `/tmp/ferrofin-dashboard-m03-{checks,coverage}.py` aliases delegate to the reviewed chapter adapter. Core requires an independent 80% measurement with fresh affected tests and a verified seed or full-suite fallback. The native replacement bounds owned cleanup and preserves original errors. Evidence: `/tmp/ferrofin-m03-m04-independent-review/EVIDENCE.md`. The common locale HTTP gate is separate from the chapter-specific native evidence below.

#### Actual failed runs and fixture corrections

The original Jellyfin 12.1.0 reference passed all eight duration/position controls but returned English generated names after saving `UICulture=fr`. A second run with explicit `culture=fr` requests still produced English names. The saved configuration and completed-task observations were correct. Pinned FFProbeVideoInfo calls `GetLocalizedString`, which reads ambient `CurrentUICulture`. The nested `LimitedConcurrencyLibraryScheduler` retains workers created by the initial English scan; its queued item carries no per-job execution context and idle workers retire after 60 seconds. The outer task's request culture therefore does not determine every persistent child worker's culture.

The revised fixture loads a fresh French `config/system.xml` before starting Jellyfin and keeps startup configuration, explicit refresh culture and saved-setting readbacks aligned. Every French-name, position, runtime, Etag, new Completed task, original-marker and image-off assertion remains. This prerequisite establishes French workers for the duration comparison; it does not claim live ambient-culture parity. [Ambient UI culture vs configured server culture](#ambient-ui-culture-vs-configured-server-culture) tracks distinct current/server string lookups, “Persistent parallel library-worker culture” (open) the persistent nested-worker context, and [Blank and time-formatted chapter names](#blank-and-time-formatted-chapter-names) independent original-name normalization.

The first normal core run passed 73 cases and failed the two real-reprobe regressions because the reused extractor fixture seeded a Movie at fixed ID `0xC28`. The actual scanner derives Movie identity from kind/path, so the tests read the untouched old row after a different row was probed. The fixture now uses the same existing Movie identity derivation; seeded items, streams, chapter rows and image targets follow that ID. No production behavior or runtime/chapter/image/error assertion was changed to address these failures.

After the identity correction, 74 core cases passed and one failed: the fresh unknown runtime and dimensions persisted, but `DefaultVideoStreamIndex` remained the previous `2` instead of the probed `7` when metadata replacement was false. This was a production merge defect. The generic Data fill rules restored the old index after `apply_probe`. A guarded post-merge overlay now restores the successful Video probe's nullable index while preserving unrelated Data and existing Container behavior. Pinned ProbeProvider is a pre-refresh provider; its direct index assignment survives subsequent metadata merging and whole-item locking. New controls cover fresh/null indexes, omitted probes and audio under both replacement and lock choices. The real reprobe test resets the stale index independently for both replacement iterations and keeps its exact fresh-index, streams, runtime, timestamp, chapters and image assertions.

The first prepared overlay contained a test-only typed-reset error caught by independent review before application or execution. The corrected v2 uses a JSON numeric merge with conditional assignment; original preparation and actual failures remain preserved. The new index matrix is explicitly selected by both normal and fresh coverage filters.

Original reference and core records are preserved under `/tmp/ferrofin-m03-root-failure-evidence/`. Native source/culture evidence is in `/tmp/ferrofin-m03-native-culture-review/` and `/tmp/ferrofin-m03-native-culture-independent-review/`. The fixture-identity patch is in `/tmp/ferrofin-m03-probe-fixture-identity-followup/`, and the independently reviewed production index correction is in `/tmp/ferrofin-m03-probe-index-review/`. Durable follow-up planning is in `brain/knowledge/DASHBOARD_SETTINGS_S33_S34_AMBIENT_CULTURE.md`. Corrected native and core runs passed below; the common locale HTTP gate is separate evidence.

Validation passed:

- fmt: passed.
- core: 76 tests run: 76 passed, 2314 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 101,248/122,567 lines (82.61%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.

Evidence: `/tmp/ferrofin-dashboard-m03-checks.json`,
`/tmp/ferrofin-dashboard-m03-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 22.13 GiB generated target, 518.68 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| generation_disabled / elapsed_ms | 219.660 | 225.170 | 157.490 |
| interval_10 / elapsed_ms | 218.830 | 246.320 | 167.650 |
| interval_18 / elapsed_ms | 217.080 | 216.570 | 145.060 |
| interval_larger_than_runtime / elapsed_ms | 219.300 | 219.450 | 154.200 |
| disabled_again / elapsed_ms | 218.820 | 217.800 | 140.010 |
| negative_disables / elapsed_ms | 222.170 | 217.160 | 138.050 |
| single_original_replaced / elapsed_ms | 227.960 | 215.810 | 145.600 |
| multiple_original_preserved / elapsed_ms | 221.510 | 215.500 | 145.850 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

### Chapter image resolution (`ChapterImageResolution`)

The change passes the live saved resolution through the actual chapter-image encoder request. Source pin `4910aafa1a`, `MediaEncoder.cs:651–676,747–760`, maps the eight fixed choices to 256×144, 426×240, 640×360, 854×480, 1280×720, 1920×1080, 2560×1440 and 3840×2160. MatchSource/unknown values omit scaling; audio requests receive no video-resolution argument.

The setting changes newly encoded images. It does not invalidate cached chapter images: pinned `ChapterManager.cs:145–225` keys reuse by date/position rather than resolution. The native case requires identical file hashes/dimensions after P144→P480 while cache remains, then removes image targets before testing newly encoded P480/MatchSource/P360/MatchSource output. It requires three nonempty real images at 0/26/35 seconds and checks actual pixels/dimensions, avoiding empty-result success. Recording-client and argument regressions cover all enum choices and audio exclusion.

Native Jellyfin 12.1.0 is separate runtime evidence; the actual results are recorded below. Existing encoder error/timeout/HDR/3D behavior remains outside this parameter repair. [Blank and time-formatted chapter names](#blank-and-time-formatted-chapter-names) chapter-name normalization is independently open and is not closed by correct image sizing.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-m03-m04-independent-review/m04-checks.py --list
python3 /tmp/ferrofin-m03-m04-independent-review/m04-coverage.py --list
```

Canonical `/tmp/ferrofin-dashboard-m04-{checks,coverage}.py` aliases delegate to the reviewed adapter. Mediaencoding requires a fresh full suite and an independent 80% measurement; prior LLVM diagnostics are not carried as proof. The native replacement bounds owned cleanup and preserves original errors. Evidence: `/tmp/ferrofin-m03-m04-independent-review/EVIDENCE.md`. The common locale HTTP gate is separate from the resolution-specific native evidence below.

The first affected-test build failed because the new dashboard-resolution matrix called the private parent helper without importing it into the test module. A function-local `use super::image_resolution_parameter` fixes that test scope. Production code and every resolution, MatchSource, unknown-value, stream-selection and pixel assertion are unchanged. The original compile log and checks are preserved in `/tmp/ferrofin-m04-test-import-followup/`; independent review is in `/tmp/ferrofin-m04-test-import-independent-review/`.

Validation passed:

- fmt: passed.
- mediaencoding: 8 tests run: 8 passed, 913 skipped.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-mediaencoding: 17,122/18,821 lines (90.97%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 9 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-m04-checks.json`,
`/tmp/ferrofin-dashboard-m04-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 22.67 GiB generated target, 518.41 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| resolution_0_P144 / elapsed_ms | 138.900 | 134.780 | 196.780 |
| resolution_changed_cached_images_retained / elapsed_ms | 29.430 | 27.250 | 38.690 |
| resolution_1_P480 / elapsed_ms | 137.820 | 130.580 | 156.370 |
| resolution_2_MatchSource / elapsed_ms | 133.290 | 140.100 | 148.930 |
| resolution_3_P360 / elapsed_ms | 130.850 | 136.160 | 156.340 |
| resolution_4_MatchSource / elapsed_ms | 137.600 | 137.050 | 152.380 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

## Libraries: NFO

### Save selected-user watch data in NFO (`xbmcmetadata.UserId`)

The change completes the actual selected-user saver consumer. Source pin `4910aafa1a`, `BaseNfoSaver.cs:861–915`, exports the live configured user's rating/favorite/watched/count/date/resume fields, including hidden/disabled users. Blank Unicode whitespace and valid unknown users omit userdata. Source GUID forms D/N/B/P/X are accepted; malformed/URN/nil selections produce contained save failures and preserve the existing file.

Pinned `NfoUserDataSaver.cs:58–86` reacts only to PlaybackFinished, TogglePlayed and UpdateUserRating for eligible local metadata, dispatching Nfo with ordinary library saver policy. The weak-owned listener runs after user-data commit, initially polls inline and detaches pending work; saver failure cannot undo committed data. Automatic native/HTTP observations require actual file changes without a metadata edit. The native failure case holds the obstruction until a new matching saver-error receipt; an HTTP response alone does not prove detached I/O completed.

Likes writes map true→10, false→1 and null→no rating; the getter threshold is 6.5. The selected read now honors current provider/Series/channel/GUID key priority and exact attached-row matching. [A video being recorded is keyed under Live TV](#a-video-being-recorded-is-keyed-under-live-tv) active-DVR key production remains open. Normal pinned local NFO userdata import is inert: temporary-item mutations are neither merged nor persisted. The fixture proves an actual title import before asserting unchanged userdata.

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-dashboard-n01-checks.py --list
python3 /tmp/ferrofin-dashboard-n01-coverage.py --list
```

Core/API/providers each require an independent 80% gate; API uses a fresh full suite. Held/erroring listener and conflicting-key regressions complement the real mutation/file fixture. Jellyfin 12.1.0 comparison remains separate from the source pin; its actual results are recorded below. The owned wrapper bounds child cleanup and preserves original failures. Evidence and linked current manifests: `/tmp/ferrofin-n01-n02-independent-review/`. 

The first core run passed 158 tests and failed one hierarchy test before reaching its assertions. Its parent FK bind used lowercase `Uuid::to_string()`, while `seed_item` stores uppercase hyphenated IDs through `guid_to_db`. The correction uses that existing conversion for the parent bind, matching the neighboring cycle test. Production logic, schema, SQL text and all nearest-parent/missing-pointer/nil-pointer/hydrated/read/write assertions remain unchanged. The actual mismatch is casing, not hyphen removal. Original failure records are preserved under `/tmp/ferrofin-n01-parent-guid-fixture-followup/`; independent review is in `/tmp/ferrofin-n01-parent-guid-fixture-independent-review/`.

After the fixture correction, all 159 core cases passed. The API runner then failed before execution because `binary(playstate)` and `binary(session_ctx)` named handler modules rather than actual Cargo test targets. Cargo metadata confirms `user_library` and `session_playstate`; the corrected filter selects both actual integration binaries and retains the `test(user_library)`, `test(playstate)` and `test(session_ctx)` handler selectors. The API coverage gate still requires a fresh full suite without a seed. Failed-filter records and target metadata are preserved under `/tmp/ferrofin-n01-api-filter-review/` and `/tmp/ferrofin-n01-api-target-metadata.json`.

The providers seeded measurement failed the 80% gate at 1.50%. Its profiles/exports and the actual failed gate remain in the JSON attempt history. The automatic fallback ran the full current providers suite without a seed and measured 92.10%; its six-function LLVM diagnostic is retained. Core measured 82.14% with a verified seed and 22 mismatched functions. API measured 86.27% from the mandatory fresh full 908-test suite, retaining 53 mismatched functions. These diagnostics are disclosed below; no fresh successful run was repeated to erase them.

Validation passed:

- fmt: passed.
- core: 159 tests run: 159 passed, 2241 skipped.
- api: 77 tests run: 77 passed, 831 skipped.
- providers: 17 tests run: 17 passed, 847 skipped.
- traits: passed.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- build: passed.
- clippy: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-core: 101,420/123,465 lines (82.14%). Fresh affected tests and the recorded prior profile were merged and exported against current binaries. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 22 functions have mismatched data.
- ferrofin-api: 41,374/47,956 lines (86.27%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
- ferrofin-providers: 24,504/26,605 lines (92.10%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 6 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-n01-checks.json`,
`/tmp/ferrofin-dashboard-n01-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 23.55 GiB generated target, 518.08 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| selected_a / edit_ms | 20.850 | 19.840 | 78.960 |
| selected_hidden_disabled_b / edit_ms | 16.220 | 15.000 | 34.540 |
| selected_a_again / edit_ms | 22.990 | 14.470 | 28.360 |
| selected_whitespace_guid / edit_ms | 16.030 | 16.450 | 29.290 |
| selected_guid_n / edit_ms | 19.720 | 13.290 | 27.650 |
| selected_guid_b / edit_ms | 18.210 | 14.780 | 30.130 |
| selected_guid_p / edit_ms | 17.150 | 14.090 | 29.480 |
| selected_guid_x / edit_ms | 18.750 | 13.710 | 42.030 |
| malformed_selection / edit_ms | 18.600 | 7.910 | 24.400 |
| urn_selection / edit_ms | 18.400 | 8.250 | 24.860 |
| nil_selection / edit_ms | 21.570 | 7.940 | 24.040 |
| no_selection / edit_ms | 15.570 | 14.090 | 23.580 |
| whitespace_selection / edit_ms | 16.620 | 13.250 | 24.040 |
| unknown_selection / edit_ms | 17.090 | 13.710 | 25.240 |
| rating_boundary_5.0 / http_ms | 7.200 | 6.730 | 20.480 |
| rating_boundary_6.49 / http_ms | 5.160 | 4.880 | 18.290 |
| rating_boundary_6.5 / http_ms | 5.550 | 4.980 | 120.980 |
| auto_initial_favorite / http_ms | 2.720 | 2.720 | 16.450 |
| auto_initial_favorite / observation_ms | 5004.590 | 28.030 | 16.640 |
| ignored_update_userdata / http_ms | 6.690 | 3.270 | 8.910 |
| ignored_update_userdata / observation_ms | 6.760 | 3.320 | 8.960 |
| auto_favorite / http_ms | 6.360 | 3.050 | 14.500 |
| auto_favorite / observation_ms | 5017.690 | 28.290 | 14.630 |
| auto_like / http_ms | 4.720 | 3.380 | 13.930 |
| auto_like / observation_ms | 5020.510 | 28.640 | 14.090 |
| auto_dislike / http_ms | 5.950 | 3.320 | 12.620 |
| auto_dislike / observation_ms | 5006.090 | 28.580 | 13.720 |
| auto_clear_rating / http_ms | 5.730 | 3.810 | 26.480 |
| auto_clear_rating / observation_ms | 5019.010 | 29.060 | 26.650 |
| auto_played / http_ms | 5.940 | 3.270 | 38.320 |
| auto_played / observation_ms | 5030.450 | 28.510 | 38.470 |
| auto_unplayed / http_ms | 6.570 | 3.320 | 29.170 |
| auto_unplayed / observation_ms | 5027.240 | 28.580 | 29.330 |
| auto_other_user_exports_selected_b / http_ms | 4.140 | 3.460 | 12.290 |
| auto_other_user_exports_selected_b / observation_ms | 5005.280 | 28.820 | 12.410 |
| ignored_blank_selection / http_ms | 5.740 | 4.400 | 9.780 |
| ignored_blank_selection / observation_ms | 5.810 | 4.440 | 9.840 |
| ignored_disabled_saver / http_ms | 3.170 | 3.940 | 8.840 |
| ignored_disabled_saver / observation_ms | 3.200 | 3.980 | 8.880 |
| auto_saver_enabled_again / http_ms | 3.060 | 4.030 | 19.540 |
| auto_saver_enabled_again / observation_ms | 5024.310 | 29.380 | 19.830 |
| ignored_playback_start / http_ms | 2.970 | 3.400 | 35.000 |
| ignored_playback_start / observation_ms | 3.000 | 3.440 | 35.090 |
| ignored_playback_progress / http_ms | 1.820 | 2.920 | 17.700 |
| ignored_playback_progress / observation_ms | 1.850 | 2.960 | 17.770 |
| auto_playback_finished / http_ms | 2.040 | 3.260 | 27.820 |
| auto_playback_finished / observation_ms | 5014.940 | 28.650 | 28.030 |
| ignored_second_playback_start / http_ms | 5.710 | 4.090 | 19.660 |
| ignored_second_playback_start / observation_ms | 5.790 | 4.130 | 19.710 |
| auto_playback_finished_without_position / http_ms | 3.100 | 2.620 | 17.320 |
| auto_playback_finished_without_position / observation_ms | 5008.060 | 27.950 | 17.510 |
| userdata_survives_nfo_error / http_ms | 5.940 | 4.460 | 13.860 |
| userdata_survives_nfo_error / observation_ms | 5.970 | 4.470 | 13.880 |
| userdata_survives_nfo_error / saver_error_wait_ms | 5013.370 | 0.050 | 0.070 |
| normal_local_import / refresh_ms | 220.710 | 223.400 | 130.290 |
| auto_initial_favorite / consumer_ms | unavailable | 28.030 | 16.640 |
| auto_favorite / consumer_ms | unavailable | 28.290 | 14.630 |
| auto_like / consumer_ms | unavailable | 28.640 | 14.090 |
| auto_dislike / consumer_ms | unavailable | 28.580 | 13.720 |
| auto_clear_rating / consumer_ms | unavailable | 29.060 | 26.650 |
| auto_played / consumer_ms | unavailable | 28.510 | 38.470 |
| auto_unplayed / consumer_ms | unavailable | 28.580 | 29.330 |
| auto_other_user_exports_selected_b / consumer_ms | unavailable | 28.820 | 12.410 |
| auto_saver_enabled_again / consumer_ms | unavailable | 29.380 | 19.830 |
| auto_playback_finished / consumer_ms | unavailable | 28.650 | 28.030 |
| auto_playback_finished_without_position / consumer_ms | unavailable | 27.950 | 17.510 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

### Save image paths in NFO (`SaveImagePathsInNfo`)

The change covers the actual enabled writer contract. Source pin `4910aafa1a`, `BaseNfoSaver.cs:131–142,842–857,928–989`, permits ImageUpdate saves when enabled and MetadataDownload otherwise. It emits the first Primary, all Backdrops and credited actor Primary thumbs; Logo/Thumb/Banner are excluded. Disabled metadata saves remove those fields. Original trimmed Backdrop paths are stably sorted under current culture before path substitution; remote paths remain unchanged and only emitted paths are mapped.

The culture seam covers immediate HTTP saves, provider-refresh batches, accepted idle scheduled-task entrypoints and detached [Save selected-user watch data in NFO (`xbmcmetadata.UserId`)](#save-selected-user-watch-data-in-nfo-xbmcmetadatauserid) notifications. That does not establish culture parity for every nested full-library child worker. Request selection follows the pinned startup snapshot and query→cookie→Accept-Language precedence using the actual 105-resource supported catalog. Root executed separate .NET 10.0.12 comparer/request/raw-name/mixed-name oracles; the Rust implementations are independently exercised by the validation below. [Generic NFO ordering follows the culture, and duplicates fold Unicode](#generic-nfo-ordering-follows-the-culture-and-duplicates-fold-unicode) generic NFO ordering, [The metadata refresh queue](#the-metadata-refresh-queue) provider priority queue, [Localization dropdown resource catalog](#localization-dropdown-resource-catalog) dropdown catalog, [Busy scheduled-task queue and completion context](#busy-scheduled-task-queue-and-completion-context) busy task draining, [Ambient UI culture vs configured server culture](#ambient-ui-culture-vs-configured-server-culture) ambient localized strings and “Persistent parallel library-worker culture” (open) nested scan-worker culture remain open; [NFO path substitution (`EnablePathSubstitution`)](#nfo-path-substitution-enablepathsubstitution)'s wider path-substitution contract is separate.

The native fixture independently checks false/true/false image fields, excluded kinds, actual actor import, a genuinely new Backdrop failure, Unicode ordering with reversing substitutions, queued/detached saves and a Swedish library scan after an owned restart with saved `UICulture=sv`. Persisted selectable `es_419` is tested through a real restart, reauthentication, Content-Language and an eligible changed-title NFO save. Before observations are bounded; after/reference assert the outputs. Jellyfin 12.1.0 remains separate supporting runtime evidence and the actual results are recorded below.

The Swedish scan prerequisite is explicit in the reviewed native fixture (`315f94d9`). Pin `Folder.cs:840–847` queues child work into `LimitedConcurrencyLibraryScheduler`; its persistent workers capture the first starter's context (`:132–137`) and later execute queued delegates without restoring the newest request's culture (`:150–189,225–244,300–312`). The fixture first reaps its English server, restarts the same isolated data with saved Swedish culture, reauthenticates the same administrator and verifies item/library identity plus unchanged NFO bytes before scanning. It then retains the fresh Completed-task and newly discovered Backdrop checks. This aligned worker lifetime isolates current-culture image ordering; it does not close “Persistent parallel library-worker culture” (open)'s reused-worker/sequential behavior or [Ambient UI culture vs configured server culture](#ambient-ui-culture-vs-configured-server-culture)'s distinct `GetLocalizedString` context, and it does not broaden the completion claim for [The metadata refresh queue](#the-metadata-refresh-queue) or [Busy scheduled-task queue and completion context](#busy-scheduled-task-queue-and-completion-context).

Reviewed entrypoints:

```sh
python3 /tmp/ferrofin-dashboard-n02-checks.py --list
python3 /tmp/ferrofin-dashboard-n02-coverage.py --list
```

Util/core/API/providers each require an independent 80% gate. Traits/server have compile/HTTP composition gates. The fixture override prevents existing FakeLocalization construction panics. Owned cleanup covers all three server lifetimes across the Swedish-worker and raw-culture restarts, preserves original errors, and keeps unique toggle phases. Evidence and linked current manifests: `/tmp/ferrofin-n01-n02-independent-review/`.

The original utility build failed because ICU Locale 2.3 has no `Default` implementation. The invariant empty-culture branch now uses the installed library's `Locale::UNKNOWN` (`und`) constant. ICU's root collator metadata uses that same undetermined locale without tailoring; public culture identity/name remain empty and the parent remains absent. The existing empty-culture comparer and all culture-name/ordering controls remain unchanged. The compile failure is preserved under `/tmp/ferrofin-n02-icu-root-locale-followup/`, and installed-API/source review under `/tmp/ferrofin-n02-icu-root-locale-independent-review/`. Earlier static review missed this constructor; it was the actual utility build that exposed it.

The corrected utility build exposed a real comparison failure: bundled ICU4X excludes search collation tables and silently falls back to ordinary ordering. German search therefore incorrectly placed ä before ae. A phonebook alias also fails the independent Arabic, Thai, symbol and Hangul controls. The isolated provider imports 22 actual search/searchjl tailorings (44 data/metadata files) from the official ICU78.1rc export matched to the installed ICU4X data. Safe trie constructors and an exact root-data comparison cover CE/context/Jamo/diacritic arrays plus 1,102,940 non-Hangul code points. Its initial expanded 500 comparisons pass, but the subsequent full matrix of 1,276 pairs, every 11,172 precomposed Hangul syllables and 29 culture identities exposes Korean, searchjl and parser/fallback differences. A first isolated vendor-condition experiment also failed, including ordinary-culture controls. These are preserved failed attempts; those earlier prototypes were not accepted into production or counted complete. The final implementation below includes source-data and vendor license notices, reproducible generation and a complete independent comparison. Evidence: `/tmp/ferrofin-n02-search-portable-data-review/`, `/tmp/ferrofin-n02-search-full-hangul-prototype-oracle-v2/` and `/tmp/ferrofin-n02-search-full-hangul-prototype-oracle-v3/`.

The subsequent isolated v4 consumer passes the strict complete comparison: 29 culture identities/acceptance/name/parent results, 1,276 pair signs for every valid culture, both comparison vectors for all 11,172 precomposed Hangul syllables, stable path ordering, 42 staged metadata/lazy-comparer cases and all 22 imported constructors. It restores the 67 modern Jamo entries omitted by genrb using private trie blocks, verified independently against all 24,510,464 logical source code points; remaining arrays and high/error values are unchanged. Korean archaic equality rules come from CLDR and apply only to the resolved ko/search tailoring; one reset expands through an imported root equality. A narrowly patched vendored ICU4X honors actual Jamo CE32s while retaining the original root shortcut. Primary conversion evidence is [ICU genrb](https://github.com/unicode-org/icu/blob/release-78.1rc/icu4c/source/tools/genrb/parse.cpp), [ICU collation builder](https://github.com/unicode-org/icu/blob/release-78.1rc/icu4c/source/i18n/collationdatabuilder.cpp) and [CLDR Korean collation](https://github.com/unicode-org/cldr/blob/release-48-2/common/collation/ko.xml). Actual prototype checks and strict comparison are preserved under `/tmp/ferrofin-n02-search-full-hangul-consumer-oracle-v4/`; comparison SHA256 is `f5caa57e794869ae2f4d15951e37ef69eb64d6142550f53bd89953417aad8b5b`. This establishes the prototype behavior; the actual production comparison is recorded next.

The integrated production implementation also passes the strict .NET comparison through the actual public Ferrofin API: 29 culture acceptance rows (25 usable comparers and four matching lazy failures), 31,900 pair signs, 558,600 syllable signs, stable sorting and all 42 staged construction/metadata/comparer cases. Every one of the 22 imported tailoring constructors succeeds. No copied parser or prototype adapter is used. The frozen production input manifest is `/tmp/ferrofin-n02-search-full-hangul-production-oracle-v4/oracle-manifest.json` (SHA256 `a8612fe20721b9930d283f9096c8bfa9938dcbc41a2078ef5b85ad74c64a91ad`); its actual strict comparison is `comparison.json` (SHA256 `6d58ea0524f0001df45cf79c677a65f22ea48746a14de7c0344291bf0e576214`). The normal utility gate runs all 15 culture tests, including the exact compiled-root compatibility, .NET pair oracle and every modern syllable. This source61 checkpoint preceded the integration and coverage corrections recorded below; final current-source results are listed separately.

A fresh run of the shipped data converter reproduces both generated files byte for byte (601,792 bytes total), with every pinned input hash unchanged. The actual regeneration record is `/tmp/ferrofin-dashboard-n02-data-regeneration-checks.json`; preserved input/output evidence is `/tmp/ferrofin-n02-data-regeneration-preserved/manifest.json`. All 195 utility tests pass with a fresh, unseeded 96.481217% line-coverage result (2,029/2,103 lines, 33 fresh profiles, no LLVM diagnostics).

The real server HTTP gate then exposed a separate actor-image lookup bug: enabled artwork emits the movie poster/fanart but omits the credited actor Primary image. The fixture imports an actual NFO credit, resolves its actual Person item and uploads that item's image. The saver incorrectly reads images using the credit row ID. Pinned `BaseNfoSaver.cs:963–972` instead calls `GetPerson(credit.Name)` and uses that exact Person's first Primary; `LibraryManager.GetPerson` derives the configured per-name path identity and requires the resulting row to be a Person. The strong off/on/off HTTP assertion is retained. The failed gate and source fixture are preserved under `/tmp/ferrofin-n02-http-actor-failure/`; the correction and its subsequent integration evidence are recorded below; the failed observation remains part of the review history.

The actor-image correction now derives aligned per-name Person IDs through the repository's already configured identity seam, retrieves those exact rows and accepts only Person items before reading their first Primary image. Director/Writer credits and a disabled image-path flag skip this lookup; duplicate identities reuse the image read. It does not fall back to credit IDs, arbitrary name matches or retained previous identities. No SQL statement or service-ownership wiring is added. The actual applied sequence is `56464ef3` plus the test-only owned-directory correction `d5722740`; the earlier run was interrupted during compilation before executing the fixed-root fixture. Its record is preserved under `/tmp/ferrofin-n02-actor-fixture-interruption/`. All 269 affected core tests, including the new real-repository identity regressions, pass. The subsequent actual HTTP run passes the unchanged actor off/on/off controls, then fails the distinct newly appended Backdrop path assertion. Its failed record, log and unchanged fixture are preserved under `/tmp/ferrofin-n02-http-new-backdrop-failure/`. The writer uses the logical slot count as its internal filename suffix, so a refresh-created suffix gap can make an append overwrite a currently referenced file. Pinned `ImageSaver.cs:121–128,528–583` separates the logical slot from its first-unused case-insensitive Backdrop filename. The additive `1aea87ad` fix retains the requested logical slot while allocating the first-unused positive filename suffix, then removes only a replaced file owned by that item after successful persistence. Case-insensitive duplicate references, external/shared files and paths escaping through a symlinked parent are retained. This repair covers internal upload allocation and owned replacement cleanup. The existing media-folder saver separately removes replaced local artwork. Complete eligible old-path cleanup and source deletion-error propagation need separate lifecycle review; the existing and new Ferrofin cleanup helpers log non-FileNotFound deletion errors where the pin propagates them. All 164 affected provider tests pass, including nine pinned filename cases and real file-preservation controls. The unchanged actual HTTP gate now passes, including the new path, count and image-read assertions after an NFO write error. Independent source review: `/tmp/ferrofin-n02-backdrop-independent-review/REVIEW-v3.md`.

This uses the existing default normalized by-name identity rules. It does not add support for the nondefault by-name identity contract (`EnableNormalizedItemByNameIds=false` with `EnableCaseSensitiveItemIds=true`) or the pin's 128-byte truncation/hash rule for overlong by-name folder names. Those are preexisting general identity gaps recorded separately as [By-name identity settings and long folder names](#by-name-identity-settings-and-long-folder-names); the short and awkward-name controls establish the tested actor lookup without claiming full named-item lifecycle parity.

Strict Clippy exposed concrete parser/API style errors and intentional non-NFC oracle operands. The additive source65–70 corrections use explicit default types, consume the final owned base name, read the same last cookie by backward iteration, document the existing constructor panic, move an unchanged test helper before statements, use fixed test arrays and Rust’s standard no-op waker, and format explicit test defaults. A narrowly documented Unicode lint allowance preserves the exact generated .NET oracle module; all operand/sign payload bytes remain unchanged. No comparison assertion is relaxed. Failed Clippy attempts remain under `/tmp/ferrofin-n02-parser-lint-followup/`, `/tmp/ferrofin-n02-api-lint-followup/`, `/tmp/ferrofin-n02-api-test-order-lint-followup/` and `/tmp/ferrofin-n02-util-test-lint-followup/`. Final source70 passes formatting, all 15 culture tests and strict workspace Clippy for all targets/features. Historical source61 utility coverage is preserved under `/tmp/ferrofin-n02-source61-utility-coverage-preserved/` before its current-source refresh.

The final source70 public production API rerun also passes the strict comparison with unchanged inputs before and after. Its comparison SHA256 remains `6d58ea0524f0001df45cf79c677a65f22ea48746a14de7c0344291bf0e576214`; all 65 inputs and 45 production hashes are verified. The root-only runner adds `--locked` to the existing offline Rust command. Actual execution and saved strict-checker output are under `/tmp/ferrofin-n02-search-full-hangul-production-oracle-source70/`, with `actual-strict-receipt.json` recording the actual exit status. All 195 utility tests pass again without a seed: 2,031/2,104 lines (96.530418%), 15 fresh profiles and no LLVM diagnostics. Final normal gates pass 269 core, 122 API and 164 provider tests, unchanged real HTTP and native after observation, build, formatting, traits, SQL boundary and strict Clippy. Original source61 generation/utility evidence remains separately preserved. [By-name identity settings and long folder names](#by-name-identity-settings-and-long-folder-names) general identity and [Artwork replacement cleanup failures](#artwork-replacement-cleanup-failures) cleanup-error handling are still open planning work.

Every final changed-crate coverage gate passes with a fresh full suite and no seed: utility 195 tests, 96.530418%; core 2,406 tests, 95.393642%; API 914 tests, 86.384957%; providers 880 tests, 91.982437%. Core/API/provider seeded attempts measured 17.625382%/16.664945%/39.943918% and failed their threshold; their tests passed, and the runner retained both attempts before its required full fallback. Final utility/core exports have no LLVM diagnostics; API retains a 53-function mismatch warning and providers a six-function warning, including in their fresh full exports. No passing full run was repeated to suppress those diagnostics. Exact counts, profiles, tests, seed provenance and warnings remain in `/tmp/ferrofin-dashboard-n02-coverage.json`. After all coverage runtimes closed, generated `target/llvm-cov-target` was removed under the validation lock: target usage fell from 28.37 to 14.64 GiB with unchanged source status and all four verified profiles retained. Evidence: `/tmp/ferrofin-n02-post-coverage-target-cleanup.json`.

Validation passed:

- fmt: passed.
- util: 15 tests run: 15 passed, 180 skipped.
- core: 269 tests run: 269 passed, 2137 skipped.
- api: 122 tests run: 122 passed, 792 skipped.
- providers: 164 tests run: 164 passed, 716 skipped.
- traits: passed.
- sql-boundary: 1 test run: 1 passed, 0 skipped.
- http: 1 test run: 1 passed, 0 skipped.
- data-regeneration: passed.
- build: passed.
- clippy: passed.
- production-culture-oracle: passed.

Each changed nonexempt crate passed its own line-coverage gate:

- ferrofin-util: 2,031/2,104 lines (96.53%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-core: 118,042/123,742 lines (95.39%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: no LLVM diagnostics.
- ferrofin-api: 41,806/48,395 lines (86.38%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 53 functions have mismatched data.
  Earlier attempt/seed diagnostics retained: warning: 53 functions have mismatched data.
- ferrofin-providers: 24,930/27,103 lines (91.98%). A fresh full test suite was exported against current binaries without a seed. Provenance and any full-suite fallback are retained in the JSON record.
  Successful export/merge: warning: 6 functions have mismatched data.
  Earlier attempt/seed diagnostics retained: warning: 6 functions have mismatched data.

Evidence: `/tmp/ferrofin-dashboard-n02-checks.json`,
`/tmp/ferrofin-dashboard-n02-coverage.json`, and the passed native before/after/reference JSON records.
Storage recorded at the strict Clippy gate: 19.70 GiB generated target, 519.38 GiB free on the host.
Builds/coverage/native processes were serialized with one compiler job and
incremental/debug data disabled. The source worktree and commits are preserved.


Final 80-finding milestone validation:

- fmt: passed.
- workspace: 8332 tests run: 8332 passed (1 slow), 5 skipped.
- doctests: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s; ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s.

Evidence: `/tmp/ferrofin-dashboard-settings-80-milestone.json`. Workspace tests used the immutable
date-binding oracle; the opt-in compiled WASM guest tests remained disabled.


Measured local before/after observations (milliseconds):

| Phase / consumer / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| images_False_Logo / edit_ms | 10.290 | 9.900 | 30.550 |
| images_False_Logo / image_update_ms | 10.410 | 11.950 | 46.360 |
| images_True_Backdrop / edit_ms | 11.190 | 13.220 | 15.640 |
| images_True_Backdrop / image_update_ms | 16.160 | 19.750 | 34.260 |
| images_False_Thumb / edit_ms | 11.320 | 11.930 | 15.480 |
| images_False_Thumb / image_update_ms | 10.410 | 18.560 | 27.630 |
| order_startup_default_after_ui_change / http_ms | 1.770 | 1.840 | 8.440 |
| order_startup_default_after_ui_change / observation_ms | 5026.990 | 253.800 | 84.420 |
| order_queued_swedish / http_ms | 2.350 | 2.040 | 8.120 |
| order_queued_swedish / consumer_ms | 2.480 | 203.540 | 58.790 |
| order_queued_swedish / observation_ms | 2.480 | 203.540 | 58.790 |
| order_image_query_precedes_cookie_header / http_ms | 12.860 | 21.300 | 29.010 |
| order_image_query_precedes_cookie_header / observation_ms | 5019.410 | 21.380 | 29.230 |
| order_detached_favorite_swedish / http_ms | 6.520 | 4.020 | 46.380 |
| order_detached_favorite_swedish / consumer_ms | 6.620 | 29.220 | 46.530 |
| order_detached_favorite_swedish / observation_ms | 6.620 | 29.220 | 46.530 |
| order_image_cookie_precedes_header / http_ms | 15.340 | 16.240 | 46.570 |
| order_image_cookie_precedes_header / observation_ms | 5038.480 | 16.310 | 46.780 |
| order_image_accept_language_swedish / http_ms | 14.710 | 16.570 | 31.500 |
| order_image_accept_language_swedish / consumer_ms | 14.830 | 16.630 | 31.690 |
| order_image_accept_language_swedish / observation_ms | 14.830 | 16.630 | 31.690 |
| order_image_reset_before_library / http_ms | 10.710 | 17.190 | 30.420 |
| order_image_reset_before_library / observation_ms | 5033.690 | 17.260 | 30.560 |
| swedish_scan_worker_restart / restart_ms | 2617.610 | 2598.930 | 4918.410 |
| order_full_library_background / http_ms | 0.910 | 0.660 | 11.810 |
| order_full_library_background / consumer_ms | 1.050 | 202.110 | 1072.070 |
| order_full_library_background / observation_ms | 1.050 | 202.110 | 1072.070 |
| order_images_disabled / edit_ms | 18.550 | 13.190 | 73.930 |
| image_save_survives_nfo_error / image_update_ms | 13.820 | 18.110 | 47.110 |
| order_reset_before_raw_restart / http_ms | 16.250 | 19.760 | 20.410 |
| order_reset_before_raw_restart / consumer_ms | 16.400 | 19.920 | 20.550 |
| order_reset_before_raw_restart / observation_ms | 16.400 | 19.920 | 20.550 |
| raw_saved_culture_restart / restart_ms | 506.400 | 506.110 | 4106.270 |
| raw_saved_culture_restart / http_ms | 15.300 | 24.130 | 353.990 |
| raw_saved_culture_restart / observation_ms | 5027.020 | 24.380 | 354.210 |
| order_startup_default_after_ui_change / consumer_ms | unavailable | 253.800 | 84.420 |
| order_image_query_precedes_cookie_header / consumer_ms | unavailable | 21.380 | 29.230 |
| order_image_cookie_precedes_header / consumer_ms | unavailable | 16.310 | 46.780 |
| order_image_reset_before_library / consumer_ms | unavailable | 17.260 | 30.560 |
| raw_saved_culture_restart / consumer_ms | unavailable | 24.380 | 354.210 |

Paired values show median / p95 where the native fixture records repeated
HTTP reads; single values are the actual recorded request or completion time.
Before uses the preserved parent binary; after uses the verified current server. Each
run creates the same isolated data and exercises the same setting transitions.
Unavailable means that consumer was absent or did not complete in that run.
These are local observations on a shared host, not publishable benchmark claims.
The preferred Docker benchmark was unavailable; no quiet-host result is claimed.
The runtime reference is separate from the pinned source oracle.

### NFO path substitution (`EnablePathSubstitution`)

The checkbox is persisted but unused by the pinned Jellyfin server as well as Ferrofin. This corrects the original finding's implication that Ferrofin alone omits a working source consumer. Web `1e507c588f353482a00333f84e36ddb7c8fc8221`, `src/apps/dashboard/routes/libraries/nfo.tsx:39,147–153`, exposes and saves it. Source pin `4910aafa1a` only references the property in `MediaBrowser.Model/Configuration/XbmcMetadataOptions.cs:12,21`.

Pinned `BaseNfoSaver.cs:980–988` unconditionally maps local artwork through `LibraryManager.GetPathAfterNetworkSubstitution`, while remote artwork retains its URL. The library manager applies the first successful server path mapping (`LibraryManager.cs:3505–3515`). This behavior is independent of the named NFO checkbox. Changing `SaveImagePathsInNfo` controls whether poster/fanart/actor thumbs are emitted; changing server mappings changes their exported paths. Neither operation moves the artwork itself. A newly invented checkbox consumer would diverge from this source contract.

The real HTTP regression independently saves false/true/false, checks each readback, forces a new titled NFO save and verifies mapped movie poster, backdrop and exact credited Person thumb each time. It verifies unchanged image catalogs and original files, then disables the image fields and removes the server mappings as independent controls. The isolated binary fixture repeats this through a generated video, a real NFO actor import and uploaded images; its owned-process verifier asserts exact emitted fields, saved values and all six phases. Jellyfin 12.1.0 is supporting runtime evidence, separate from the primary source pin. Before and after use the same production behavior because this finding adds verification only.

The only Rust changes are in the server integration test, which is exempt from line coverage under the contributor policy. No nonexempt library crate changes. Existing broad NFO ordering/path edge cases and additional findings remain separately tracked; this verification does not claim to repair the misleading checkbox in Jellyfin Web.

Validation passed:

- fmt: passed (2.23 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (22.99 s, including any compilation).
- build: passed (66.98 s, including any compilation).
- native-before: passed (6.17 s, including any compilation).
- native-reference: passed (8.98 s, including any compilation).
- native-after: passed (6.27 s, including any compilation).
- clippy: passed (1.67 s, including any compilation).
- doctests: 3 passed, 0 ignored (111.97 s, including any compilation).

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| path_substitution_0 / edit_ms | 15.048 | 14.043 | 15.971 |
| path_substitution_1 / edit_ms | 13.635 | 13.383 | 19.784 |
| path_substitution_2 / edit_ms | 13.332 | 14.521 | 12.606 |
| image_fields_disabled / edit_ms | 12.236 | 12.114 | 14.053 |
| mappings_removed / edit_ms | 13.255 | 11.921 | 13.370 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/n03/checks.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 416 GiB host reserve. Source and commits are preserved.


The independently reviewed HTTP title assertion was strengthened after the first native run. Source manifest 002 repeats formatting, HTTP (one test passed) and strict Clippy on the final test source; production code and the native fixture are unchanged from source manifest 001. Original tools, fixture and test source for manifest 001 are preserved alongside its evidence.

### Extra thumbnail duplication (`EnableExtraThumbsDuplication`)

The production implementation already added for [Artwork saved beside media](#artwork-saved-beside-media) consumes the named option live. This finding adds independent verification of that setting rather than another production change. Web pin `1e507c588f353482a00333f84e36ddb7c8fc8221` exposes it in NFO settings; primary server source is `4910aafa1a`.

Pinned `ImageSaver.cs:63–70,595–654` reads the flag and adds `extrathumbs/thumb<logical index>` for positive-index Compatible local backdrops. Index zero, mixed folders, Legacy convention and internal saving use one output. Turning the flag off stops subsequent copies and preserves existing duplicate files. Mixed-folder exclusion is source-backed and covered by existing policy tests; the new native fixture does not claim to exercise mixed folders.

The HTTP regression and independent native fixture check false/true/false, existing sentinel bytes, first-backdrop exclusion, Legacy and internal destinations, failure of a duplicate destination and recovery without restarting. The native verifier requires ten distinct phases, actual upload status, actual selected-file bytes and duplicate bytes, no thumb zero, and unchanged image catalog on failure. The pinned saver writes all outputs before publishing the image path (`ImageSaver.cs:156–175`), so a failed second copy propagates a server error rather than silently falling back. The first output can already have been written; this is not a transactional rollback claim. Broader cleanup failures remain separately tracked.

Only the server integration test changes, so no nonexempt library crate requires a coverage run. Existing source and worktree history are preserved. Before and after share the already implemented production behavior; Jellyfin 12.1.0 provides separate supporting runtime evidence.

The first reference attempt is retained as failed evidence: indexed POST uploads append in pinned `ImageController.cs:395–423`, whereas Ferrofin currently honors the index. The corrected native fixture uses the unindexed append route and clears cataloged backdrops before each phase and seeds a first backdrop when needed to obtain the requested logical index. It checks the actual starting count and retains the same substantive duplicate-byte and error assertions. A second retained attempt confirmed the already tracked [Indexed artwork deletion](#indexed-artwork-deletion) all-slots deletion behavior; clearing the entire catalog avoids relying on single-slot deletion. Indexed upload API parity remains a separate open finding; the setting implementation is independently verified through the shared append route.

The third retained reference attempt found an upstream corruption bug: on the two successful duplicate phases, Jellyfin writes the duplicate PNG but leaves the cataloged first output empty. Pinned `ImageController.cs:85–86` supplies a nonseekable `CryptoStream`; `ImageSaver.cs:137–145` copies it into a `MemoryStream` without rewinding, and `156–160` rewinds only for subsequent outputs. The final verifier asserts that exact empty first output for these reference phases and full PNG bytes for both Ferrofin outputs; every other successful reference output still requires full PNG bytes. Ferrofin intentionally retains correct artwork bytes rather than reproducing this source bug. Duplicate eligibility, live toggles and failure behavior remain independently gated.

Validation passed:

- fmt: passed (2.18 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (20.66 s, including any compilation).
- build: passed (0.44 s, including any compilation).
- native-before: passed (6.29 s, including any compilation).
- native-reference: passed (9.39 s, including any compilation).
- native-after: passed (6.29 s, including any compilation).
- clippy: passed (1.58 s, including any compilation).
- doctests: 3 passed, 0 ignored (6.18 s, including any compilation).

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| off_first / upload_ms | 21.216 | 17.896 | 62.241 |
| off_extra / upload_ms | 16.774 | 16.405 | 20.679 |
| off_preserves_existing / upload_ms | 17.551 | 20.555 | 17.060 |
| enabled_extra / upload_ms | 25.310 | 30.125 | 17.385 |
| disabled_again / upload_ms | 21.044 | 18.164 | 16.718 |
| enabled_first / upload_ms | 15.801 | 17.972 | 18.018 |
| legacy_extra / upload_ms | 13.885 | 17.424 | 14.427 |
| internal_extra / upload_ms | 9.706 | 8.812 | 15.465 |
| blocked_duplicate / upload_ms | 17.934 | 21.973 | 7.831 |
| repaired_duplicate / upload_ms | 16.063 | 19.588 | 17.518 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/n04/checks.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 416 GiB host reserve. Source and commits are preserved.

## Playback: transcoding

### Resolved encoder path display (`EncoderAppPathDisplay`)

FFmpeg-validated production startup now atomically saves the resolved FFmpeg executable into the named encoding configuration before services are composed. The existing encoding GET consequently returns the actual startup path to the dashboard's disabled display box. Updating this display preserves all typed encoding options and survives restart; invalid FFmpeg preserves a previously saved display. A real persistence failure propagates from startup.

Primary source `4910aafa1a`, `MediaBrowser.MediaEncoding/Encoder/MediaEncoder.cs`, `SetFFmpegPath`, validates startup selection and then saves `EncoderAppPathDisplay`. Web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. The display is not an independent health endpoint or live executable selector. A client may still write the field with a full encoding document, and the next successful startup refreshes it, as in upstream. The hidden named `EncoderAppPath` startup selection discrepancy remains separate; this change does not implement that selector.

Production discovery carries the independently validated FFmpeg display path, including typed error context when a later ffprobe check fails. Invalid FFmpeg never attaches that context. The existing bare runtime fallback and capability behavior remain unchanged; fake compositions initialize the display path absent. Core regressions cover option preservation, restart and changed executables, absent or corrupt configuration, and a blocked persistence destination. The owned native fixture starts with two different copied FFmpeg/ffprobe executable paths, checks encoding readback and durability, separately exercises failed and missing FFprobe, and finally verifies that invalid FFmpeg preserves the previous display. It retains raw response bytes, status, encoding-file hashes and owned process exits. Before observations preserve the missing binding rather than asserting the proposed fix.

Only core needs separate line coverage; traits and server wiring have their compile/integration exemption. Relevant bootstrap, composition and HTTP checks still run. Supporting Jellyfin 12.1.0 observations remain distinct from the pinned source. No GPU availability is claimed.

The retained first reference attempt exposed a startup-lifecycle difference: the pinned host throws when FFmpeg validation fails (`ApplicationHost.cs:427–433`), and supporting Jellyfin exits with code zero after logging the fatal startup error. Ferrofin already permits startup with a fallback encoder. The corrected failure phase asserts the reference's actual exit, fatal log and unchanged persisted XML display, while Ferrofin must remain reachable and return its unchanged saved display. This finding corrects display persistence; it does not claim startup-failure lifecycle parity.

The second retained attempt caught a fixture alias: the save step changed the in-memory first GET observation while its captured response bytes remained correct. The fixture now copies the document before changing it; the independent verifier additionally requires every decoded JSON observation to equal its hashed raw response. No production assertion was relaxed.

Independent review found and corrected two issues in the preliminary implementation before coverage or commit. Pinned display persistence precedes FFprobe support checks; the final startup binding now preserves the validated FFmpeg display through missing and failing FFprobe validation without changing fallback runtime selection. Core startup also intentionally retries failed legacy XML import: the setter now leaves the named JSON absent while that import is pending, and the existing two-mode adoption regression repairs the XML, restarts, verifies imported options and then persists the display. The display remains unavailable until malformed legacy XML is repaired; a warning explains this preserved adoption limitation. Broader FFprobe/version/startup validation differences remain separate from the display setting.

The final native fixture adds separate missing and failing FFprobe phases. It retains the actual failing executable source and phase-scoped invocation JSON, requiring Ferrofin's real `-version` call and the reference's real `-only_first_vframe` support probe. Removing the executable must yield no new invocation receipt; both servers must persist the validated FFmpeg path. The original failed attempts remain intact. Strict Clippy rejected a floating-point equality in the adoption test; the final assertion compares exact IEEE bits for the exactly representable imported value rather than suppressing the lint or weakening the preservation check.

The normal targeted core, selected server, HTTP and binary build gates use source manifest 004. Manifest 005 changes only the core adoption test's floating-point assertion and the native fixture/verifier; production Rust is unchanged. The final full core coverage suite, expanded native runs, strict lint and doctests use manifest 005. Earlier successful gate evidence and failed attempts remain preserved.

Validation passed:

- fmt: passed (2.39 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (50.23 s, including any compilation).
- build: passed (76.99 s, including any compilation).
- native-before: passed (29.54 s, including any compilation).
- native-reference: passed (29.46 s, including any compilation).
- native-after: passed (17.21 s, including any compilation).
- clippy: passed (64.32 s, including any compilation).
- doctests: 3 passed, 0 ignored (64.10 s, including any compilation).
- ferrofin-core: 34 tests run: 34 passed, 2375 skipped (123.04 s, including any compilation).
- server: 48 tests run: 48 passed, 121 skipped (50.44 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-core: 118,160/123,865 lines (95.394179%), fresh full suite without seed.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| resolved_startup_0 / startup.startup_ms | 9162.894 | 5588.300 | 7229.243 |
| resolved_startup_0 / response.milliseconds | 4.595 | 1.918 | 63.016 |
| save_preserves_display / save.milliseconds | 9.245 | 7.247 | 23.954 |
| save_preserves_display / response.milliseconds | 1.655 | 0.995 | 7.800 |
| resolved_startup_1 / startup.startup_ms | 5003.224 | 2767.516 | 5011.579 |
| resolved_startup_1 / response.milliseconds | 3.180 | 1.861 | 200.458 |
| validated_ffmpeg_failed_ffprobe / startup.startup_ms | 4667.577 | 2861.587 | 6345.355 |
| validated_ffmpeg_failed_ffprobe / response.milliseconds | 6.749 | 1.720 | 146.422 |
| validated_ffmpeg_missing_ffprobe / startup.startup_ms | 5363.224 | 2729.717 | 6277.692 |
| validated_ffmpeg_missing_ffprobe / response.milliseconds | 6.428 | 1.968 | 144.761 |
| failed_discovery_keeps_prior_display / startup.startup_ms | 4252.962 | 2501.774 | unavailable |
| failed_discovery_keeps_prior_display / response.milliseconds | 2.360 | 1.747 | unavailable |
| failed_discovery_keeps_prior_display / failure_ms | unavailable | unavailable | 2442.034 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t01/checks.json` and `/tmp/ferrofin-next10-root/evidence/t01/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 416 GiB host reserve. Source and commits are preserved.

### Effective transcode directory (`TranscodingTempPath`)

The saved encoding path now selects the one live application transcode root. Startup loads the saved value; committed named-config changes publish it to subsequent operations. Output planning, current-root legacy HLS routes, system/storage reporting, maintenance cleanup and Live TV buffer selection use that shared root and prepare its ownership marker. An already planned operation retains its original path; this selects destinations rather than moving existing files.

Primary source pin `4910aafa1a` uses `EncodingConfigurationExtensions.GetTranscodePath`, recursive marker validation in `BaseApplicationPaths`, `SystemManager` and legacy HLS/transcode/cleanup consumers. Web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. Null or empty values restore the current cache/transcodes default, and whitespace remains a configured path. The original audit's validator warning is stale: [Validate and atomically save named configurations](#validate-and-atomically-save-named-configurations) previously registered the encoding-store validator in production. Changed non-whitespace paths must exist before saving; unchanged paths and whitespace follow the source's distinct handling. A rejected save must preserve both persisted and effective selection.

Core regressions cover startup, committed updates, captured operation paths, cache fallback, restart and consistent system/storage reports. Owned utility tests cover recursive matching/conflicting markers, case-sensitive Linux wildcard prefix, dangling conflicting markers and directory/marker creation errors. The native fixture saves two actual directories and fetches generated HLS playlist/segment bytes in each, exercises rejection and legacy filename routing, restarts, clears the setting, saves whitespace and observes conflicting marker errors. Eight additional phases cover nested matching-case markers, ignored uppercase glob prefix, dangling conflicting markers, a directory in place of the marker, preservation of existing marker bytes, and matching dangling links, FIFOs and sockets. Special entries retain their exact lstat identity; no FIFO/socket payload is opened. Linux socket binding uses an owned directory descriptor to avoid long socket addresses and process-wide cwd changes.

Separate changed-crate coverage is required for utility, core, HLS and Live TV. Traits and server wiring are exempt from line coverage but compile, planner/composition and real HTTP checks remain required. The new native fixture exercises output and storage paths; it does not claim to run a real tuner, scheduled cleanup task, or a path switch while a previous transcode remains running. Their shared-root/captured-path wiring is source and unit backed. Broader marker ownership and filesystem enumeration edges remain separate; a cyclic symlink fixture is not executed.

The first two core attempts and the following server build were stopped during compilation when host free space crossed the original 416 GiB reserve. Their logs and failure records remain intact; none reached a failing test. Quiescent cleanup removed generated test executables, coverage build cache and an obsolete generated release cache, retaining source, commits, actual server binaries, response bytes and exported profiles. The validator now records budget failures without triggering a second guard exception while recording them.

A read-only request to this node's kubelet `configz` verified explicit `nodefs.available` and `imagefs.available` hard thresholds of `100Gi`, with no soft thresholds. The root-only validation reserve is consequently recalibrated to 256 GiB, leaving 156 GiB above the configured threshold and keeping the 30 GiB generated-target cap, one compiler job and watched process groups. No node, cluster or historical helper file is changed. [Kubernetes documents absolute eviction thresholds](https://kubernetes.io/docs/concepts/scheduling-eviction/node-pressure-eviction/). The live observation, its hash and the policy are preserved in `/tmp/ferrofin-next10-root/node-disk-policy-probe.json` and `budget-policy.json`; all earlier guard interruptions remain failed attempts.

Earlier native before/reference, utility, core, HLS and Live TV passes use manifests 001/002. Manifest 003 changes the root budget/evidence policy and adds a scoped deprecated-field allowance to the test that deliberately verifies both legacy `SystemInfo.TranscodingTempPath` and newer storage reporting. Production Rust and native fixtures are unchanged. The final full core coverage suite and strict lint exercise that exact test. The first server, HTTP, binary and 16-phase native-after gates passed on manifest 003, but its lint gate rejected the load-function length and missing warning semicolon. A private initializer extraction preserves the exact cache→encoding/default/path preparation→metadata startup order, without suppression. Independently reviewed Unix marker creation now mirrors .NET LStat/Stat existence rules: matching dangling symlinks and non-directory special files are already present; matching directories and directory links still fail. The original regular-file-only check could create a dangling link target or block on a FIFO. The subsequent core coverage compilation was intentionally stopped; its raw profiles and the preliminary utility export remain retained. The validation runner now requires passing normal gates before dependent coverage and gives every verified profile an immutable attempt-specific path. Manifest 005 freezes these source/fixture corrections. Utility/core/server/HTTP/build, expanded 19-phase native runs, lint/doc and fresh full coverage use it; earlier targeted HLS/Live TV passes are retained alongside exact final full suites.

Validation passed:

- fmt: passed (2.29 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (28.36 s, including any compilation).
- build: passed (63.02 s, including any compilation).
- native-before: passed (8.94 s, including any compilation).
- native-reference: passed (13.93 s, including any compilation).
- native-after: passed (8.86 s, including any compilation).
- clippy: passed (121.18 s, including any compilation).
- doctests: 3 passed, 0 ignored (77.17 s, including any compilation).
- ferrofin-util: 10 tests run: 10 passed, 194 skipped (2.03 s, including any compilation).
- ferrofin-core: 62 tests run: 62 passed, 2351 skipped (110.43 s, including any compilation).
- ferrofin-hls: 31 tests run: 31 passed, 85 skipped (40.59 s, including any compilation).
- ferrofin-livetv: 140 tests run: 140 passed, 156 skipped (67.41 s, including any compilation).
- server: 74 tests run: 74 passed, 95 skipped (81.39 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-util: 2,218/2,291 lines (96.813619%), fresh full suite without seed.
- ferrofin-core: 118,304/124,021 lines (95.390297%), fresh full suite without seed.
- ferrofin-hls: 2,800/3,232 lines (86.633663%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 1 functions have mismatched data.
- ferrofin-livetv: 10,324/11,000 lines (93.854545%), fresh full suite without seed.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| default_path / encoding.milliseconds | 2.282 | 2.286 | 48.115 |
| default_path / info.milliseconds | 2.502 | 1.686 | 8.657 |
| default_path / storage.milliseconds | 3.124 | 1.715 | 8.851 |
| default_path / startup.startup_ms | 4935.517 | 4897.239 | 5952.164 |
| fixture_media_main / generation_ms | 43.277 | 44.153 | 40.504 |
| configured_A / encoding.milliseconds | 1.605 | 0.815 | 5.972 |
| configured_A / info.milliseconds | 1.821 | 1.395 | 6.088 |
| configured_A / storage.milliseconds | 2.561 | 2.213 | 8.256 |
| configured_A / save.milliseconds | 7.017 | 7.117 | 13.772 |
| transcode_A / encoding.milliseconds | 1.573 | 1.492 | 6.643 |
| transcode_A / info.milliseconds | 1.791 | 1.786 | 5.942 |
| transcode_A / storage.milliseconds | 3.081 | 2.646 | 6.754 |
| transcode_A / transcode.playlist.milliseconds | 3.466 | 2.836 | 40.250 |
| transcode_A / transcode.segment.milliseconds | 42.403 | 43.640 | 141.466 |
| configured_B / encoding.milliseconds | 0.923 | 2.382 | 5.857 |
| configured_B / info.milliseconds | 1.361 | 2.262 | 5.801 |
| configured_B / storage.milliseconds | 2.663 | 3.115 | 7.884 |
| configured_B / save.milliseconds | 6.340 | 7.464 | 6.222 |
| transcode_B / encoding.milliseconds | 1.461 | 1.923 | 7.341 |
| transcode_B / info.milliseconds | 1.589 | 2.263 | 5.982 |
| transcode_B / storage.milliseconds | 2.511 | 4.100 | 6.751 |
| transcode_B / transcode.playlist.milliseconds | 3.285 | 2.990 | 13.187 |
| transcode_B / transcode.segment.milliseconds | 40.088 | 37.422 | 114.673 |
| missing_changed_path_rejected / encoding.milliseconds | 0.840 | 0.785 | 5.832 |
| missing_changed_path_rejected / info.milliseconds | 1.394 | 1.319 | 6.387 |
| missing_changed_path_rejected / storage.milliseconds | 3.243 | 2.377 | 6.794 |
| missing_changed_path_rejected / save.milliseconds | 1.272 | 1.567 | 7.667 |
| legacy_current_root / encoding.milliseconds | 1.753 | 0.710 | 6.253 |
| legacy_current_root / info.milliseconds | 1.887 | 1.104 | 5.954 |
| legacy_current_root / storage.milliseconds | 3.250 | 1.948 | 6.409 |
| legacy_current_root / legacy.milliseconds | 0.882 | 0.896 | 13.082 |
| saved_path_after_restart / encoding.milliseconds | 1.645 | 1.911 | 82.939 |
| saved_path_after_restart / info.milliseconds | 1.698 | 1.683 | 12.545 |
| saved_path_after_restart / storage.milliseconds | 2.733 | 2.531 | 13.175 |
| saved_path_after_restart / startup.startup_ms | 2699.511 | 2680.466 | 4532.456 |
| clear_to_cache / encoding.milliseconds | 0.763 | 1.979 | 9.892 |
| clear_to_cache / info.milliseconds | 1.094 | 2.250 | 7.037 |
| clear_to_cache / storage.milliseconds | 3.181 | 2.930 | 6.832 |
| clear_to_cache / save.milliseconds | 6.856 | 7.654 | 22.895 |
| whitespace_is_a_path / encoding.milliseconds | 0.706 | 0.810 | 5.531 |
| whitespace_is_a_path / info.milliseconds | 0.930 | 1.265 | 5.940 |
| whitespace_is_a_path / storage.milliseconds | 1.711 | 1.926 | 7.724 |
| whitespace_is_a_path / save.milliseconds | 6.357 | 6.588 | 6.060 |
| conflict_detected_on_access / encoding.milliseconds | 0.641 | 0.675 | 7.166 |
| conflict_detected_on_access / info.milliseconds | 0.883 | 1.287 | 10.736 |
| conflict_detected_on_access / storage.milliseconds | 1.549 | 1.304 | 6.621 |
| conflict_detected_on_access / save.milliseconds | 5.720 | 5.331 | 8.519 |
| marker_matching_nested / save.milliseconds | 6.266 | 7.626 | 6.547 |
| marker_matching_nested / encoding.milliseconds | 0.654 | 0.733 | 32.360 |
| marker_matching_nested / info.milliseconds | 0.915 | 1.029 | 11.998 |
| marker_matching_nested / storage.milliseconds | 1.636 | 1.784 | 12.514 |
| marker_uppercase_prefix / save.milliseconds | 5.913 | 7.147 | 6.103 |
| marker_uppercase_prefix / encoding.milliseconds | 0.879 | 2.758 | 5.495 |
| marker_uppercase_prefix / info.milliseconds | 1.225 | 2.866 | 5.797 |
| marker_uppercase_prefix / storage.milliseconds | 2.675 | 4.387 | 7.017 |
| marker_dangling_conflict / save.milliseconds | 7.065 | 7.197 | 6.283 |
| marker_dangling_conflict / encoding.milliseconds | 0.951 | 0.841 | 6.151 |
| marker_dangling_conflict / info.milliseconds | 1.473 | 1.473 | 6.124 |
| marker_dangling_conflict / storage.milliseconds | 1.984 | 1.466 | 6.171 |
| marker_is_directory / save.milliseconds | 5.907 | 5.923 | 5.769 |
| marker_is_directory / encoding.milliseconds | 0.670 | 0.657 | 8.779 |
| marker_is_directory / info.milliseconds | 0.960 | 1.152 | 12.723 |
| marker_is_directory / storage.milliseconds | 1.622 | 1.247 | 6.625 |
| marker_existing_not_truncated / save.milliseconds | 6.516 | 5.463 | 31.995 |
| marker_existing_not_truncated / encoding.milliseconds | 0.714 | 0.662 | 6.742 |
| marker_existing_not_truncated / info.milliseconds | 1.063 | 1.064 | 18.778 |
| marker_existing_not_truncated / storage.milliseconds | 2.151 | 2.075 | 6.900 |
| marker_matching_dangling / save.milliseconds | 6.015 | 7.506 | 11.216 |
| marker_matching_dangling / encoding.milliseconds | 0.858 | 1.500 | 10.466 |
| marker_matching_dangling / info.milliseconds | 1.052 | 1.745 | 6.675 |
| marker_matching_dangling / storage.milliseconds | 2.205 | 2.750 | 6.741 |
| marker_matching_fifo / save.milliseconds | 6.561 | 5.484 | 6.776 |
| marker_matching_fifo / encoding.milliseconds | 0.799 | 0.717 | 8.154 |
| marker_matching_fifo / info.milliseconds | 1.009 | 1.083 | 5.866 |
| marker_matching_fifo / storage.milliseconds | 1.679 | 1.902 | 6.301 |
| marker_matching_socket / save.milliseconds | 5.813 | 7.008 | 5.751 |
| marker_matching_socket / encoding.milliseconds | 0.653 | 0.650 | 5.251 |
| marker_matching_socket / info.milliseconds | 0.899 | 0.956 | 5.373 |
| marker_matching_socket / storage.milliseconds | 1.805 | 1.700 | 6.789 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t02/checks.json` and `/tmp/ferrofin-next10-root/evidence/t02/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.


Independent source and actual-native acceptance is preserved in `/tmp/ferrofin-next10-throttling/t02-independent-review/source005-native-acceptance.json` (SHA256 `f406c37297ac5713c069dede38f17f20e07db60dfddb5c9ccbb8942c07b4e3e8`). Its receipt preceded the now-passing final normal/coverage gates; it does not substitute for them.

The HLS coverage suite includes opt-in FFmpeg cases that return early when their environment flag is absent. The separate actual-binary fixture executes real FFmpeg HLS jobs and verifies the resulting bytes; no opt-in integration execution is inferred from the coverage test count.

### Encoding threads and `CpuCoreLimit`

The missing `CpuCoreLimit` query now reaches the shared streaming request and planner. It overrides saved `EncodingThreadCount` before the existing helper applies the host processor count; it is not a minimum against the saved setting. Nonpositive selected values emit FFmpeg `-threads 0`. Positive values are capped at the processor count captured when `EncodingHelper` is built.

Primary source is Jellyfin `4910aafa1a`, `EncodingHelper.GetNumberOfThreads` and the nullable CPU request binding; web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. A CPU-specific deserializer follows ASP.NET Core v10 `SimpleTypeModelBinder` and .NET `BaseNumberConverter` / `Int32Converter`: omitted, bare, empty and whitespace values are null; nonempty values are trimmed before signed decimal or #/0x/&h hexadecimal conversion, including signed bit patterns and the decimal trailing-NUL compatibility. Malformed/overflow/fraction/interior-NUL values reject before planning. Only this query field uses the new helper. Existing named JSON binding, raw-response immutability and earlier path/display findings remain intact.

Source-derived regressions use the actual query extractor and a planner with persisted named encoding configuration. They cover case spellings, blank/Unicode inputs, signed boundaries, hexadecimal forms, malformed values, override precedence and emitted arguments. Planner unit tests pin eight processors for determinism. Root separately measured the production helper's `std::thread::available_parallelism`:32 available and32 allowed processors, recorded in `/tmp/ferrofin-next10-root/cpu-count.json` with source and binary hashes. That current runtime evidence supports the native positive2/3 assertions; the processor-count detector is unchanged.

The native fixture has 31 substantive setting/request phases: the original seven thread-selection cases plus15 accepted/absent nullable-query forms and9 rejected forms. Bootstrap/media/probe/owned-cleanup rows are additional evidence. It saves settings over authenticated HTTP, captures immutable copies of actual encoding JSON/XML and raw response bytes/status/headers/timing, then receives a real HLS playlist/transport segment and the actual launched FFmpeg argv for accepted requests. Rejected inputs must return400 without starting an HLS encoder. The independent verifier checks saved file content, exact query, real TS packet bytes and `-threads`, without trusting a fixture pass label. Baseline accepts observed behavior differences while enforcing evidence integrity; reference/after enforce source-derived substantive outcomes.

The recording executable passes unchanged args to actual FFmpeg and restricts only that process's affinity; the server processor count remains the measured host count. This verifies argument propagation and genuine media output, not thread-performance scaling. Supporting Jellyfin12.1.0 runtime evidence remains separate from the primary pin; no GPU execution claim is made. 

Changed-crate coverage is required separately for `ferrofin-api` at>=80%; server/traits retain their documented compile/integration exemptions. Run HLS query/controller regressions, planner propagation and existing thread-helper tests, the maintained dashboard HTTP regression, strict formatting/Clippy/doctests/build, serial native before/reference/after, then fresh API coverage. Preserve the date/JSON oracle inputs and existing [Resolved encoder path display (`EncoderAppPathDisplay`)](#resolved-encoder-path-display-encoderapppathdisplay)/T02 verifier guards. The first expanded server gate exposed a preexisting composition fixture using `/cache/transcodes`, which [Effective transcode directory (`TranscodingTempPath`)](#effective-transcode-directory-transcodingtemppath) now prepares. Its owned temporary paths are repaired without changing production. Earlier API and native before/reference passes are retained with unchanged code/fixtures; the corrected server gate and final coverage use the subsequent frozen inputs. The failed attempt remains preserved.

Validation passed:

- fmt: passed (2.32 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (26.76 s, including any compilation).
- build: passed (65.42 s, including any compilation).
- native-before: passed (8.12 s, including any compilation).
- native-reference: passed (14.54 s, including any compilation).
- native-after: passed (7.78 s, including any compilation).
- clippy: passed (108.28 s, including any compilation).
- doctests: 3 passed, 0 ignored (79.41 s, including any compilation).
- ferrofin-api: 24 tests run: 24 passed, 891 skipped (270.72 s, including any compilation).
- server: 88 tests run: 88 passed, 82 skipped (8.88 s, including any compilation).
- thread-helper: 3 tests run: 3 passed, 763 skipped (14.54 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-api: 41,829/48,484 lines (86.273822%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 53 functions have mismatched data.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 5136.886 | 5103.444 | 7248.050 |
| fixture_media_main / generation_ms | 46.526 | 47.251 | 100.320 |
| threads_0 / save.milliseconds | 6.672 | 7.871 | 21.551 |
| threads_0 / encoding.milliseconds | 0.826 | 1.466 | 10.677 |
| threads_0 / operation.playlist.milliseconds | 2.793 | 3.321 | 111.003 |
| threads_0 / operation.segment.milliseconds | 50.356 | 54.127 | 158.264 |
| threads_0 / stop.milliseconds | 2.264 | 1.986 | 34.433 |
| threads_1 / save.milliseconds | 8.567 | 6.545 | 5.139 |
| threads_1 / encoding.milliseconds | 1.270 | 1.176 | 4.836 |
| threads_1 / operation.playlist.milliseconds | 3.260 | 3.316 | 9.044 |
| threads_1 / operation.segment.milliseconds | 50.360 | 50.082 | 112.021 |
| threads_1 / stop.milliseconds | 1.987 | 1.805 | 6.433 |
| threads_2 / save.milliseconds | 6.651 | 7.727 | 7.320 |
| threads_2 / encoding.milliseconds | 0.724 | 1.060 | 4.595 |
| threads_2 / operation.playlist.milliseconds | 2.512 | 3.146 | 7.554 |
| threads_2 / operation.segment.milliseconds | 54.842 | 51.833 | 110.520 |
| threads_2 / stop.milliseconds | 1.884 | 1.812 | 5.691 |
| threads_3 / save.milliseconds | 6.789 | 6.583 | 5.302 |
| threads_3 / encoding.milliseconds | 1.321 | 0.806 | 4.786 |
| threads_3 / operation.playlist.milliseconds | 3.271 | 2.795 | 8.332 |
| threads_3 / operation.segment.milliseconds | 47.968 | 51.351 | 111.178 |
| threads_3 / stop.milliseconds | 1.892 | 1.796 | 5.532 |
| threads_4 / save.milliseconds | 7.982 | 7.380 | 5.818 |
| threads_4 / encoding.milliseconds | 1.364 | 0.875 | 4.788 |
| threads_4 / operation.playlist.milliseconds | 3.627 | 2.652 | 7.937 |
| threads_4 / operation.segment.milliseconds | 48.978 | 54.565 | 110.881 |
| threads_4 / stop.milliseconds | 2.020 | 2.702 | 5.879 |
| threads_5 / save.milliseconds | 6.511 | 8.546 | 5.144 |
| threads_5 / encoding.milliseconds | 0.706 | 1.682 | 20.998 |
| threads_5 / operation.playlist.milliseconds | 2.195 | 3.368 | 7.828 |
| threads_5 / operation.segment.milliseconds | 45.258 | 51.402 | 110.222 |
| threads_5 / stop.milliseconds | 1.038 | 1.776 | 5.281 |
| threads_6 / save.milliseconds | 7.017 | 6.014 | 4.675 |
| threads_6 / encoding.milliseconds | 0.704 | 0.759 | 5.099 |
| threads_6 / operation.playlist.milliseconds | 3.443 | 2.518 | 8.142 |
| threads_6 / operation.segment.milliseconds | 45.050 | 56.005 | 112.769 |
| threads_6 / stop.milliseconds | 1.610 | 1.957 | 5.211 |
| cpu_query_omitted / save.milliseconds | 7.728 | 9.098 | 4.765 |
| cpu_query_omitted / encoding.milliseconds | 0.938 | 2.454 | 4.476 |
| cpu_query_omitted / operation.playlist.milliseconds | 2.673 | 4.360 | 7.738 |
| cpu_query_omitted / operation.segment.milliseconds | 45.338 | 59.982 | 111.124 |
| cpu_query_omitted / stop.milliseconds | 1.864 | 1.928 | 5.564 |
| cpu_query_omitted / elapsed_ms | 61.665 | 80.591 | 140.438 |
| cpu_query_bare / save.milliseconds | 7.304 | 7.450 | 4.687 |
| cpu_query_bare / encoding.milliseconds | 1.048 | 0.898 | 4.680 |
| cpu_query_bare / operation.playlist.milliseconds | 2.993 | 2.854 | 7.338 |
| cpu_query_bare / operation.segment.milliseconds | 43.943 | 51.868 | 110.405 |
| cpu_query_bare / stop.milliseconds | 1.056 | 1.806 | 5.397 |
| cpu_query_bare / elapsed_ms | 58.887 | 67.214 | 139.476 |
| cpu_query_empty / save.milliseconds | 8.595 | 6.795 | 7.581 |
| cpu_query_empty / encoding.milliseconds | 0.706 | 1.060 | 6.344 |
| cpu_query_empty / operation.playlist.milliseconds | 2.612 | 3.676 | 7.609 |
| cpu_query_empty / operation.segment.milliseconds | 44.568 | 50.820 | 110.476 |
| cpu_query_empty / stop.milliseconds | 1.008 | 1.914 | 8.152 |
| cpu_query_empty / elapsed_ms | 59.273 | 66.913 | 147.721 |
| cpu_query_ascii_white / save.milliseconds | 6.313 | 6.838 | 5.142 |
| cpu_query_ascii_white / encoding.milliseconds | 0.697 | 0.854 | 4.941 |
| cpu_query_ascii_white / operation.playlist.milliseconds | 2.197 | 3.021 | 8.340 |
| cpu_query_ascii_white / operation.segment.milliseconds | 43.667 | 50.410 | 211.861 |
| cpu_query_ascii_white / stop.milliseconds | 1.170 | 1.715 | 37.352 |
| cpu_query_ascii_white / elapsed_ms | 56.243 | 65.018 | 275.957 |
| cpu_query_unicode_white / save.milliseconds | 8.617 | 7.425 | 5.055 |
| cpu_query_unicode_white / encoding.milliseconds | 0.680 | 0.870 | 5.292 |
| cpu_query_unicode_white / operation.playlist.milliseconds | 2.918 | 2.773 | 7.810 |
| cpu_query_unicode_white / operation.segment.milliseconds | 46.412 | 51.381 | 110.600 |
| cpu_query_unicode_white / stop.milliseconds | 1.883 | 1.687 | 5.123 |
| cpu_query_unicode_white / elapsed_ms | 63.003 | 66.534 | 142.907 |
| cpu_query_trimmed_plus / save.milliseconds | 10.404 | 7.063 | 5.213 |
| cpu_query_trimmed_plus / encoding.milliseconds | 0.885 | 1.313 | 6.194 |
| cpu_query_trimmed_plus / operation.playlist.milliseconds | 2.130 | 3.713 | 8.322 |
| cpu_query_trimmed_plus / operation.segment.milliseconds | 45.052 | 52.627 | 115.390 |
| cpu_query_trimmed_plus / stop.milliseconds | 0.935 | 1.909 | 6.635 |
| cpu_query_trimmed_plus / elapsed_ms | 62.021 | 69.129 | 148.852 |
| cpu_query_unicode_trim / save.milliseconds | 5.567 | 7.558 | 4.865 |
| cpu_query_unicode_trim / encoding.milliseconds | 0.688 | 0.898 | 5.819 |
| cpu_query_unicode_trim / operation.playlist.milliseconds | 2.058 | 2.984 | 7.402 |
| cpu_query_unicode_trim / operation.segment.milliseconds | 43.919 | 51.965 | 185.796 |
| cpu_query_unicode_trim / stop.milliseconds | 1.631 | 1.576 | 5.668 |
| cpu_query_unicode_trim / elapsed_ms | 55.992 | 67.397 | 217.061 |
| cpu_query_hex / save.milliseconds | 7.684 | 7.171 | 7.854 |
| cpu_query_hex / encoding.milliseconds | 1.224 | 1.028 | 4.917 |
| cpu_query_hex / operation.playlist.milliseconds | 3.506 | 2.611 | 7.496 |
| cpu_query_hex / operation.segment.milliseconds | 45.358 | 55.147 | 110.481 |
| cpu_query_hex / stop.milliseconds | 1.840 | 1.764 | 5.442 |
| cpu_query_hex / elapsed_ms | 62.851 | 70.329 | 144.482 |
| cpu_query_hex_hash_nested_plus / save.milliseconds | 6.887 | 7.994 | 5.033 |
| cpu_query_hex_hash_nested_plus / encoding.milliseconds | 1.009 | 1.274 | 7.166 |
| cpu_query_hex_hash_nested_plus / operation.playlist.milliseconds | 2.912 | 5.023 | 9.202 |
| cpu_query_hex_hash_nested_plus / operation.segment.milliseconds | 45.320 | 63.266 | 110.652 |
| cpu_query_hex_hash_nested_plus / stop.milliseconds | 1.801 | 1.951 | 5.916 |
| cpu_query_hex_hash_nested_plus / elapsed_ms | 61.212 | 82.424 | 145.193 |
| cpu_query_hex_ampersand / save.milliseconds | 8.149 | 6.741 | 5.053 |
| cpu_query_hex_ampersand / encoding.milliseconds | 1.231 | 0.950 | 5.741 |
| cpu_query_hex_ampersand / operation.playlist.milliseconds | 4.729 | 3.234 | 12.738 |
| cpu_query_hex_ampersand / operation.segment.milliseconds | 50.023 | 63.021 | 114.971 |
| cpu_query_hex_ampersand / stop.milliseconds | 2.264 | 1.970 | 5.665 |
| cpu_query_hex_ampersand / elapsed_ms | 70.132 | 78.742 | 152.496 |
| cpu_query_hex_signed_bits / save.milliseconds | 7.409 | 6.937 | 5.462 |
| cpu_query_hex_signed_bits / encoding.milliseconds | 1.898 | 1.339 | 5.231 |
| cpu_query_hex_signed_bits / operation.playlist.milliseconds | 4.060 | 3.374 | 8.035 |
| cpu_query_hex_signed_bits / operation.segment.milliseconds | 50.013 | 53.326 | 111.379 |
| cpu_query_hex_signed_bits / stop.milliseconds | 1.893 | 2.152 | 6.376 |
| cpu_query_hex_signed_bits / elapsed_ms | 68.929 | 70.313 | 144.496 |
| cpu_query_trailing_nuls / save.milliseconds | 6.575 | 7.589 | 6.525 |
| cpu_query_trailing_nuls / encoding.milliseconds | 0.837 | 0.917 | 5.817 |
| cpu_query_trailing_nuls / operation.playlist.milliseconds | 3.022 | 3.396 | 8.898 |
| cpu_query_trailing_nuls / operation.segment.milliseconds | 53.471 | 52.266 | 112.144 |
| cpu_query_trailing_nuls / stop.milliseconds | 1.688 | 1.736 | 6.098 |
| cpu_query_trailing_nuls / elapsed_ms | 68.381 | 68.632 | 147.206 |
| cpu_query_white_before_nul / save.milliseconds | 7.442 | 15.104 | 5.570 |
| cpu_query_white_before_nul / encoding.milliseconds | 0.747 | 1.703 | 6.007 |
| cpu_query_white_before_nul / operation.playlist.milliseconds | 3.654 | 3.419 | 7.634 |
| cpu_query_white_before_nul / operation.segment.milliseconds | 46.621 | 54.671 | 113.720 |
| cpu_query_white_before_nul / stop.milliseconds | 1.526 | 1.702 | 7.739 |
| cpu_query_white_before_nul / elapsed_ms | 62.946 | 79.872 | 148.941 |
| cpu_query_duplicate_first / save.milliseconds | 6.579 | 7.430 | 30.825 |
| cpu_query_duplicate_first / encoding.milliseconds | 1.056 | 0.756 | 5.521 |
| cpu_query_duplicate_first / operation.playlist.milliseconds | 3.690 | 2.964 | 8.510 |
| cpu_query_duplicate_first / operation.segment.milliseconds | 50.975 | 56.401 | 222.907 |
| cpu_query_duplicate_first / stop.milliseconds | 2.112 | 2.062 | 7.830 |
| cpu_query_duplicate_first / elapsed_ms | 67.724 | 72.065 | 291.947 |
| cpu_query_duplicate_empty_first / save.milliseconds | 30.224 | 7.695 | 5.613 |
| cpu_query_duplicate_empty_first / encoding.milliseconds | 1.431 | 0.991 | 5.023 |
| cpu_query_duplicate_empty_first / operation.playlist.milliseconds | 3.700 | 3.428 | 7.812 |
| cpu_query_duplicate_empty_first / operation.segment.milliseconds | 54.742 | 50.384 | 467.531 |
| cpu_query_duplicate_empty_first / stop.milliseconds | 1.510 | 2.225 | 6.547 |
| cpu_query_duplicate_empty_first / elapsed_ms | 94.695 | 68.198 | 500.985 |
| cpu_query_invalid_word / save.milliseconds | 5.867 | 11.541 | 6.483 |
| cpu_query_invalid_word / encoding.milliseconds | 0.800 | 1.694 | 4.806 |
| cpu_query_invalid_word / operation.playlist.milliseconds | 2.400 | 1.135 | 11.795 |
| cpu_query_invalid_word / operation.segment.milliseconds | 47.143 | unavailable | unavailable |
| cpu_query_invalid_word / stop.milliseconds | 1.103 | 0.797 | 5.874 |
| cpu_query_invalid_word / elapsed_ms | 59.675 | 18.496 | 37.888 |
| cpu_query_decimal_overflow / save.milliseconds | 6.600 | 6.274 | 4.572 |
| cpu_query_decimal_overflow / encoding.milliseconds | 0.746 | 1.282 | 4.311 |
| cpu_query_decimal_overflow / operation.playlist.milliseconds | 2.651 | 0.986 | 4.891 |
| cpu_query_decimal_overflow / operation.segment.milliseconds | 46.984 | unavailable | unavailable |
| cpu_query_decimal_overflow / stop.milliseconds | 1.081 | 0.786 | 4.152 |
| cpu_query_decimal_overflow / elapsed_ms | 60.231 | 12.017 | 24.615 |
| cpu_query_hex_overflow / save.milliseconds | 6.750 | 6.782 | 4.535 |
| cpu_query_hex_overflow / encoding.milliseconds | 0.684 | 0.995 | 4.329 |
| cpu_query_hex_overflow / operation.playlist.milliseconds | 2.058 | 0.777 | 5.102 |
| cpu_query_hex_overflow / operation.segment.milliseconds | 46.941 | unavailable | unavailable |
| cpu_query_hex_overflow / stop.milliseconds | 1.014 | 0.612 | 4.078 |
| cpu_query_hex_overflow / elapsed_ms | 59.503 | 11.277 | 24.580 |
| cpu_query_fraction / save.milliseconds | 6.261 | 6.594 | 4.517 |
| cpu_query_fraction / encoding.milliseconds | 0.669 | 0.879 | 4.130 |
| cpu_query_fraction / operation.playlist.milliseconds | 2.225 | 0.622 | 4.676 |
| cpu_query_fraction / operation.segment.milliseconds | 44.144 | unavailable | unavailable |
| cpu_query_fraction / stop.milliseconds | 0.948 | 0.535 | 4.289 |
| cpu_query_fraction / elapsed_ms | 56.256 | 10.550 | 24.040 |
| cpu_query_negative_hex / save.milliseconds | 6.265 | 6.017 | 4.411 |
| cpu_query_negative_hex / encoding.milliseconds | 0.672 | 0.772 | 4.305 |
| cpu_query_negative_hex / operation.playlist.milliseconds | 2.289 | 0.666 | 5.008 |
| cpu_query_negative_hex / operation.segment.milliseconds | 43.328 | unavailable | unavailable |
| cpu_query_negative_hex / stop.milliseconds | 2.100 | 0.612 | 4.589 |
| cpu_query_negative_hex / elapsed_ms | 57.403 | 10.280 | 24.597 |
| cpu_query_double_hex_plus / save.milliseconds | 6.768 | 6.456 | 4.812 |
| cpu_query_double_hex_plus / encoding.milliseconds | 0.948 | 0.765 | 4.204 |
| cpu_query_double_hex_plus / operation.playlist.milliseconds | 2.661 | 0.683 | 4.641 |
| cpu_query_double_hex_plus / operation.segment.milliseconds | 53.102 | unavailable | unavailable |
| cpu_query_double_hex_plus / stop.milliseconds | 2.070 | 0.469 | 4.115 |
| cpu_query_double_hex_plus / elapsed_ms | 68.420 | 10.537 | 24.422 |
| cpu_query_interior_nul / save.milliseconds | 6.982 | 5.822 | 4.719 |
| cpu_query_interior_nul / encoding.milliseconds | 1.044 | 0.878 | 4.398 |
| cpu_query_interior_nul / operation.playlist.milliseconds | 3.211 | 0.748 | 4.815 |
| cpu_query_interior_nul / operation.segment.milliseconds | 54.864 | unavailable | unavailable |
| cpu_query_interior_nul / stop.milliseconds | 1.622 | 0.562 | 4.076 |
| cpu_query_interior_nul / elapsed_ms | 70.899 | 9.917 | 24.601 |
| cpu_query_unicode_before_nul / save.milliseconds | 7.425 | 8.408 | 4.587 |
| cpu_query_unicode_before_nul / encoding.milliseconds | 1.231 | 0.890 | 4.595 |
| cpu_query_unicode_before_nul / operation.playlist.milliseconds | 3.946 | 0.735 | 4.634 |
| cpu_query_unicode_before_nul / operation.segment.milliseconds | 54.265 | unavailable | unavailable |
| cpu_query_unicode_before_nul / stop.milliseconds | 2.019 | 0.597 | 4.175 |
| cpu_query_unicode_before_nul / elapsed_ms | 72.129 | 12.489 | 24.532 |
| cpu_query_only_nul / save.milliseconds | 7.921 | 6.428 | 4.565 |
| cpu_query_only_nul / encoding.milliseconds | 1.074 | 0.873 | 4.453 |
| cpu_query_only_nul / operation.playlist.milliseconds | 3.353 | 0.710 | 4.617 |
| cpu_query_only_nul / operation.segment.milliseconds | 50.435 | unavailable | unavailable |
| cpu_query_only_nul / stop.milliseconds | 1.724 | 0.489 | 4.369 |
| cpu_query_only_nul / elapsed_ms | 67.941 | 10.366 | 24.484 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t03/checks.json` and `/tmp/ferrofin-next10-root/evidence/t03/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.

### Configured encoder preset and CRF

Saved `EncoderPreset.auto` now reaches the encoder's actual auto arm. Previously the private helper substituted its caller's default, giving EVENT x264/x265 `superfast` and VOD SVT-AV1 preset 11. Pinned behavior uses software `veryfast`, NVENC `p1` and SVT-AV1 preset 10 for configured auto. Explicit presets and codec-specific CRF rules remain separate: values from 0 through 51 are accepted, otherwise x264 falls back to 23 and x265 to 28.

Primary source is Jellyfin `4910aafa1a`, `EncodingOptions` and `EncodingHelper.GetEncoderParam` / `GetVideoQualityParam`; web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. `EncodingOptions.EncoderPreset` is non-nullable and defaults to auto. The helper's nullable signature does not make saved null, empty, omitted and auto interchangeable. An omitted field uses auto; named configuration binding rejects explicit null or empty with a backend error, preserving the previous file.

Tests cover encoder auto arms against caller defaults, CRF boundaries, and actual persisted VOD/EVENT planner arguments. Actual-binary observations cover configured auto, omitted auto, medium, invalid CRF fallbacks, and exact null/empty HTTP responses with persisted file hashes. They collect real HLS bytes and launched FFmpeg arguments. Supporting Jellyfin 12.1.0 remains separate from the pinned source; hardware argument tests are not GPU execution certification.

Mediaencoding needs separate changed-crate coverage; server planner wiring has its compile/integration exemption and still runs regressions. The existing named enum model and prior findings are preserved.

Validation passed:

- fmt: passed (2.44 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (25.64 s, including any compilation).
- build: passed (6.43 s, including any compilation).
- native-before: passed (6.94 s, including any compilation).
- native-reference: passed (9.84 s, including any compilation).
- native-after: passed (6.69 s, including any compilation).
- clippy: passed (14.38 s, including any compilation).
- doctests: 3 passed, 0 ignored (13.35 s, including any compilation).
- ferrofin-mediaencoding: 35 tests run: 35 passed, 888 skipped (9.09 s, including any compilation).
- server: 89 tests run: 89 passed, 82 skipped (9.72 s, including any compilation).
- real-ffmpeg: 7 tests run: 7 passed, 0 skipped (8.08 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-mediaencoding: 17,173/18,868 lines (91.016536%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 9 functions have mismatched data.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 5356.061 | 5151.488 | 6353.291 |
| fixture_media_main / generation_ms | 44.031 | 47.089 | 41.301 |
| quality_0 / save.milliseconds | 7.424 | 6.607 | 14.302 |
| quality_0 / encoding.milliseconds | 2.116 | 0.944 | 6.666 |
| quality_0 / operation.playlist.milliseconds | 4.759 | 3.469 | 38.893 |
| quality_0 / operation.segment.milliseconds | 60.731 | 51.942 | 144.107 |
| quality_0 / stop.milliseconds | 3.239 | 2.052 | 9.544 |
| quality_1 / save.milliseconds | 8.030 | 6.602 | 6.420 |
| quality_1 / encoding.milliseconds | 1.291 | 1.018 | 6.209 |
| quality_1 / operation.playlist.milliseconds | 4.076 | 3.761 | 13.444 |
| quality_1 / operation.segment.milliseconds | 52.563 | 53.097 | 116.217 |
| quality_1 / stop.milliseconds | 2.064 | 2.286 | 7.594 |
| quality_2 / save.milliseconds | 7.040 | 6.553 | 7.536 |
| quality_2 / encoding.milliseconds | 1.544 | 1.185 | 8.868 |
| quality_2 / operation.playlist.milliseconds | 3.551 | 2.998 | 13.291 |
| quality_2 / operation.segment.milliseconds | 65.541 | 54.436 | 115.802 |
| quality_2 / stop.milliseconds | 2.437 | 2.261 | 7.978 |
| quality_3 / save.milliseconds | 8.389 | 6.811 | 6.773 |
| quality_3 / encoding.milliseconds | 2.040 | 1.012 | 6.582 |
| quality_3 / operation.playlist.milliseconds | 60.402 | 50.563 | 140.063 |
| quality_3 / operation.segment.milliseconds | 1.626 | 1.362 | 5.699 |
| quality_3 / stop.milliseconds | 2.175 | 1.635 | 6.476 |
| quality_4 / save.milliseconds | 7.101 | 6.306 | 6.347 |
| quality_4 / encoding.milliseconds | 1.162 | 1.386 | 6.470 |
| quality_4 / operation.playlist.milliseconds | 58.147 | 58.981 | 115.932 |
| quality_4 / operation.segment.milliseconds | 2.026 | 1.856 | 6.694 |
| quality_4 / stop.milliseconds | 2.042 | 2.207 | 6.503 |
| quality_5 / save.milliseconds | 6.277 | 7.156 | 6.315 |
| quality_5 / encoding.milliseconds | 1.188 | 0.873 | 6.083 |
| quality_5 / operation.playlist.milliseconds | 3.322 | 2.896 | 18.601 |
| quality_5 / operation.segment.milliseconds | 47.933 | 52.632 | 118.738 |
| quality_5 / stop.milliseconds | 1.995 | 2.019 | 6.888 |
| quality_6 / save.milliseconds | 6.718 | 6.527 | 6.040 |
| quality_6 / encoding.milliseconds | 1.621 | 1.095 | 5.550 |
| quality_6 / operation.playlist.milliseconds | 4.190 | 3.257 | 11.847 |
| quality_6 / operation.segment.milliseconds | 46.358 | 49.770 | 113.170 |
| quality_6 / stop.milliseconds | 1.874 | 1.832 | 6.903 |
| invalid_preset_null / save.milliseconds | 1.228 | 1.401 | 8.273 |
| invalid_preset_null / encoding.milliseconds | 0.954 | 1.061 | 5.653 |
| invalid_preset_empty / save.milliseconds | 0.957 | 1.174 | 9.536 |
| invalid_preset_empty / encoding.milliseconds | 0.719 | 1.095 | 5.573 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t04/checks.json` and `/tmp/ferrofin-next10-root/evidence/t04/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.

### Allowed HEVC and AV1 encoder selection

The planner now selects the first codec after the existing source-compatible allow-flag reorder. A second private filter previously removed software HEVC/AV1 targets, so saving `AllowHevcEncoding` or `AllowAv1Encoding` did not enable those software jobs. The corrected selection reaches the existing software or hardware encoder dispatcher.

Primary source is Jellyfin `4910aafa1a`, `AttachMediaSourceInfo`, `ShiftVideoCodecsIfNeeded` and `GetVideoEncoder`; web pin is `1e507c588f353482a00333f84e36ddb7c8fc8221`. These flags reorder a disallowed leading target when an allowed alternative exists. They preserve a sole target and an all-disallowed list, and do not prohibit direct copy. The first generic codec, including VP9, also remains first. This intentionally removes Ferrofin's private VP9 skip along with its software HEVC/AV1 filter.

Persisted planner regressions cover target order, HEVC/H265 aliases, AV1, single/all-disallowed lists, copy, and HDR master-playlist entrances. Actual-binary observations exercise six software codec selections and a generated 10-bit PQ HEVC source with copy/master behavior under saved flag changes. The recording executable passes unchanged arguments to actual FFmpeg, limits only its own CPU affinity, and retains invocation and response bytes. Supporting Jellyfin 12.1.0 remains distinct from the source pin. No hardware/GPU execution claim is made.

Production changes are confined to server planning, which has its compile/integration coverage exemption. Planner, real HTTP/native, lint and documentation gates remain required; earlier thread, preset and path settings are preserved.

The first native attempt exposed an SDR test fixture: this FFmpeg build erased transfer/primaries when generic output flags were combined with x265 VUI parameters. The corrected fixture uses explicit x265 PQ/BT.2020 metadata and verifies both raw ffprobe output and each server's HDR classification before checking playlist entrances. Failed fixture/verifier attempts remain retained.

Validation passed:

- fmt: passed (1.98 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (23.53 s, including any compilation).
- build: passed (5.64 s, including any compilation).
- native-before: passed (6.49 s, including any compilation).
- native-reference: passed (10.68 s, including any compilation).
- native-after: passed (7.29 s, including any compilation).
- clippy: passed (7.64 s, including any compilation).
- doctests: 3 passed, 0 ignored (8.90 s, including any compilation).
- server: 90 tests run: 90 passed, 82 skipped (7.10 s, including any compilation).

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 4833.669 | 4808.230 | 5372.088 |
| fixture_media_main / generation_ms | 36.827 | 37.711 | 38.394 |
| codec_0 / save.milliseconds | 21.205 | 11.036 | 16.402 |
| codec_0 / encoding.milliseconds | 1.398 | 0.869 | 8.485 |
| codec_0 / operation.playlist.milliseconds | 3.507 | 2.571 | 73.033 |
| codec_0 / operation.segment.milliseconds | 45.666 | 47.645 | 138.612 |
| codec_0 / stop.milliseconds | 1.728 | 1.946 | 9.000 |
| codec_1 / save.milliseconds | 8.282 | 7.535 | 6.249 |
| codec_1 / encoding.milliseconds | 1.459 | 0.872 | 5.792 |
| codec_1 / operation.playlist.milliseconds | 3.351 | 2.983 | 12.301 |
| codec_1 / operation.segment.milliseconds | 44.073 | 103.374 | 217.427 |
| codec_1 / stop.milliseconds | 1.672 | 6.776 | 7.053 |
| codec_2 / save.milliseconds | 7.178 | 15.928 | 6.027 |
| codec_2 / encoding.milliseconds | 0.944 | 0.878 | 5.746 |
| codec_2 / operation.playlist.milliseconds | 2.884 | 2.789 | 9.361 |
| codec_2 / operation.segment.milliseconds | 40.557 | 45.165 | 113.033 |
| codec_2 / stop.milliseconds | 1.370 | 1.714 | 7.156 |
| codec_3 / save.milliseconds | 6.174 | 7.506 | 5.956 |
| codec_3 / encoding.milliseconds | 0.772 | 0.900 | 5.550 |
| codec_3 / operation.playlist.milliseconds | 2.689 | 2.832 | 9.328 |
| codec_3 / operation.segment.milliseconds | 40.391 | 300.990 | 314.198 |
| codec_3 / stop.milliseconds | 1.261 | 7.629 | 8.208 |
| codec_4 / save.milliseconds | 7.413 | 15.742 | 6.177 |
| codec_4 / encoding.milliseconds | 0.737 | 0.759 | 6.335 |
| codec_4 / operation.playlist.milliseconds | 2.435 | 3.120 | 9.814 |
| codec_4 / operation.segment.milliseconds | 41.967 | 106.075 | 212.914 |
| codec_4 / stop.milliseconds | 2.802 | 6.840 | 6.710 |
| codec_5 / save.milliseconds | 50.255 | 82.680 | 5.867 |
| codec_5 / encoding.milliseconds | 1.618 | 1.403 | 5.643 |
| codec_5 / operation.playlist.milliseconds | 3.510 | 3.601 | 9.533 |
| codec_5 / operation.segment.milliseconds | 42.836 | 292.270 | 313.785 |
| codec_5 / stop.milliseconds | 1.726 | 6.613 | 12.232 |
| fixture_media_hdr-hevc / generation_ms | 80.818 | 79.152 | 73.457 |
| hdr_hevc_false / save.milliseconds | 7.589 | 7.795 | 4.306 |
| hdr_hevc_false / encoding.milliseconds | 0.804 | 1.282 | 3.874 |
| hdr_hevc_false / master.response.milliseconds | 3.265 | 4.459 | 19.017 |
| hdr_hevc_false / operation.playlist.milliseconds | 2.603 | 2.772 | 29.028 |
| hdr_hevc_false / operation.segment.milliseconds | 36.727 | 36.683 | 109.382 |
| hdr_hevc_false / stop.milliseconds | 1.089 | 1.051 | 4.383 |
| hdr_hevc_true / save.milliseconds | 6.934 | 6.190 | 4.256 |
| hdr_hevc_true / encoding.milliseconds | 0.852 | 0.870 | 3.566 |
| hdr_hevc_true / master.response.milliseconds | 3.115 | 3.414 | 6.027 |
| hdr_hevc_true / operation.playlist.milliseconds | 2.261 | 2.641 | 9.022 |
| hdr_hevc_true / operation.segment.milliseconds | 34.236 | 34.893 | 107.055 |
| hdr_hevc_true / stop.milliseconds | 1.028 | 1.176 | 4.333 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t05/checks.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.

### Live transcode throttling

`EnableThrottling` and `ThrottleDelaySeconds` now control eligible live encoder jobs. A five-second controller reads the saved settings on each tick, clamps delay to 60 seconds, and resumes an already paused encoder when disabled. Enrollment follows startup and checks File protocol, known runtime of at least five minutes, video input, the separate encoding-state VideoFile default, and actual keyboard capability. Successful writes alone change pause state; failures are logged and retried. Teardown cancels scheduling and resumes before graceful quit under the same controller lock.

Primary behavior is pinned to Jellyfin `4910aafa1a`: TranscodingThrottler, TranscodeManager, EncodingHelper, JobLogger, DynamicHlsController and TranscodingJob. The supporting runtime is Jellyfin 12.1.0 and remains separate from that source pin. Real interactive help selects p/u; otherwise exact .NET version ordering permits legacy c/newline through version 6.1, excluding 6.1.0. The startup help probe has a bounded 15-second timeout, an explicit operational qualification relative to upstream's unbounded wait.

Actual CR/LF stderr statistics supply encoded position, seek offset, percentage, fps, size and bitrate. HLS tracks the monotonic maximum of client RuntimeTicks plus ActualSegmentLengthTicks when the response completes or is dropped, including aborted transfers. Progressive responses count bytes actually read and use encoded-byte/file-size fallback. Captured response observers retain the producing job across seek replacement. Session pings are not download progress. The API keeps its model/traits dependency boundary and SQL ceilings remain unchanged.

The necessary progressive repair routes ordinary Static=false requests through a genuine single growing output file; these requests previously served the existing source file or fell back to an HLS playlist. Static=true retains source serving. Video/HLS default Context to Streaming; ordinary audio defaults to Static. The registered Jellyfin nullable-enum binder leaves invalid text unset, accepts undefined Int32 values, and combines comma-separated names/numbers with bitwise OR. A retained actual .NET10 EnumConverter oracle covers limits, whitespace and trailing NUL behavior. Fragmented MP4 flags require both MP4 and Streaming context. HEAD resolves the format without starting an encoder. GET has no fixed length or range processing, waits for appended bytes while the producing encoder lives, and ends after it exits.

Native identifier limits prevent Container/codec values from escaping the captured output root. Container inference follows shifted target codecs before copy selection; audio inference uses the request URL suffix. Positive progressive seek clamps to runtime minus five seconds while progress retains the requested offset. Effective EncoderPath/EncoderProtocol require both fields. Ordinary native-rate input uses -re except RTSP and adds catchup on FFmpeg 8+. Earlier live roots, nullable CPU binding, preset and HEVC/AV1 selection remain intact. Advanced sample-rate/VBR/subtitle-embed/CopyTimestamps binding remains open review scope; segment deletion and mux queue settings belong to subsequent findings.

Regression checks cover policy boundaries, actual progress parsing, live configuration changes, failed writes, teardown ordering, replaced-job response ownership, dropped/error responses, growing files, nullable Context, permissions and container inference. Real FFmpeg integration checks stderr, interactive help and graceful exit. Native HTTP validation separately labels stock FFmpeg capability and a controlled wrapper: the latter delegates real encoding/statistics to FFmpeg, inserts fixture-only readrate 10 and a 120-second initial read burst and implements observed p/u with SIGSTOP/SIGCONT. This verifies controller transport and actual paused/running output; it does not certify stock FFmpeg p/u or hardware acceleration. The aborted-HLS probe repeats actual TS packets into a bounded response solely to exceed socket buffering; those bytes never supply encoder statistics.

The first API gate exposed scalar Container metadata on universal audio and an anonymous fixture using an always-authenticated stub. Route-specific collection metadata now preserves ordinary scalar binding and the existing universal fallback; the anonymous fixture supplies genuinely unauthenticated authorization. Universal-audio container negotiation remains an open behavior gap and is not completed by this throttling finding. The day-qualified progress parser correction also preserves its existing regression. Failed attempts are retained.

The initial prepared enum binder incorrectly assumed default ASP.NET rejection rules. Native reference Context=2 returned 200; pinned NullableEnumModelBinder and its provider registration instead use EnumConverter and catch FormatException, leaving invalid text unset. Production binding and a committed actual-oracle fixture now follow that source. Framework conversion source: [EnumConverter at .NET10](https://github.com/dotnet/runtime/blob/v10.0.0/src/libraries/System.ComponentModel.TypeConverter/src/System/ComponentModel/EnumConverter.cs).

The initial aborted-response fixture omitted the stream User-Agent, which the pinned output-path hash includes, and therefore triggered a reference seek/replacement. Its apparent resume was rejected by the independent exact producing-job count. Normal and aborted requests now carry the same explicit User-Agent, with the raw aborted request retained. The cached segment receives a deliberately advanced client marker of RuntimeTicks=600 seconds plus ActualSegmentLengthTicks=6 seconds; this tests callback ownership and cancellation, not actual playback of 606 seconds. Real output/progress and exact child identities remain independently verified.

Validation passed:

- fmt: passed (2.36 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (39.71 s, including any compilation).
- build: passed (2.63 s, including any compilation).
- native-before: passed (84.97 s, including any compilation).
- native-reference: passed (133.16 s, including any compilation).
- native-after: passed (126.09 s, including any compilation).
- clippy: passed (1.61 s, including any compilation).
- doctests: 3 passed, 0 ignored (6.57 s, including any compilation).
- ferrofin-mediaencoding: 650 tests run: 650 passed, 288 skipped (17.34 s, including any compilation).
- ferrofin-hls: 32 tests run: 32 passed, 85 skipped (2.39 s, including any compilation).
- ferrofin-api: 61 tests run: 61 passed, 866 skipped (69.39 s, including any compilation).
- ferrofin-model: 16 tests run: 16 passed, 965 skipped (30.42 s, including any compilation).
- server: 128 tests run: 128 passed, 51 skipped (7.30 s, including any compilation).
- traits: 76 tests run: 76 passed, 0 skipped (11.03 s, including any compilation).
- real-segment_transcode_ffmpeg: 8 tests run: 8 passed, 0 skipped (8.90 s, including any compilation).
- real-hls_stream_manager_ffmpeg: 4 tests run: 4 passed, 0 skipped (1.43 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-mediaencoding: 19,032/20,144 lines (94.479746%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 9 functions have mismatched data.
- ferrofin-hls: 3,066/3,300 lines (92.909091%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 1 functions have mismatched data.
- ferrofin-api: 42,531/49,204 lines (86.438094%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 53 functions have mismatched data.
- ferrofin-model: 8,954/9,921 lines (90.252999%), fresh full suite without seed.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| stock / hls / context_binding / Videos / Context='' / response_ms | 2.037 | 2.390 | 36.131 |
| stock / hls / context_binding / Videos / Context=' \t' / response_ms | 2.462 | 2.239 | 9.574 |
| stock / hls / context_binding / Videos / Context='stReaMing' / response_ms | 2.424 | 2.190 | 7.620 |
| stock / hls / context_binding / Videos / Context='STATIC' / response_ms | 1.635 | 1.929 | 7.047 |
| stock / hls / context_binding / Videos / Context='0' / response_ms | 1.648 | 1.700 | 6.949 |
| stock / hls / context_binding / Videos / Context='1' / response_ms | 1.716 | 1.769 | 6.965 |
| stock / hls / context_binding / Videos / Context='2' / response_ms | 1.644 | 1.714 | 6.762 |
| stock / hls / context_binding / Videos / Context='other' / response_ms | 1.487 | 1.679 | 6.879 |
| stock / hls / context_binding / Audio / Context='' / response_ms | 1.396 | 1.979 | 15.848 |
| stock / hls / context_binding / Audio / Context=' \t' / response_ms | 1.311 | 1.957 | 8.116 |
| stock / hls / context_binding / Audio / Context='stReaMing' / response_ms | 1.181 | 1.845 | 7.731 |
| stock / hls / context_binding / Audio / Context='STATIC' / response_ms | 1.236 | 1.941 | 6.890 |
| stock / hls / context_binding / Audio / Context='0' / response_ms | 1.071 | 2.007 | 6.956 |
| stock / hls / context_binding / Audio / Context='1' / response_ms | 1.318 | 1.830 | 7.235 |
| stock / hls / context_binding / Audio / Context='2' / response_ms | 1.262 | 1.792 | 6.882 |
| stock / hls / context_binding / Audio / Context='other' / response_ms | 1.623 | 1.770 | 7.266 |
| stock / hls / first_response / response_ms | 92.381 | 86.209 | 238.124 |
| stock / progressive / context_binding / Videos / Context='' / response_ms | 4.121 | 2.828 | 34.177 |
| stock / progressive / context_binding / Videos / Context=' \t' / response_ms | 4.729 | 2.678 | 9.079 |
| stock / progressive / context_binding / Videos / Context='stReaMing' / response_ms | 4.148 | 2.037 | 7.279 |
| stock / progressive / context_binding / Videos / Context='STATIC' / response_ms | 4.257 | 2.309 | 6.931 |
| stock / progressive / context_binding / Videos / Context='0' / response_ms | 2.804 | 1.982 | 6.718 |
| stock / progressive / context_binding / Videos / Context='1' / response_ms | 2.272 | 2.028 | 6.641 |
| stock / progressive / context_binding / Videos / Context='2' / response_ms | 2.016 | 1.891 | 6.469 |
| stock / progressive / context_binding / Videos / Context='other' / response_ms | 2.157 | 1.982 | 6.584 |
| stock / progressive / context_binding / Audio / Context='' / response_ms | 1.676 | 1.773 | 14.875 |
| stock / progressive / context_binding / Audio / Context=' \t' / response_ms | 1.393 | 1.652 | 7.717 |
| stock / progressive / context_binding / Audio / Context='stReaMing' / response_ms | 1.356 | 1.649 | 7.444 |
| stock / progressive / context_binding / Audio / Context='STATIC' / response_ms | 1.364 | 1.808 | 6.650 |
| stock / progressive / context_binding / Audio / Context='0' / response_ms | 1.647 | 1.749 | 7.077 |
| stock / progressive / context_binding / Audio / Context='1' / response_ms | 1.836 | 1.859 | 7.358 |
| stock / progressive / context_binding / Audio / Context='2' / response_ms | 1.286 | 1.725 | 6.735 |
| stock / progressive / context_binding / Audio / Context='other' / response_ms | 1.287 | 1.767 | 6.887 |
| stock / progressive / first_response / response_ms | 1.343 | 1028.297 | 1124.476 |
| patched-wrapper / hls / context_binding / Videos / Context='' / response_ms | 2.429 | 3.291 | 39.632 |
| patched-wrapper / hls / context_binding / Videos / Context=' \t' / response_ms | 2.995 | 2.591 | 13.488 |
| patched-wrapper / hls / context_binding / Videos / Context='stReaMing' / response_ms | 2.533 | 2.265 | 11.493 |
| patched-wrapper / hls / context_binding / Videos / Context='STATIC' / response_ms | 1.702 | 2.781 | 8.744 |
| patched-wrapper / hls / context_binding / Videos / Context='0' / response_ms | 1.526 | 2.223 | 7.974 |
| patched-wrapper / hls / context_binding / Videos / Context='1' / response_ms | 1.408 | 2.058 | 7.769 |
| patched-wrapper / hls / context_binding / Videos / Context='2' / response_ms | 1.673 | 1.879 | 7.696 |
| patched-wrapper / hls / context_binding / Videos / Context='other' / response_ms | 1.664 | 2.029 | 7.675 |
| patched-wrapper / hls / context_binding / Audio / Context='' / response_ms | 1.935 | 2.312 | 17.684 |
| patched-wrapper / hls / context_binding / Audio / Context=' \t' / response_ms | 2.109 | 2.227 | 9.121 |
| patched-wrapper / hls / context_binding / Audio / Context='stReaMing' / response_ms | 2.234 | 1.945 | 8.065 |
| patched-wrapper / hls / context_binding / Audio / Context='STATIC' / response_ms | 2.015 | 2.131 | 7.700 |
| patched-wrapper / hls / context_binding / Audio / Context='0' / response_ms | 2.134 | 2.133 | 7.553 |
| patched-wrapper / hls / context_binding / Audio / Context='1' / response_ms | 2.055 | 1.834 | 7.314 |
| patched-wrapper / hls / context_binding / Audio / Context='2' / response_ms | 2.217 | 1.963 | 8.077 |
| patched-wrapper / hls / context_binding / Audio / Context='other' / response_ms | 2.233 | 1.866 | 7.694 |
| patched-wrapper / hls / first_response / response_ms | 121.524 | 106.870 | 238.321 |
| patched-wrapper / progressive / context_binding / Videos / Context='' / response_ms | 2.481 | 2.595 | 33.626 |
| patched-wrapper / progressive / context_binding / Videos / Context=' \t' / response_ms | 2.392 | 2.373 | 9.255 |
| patched-wrapper / progressive / context_binding / Videos / Context='stReaMing' / response_ms | 7.909 | 2.370 | 7.165 |
| patched-wrapper / progressive / context_binding / Videos / Context='STATIC' / response_ms | 3.905 | 1.813 | 6.547 |
| patched-wrapper / progressive / context_binding / Videos / Context='0' / response_ms | 3.794 | 1.867 | 6.442 |
| patched-wrapper / progressive / context_binding / Videos / Context='1' / response_ms | 3.137 | 1.710 | 6.430 |
| patched-wrapper / progressive / context_binding / Videos / Context='2' / response_ms | 2.317 | 1.994 | 6.309 |
| patched-wrapper / progressive / context_binding / Videos / Context='other' / response_ms | 3.402 | 1.902 | 6.487 |
| patched-wrapper / progressive / context_binding / Audio / Context='' / response_ms | 2.661 | 1.858 | 14.294 |
| patched-wrapper / progressive / context_binding / Audio / Context=' \t' / response_ms | 2.671 | 1.769 | 7.333 |
| patched-wrapper / progressive / context_binding / Audio / Context='stReaMing' / response_ms | 2.059 | 1.703 | 7.121 |
| patched-wrapper / progressive / context_binding / Audio / Context='STATIC' / response_ms | 1.598 | 1.716 | 6.604 |
| patched-wrapper / progressive / context_binding / Audio / Context='0' / response_ms | 1.408 | 1.794 | 6.346 |
| patched-wrapper / progressive / context_binding / Audio / Context='1' / response_ms | 1.683 | 1.745 | 6.352 |
| patched-wrapper / progressive / context_binding / Audio / Context='2' / response_ms | 1.469 | 2.137 | 6.321 |
| patched-wrapper / progressive / context_binding / Audio / Context='other' / response_ms | 1.380 | 1.989 | 6.506 |
| patched-wrapper / progressive / first_response / response_ms | 1.925 | 1046.089 | 1122.685 |
| patched-wrapper / hls / enable_clamped_delay / wait_ms | unavailable | 4112.218 | 3909.406 |
| patched-wrapper / hls / raise_delay_live / wait_ms | unavailable | 4213.165 | 4212.662 |
| patched-wrapper / hls / reduce_delay_live / wait_ms | unavailable | 3009.516 | 3006.055 |
| patched-wrapper / hls / disable_live / wait_ms | unavailable | 5016.304 | 5016.785 |
| patched-wrapper / hls / reenable_live / wait_ms | unavailable | 4916.674 | 4910.297 |
| patched-wrapper / hls / stop_encoding / stop_ms | unavailable | 1763.046 | 1765.297 |
| patched-wrapper / hls / cancellation_job_first_response / response_ms | unavailable | 107.038 | 212.663 |
| patched-wrapper / hls / cancellation_job_pause / wait_ms | unavailable | 5211.626 | 9937.786 |
| patched-wrapper / hls / aborted_hls_callback / wait_ms | unavailable | 5018.508 | 5017.369 |
| patched-wrapper / progressive / enable_clamped_delay / wait_ms | unavailable | 4112.155 | 4012.459 |
| patched-wrapper / progressive / raise_delay_live / wait_ms | unavailable | 4313.496 | 4211.446 |
| patched-wrapper / progressive / reduce_delay_live / wait_ms | unavailable | 3010.209 | 3009.310 |
| patched-wrapper / progressive / disable_live / wait_ms | unavailable | 4919.942 | 5015.697 |
| patched-wrapper / progressive / reenable_live / wait_ms | unavailable | 5019.316 | 4916.656 |
| patched-wrapper / progressive / stop_encoding / stop_ms | unavailable | 3800.394 | 1721.489 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t06/checks.json` and `/tmp/ferrofin-next10-root/evidence/t06/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.


Final complete workspace matrix: **8,393 passed, 5 skipped, 1 slow**, across all 21 workspace packages and 201 default test targets. Every package has a passing run with real FFmpeg enabled; WASM guest tests were explicitly disabled. Generated FFmpeg logs were preserved byte-for-byte, and only generated test executables were cleaned between packages. Full commands, target enumeration and frozen source evidence: `/tmp/ferrofin-next10-root/evidence/final88-matrix/run-002`. The previous passing formatting, strict all-target/all-feature Clippy and three-doctest checks match all 896 current Rust/Cargo source hashes; proof: `/tmp/ferrofin-next10-root/final88-quality-source-proof-v2.json`. The actual nullable Context oracle fixture is independently hashed in both this proof and the complete matrix.

The initial full server suite caught two pre-existing direct-play fixtures that omitted `Static=true` while expecting original-file bytes/ranges. Pinned `VideosController` only serves that static path when the flag is true; otherwise it starts progressive transcoding. Both fixture requests now explicitly select static serving. Their status/range/body assertions remain unchanged. All 227 server tests were rerun, while the 20 successful packages were retained with proof that only these server integration fixture files changed. The failed run remains in `run-001`.

### Rolling HLS segment retention

`EnableSegmentDeletion` and `SegmentKeepSeconds` now control rolling deletion of
HLS segments behind the finalized download position while ffmpeg remains active. This is separate
from the existing end-of-job cleanup. The primary oracle is Jellyfin
`4910aafa1a`, captured in the adjacent `oracle/` manifest and source snapshots.

`TranscodeManager.EnableSegmentCleaning` (lines 570–575) enrolls File or HTTP
inputs, video inputs, HLS output, and known runtime of at least five minutes.
Short video, actual audio inputs, progressive and DASH output, other protocols,
and live sources without a known runtime do not enroll. `IsInputVideo` follows
the input item's media type independently of output selection: audio-only HLS
requested from a qualifying video item can enroll while its ffmpeg stays active.
The separate copy pacing branch still requires a selected video stream. Effective input protocol uses
a nonempty `EncoderPath` together with `EncoderProtocol` when both exist,
otherwise the source's ordinary `Protocol`, as `EncodingHelper.AttachMediaSourceInfo`
does. The cleaner starts only after ffmpeg's first-output wait and only while
the child remains active. It exists even if the enable switch initially is
false, because every callback reads the live encoding settings.

The first and subsequent ticks are 20 seconds apart. Retention is floored at
20 seconds. Position comes from finalized dynamic-HLS response lifetimes, including
aborted responses, not playback-position reports or the furthest produced
segment. `DynamicHlsController` registers `Response.OnCompleted` with the
request's supplied `RuntimeTicks + ActualSegmentLengthTicks`; Kestrel fires that
completion callback when an aborted response lifetime finalizes as well, as
[ASP.NET Core 10 request finalization](https://github.com/dotnet/aspnetcore/blob/v10.0.0/src/Servers/Kestrel/Core/src/Internal/Http/HttpProtocol.cs#L703-L714)
shows. The source
converts ticks to seconds with midpoint ties to even, then computes integer
`(downloadSeconds - max(keepSeconds,20)) / segmentLength`. A positive result
selects inclusive indices zero through that maximum and waits 1.5 seconds
before removing them. A playlist, fMP4 init segment index -1, in-progress
`.tmp` segment and future segment remain. The source selector removes all
case-sensitive occurrences of the playlist stem from each basename and parses
an Int64 index; it does not limit the segment extension. One removal failure
does not skip other selected files. Filesystem/config read failures log at
debug level and do not terminate streaming.

Blocking directory enumeration and removal run in a single blocking batch per
job, so retention does not occupy the HTTP executor or create overlapping
batches when storage is slow. The owned cleaner task ends when it observes ffmpeg's exit. Killing or removing
a job drops and aborts its task, including an in-progress deletion delay. An atomic stop flag also ends a
removal batch between files; one filesystem operation already in progress may
finish, but a removed job schedules no later removals. This explicitly cancelable task
is a Rust lifecycle adaptation; the C# timer disposes future callbacks on stop
but an already dispatched callback may finish its delay. Job teardown keeps its
existing separate `delete_files` behavior.

`EncodingHelper.GetInputModifier` also slows copied video HLS input to
`-readrate 10` when segment deletion is enabled and ffmpeg is at least version
5.0. This command policy is independent of cleaner eligibility. Native-rate
sources instead use `-re`, except RTSP, with precedence over the copy pacing.
At the pinned source's ffmpeg version 8.0 threshold, the corresponding
`-readrate_catchup` is 100 for native rate and 1000 for copy pacing; earlier
versions must not receive this option. Native-rate jobs with deletion disabled
still retain their ordinary `-re` behavior.

Prepared tests cover enrollment boundaries and effective protocol override,
20-second floor, signed/extreme retention and download values, midpoint
rounding and the inclusive deletion boundary, filenames and init/partial files,
continued deletion after a controlled failure, version gates and pacing
precedence, live setting updates, disabled retention, task drop, a canceled pending deletion delay, stopped removal batches and exited
process stop. Tests using a short injected timer exercise the worker lifecycle;
they do not claim to measure the production 20-second interval.

Real verification must run the binary and real ffmpeg with a video at least
five minutes long. Use an inexpensive low-resolution, low-frame-rate clip.
Start a copied video HLS job with deletion enabled to observe `-readrate 10`.
Consume completed segments far enough ahead of a 20-second retained window,
keep the job alive, and record the exact on-disk old/future/init/playlist files
before and after the first production 20-second tick plus 1.5-second delay.
Use a longer clip or native-rate input to keep ffmpeg active throughout. Also
start disabled deletion and short/actual-audio-input/control jobs, change retention and the
enable switch while an eligible job is active, and verify that canceled response finalization advances the supplied dynamic
segment ending position, while canceled progressive reads only count bytes
actually consumed, matching the pinned callback behavior. Stop the actual job and
confirm no further rolling task acts; end-of-job file cleanup is assessed
separately. Native Jellyfin binary evidence is supporting evidence if its
release differs from the primary source pin, and must be labeled accordingly.

The prepared v3 owned HTTP fixture uses a tiny 900-second, 64x64, 1fps clip
and the root's recording encoder without adding pacing. Its separate verifier
checks real copy argv/readrate placement, 13 actual segment responses and
authoritative supplied ending ticks, recorded session pings that keep the 60-second
idle timer from ending the job, live enable and 20/30-second retention
changes across production timer ticks, exact recorded live process argv, and a
post-stop sentinel after 22 seconds. An early elapsed-time guard rejects a
fixture that cannot disable the initial setting before its first tick. The corrected
fixture disables immediately after the first real segment response, before the
remaining twelve reads. VOD playlist delivery alone does not start the reference
encoder; moving the save before that first segment suppressed startup copy pacing
and was independently rejected by the argv assertion. Both failed fixtures are
preserved; the initial draft disabled afterward and its shared-host reference run
took 19.33 seconds, failing the 18-second guard. The exact early save/readback
and timing are now retained. Production timer, deletion and process assertions
remain unchanged; failed evidence is preserved. This is
a prepared verification plan; it has not been executed.

The native draft also assumed a running VOD encoder had written its disk playlist.
FFmpeg writes it at completion; the HTTP playlist and real TS segments are already
available. The fixture now derives the output stem from exact recorded argv and
asserts the expected absent live VOD playlist while retaining all TS, boundary,
process and cleanup proofs. The successful baseline was reverified against this
corrected verifier without rerunning its unchanged observation sequence.

The final sentinel uses numeric basename `0.ts`: the rolling source selector
accepts that index, while ordinary end-of-job cleanup selects paths containing
the job stem. The initial stem-prefixed sentinel was removed by reference final
cleanup, correctly kept separate from rolling deletion. All live retention
phases passed in that preserved attempt; the isolated teardown probe is rerun
with the reference and updated server.

Validation passed:

- fmt: passed (2.18 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (25.84 s, including any compilation).
- build: passed (7.82 s, including any compilation).
- native-before: passed (99.54 s, including any compilation).
- native-reference: passed (100.40 s, including any compilation).
- native-after: passed (100.72 s, including any compilation).
- clippy: passed (13.87 s, including any compilation).
- doctests: 3 passed, 0 ignored (11.97 s, including any compilation).
- ferrofin-mediaencoding: 59 tests run: 59 passed, 890 skipped (11.12 s, including any compilation).
- server: 98 tests run: 98 passed, 82 skipped (10.22 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-mediaencoding: 19,187/21,036 lines (91.210306%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 9 functions have mismatched data.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 6473.783 | 6488.588 | 5566.607 |
| fixture_media_retention / generation_ms | 67.377 | 55.404 | 53.151 |
| segment_reads / playlist.milliseconds | 6.792 | 3.275 | 64.414 |
| segment_reads / early_disable_save.milliseconds | 9.852 | 7.720 | 14.782 |
| segment_reads / early_disable_encoding.milliseconds | 1.427 | 1.169 | 4.284 |
| disabled_live_tick / save.milliseconds | 7.913 | 9.562 | 4.733 |
| disabled_live_tick / encoding.milliseconds | 2.281 | 3.041 | 4.999 |
| enabled_keep30_live_tick / save.milliseconds | 7.163 | 11.478 | 4.190 |
| enabled_keep30_live_tick / encoding.milliseconds | 2.804 | 2.154 | 7.476 |
| changed_keep20_live_tick / save.milliseconds | 7.976 | 9.756 | 4.784 |
| changed_keep20_live_tick / encoding.milliseconds | 2.408 | 2.024 | 4.649 |
| stopped_timer / stop.milliseconds | 7.198 | 1231.942 | 460.173 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t07/checks.json` and `/tmp/ferrofin-next10-root/evidence/t07/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.

### Maximum HLS muxing queue

`MaxMuxingQueueSize` now reaches each newly started HLS ffmpeg command as the
output option `-max_muxing_queue_size`. The saved signed integer is floored at
128, matching `DynamicHlsController.GetCommandLineArguments` in Jellyfin
`4910aafa1a` lines 1627–1650. The model's default remains 2048. No upper clamp or
rejection replaces the upstream value; negative, zero and values through 128
all select 128, while 129 and larger pass through exactly.

The option applies to video and audio-only HLS, copied and re-encoded streams,
VOD and EVENT playlists, and MPEG-TS and fMP4 segments. It appears once after
input arguments and before `-f hls`. Running processes retain the command with
which they started. A subsequent configuration save affects newly started
processes, as it does upstream.

The prepared unit test builds 64 actual plans crossing eight signed boundary
values, two playlist kinds, two containers and audio/video. It checks the
exact value, one flag occurrence, and output-option placement. Existing planner
tests cover copied and encoded stream paths. The real HTTP verification must
save encoding settings, start distinct jobs through the HLS routes, consume
nonempty segments, and inspect the real process/log argument vector at default
2048, configured 1024/4096, the signed/extreme boundary values, and floor cases 0 and 128. Use a fresh playback-session
and query identity per job, or explicitly stop the previous job, so an existing
transcode does not mask a newly saved command setting.

The prepared actual-runtime fixture uses the root's recording encoder to capture
real ffmpeg argv while executing ffmpeg, plus actual save/readback/stop HTTP
responses and consumed segment bytes. Thirteen distinct jobs cover the default,
eight signed boundaries, TS/fMP4, VOD/EVENT, and copied/re-encoded video. Its
separate verifier requires the exact argument value and placement for reference
and after runs, and the exact missing argument for the before run. This remains
prepared evidence until root runs it.

The primary source has no reference to this setting in progressive output
commands. [Live transcode throttling](#live-transcode-throttling)'s prepared implementation now provides a separate progressive
plan and single-file muxer, returning before the HLS muxer; this finding therefore also
prepares four actual progressive audio/video plans with changed queue values
and asserts that neither the queue option nor HLS segment options appear.
This finding depends on the [Live transcode throttling](#live-transcode-throttling) progressive implementation passing its own
validation before these exclusions can be claimed as implemented behavior.

The initial native verifier reused a TS-only packet check for fMP4 responses.
It now checks TS packet framing for TS and complete ISO BMFF box boundaries with
real `moof`/`mdat` boxes for fMP4; response bytes, argv and queue assertions are
retained. The failed baseline attempt is preserved.

Validation passed:

- fmt: passed (2.31 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (26.45 s, including any compilation).
- build: passed (2.06 s, including any compilation).
- native-before: passed (8.50 s, including any compilation).
- native-reference: passed (10.08 s, including any compilation).
- native-after: passed (8.28 s, including any compilation).
- clippy: passed (9.07 s, including any compilation).
- doctests: 3 passed, 0 ignored (10.59 s, including any compilation).
- server: 100 tests run: 100 passed, 82 skipped (8.77 s, including any compilation).

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 6507.930 | 6477.750 | 5906.765 |
| fixture_media_main / generation_ms | 46.562 | 45.767 | 50.119 |
| default / save.milliseconds | 10.362 | 6.495 | 12.871 |
| default / encoding.milliseconds | 1.878 | 0.743 | 5.386 |
| default / operation.playlist.milliseconds | 4.242 | 3.595 | 39.512 |
| default / operation.segment.milliseconds | 51.781 | 56.147 | 134.991 |
| default / stop.milliseconds | 8.398 | 10.419 | 7.292 |
| boundary_0 / save.milliseconds | 7.484 | 10.841 | 4.882 |
| boundary_0 / encoding.milliseconds | 2.151 | 2.763 | 4.382 |
| boundary_0 / operation.playlist.milliseconds | 7.178 | 12.482 | 10.093 |
| boundary_0 / operation.segment.milliseconds | 52.813 | 55.571 | 117.718 |
| boundary_0 / stop.milliseconds | 9.007 | 8.972 | 5.923 |
| boundary_1 / save.milliseconds | 7.748 | 7.043 | 5.026 |
| boundary_1 / encoding.milliseconds | 1.129 | 0.795 | 4.986 |
| boundary_1 / operation.playlist.milliseconds | 56.634 | 46.926 | 121.534 |
| boundary_1 / operation.segment.milliseconds | 1.526 | 1.461 | 5.126 |
| boundary_1 / stop.milliseconds | 7.545 | 8.402 | 5.070 |
| boundary_2 / save.milliseconds | 7.141 | 7.033 | 4.758 |
| boundary_2 / encoding.milliseconds | 1.207 | 0.991 | 5.349 |
| boundary_2 / operation.playlist.milliseconds | 3.566 | 2.976 | 9.352 |
| boundary_2 / operation.segment.milliseconds | 51.920 | 49.117 | 113.115 |
| boundary_2 / stop.milliseconds | 12.230 | 8.803 | 5.761 |
| boundary_3 / save.milliseconds | 7.597 | 6.613 | 5.850 |
| boundary_3 / encoding.milliseconds | 1.537 | 0.680 | 5.029 |
| boundary_3 / operation.playlist.milliseconds | 47.199 | 44.814 | 110.679 |
| boundary_3 / operation.segment.milliseconds | 1.833 | 0.965 | 4.349 |
| boundary_3 / stop.milliseconds | 7.316 | 8.575 | 5.167 |
| boundary_4 / save.milliseconds | 9.286 | 6.083 | 4.885 |
| boundary_4 / encoding.milliseconds | 2.436 | 0.856 | 4.713 |
| boundary_4 / operation.playlist.milliseconds | 6.499 | 2.684 | 17.285 |
| boundary_4 / operation.segment.milliseconds | 50.952 | 44.947 | 112.798 |
| boundary_4 / stop.milliseconds | 8.754 | 8.869 | 9.146 |
| boundary_5 / save.milliseconds | 7.910 | 10.265 | 9.808 |
| boundary_5 / encoding.milliseconds | 1.264 | 0.824 | 6.885 |
| boundary_5 / operation.playlist.milliseconds | 53.728 | 52.199 | 114.926 |
| boundary_5 / operation.segment.milliseconds | 1.568 | 1.683 | 4.008 |
| boundary_5 / stop.milliseconds | 7.865 | 7.610 | 5.594 |
| boundary_6 / save.milliseconds | 10.751 | 6.140 | 4.863 |
| boundary_6 / encoding.milliseconds | 1.025 | 0.809 | 4.461 |
| boundary_6 / operation.playlist.milliseconds | 3.275 | 2.945 | 7.194 |
| boundary_6 / operation.segment.milliseconds | 49.914 | 58.081 | 110.983 |
| boundary_6 / stop.milliseconds | 9.384 | 8.849 | 5.894 |
| boundary_7 / save.milliseconds | 9.140 | 6.578 | 4.872 |
| boundary_7 / encoding.milliseconds | 2.513 | 1.070 | 5.506 |
| boundary_7 / operation.playlist.milliseconds | 49.178 | 50.073 | 110.751 |
| boundary_7 / operation.segment.milliseconds | 1.818 | 2.182 | 2.760 |
| boundary_7 / stop.milliseconds | 8.554 | 12.268 | 4.381 |
| fragmented_floor / save.milliseconds | 8.470 | 6.826 | 4.815 |
| fragmented_floor / encoding.milliseconds | 2.051 | 1.361 | 5.650 |
| fragmented_floor / operation.playlist.milliseconds | 4.935 | 3.643 | 7.731 |
| fragmented_floor / operation.segment.milliseconds | 49.618 | 47.156 | 110.650 |
| fragmented_floor / stop.milliseconds | 8.566 | 8.501 | 5.160 |
| fragmented_configured / save.milliseconds | 7.846 | 6.501 | 5.145 |
| fragmented_configured / encoding.milliseconds | 1.405 | 1.087 | 4.620 |
| fragmented_configured / operation.playlist.milliseconds | 59.564 | 52.137 | 122.152 |
| fragmented_configured / operation.segment.milliseconds | 1.492 | 1.640 | 3.555 |
| fragmented_configured / stop.milliseconds | 7.959 | 7.571 | 4.604 |
| copy_transport / save.milliseconds | 6.636 | 6.759 | 4.735 |
| copy_transport / encoding.milliseconds | 1.101 | 0.701 | 4.217 |
| copy_transport / operation.playlist.milliseconds | 3.718 | 2.692 | 27.909 |
| copy_transport / operation.segment.milliseconds | 43.921 | 49.897 | 113.732 |
| copy_transport / stop.milliseconds | 1.346 | 1.326 | 5.413 |
| copy_fragmented / save.milliseconds | 7.330 | 6.224 | 4.969 |
| copy_fragmented / encoding.milliseconds | 0.851 | 0.729 | 4.451 |
| copy_fragmented / operation.playlist.milliseconds | 43.051 | 39.257 | 109.601 |
| copy_fragmented / operation.segment.milliseconds | 2.835 | 1.356 | 4.155 |
| copy_fragmented / stop.milliseconds | 2.264 | 0.955 | 5.124 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t08/checks.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.

Final complete workspace matrix: **8,407 passed, 5 skipped, 1 slow** across all 21 packages and 201 default test targets. Mediaencoding, HLS and server were rerun with real FFmpeg enabled. Eighteen independent packages retain their previous passing results, with all source inputs and every workspace dependency kind checked to exclude either changed package from their dependency closure. WASM guest skips remain qualified. Full commands, target enumeration and reuse proof: `/tmp/ferrofin-next10-root/evidence/final90-matrix/run-001`. Fresh formatting, strict workspace Clippy, three doctests and production build match all 895 current Rust/Cargo input hashes: `/tmp/ferrofin-next10-root/final90-quality-source-proof.json`.

## Networking

### The listener follows the network settings

The dashboard's **Local IP address** (`LocalNetworkAddresses`) and **Enable
IPv4 / IPv6** (`EnableIPv4`, `EnableIPv6`) settings changed which interfaces
Ferrofin advertised and which clients counted as local, but the HTTP listener
always bound the single startup address (`0.0.0.0` by default). Restricting
the server to one interface, or turning a family off, did nothing to what it
accepted.

Source oracle: Jellyfin `4910aafa1a`, `WebHostBuilderExtensions.
SetupJellyfinWebServer` (one `Listen` per address of
`NetManager.GetAllBindInterfaces(false)`) and `NetworkManager.
GetAllBindInterfaces`. Ferrofin's port of the latter already existed and was
unused by the listener.

#### Behavior

- **No startup bind given** (no `--bind`, `FERROFIN_BIND_ADDR` or
  `config.toml` `bind_addr`): the listeners are `GetAllBindInterfaces(false)`
  over the saved network settings:
  - `LocalNetworkAddresses` set → one listener per resolved interface address;
  - otherwise IPv4 only → `0.0.0.0`; IPv4 and IPv6 → a dual-stack `::`;
    IPv6 only → the host's IPv6 interface addresses, link-local ones bound on
    their interface (the scope id a .NET `IPAddress` carries);
  - both families off → startup fails with a message saying so (Kestrel with no
    `Listen` has nothing to serve).
- **A startup bind given** stays the one listener. This is a deliberate
  divergence: Jellyfin has no such flag, and an address an operator pins in a
  container or service file is an explicit exposure choice that a dashboard
  change should not widen. The default is unchanged (`0.0.0.0`, like Jellyfin's
  all-interfaces default).
- `::` is bound dual-stack explicitly (`IPV6_V6ONLY` off) rather than by
  platform default, which is IPv6-only on Windows and the BSDs. A host without
  IPv6 falls back to `0.0.0.0` with a warning, as Jellyfin drops IPv6 when the
  OS does not support it.
- Several listeners share one router, one drain trigger and one shutdown
  timeout; the first one names the bound port reported to the host. Changing the
  settings takes effect at the next start, as in Jellyfin.

#### Verification

- Unit (`listen`): an operator's bind wins over the manager's list; a list
  becomes one address per interface on the port, link-local IPv6 with its scope;
  several binds yield several listeners; `::` accepts an IPv4 client; an address
  in use is an error naming it. The nagle test, which binds through the same
  function, still passes.
- Production binary, previous baseline vs updated, started without `--bind` on
  a host with a LAN address, IPv6 on `lo`, and `network.json` varied. Reachable
  = a TCP connect succeeds:

  | network settings | before: 127.0.0.1 / ::1 / LAN | after: 127.0.0.1 / ::1 / LAN |
  |---|---|---|
  | defaults | yes / no / yes | yes / no / yes |
  | `LocalNetworkAddresses = [127.0.0.1]` | yes / no / **yes** | yes / no / **no** |
  | `EnableIPv6 = true` | yes / **no** / yes | yes / **yes** / yes |
  | `EnableIPv4 = false, EnableIPv6 = true` | **yes** / no / **yes** | **no** / yes / **no** |
  | both off | boots | **refuses to start**, naming why |
  | `--bind 127.0.0.1` with `LocalNetworkAddresses = [LAN]` | yes / no / no | yes / no / no |

#### Limitations

- The HTTPS ports and certificate (“Local HTTP/HTTPS ports `InternalHttpPort`, `InternalHttpsPort`” (open)–“Enable/require HTTPS `EnableHttps`, `RequireHttps`; certificate and password `CertificatePath`, `CertificatePassword`” (open)) and the base URL (“Base URL `BaseUrl`” (open)) are
  separate findings and unchanged.
- Interface addresses are resolved when the server starts; a changed address
  needs a restart, as in Jellyfin.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for the server
package passed: server 235. Real FFmpeg tests enabled; the WASM guest build
remains disabled. Clippy flagged an over-long `serve_once`, an unneeded `async`
and two style lints in new code; they were fixed (the bind moved into a helper)
and Clippy, the listener tests and the live check were rerun clean, while the
full package run and doctests were taken before those edits. The server is
exempt from the coverage gate (composition root); the planner and listener
logic it contains is tested above. The dependency added is `socket2` 0.6, the
latest release and already in the lockfile through tokio.

## Live TV

### The M3U playlist is fetched with the tuner's user agent

The tuner's **User agent** setting reached only the stream's required HTTP
headers. The playlist request itself always presented Ferrofin's own product
agent, so a provider whose playlist URL only answers a particular client failed
before any channel was listed.

Source oracle: Jellyfin `4910aafa1a`, `M3uParser.GetListingsStream`
(`requestMessage.Headers.UserAgent.TryParseAdd(info.UserAgent)` for a non-empty
value) and `M3UTunerHost` (which already sends the agent for the stream).

#### Behavior

- `SourceFetcher` gains `fetch_as(url, user_agent)`. The real fetcher sends
  the agent on `http(s)` requests; a local playlist path needs none. The default
  ignores it, so existing fakes keep working.
- The M3U tuner host fetches its playlist with `tuner.user_agent`.
- A blank value, or one that is not a valid header value, leaves the client's
  own agent in place (`TryParseAdd` drops what it cannot parse).
- The XMLTV listings fetch is unchanged: upstream's listings provider has no
  user-agent setting.

#### Verification

- Unit: a one-shot HTTP server records the request's `User-Agent` — a
  configured value (trimmed) arrives, an absent, blank or invalid one leaves the
  product agent; the default `fetch_as` ignores the agent; the host passes the
  tuner's value through.
- Production binary, previous baseline vs updated, an M3U tuner with `UserAgent
  = "VLC/3.0 ferrofin-test"` served by a local HTTP server that records the
  header:

  | | playlist request `User-Agent` |
  |---|---|
  | before | `Ferrofin/0.4.1` |
  | after | **`VLC/3.0 ferrofin-test`** |

#### Limitations

- A configured agent is sent as a single `User-Agent` value; upstream's
  `TryParseAdd` also accepts product/comment lists, which reqwest passes through
  as the same single string.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on livetv passed: livetv 299, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. Line coverage over livetv's own
sources: **93.82%**, above the 80% gate; traits and server composition are
coverage-exempt. The instrumented build was cleaned right after the numbers
were taken, leaving `target` at about 24 GiB with about 494 GiB free.

### Tuner stream settings reach the HLS transcode

A tuner's **Enable stream looping**, **Ignore DTS** and **Read input at native
frame rate** settings reached the media source (and the DVR recorder), but the
general HLS transcode planner consumed only the native-rate flag (as `-re`).
Playing a live channel through HLS therefore ignored the other two, and the
input timestamp sync a native-rate source needs.

Source oracle: Jellyfin `4910aafa1a`, `EncodingHelper.GetInputModifier` and
`AttachMediaSourceInfo` (`ReadInputAtNativeFramerate`, `IgnoreInputDts` →
`+igndts`, `RequiresLooping` → `-stream_loop -1 -reconnect_at_eof 1
-reconnect_streamed 1 -reconnect_delay_max 2`, `InputAudioSync = "1"` and
`InputVideoSync = "-1"` for native-rate sources that are not segmented live
streams or for a local `wtv` file).

#### Behavior

The input options before `-i` now follow `GetInputModifier`'s order:

1. **Timestamp sync** — `-async 1`, and `-vsync -1` below ffmpeg 5.1 (where
   `-fps_mode` cannot be an input option), for a native-rate source that is not
   a segmented live stream, and for a local `wtv` file.
2. **Pacing** — unchanged (`-re`, or the copy `-readrate` pacing, with
   `-readrate_catchup` on ffmpeg 8).
3. **`-fflags +igndts`** when the source ignores input DTS.
4. **Looping** — `-stream_loop -1 -reconnect_at_eof 1 -reconnect_streamed 1
   -reconnect_delay_max 2` when the source requires looping.

#### Verification

- Unit (mediaencoding): the sync options for native/non-native, segmented live,
  `wtv` and the ffmpeg 5.1 boundary; ignore-DTS and looping alone and together.
- Planner (server): a source with all three flags on ffmpeg 5.0 yields
  `-async 1 -vsync -1 -re -fflags +igndts -stream_loop -1 …` before `-i` and
  none of it after.
- Production binary, previous baseline vs updated, an M3U tuner with stream
  looping, ignore DTS and native rate over a local MPEG-TS stream, opened
  through `PlaybackInfo` and `live.m3u8` with ffmpeg's argv recorded:

  | | input options before `-i` |
  |---|---|
  | before | `-analyzeduration 200M -probesize 1G -re -readrate_catchup 100` |
  | after | same, then **`-fflags +igndts -stream_loop -1 -reconnect_at_eof 1 -reconnect_streamed 1 -reconnect_delay_max 2`** |

  A segmented live stream takes no `-async`, as upstream. The playlist request
  itself answered 500 on both builds in this harness; its cause was not
  investigated, and it is identical before and after, so the recorded argv is the
  evidence.

#### Limitations

- The other `-fflags` members upstream's modifier can add (`+ignidx`,
  `+genpts` for copy, `+discardcorrupt`, `+fastseek`) and the `-f` input
  container option are not driven by these settings and are unchanged here.
- Direct play of a looping tuner is already refused by the media source
  (`SupportsDirectPlay` is false), so looping sources always reach this path.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on mediaencoding passed: mediaencoding 954, hls 117, core
2,447, extensions 96, livetv 299, wasm 62, server 231. Real FFmpeg tests
enabled; the WASM guest build remains disabled. Line coverage over
mediaencoding's own sources: **94.22%**, above the 80% gate; traits and server
composition are coverage-exempt. The instrumented build was cleaned right after
the numbers were taken, leaving `target` at about 24 GiB with about 494 GiB
free.

## Scheduled tasks

### A trigger's maximum runtime applies to the runs it starts

`MaxRuntimeTicks` was enforced as the smallest limit across **all** of a
task's triggers, for any running invocation — including one an administrator
started by hand. Two triggers with different limits could not behave
independently, and a manual run was killed by a limit it never agreed to.

Source oracle: Jellyfin `4910aafa1a`, `ScheduledTaskWorker.GetTrigger`
(`TaskOptions { MaxRuntimeTicks = info.MaxRuntimeTicks }` per trigger),
`ExecuteInternal` (`CancelAfter(options.MaxRuntimeTicks)` when options carry
one) and `TaskManager.QueueScheduledTask`/`ExecuteQueuedTasks`, whose queue
entries carry their options. A manual start passes no options.

#### Behavior

- The run records the limit of the trigger that started it: a daily, weekly or
  interval trigger that fires, or a startup trigger (including a late
  registration's). A manual run, the API's start, and a trigger without a limit
  record none.
- The scheduler sweep aborts a run only when it has exceeded **its own**
  limit, measured from when it was claimed; the minimum across triggers is
  gone.
- A request queued while the task is busy keeps its trigger's limit through the
  queue and starts with it (`ExecuteQueuedTasks`); the first request per task
  wins, as before.
- The limit is cleared when a run ends or is stopped, so it never carries to
  the next invocation.

#### Verification

- Unit: a trigger-started run past its limit is aborted; a manual run of a
  task whose trigger has a limit survives a sweep; a request queued behind a
  running task starts with its own limit and is aborted by it; a run an
  unlimited trigger starts survives a sweep even though another trigger has a
  limit.
- Production binary, previous baseline vs updated, a manual "Scan Media
  Library" over 12,000 files with a daily trigger of `MaxRuntimeTicks = 1`
  configured (the trigger itself never fires during the test):

  | | result | time |
  |---|---|---|
  | before | **Aborted** by the first sweep | 29 s |
  | after | **Completed** | 61 s |

#### Limitations

- When several triggers of one task are due in the same sweep, the first one's
  limit is used; upstream queues one run per fired trigger and the task manager
  keeps the first queued request, which is the same outcome.
- The limit is checked by the 30-second sweep, so an abort can lag the limit by
  up to one period; this predates the change.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,447, extensions 96, livetv 296, wasm
62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The first Clippy run flagged a `clone` on a `Copy` type in the new
test; it was fixed and Clippy and the scheduler tests rerun clean, while the
package and coverage results were taken just before. Line coverage over core's
own sources: **95.10%**, above the 80% gate; traits and server composition are
coverage-exempt. The instrumented build took `target` to 42.9 GiB, above the
30 GiB cap; its cache was removed right after the numbers were taken, leaving
24.2 GiB with about 494 GiB free.

### A trigger save that cannot be written is an error

Saving a scheduled task's triggers changed the in-memory schedule and then
wrote the store; a failed write was only logged, so the API answered success
and the schedule vanished at the next restart.

Source oracle: Jellyfin `4910aafa1a`, `ScheduledTaskWorker.Triggers` (setter)
and `SaveTriggers`: the file is written first and the in-memory triggers are
assigned afterwards, and an I/O failure propagates to the caller.

#### Behavior

- `set_triggers` validates the triggers, writes the store, and only then
  replaces the task's schedule and the stored overrides. A failed write returns
  a backend error (HTTP 500 from `POST /ScheduledTasks/{id}/Triggers`) and
  leaves both the running schedule and the stored map as they were.
- An unknown task is still reported as not found, before anything is written.
- The run-history store keeps its log-only behavior: upstream's result save is
  part of finishing a run, and a history write must not fail the run it
  describes.

#### Verification

- Unit: a store whose directory cannot exist (a file stands in its place)
  makes `set_triggers` fail with "could not save the triggers", leaving the task's
  triggers and the stored overrides empty; the existing persistence, restart and
  invalid-trigger tests still pass.
- Production binary, previous baseline vs updated, config directory made
  read-only and a trigger saved:

  | | HTTP status | triggers afterwards | store file |
  |---|---|---|---|
  | before | 204 | the unsaved interval, until restart | none |
  | after | **500** | unchanged (the default) | none |

#### Limitations

- The write is still atomic-by-rename rather than upstream's truncating
  `FileMode.Create`; a failure partway leaves the previous file intact, which is
  safer, and the visible result (an error) is the same.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,446, extensions 96, livetv 296, wasm
62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.10%**, above the 80% gate;
traits and server composition are coverage-exempt. The instrumented build took
`target` to 42.9 GiB, above the 30 GiB cap; its cache was removed right after
the numbers were taken, leaving 24.2 GiB with about 494 GiB free.

## Backups

### A restored backup's metadata follows its restored configuration

A scheduled restore is applied at the next boot, before the configuration
manager exists. The metadata destination was read from the **current**
`system.json` before the archive replaced it, so a backup whose configuration
named a different metadata root restored its files into the old root while the
recovered configuration pointed at the new one: after the restart the server
looked for its metadata where nothing had been restored.

Source oracle: Jellyfin `4910aafa1a`, `BackupService` (restore extracts the
archived `Config`/`Metadata`/… trees and then the server restarts into the
restored configuration). Jellyfin resolves `InternalMetadataPath` from the live
configuration at the time and so shares the mismatch; Ferrofin corrects it
rather than port it.

#### Behavior

- The archive is extracted in two passes: everything except `metadata/` first
  (database, `config/`, trickplay, subtitles), then `metadata/`.
- Between the passes the restored `config/system.json` is read; if it names a
  `MetadataPath`, the metadata tree is extracted there (created if missing).
  Without one, the boot-time root is used as before.
- The boot hook and the restore share one reader for that setting
  (`restored_metadata_root`).

#### Verification

- Unit: an archive whose `system.json` relocates the metadata, restored over
  boot roots that know nothing of it, puts `library/poster.jpg` in the relocated
  root — recreated when it no longer exists — and leaves no `metadata/` under
  the boot-time root. The existing round-trip, marker and manifest tests pass.
- Production binary, previous baseline vs updated: `MetadataPath` set to
  `meta1`, a marker file written there, a backup with metadata created, the
  setting changed to `meta2`, the backup restored, and the server restarted:

  | | `system.json` `MetadataPath` after restore | marker restored to `meta1` | marker restored to `meta2` |
  |---|---|---|---|
  | before | `meta1` | no | **yes** (the wrong root) |
  | after | `meta1` | **yes** | no |

#### Limitations

- A relative `MetadataPath` is used as written (relative to the process), as
  the boot hook already did for the current configuration.
- Archives written by other Ferrofin versions restore the same way; only the
  destination of the metadata tree changed.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on api passed: api 932, server 231. Real FFmpeg tests
enabled; the WASM guest build remains disabled. Line coverage over api's own
sources: **87.84%**, above the 80% gate; traits and server composition are
coverage-exempt. The instrumented build was cleaned right after the numbers
were taken, leaving `target` at about 24 GiB with about 494 GiB free.

### A failed restore is reported, not announced as a success

The web client treats "the server answers again" as a finished restore. A
scheduled restore that failed was logged and the boot carried on regardless —
over a data directory that might be half replaced — so the client announced
success for a restore that had not happened. The marker was removed either way
and nothing durable recorded the failure.

Source oracle: Jellyfin `4910aafa1a`, `Program.cs` (`RestoreBackupAsync` runs
before the host starts and its exceptions propagate out of startup, so the
server does not come up) and `BackupService.RestoreBackupAsync` (missing
archive, missing manifest and incompatible engine are thrown, not swallowed).

#### Behavior

`apply_pending_restore` now returns a `RestoreFailure` that says whether
extraction had begun:

- **Rejected before anything was written** (the marker names no archive, the
  archive is missing, foreign or not restorable): nothing was touched, so the
  server boots on its existing data, logs the error, and writes an **error
  entry to the activity log** — "Restoring the backup failed", type
  `BackupRestoreFailed`, with the reason as its overview — which the dashboard's
  Activity page shows.
- **Failed during extraction** (a file cannot be written, the disk fills): the
  trees may be partly replaced. The boot is aborted with an error, as upstream's
  startup exception does, instead of serving them; the process exits and the
  client never sees the server come back.
- The pending marker is still removed in every case, so a failed restore
  cannot loop the next boot. Success is unchanged.

#### Verification

- Unit: a missing archive and a foreign-engine archive are rejected with
  `partially_applied == false` and the archive named in the message; a valid
  archive whose extraction cannot write is `partially_applied == true` and still
  consumes the marker; a marker naming nothing is an untouched failure with no
  archive.
- Production binary, previous baseline vs updated, two scheduled restores
  started from a stopped server:

  | | marker names a missing archive | real archive, `system.json` read-only (fails mid-extraction) |
  |---|---|---|
  | before | boots; no activity entry | **boots and serves** |
  | after | boots; activity entry **"Restoring the backup failed"** | **process exits** |

#### Limitations

- The activity entry is written once the application state is wired, so it
  appears after the boot completes, not during the restore.
- A part-way failure leaves whatever was already extracted in place; there is
  no rollback.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on api passed: api 933, server 231. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
`serve_once` at 126 lines; the restore handling moved into two helpers and
Clippy, the server tests and the live check were rerun clean, while the api
results and coverage were taken just before that refactor. Line coverage over
api's own sources: **87.88%**, above the 80% gate; traits and server
composition are coverage-exempt. The instrumented build was cleaned right after
the numbers were taken, leaving `target` at about 24 GiB with about 494 GiB
free.

## Scanning and library structure

### Plain subfolders of a movie library are `Folder`s

A movie library's plain subfolders — a genre or decade directory that is no
title's own folder (`Action/`, `Action/90s/`) — were flattened: their titles
hung straight off the library and the directory was no item at all, so a
"Folders" view (and `GET /Items?ParentId=<library>`) could not browse into it.
Jellyfin's `FolderResolver` claims any directory no other resolver does
(`FolderResolver.cs:24-27`, last priority), and `Folder.GetNonCachedChildren`
resolves what is inside it with the folder as parent
(`ResolvePaths(children, …, this, …)`). The old code carried the gap as an
explicit TODO naming this exact path.

Oracle: Jellyfin 12.2 (portable build, run against the same trees as Ferrofin;
the pinned source is `4910aafa1a`) — see the comparison below.

#### Behavior

- `collect_movie_plan` (movies, music videos and home videos) plans each
  directory that is not a title's own folder, not a disc rip, and not inside a
  title's own folder (whose subfolders hold its extras) as a `Folder` row:
  `ParentId` is the folder above (the location for a top-level one), `TopParentId`
  stays the library, and what is in it is planned with that folder as parent and
  an ancestor closure that runs through it. Titles keep every other rule (own
  folder → the movie, named from the folder; disc rips; extras; samples).
- An empty directory, or one whose only video is an ignored sample, is a folder
  too (as Jellyfin lists it). A title whose file is gone leaves its directory as a
  plain folder. A path-scoped scan keeps the folders above the scoped path as
  context rows, as it keeps every row above.
- **Existing databases upgrade in place**: the next scan creates the folder rows
  and re-parents the titles under them; ids, user data and metadata are kept (the
  row's identity is its path).

#### Verification

- Core tests: a library with `Action/Heat`, `Action/90s/Ronin` (+ `extras/`),
  `Top` in its own folder, a loose movie and an empty directory — folder rows
  with the right parents, tops and ancestor rows, no row for a title's own folder
  or its `extras/`, listings of the library and of each folder, an unchanged
  rescan (nothing created/updated/removed) and the removal of one title leaving its
  folder and its siblings. The existing scanner suites were updated where they had
  pinned the flattened shape (the adopted-library fixture, the nested-location
  test, the extras browse counts, an emptied title folder and a refresh of a gone
  item, each now with a folder row Jellyfin also has); `plan_paths_matches_the_filtered_full_plan`
  (scoped plan = filtered full plan for every fixture path) still holds.
- Production binary, `Action/{Heat,Ronin}`, `Top`, a loose file:

  | | `GET /Items?ParentId=<library>` | `Action` |
  |---|---|---|
  | before | `Heat, Loose Movie, Ronin, Top` (all flat) | — |
  | after | **`Action` (Folder), `Loose Movie`, `Top (2001)`** | **`Heat`, `Ronin`** |

  Upgrade: a library scanned by the previous build and rescanned by this one keeps
  the ids and a favorite and re-parents `Heat`/`Ronin` under `Action`.
- **Against Jellyfin 12.2**, the same media tree with `Outer` = the whole tree
  and `Inner` = `…/Kids` (nested), three scans each, then removing `Inner`:

  | | Jellyfin 12.2 | Ferrofin |
  |---|---|---|
  | Outer, direct children | `Action, Empty, Kids` (Folders), `Loose Movie`, `Top (2001)` | identical |
  | Outer, recursive | `90s, Action, Coco, Empty, Heat, Kids, Loose Movie, Ronin, Sub, Top, Up, movies` | identical |
  | Inner, direct children | `Sub` (Folder), `Up (2009)` | identical |
  | Inner, recursive movies | none | none |
  | Folder rows (name → parent) | `Action`,`Empty`,`Kids` → the location; `90s` → `Action`; `Sub` → `Kids` | identical shape |
  | After removing `Inner` | `Outer` unchanged | unchanged |

  (The location folder `movies` carries the same id prefix in both.)

#### Limitations

- Only the movie-style planner (movies, music videos, home videos). The TV,
  music, book and photo planners are untouched here: their intermediate
  directories are series, seasons, artists and albums, or are not probed against
  the oracle in this change.
- "Inherited policies" for an inner library (parental rating, tag filters on a
  nested library) is not separately probed.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,486, livetv 299, server 237,
extensions 96, wasm 62; real FFmpeg tests enabled, the WASM guest build
disabled. The first run's Clippy flagged the new test as over-long (one tree,
every property of it, so it carries an allow); fmt, Clippy, the boundary test and
core's tests and doctests were rerun clean after. Line coverage over core's own
sources: **95.15%**, above the 80% gate; the server composition root is
coverage-exempt. The instrumented build was cleaned (`target` about 25 GiB).

### Unlocked metadata after a library rename and an explicit scan — owner decision needed

Not a closure: this records what the native oracle shows and the decision it
puts to the project owner. No behavior was changed.

The [Rename honors the refresh setting](#rename-honors-the-refresh-setting) fixture found that native Jellyfin 12.1 resets an item's edited title and
overview on a later explicit scan, where Ferrofin keeps them. The finding asked
to check the pinned refresh/merge rules before changing anything. Replaying the
scenario against a live Jellyfin 12.2 (portable build) and the current Ferrofin
shows what actually triggers it.

#### The scenario (one movie; metadata edited through `POST /Items/{id}`, played)

| step | Jellyfin 12.2 | Ferrofin |
|---|---|---|
| edit title and overview, mark played | kept | kept |
| rename the library (`POST /Library/VirtualFolders/Name`, no refresh) | kept | kept |
| **explicit library scan** | **title reset to the parsed name `Allowed (2020)`, overview cleared**; same item id, still played | kept |
| second scan | stays reset | kept |
| *same steps without the rename*: explicit scan | **kept** | kept |

So an explicit scan alone never resets an unlocked edit in either server
(`MetadataRefreshMode.Default` re-reads nothing for an unchanged file). The reset
is specific to **renaming a library**: Jellyfin replaces the library's collection
folder (its id derives from the new name's path), the old one and the items under
it are removed, and the next scan resolves them again from disk — a fresh row
with the file's parsed name, no overview, no manual edits — while the item id
(path-derived) and the user data (keyed, retained) survive.

#### What porting it would mean

Ferrofin keeps the library's items across a rename (the row, its edits and its
locks). Matching Jellyfin means a library rename discards every manually edited
field of every item in the library — titles, overviews, cast, images the user
chose — which the user must re-enter. That is data loss caused by a rename, which
the user did not ask for, and which Jellyfin's own web client does not warn about.

The repository's standing guidance is not to port Jellyfin behavior that is a
defect where Ferrofin is correct, and only the owner may add an accepted
divergence (`CLAUDE.md`, *Nothing is deferred*; "don't port Jellyfin bugs").

#### Decision needed

1. **Keep Ferrofin's behavior and record it as an accepted divergence** (library
   rename keeps the library's items and their edits), or
2. **Port the reset** (a rename removes the old library's items; the next scan
   re-resolves them), including user-data retention across it.

Recommendation: 1. Until it is decided this finding stays open.

Reproduce: `/tmp/ferrofin-st04/oracle_s04.py` style probe — edit via `POST /Items`,
`POST /Library/VirtualFolders/Name?...&refreshLibrary=false`, `POST /Library/Refresh`,
read the item back — against the portable Jellyfin 12.2 and the dev server.

### Repair the complete scan-change fixture

The complete scan matrix passes again without changing production behavior.
Its mock now provides separate TMDB image-list responses, including dimensions,
which the [Automatic artwork limits and minimum widths](#automatic-artwork-limits-and-minimum-widths) acquisition policy requires. Movie default ordering expects TMDB
before FanArt, and the explicit-order case reverses that order and checks the
FanArt download. TVDB extended replies include actual English translations,
required when the plugin's original-language fallback is disabled.

Watcher create/delete rows retain exact probe, provider, table and item-write
assertions. The existing sibling episode remains unchanged: pinned EpisodeResolver
and BaseVideoResolver do not set IsInMixedFolder when a sibling appears. The
season refresh, series newest-media date, and created/deleted episode writes
remain required. Movie resolution has its own mixed-folder rules.

Source: Jellyfin `4910aafa1a`, EpisodeResolver, BaseVideoResolver and BaseItem;
TVDB plugin `5c4592f`, TvdbSeriesProvider, TvdbEpisodeProvider, TvdbSdkExtensions
and original-language fallback configuration; provider intrinsic ordering
already verified and recorded in [Image provider selection and order](#image-provider-selection-and-order). The expanded movie mock is extracted
into a helper to retain strict lint compliance.

Validation: both scan_change_detection tests pass, including the entire matrix
through its later provider, subtitle and outage scenarios. Formatting and
strict workspace all-target/all-feature Clippy pass. Final matrix run takes
25.43 seconds including the test command. This is a test-only server-crate
change, so it does not change production coverage or need a runtime performance
comparison. Evidence: `/tmp/ferrofin-dashboard-s18-checks.json` and corresponding
logs. This closes the fixture repair, not the separate final full workspace gate.

### Filesystem-root library shortcuts

Libraries configured with `/` previously saved an empty Locations list because
shortcut resolution stripped every slash. Resolution now removes one native
trailing directory separator and preserves the filesystem root. Backslashes
remain literal characters on Unix, and neither whitespace nor line endings
are trimmed. Root-path removal also keeps its separator when matching the
resolved shortcut, so removing `/` removes the shortcut and saved PathInfos.

The source oracle is Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/IO/MbLinkShortcutHandler.cs`, which calls
`Path.TrimEndingDirectorySeparator`, and the root match in
`LibraryManager.RemoveMediaPath`. Existing general path comparison behavior
and the separate physical hierarchy finding [Plain subfolders of a movie library are `Folder`s](#plain-subfolders-of-a-movie-library-are-folders) remain independent.

Regressions cover root and repeated separators, Unix literal backslashes,
relative paths, blank contents, whitespace and line endings. A real manager
round-trips the configured root, physical paths, owning-library policy and
removal. HTTP tests check saved Locations, PathInfos and library identity.
The disposable native fixture never refreshes after adding the host root.
Its Ferrofin ownership check uses an already scanned disposable movie and
uploads a subtitle after removing its original library: the root library's
SaveSubtitlesWithMedia=false must select internal metadata storage.
The Jellyfin 12.1.0 comparison checks root Locations and removal.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary
check, production build, targeted regressions, real HTTP and disposable native
checks passed. The full source-qualified workspace matrix passed **8,410
tests** across 21 packages and 201 default test targets.
All changed packages and their dependents were rerun; unchanged independent
packages retain their prior passing results after source and dependency checks.
Real FFmpeg tests were enabled. The five existing skipped tests remain in the
DB/provider suites. The WASM guest build was separately disabled explicitly.
These limits remain visible in the retained commands and test summaries.

Fresh, unseeded **ferrofin-core line coverage is 95.40%**,
above its separate 80% gate. These final gates cover both [Indexed artwork deletion](#indexed-artwork-deletion) and this finding; every
tested Rust/Cargo input and the final production binary is checked by
`/tmp/ferrofin-extra-two/quality-proof.json`. The canonical profile and complete
logs are retained under `/tmp/ferrofin-extra-two/`. Both native fixtures pass
against the final production binary, and every owned server was reaped.
The root ownership fixture reproduced sidecar fallback before the fix and
verified internal subtitle storage afterward. The reference root fixture
checks Locations and removal independently. Shared-host request timings are
observations, not publishable benchmarks. Earlier fixture and lint failures
remain preserved with their corrected reruns.

Only generated build caches were cleaned. The worktree, commits, production
binary, native data and canonical coverage profiles were preserved. After validation cleanup, target
usage is **11.48 GiB**, with **510.84 GiB free** on the host.

### A video being recorded is keyed under Live TV

`Video.SourceType` is `LiveTV` while `RecordingsManager.GetActiveRecordingInfo(Path)`
knows the file (`Video.cs:171–181, 454–457`). `BaseItem.SourceType` is `Channel`
for any other item with a ChannelId, and only `Channel` items put their
`ExternalId` ahead of their own id in `GetUserDataKeys` (`BaseItem.cs:1733–1747`).
So a recording that carries a ChannelId and ExternalId is keyed by its channel
id *until* its file is being written to, and by its own id (the Live TV arm)
while it is. Ferrofin's in-memory folder filter chose the channel key from the
row alone, so favorites, likes, resume and played filters over a physical
folder's children read the channel-keyed row of a recording that Jellyfin reads
by the item's own key.

Source oracle: Jellyfin `4910aafa1a` (`Video.SourceType`, `Video.IsActiveRecording`,
`BaseItem.SourceType`, `BaseItem.GetUserDataKeys`); evidence in the living
review row this finding.

#### Behavior

- `FerrofinItemRepository::folder_own_user_data` treats a video whose `Path`
  is among Live TV's active-recording paths as `LiveTV`: no channel
  `ExternalId` key is inserted for it. Non-video kinds, videos that are not
  recording, and recordings without a ChannelId/ExternalId are unchanged.
- The fact comes from the real Live TV manager (`active_recording_paths`, the
  same source delete/trickplay/subtitle checks use), not from the stored `Data`
  blob or the row's own fields. The repository cannot own the manager (Live TV
  is built after it and needs the DTO service), so it holds a late-bound slot
  (`ActiveRecordingSlot`) that the composition root fills once with a closure
  over a **weak** manager reference: no ownership cycle, and a dropped manager
  reads as "nothing recording". The manager is asked only when a page contains
  a video that could be channel-keyed, so ordinary browses add no work.

#### Verification

- Core test: four items with the same channel external key and a favorite
  stored only under that key — an active recording movie, an inactive movie, an
  active episode and a folder at the same path. Before the slot is bound all
  four select by the channel key; once Live TV reports `/rec/active.ts`, the two
  active videos drop out and the inactive movie and the folder keep the channel
  key.
- Production binary, one M3U channel, a manual timer recording into a library
  folder (the capture is in progress, status `InProgress`), the recording's row
  given a ChannelId and ExternalId with a favorite stored only under that
  external key, user data of the own id not favorite:

  | favorites of the physical folder's children | count |
  |---|---|
  | before (old binary) | 1 (channel key read) |
  | after (new binary) | **0** (Live TV key, own row not favorite) |

  Unchanged in both builds: the item's own DTO (`IsFavorite` false), and the
  SQL-translated recursive favorite queries (1 in both).

#### Limitations

- The SQL-translated recursive/paged queries and the user-data sorts do not
  consult the active-recording fact; they read the stored key list and are not
  the in-memory GetResult consumer this finding covers. Unifying them with
  `folder_own_user_data` is a separate source comparison.
- `user_data_keys` still never emits a channel key when saving (the known
  omission documented in that module); this change does not add channel items.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,460, extensions 96, livetv 299, wasm
62, server 237. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.13%**, above the 80% gate;
the server composition root and traits are coverage-exempt (the slot wiring is
exercised by the live run above). The instrumented build was cleaned afterward
(`target` about 25 GiB, about 494 GiB free).

### Home-video libraries resolve plain videos

A library registered as **Home Videos** resolved its files as `Movie`s, named
and dated by the movie parser. Jellyfin's `MovieResolver` resolves them as
generic `Video`s with `parseName: false` and treats a folder of videos as just
a folder; the item type is visible to every client (`Type`), and the choice
decides names, year, `IsMovie` and which movie-only behaviors an item takes
part in.

Source oracle: Jellyfin `4910aafa1a`, `MovieResolver` (`Resolve`:
`ResolveVideo<Video>(args, false)` for `homevideos`/`photos`; `FindMovie`'s
`isPhotosCollection` arm; `ResolveVideos<Video>(…, supportMultiEditions: false,
…, parseName: false)`), `VideoResolver` (`parseName: false` keeps the stem and
no year); evidence note `brain/knowledge/DASHBOARD_SETTINGS_S32_HOME_VIDEO_
RESOLVER.md`.

#### Behavior

- A home-video library plans every video file as a `Video` (stored type
  `MediaBrowser.Controller.Entities.Video`): `Name` is the file stem as written
  (`Beach Day (2021)`), `ProductionYear` is not read from it, `IsMovie` is
  false, `IsInMixedFolder` is true (also for a lone file in its own folder),
  `MediaType` is `Video`. A disc rip (`VIDEO_TS`, `BDMV`) still resolves as one
  item for its folder, now a `Video`.
- A folder of videos is no title's "own-folder movie" there: `FindMovie` only
  makes one outside photo collections, so the single-video-folder naming and
  alternate-version grouping do not apply.
- Movie and music-video libraries are untouched (`Movie` with the folder name
  rule; `MusicVideo` named by its stem); the `null`/mixed library arms are not
  changed here.
- **Existing rows keep their identity.** A file an earlier build stored as a
  `Movie` is planned at the same path as a `Video`: the scan reuses that row's
  id (so user data, links and history stay attached) and moves its type to
  `Video`; no second row appears. An adopted `Video` row is reused as before.
  Rows keep their stored name until a metadata refresh replaces it.
- Photo handling (`EnablePhotos`) is a separate consumer and unchanged.

#### Verification

- Scan tests: a flat file, a file in a folder, and a lone file in its own
  folder become `Video`s named by stem with no year, `IsMovie` false, mixed
  folder, and no `Movie` row exists for them; a movies library still makes the
  `Heat (1995)` movie from its folder and a music-video library the stem-named
  `MusicVideo`; a pre-existing `Movie` row at the path survives two scans as one
  row of type `Video` with its id; the photo-toggle test (which seeds home
  videos) now follows the `Video` identity.
- Production binary, previous baseline then updated on the same data directory,
  two home videos, one marked played:

  | | Type | Id | Played |
  |---|---|---|---|
  | before (old binary) | `Movie`, `Movie` | `70e8cb58…`, `cf925f6e…` | yes, no |
  | after (new binary, rescans) | **`Video`, `Video`** | **unchanged** | **yes, no (kept)** |

#### Limitations

- Stacked-part grouping (`cd1`/`cd2`) of flat files is not applied to home
  videos, as it is not to movie files in a flat folder here.
- The `null`/mixed-library resolver arms and generic `Video` local readers
  ([Local NFO readers beyond movies, series and episodes](#local-nfo-readers-beyond-movies-series-and-episodes)) are separate.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,458, extensions 96, livetv 299, wasm
62, server 237. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.13%**, above the 80%
gate; traits and server composition are coverage-exempt. The instrumented
build was cleaned right after the numbers were taken, leaving `target` at
about 24 GiB with about 494 GiB free.

### By-name identity settings and long folder names

Two shared identity rules were only half ported: the item-id casing settings,
and the length bound on a by-name entity's folder.

Source oracle: Jellyfin `4910aafa1a` — `LibraryManager.GetNewItemIdInternal`
(`forceCaseInsensitive || !EnableCaseSensitiveItemIds` ⇒ `ToLowerInvariant`),
`LibraryManager.GetItemByNameId` (`forceCaseInsensitive =
EnableNormalizedItemByNameIds`), and `BaseItem.GetItemByNameFolderName`
(128-byte bound; used by `Person`, `Genre`, `MusicGenre`, `Studio`, `Year` and
`MusicArtist` `GetPath`).

#### Behavior

- **Id casing follows both settings.** A scanned item's id key is lowercased
  only when `EnableCaseSensitiveItemIds` is false; a by-name id key is
  lowercased when that is false **or** `EnableNormalizedItemByNameIds` is true.
  Ferrofin always lowercased by-name keys and always kept the case of scanned
  ones, so the case-sensitive, non-normalized combination had no
  implementation. The defaults (both true) are unchanged, so no existing id
  moves. The two settings are published to `item_type_lookup` when the
  configuration loads and each time it is saved, so the next derivation sees
  them as upstream's live reads do. Changing them changes the ids derived from
  then on; nothing rewrites ids already stored, which is also upstream's
  behavior.
- **Folder names are bounded.** The sanitized, trimmed name is kept when it
  fits 128 UTF-8 bytes; otherwise it becomes a readable prefix, `-`, and the
  MD5 (`N` format) of the whole sanitized name, cut on UTF-16 units without
  splitting a surrogate pair and with trailing space and dots trimmed before
  the suffix. Every by-name path (`person_path`, `by_name_path`, used for
  people, genres, music genres, studios, years and artists) uses it. Existing
  rows keep the paths they were created with.

#### Verification

- **.NET oracle.** The upstream method, copied verbatim and run on .NET 10 over
  39 names (one byte under/at/over the bound; 2- and 3-byte characters at the
  cut; supplementary-plane characters in every alignment of the surrogate
  guard; trailing space/dots exposed by the cut; the credit-list and shared-
  prefix cases of upstream's `BaseItemTests`), matches the Rust output exactly
  and stays within 128 bytes. The upstream xUnit cases are also transliterated.
- A separate test binary (the policy is process-wide) loads the real
  configuration manager, saves each combination of the two settings and checks
  which id keys keep their case — defaults `(true, false)`, case-sensitive but
  not normalized `(true, true)`, not case-sensitive `(false, false)`; a restart
  restores what was saved; and the key rule is checked for all four
  combinations.
- Production binary, previous baseline vs updated: an NFO actor whose name is
  270 characters. The Person folder component was **269 bytes** before (over
  the 255-byte limit of common Linux file systems, the case the upstream
  comment describes) and is **128 bytes** after
  (`Longname … Longn-75790778cd47ec616e1db5eed51b493b`). The id-casing flags are
  not observable over HTTP, because by-name rows are persisted when created and
  ids differ between fixtures only through their temporary data paths; the
  configuration-manager test above is the check for them.

#### Limitations

- The policy is a process-wide setting rather than a parameter of
  `IdDerivation`, which 36 call sites build; the configuration manager is its
  only writer.
- Changing the casing settings on an existing library does not migrate stored
  ids, so by-name entities scanned before the change keep their old ids while
  new ones use the new rule (as upstream).

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,436, extensions 96, livetv 296,
wasm 62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The first Clippy run flagged `load` growing to 101 lines and a
redundant `to_string`; both were fixed (the call moved into a shared helper),
and Clippy and the touched modules' tests were rerun clean, while the
downstream package results and coverage were taken just before that edit.
Line coverage over core's own sources: **95.10%**, above the 80% gate; traits
and server composition are coverage-exempt. The instrumented build took
`target` to 42.5 GiB, above the 30 GiB cap; its cache was removed right after
the numbers were taken, leaving 23.9 GiB with about 494 GiB free.

## Metadata providers and local metadata

### Path-less episode metadata providers

A TV episode with no file of its own (a virtual or missing episode) refreshes
through `LocalProviderManager`, not the library scan. Its refresh ran
TheMovieDb's episode lookup only — and resolved the series by a *name search*
when no `Tmdb` id was stored — dropping OMDb's and the TVDB plugin's episode
providers, the library's provider order, the episode's own credits, and the
episode's ids. A TODO in the code named exactly that gap.

Source oracle: Jellyfin `4910aafa1a` `TmdbEpisodeProvider`
(`TmdbEpisodeProvider.cs:92-97`: the series' `Tmdb` id, no search),
`OmdbEpisodeProvider`, jellyfin-plugin-tvdb `5c4592f` `TvdbEpisodeProvider`,
`MetadataService.ExecuteRemoteProviders`/`RefreshWithProviders`
(`MetadataService.cs:955-1025`, `:272-293`).

#### Behavior

- New `ferrofin-providers/src/episode.rs`, the episode counterpart of
  `season.rs`: `tmdb_answer`, `omdb_answer` (+ `OmdbGates`) and `tvdb_answer`
  turn each client's answer into the provider's `MetadataResult` — a fresh item
  with only what the provider sets, its credits and its ids. TheMovieDb: numbers,
  title, synopsis, rating (> 0), air date and year, the episode's own `Tmdb` id
  and the `Imdb`/`Tvdb`/`TvRage` ids of its external ids, its trailers, and its
  credits (a null list when TMDB credits nobody or the request missed). OMDb:
  title/certificate/genres only for an English library (certificate US only),
  original language, home page, plot, ratings, year, `Imdb` id, credits only with
  the plugin's `CastAndCrew`. TheTVDB: numbers, original title, the localized
  title/synopsis, air date, the specials' `Airs*` placement for the aired order,
  the episode's `Tvdb`/`Imdb` ids and its own credits (a list even when empty).
  `omdb_people`/`tvdb_people` moved here; the scanner calls them.
- `LocalProviderManager` registers TheTVDB and OMDb as episode fetchers (the
  latter only with a key) beside TheMovieDb, and a path-less episode's refresh
  (`refresh_item`) now runs `fetch_episode_providers`: the providers the library
  has enabled, in its saved order (default registration order breaks ties),
  Identify's chosen provider first. Every provider resolves by the **series'
  stored ids** — `Tmdb`, `Imdb`, `Tvdb` — never by a name search; a provider the
  series has no id for is not asked. OMDb also reads the episode's own `Imdb` id
  from the providers that ran before it.
- The answers fold as `ExecuteRemoteProviders` folds them: the first answer wins
  a field, later ones fill what is empty (lists union), ids gather, an overview in
  the preferred language displaces a fallback in another (OMDb's English plot),
  each answer's credits are kept and written by the usual rule (a `Cast` lock
  keeps the stored ones). An identified episode (Apply) looks up by the chosen
  result's ids as its series' and has none of its own. All failures are counted
  per request, so a failing provider leaves the refresh unstamped and never
  clears what the episode has.
- `fetch_tv`/`fetch_season` (the TMDB-only, name-searching arm) are gone; the
  TODO with them.

#### Verification

- `episode.rs` unit tests over the three mappers (empty synopsis/zero rating not
  set, trailers and credits, language and region gates, `CastAndCrew`, aired-order
  placement, empty credit lists).
- Manager tests through mock TheMovieDb/TheTVDB/OMDb servers over real HTTP: all
  three providers in both libraries' orders (first answer wins title/synopsis/date,
  the first `Imdb` id wins, every other id gathers, no `/search` request); a
  library with one provider and a series with no ids (nothing asked); credits
  written and a `Cast` lock; a French library taking TheTVDB's French synopsis over
  OMDb's English plot that ran first; every provider failing under Replace all
  (nothing erased, not stamped).
- Production binary smoke (a path-less episode, providers unreachable as the
  fixture has no network): `POST /Items/{id}/Refresh` with Replace all returns
  `204`, the episode keeps its title and synopsis, the server stays healthy —
  same before and after. The server's provider endpoints are a test seam that is
  never set from a file or environment, so the provider answers themselves are
  verified by the manager tests above, not through the binary.

#### Limitations

- The TVDB plugin's `IsAutomated` branch for episodes
  (`TvdbEpisodeProvider.cs:171`, an item-refresh route detail shared with the
  season TODO) is unchanged.
- `Fetched` images for an episode still come from the ordered image pass
  (`refresh_ordered_images`); this change is metadata only.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the providers crate passed: providers 926 (4 skipped), core
2,484, livetv 299, server 237, extensions 96, wasm 62; real FFmpeg tests
enabled, the WASM guest build disabled. Clippy first flagged the new episode fold
as over-long and the two now-identical `RefreshTarget` arms; the OMDb and TVDB
branches became `omdb_episode`/`tvdb_episode` and the arms were merged, then fmt,
Clippy, the boundary test and every package's tests and doctests were rerun
clean. Separate line coverage over each changed crate's own sources, measured
before that refactor (a move of code with its tests unchanged): providers
**92.05%**, core **95.15%**, both above the 80% gate; the server composition root
is coverage-exempt. Instrumented builds were cleaned (`target` about 25 GiB).

### TVDB metadata follows the series display order

The filesystem metadata pass asked TheTVDB for every season and episode in
`official` order. Both lookups now use the series' stored `DisplayOrder`, as
the TVDB plugin does, and the series' order is now read from its NFO and
shown to clients.

Source oracle: Jellyfin `4910aafa1a` and the TVDB plugin `5c4592f` —
`TvdbSeasonProvider.GetMetadata`, `TvdbClientManager.GetEpisodeTvdbId`,
`TvdbEpisodeProvider.MapEpisodeToResult`, `Season.cs:268` / `Episode.cs:314`
(`SeriesDisplayOrder = series.DisplayOrder`),
`LibraryManager.FillMissingEpisodeNumbersFromPath`, `BaseNfoParser`
(`<displayorder>`), `MetadataService.MergeDisplayOrder` and
`DtoService.cs:1207`.

#### Behavior

- **Seasons.** The season id is read from the series' season list for the
  display order — `official` when it is blank — matched case-insensitively with
  no fallback to another order when the series lists no such season. Scan and
  path-less refresh both read the series row.
- **Episodes.** The ordering, season and number asked for follow the plugin's
  case-sensitive switch (`tvdb::episode_lookup`): `regional`, `alternate`,
  `altdvd`, `dvd` and `alttwo` use their own order with the episode's season;
  `absolute` uses the `absolute` order in season 1, except specials (season 0),
  which use `official`; anything else is `official`. A season-less episode is in
  season 1 (`ParentIndexNumber ?? 1`); previously it was not looked up at all.
- **Aired-order fields.** `AirsBeforeEpisodeNumber`, `AirsAfterSeasonNumber`
  and `AirsBeforeSeasonNumber` are taken from TVDB only when the series has no
  display order ("only applicable for Aired Order").
- **Episode stills.** The metadata lookup and the image provider now use the
  same ordering, so a still found by the metadata pass is reused for every
  order, not only `official`.
- **Absolute naming.** A series whose display order is `absolute` (ordinal,
  ignore case) resolves its episodes' numbers with `isAbsoluteNaming`: only the
  absolute-number expressions run, so a name they do not match (`Show.102`)
  takes nothing from the path instead of reading season 1, episode 2.
- **`<displayorder>` is applied.** The NFO parser read it, but the scan never
  stored it, so no series could carry one from its NFO. It is now filled into
  the series row's `Data` blob (the merge rule `MergeDisplayOrder` already
  existed there), and the NFO saver writes it back.
- **DTO.** Series and BoxSet DTOs now emit `DisplayOrder` (previously never
  emitted, so the web editor's "Display order" select was always blank).
- Each pass reads a series' order once and shares it between its seasons and
  episodes.

#### Verification

- Unit: the episode lookup for every order (case-sensitive, specials,
  season-less), the raw reader and the season default, the DTO field for a
  series, a blank value, a collection and a movie, and absolute naming on a
  stored episode row.
- Scan (`library_scan_seasons`): a series whose `tvshow.nfo` says `dvd` reads
  its season from the `dvd` season TVDB lists (76), makes no request for the
  official season (77), and asks `/episodes/dvd`, never `/episodes/official`.
- Production binary, previous baseline vs updated, three series with
  `tvshow.nfo` orders `dvd` and `absolute` and none:

  | | Series DTO `DisplayOrder` |
  |---|---|
  | before | none, none, none |
  | after | **`dvd`, `absolute`**, none |

  The binary has no TVDB base-URL override and the fixture has no network, so
  the TVDB requests themselves are covered by the scan test above, not by the
  binary. Episode numbers were identical before and after (1×2): a new episode
  is numbered from its file name by the planner, and the absolute filter only
  matters for a stored row with missing numbers.

#### Limitations

- The planner still numbers a new loose episode without the absolute filter;
  upstream applies it only in `FillMissingEpisodeNumbersFromPath`, as here.
- TVDB's date-based episode lookup (`PremiereDate` without a number) is not
  ported in the scan's episode arm; it was not before.
- The path-less episode refresh still runs only TheMovieDb ([Path-less episode metadata providers](#path-less-episode-metadata-providers)).

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 911 (4 existing
skips), core 2,445, extensions 96, livetv 296, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled. The first Clippy
run flagged `fetch_season_providers` at 102 lines; the series-order read moved
into a helper and Clippy and the season tests were rerun clean, while the
package and coverage results above were taken just before that refactor. Line
coverage over each crate's own sources: **providers 91.40%**, **core 95.09%**,
both above the 80% gate; traits and server composition are coverage-exempt.
The instrumented core build took `target` to 42.9 GiB, above the 30 GiB cap;
each coverage cache was removed right after its numbers were taken, leaving
24.2 GiB with about 494 GiB free.

### The fanart.tv season image provider

The fanart.tv plugin ships a `SeasonProvider` (Backdrop, Thumb, Banner,
Primary). Ferrofin registered fanart.tv for movies and series only, so a
season's **Image fetchers** list never offered it, the season "Choose image"
dialog never listed it, and a scan never asked fanart.tv for season artwork.

Source oracle: `jellyfin-plugin-fanart` `7c2f859`,
`Providers/SeasonProvider.cs` (`Supports`, `GetSupportedImages`, `GetImages`,
`AddImages`, `PopulateImages`) and `Configuration/Constants.cs`.

#### Behavior

- **Registered for `Season`.** fanart.tv's library registry entry covers
  seasons with the plugin's list `Backdrop, Thumb, Banner, Primary`, so the
  dashboard advertises it, the per-library checkbox and order apply, and its
  default order is the plugin's `Order = 1`.
- **`FanartClient::season_images(series_tvdb_id, season)`** reads the series'
  fanart document and keeps, from `seasonposter` (Primary, 1000×1426),
  `seasonbanner` (Banner, 1000×185), `seasonthumb` (Thumb, 1000×562) and
  `showbackground` (Backdrop, 1920×1080), only images whose `season` parses to
  the season's number — so an `all`-seasons background is never a season's.
  A thumb added before 2016-01-08, or with no usable date, is 500×281
  (`WorkingThumbImageDimensions`). Languages `00` become none, `http://` urls
  become `https://`, likes are the rating, and the list is ordered like the
  other fanart providers (widest, then language, then likes).
- **Where it runs.** The scan's season artwork pass, in the library's configured
  image order beside TheMovieDb and TheTVDB; and the manual "Choose image" /
  path-less refresh listing (`RemoteImageSource::FanartSeason`).
- **Accepted divergence — which id keys the document.** The plugin looks the
  document up by the *season's own* `Tmdb`/`Tvdb` provider id. A season's ids
  identify the season (TheTVDB's season id, TheMovieDb's season id), not a
  fanart.tv show, so upstream's lookup finds nothing outside coincidences.
  Ferrofin keys the document by the **series'** TVDB id — the document the
  season filter was written for — which is what makes the provider usable.

#### Verification

- Unit (`fanart`): the season filter (the number, its zero-padded twin, not
  `all`, not blank, not other seasons), the array sizes, the thumb date rule at
  its boundary and for unusable dates, language `00`, rating, the case-
  insensitive `https` upgrade; the mock-server leg returns season 2's four images
  widest first and nothing for another season.
- Provider manager: a season under a series with a `Tvdb` id lists FanArt with
  `Backdrop, Thumb, Banner, Primary` and returns only that season's images.
- Scan (`library_scan_seasons`): with FanArt enabled for seasons the series'
  fanart document is read once more, for the season, by the series' TVDB id;
  unticked for the Season type, that request disappears and the series' own is
  unchanged.
- Production binary, previous baseline vs updated (offline fixture):

  | | Season image fetchers (library options) | Season remote image providers |
  |---|---|---|
  | before | TheMovieDb, TheTVDB | TheMovieDb, TheTVDB |
  | after | TheMovieDb, TheTVDB, **FanArt** | TheMovieDb, **FanArt**, TheTVDB |

  fanart.tv itself is not reachable from the fixture; its responses are covered
  by the mock-server tests above.

#### Limitations

- A season's downloaded image still obeys the library's image limits; the
  default Season options keep only one Primary, so fanart's banner/thumb/backdrop
  appear once an administrator raises those limits.
- The scan's download of a season image goes through the shared acquirer; its
  `https` upgrade means a plain-HTTP mock cannot serve it, so image bytes are
  not asserted end to end here.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on providers passed: providers 915 (4 existing skips), core
2,448, extensions 96, livetv 299, wasm 62, server 235. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first run failed one
providers test that listed a season's advertised image types (now including
fanart.tv's Thumb; updated) and Clippy flagged `fetch_tv_still_images` at 101
lines, so the season arm moved into its own method; providers, the core
season/artwork tests and Clippy were rerun clean afterwards, while the other
package results above were taken before that refactor. Line coverage over each
crate's own sources: **providers 91.48%**, **core 95.11%**, both above the 80%
gate; traits and server composition are coverage-exempt. Instrumented builds
were cleaned right after their numbers were taken, leaving `target` at about
24 GiB with about 494 GiB free.

### Book covers are image providers, not a side effect of the metadata read

Jellyfin extracts a book's cover with two dynamic image providers —
`ComicImageProvider` ("Comic Book Archive Cover Extractor", comic archives) and
`EpubImageProvider` ("EPUB Metadata", `.epub`) — each a registered image
fetcher with its own checkbox and place in the library's image order, and each
run by the image pass. Ferrofin extracted the cover inside the **metadata**
reader (`enrich_book`): neither extractor was registered or offered in the
dashboard, the checkboxes and order did nothing, only the Primary limit gated
it, and an image-only refresh never ran it.

Source oracle: Jellyfin `4910aafa1a`, `MediaBrowser.Providers/Books/
ComicImageProvider.cs` (`Name`, `Supports`, `GetSupportedImages`, `GetImage`),
`OpenPackagingFormat/EpubImageProvider.cs`.

#### Behavior

- **Registered image fetchers for `Book`.** "Comic Book Archive Cover
  Extractor" is new; "EPUB Metadata" (already the EPUB metadata reader) now also
  carries the image-fetcher capability — upstream's EPUB reader and cover
  extractor share the name, so its checkbox gates both. Both advertise
  `Primary`. They default on, like every built-in fetcher, and take part in the
  saved fetcher list and order (default order 50, as plugins without
  `IHasOrder`).
- **Run in the artwork pass.** For a book, the ordered artwork pass tries each
  ticked extractor in the library's order while the item has no Primary and the
  library's Primary limit is above zero. The comic extractor takes `.cb7`,
  `.cbr`, `.cbt` and `.cbz` files; the EPUB extractor takes `.epub`; each only
  its own type. Ticking one does not extract for the other's files.
- **Not tied to the metadata read.** The cover is no longer produced by
  `enrich_book`; the same code now runs for an image-only refresh (metadata
  mode `None`), a locked item is skipped as other dynamic providers are, and a
  stored extraction is reused unless images are being replaced.
- **Unchanged.** Archive types the port cannot open (`.cbr`, `.cb7`, `.cbt` —
  no maintained pure-Rust reader; see `books::READABLE_ARCHIVE_EXTENSIONS`) are
  recognised as comics but yield no cover or embedded metadata; that is a
  separate, older limitation.

#### Verification

- Unit (scan): a `.cbz` is covered with the comic extractor ticked, not with only
  "EPUB Metadata" ticked and not with nothing ticked; the existing Primary-limit
  test (0 / 1) now drives the artwork pass. Registry tests list the two book
  fetchers.
- Production binary, previous baseline vs updated, two comic libraries — one
  with default options, one with every Book image fetcher unticked:

  | | Book image fetchers offered | default library cover | unticked library cover |
  |---|---|---|---|
  | before | none | yes | **yes** (the checkbox did nothing) |
  | after | EPUB Metadata, Comic Book Archive Cover Extractor | yes | **no** |

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on providers passed: providers 917 (4 existing skips), core
2,456, extensions 96, livetv 299, wasm 62, server 237. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
`run_ordered_artwork` at 108 lines; the cover step moved into its own method
and Clippy and the book-cover tests were rerun clean, while the package results
and coverage were taken just before that move. Line coverage over providers'
own sources: **91.54%**, and over core's **95.13%**, both above the
80% gate; traits and server composition are coverage-exempt. Instrumented
builds were cleaned right after their numbers were taken.

### Local NFO readers beyond movies, series and episodes

The scanner's NFO reader now covers the item kinds Jellyfin's NFO providers
cover and searches in their order.

Source oracle: Jellyfin `4910aafa1a`, `MediaBrowser.XbmcMetadata` —
`BaseVideoNfoProvider` (`VideoNfoProvider`, `MusicVideoNfoProvider`,
`MovieNfoProvider`: `MovieNfoSaver.GetMovieSavePaths` + `MovieNfoParser`),
`SeasonNfoProvider`, `SeriesNfoSeasonProvider` / `SeriesNfoSeasonParser`, and
`BaseNfoProvider` (`HasChanged`, `IndexNumber = info.IndexNumber`).

#### Behavior

- **Video and MusicVideo are read.** The dispatch had no arm for them; both
  now use the movie parser, as upstream's providers do.
- **Movie-family search order** follows `GetMovieSavePaths`, which the
  scanner had reversed and simplified: a DVD's `VIDEO_TS/VIDEO_TS.nfo`;
  `movie.nfo` in the containing folder for a *Movie* that is not in a mixed
  folder (owned videos and music videos never read it); then the folder-named
  `.nfo` of a disc structure (not a placeholder), otherwise the file's own
  `.nfo`. Before, a movie file read its own `.nfo` first, so when both existed
  `movie.nfo` lost to `<name>.nfo`.
- **Seasons have a second reader.** After `season.nfo`,
  `SeriesNfoSeasonProvider` names the season from a matching
  `<namedseason number="N">` in `tvshow.nfo` in the season folder, else the
  series folder. The first sidecar that reads wins, so a readable `season.nfo`
  suppresses it. The reader needs the season's number, which upstream seeds on
  the item (`IndexNumber = info.IndexNumber`); the scan now seeds it for every
  kind, deriving a season's number from its folder when not yet set
  (`Season.BeforeMetadataRefresh`).
- **Change monitoring covers every group.** The planner's NFO sidecar fact is
  the newest first-existing file across a kind's provider groups, so a
  `tvshow.nfo` edit alone re-reads its seasons, subject to upstream's
  one-minute NFO save tolerance.

#### Verification

- Unit: the candidate groups for movie, mixed-folder movie, owned video,
  music video, folder movie, DVD, Blu-ray, DVD placeholder, series, episode and
  season.
- Scan (`library_scan_extras`): `movie.nfo` beats `Heat.nfo`; a music video's
  sidecar supplies its title and overview; a season is named from `tvshow.nfo`
  and a readable `season.nfo` wins; editing `tvshow.nfo` renames the season on
  the next scan.
- Production binary, previous baseline vs updated (empty media files, one
  library per kind, NFO reader only):

  | | before | after |
  |---|---|---|
  | movie with `movie.nfo` and `Heat.nfo` | `From Heat.nfo` | `From movie.nfo` |
  | music video with `Song.nfo` | `Artist - Song (2001)` | `Local video title` |
  | season 1 with `<namedseason>` in `tvshow.nfo` | `Season 1` | `First Arc` |

  The scan adds a handful of `stat` calls per item; no timing was measured.

#### Limitations

- The series folder for the season fallback is taken as the season folder's
  parent (upstream asks the library for the parent item's path); they agree
  for the folder layouts the scan resolves.
- `Video` items arise from home-video libraries, which Ferrofin still resolves
  as movies ([Home-video libraries resolve plain videos](#home-video-libraries-resolve-plain-videos)); the Video reader is in place for when that is fixed.
- The upstream tie between the two season providers' registration order is not
  observable in the source; `season.nfo` is tried first.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,432, extensions 96, livetv 296,
wasm 62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The first Clippy run flagged the new 110-line candidate test
(`too_many_lines`); it now carries the allow the repository uses on long
tests, and Clippy was rerun clean afterwards (the test results above predate
only that attribute). Line coverage over core's own sources: **95.09%**, above
the 80% gate; traits and server composition are coverage-exempt. The
instrumented build took `target` to 42.4 GiB, above the 30 GiB cap; its cache
was removed right after the numbers were taken, leaving 23.9 GiB with about
494 GiB free.

### Book local-metadata change monitors

A book's Comic and Open Packaging Format readers now watch their own files,
so editing a sidecar is enough for the next library refresh to re-read it.

Source oracle: Jellyfin `4910aafa1a` — `ComicProvider.HasChanged` (any of the
registered `IComicProvider.HasItemChanged`: `ExternalComicInfoProvider`,
`InternalComicInfoProvider`, `ComicBookInfoProvider`) and
`OpfProvider.HasChanged`, consulted by `MetadataService` as
`IHasItemChangeMonitor` when not every provider is forced to run.

#### Behavior

- Each monitor's test is `file.Exists && mtime > item.DateLastSaved` — strictly
  newer, no tolerance (unlike the NFO provider's one-minute allowance).
  Ferrofin's refresh planner gains a `Book` sidecar kind with that rule.
- `books::book_change_mtime(path, readers)` reports the newest mtime among the
  files the *enabled* readers watch; since any watched file being newer is the
  newest being newer, one value is exactly equivalent to upstream's "any
  monitor fired":
  - **Open Packaging Format**: `<stem>.opf`, else `content.opf`, else
    `metadata.opf` beside the book.
  - **Comic Provider**: the external ComicInfo file (`<stem>.xml`, else
    `ComicInfo.xml`) and, for a `.cbz`, the book file itself (the internal
    ComicInfo and ComicBookInfo readers).
  - **EPUB Metadata** has no monitor, and a disabled reader's file is not
    watched.
- The scanner supplies it for Book items (a book has no NFO), using the
  library's own reader selection (`LocalMetadataReaderOrder` /
  `DisabledLocalMetadataReaders`). A changed monitor makes the plan re-run the
  local readers for that item only; an unchanged book stays untouched.

#### Verification

- Unit: each reader watches only its own file with upstream's candidate order;
  the specific name beats `content.opf`; EPUB-only and Comic-without-sidecar
  report nothing; `.cbz` is watched itself and `.cbr` is not; the planner's
  `Book` rule at +1 s, +1 ms, equal and older.
- Real server (`dashboard_metadata_locale`): after the reader-order checks, an
  edit to the ComicInfo sidecar alone is picked up by a plain library refresh.
- Production binary, previous baseline vs updated, one comic with a
  ComicInfo sidecar and one book with an OPF sidecar, each edited after the
  first scan and followed by a normal library refresh:

  | | comic title after edit | OPF title after edit |
  |---|---|---|
  | before | `First title` (edit ignored) | `Opf first` (edit ignored) |
  | after | `Edited title` | `Opf edited` |

  The added work is a few `stat` calls per Book item per scan; no timing was
  measured.

#### Limitations

- For a book that is a directory, upstream looks for the sidecar inside the
  directory under the directory's name; Ferrofin's existing reader (and so this
  monitor) looks beside it. The reader's behavior is unchanged here.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 896 (4 existing
skips), core 2,428, livetv 296, extensions 96, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled. Line coverage
over each crate's own sources: **providers 91.36%**, **core 95.09%**, both
above the 80% gate; traits and server composition are coverage-exempt. The
instrumented core build took `target` to 42.4 GiB, above the 30 GiB cap; each
coverage cache was removed right after its numbers were taken, leaving
23.9 GiB with about 494 GiB free.

### Custom NFO release-date formats

`XbmcMetadataOptions.ReleaseDateFormat` is any .NET date pattern. Ferrofin
accepted the setting but wrote every date as `yyyy-MM-dd`, read only that one
shape, and its scanner never consulted the saved options at all.

Source oracle: Jellyfin `4910aafa1a` —
`BaseNfoSaver.AddCommonNodes` / `EpisodeNfoSaver` / `ArtistNfoSaver`
(`DateTime.ToString(format, InvariantCulture)`) and
`XmlReaderExtensions.TryReadDateTimeExact` (`DateTime.TryParseExact(…,
InvariantCulture, AssumeUniversal | AdjustToUniversal)`), used by
`BaseNfoParser` for `aired`/`formed`/`premiered`/`releasedate`/`enddate`.

#### Behavior

- **`xbmc::dotnet_date`** reproduces both halves for UTC values: standard
  single-letter formats (`d D f F g G m M o O r R s t T u U y Y`), the empty
  format (`G`), custom patterns (`d f F g h H K m M s t y z`, `:` `/`, quoting
  with `'`/`"`, `\` and `%`), the invariant-culture month/day names, and — for
  reading — field widths, name matching ignoring case, `h`/`H` with `tt`
  consistency, 12-hour handling, `z`/`zz`/`zzz`/`K` offsets (±14:00),
  duplicate fields that must agree, weekday validation, the two-digit-year
  pivot (2049), year range 1–9999, and the defaults for fields a pattern
  omits (today's date; month and day 1 once any date field is given).
- **Writing.** Every release date the NFO savers emit goes through it. A
  malformed pattern throws `FormatException` upstream when a date is written;
  `serialize` now refuses the save in that case (`release_date_format_error`),
  and only when a date would actually be written. Dates aren't touched by the
  separate `dateadded` timestamp.
- **Reading.** `TryReadDateTimeExact` matches the element text as written —
  no trimming, no lenient fallback — so ` 1999-03-31 `, `1999-3-31` and
  `1999-03-31 00:00:00` are not dates under the default pattern, as upstream.
- **The scanner reads the saved options.** `fetch_local_nfo` used
  `NfoConfiguration::default()` regardless of the setting. It now reads
  `named/xbmcmetadata.json` (`ReleaseDateFormat`, `UserId`) per sidecar through
  `NfoConfigurationFile`, which stat-checks the file and re-parses only when it
  changed, so a dashboard edit applies to the next item without a restart.

#### Verification

- **.NET oracle.** A .NET 10 harness produced 1,947 formatting rows (183
  formats × 11 instants, including quoting/escape/exception cases) and 25,166
  parse rows (13,768 + 11,398 generated texts: formatted values, case,
  padding, truncation, offsets, designators, duplicates, weekday mismatches,
  range edges) with `TryParseExact`'s result, including the
  `FormatException` it throws for a lone non-standard character. The Rust
  output matches every row; a 5,214-row subset (all accepted rows and every
  exception) and all formatting rows are checked in under
  `crates/ferrofin-providers/tests/data/` and asserted by unit tests. The
  parse oracle ran on 2026-10-10, the date undetermined fields resolve to, so
  the test fixes that date. `z`/`zz`/`zzz` format the host's local offset
  (as .NET does for a UTC value) and have their own host-independent test.
  Harness, corpora and complete results are under `/tmp/ferrofin-s13/`.
- NFO level (`tests/xbmc_release_date_format.rs`): `dd.MM.yyyy`, `yyyy/M/d`,
  `MMMM d, yyyy`, `yyyyMMdd` written for movie, episode (`aired`) and artist
  (`formed`/`disbanded`) and read back; the default pattern rejects what a
  custom one wrote; padded/short/long dates are rejected; a malformed pattern
  is refused only when a date would be written.
- Scan (`library_scan_extras`): sidecars with `05.12.2010` /
  `December 5, 2010` are dated when the pattern says so and undated when it
  doesn't.
- Real server (`dashboard_metadata_locale`): changing the saved
  `ReleaseDateFormat` rewrites the NFO's `<premiered>` as `31.03.1999` /
  `March 31, 1999`, and an invalid pattern fails the NFO save without losing
  the edit.
- Production binary, previous baseline vs updated:

  | | before | after |
  |---|---|---|
  | `<premiered>` written with `dd.MM.yyyy` | `1999-03-31` | `31.03.1999` |
  | with `MMMM d, yyyy` | `1999-03-31` | `March 31, 1999` |
  | refresh of an NFO with `<premiered>05.12.2010</premiered>` under `dd.MM.yyyy` | no date | `2010-12-05` |
  | scan of 3,000 movies with NFO sidecars (median of 3) | 14.54 s | 14.74 s |

  The scan difference (+1.4 %) is within the run-to-run spread (14.5–15.0 s)
  on a shared host: one `stat` per sidecar. Not a benchmark.

#### Limitations

- A malformed pattern while *reading* yields no date for that element;
  upstream's `TryParseExact` would throw out of the NFO read for a lone
  non-standard character. Writing is faithful (the save is refused).
- Local-time zones for `z`/`zz`/`zzz` follow the host's time-zone database,
  as .NET's do; historical offsets for extreme years can differ between the
  two databases.
- Only UTC values are formatted, which is what Jellyfin stores.
- Live TV recording NFOs (`RecordingsMetadataManager`) also use
  `ReleaseDateFormat` upstream. Ferrofin writes no recording NFO at all (no
  `.nfo` code in `ferrofin-livetv`); that gap is “Save recording NFO and images `SaveRecordingNFO`, `SaveRecordingImages`” (open) in the living review,
  not part of this finding, and will use `dotnet_date` when it is ported.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 890 (4 existing
skips), core 2,428, livetv 296, extensions 96, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled. The first Clippy
run flagged the new module (`manual_is_multiple_of`, `format_push_string`, one
test-only `unnecessary_lazy_evaluations`); those style fixes touch only
padding helpers, were re-verified against the oracle, and the full Clippy and
the providers suite were rerun clean afterwards, while the downstream package
runs and coverage below were taken just before them.

Line coverage over each crate's own sources (nextest, per crate, FFmpeg tests
on): **providers 91.33%**, **core 95.09%**, both above the 80% gate; traits and
server composition are coverage-exempt. The instrumented core build took
`target` to 42.4 GiB, above the 30 GiB cap; each coverage cache was removed
right after its numbers were taken, leaving 23.9 GiB with about 494 GiB free.

### Embedded tags become an audio item's credits

`AudioFileProber.FetchDataFromTags` does more than copy artist names onto the
item: it replaces the item's **people** with the tags' album artists,
performers and composers (and, for an audiobook, authors, narrators and cast),
so the cast/crew list, `/Persons`, and the per-person browse pages know them.
Ferrofin applied the normalised `Artists`/`AlbumArtists` columns but never
wrote the credits, so a scanned track had none.

Source oracle: Jellyfin `4910aafa1a`, `AudioFileProber.FetchDataFromTags`
(`:174-345`), `PeopleHelper.AddPerson` and `ILibraryManager.UpdatePeople`.

#### Behavior

- **A music track** is credited, in this order, with its album artists
  (`AlbumArtist`), performers (`Artist`) and composers (`Composer`). Names are
  the library's own interpretation of the tags — `ALBUMARTISTS`/`ARTISTS` when
  the library prefers the nonstandard tags, custom delimiters and the
  whitelist applied — and the album artists are **not** replaced by the
  performers here (that fallback is only for the stored `AlbumArtists` column).
  `COMPOSER` splits on the tag separator only.
- **An audiobook** is credited with its authors (the album artists, else the
  performers; `Author`), the composers as `Narrator`, and each performer who is
  not an author as `Actor` (cast).
- Blank names are dropped; `update_people` trims and de-duplicates by name and
  type like `AddPerson`.
- The list is written whenever an unlocked audio item is probed, **replacing**
  the stored credits — a track that names nobody clears them, as
  `UpdatePeople(audio, people)` with an empty list does — unless the item's
  `Cast` field is locked. A provider answer that carries credits later in the
  same pass replaces the tags' in turn, as the provider merge does upstream.

#### Verification

- Unit: the credit list for a track (order, blanks, the composer split, no
  album-artist fallback, no tags) and for an audiobook (authors from album
  artists else performers, narrators, cast without the authors).
- Scan (`library_scan_audio_credits`): a track's tags become four credits; after
  the file is "edited" and re-tagged, the next scan probes it again and
  *replaces* them; with `Cast` locked, a re-tag leaves the stored credits.
- Production binary, previous baseline vs updated, a FLAC with
  `ARTIST=Lead`, `ALBUM_ARTIST=The Band`, `COMPOSER=Bach`:

  | | `People` of the scanned track |
  |---|---|
  | before | none |
  | after | **Bach (Composer), Lead (Artist), The Band (AlbumArtist)** |

#### Limitations

- ATL's multi-value joining (the internal separator) is what the ffprobe tag
  path produces for repeated tags; a single ffprobe string such as `A;B` is one
  name unless the library enables custom delimiters, as for the stored artists.
- Each credited name also becomes a browsable `Person` row, as it does for any
  other credit.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,452, extensions 96, livetv 299, wasm
62, server 237. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.12%**, above the 80%
gate; traits and server composition are coverage-exempt. The instrumented
build was cleaned right after the numbers were taken, leaving `target` at
about 24 GiB with about 494 GiB free.

### Generic NFO ordering follows the culture, and duplicates fold Unicode

`BaseNfoSaver` sorts most of its string output with `OrderBy(x => x)`, which
uses `Comparer<string>.Default` — the actual `CurrentCulture` — and drops
duplicate directors and writers with `StringComparer.OrdinalIgnoreCase`.
Ferrofin used Rust's byte-wise `cmp` for the ordering and ASCII-only equality
for the duplicates, so non-ASCII names were ordered differently from the same
item saved by Jellyfin and `Élodie`/`élodie` stayed two directors.

Source oracle: Jellyfin `4910aafa1a`, `BaseNfoSaver` (`:508-534`, `:537`,
`:680-705`, `:755-769`, `:929-932`), `MovieNfoSaver` and `AlbumNfoSaver`
artists, `GetTagForProviderKey`; review note
`/tmp/ferrofin-n02-independent-culture-review/S28-generic-nfo-ordering.md`.

#### Behavior

Every generic ordering now uses the culture collation of the current culture
(the same comparer [Save image paths in NFO (`SaveImagePathsInNfo`)](#save-image-paths-in-nfo-saveimagepathsinnfo) uses for backdrop paths), stable like `OrderBy`:

- actors, by `SortOrder ?? 0` and then the trimmed name;
- directors, and writers/credits, trimmed, de-duplicated, then ordered;
- trailers, ordered by the trimmed original URL before the output rewrite;
- countries, genres, studios and tags (or styles for music kinds);
- music-video and album artists and album artists;
- the remaining custom provider-id tags, by provider key.

Also:

- Director/writer de-duplication is `OrdinalIgnoreCase`: characters are
  upper-cased one-to-one (`é`/`É`, `ж`/`Ж` equal; `ß` and `SS` not, as .NET's
  `ToUpperInvariant`; a precomposed `é` and `e`+combining accent are not).
- The custom provider tag is `ToLowerInvariant()` of the key, so a non-ASCII key
  (`Äpple` → `äppleid`) is lower-cased like any other letter; the ASCII-only
  rule left `<Äppleid>`.
- The comparer is built once per culture per thread; an unusable culture falls
  back to byte order instead of failing the save.

#### Verification

- Unit: a movie serialized under `en-US` and under `sv` puts genres, studios,
  tags, countries, directors, trailers, custom provider tags and actors in
  opposite relative order for `Ä…` versus `Z…` (English sorts `Ä` beside `A`,
  Swedish after `Z`); the `OrdinalIgnoreCase` equalities and non-equalities and
  de-duplication keeping the first spelling; the provider-tag lower-casing.
- Production binary, previous baseline vs updated, a movie whose genres are set
  to `Zebra`, `Ärlig`, `Apa` by `POST /Items/{id}` under two `Accept-Language`s,
  read back from the saved `movie.nfo`:

  | request culture | before | after |
  |---|---|---|
  | `en-US` | Apa, Zebra, Ärlig | Apa, **Ärlig, Zebra** |
  | `sv` | Apa, Zebra, Ärlig | Apa, Zebra, Ärlig |

#### Limitations

- A box set's `<collectionitem>` list (`BaseNfoSaver.cs:808-825`) is part of the
  container-folder work (“Collection/playlist local metadata execution” (open)) and is not written yet.
- Comparisons use the collation tables bundled with the build, as the rest of
  the culture seam does; a culture .NET knows and the tables do not falls back
  to byte order.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on providers passed: providers 917 (4 existing skips), core
2,448, extensions 96, livetv 299, wasm 62, server 237. Real FFmpeg tests
enabled; the WASM guest build remains disabled. Line coverage over providers'
own sources: **91.54%**, above the 80% gate; only providers changed, and
traits and server composition are coverage-exempt. The instrumented build was
cleaned right after the numbers were taken, leaving `target` at about 24 GiB
with about 494 GiB free.

### The metadata refresh queue

`ProviderManager.QueueRefresh` is a serial, priority-ordered queue.
Ferrofin's `queue_refresh` ignored the priority and spawned a task per
request, so a burst of refreshes ran concurrently in no defined order, each
under whatever culture its own requester carried.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Manager/ProviderManager.cs` — `QueueRefresh`,
`StartProcessingRefreshQueue`, `GetRefreshQueue`; `RefreshPriority`
(`High = 0`, `Normal = 1`, `Low = 2`).

#### Behavior

- **One worker, priority order.** Requests join a queue ordered by priority
  (`High` first) and drained by one worker, one refresh at a time. Equal
  priorities run in arrival order; .NET's `PriorityQueue` leaves that order
  unspecified, so arrival order is one valid choice.
- **First-starter context.** The worker is started by the first request that
  finds none running, so the whole drain, including refreshes queued later by
  other callers, runs under the first requester's culture, as upstream's
  `Task.Run` captures it.
- **Start/stand-down under one lock.** The worker takes the next item and
  stands down under the same lock that requests use to join, so a request
  queued as the queue runs dry starts a new worker rather than waiting on one
  that has stopped. A panicking refresh stands the worker down too (a drop
  guard), so the queue cannot be wedged; items still waiting start with the
  next request.
- **Failures keep draining.** A failed refresh is logged and the next one runs.
- An empty item id is rejected (`ArgumentException`), and `get_refresh_queue`
  returns the ids waiting (previously always empty), without the one being
  refreshed, as upstream's `GetRefreshQueue`.

#### Verification

- Four requests (Low, Low, High, Normal) enqueued before the worker's first
  turn run as High, Normal, Low, Low in arrival order, while `get_refresh_queue`
  lists all four waiting and is empty after the drain.
- An empty id is `InvalidInput`; after the queue drains and the worker stands
  down, the next request starts a new worker.
- Three requests under cultures `sv`, `de`, `fr` (the first refresh fails)
  all run under `sv`, and the failure does not stop the others.
- Production binary, previous baseline vs updated, 300 movies, 300 full
  refresh requests in a row: all `204`, accepted in 0.61 s before and 0.65 s
  after, and the queue drained in about the same time (62.3 s before, 61.1 s
  after — dominated by the polling used to detect completion, so a sanity
  check, not a benchmark). Ordering and context are not observable over HTTP
  because every route queues at `High`; they are covered by the tests above.

#### Limitations

- No `Dispose` seam exists on the provider manager, so upstream's
  cancel-on-dispose path has nothing to attach to; shutdown ends the process.
- `MusicArtist` items use `RefreshArtist` upstream; Ferrofin's refresh already
  routes every item through `refresh_full_item`, unchanged here.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on providers passed: providers 899 (4 existing skips),
core 2,432, livetv 296, extensions 96, wasm 62, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first run exposed an
older test that queued a nil id and expected success (upstream refuses it; the
test now queues a real one) and a Clippy `missing_fields_in_debug` on the new
field; the providers suite, Clippy and coverage were rerun on the fixed tree,
while the downstream package results were taken before those test-only and
`Debug` edits. Line coverage over providers' own sources: **91.40%**, above the
80% gate; traits and server composition are coverage-exempt. The instrumented
build took `target` to 28.3 GiB; its cache was removed after the numbers,
leaving 24.0 GiB with about 494 GiB free.

## Artwork

### Embedded images from videos

The "Embedded Image Extractor" image fetcher was advertised for videos but only
worked for audio files. A movie's own file can now supply its Primary,
Backdrop and Logo.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/MediaInfo/EmbeddedImageProvider.cs` (`Supports`,
`GetSupportedImages`, `GetImage`, `GetEmbeddedImage`) and
`MediaEncoder.ExtractVideoImage(…, imageStream, imageStreamIndex, targetFormat)`.

#### Behavior

- **Types.** An episode offers Primary only; a movie, video, music video or
  trailer offers Primary, Backdrop and Logo. Each runs when the item lacks that
  image and the library's image limit for the type is above zero.
- **Source choice.** An attachment whose file name contains a fragment
  (case-insensitive) wins; otherwise the first embedded-image stream whose
  comment contains one; a Primary with no labelled match takes the first
  embedded-image stream. Fragments — Primary: `poster folder cover default
  movie show`; Backdrop: `backdrop background art`; Logo: `logo`.
- **Format.** An attachment's comes from its MIME type (else its file
  extension), a stream's from its codec (`bmp`, `gif`, `png`, `webp`; anything
  else, including `mjpeg`, is `jpg`).
- **Eligibility.** No `.strm` shortcut, non-file protocol, placeholder or DVD
  structure.
- **Extraction.** New `MediaEncoder::extract_image_stream` maps the stream by
  index (`-map 0:N`, one frame) into the temp directory in the chosen format,
  trying the I-frame path first and the standard one second, as
  `ExtractImage` does. The default implementation reports it unsupported so
  frame-only encoders and fakes are unaffected. The picture is stored as
  `{metadata}/library/{id}/{primary|backdrop|logo}.{ext}`; an existing file is
  reused unless images are being replaced.
- **Attachments from the same pass.** The scan persists attachments after the
  artwork step, so the probe that just ran hands its attachment rows to the
  extractor directly; a refresh without a probe reads the stored rows.
- **Checkbox.** Unticking "Embedded Image Extractor" for the type skips all of
  this.

#### Verification

- Unit: the source choice (attachment over stream, MIME over extension, codec
  formats, Primary fallback, no fallback for other types, empty names) and the
  per-kind type list.
- Scan (`library_scan_video_embedded_images`): a movie probed with a "Cover"
  stream, a "Backdrop art" stream and a `Studio-LOGO.png` attachment gets
  `primary.jpg`, `backdrop.png` and `logo.png` with the right bytes and the
  right stream indexes; an unchanged rescan extracts nothing; a library whose
  image fetchers omit the extractor gets nothing.
- Encoder: the I-frame attempt failing then the standard one succeeding, the
  failure message carrying ffmpeg's tail, and no stray temp files.
- Real ffmpeg (`FERROFIN_FFMPEG_TESTS=1`): a Matroska file with a PNG
  attachment yields a valid 32×24 PNG for its stream index; a non-picture
  stream errors and leaves nothing behind. ffmpeg 9 rejects `-vsync`, so the
  test supplies the probed version as the server does.
- Production binary, previous baseline vs updated, a real MKV with three PNG
  attachments (`cover`, `backdrop`, `logo`), one library with defaults and one
  with the extractor unticked:

  | | default library | extractor unticked |
  |---|---|---|
  | before | no images | no images |
  | after | **Primary, Logo, 1 Backdrop** | no images |

- Scan cost, 300 empty movies, three runs each (debug builds): 1.84/1.95/1.93 s
  before, 1.95/1.83/2.01 s after — no difference beyond noise. Items with no
  embedded pictures add only the lookups above.

#### Limitations

- Stored width, height and blur hash are filled by the image pass that
  follows, as for other local images.
- A Blu-ray or other non-DVD disc structure is treated like a file, as
  upstream's `Supports` does.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: mediaencoding 952, hls 117, core
2,441, extensions 96, livetv 296, wasm 62, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
two over-long functions and later runs a file-extension comparison and a
similar-names lint in new tests; the extraction was split into helpers
(`embedded_image_eligible`, `stored_media_source`,
`extract_video_embedded_images`), the lints fixed and Clippy rerun clean, with
the new core and encoder tests rerun. The package and coverage results above
were taken just before those refactors. Line coverage over each crate's own
sources: **mediaencoding 94.20%**, **core 95.08%**, both above the 80% gate;
traits and server composition are coverage-exempt. The instrumented core build
took `target` to 42.7 GiB, above the 30 GiB cap; each coverage cache was
removed right after its numbers were taken, leaving 24.0 GiB with about
494 GiB free.

### Indexed artwork deletion

Deleting `/Items/{id}/Images/Backdrop/1` previously removed every backdrop and
returned all of their paths for deletion. It now removes only the selected slot.
The remaining rows keep their identities and insertion order, with subsequent
slots compacting naturally. The same behavior applies to screenshots and other
image types. An omitted index selects zero; negative and missing indexes remove no stored images. Selection, deletion and collection of the removed path use one atomic
writer statement, avoiding a separate read/delete race.

The source oracle is Jellyfin `4910aafa1a`,
`MediaBrowser.Controller/Entities/BaseItem.cs`: `GetImageInfo` uses
`GetImages(type).ElementAtOrDefault(index)` and `DeleteImageAsync` removes only
that image. The existing provider service retains its shared album artwork
ownership rule.

Regression coverage checks descending GUIDs versus insertion order, middle and
last slots, default and invalid indexes, row identity, other image types and
other items. Real HTTP regressions upload backdrops and screenshots, delete a
middle slot, check the surviving files and serve them through the image routes.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary
check, persistence/virtual-folder tests, production build and real HTTP regression
passed. Disposable binary comparisons reproduced the old whole-type deletion
and verified the fixed middle/default/missing-index behavior against Jellyfin
12.1.0. Every owned server was reaped. Response bodies, file facts, timings,
binary hashes and the earlier failed fixture are retained under
`/tmp/ferrofin-extra-two/s14/` and `target/dashboard-test-tmp/`.

The native reference uses backdrops. Its unindexed screenshot uploads replace
one image, while Ferrofin appends; that independent upload gap remains **[Screenshot upload replacement](#screenshot-upload-replacement)**.
Stored screenshot-slot deletion is covered by persistence and HTTP regressions.
Shared-host timings are observations, not publishable benchmark results.
Artwork mutation repository/save notifications and missing-slot NFO saver
suppression remain the independent source-audit finding **[Artwork mutation metadata lifecycle](#artwork-mutation-metadata-lifecycle)**.
The final shared workspace and fresh core coverage gates passed for this
two-finding batch; their measured results are recorded in
[the [Filesystem-root library shortcuts](#filesystem-root-library-shortcuts) review](#filesystem-root-library-shortcuts).

### Artwork replacement cleanup failures

Replacing artwork removes the file it replaced, and a failure to do so is no
longer swallowed.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Manager/ImageSaver.cs` `SaveImage` (the "Delete the
current path" block). Upstream sets the new path on the item, then deletes the
previous local/internal file unless any saved path equals it under
`OrdinalIgnoreCase`; it forgives only `FileNotFoundException`, so any other
failure escapes to the controller *before* its repository save.

#### Behavior

- **Failures surface.** `ImageFileSaver::save` now returns the published
  image together with `cleanup` (the failed removal of the replaced file).
  The upload path persists the new image first, then returns the error: the
  client gets a `500`, the replacement stays stored and served, the stuck file
  stays, and the item is not re-saved (no Etag change, no update
  notification), as in upstream where the exception precedes
  `UpdateToRepositoryAsync`. Scans and refreshes (`save_all`) store the images
  and report the failure through their existing failure channel.
- **Only "not found" is forgiven**, in the media-folder path and the
  internal-metadata path alike. The duplicate `warn!` is gone; the error is
  returned and logged once at the outer layer.
- **Case-insensitive saved-path comparison.** A previous path equal to a
  saved path ignoring case is kept (`savedPaths.Contains(…, OrdinalIgnoreCase)`),
  where the old comparison was exact.
- **All image types, not only backdrops.** The earlier internal cleanup was
  specific to backdrops; upstream applies the same rule to every type whose
  file lives under the internal metadata path. It now covers every type, with
  the existing ownership guards kept: the file must be inside this item's
  own internal folder, not the shared album artwork, referenced by one slot,
  and not under a symlinked parent. No arbitrary registered path is deleted.

#### Verification

- Unit: `copy_images` reports an undeletable previous file after the new image
  is written and the staged source removed, forgives an absent one, and keeps a
  case-alias; the internal-file cleanup returns its failure and forgives a
  missing file.
- Real server (`dashboard_metadata_locale`): a Primary replacement whose
  previous file is an undeletable directory returns `500`, leaves the
  replacement stored as a file and the stuck file in place. The existing
  destination-fallback check now keeps its Primary in internal storage first,
  because replacing a file that cannot be deleted fails the upload; the case it
  tests (an unwritable *destination*) is unchanged.
- Production binary, previous baseline vs updated:

  | | before | after |
  |---|---|---|
  | replace Primary (png → jpeg) | 204, old file removed | 204, old file removed |
  | replace Primary when the stored file is undeletable | **204** (error hidden) | **500**, new image persisted and served, stuck file kept |

  The normal path is unchanged, so no latency effect is expected; none was
  measured.

#### Limitations

- Upstream's case-insensitive path test also applies on case-sensitive file
  systems, as ported; two files differing only in case are therefore treated
  as one.
- A failed cleanup leaves the Etag untouched while the stored image has
  changed, matching upstream's observable state (its in-memory item changes,
  the repository save does not run).

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 882 (4 existing
skips), core 2,423, livetv 296, extensions 96, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled.

Line coverage over each crate's own sources (nextest, per crate, FFmpeg tests
on): **providers 91.20%**, **core 95.07%**, both above the 80% gate. The
instrumented core build took `target` to 39.9 GiB, above the 30 GiB cap; each
coverage cache was removed after its numbers were taken, leaving 21.5 GiB with
about 495 GiB free. A first core coverage run overlapped with unrelated
in-progress work and was discarded; the numbers above are from a clean rerun of
exactly the committed tree.

### Indexed image uploads

Jellyfin's indexed upload route accepts the URL index but passes `null` to
`ProviderManager.SaveImage`. Backdrops therefore append even when the URL names
an existing slot; single images replace their only slot. Ferrofin passed the
index through, replacing backdrops and allowing arbitrary slot selection.

The route now saves without an index, retaining the existing `i32` URL binding,
admin authorization, item lookup, content-type and base64 validation. Internal
indexed saves and indexed deletion remain available to their existing callers.
Primary source: `4910aafa1a`, `Jellyfin.Api/Controllers/ImageController.cs`,
`SetItemImageByIndex`; the full controller capture is retained under
`/tmp/ferrofin-s37/ImageController.cs`.

Handler regressions capture the provider arguments for Backdrop and Primary,
including minimum/maximum integers, negative values and existing slots. Invalid
and overflowing indices return 400 without saving. The production HTTP fixture
checks append ordering and surviving files. [Extra thumbnail duplication (`EnableExtraThumbsDuplication`)](#extra-thumbnail-duplication-enableextrathumbsduplication)'s extrathumbs and [Artwork saved beside media](#artwork-saved-beside-media)'s destination fixtures now
normalize the starting backdrop count through real DELETE requests before each
configuration phase, so it tests the requested destination with append semantics.

The native oracle uses distinct PNG colors and checks catalog paths and served
body hashes, comparing the previous production binary, Jellyfin 12.1.0 and the
updated production binary. It exercises existing, zero, negative and maximum
indices, invalid binding, single-image replacement and preserved backdrop order.
Screenshot multiplicity ([Screenshot upload replacement](#screenshot-upload-replacement)) and mutation lifecycle ([Artwork mutation metadata lifecycle](#artwork-mutation-metadata-lifecycle)) remain separate open
findings.

The first dashboard HTTP attempt exposed [Artwork saved beside media](#artwork-saved-beside-media)'s older replacement assumption;
its failed log remains in `/tmp/ferrofin-s37/s37/http-001.log`. Controlling the
initial catalog corrected the fixture, and the complete regression suite passed.
The server matrix regenerated the executable after the initial native check;
the final native check was rerun against the preserved binary. That matching
check, source hashes and profile hashes passed the final evidence audit.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary,
production build, handler regressions, the real HTTP integration fixture and
all three native checks passed. The complete source-qualified workspace matrix
passed **8,412 tests** across 21 packages and
201 default targets. API and server were rerun; all other
packages retain source-identical independent results from the preceding passing
matrix, verified across all dependency kinds. Real FFmpeg tests were enabled;
five existing DB/provider skips remain visible and WASM guest tests were
separately disabled explicitly.

Fresh unseeded **ferrofin-api line coverage: 86.45%**, above the
separate 80% gate. Server composition is coverage-exempt under CLAUDE.md.

Ten identical `Primary/0` PNG uploads per production binary yielded median local
request times of **9.039 ms before** and
**8.176 ms after** (Jellyfin: 22.512 ms).
These are bounded observations on a shared host, not a controlled benchmark;
primary replacement keeps the timing workload constant while distinct-color
backdrop checks prove the changed behavior separately.

Logs, exact source/binary/profile hashes and retained native results are indexed
in `/tmp/ferrofin-s37/quality-proof.json`. Only generated build caches were
cleaned; the worktree, commits, production binary, native datasets and profiles
remain. Validation checkpoint: target **12.84 GiB**, host free
**509.79 GiB**. Baseline advancement follows the separate commit.

### Screenshot upload replacement

Screenshots are single images in pinned Jellyfin `4910aafa1a`:
`BaseItem.AllowsMultipleImages` permits only Backdrop and Chapter. Ferrofin's
manual upload manager and artwork row writer treated Screenshot as appendable,
and its DTO image-tag loop incorrectly excluded screenshots.

Screenshot uploads now replace their single persisted image, use the unnumbered
`screenshot` filename, and emit Screenshot in ImageTags and blur hashes.
Internal indexed artwork writes also replace screenshots. Indexed HTTP upload
continues to ignore its route index, as completed in [Indexed image uploads](#indexed-image-uploads). Screenshot swap
rejection was already implemented and is verified alongside a backdrop control.

A real database regression checks explicit internal screenshot indices,
including usize::MAX; an existing DTO integration test now checks screenshot
tags and blur hashes. Filename regressions use the pinned single-image policy.
The dashboard HTTP fixture checks unindexed and indexed screenshot replacement,
stable filenames, existing files, DTO tags and swap rejection. Its [Indexed artwork deletion](#indexed-artwork-deletion) multiple
upload/deletion scenario now uses Backdrop; [Indexed artwork deletion](#indexed-artwork-deletion)'s database test still verifies
deleting individual slots from preexisting duplicate screenshot rows.

Native before/Jellyfin/after checks upload distinct PNG colors and verify counts,
paths and served image hashes. Source captures and raw checks are retained in
`/tmp/ferrofin-s39/`. Broader replacement cleanup and mutation lifecycle remain
[Artwork replacement cleanup failures](#artwork-replacement-cleanup-failures)/S40; this finding does not close those separate reviews.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary,
production build, targeted regressions, the real HTTP integration fixture and
all three native checks passed. The complete source-qualified workspace matrix
passed **8,413 tests** across 21 packages and
201 default targets. Providers, core and every dependent were rerun; all other
packages retain source-identical independent results from the preceding passing
matrix, verified across all dependency kinds. The closure includes Live TV,
extensions and WASM through their normal/dev dependencies; API tests remain
independent through the traits seam. Real FFmpeg tests were enabled;
five existing DB/provider skips remain visible and WASM guest tests were
separately disabled explicitly.

Fresh unseeded line coverage: **providers 91.98%**,
**core 95.40%**, each above its separate 80% gate. Server composition is coverage-exempt under CLAUDE.md.

Ten identical unindexed Screenshot PNG uploads per production binary yielded median local
request times of **8.564 ms before** and
**8.191 ms after** (Jellyfin: 16.966 ms).
These are bounded observations on a shared host, not a controlled benchmark;
the previous implementation grows its catalog while the corrected one replaces
a single image. Distinct-color delivery assertions prove correctness separately.

Logs, exact source/binary/profile hashes and retained native results are indexed
in `/tmp/ferrofin-s39/quality-proof.json`. Generated test executables and the coverage build cache were cleaned.
The small ordinary compiler cache is retained under the 30 GiB cap for subsequent
findings; the worktree, commits, production binary, native datasets and profiles
remain. Validation checkpoint: target **20.61 GiB**, host free
**507.43 GiB**. Baseline advancement follows the separate commit.

### Artwork mutation metadata lifecycle

Artwork changes now perform the repository save Jellyfin performs, and
nothing happens when there was nothing to change.

Source oracle: Jellyfin `4910aafa1a` — `BaseItem.DeleteImageAsync`,
`BaseItem.SwapImagesAsync`, `ImageController.PostSetItemImage*`,
`LibraryManager.UpdateItemsAsync`, `ImageInfo.IsLocalFile`.

#### Behavior

- **Repository save after a change.** Upload, remote download, delete and
  reorder end with `BaseItem.UpdateToRepositoryAsync(ImageUpdate)`: the metadata
  savers run, the item's `DateLastSaved` is re-stamped (the DTO `Etag` is
  derived from it) and `ItemUpdated` reaches the library-changed notifier.
  New `ProviderManager::update_to_repository` and
  `ItemPersistenceService::touch_item_updated` carry this; the one targeted
  UPDATE avoids rewriting the whole row.
- **Missing slot is "nothing to do".** `delete_image` returns before the
  saver/save when no row was removed. Reorder now reports whether it swapped
  (`swap_item_images`/`swap_images` return `bool`) and the handler saves only
  then.
- **Remote artwork is not swapped** (`!IsLocalFile` on either side), and a
  slot swapped with itself is an ordinary swap that resets dimensions, as
  upstream. Previously the same-index case returned early.
- The remote-download handler no longer calls the saver a second time; the
  provider manager's save path already performs the lifecycle once.

#### Verification

- Core: `touch_item_updated` stamps the row, announces exactly one
  `ItemsUpdated` entry and ignores a missing row; swap returns
  `true`/`false` for real, out-of-range, same-slot and remote-path cases.
- Providers: `delete_image` stamps only after a real removal;
  `update_to_repository` stamps through the store and tolerates a manager
  without one.
- API: reorder saves the item only after a real swap.
- Real server (`dashboard_metadata_locale`): uploads move the item's Etag; a
  missing-slot delete leaves Etag and images untouched; a real delete moves it.
- Production binary, previous baseline vs updated, same fixture:

  | Operation | Etag moved before | Etag moved after |
  |---|---|---|
  | upload | no | yes |
  | delete missing slot | no | no |
  | reorder missing slot | no | no |
  | reorder real slot | no | yes |
  | delete real slot | no | yes |

  Median of ten Primary uploads: **7.0 ms before, 9.9 ms after** (an extra
  UPDATE and one row read for the notifier). Sample ranges overlap
  (5.6–16.2 ms after vs 6.2–17.0 ms before) and the host is shared, so this
  is an observation, not a benchmark.

#### Limitations

- Reorder still exchanges stored rows rather than swapping the two files on
  disk as upstream does. That is separate from this lifecycle finding and is
  tracked as [Backdrop reorder exchanges the files](#backdrop-reorder-exchanges-the-files) in the living review.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package that depends on a changed crate passed: traits 76, providers 878
(4 existing skips), core 2,422, api 929, drawing 73, mediaencoding 949,
hls 117, livetv 296, extensions 96, wasm 62, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run failed on
a similar-names lint in a new test; it was renamed and the rerun is clean.
Line coverage over each crate's own sources (nextest, per crate, FFmpeg tests
on): **providers 91.19%**, **core 95.07%**, **api 87.81%**, all above the 80%
gate; traits and server composition are coverage-exempt. The instrumented
builds took `target` to 39.8 GiB (core) and 33.9 GiB (api), above the 30 GiB
cap; each coverage cache was removed right after its numbers were taken,
leaving 21.4 GiB with about 495 GiB free. Worktree, history, production binary,
native datasets and canonical profiles were untouched.

### Backdrop reorder exchanges the files

Reordering backdrops swapped the two rows' *paths*. Jellyfin swaps the two
image *files'* contents and leaves each row's path alone, so the files on disk
stay in slot order and the rows keep pointing at their own names.

Found while closing [Artwork mutation metadata lifecycle](#artwork-mutation-metadata-lifecycle); the source is Jellyfin `4910aafa1a`,
`BaseItem.SwapImagesAsync` and `ManagedFileSystem.SwapFiles`
(`ImageController.UpdateItemImageIndex` calls the former).

#### Behavior

- `swap_item_images` exchanges the contents of the two files through a
  temporary copy of the first (`SwapFiles`: copy the first aside, copy the
  second over the first, move the copy over the second). A failure part-way
  restores the first file and removes the copy; the rows are not touched.
- Each swapped row keeps its path and has its cached width and height cleared,
  its blur hash cleared (it described the old content; upstream leaves it
  stale) and its modified time re-read from its file, so the image tag moves
  and clients refetch.
- A missing file (the row points nowhere) fails the request, as `File.Copy`
  throws; nothing changes.
- Unchanged: an out-of-range slot or a remote (`http…`) path on either side is
  "nothing to do", and the item is saved afterwards only when files were
  swapped ([Artwork mutation metadata lifecycle](#artwork-mutation-metadata-lifecycle)).
- A slot swapped with itself, or two rows naming one file, is now a no-op that
  reports "nothing swapped". Upstream's copy onto the same file throws an
  `IOException` there, which is not worth reproducing; it also supersedes the
  [Artwork mutation metadata lifecycle](#artwork-mutation-metadata-lifecycle) note that such a swap was treated "like any other pair".

#### Verification

- Repository: three backdrops swapped 0↔2 trade contents while their rows keep
  their paths, no temporary file remains, the swapped rows lose size and blur
  hash and gain a fresh modified time and the middle one is untouched;
  out-of-range, negative and same-slot requests change nothing; a row whose
  file is gone fails with the other file and both rows unchanged; remote
  paths are ignored. The library manager's swap test checks the same through
  the manager.
- Real server (`dashboard_metadata_locale`): two backdrops with different
  bytes are uploaded and reordered over HTTP; the listed paths are unchanged,
  the files trade contents, and the bytes served at the first index are the
  second image's.
- Production binary, previous baseline vs updated, a red and a blue backdrop
  swapped:

  | | row paths after the swap | file at slot 0 / 1 | served slot 0 / 1 |
  |---|---|---|---|
  | before | exchanged | blue / red | blue / red |
  | after | **unchanged** | blue / red | blue / red |

  What a client sees is the same; what is on disk and in the item's image list
  now matches Jellyfin (a later delete or replace of a slot touches that slot's
  own file).

#### Limitations

- Swapping files with different extensions leaves each path's extension
  unchanged, as upstream does.
- The blur hash is cleared where upstream leaves a stale one; this is a
  deliberate correction.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,437, extensions 96, livetv 296,
wasm 62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The trait change is documentation only. The first Clippy run flagged
two style lints in the new code (`or_else` returning `Err`, a
`map().unwrap_or_else()`); they were fixed and Clippy rerun clean with the swap
tests, while the downstream package results and coverage were taken just
before. Line coverage over core's own sources: **95.10%**, above the 80% gate;
traits and server composition are coverage-exempt. The instrumented build took
`target` to 42.6 GiB, above the 30 GiB cap; its cache was removed right after
the numbers were taken, leaving 23.9 GiB with about 494 GiB free.

## Subtitles and chapters

### Subtitle provider namespaces

Remote subtitle candidate ids and provider descriptor ids now use Jellyfin's
namespace: the .NET `name.ToLowerInvariant().GetMD5().ToString("N")` of the
provider name (`opensubtitles` → `29c0c3633fefe8ab4d5b9af1b0275f3d`).

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Subtitles/SubtitleManager.cs`
(`Normalize`, `GetProviderId`, `GetProvider`, `GetRemoteSubtitles`,
`GetSupportedProviders`).

#### Behavior

- Providers return their own local ids. The manager prefixes each search
  candidate with `{md5}_` using the candidate's `ProviderName` (falling back to
  the provider's name), exactly as `Normalize` does. The OpenSubtitles adapter
  no longer pre-prefixes its ids with a readable name.
- `GetSupportedProviders` advertises the same MD5 id.
- Download and raw-content routing split on the first `_` only
  (`Split('_', 2)`), so later underscores stay in the local id; an id with no
  underscore is both provider id and local id (`parts[0]`/`parts[^1]`). The
  readable-name prefix is no longer an alias and is rejected.
- Provider filters and fetcher order remain name-based, as upstream
  (`DisabledSubtitleFetchers`, `SubtitleFetcherOrder` compare `Name`).

The hash reuses the existing `ferrofin_common::extensions::get_md5` (.NET
UTF-16LE MD5 wrapped in a `Guid`); no new dependency or SQL.

#### Verification

- Core regressions: known hash for `opensubtitles` computed independently
  from the .NET Guid layout, case-insensitive naming, descriptor id, search→
  download round trip, readable-name rejection, later-underscore and
  no-underscore routing. Existing provider-order/fallback/filter tests now
  assert namespaced ids.
- Real server integration (`dashboard_metadata_locale`): a manual search over
  the real router, manager and OpenSubtitles adapter returns an id with the
  MD5 namespace; `/Providers/Subtitles/Subtitles/{id}` serves it (200) and the
  old `OpenSubtitles_{local}` form is 400. The automatic-download path in the
  same test exercises the namespaced id end to end.
- Production binary (previous baseline vs updated), ten manual searches on a
  scanned movie, median **2.53 ms → 2.42 ms**. Limitation: the production
  binary has no OpenSubtitles endpoint override and no credentials in a clean
  data directory, so these samples measure the item/search path with an
  unconfigured provider and return no candidates; hashed/readable/bare
  download ids all return 400 there for that reason. Namespacing itself is
  verified by the integration test above. Shared-host timings are
  observations, not benchmarks.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 876 (4 existing
skips), core 2,420, livetv 296, extensions 96, wasm 62, server 230. Real
FFmpeg tests enabled; the WASM guest build remains disabled. Independent
packages (api, db, model, traits, hls, mediaencoding, …) do not depend on the
changed crates and keep their previous passing results. Separate line
coverage over each crate's own sources (nextest, per-crate, FFmpeg tests on):
**providers 91.16%**, **core 95.07%**, both above the 80% gate. Traits and
server composition are coverage-exempt. The instrumented build briefly took
`target` to 39.5 GiB, above the 30 GiB cap; it was removed immediately after
the numbers were taken (coverage cache only), leaving 21.0 GiB with about
510 GiB free. Worktree, history, production binary, native datasets and
canonical profiles were untouched.

### Saved item metadata in subtitle searches

Subtitle searches now forward the item's complete saved ProviderIds map,
episode IndexNumberEnd and runtime to registered providers. The existing
OpenSubtitles REST adapter also receives the saved IMDb ID, resolving its key
case-insensitively. Saved metadata replaces stale caller values; absent IDs,
absent/invalid ranges and non-episodic items clear the corresponding values.
IDs come from the existing database repository read, with no new manager SQL.

The source oracle is Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Subtitles/SubtitleManager.cs`, the overload creating
a SubtitleSearchRequest from Video and Episode. A .NET harness calls the
local Jellyfin 12.1.0 managed manager with a capturing provider. It verifies
all three ID keys/values, episodes 2–3, season 1, runtime 42,000,000 and an
unchanged perfect-match flag, plus a movie without episode range/runtime.
The runtime constructor includes its additional directory-service argument;
that does not change the request mapping under review. Sources, failed harness
build attempts and corrected observed output remain under
`/tmp/ferrofin-followup-two/subtitle-oracle/` and `reference-proof.json`.

The real Rust manager regression seeds the actual store, captures the provider
request, checks the same range/IDs/runtime, preserves a custom provider key,
clears stale metadata, handles malformed/overflowing range data and excludes
movie ranges. The provider's non-hash candidate must remain rejected when
perfect matching is enabled. The real HTTP integration fixture then checks
that saved IMDb metadata reaches the OpenSubtitles request with both perfect
matching enabled and disabled, while existing download behavior still works.
The built-in REST provider's query continues to use its existing single episode
parameter; providers receive the full range through the shared request.
[Subtitle provider namespaces](#subtitle-provider-namespaces)'s separate provider namespace contract remains open.

A disposable production-binary fixture measures ten subtitle-search requests
before and after the change, with a real scanned video and saved IDs. Its
provider is unconfigured and returns no candidates: these samples measure the
item/search path, not a provider response or download. Provider mapping is
verified independently by the capturing-manager and offline HTTP tests.
All owned native fixture servers are reaped. Shared-host request timings are
observations, not publishable benchmark results.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary,
production build, targeted regressions, the real HTTP integration fixture and
native checks passed. The source-qualified workspace matrix passed **8,411
tests** across 21 packages and 201 default test targets.
Changed packages and every dependent were rerun; source-identical independent
packages retain their prior passing results after source/dependency checks.
Real FFmpeg tests were enabled. Five existing DB/provider skips remain visible;
the WASM guest build was separately disabled explicitly.

Fresh unseeded **ferrofin-core line coverage is 95.40%**, above
its separate 80% gate. Traits and server wiring are coverage-exempt under
CLAUDE.md. These final gates cover [Localization dropdown resource catalog](#localization-dropdown-resource-catalog) and this finding together. Tested source files,
embedded assets, final native binary and canonical profile hashes are checked
in `/tmp/ferrofin-followup-two/quality-proof.json`. Complete logs and profiles
remain under `/tmp/ferrofin-followup-two/`.

Median elapsed subtitle-search time across ten local requests: previous binary
3.361 ms, updated binary 2.341 ms.
The unconfigured-provider limitation above applies. Final localization request
median was 0.793 ms; complete catalogs match Jellyfin.

Only generated build artifacts were cleaned. The worktree, commits, production
binary, native datasets and canonical profiles remain intact. At this validation
checkpoint, target usage is **11.78 GiB**, with
**510.38 GiB free** on the host.

### Blank and time-formatted chapter names

A video's chapter names are normalized the way Jellyfin normalizes them: a
blank name, or one that .NET's `TimeSpan.TryParse` accepts, is replaced by the
localized `ChapterNameValue` template numbered from 1. Ripping tools often
leave exactly those two forms.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/MediaInfo/FFProbeVideoInfo.cs` `NormalizeChapterNames`
(called after the optional dummy-chapter generation, for every video
refreshed in Default/Full mode).

#### Behavior

- `ferrofin_providers::chapter_names::normalize_chapter_names` renames each
  chapter whose name is missing, whitespace-only, or accepted by
  `is_time_span`. The scan applies it to probed and dummy chapters alike; the
  dummy-only naming it replaces is the same rule over a subset.
- `is_time_span` reproduces `TimeSpan.TryParse`'s accept/reject set rather
  than approximating it with a pattern: surrounding Unicode whitespace and one
  leading `-` (no space after it, no `+`); `d`, `h:m`, `h:m:s[.f]`, `d.h:m`,
  `d.h:m:s[.f]`, `d:h:m:s[.f]`; three colon-separated numbers with a first
  number above 23 read as `d:h:m`; an empty seconds field before a fraction
  (`1:02:.5`); hours ≤ 23, minutes and seconds ≤ 59, at most seven fraction
  digits, and a total within signed 64-bit ticks (a negative one tick further).
  A bare number is a day count, so `12` is a time span and `Chapter 01` is not.
  ASCII digits only; full-width and Arabic-Indic digits are rejected, as in
  .NET.

#### Verification

- **.NET oracle.** A .NET 10 harness runs `TimeSpan.TryParse` over three
  generated corpora (22,822 + 661 + 148 strings: sign/whitespace/digit-count/
  separator/range/overflow combinations, Unicode whitespace and digits, tick
  limits, the `1:02:.5` family). The Rust function was fitted against it and
  matches every row; a 11,545-row subset (all non-trivial shapes) is checked in
  as `tests/data/time_span_oracle.json` and asserted by a unit test. The
  harness, corpora and complete results are under `/tmp/ferrofin-s26/`.
- Unit test of the rename rule (`None`, empty, whitespace, `00:12:34`,
  fractional time, real names, a bare number, `Chapter 01`).
- Scan regression: probed chapters with the same names persist as
  `Chapitre 1…` under a French UI culture; `Opening` survives.
- Production binary, previous baseline vs updated, on an mkv with eight
  chapters titled (none), empty, blanks, `00:05:00`, `Opening`, `12`,
  `1.02:03:04.5`, `Chapter 01`:

  | | names over HTTP (`/Items/{id}?fields=Chapters`) |
  |---|---|
  | before | `null`, ``, `   `, `00:05:00`, `Opening`, `12`, `1.02:03:04.5`, `Chapter 01` |
  | after | `Chapter 1`, `Chapter 2`, `Chapter 3`, `Chapter 4`, `Opening`, `Chapter 6`, `Chapter 7`, `Chapter 01` |

  This path adds one linear pass over a video's chapters, so no latency
  measurement was taken; the scan's own timings are unchanged in order of
  magnitude.

#### Limitations

- The accept set is the invariant/en-US grammar. `TimeSpan.TryParse` uses the
  ambient culture, so a culture with other separators would accept different
  strings; that ambient-culture behavior is the open [Ambient UI culture vs configured server culture](#ambient-ui-culture-vs-configured-server-culture)/S34 divergence and is
  not claimed here.
- Only the scan's chapter fold is changed; chapters from other sources go
  through the same fold.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: providers 880 (4 existing skips),
core 2,423, livetv 296, extensions 96, wasm 62, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
`is_time_span` at 103 lines (`too_many_lines`); it was restructured into a
field-extraction helper and re-verified against the full 23k-row oracle, then
the rerun was clean.

Line coverage over each crate's own sources (nextest, per crate, FFmpeg tests
on): **providers 91.23%**, **core 95.07%**, both above the 80% gate. The new
file alone is at 79.8% of lines; the unreached lines are defensive rejections
of impossible digit groups, and the gate is per crate. The instrumented core
build took `target` to 39.9 GiB, above the 30 GiB cap; coverage caches were
removed after each crate's numbers, leaving 21.5 GiB with about 495 GiB free.

## Playback and encoding

### Remote `.strm` shortcut playback

A `.strm` library item stands for a remote stream. Ferrofin treated it as a
local file: its source was the `.strm` path over the file protocol with no
streams, playback info reported nothing playable, and a static stream served
the *text of the `.strm` file* to the player.

Source oracle: Jellyfin `4910aafa1a`: `BaseVideoResolver`/`AudioResolver`
(`IsShortcut`), `ProbeProvider.FetchShortcutInfo` (`ShortcutPath`),
`FFProbeVideoInfo`/`AudioFileProber` (`!IsShortcut || EnableRemoteContentProbe`),
`BaseItem.GetVersionInfo` (remote shortcut arm; local targets refused),
`MediaSourceManager.GetPlaybackMediaSources` (forced remote-probe refresh),
`MediaSourceManager.GetPathProtocol`/`SupportsDirectStream`,
`VideosController` + `FileStreamResponseHelpers.GetStaticRemoteStreamResult`
(static remote relay; other protocols `400`).

#### Behavior

- **Scan** (`library_scan.rs`, new `shortcut.rs`): a `.strm` video or audio item
  gets `IsShortcut: true` in its `Data`; when its probe runs the file is read
  and its first line that is not blank or a `#` comment becomes `ShortcutPath`
  — only when it is an `http`, `https`, `rtsp` or `rtp` URL (anything else is
  logged and ignored, as is a file with no usable line; the item keeps the last
  target it had). A scan never probes a shortcut (the ffprobe request is
  dropped), so first scans of `.strm` items no longer run ffprobe on the
  `.strm` file.
- **Remote probe**: `MetadataRefreshOptions.enable_remote_content_probe`
  (`EnableRemoteContentProbe`). A refresh with it probes the target the `.strm`
  names (request path = the URL, protocol from the URL; external sidecars still
  resolve beside the `.strm`) and stores streams, runtime, container and
  chapters like any probe.
- **Media sources** (`media_source_manager.rs`): a video with
  `IsShortcut`+`ShortcutPath` whose target protocol is not `File` is served from
  the target — `Path` = the URL, `Protocol` Http/Rtsp/Rtp, `IsRemote` true, the
  probed container — and `SupportsDirectStream` is re-derived from
  path+protocol for every source (an `.m3u8` target is not direct-streamable).
  A target on the file protocol is never followed. DTO list sources use the same
  builder.
- **Playback** (`GetPlaybackMediaSources`): with a probe allowed, a `.strm`, or a
  video/audio item whose first source lacks a stream of its main type, is
  refreshed `FullRefresh` + remote probe — awaited, through the scanner's
  path-scoped scan, wired in the composition root — and its sources re-read.
- **Delivery** (`streaming.rs`): a static stream of an HTTP(S) source is relayed
  from the target (client `Range` forwarded; upstream status,
  `Accept-Ranges` (`none` when absent), `Content-Range`, `Content-Length` and
  `Content-Type` returned, `application/octet-stream` by default); any other
  non-file protocol is `400 Input protocol … cannot be streamed statically`; an
  unreachable origin is a server error. Transcoding already reads the source's
  path/protocol from the same media source.

#### Verification

- Core: scan tests (remote/local/empty/FTP targets: `IsShortcut` always, target
  only when remote, no probe, unchanged files keep it; remote probe reads the
  URL with the Http protocol and not the file, stores a stream and the
  container, picks up a changed target, keeps the last target when the file
  stops naming a remote one, and a non-remote shortcut is never probed even with
  the option); media-source tests (remote target, playlist target, local target
  and stray `ShortcutPath`); playback tests (refresh only with a probe allowed
  and only for a `.strm` or a stream-less video, then sources re-read);
  `shortcut.rs` unit tests (line selection, scheme gate, protocol by prefix).
- API: a loopback origin with `Range` — whole body (`200`, `Accept-Ranges`,
  length), `bytes=2-5` (`206`, `Content-Range: bytes 2-5/10`), RTSP `400`, a
  dead origin `500`.
- Production binary, one `.strm` naming a local origin serving an MP4 and one
  naming `/etc/hostname`, PlaybackInfo then `GET /Videos/{id}/stream?static=true`:

  | | remote `.strm` source | static stream | local-path `.strm` |
  |---|---|---|---|
  | before | local `.strm` path, `File`, no streams, not direct streamable | **the text of the `.strm`** | `.strm` file |
  | after | `http://…/origin.mp4`, `Http`, `IsRemote`, Video+Audio streams, direct stream | **the origin's bytes**, origin hit twice (probe, relay) | unchanged: not followed, no probe |

#### Limitations

- Media segments, trickplay and chapter-image extraction already skip
  shortcuts (their own eligibility checks); no change there.
- A forced remote probe re-runs the full metadata refresh of the item (as
  upstream's `FullRefresh` does), so a playback of a stream-less item can reach
  the configured metadata providers.
- Per-source `RequiredHttpHeaders`/User-Agent forwarding for the relay has no
  source of headers for a `.strm`, so none is sent.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the changed crates passed: core 2,484, api 954, providers
935, extensions 96, livetv 299, wasm 917 (4 skipped), server 237, mediaencoding
205 (real FFmpeg tests enabled; the WASM guest build remains disabled). The first
full run failed the SQL-boundary ratchet because the new media-source tests used
raw SQL; they were rewritten over the shared test helpers, and fmt, Clippy, the
boundary test and core's tests and doctests were rerun clean. Separate line
coverage over each changed crate's own sources: core **95.15%**, api **87.88%**,
providers **91.53%**, all above the 80% gate; traits and the server composition
root (the `RemoteProbe` wiring, exercised by the live run above) are
coverage-exempt. Instrumented builds were cleaned after each crate (`target`
peaked near 30 GiB during the api coverage build and returned to about 25 GiB,
with about 494 GiB free).

### Audio-specific device-profile negotiation

Playback negotiation always ran the *video* stream builder. A music track
therefore went through the video decision (HLS pin, `/videos/{id}/master.m3u8`
URL, video-container rules) even when the device directly played its format,
and a cover-art "video" stream of an audio file did not help.

Source oracle: Jellyfin `4910aafa1a`: `MediaInfoHelper.SetDeviceSpecificData`
(`item.MediaType == Audio ? GetOptimalAudioStream : GetOptimalVideoStream`, and
the shared capability/URL block), `StreamBuilder.GetOptimalAudioStream`,
`GetAudioDirectPlayProfile`, `IsAudioContainerSupported`,
`IsAudioDirectPlaySupported`/`IsAudioDirectStreamSupported`,
`GetTranscodeReasonsFromDirectPlayProfile`.

#### Behavior

- `ferrofin-model` `StreamBuilder::get_optimal_audio_stream` (+ the audio
  direct-play helpers), a faithful port:
  - forced direct play / direct stream short-circuit;
  - the default audio stream (an item with none offers nothing — upstream
    throws there);
  - a direct-play profile that names the container *and* the audio codec, with
    the Matroska/WebM rule (`mkv,webm` is not played by a `webm`-only device);
  - failing that, a profile whose container is not the item's but whose audio
    codec (or bare container name) is — a **direct stream**, reason
    `ContainerNotSupported`; a bitrate over the limit adds
    `ContainerBitrateExceedsLimit`; with no profile, reasons come from the
    profile list;
  - direct play also needs the codec conditions to pass
    (`GetCompatibilityAudioCodec`: channels, bitrate, sample rate, bit depth);
  - a direct stream remuxes into `ts` (or the profile's `ts`/`mp4`), refused
    over HLS when the profile's codec is not one HLS carries
    (`ts`: aac/ac3/eac3/mp3; `mp4` adds alac/flac/opus/dts/truehd);
  - otherwise the first audio transcoding profile the transcoder can encode to:
    its container/protocol, the codec conditions, `GlobalMaxAudioChannels`, and
    the bitrate `min(configured, requested ?? music-streaming bitrate ?? 128 k)`
    capped by what the source condition left.
- The playback-info handler (`media_info.rs`) uses it for items whose media type
  is `Audio`: direct play stays direct play; anything else becomes an audio
  transcode at `/audio/{id}/stream.{container}` (or `master.m3u8` when the
  profile says HLS) with the profile's sub-protocol and container, `Allow*Copy`
  vetoes appended, `SupportsDirectPlay/DirectStream` cleared. There is no HLS
  pin for audio. A source that cannot transcode (permission or client veto)
  yields no decision, exactly as upstream's `null` leaves the static source.

#### Verification

- Model: eleven cases over music-client profiles (direct play, unsupported
  format → mp3 transcode with the music bitrate, bitrate over limit, requested
  bitrate capped by the limit, codec-condition failure, container remux, HLS
  codec restriction in `ts` vs `mp4`, Matroska/WebM, forced methods, no audio
  stream / no permitted transcode, profile the transcoder cannot encode).
- API handler test: direct play with cover art stays direct; FLAC transcodes at
  `/audio/…/stream.mp3` with the codec, container and sub-protocol; copy vetoes;
  permission and client vetoes produce no URL.
- Production binary, a FLAC (with embedded cover art) and an MP3 under a profile
  that direct plays MP3 and transcodes to MP3, PlaybackInfo then a GET of the
  returned URL:

  | | FLAC track | MP3 track |
  |---|---|---|
  | before | `/videos/…/master.m3u8` (HLS video master), `ts` | **same HLS URL, direct play denied** |
  | after | **`/audio/…/stream.mp3`, http**; the URL serves 49 KB of MP3 (ffprobe `mp3`) | **direct play, no transcode** |

#### Limitations

- An audio item with no probed audio stream gets no negotiation (upstream
  throws); it keeps its static source.
- `DirectStream` for audio is reported as a transcode URL because the server
  keeps `EnableDirectStream` off (upstream's "direct-stream http streaming is
  currently broken" rule); the `/audio` route's remux itself is unchanged.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the model crate passed: model 992, api 936, core 2,484,
providers 917 (4 skipped), mediaencoding 954, livetv 299, server 237, hls 117,
extensions 96, wasm 62; real FFmpeg tests enabled, the WASM guest
build disabled. Two lints and an old assertion changed along the way: the new
`clone_into` and an over-long helper (split into `optimal_stream`), and
`negotiated_capabilities_respect_every_playback_permission_combination`, which
had pinned the video builder's behavior for audio (an audio source that cannot
transcode now gets no decision and keeps the static source's flags, as
upstream's `null`); after those, fmt, Clippy, the boundary test and the model and
api tests and doctests were rerun clean. Separate line coverage over each changed
crate's own sources: model **89.76%**, api **87.95%**, both above the 80% gate.
Instrumented builds were cleaned after each crate (`target` about 25 GiB).

### Encoder selection honours the saved path, and ffprobe is found like Jellyfin finds it

`MediaEncoder.SetFFmpegPath` chooses ffmpeg from the command line or
environment, then the encoding configuration's saved `EncoderAppPath`, then
`$PATH`. Ferrofin's bootstrap had the middle rung written but never wired —
startup always passed "no saved path" — so an adopted Jellyfin server whose
`encoding.xml` named a custom ffmpeg silently ran whatever was on `$PATH`.
Its ffprobe derivation also differed from upstream's.

Source oracle: Jellyfin `4910aafa1a`, `MediaEncoder.SetFFmpegPath`
(`MediaEncoder.cs:180-238`) and `FfprobePathRegex`; the existing evidence note
is `brain/knowledge/DASHBOARD_SETTINGS_S38_ENCODER_BOOTSTRAP.md`.

#### Behavior

- **Selection order** is now `--ffmpeg` / `FERROFIN_FFMPEG_PATH` → the saved
  `EncoderAppPath` → `ffmpeg` on `$PATH`. The saved value is read before
  discovery from `{config}/users/named/encoding.json` (the live store) or, on a
  server Ferrofin has not yet saved for, the adopted Jellyfin
  `{config}/encoding.xml`. A missing, blank, malformed or null value is "not
  configured"; `EncoderAppPathDisplay` is never mistaken for the selector. The
  failure message and log label name the encoding configuration.
- **ffprobe derivation** is `FfprobePathRegex`: the whole final path component
  becomes `ffprobe` and only a trailing `.ext` survives, `/` and `\` both ending
  a directory — `jellyfin-ffmpeg` and `ffmpeg-static` → `ffprobe`, `ffmpeg.exe`
  → `ffprobe.exe`, `ffmpeg-6.1` → `ffprobe.1`. The old rule replaced the
  substring `ffmpeg` inside the name (`jellyfin-ffprobe`, which does not exist
  beside a `jellyfin-ffmpeg`).
- **Unchanged, and a recorded difference.** A host without a usable ffmpeg
  still boots with transcoding disabled (documented in the repository guide),
  where Jellyfin aborts startup with "Failed to find valid ffmpeg"; Ferrofin
  also still validates ffprobe's version where Jellyfin's check is nonfatal, and
  has no `FFmpeg:novalidation` bypass. Whether to match those startup rules is
  the owner's call (the guide lists the accepted divergences), so this finding is closed
  for selection and derivation only; the lifecycle rules above remain an open
  decision.

#### Verification

- Unit: the derivation for every shape above, including Windows separators and
  dotted names; the saved path from JSON, from XML (entities decoded, the
  display field ignored, self-closing and blank = unconfigured), JSON winning over
  an XML that names another, and every unusable JSON shape.
- Production binary, previous baseline vs updated, started without `--ffmpeg`
  with `EncoderAppPath` saved to a symlink named `my-ffmpeg` (its sibling
  `ffprobe` beside it):

  | | encoder in use (`EncoderAppPathDisplay`) |
  |---|---|
  | before | `/usr/bin/ffmpeg` — the saved path ignored |
  | after | **the saved `…/tools/my-ffmpeg`**, ffprobe found beside it |

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for the server
package passed: server 237. Real FFmpeg tests enabled; the WASM guest build
remains disabled. The server is exempt from the coverage gate (composition
root); the selection and derivation logic it contains is tested above. Only the
server crate changed.

## Browsing and ordering

### Artist DTO links name real artist items

`DtoService` builds a track's or album's `ArtistItems` and `AlbumArtists`
pairs only for names that match an actual `MusicArtist` item, and leaves the
others out. Ferrofin built a pair for every stored name — from the
`ItemValues` row when no artist item existed — so a performer's link pointed
at an id no `/Items/{id}` request could resolve, and the shape of the lists
(an empty array for an untagged item) differed from Jellyfin's.

Source oracle: Jellyfin `4910aafa1a`, `DtoService.cs:1335-1402`
(`GetArtists` → `LinkedChildrenService.FindArtists`, a clean-name match on
`MusicArtist` rows), `ArtistsValidator.Run` (one artist item per name in
`GetAllArtistNames()`, performers included).

#### Behavior

- **Pairs.** `ArtistItems` and `AlbumArtists` contain the distinct, non-blank
  stored names that match a `MusicArtist` item by clean name, each with that
  item's id (the first by sort name when several match). A name with no item is
  omitted — never given an `ItemValues` id. `Artists` stays the full stored list
  and `AlbumArtist` its first name.
- **Shape.** Every kind that carries artist fields always emits the arrays,
  empty when untagged (`[]`, never absent). A music video carries `Artists` and
  `ArtistItems` only (`IHasArtist`); it has no album artists.
- **Items for every artist.** Scans materialised a browsable `MusicArtist` row
  only for album artists, leaving performers as filter-only values — so linking
  strictly to items would have dropped every performer's link. They now
  materialise one `MusicArtist` row per distinct artist name (the existing
  type-and-clean-name guard keeps a name that is both to one row), as
  `ArtistsValidator` does. `/Artists` therefore lists performers, which it could
  not before.
- The lookup is no longer routed through the `ItemValues` rows: it reads
  `MusicArtist` rows by clean name directly, so an item without a stored value
  still links.

#### Verification

- Unit: pairs for a track whose stored artists repeat a name and include an
  unknown one (distinct, linked only, with the artist item's id, the plain list
  untouched, `AlbumArtist` the first stored name); an untagged track's empty
  arrays and null album artist; a music video's artists with no album artists;
  the end-to-end clean-name resolution for awkward names now links to the
  `MusicArtist` item rather than an `ItemValues` id; the materialiser makes one
  row for each of a performer, an album artist and a name that is both.
- Production binary, previous baseline vs updated, a FLAC by a guest performer
  on a band's album:

  | | performer's `ArtistItems` id → `GET /Items/{id}` | `/Artists` total |
  |---|---|---|
  | before | **404** (an `ItemValues` id) | 0 |
  | after | **200** | 1 |

  The album artist's link resolved both before and after.

#### Limitations

- Jellyfin refreshes each new artist item's metadata in the artists validator;
  Ferrofin's by-name artist pass already refreshes the artists never refreshed,
  so performers now take part in it (extra provider lookups on a scan, as there).
- The `ItemValues`-based fallback ids of earlier databases remain stored; they
  are simply no longer used for these two lists.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,453, extensions 96, livetv 299, wasm
62, server 237. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The first run tripped the SQL boundary ratchet (the artist lookup and
four test updates had put new `sqlx::query` calls in `dto_service.rs`) and a
Clippy type-complexity lint; the lookup moved into `item_repository.rs`, the
tests seed through `test_support` helpers, and the ratchet, Clippy and the full
core suite were rerun clean, while the dependent packages and coverage were
taken before that refactor. Line coverage over core's own sources: **95.13%**,
above the 80% gate; traits and server composition are coverage-exempt. The
instrumented build was cleaned right after the numbers were taken, leaving
`target` at about 24 GiB with about 494 GiB free.

### The user's saved view order (`OrderedViews`)

This finding collects several independent ordering gaps. This is the first slice that
closes — the one a user sees on the home screen: `OrderedViews`, the library
order a user saves in their display settings.

`GetUserViews` ends with the user's own order (`UserViewManager.cs:141-158`):
a view whose id is in `OrderedViews` takes that position, a user view that
stands for a library takes its `DisplayParentId`'s, every other view follows,
and ties keep the name order. Ferrofin kept the preference (it round-trips
through `/Users/Configuration`) but never applied it: the home screen and
`/UserViews` ignored the order a user saved.

#### Behavior

- `FerrofinUserViewManager::get_user_views` (and the `IncludeHidden` form) stable
  sorts its views by the user's saved `OrderedViews`: a view by its own id, else
  by its `DisplayParentId` (read from the view's `Data`), else after every named
  view, all ties staying in the name order the list already has. Ids that name no
  view are ignored; views the user hid (`MyMediaExcludes`) are removed first, as
  upstream. With nothing saved nothing changes.

#### Verification

- Core test: four libraries, no saved order (name order); `OrderedViews` naming
  two of them plus an id that is no view (the two first in that order, the rest by
  name, the stray id ignored; the `IncludeHidden` list ordered the same); then one
  of the named views hidden through `MyMediaExcludes` (gone, the rest still in
  order).
- Production binary, three libraries, then `POST /Users/Configuration` with
  `OrderedViews` = [Music, Movies], `GET /UserViews`:

  | | order |
  |---|---|
  | before | `B Shows, C Music, A Movies` (the order does not change) |
  | after | **`C Music, A Movies, B Shows`** |

  (The default order is by sort name: "A Movies" sorts as `movies`, the
  leading article being dropped, as Jellyfin does.)

#### What this finding still has open

The other independent gaps stay on this finding: the in-memory `GetResult` comparers for
null/date defaults, localized ratings, Studio numeric collation and user-data
sorts, BoxSet `DisplayOrder` pre-sort, and generated-key transliteration and
Unicode padding.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,485, livetv 299, server 237, extensions
96, wasm 62; real FFmpeg tests enabled, the WASM guest build disabled. Line
coverage over core's own sources: **95.15%**, above the 80% gate; the server
composition root is coverage-exempt. The instrumented build was cleaned
(`target` about 25 GiB, about 494 GiB free).

### Recursive browse ordering — `Studio`, `Artist`, `AlbumArtist`, `IsPlayed`

A second slice of [The user's saved view order (`OrderedViews`)](#the-users-saved-view-order-orderedviews). With a Jellyfin 12.2 oracle available, every `SortBy` key
was compared on the same library (six movies with and without premiere date,
rating, certificate, runtime, studio, critic rating; two played, one favorited),
ascending and descending, for a **recursive** listing (the repository query,
`OrderMapper`) and a **direct** listing of the library (the in-memory
`LibraryManager.Sort`).

Recursive listings: every key matched except four, now fixed. Direct listings:
see "What [The user's saved view order (`OrderedViews`)](#the-users-saved-view-order-orderedviews) still has open".

#### Behavior

- `Studio`, `Artist` and `AlbumArtist` order by the item's first `ItemValues`
  entry of that type by its clean text (`OrderMapper.cs:61-66`), an item with none
  first — Ferrofin had fallen back to `SortName` for all three (the code carried
  it as an open work item). The comparison is ordinal on the clean text, so
  `alpha studio 10` precedes `alpha studio 2`, as Jellyfin's query does.
- `IsPlayed`/`IsUnplayed` read a missing `UserData` row as not played
  (`COALESCE(…, 0)`, as `IsFavoriteOrLiked` already did): an item the user has no
  row for ties with one whose row says unplayed and lists by name, instead of
  sorting ahead of it as a SQL `NULL` does.

#### Verification

- Core: five movies with studios (`Zed`; `Zed`+`Alpha Studio 10`; none;
  `Alpha Studio 2`; `Beta`) and artists, ordered by `Studio` both ways and by
  `Artist`; an unplayed row, a missing row and two played rows ordered by
  `IsPlayed`/`IsUnplayed` both ways.
- Against Jellyfin 12.2, the same 18 keys × 2 directions, **recursive**: all 36
  listings now identical (before: `Studio` ×2 and `IsPlayed`/`IsUnplayed` ×4
  differed).

#### What [The user's saved view order (`OrderedViews`)](#the-users-saved-view-order-orderedviews) still has open

- *Direct* listings of a library (the in-memory comparers): `Studio` compares the
  first studio with numeric ordering (`alpha studio 2` before `…10`, via
  `CompareOptions.NumericOrdering`), `OfficialRating` descending keeps equal
  scores in their original order (the oracle lists the unrated `Charlie` before the
  `G`-rated `Delta`; Ferrofin reverses that tie), and generated-key
  transliteration/Unicode padding are not yet ported.
- The oracle's direct listings **ignore** `PlayCount`, `IsPlayed`, `IsUnplayed`,
  `IsFavoriteOrLiked` and `DatePlayed` (the order stays by name in both
  directions) although the comparers exist in the pinned source — an upstream
  defect as far as can be told. Ferrofin sorts by them. Whether to reproduce a
  no-op is the owner's decision, not made here.
- Per-user `OrderedViews` is “Complete” (open).

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,488, api 937, livetv 299, server 237,
extensions 96, wasm 62; real FFmpeg tests enabled, the WASM guest build
disabled. Clippy first flagged the extended play-state test as over-long (one
user and library, every order — it carries an allow); fmt, Clippy, the boundary
test and core's tests and doctests were rerun clean after. Line coverage over
core's own sources: **95.16%**, above the 80% gate; the server composition root
is coverage-exempt. The instrumented build was cleaned (`target` about 25 GiB).

### A box set's `DisplayOrder`

`BoxSet.GetChildren` sorts the set's direct members by its `DisplayOrder`
(`BoxSet.cs:116-132`), and its constructor makes the default `PremiereDate`.
Ferrofin listed a set's members by name whatever the set said, returned no
`DisplayOrder` for a set that had not stored one, and — found while verifying —
**ignored the `DisplayOrder` the metadata editor posts** for a box set or a
series (the update route never wrote it, so the editor's "Display order" select
did nothing; a series' change also never queued the refresh the editor expects).

Oracle: Jellyfin 12.2 (portable build) on the same media: a set created from
Zeta (2001), Alpha (2003), Mid (1999), Beta (2002), in that order.

#### Behavior

- A direct listing of a box set with no `SortBy` is ordered by the set's
  `DisplayOrder`: `Enum.TryParse<ItemSortBy>` of the stored text (a name,
  case-sensitively, or a number) ascending; a set that stored none, and any text
  that is not an `ItemSortBy`, order by `PremiereDate`; `Default` lists the
  members in the order they were added (the container's linked-children order).
  An order the request names (`SortBy=…`) wins, as upstream. A recursive listing is
  the repository query and ignores the set's order — measured on the oracle, not
  assumed.
- The DTO of a box set carries `DisplayOrder`: the stored text, `PremiereDate`
  for a set that stored none (the constructor's default).
- `POST /Items/{id}` writes `DisplayOrder` for a series and a box set (as
  `IHasDisplayOrder.DisplayOrder = request.DisplayOrder`, a null clearing it) and
  for no other kind, and a **series** whose order changed (case-insensitively, null
  as empty) queues the `FullRefresh`/`ReplaceAllMetadata` refresh upstream queues so
  its episodes are renumbered under the new order (`ItemUpdateController.cs:
  83-86,120-132`); a box set's change queues none.
- The item-refresh route and the update route share one `queue_item_refresh`.

#### Verification

- Core: one set listed under no stored order (premiere order; request `SortName`
  wins), `SortName`, a numeric `10`, unparseable texts (`premieredate`, empty,
  `Nonsense` → premiere order) and `Default` (added order), and a recursive listing
  (name order throughout).
- API: the stored text for a box set and a series (and its clearing), no
  `DisplayOrder` on a movie, and the series refresh only when the order really
  changed — case-only, null-vs-null and box-set changes queue none.
- Against Jellyfin 12.2, the same probe (`GET /Users/{u}/Items?parentId=<set>`):

  | | Jellyfin 12.2 | Ferrofin before | Ferrofin after |
  |---|---|---|---|
  | no `SortBy` | `Mid, Zeta, Beta, Alpha` | `Alpha, Beta, Mid, Zeta` | `Mid, Zeta, Beta, Alpha` |
  | `SortBy=SortName` / `…Descending` | by name / reversed | same | same |
  | DTO `DisplayOrder` | `PremiereDate` | absent | `PremiereDate` |
  | after posting `SortName` | name order | name order (not stored) | name order |
  | after posting `Default` | `Zeta, Alpha, Mid, Beta` | name order | `Zeta, Alpha, Mid, Beta` |
  | recursive, no `SortBy` | name order | name order | name order |

- Music, same probe (one artist, two albums): the listing of the artist, of each
  album and their recursive forms are identical to Jellyfin's already, so the
  music half of this finding has nothing to port.

#### Limitations

- A set's listing sorts in SQL (`PremiereDate`, `SortName`, …); Jellyfin sorts
  in memory with the comparers the [The user's saved view order (`OrderedViews`)](#the-users-saved-view-order-orderedviews) finding tracks, so ties and null dates
  inherit whatever [The user's saved view order (`OrderedViews`)](#the-users-saved-view-order-orderedviews) still differs on.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the changed crates passed: core 2,487, api 937, mediaencoding
954, providers 926 (4 skipped), livetv 299, server 237, hls 117, extensions 96,
wasm 62; real FFmpeg tests enabled, the WASM guest build disabled. Clippy first
flagged an over-long folder-display function and a helper placed after the test
module; the box-set ordering condition moved into its helper and the helper
before the module, then fmt, Clippy, the boundary test and core's and api's tests
and doctests were rerun clean. Separate line coverage over each changed crate's
own sources, measured before that move: core **95.16%**, api **88.02%**, both
above the 80% gate; the traits crate (one internal query field) and the server
composition root are coverage-exempt. Instrumented builds were cleaned
(`target` about 25 GiB).

## Localization

### Localization dropdown resource catalog

`GET /Localization/Options` now exposes all 105 bundled core translations,
including `lt-LT` and the 35 resources missing from the previous list. Values
come from the resource catalog; labels use .NET native culture names, with
explicit `English` for `en-US`, and sort with ordinal-ignore-case semantics.
The old web labels and 71-entry ordering no longer hide supported languages.

The source oracle is Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/Localization/LocalizationManager.cs`,
`BuildLocalizationData` and `GetDisplayName`. The vendored label snapshot
was generated with .NET 10.0.401 on the resource codes already bundled here:
replace underscores with hyphens for CultureInfo lookup, use NativeName,
fall back to the code for an unknown culture, add English and sort. The
complete snapshot equals the local Jellyfin 12.1.0 HTTP response, including
novelty `pr`, underscore cultures, Lithuanian and non-Latin label ordering.
Regenerate the labels when updating the pinned core resource catalog; the
unit regression requires exactly one option per resource and checks the full
native order. This dropdown fix does not close [Ambient UI culture vs configured server culture](#ambient-ui-culture-vs-configured-server-culture)'s separate ambient-culture
lookup finding.

The disposable HTTP fixture reproduced 71 options and `lt` on the previous
production binary. The reference and updated production binary must return
the complete 105-option snapshot. Ten requests per binary retain response
bodies, hashes and elapsed times under `/tmp/ferrofin-followup-two/s30/`.
Shared-host timings are observations, not publishable benchmark results.
The first targeted run caught an incorrect test fingerprint (`af` first);
the captured full catalog correctly starts with `ab`. Its corrected rerun
and the original failure remain in the evidence directory.

Final shared workspace and fresh core coverage gates passed for both findings;
measured results are recorded in [the [Saved item metadata in subtitle searches](#saved-item-metadata-in-subtitle-searches) review](#saved-item-metadata-in-subtitle-searches).

The corrected targeted run passed 75 tests, and the production build and
three native phases passed. Median request elapsed time across ten local
requests: previous Ferrofin 0.791 ms, updated Ferrofin
0.657 ms, reference Jellyfin 6.928 ms.
These samples include HTTP overhead and are not isolated performance claims.

### Ambient UI culture vs configured server culture

Jellyfin has two localized-string lookups. `GetLocalizedString(phrase)` reads
the ambient `CultureInfo.CurrentUICulture` (what `UseRequestLocalization` set
for the request, inherited by work the request starts);
`GetServerLocalizedString(phrase)` reads the configured `UICulture`. Ferrofin
had one lookup that always used the configured culture, so a request's
`Accept-Language`/`culture` never reached chapter names, stream labels or task
text, and nothing could tell the two kinds of caller apart.

Source oracle: Jellyfin `4910aafa1a`, `LocalizationManager.cs:594–603`
(`GetLocalizedString` → `CurrentUICulture.Name`; `GetServerLocalizedString` →
`Configuration.UICulture`), `Startup.cs:133–149` (request localization sets
both `CurrentCulture` and `CurrentUICulture`), consumers:
`FFProbeVideoInfo` `ChapterNameValue`, `ProbeResultNormalizer` and
`MediaStreamRepository` labels, scheduled-task names (ambient);
`UserViewManager` "Folders", `LibraryManager` extra names, `CollectionManager`
"Collections", `SeasonResolver` (server). Evidence note
`brain/knowledge/DASHBOARD_SETTINGS_S33_S34_AMBIENT_CULTURE.md`.

#### Behavior

- `ferrofin_util::current_culture` gains an ambient UI culture beside the
  ambient culture: `scope_with_ui(culture, ui, future)` sets both, `scope`
  (used when work is spawned from a request) inherits the creator's UI culture,
  both restore on every poll and on panic, and `ambient_ui_culture()` reads it
  (`None` outside any request).
- The request-localization middleware sets both from the selected request
  culture (query → cookie → `Accept-Language`, with the startup default for an
  unmatched side), as the pinned `UseRequestLocalization` does.
- `LocalizationManager::get_localized_string` uses the ambient UI culture and,
  with none (startup, scheduled work not started from a request), the configured
  culture like an empty `CurrentUICulture.Name`; the new trait method
  `get_server_localized_string` always uses the configured culture. Library
  "Folders" and extra names ("Trailer", "Trailer 2") now use the server lookup.
- Unchanged and matching upstream: a name with no resource (`fr-FR`) falls back
  to `en-US`, as `GetLocalizationDictionary` does for a missing resource.

#### Verification

- Util: the UI culture follows its scope, is inherited by a nested `scope`,
  can be replaced by an inner `scope_with_ui`, and does not leak out.
- Core: on a German-configured server a request in French sees French for
  `get_localized_string` (also from work spawned inside the request) and
  German for `get_server_localized_string`; outside a request the ambient
  lookup is the configured culture.
- Production binary, server configured `UICulture=de`, a movie with three
  unnamed chapters, first scan started by a request with `Accept-Language: fr`:

  | | chapter names | library view name |
  |---|---|---|
  | before | `Kapitel 1, 2, 3` (configured) | `Films` |
  | after | **`Chapitre 1, 2, 3`** (the request's) | `Films` |

#### Limitations

- **“Persistent parallel library-worker culture” (open) stays open.** Jellyfin's persistent `LimitedConcurrencyLibraryScheduler`
  workers keep the context of the request that first started them, so a later
  request's child refreshes can be named in an earlier request's culture;
  Ferrofin inherits per spawned task and has no persistent worker pool to
  reproduce that. That needs the worker lifecycle traced and ported separately.
- `Collections` and season-name resolution still use fixed English text in
  Ferrofin; only the lookup contract and the consumers that already localized
  changed here.
- Scanner-level chapter naming is covered by the live run, not a unit test: its
  naming path needs a probed file with chapters.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the changed crates passed: util 62, mediaencoding 205,
server 237, livetv 299, wasm 917 (4 skipped), api 954, extensions 96,
providers 933, core 2,459. Real FFmpeg tests enabled; the WASM guest build
remains disabled. Separate line coverage over each changed crate's own sources:
util **96.72%**, core **95.13%**, api **87.87%**, providers **91.53%**, all above
the 80% gate; traits is coverage-exempt. An earlier run failed because three api
integration-test stubs lacked the new trait method; they were fixed and every
gate was rerun from scratch. Instrumented builds were cleaned after the numbers
were taken (`target` about 25 GiB, about 494 GiB free).

## Task queue

### Busy scheduled-task queue and completion context

A request to run a task that is already running is remembered and started when
the current run ends, instead of being dropped.

Source oracle: Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/ScheduledTasks/TaskManager.cs`
(`QueueScheduledTask`, `OnTaskCompleted`, `ExecuteQueuedTasks`) and
`ScheduledTaskWorker` (triggers call `QueueScheduledTask`).

#### Behavior

- **Queue producer.** `FerrofinTaskManager::queue_scheduled` starts an idle
  task, and for a busy one appends `(task, trigger)` to an ordered queue. The
  scheduler uses it for startup triggers and for every due trigger, including
  those of a task that is currently running; previously the sweep skipped
  running tasks' triggers entirely and a startup trigger on a busy task was
  logged and lost.
- **Drain.** Wherever a run completes — normal finish, failure, or cancel/
  abort (`record_result` and `stop_run`) — the queue is drained after the task
  is `Idle`: every request is taken, the first per task is kept
  (`list.All(i => i.Item1 != item.Item1)`), and each task that is idle is
  started. Request and drain take the same lock, and the producer checks state
  and pushes under it while the drain runs after the state flips, so a request
  is started directly or seen by the drain — never lost between them.
- **Context.** A drained run starts inside the completing run's tokio task, so
  it inherits that worker's culture scope, as upstream's inherits the
  completing worker's execution context, not the producer's.
- **`StartTask` is unchanged.** The HTTP start path stays upstream's
  `Execute`, which refuses a running task (`400` here).

#### Verification

- Unit: three requests for a busy task coalesce into one follow-up run that
  starts only after the first ends; an unknown task is `NotFound`; the
  explicit start path still refuses a running task; cancelling a busy task
  starts its queued request; a due interval trigger found by the sweep on a
  running task queues a follow-up; a request queued under culture `de` starts
  under the completing run's `sv`.
- Production binary, previous baseline vs updated, 6,000-folder library so the
  scan runs about 28 s, interval trigger of one second on the library scan
  (the scheduler sweeps every 30 s):

  | | idle `StartTask` | `StartTask` while running | follow-up gaps (run end → next start) |
  |---|---|---|---|
  | before | 204 | 400 | 0.02 s, **30.56 s** |
  | after | 204 | 400 | 0.03 s, **1.94 s**; a second run: 0.21 s, 30.47 s |

  In the first updated run a sweep landed during the scan, the trigger was
  queued and the follow-up started as soon as the scan ended; in the second a
  sweep happened not to fall inside the run, matching the baseline. The
  observation depends on where the 30 s sweep falls relative to the run, so it
  is supporting evidence; the unit tests are the deterministic check.
  A single request costs one lock and a vector push; no latency was measured.

#### Limitations

- Library scans requested through `LibraryManager` use their own scan queue
  and are not routed through this registry, so they are unaffected.
- Task options are limited to the trigger tag; Ferrofin tasks take no other
  per-run options.

#### Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,427, extensions 96, livetv 296,
wasm 62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.08%**, above the 80%
gate (the traits crate and server composition are coverage-exempt). The
instrumented build took `target` to 39.95 GiB, above the 30 GiB cap; its cache
was removed right after the numbers were taken, leaving 21.5 GiB with about
495 GiB free.
