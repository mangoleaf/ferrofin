# Live parity replay

`gates/parity/run.sh` answers one question on a running server: **does Ferrofin answer the way Jellyfin does?**
It boots a Jellyfin (the release named by `UPSTREAM_TAG`) and the Ferrofin image under test on two
copies of the same library, sends the same jellyfin-web-shaped requests to both, compares the
answers on values, JSON types and nulls, matches each difference against a ledger of known ones,
and writes `report.html`. A new difference makes the run exit non-zero.

This is supporting evidence. **"Deep verified" stays code against upstream C#**: the replay never
adds to, redefines or shrinks the `handlers::VERIFIED` count.

The request sets:
- `requests/sweep/`, **the sweep**: at least one request for every one of the 412 operations of the vendored
  10.11.8 contract (`contract.jsonl`); one request per documented value of each enumerated or boolean
  parameter of the query-heavy operations (`combos.jsonl`: `/Items`, the by-name lists, `/Shows/*`, Latest,
  Resume); and jellyfin-web's request shapes from `gates/bench/screens.js` (`screens.jsonl`).
- `requests/edge/`, **the edge cases** (B7): two users with restrictions, each made on a fresh copy and asked
  about by id in its read-backs (a user allowed up to PG-13, a user with a blocked tag), each also asked for the
  one movie it must not see, which the oracle must refuse, so a restriction both servers ignored fails the run; paging at the first page,
  the last, exactly at the end and past it; SortBy/SortOrder combinations; `Fields` with every documented
  field; `EnableUserData=false`; `ImageTypeLimit` 0, 1 and many; and the item kinds that carry the fields the
  TV clients require non-null (`ChildCount`, `ImageTags`, `ImageBlurHashes`): views, series, seasons, albums,
  artists, episodes, a new box set, the root and the libraries. A template may offset an integer:
  `{counts.movies-1}` is the last page's index on any corpus size.
- `requests/writes.jsonl`: a starter set of writes with read-backs (played and unplayed, favourite and
  unfavourite, a resume position, a new playlist and a new collection).
- `requests/tracer.jsonl`: the item-details call of `gates/bench/screens.js` alone.

A release run sends the sweep and the edge cases as one set; a new difference in either fails it. The seeded
ledger and the filing of new differences as issues extend this same path in later steps.

```bash
# needs docker, python3 (3.11+), jq, curl, timeout, taskset; the benchmark corpus, prepared for Jellyfin
export FERROFIN_GATES_DIR=/srv/gates          # the output root (gates/lib.sh): the corpus and every run go under it
gates/bench/testdata/build.sh                       # once: the corpus (see gates/bench/README.md)
gates/bench/testdata/build.sh --prepare-jellyfin    # once: the corpus upgraded and scanned by the pinned Jellyfin
docker build -t ferrofin:parity .             # the commit under test: REBUILD IT
gates/parity/run.sh --oracle "$(cat gates/bench/testdata/jellyfin-image.txt)" --ferrofin ferrofin:parity
gates/parity/run.sh --oracle "$(cat gates/bench/testdata/jellyfin-image.txt)" --ferrofin ferrofin:parity --requests gates/parity/requests/writes.jsonl
gates/parity/run.sh --oracle "$(cat gates/bench/testdata/jellyfin-image.txt)" --ferrofin ferrofin:parity \
  --requests gates/parity/requests/sweep --requests gates/parity/requests/edge                                  # the release run
python3 gates/bench/report.py --serve 8097 "$FERROFIN_GATES_DIR/runs" --parity-dir "$FERROFIN_GATES_DIR/runs"   # list and compare runs
```

`--gate` makes it a release-gate run: a stale oracle pin, or an `--oracle` that is not the pin, is refused
instead of warned about. The oracle is the same pinned image and the same stale-pin check as the benchmark
(`gates/bench/oracle.py`).

## What a run does

1. **Preflight.** Nothing is booted and no run directory is made if an input is refused (exit 2): the
   request sets, `normalise.toml` and `ledger.toml` must be valid, the corpus must be prepared for this
   oracle image, the oracle pin must be current (`--gate`), and the cores must be idle.
2. **Boot.** Both servers boot at once, each on its own disposable copy of the **same** configuration (the one the
   pinned Jellyfin upgraded and scanned, `jellyfin/config`; the benchmark boots Ferrofin on the 10.11.8 source
   instead, and a replay on that would compare two different scans, not two servers), media read-only, each on
   half of `SERVER_CPUS`, with the same container hostname (an unnamed server's name). Plugins that can be
   uninstalled and automatic tasks are switched off and the servers restarted, so the answers come from the core
   servers and nothing rewrites the copy; the in-tree plugins (TMDb, OMDb, …), which Jellyfin cannot disable, stay
   as they are on both, and Ferrofin's compiled-in extensions, which Jellyfin does not have, are off
   (`FERROFIN_DISABLE_EXTENSIONS=1`). While each server is stopped for
   that restart, its isolated configuration and cache are copied to `<server>/pristine/`: the snapshot every
   write starts from. Readiness is `/System/Ping` and idle `/ScheduledTasks`, never a fixed sleep.
3. **Compare.** `diff.py run` signs in as the corpus's browse user on both, sends each read to both, and
   compares the normalised bodies. **Then the writes, one at a time, after every read** (a write never runs
   beside a read on the same server): before each one `reset.sh` stops both containers, puts the snapshot
   back, starts them and waits for `/System/Ping`; `diff.py` signs in again, sends the write's setup steps and
   then the write to both, compares the answers, then sends the write's read-backs and compares them like any
   read. A server that does not come back fails the run, naming it (`<server>/reset.log` keeps what docker said). The report
   shows each write's reset time, and the resets count against the cap like everything else.
4. **Report and tear down.** `report.html`, `results.json` and `new-differences.json` are written, both containers are removed and
   their logs saved. **A run that ends any way at all leaves a report**: a server that did not boot, the cap,
   a signal.

The run is bounded by `PARITY_CAP_S` (at most 7200, two hours, the owner's limit and the default), counted from
the start, boot included. Each boot step has its own short bound (`READY_TIMEOUT_S`, `DRAIN_TIMEOUT_S`, ...), the cap
is checked as boot ends (a cap shorter than the boot fails the run with a named reason), and the comparison gets what
is left. Each request is bounded in total by `PARITY_REQUEST_TIMEOUT` (default 30 s, 10 to 120) and never runs past the cap; a
request the cap cuts short counts as the cap's doing, not as a slow Ferrofin. At the cap the runner stops sending,
writes a partial report marked `cap exceeded`, and the run fails. A comparison that hangs past it is killed.

Each run directory keeps what the servers left (`oracle/` and `ferrofin/`: their logs, config copies and the
isolation snapshots): remove the directory when you are done with the run.

## Exit status

| Exit | Meaning |
|---|---|
| 0 | no new difference (ledgered differences and oracle failures do not fail it, unless the oracle failed on every request: nothing was compared) |
| 1 | a new difference, or the run failed (a server did not boot, the cap passed, a reset before a write failed, the comparison crashed) |
| 2 | refused before anything started: bad arguments, an invalid input file, a stale pin on `--gate`, busy cores |
| 129, 130, 143 | interrupted (SIGHUP, SIGINT, SIGTERM); the containers are removed and a failure report is written |

## Files

| File | Purpose |
|---|---|
| `run.sh` | orchestration: preflight, boot, cap, teardown |
| `reset.sh` | puts both servers back on the boot snapshot before a write (`diff.py` runs it) |
| `lib.sh` | what `run.sh` and `reset.sh` share: the wait bounds and the readiness check |
| `diff.py` | the requests, the comparison, the ledger, the report (`diff.py --help`) |
| `requests/*.jsonl`, `requests/sweep/`, `requests/edge/` | the request sets; `--requests` takes a file or a directory (every `*.jsonl` in it, in name order), and repeats to send several as one set |
| `generate.py`, `sweep.toml` | build `requests/sweep/contract.jsonl` and `combos.jsonl` from the vendored contract (see below) |
| `discover.toml` | names the corpus does not record, read from the oracle at run start (`{found.<name>}`) |
| `normalise.toml` | what the comparison treats as equal although two servers cannot agree |
| `ledger.toml` | the accepted divergences |
| `filed.toml` | the new differences already filed as issues, so the report names them (see "Filing new differences") |
| `tests/` | `test_diff.py` (the comparison, over fixture pairs and local servers), `test_generate.py` (the sweep covers every operation and is current), `test_edge.py` (every edge category has requests) and `parity.bats` (the runner, over a fake docker) |

### Requests

JSON lines. Each object has `id` (unique, without `/`), `method` (`GET`, `HEAD`, or a write: `POST`, `DELETE`), `path`
and optionally `query`, `source` (where jellyfin-web issues it) and `oracle_trap` (why the oracle is known to
stall or fail on it: a timeout there is recorded as `oracle failed` and is no difference; on any other
request, or on Ferrofin, a timeout is a difference).
`{name}` in a path or query value is read from the corpus's `ids.json` (`{user}`, `{movie}`, `{views.movies}`);
the runner checks that every template resolves before it boots anything.
`expect_status` (default 200) is the status the oracle answers: **two servers that agree on an error are not a
pass**, so an oracle that answers anything else makes the request a difference unless it says an error is expected.
`oracle_trap` covers any oracle answer other than the expected one (a stall, a connection failure or a 5xx).
A write may carry a `body` (a JSON object or array; the templates in its strings are filled too) and
`readbacks`: GETs with their own `id`, `path`, `query`, `source`, `oracle_trap` and `expect_status`, sent
after the write and reported as `<write id>/<read-back id>` (`playlist-create/items`), which is also the id
the ledger and a normalisation rule's `requests` match. In a read-back, `{write.Id}` (any field of the write's
JSON answer, `{write.A.B}` reaching into it) is read from **what each server answered**, so each is asked about
the playlist it created; a server whose answer lacks the field is not asked, and that is a difference (an
`oracle_trap` never excuses it). When the oracle fails on a write with an `oracle_trap`, its read-backs are not
sent and count as oracle failures too.

A read-back may name a `settle` field (`"settle": "ProductionYear"`): a field the server fills in a background
refresh the write only queues (both servers queue a new collection's first refresh). A server whose answer lacks
it, or has it null or an empty list, is asked again every second (lib.sh's `POLL_S`) until it is there or the request
timeout has passed; then its last answer is compared. Each server settles on its own, so a slow refresh is
compared once it is done and a refresh that never fills the field is still a difference.

A write may also carry `setup`: writes (with a request's fields, without read-backs) sent on the fresh copy
before it, to build the state it acts on. An unfavourite first favourites the movie, so it clears something
rather than nothing. Setup steps are compared like everything else and reported as `<write id>/<step id>`; ids
are unique among a write's setup steps and read-backs.

```json
{"id": "playlist-create", "method": "POST", "path": "/Playlists", "body": {"Name": "Parity playlist", "Ids": ["{movie}"], "UserId": "{user}"},
 "readbacks": [{"id": "items", "path": "/Playlists/{write.Id}/Items", "query": {"userId": "{user}"}}]}
```

### Comparison

Bodies are compared as JSON, after the normalisation rules:

| Kind | When |
|---|---|
| `status` | the status codes differ, or either side did not answer (a timeout is a difference on Ferrofin) |
| `value` | the same JSON type, a different value |
| `type` | different JSON types (including a string where a number belongs) |
| `null` | a value on one side is `null` on the other, **always reported, even inside an otherwise matching object** |
| `missing` / `extra` | a field only Jellyfin / only Ferrofin has: an extra field on Ferrofin is a difference too |
| `length` | arrays of different length |

Arrays are compared in order unless a rule says otherwise. Two elements in the same place that are objects with
different `Id`s are different items: the order or the selection differs, and that is reported once, as a `value`
difference at `<array>[].Id`, without comparing the two items field by field, and the rules under them are not
counted as used by that pair. Ids that differ only in dashes or case are the same item (the format is reported at
`.Id` and the fields are compared), and a rule on the element itself or one making its `Id` opaque turns the
collapse off. A body that is not JSON must be byte-equal.

### Normalisation (`normalise.toml`)

Nothing is normalised implicitly. A rule names the **exact field paths** it applies to (`Items[].ServerId`: `[]`
means each element of that array; there are no wildcards over names) and says **why**. Three kinds:

- `opaque`: the values may differ; the JSON type and nullness may not.
- `opaque-keys`: the object is keyed by something opaque (an image tag): the keys may differ, the number of entries and their values may not.
- `unordered`: the array has no order the contract specifies; elements are matched by the field in `by`.

A rule may name `requests` (ids or globs, `["playlist-create", "playlist-create/*"]`): it then applies to
those requests only. That is for what a write makes differ on its own, the time something was marked played
or a playlist was created, without making the same path opaque everywhere. A new item's id is no such case:
both servers derive it from the same path, so it must agree. A path belongs to one rule for every
request, or to rules limited to requests that never share one (checked before anything boots).

The path `$` is the whole body, for the requests a rule names (it must name them): `unordered` over a root array
(`/Library/VirtualFolders`), or `opaque` over a body that can never agree. For a body that is not JSON (an
encoder's output: a re-encoded image, a remuxed stream) both sides must still answer one; a JSON body is then
compared only for its type and nullness, nothing inside it.

Each server's own harness token is replaced by `<token>` in its answers before they are compared or stored: the
servers write the token their session holds into the URLs they return (a subtitle playlist's `ApiKey=`).

A rule that matches nothing in a run is listed in the report. Rules change only through a reviewed PR.

### Ledger (`ledger.toml`)

One entry per request pattern (an id or a glob), field path and difference `kind` (one or a list, required: an
entry written for a harmless value difference must not also swallow a null or a Ferrofin timeout), with a `reason`
and exactly one link:
`verified = "<METHOD> <path>"` (a row of `handlers::VERIFIED`, checked to exist) or `issue = <iid>` (the GitLab
issue that tracks it). An entry with no link, two, or a `verified` that is not a row of the record fails the run
before anything boots. A path with `<tag>` matches every image-tag key of that map
(`Items[].ImageBlurHashes.Primary.<tag>`), as in `filed.toml`. The report groups ledgered differences by their link
and lists the entries (not links) that matched nothing.

### Filing new differences (`new-differences.json`, `filed.toml`)

Every run writes `new-differences.json` beside its report. It holds each request with a difference no ledger
entry accepts: the request (method, target, source, request file), each new difference's field path, place,
kind and both values, both bodies **as the comparison saw them** (`<opaque>` where a rule made a value opaque,
`<key N>` for opaque keys, unordered arrays sorted), the rules that changed them, and the issue a difference
was filed as. The harness never talks to GitLab: filing is a step a person or an agent takes after the run.

1. `python3 gates/parity/diff.py findings --out "$FERROFIN_GATES_DIR"/runs/parity-<run>` prints the differences not filed yet as Markdown,
   one section per field path and kind (with the requests that show it and the first one's bodies), largest
   group first.
2. File them through the project's issue-filing workflow (agents: the issue-filing skill), which dedupes
   against the existing issues on the request pattern and the field path, and verifies each against the
   upstream code before it is filed.
3. Add each filed difference to `filed.toml` (`request` pattern, `path` as `findings` prints it, `kind`, `issue`)
   in a reviewed PR; a path with `<tag>` covers every image tag of that map. The next run's report then shows
   `gl-<iid>` beside it, `findings` no longer offers it, and an entry that matched nothing is listed in the
   report (fixed, or no longer sent). `filed.toml` is validated before anything boots, like the ledger.

A filed difference is still new: it fails the run until Ferrofin is fixed (or the difference is accepted into
`ledger.toml`, which needs a `VERIFIED` row or the issue as its link).

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `PARITY_CAP_S` | 7200 | the whole run's limit, at most 7200 |
| `PARITY_REQUEST_TIMEOUT` | 30 | seconds per request, 10 to 120 |
| `PARITY_CONCURRENCY` | 4 | reads in flight per server, 1 to 8; writes always run one at a time |
| `PARITY_ORACLE_PORT`, `PARITY_FERROFIN_PORT` | 18111, 18113 | loopback ports |
| `PARITY_PIN` | 1 | `0`: no cpusets and no idle-core check, for a host without the benchmark's core layout and for the runner's own tests; a `--gate` run refuses it |
| `SERVER_CPUS`, `CLIENT_CPUS` | 8-15, 16-19 | the benchmark's cpusets; each server gets half of `SERVER_CPUS` |
| `CORE_IDLE_MIN` | 0.90 | how idle those cores must be before a run starts |
| `MEMORY` | 8g | per server, swap disabled |
| `READY_TIMEOUT_S`, `DRAIN_TIMEOUT_S`, `HTTP_TIMEOUT_S`, `CLEANUP_TIMEOUT_S` | 300, 300, 10, 30 | boot and teardown bounds |

### The sweep (`generate.py`, `sweep.toml`)

`python3 gates/parity/generate.py` writes `requests/sweep/contract.jsonl` and `combos.jsonl` from
`contracts/jellyfin-openapi-10.11.8.json` and `sweep.toml`; the files are checked in, so a change to the request
set is a diff to review, and a self-test fails when they are stale or an operation has no request.
`screens.jsonl` is written by hand from `gates/bench/screens.js`: a change to a screen there is copied here by hand.
- A path parameter takes its value from `[path]` by name, or from `[path_by_prefix]` where the same name means
  something else (`name` is a genre under `/Genres/`, a person under `/Persons/`). A required query
  parameter takes its value from `[query]`. The expected status is the lowest success status the contract
  documents.
- `[op.<operationId>]` overrides one operation: a body, query values, setup steps, an `oracle_trap`, the status
  it answers on this corpus with the reason (both servers are asked about something the corpus lacks: a Live TV
  timer, an artist's image), or `readback_of`: a read that needs something a write creates (a playlist's items)
  is sent as a read-back of that write, with `{write.Id}`.
- An `oracle_trap` excuses **any** answer from the oracle other than the expected status (a stall, a 404, a 500),
  never Ferrofin's; when the oracle answers as expected the pair is compared as usual. The sweep marks, each with
  its reason: the three known traps of `POST /Backup/Create`, `/Years` and `/Items/Filters` (which this corpus
  happens not to trip); the routes of the plugins the vendored contract carries (the oracle runs with its plugins
  disabled); the routes Jellyfin 12 removed; remote lookups that need the internet; Live TV writes without a
  tuner; and requests the oracle fails on this corpus (a read-only media mount, no trailers, a slow
  `/Items/Suggestions`). The report lists every oracle failure with its reason and what Ferrofin answered, so
  what a trap leaves unchecked stays visible.
- A write that reads `{setup.<name>.…}` gets the `[setups.<name>]` sequence in front of it (a playlist to add
  to, the session to send a command to). A setup step may be a read; its answer on each server is what
  `{setup.<step>.Field}` reads there (list indices allowed: `{setup.session.0.Id}`).
- `[combos]` lists the query-heavy operations; each gets its base query plus one parameter at a time, once per
  enum value, true and false, or the values `[combos.values]` names.

### Discovered names (`discover.toml`)

Names the corpus's `ids.json` does not record (a person, an artist, a log file, a task) are read from the
oracle once it is up: `[found.<name>]` names the GET, its query and the field of the answer (`Items.0.Name`).
Only the names the request set uses are asked for, and a name the oracle does not give fails the run.
Both servers serve the same library, so the oracle's answer serves both. A run's `results.json` records them.

### Timing

Reads run `PARITY_CONCURRENCY` at a time per server (a worker sends one read to the oracle, then to Ferrofin);
writes run one at a time after every read. The report shows the time of each phase against the cap: the
boot, the reads and their concurrency, the writes and how much of that went to resets. Each request file is a
shard of the set and a group of its own: the report's "By request file" table gives each file's requests by
state (identical, ledgered, new differences, oracle failed), both servers' answer times added up, and its
resets (`results.json`: `timing.files`), so the edge cases read apart from the sweep.

## Tests

```bash
python3 -m unittest discover -s gates/parity/tests     # the comparison, the ledger, the cap, the report, the sweep's coverage
bats gates/parity/tests                                # the runner: refusals, the cap and its backstop, teardown, failure reports
shellcheck -x gates/parity/lib.sh gates/parity/run.sh gates/parity/reset.sh && shellcheck -x -s bash gates/parity/tests/parity.bats
```

CI runs all three. The runner's pinned-cpuset path needs the benchmark's core layout, so only a live run covers it.
