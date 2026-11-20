# Round trip: back to Jellyfin

Answers one question: **does the latest Jellyfin release start on what Ferrofin leaves behind, with
everything the user did in Ferrofin?** It is the proof the way back is built against. Until it passes on
every route, adoption stays one-way in every document.

Each route copies a pristine synthetic fixture. Ferrofin adopts and scans the copy, a scripted usage
stage changes it, and Ferrofin stops. Then `ferrofin-server export-jellyfin` from the image under test
exports the result, the Jellyfin release that `UPSTREAM_TAG` names boots on the export, and its answers
are compared with Ferrofin's.

| Route | Source | Jellyfin on the export |
|---|---|---|
| `10.11.8` | `jellyfin-10.11.8` | applies no schema migration (the exported history records the 20 that Ferrofin's own migrations already applied) and runs the 19 code migrations it runs on a 10.11.8 database of its own |
| `12.4-from-123` | `jellyfin-12.4-from-123` | runs no migration: the history is already the 12.2 set, which 12.3 and 12.4 keep |

The routes are in [`routes.sh`](routes.sh). The other generations come with their history rule.
Without the history rewrite, Jellyfin's start fails on the `10.11.8` route:
`migration 20260113102337_AddLinkedChildrenTable failed: SQLite Error 1: 'table "LinkedChildren" already exists'`.

## Running it

```bash
export FERROFIN_GATES_DIR=/srv/gates          # the gates' output root (../README.md)
gates/adoption/build-fixtures.sh              # the fixtures, once (the adoption gate's own)
docker build --build-arg SOURCE_COMMIT="$(git rev-parse HEAD)" -t ferrofin:bench .
docker pull jellyfin/jellyfin:12.4            # the release UPSTREAM_TAG names
gates/roundtrip/run.sh                        # both routes; --only 10.11.8 for one
```

It prints one `PASS`/`FAIL` line per route, and the reasons. It exits 1 if a route failed and 2 if
the run was refused. The run directory, `$FERROFIN_GATES_DIR/runs/roundtrip-<run id>/`, holds
`report.html` (every route, stage and check, built however the run ends) and `results.jsonl` (the same
events, the adoption report's format). Each route also has a folder with:

| File | What |
|---|---|
| `oracle.txt` | Ferrofin's answers before the stop (the usage stage's) |
| `oracle.txt.actions` | each usage-stage action's request and its HTTP status |
| `export.log` | what the export printed |
| `answers.txt` | Jellyfin's answers after the return |
| `ferrofin-boot.log`, `ferrofin.log` | Ferrofin's log after the scan, and its whole log |
| `jellyfin-boot.log`, `jellyfin.log` | Jellyfin's log up to the probes, and its whole log |
| `ferrofin/`, `jellyfin/` | the copy Ferrofin ran on and the export Jellyfin ran on, kept only when the route failed |
| `ferrofin.cid`, `export.cid`, `jellyfin.cid` | while a container runs, the id docker gave it |

Options: `--image IMAGE` (default `ferrofin:bench`), `--jellyfin-image IMAGE` (default
`jellyfin/jellyfin:<minor of UPSTREAM_TAG>`), `--fixtures DIR` (default the adoption fixtures under the
output root), `--only ROUTE`, `--run-id ID` and `--work DIR` (the run directory). The gate never builds
or pulls an image. It runs Jellyfin by its image id, and the report names both images with their ids, so
a later `docker pull` of the same tag is never confused with this run's.
It refuses a fixture or work directory on a network filesystem, and so a real library or a NAS: the
fixtures are synthetic only, as for adoption.

Settings (environment), each a whole number of seconds:

| Setting | Default | What |
|---|---|---|
| `ROUNDTRIP_READY_TIMEOUT` | 300 | how long a server may take to answer after its start |
| `ROUNDTRIP_SCAN_TIMEOUT` | 600 | how long a library scan, the export, Jellyfin's startup tasks or Jellyfin's answers may take (the adoption gate's scan timeout); five times that bounds the whole usage stage (two scans, each followed by a settle, and the probes) |
| `ROUNDTRIP_STOP_TIMEOUT` | 60 | how long a server is given to stop after TERM before docker kills it |
| `ROUNDTRIP_DOCKER_TIMEOUT` | 120 | how long any other docker call may take (a stop gets `ROUNDTRIP_STOP_TIMEOUT` more) |

## Stages and checks

| Stage | Check | Fails when |
|---|---|---|
| Ferrofin | ready | the server never answers `/System/Ping` and `/System/Info/Public` (it exited, named with its status, or the deadline passed) |
| | the library scan completes | `POST /Library/Refresh` does not end `Completed` |
| | boot log (generation, migrations, no ERROR) | as in the adoption gate (`adoption_check_boot_log`) |
| | usage stage, then Ferrofin's answers | an action fails (named), an action does not show in the answers, or no answers were written |
| | clean stop | the server does not exit 0 on TERM (judged only when every check before it passed) |
| Export | the export completes | `export-jellyfin`, run from the image under test in a container of its own on the stopped server's data directory, exits non-zero (its last line is the reason) or runs past `ROUNDTRIP_SCAN_TIMEOUT` |
| Jellyfin | ready | its start failed (`Main: Error while starting server`; it keeps answering 503, so the wait stops at that line), it exited (named with its status), or it is not ready within `ROUNDTRIP_READY_TIMEOUT`. Ready is `/System/Ping` and then `/System/Info/Public` answering: Jellyfin's setup server answers `/System/Info/Public` while Jellyfin still runs its last migrations and optimises the database, and 503 to everything else; the real server then answers only `/System/Ping` until its core startup completes |
| | its startup tasks finish | no scheduled task runs on two polls in a row, within `ROUNDTRIP_SCAN_TIMEOUT` |
| | every migration it began finished | a `Perform migration` with no `was successfully applied`, named with its SQLite error ([`checks_boot.py`](checks_boot.py)); a log with no entry at all fails too |
| | it ran the code migrations it runs on the source, and no schema migration | it applied a schema migration, which the exported history should record; or the code migrations it ran differ from those its own first start on the route's source ran, which `build-fixtures.sh` logged (`<fixture>.server.log` of the route's fourth field in `routes.sh`), so the fixtures must have been built with the Jellyfin image the gate runs. A history that records too much shows here: the code migrations it left out do not run |
| | no ERR or FTL outside the allowlist | an error entry other than the blocked plugin repository, up to the probes; an error a probe causes is judged by the answers instead |
| | answers equal Ferrofin's before the stop | `answers.txt` differs from `oracle.txt` through `adoption_normalise`, which leaves out ids, image sizes and the probes that differ by design; or the `userdata` lines differ with their item ids kept; or no answers came within `ROUNDTRIP_SCAN_TIMEOUT` |
| | integrity_check and foreign_key_check | as in the adoption gate (`adoption_check_db`), after Jellyfin stopped |

Jellyfin is booted only on an export of a copy Ferrofin finished with: when a Ferrofin or export check
fails, the route stops there.

### Controls

Two runs show that the gate fails on a wrong history. Neither is part of a run; they are done by hand
when the history rule or the checks above change:

- **The history left as the source had it:** run the gate with `--image` of a build whose export does not
  rewrite the history. Jellyfin's start fails on the `10.11.8` route, naming
  `20260113102337_AddLinkedChildrenTable` (`table "LinkedChildren" already exists`).
- **Every id recorded:** in a scratch copy of the repository's `gates/`, never in a commit, have
  `export_stage` insert every row of `crates/ferrofin-db/tests/data/jellyfin-12.4-fresh-history.tsv` the
  history lacks into `$dst/jellyfin/data/jellyfin.db` after the export (`INSERT OR IGNORE`). Jellyfin then runs none of its code migrations, and
  "it ran the code migrations it runs on the source" fails on the `10.11.8` route, naming them. Nothing
  else fails: since the export writes `encoding.xml`, Jellyfin logs no error for the routine it skipped.

## Several runs on one host

Runs do not share anything:

- Every run has an id, by default `<date>-<time>-<pid>` and always unique on the host while it runs.
- Its containers are `rt-<id>-<route>-ferrofin|export|jellyfin`.
- The image under test runs as the tag `ferrofin-roundtrip:<id>`, so a rebuild of `ferrofin:bench`
  mid-run changes nothing the run starts.
- Its run directory is `runs/roundtrip-<id>/`, claimed with a plain `mkdir`, so one that exists is
  refused. An id whose tag exists (another run holds it) is refused too.
- Every port is one docker picks (`-p 127.0.0.1::8096`).

There is no lock. On any exit a run removes only the containers whose ids docker wrote to its own
cidfiles, never one by a name another run could hold, and only the tag it made. Unlike the adoption gate
(fixed ports) and `gates/run` (`gates.lock`), two agents can run it at once.

## The usage-stage contract

The harness runs the usage stage as:

```bash
python3 gates/roundtrip/usage.py --base-url <url> --data-dir <dir> --oracle-out <file>
```

- `<dir>` is the copy the server runs on (it holds `data/jellyfin.db`). Credentials come from it at run
  time (its newest API key, and the account `smoke.sh` probes as, `ADOPTION_USER` when set, with that
  account's own web session for the playlist edits, which upstream refuses an API key) and are
  never written anywhere.
- The script performs its actions, scanning the library partway through and again at the end; after each
  scan has completed it waits until no scheduled task runs on two polls in a row (each within
  `ROUNDTRIP_SCAN_TIMEOUT` seconds, from the environment the harness gives it). It then
  captures Ferrofin's answers with [`answers.py`](answers.py), checks that each action shows in them, and
  writes them to `<file>`. The lines are written as captured; the harness normalises both files when it
  compares them.
- It exits 0, or non-zero with one line on stderr naming the action that failed (`usage: create the
  playlist Round trip playlist: HTTP 500`, or `usage: favorite <movie>: not in Ferrofin's answers (…)`
  for a write the server accepted and lost).
- Each action's request goes to `<file>.actions`, one `<action>  <HTTP status>` per line in order (a scan's
  status is `completed` or `failed`, a request that got no answer `no answer`), whether the stage passed,
  failed or was stopped at its deadline (TERM; the action in flight then has no line). The probes' own
  requests are not in it.
- The stage creates an API key, which is then the copy's newest: `smoke.sh` and `answers.py` probe through
  it, so Jellyfin must take a key Ferrofin made for every administrator probe after the return.

The answers file has one answer per line:

```
<status>  <metric>  <path>                                     smoke.sh's probes, as it prints them
userdata <user> <item id> played=<0|1> fav=<0|1> resume=<ticks>   what the IsPlayed, IsFavorite and IsResumable
                                                                filters return, for every user, sorted
userplays <user> <item id> count=<n> likes=<true|false|->        what the IsPlayed and Likes filters return, for
                                                                every user, sorted
user <name> admin=<0|1> disabled=… folders=… sessions=… …      every account's policy and settings, by name
badlogin <user> <status>                                       each account the stage creates with a password,
                                                                signing in with a wrong one first
login <user> <status> token=<yes|no>                           each account the stage creates (answers.GATE_USERS)
device <device id> name=<custom name>                          the device each of those sign-ins is
prefs <user> <sort by> <sort order> <custom prefs>             every account's home-screen display preferences
apikeys <app names>                                            the API keys
libraries <names>                                              the libraries (/Library/MediaFolders)
config <system|encoding|network> <setting>=<value> …           settings a dashboard shows
collection <Playlist|BoxSet> <item count> <name>               the probed account's playlists and collections
members <Playlist|BoxSet> <name> = <item names>                their items: a playlist's in order, a collection's
                                                                by name
image <Playlist|BoxSet> <name> <image types>                   which images they have (not their sizes: an
                                                                upload is stored 0x0 until gl-1054 is fixed)
item <name> lock=<0|1> fields=<locked fields> overview=<n>     the probed account's movies, by name
```

Lists inside a line are sorted unless they say otherwise. The sign-in probes write device rows, tokens and
login dates, so the capture is not read-only: it runs after anything that counts the rows a server changed.

The harness captures the same answers from Jellyfin (`answers.py --base-url --data-dir --out`) and
compares the two files, so **the oracle must be `answers.capture`'s output**, never lines a usage stage
writes itself. A probe added to `answers.py` is therefore captured and compared on both sides at once.
This README, not the issue text that first described the contract, is the contract: it adds the `login`
and later line kinds, and has the file written as captured. The harness bounds the whole stage at five
times `ROUNDTRIP_SCAN_TIMEOUT`. The actions, each checked in the answers it leads to:

- watch state: mark three movies played, favorite two, set a resume position on one, like one;
- accounts: create `roundtrip-user` with a password and `roundtrip-passwordless` without one; sign
  `roundtrip-user` in (a new device), play a movie to its end as it and name its device; narrow
  `roundtrip-passwordless`'s policy and change its settings; create an API key;
- change the probed account's display preferences;
- create a playlist (add one movie, remove one, move the last first) and a collection (add one, remove
  one), and a second of each to delete;
- add a library and remove the copy's home-videos library;
- scan the library and let the server settle, so the metadata refreshes those changes queued (no
  scheduled task shows them) have run before anything they could undo;
- rename the playlist, edit and lock a movie's metadata, give the collection a primary image;
- delete the second playlist and collection: the copy's media is mounted read-only, so the items the
  stage deletes are these, whose files live in the data directory;
- scan the library again, then capture.

A new action keeps this command line and these line kinds, and adds the probe that shows it to
`answers.py`.

## Layout

One file per stage and per check family, so later work adds files instead of growing one script:

| File | What |
|---|---|
| [`run.sh`](run.sh) | the run: its settings, names, cleanup and report, route by route |
| [`stage_ferrofin.sh`](stage_ferrofin.sh) | the Ferrofin stage |
| [`stage_export.sh`](stage_export.sh) | the export |
| [`stage_jellyfin.sh`](stage_jellyfin.sh) | the Jellyfin stage |
| [`routes.sh`](routes.sh) | the routes |
| [`lib.sh`](lib.sh) | run ids, names, containers, ports and the comparison |
| [`api.py`](api.py) | the HTTP client, the library scan and the settle |
| [`usage.py`](usage.py) | the usage stage |
| [`answers.py`](answers.py) | the answers both servers give |
| [`checks_boot.py`](checks_boot.py) | Jellyfin's log |
| [`report.py`](report.py) | `report.html` |

The gate reuses the adoption gate's parts: its fixtures, `smoke.sh`, `lib.sh` (`adoption_normalise`,
`adoption_check_boot_log`, `adoption_check_db`, `adoption_smoke_credentials`), the HTTP client in
`metadata.py` and the recorder and page of `adoption_report.py`.

## Tests

`make test-gates-offline` runs [`tests/`](tests) with no docker, image or fixture.

- [`tests/fake_docker`](tests/fake_docker) plays each container with
  [`tests/fake_server.py`](tests/fake_server.py), and the export with a copy, so whole routes run:
  - both routes passing every check on the export, Jellyfin running on the 10.11.8 route the code
    migrations the export left to run;
  - an export that leaves the history as it was (the 10.11.8 route fails with the migration named), one
    that records every id (no code migration runs), one that fails, one that never ends and a run stopped
    during the export;
  - a route whose first-start log is missing;
  - a Jellyfin whose setup server answers first;
  - a Jellyfin whose copy lost a write (the comparison fails), one that never answers (given up at
    the deadline) and one that exits (named with its status);
  - a Ferrofin that exits at once, and one that does not exit 0 on TERM;
  - a run stopped mid-stage (it removes its containers and its tag);
  - the refusals, and two runs at the same time sharing nothing, run directories included.
- Canned Jellyfin logs exercise `checks_boot.py`.
- The same fake server, in process, exercises `usage.py` and `answers.py` (a lost write included) and
  `api.py`'s settle.
