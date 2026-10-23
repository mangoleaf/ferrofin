# Live parity replay

`parity/run.sh` answers one question on a running server: **does Ferrofin answer the way Jellyfin does?**
It boots a Jellyfin (the release named by `UPSTREAM_TAG`) and the Ferrofin image under test on two
copies of the same library, sends the same jellyfin-web-shaped requests to both, compares the
answers on values, JSON types and nulls, matches each difference against a ledger of known ones,
and writes `report.html`. A new difference makes the run exit non-zero.

This is supporting evidence. **"Deep verified" stays code against upstream C#**: the replay never
adds to, redefines or shrinks the `handlers::VERIFIED` count.

It sends reads, the item-details call of `bench/screens.js` (`GET /Users/{user}/Items/{movie}`,
`requests/tracer.jsonl`), and writes, each on a fresh copy of the database and followed by read-backs
(`requests/writes.jsonl`: played and unplayed, favourite and unfavourite, a resume position, a new playlist
and a new collection). The full sweep of the contract, the edge cases, the seeded ledger and the filing of
new differences as issues extend this same path in later steps.

```bash
# needs docker, python3 (3.11+), jq, curl, timeout, taskset; the benchmark corpus, prepared for Jellyfin
bench/testdata/build.sh                       # once: the corpus (see bench/README.md)
bench/testdata/build.sh --prepare-jellyfin    # once: the corpus upgraded and scanned by the pinned Jellyfin
docker build -t ferrofin:parity .             # the commit under test: REBUILD IT
parity/run.sh --oracle "$(cat bench/testdata/jellyfin-image.txt)" --ferrofin ferrofin:parity
parity/run.sh --oracle "$(cat bench/testdata/jellyfin-image.txt)" --ferrofin ferrofin:parity --requests parity/requests/writes.jsonl
python3 bench/report.py --serve --parity-dir parity/runs      # list and compare runs
```

`--gate` makes it a release-gate run: a stale oracle pin, or an `--oracle` that is not the pin, is refused
instead of warned about. The oracle is the same pinned image and the same stale-pin check as the benchmark
(`bench/oracle.py`).

## What a run does

1. **Preflight.** Nothing is booted and no run directory is made if an input is refused (exit 2): the
   request file, `normalise.toml` and `ledger.toml` must be valid, the corpus must be prepared for this
   oracle image, the oracle pin must be current (`--gate`), and the cores must be idle.
2. **Boot.** Both servers boot at once, each on its own disposable copy of the **same** configuration (the one the
   pinned Jellyfin upgraded and scanned, `jellyfin/config`; the benchmark boots Ferrofin on the 10.11.8 source
   instead, and a replay on that would compare two different scans, not two servers), media read-only, each on
   half of `SERVER_CPUS`. Plugins and automatic tasks are switched off and the servers restarted, as in the benchmark,
   so the answers come from the core servers and nothing rewrites the copy. While each server is stopped for
   that restart, its isolated configuration and cache are copied to `<server>/pristine/`: the snapshot every
   write starts from. Readiness is `/System/Ping` and idle `/ScheduledTasks`, never a fixed sleep.
3. **Compare.** `diff.py run` signs in as the corpus's browse user on both, sends each read to both, and
   compares the normalised bodies. **Then the writes, one at a time, after every read** (a write never runs
   beside a read on the same server): before each one `reset.sh` stops both containers, puts the snapshot
   back, starts them and waits for `/System/Ping`; `diff.py` signs in again, sends the write's setup steps and
   then the write to both, compares the answers, then sends the write's read-backs and compares them like any
   read. A server that does not come back fails the run, naming it (`<server>/reset.log` keeps what docker said). The report
   shows each write's reset time, and the resets count against the cap like everything else.
4. **Report and tear down.** `report.html` and `results.json` are written, both containers are removed and
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
| `requests/*.jsonl` | the request sets |
| `normalise.toml` | what the comparison treats as equal although two servers cannot agree |
| `ledger.toml` | the accepted divergences |
| `tests/` | `test_diff.py` (the comparison, over fixture pairs and local servers) and `parity.bats` (the runner, over a fake docker) |

### Requests

JSON lines. Each object has `id` (unique, without `/`), `method` (`GET`, or a write: `POST`, `DELETE`), `path`
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

Arrays are compared in order unless a rule says otherwise. A body that is not JSON must be byte-equal.

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

A rule that matches nothing in a run is listed in the report. Rules change only through a reviewed PR.

### Ledger (`ledger.toml`)

One entry per request pattern (an id or a glob), field path and difference `kind` (one or a list, required: an
entry written for a harmless value difference must not also swallow a null or a Ferrofin timeout), with a `reason`
and exactly one link:
`verified = "<METHOD> <path>"` (a row of `handlers::VERIFIED`, checked to exist) or `issue = <iid>` (the GitLab
issue that tracks it). An entry with no link, two, or a `verified` that is not a row of the record fails the run
before anything boots. The report groups ledgered differences by their link and lists the entries (not links) that
matched nothing.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `PARITY_CAP_S` | 7200 | the whole run's limit, at most 7200 |
| `PARITY_REQUEST_TIMEOUT` | 30 | seconds per request, 10 to 120 |
| `PARITY_ORACLE_PORT`, `PARITY_FERROFIN_PORT` | 18111, 18113 | loopback ports |
| `PARITY_PIN` | 1 | `0`: no cpusets and no idle-core check, for a host without the benchmark's core layout and for the runner's own tests; a `--gate` run refuses it |
| `SERVER_CPUS`, `CLIENT_CPUS` | 8-15, 16-19 | the benchmark's cpusets; each server gets half of `SERVER_CPUS` |
| `CORE_IDLE_MIN` | 0.90 | how idle those cores must be before a run starts |
| `MEMORY` | 8g | per server, swap disabled |
| `READY_TIMEOUT_S`, `DRAIN_TIMEOUT_S`, `HTTP_TIMEOUT_S`, `CLEANUP_TIMEOUT_S` | 300, 300, 10, 30 | boot and teardown bounds |

## Tests

```bash
python3 -m unittest discover -s parity/tests     # the comparison, the ledger, the cap, the report
bats parity/tests                                # the runner: refusals, the cap and its backstop, teardown, failure reports
shellcheck -x parity/run.sh && shellcheck -x -s bash parity/tests/parity.bats
```

CI runs all three. The runner's pinned-cpuset path needs the benchmark's core layout, so only a live run covers it.
