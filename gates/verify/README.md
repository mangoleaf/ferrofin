# Scan-behaviour check

`verify/scan-behaviour.sh` checks, against any running Ferrofin, that a library scan
reprocesses only what changed: a scan does not re-probe, re-fetch or re-save the whole
library unless the dashboard asked for a full refresh ("Search for missing metadata",
"Replace all metadata"). Point it at a local server, a copy of production data or a staging
pod. It is a live check you run deliberately, not a CI job; CI runs its shell tests only.

It is one of three ways to see that scans behave:

- **CI** — `apps/ferrofin-server/tests/scan_change_detection.rs` boots the real server on
  a generated library, with every provider a scan reaches pointed at a counting mock and
  `ffprobe` replaced by a counting stub. It asserts the same rows, plus a fourteenth (a
  webhook report while a library refresh is pending runs no full scan), on `/metrics`,
  probe spawns, provider requests, artwork downloads and database writes.
- **Production** — the library-scan metrics and the *Library scans* Grafana dashboard
  (`contrib/metrics/README.md`): after a deploy, the second scheduled scan should show
  `unchanged` ≈ the library size and no ffprobe runs.
- **On demand** — this script.

## Running it

```bash
FERROFIN_TOKEN=… verify/scan-behaviour.sh BASE_URL LIBRARY_ID SCRATCH_DIR [SERVER_SCRATCH_DIR]
```

- `FERROFIN_TOKEN` is an administrator's access token. The script reads it from the
  environment, never from the command line: a process's arguments are visible to every
  user (`ps`, `/proc/PID/cmdline`), its environment only to its own user. `curl` gets it
  from a header file in the script's private temporary directory.
- `LIBRARY_ID` is the library's `ItemId` from `GET /Library/VirtualFolders`. Movies and TV
  libraries are supported.
- `SCRATCH_DIR` is an **empty** folder inside that library, as this machine sees it, given
  without symlinks or `..`. Put it directly under the library root, never inside a real
  item's folder (the scratch items would share that folder with it). In a TV library the
  folder becomes the scratch series, so it must sit directly under the root and be named
  with the `Ferrofin Verify` prefix (`Ferrofin Verify Show`, say); the script refuses any
  other name, and a folder a real series already occupies (one named otherwise, or with
  provider ids).
- `SERVER_SCRATCH_DIR` is the same folder as the server sees it, when that differs: a
  container or a sandbox that mounts the library somewhere else.
- `--clip FILE` gives a small real video to copy as the scratch media. By default the
  script generates a one-second clip with `ffmpeg`. With neither, it skips every row (exit
  3), because a file the server cannot probe is re-probed on every scan.
- `--timeout SECONDS` sets how long one scan may take (default 300).
- `--nfo-wait SECONDS` sets how long row 4 waits, after the scratch items were saved,
  before it rewrites an NFO (default 62). The server re-reads an NFO only once the NFO is
  more than a minute newer than the item's last save, and it compares the file's mtime
  (the file server's clock) with its own. Raise it when the two clocks differ, as they
  can with NFS or a server in a remote pod.

It needs `curl` and `jq`, and it refuses to run (exit 2) unless:

- the server has `/metrics` on (`FERROFIN_ENABLE_METRICS=true`, or `EnableMetrics` in
  `system.json`, then a restart);
- the token is an administrator's;
- the library has real-time monitoring off. Otherwise the watcher would process the
  scratch changes before the scans the rows time. Turn it off for the run in Dashboard →
  Libraries.

The `/metrics` counts the rows assert are **server-wide**, not per library. Run it while no
other scan runs on the server: a scheduled scan, a webhook or another library's refresh
during the run makes rows fail that did nothing wrong.

## What it does to the server

It writes only its own scratch files under `SCRATCH_DIR`: one-second clips, their NFOs, a
1×1 poster and a subtitle sidecar. It edits the metadata of its own scratch items only. On
exit (a failure or an interrupt included) it deletes every file it created and nothing
else, lists anything else it finds left in the folder (a file the server's NFO or artwork
savers wrote there; only once the folder was found empty, so a refusal never lists the
contents of a folder it was wrongly given), and queues a library refresh so the server
prunes the scratch items. The token's header file goes with the script's temporary
directory on every exit, a second Ctrl-C during that refresh included.

In a TV library one thing stays behind: the scratch folder is the scratch series, and the
folder is not the script's to delete, so after the run an empty series with its name stays
in the library. The script says so at the end; to remove it, delete the empty folder, then
rescan the library.

Each scratch item has an NFO carrying everything the scan's "already enriched" checks look
for. The scan asks the remote providers again, on every scan, about an item that still
lacks something they could supply, until one answers: for TMDb, a movie or series with no
overview or no trailer, an episode with no real title or overview, and — when OMDb is also
on for the type — no critic rating; for OMDb alone, no overview, community rating or critic
rating. A scratch item no provider can match would then be asked about on every scan, and
the rows would see requests a correct server makes. So every scratch NFO has a title, a
plot, a community rating and a critic rating, and the movies, the series and one episode a
trailer. The field row 7 clears is the official rating, which none of those checks reads.

## The rows

| # | Scenario | Asserted |
|---|---|---|
| 1 | first scan of new items (library refresh) | created = the scratch items, nothing removed, nothing updated (in a TV library, the scratch series an earlier run left behind is refreshed once and not created), one probe per clip, provider requests when the library's remote fetchers are on (none when off); in a TV library, the scratch series has no provider ids |
| 2 | rescan, nothing changed | 0 created, updated or removed; 0 probes; 0 provider requests |
| 3 | touch one file's mtime, rescan | 1 updated, 1 probe, providers per the fetchers |
| 4 | NFO rewritten more than a minute after the last save | 1 updated, 0 probes, 0 provider requests, the new plot served |
| 5 | `POST /Library/Media/Updated`: a new file | 1 created, 1 probe; in a TV library the season may update (its folder changed) |
| 6 | the same webhook: the file deleted | 1 removed, 0 probes, 0 provider requests, gone from `/Items` |
| 7 | editor save of Overview and a cleared official rating (no LockData), rescan | nothing updated, 0 probes, 0 provider requests; the edits kept, `LockData` false |
| 8 | "Search for missing metadata" on the item | the edited Overview kept, the cleared official rating filled (from the NFO) |
| 9 | "Replace all metadata", Overview unlocked | the Overview replaced |
| 10 | Overview in `LockedFields`, "Replace all metadata" | the locked Overview kept, the unlocked rating replaced |
| 11 | `LockData=true`, a new local poster, the file touched, rescan | the poster discovered; 1 probe, 0 provider requests (unlocked, the touch alone would ask them, as row 3 shows when the fetchers are on) |
| 12 | the locked item's file touched again, rescan | 1 probe, 0 provider requests; name, overview, trailers and rating unchanged |
| 13 | an episode's edited premiere date and year (TV only), quiet rescan, then a sidecar subtitle | unchanged by the rescan (nothing created or updated, 0 probes, 0 provider requests) and by the re-probe save (1 probe, 0 provider requests). Only fields the episode's NFO does not carry: the re-probe makes the server run the local readers too, so the NFO's own fields (its ratings) are the NFO's again after that save |

Rows 5 and 6 wait the server's `LibraryMonitorDelay` (60 s by default) before their scans
run. Row 4 runs after them, to let the NFO rule's minute pass meanwhile.

Whether the library's remote fetchers are "on" is read from its saved metadata-downloader
choices for the scratch items' kinds (a movie; or a series, its seasons and its episodes, as
the choice is per kind): on when any of them ticks one, off when every one of them saved an
empty choice. Otherwise it is unknown and the rows do not check provider requests: a kind
with no saved choice follows the server-wide one. OMDb ticked alone counts as on
because it now uses a shared API key by default.

In a TV library the rows check for 0 provider requests, whatever the fetchers, on every row
but the first. An episode's (and a season's) remote lookup starts from its series' provider
ids, and the scratch series has none. Row 1 fails and stops the run if it gets some: its
name search can pin a real show, and then no later row could expect 0. Row 1's own requests
(the new series searching for its match) are not checked when the fetchers are on.

## Output

Each row prints as it finishes, then the script prints the whole table in row order and a
summary. The exit status is 0 when nothing failed, 1 when a row failed, 2 when the script
refused to run, 3 when nothing could be verified, 130 on SIGINT and 143 on SIGTERM (after
the cleanup). A failed row names each check that missed, with the value it wanted:

```
   6  FAIL  webhook: delete one file                        updated 1 (want <= 0);probes 1 (want = 0) | created=0 updated=1 …
```

## Shell tests

`verify/tests/verify.bats` runs in CI with the other shell tests. It drives the `/metrics`
arithmetic in `lib.sh` with canned exposition text, and the script's refusals against a
`python3` static file server standing in for the API. Whole runs go against
`verify/tests/fake_server.py`, which plays back a scenario of what a well-behaved server's
scans do. They cover:

- a passing run, and one with a failing row (exit 1);
- a movies library with its fetchers on and one with them off, each with the row that must
  fail when a request is missing or stray;
- OMDb ticked alone ("on" with its shared key), with every scratch NFO checked for the fields the
  enriched checks read;
- a TV library with its fetchers on (every row but the first expects 0), a stray episode
  request, a scratch series that matched a real show (the run stops after row 1), and one
  with only the series fetchers ticked (on, not off);
- options with leading zeros;
- SIGINT mid-run (exit 130), and a second SIGINT during the cleanup.

Every whole run checks that the token is on no command line (the script's, read while it
runs) and in no request URL (the cleanup's own requests included), and that the files the
script created are gone; the movies runs also that a real item beside the scratch folder is
untouched, the interrupted ones that the temporary directory holding the token is gone.

The fake cannot show a server *deciding* to ask OMDb again: it plays back what a scan does
and decides nothing. That behaviour is the server's; here the check is on the script's
side, that its NFOs leave the enriched checks nothing to ask for.

```bash
shellcheck -x verify/lib.sh verify/scan-behaviour.sh
shellcheck -x -s bash verify/tests/verify.bats
bats verify/tests
```

## Test record

2026-09-28, a release build of branch `feat/scan-change-detection` with this check. The
server ran in a `bwrap` sandbox on a disposable reflink copy of the bench corpus config
(20,497 items, remote fetchers off, real-time monitoring off, `LibraryMonitorDelay` 60 s)
with `FERROFIN_ENABLE_METRICS=true`. The movies and shows libraries were tmpfs overlays,
so no write could reach the corpus, and a host scratch directory was bound over one
subfolder of each. The corpus was scanned twice first; the second scan was quiet (20,498
unchanged, 0 probes).

- **Movies** library: 12 PASS, 1 SKIP (row 13 needs a TV library), exit 0.
- **Shows** library: 13 PASS, exit 0.
- **Forced failure**: during row 5's settle window, one scratch movie's file was touched
  behind the script's back. The next scan that validated the scratch folder (row 6's)
  re-probed and saved it, and row 6 failed with
  `updated 1 (want <= 0);probes 1 (want = 0)`, exit 1. The cleanup still removed every
  scratch file, and the server pruned the scratch items.

## Dashboard scan progress

`node verify/scan-progress.mjs BINARY [LOG_EVERY]` boots a disposable server and
checks progress over authenticated HTTP and WebSocket connections. It needs Node
22 or newer and uses controlled ffprobe stubs; no media or existing server is
modified. Logs and captured events remain in the printed temporary directory.

Run with logging cadences `0`, `1`, and `100` to check that UI notifications do not
depend on item-count logging. The fixture holds the first probe, reconnects during
that stall, and checks intermediate ratios, short unchanged scans, scoped
refreshes, cancellation, terminal cleanup, and an empty library. Controlled-clock
Rust tests cover exact timer boundaries, item 100, and overlapping scan ownership.

Add `--measure` to compare binaries on the same generated 200-item fixture. It
prints initial/unchanged scan wall times and library notification counts without
requiring the baseline to exhibit the corrected behavior. Use three alternating
baseline/fixed runs and compare medians; these local debug-build measurements are
not production throughput estimates.

## Query binding

`python3 verify/query-binding.py BINARY` boots a disposable server and checks
first-value binding for repeated scalar query parameters over authenticated HTTP.
The 27 probes cover exact and mixed-case repetitions, encoded names and commas,
integers, booleans, nullable UUIDs, collection requests, query-token authentication
and a query at the URI size limit.
Authentication follows Jellyfin's direct `StringValues` read: repeated nonempty
tokens are comma-joined, and empty values are skipped. Its behavior is separate
from scalar controller binding; tests cover encoded keys and header precedence.
API tests additionally inspect the bound collection values and the raw HLS query.
The script creates its own administrator, cleans up its data and prints no tokens.

Mixed collection syntax remains an open parity issue: Ferrofin's existing string
representation flattens `genres=A%7CB&genres=C` into `A`, `B`, `C`. Jellyfin's
[pipe collection binder](https://github.com/jellyfin/jellyfin/blob/v12.2/Jellyfin.Api/ModelBinders/PipeDelimitedCollectionModelBinder.cs)
keeps `A|B`, `C` when the key is repeated. The comma collection binder has the
same distinction. The collection probes verify ordinary repetitions and existing
Ferrofin parsing; they do not establish parity for that mixed syntax.

Add `--baseline` to report the old binary's failures without stopping. Add
`--samples 1000` to measure warmed, sequential keep-alive `GET /Sessions` requests
with the same canonical scalar query and response on both versions. Run three
alternating baseline/fixed pairs on a quiet machine and compare the medians and
their range. These local HTTP timings include the Python client's overhead and
are not a replacement for the loaded media-library benchmarks in `bench/`.

```bash
python3 verify/query-binding.py /path/to/before --baseline --samples 1000
python3 verify/query-binding.py target/debug/ferrofin-server --samples 1000
```
