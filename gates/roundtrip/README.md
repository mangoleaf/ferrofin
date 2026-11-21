# Round trip: back to Jellyfin

Answers one question: **does the latest Jellyfin release start on what Ferrofin leaves behind, with
everything the user did in Ferrofin?** It is the proof the way back is built against. Until it passes on
every route, adoption stays one-way in every document.

Each route copies a pristine synthetic fixture. Ferrofin adopts and scans the copy, a scripted usage
stage changes it, and Ferrofin stops. Then `ferrofin-server export-jellyfin` from the image under test
exports the result, the Jellyfin release that `UPSTREAM_TAG` names boots on the export, and its answers
are compared with Ferrofin's.

| Route | Source (adopted as) | Ferrofin runs at | Jellyfin on the export |
|---|---|---|---|
| `10.11.8`, `10.11.9` | `jellyfin-10.11.8`, `jellyfin-10.11.9` (10.11.8) | `/data` | applies no schema migration (the exported history records the 20 that Ferrofin's own migrations already applied) and runs the 19 code migrations it runs on a database of that release of its own |
| `10.11.10`, `10.11.11` | `jellyfin-10.11.10`, `jellyfin-10.11.11` (10.11.11) | `/data` | applies none (the history records 18) and runs 18 code migrations; its own first start there runs `UpdateNormalizedUsername` too, which the source records: there the first schema migration it applies reverts the later ones, and `AddNormalizedUsername`'s `Down` drops that routine's row |
| `12.0` | `jellyfin-12.0` (12.0.0) | `/data` | applies none (the history records `MakeOwnerIdIndexesPartial`) and runs 5 code migrations |
| `12.1-from-10`, `12.1-from-12` | `jellyfin-12.1-from-10`, `jellyfin-12.1-from-12` (12.1.0) | `/data` | applies none (the history records `MakeOwnerIdIndexesPartial`) and runs 3 code migrations |
| `12.2-from-10`, `12.2-from-12`, `12.2-from-121`, `12.3-from-122`, `12.3-from-10`, `12.4-from-10` | the fixture of that name (12.2.0) | `/data` | runs no migration: the history is already the 12.2 set, which 12.3 and 12.4 keep |
| `12.4-from-123` | `jellyfin-12.4-from-123` (12.2.0) | `/config` | runs no migration, as above |
| `12.2-fresh`, `12.3-fresh`, `12.4-fresh` | that release's first run, not upgraded (12.2.0) | `/data` | runs no migration: the history is the fresh-install set (105 ids), which the export keeps as it is |
| `12.4-debian` | a 12.4 first run in a Debian package's layout (`/var/lib/jellyfin`, `/etc/jellyfin`; 12.2.0) | `/var/lib/ferrofin`, its configuration at `/etc/ferrofin` | runs no migration; the export reads Ferrofin's two directories and writes the docker layout, the only one it writes |
| `10.11.11-messy`, `12.4-messy` | `messy.py`'s copies of `jellyfin-10.11.11` and `jellyfin-12.4-from-123`, with the rows of shapes no API makes (10.11.11, 12.2.0), on `media-messy/` | `/data` | as on its source's route |
| `10.11.11-scale` | `scale.py`'s copy of `jellyfin-10.11.11`, cloned up to 40,000 items (10.11.11), on `media-scale/` | `/data` | as on the `10.11.11` route; the time each stage took and the export's size are on its line, recorded, not gated. Ferrofin adopts it and stops with no scan and no usage stage, and Jellyfin starts, stops and starts again with no playlist refresh, answers, probes or scan ([Stages and checks](#stages-and-checks) lists what runs): the clones' files are hard links to their templates', so a scan re-identifies and re-extracts every clone, which measures the fixture rather than the way back (the adoption gate scans it neither). Jellyfin's time on its line covers both starts and the copies and row diffs of its windows. It needs a few GB free under the run directory (the export alone came to 1.6 GB on the development host, and a failed route keeps four copies of its database, about 270 MB each) and, unlike the adoption gate's scale row, checks no free space first |

The routes are in [`routes.sh`](routes.sh): one record per route, every fixture `build-fixtures.sh` builds, with
everything that makes a route differ in its own fields (its fixture, the generation Ferrofin adopts it as, the
log of Jellyfin's own first start on its source, the data directory Ferrofin runs at, its configuration directory
when apart, and its kind, which names its media tree). `run.sh` needs no change for a new route: a fixture added
later gets a record, and [`tests/test_routes.py`](tests/test_routes.py) fails until it has one.
Open work: two sources have no route yet, so a green run does not cover every supported source. A database that
never came from Jellyfin (a Ferrofin-native route, gl-1552) needs a seeding step that makes its API key,
administrator and web session; and no route starts from a database an earlier Ferrofin release wrote, whose rows
keep the forms of the writers of their day.
Jellyfin always runs at `/config`, its image's layout. Ferrofin runs at
`/data`, its own image's directory, on every route but `12.4-from-123` and `12.4-debian`, and the export reads
it there, so the export must rewrite every stored path from `/data` onto `/config`: with source and output both
at `/config`, a missed rewrite could not show. The Debian-layout route is a source layout: Ferrofin adopts that
fixture into its own package directories, its configuration apart, as the adoption gate does, and the export
still writes the docker layout. Without the history rewrite, Jellyfin's start fails on the 10.11 routes:
`migration 20260113102337_AddLinkedChildrenTable failed: SQLite Error 1: 'table "LinkedChildren" already exists'`;
on the 12.0 and 12.1 routes it applies `20261002210711_MakeOwnerIdIndexesPartial`, which the check below fails;
the 12.2 and later routes have nothing to rewrite.

### When to run it

Run it for any change to the export command, the configuration writers, the history table, the migration chain
or a writer of a Jellyfin-owned table, on the routes the change touches (`--only`), and attach the report's
verdict table to the pull request. Every release runs it on every route: the `Adoption` workflow
(`.github/workflows/adoption.yml`) runs it on the frozen release commit after the adoption gate, and a failure
there blocks the release like an adoption failure ([`RELEASE.md`](../../RELEASE.md), "Validate adoption").

## Running it

```bash
export FERROFIN_GATES_DIR=/srv/gates          # the gates' output root (../README.md)
gates/adoption/build-fixtures.sh              # the fixtures, once (the adoption gate's own)
docker build --build-arg SOURCE_COMMIT="$(git rev-parse HEAD)" -t ferrofin:bench .
docker pull jellyfin/jellyfin:12.4            # the release UPSTREAM_TAG names
gates/roundtrip/run.sh                        # every route; --only 10.11.8,12.0 for some
```

`make run-roundtrip` (`gates/run roundtrip`) runs it as a gates session instead, after the session's preflight,
into the session's `roundtrip/` folder, and the session's report reads its `result.json`.

It prints one `PASS`/`FAIL` line per route listed, and the reasons (a missing fixture fails its route, never
skips it), then how many of them passed. It exits 1 if a route failed, and 2 if the run was refused: an unknown
route in `--only`, or no route at all. The run directory, `$FERROFIN_GATES_DIR/runs/roundtrip-<run id>/`, holds
`report.html` (every route, stage and check, built however the run ends), `result.json` (the same, in the
envelope every gate writes, [`../result.py`](../result.py), gate `roundtrip`) and `results.jsonl` (the
events, the adoption report's format). Each route also has a folder with:

| File | What |
|---|---|
| `oracle.txt` | Ferrofin's answers before the stop (the usage stage's) |
| `oracle.txt.actions` | each usage-stage action's request and its HTTP status |
| `export.log` | what the export printed |
| `answers.txt`, `answers-scanned.txt`, `answers-again.txt` | Jellyfin's answers after the return, after its scan, and after its second start |
| `probes.txt` | how many of each kind of file Jellyfin served with 200, of those its database lists |
| `rows-<window>.txt`, `rows-<window>.txt.jsonl.gz` | what each window changed in Jellyfin's database ([below](#the-row-diff-and-the-allowlist)): the summary and every unexplained row, then every change with its values and the entries that explained it |
| `ferrofin-boot.log`, `ferrofin.log` | Ferrofin's log after the scan, and its whole log |
| `jellyfin-boot.log`, `jellyfin.log` | Jellyfin's log up to its startup tasks, and its whole log |
| `jellyfin-again-boot.log`, `jellyfin-again.log` | the same of its second start |
| `ferrofin/`, `ferrofin-config/`, `jellyfin/`, `db/` | the copy Ferrofin ran on (its configuration apart on the Debian-layout route), the export Jellyfin ran on, and the copies of Jellyfin's database at the end of each window, kept only when the route failed |
| `ferrofin.cid`, `export.cid`, `jellyfin.cid`, `jellyfin-again.cid` | while a container runs, the id docker gave it |

Options: `--image IMAGE` (default `ferrofin:bench`), `--jellyfin-image IMAGE` (default
`jellyfin/jellyfin:<minor of UPSTREAM_TAG>`), `--fixtures DIR` (default the adoption fixtures under the
output root), `--only ROUTE[,ROUTE…]`, `--run-id ID` and `--work DIR` (the run directory). The gate never builds
or pulls an image. It runs Jellyfin by its image id, and the report names both images with their ids, so
a later `docker pull` of the same tag is never confused with this run's.
It refuses a fixture or work directory on a network filesystem, and so a real library or a NAS: the
fixtures are synthetic only, as for adoption.

Settings (environment), each a whole number of seconds:

| Setting | Default | What |
|---|---|---|
| `ROUNDTRIP_READY_TIMEOUT` | 300 | how long a server may take to answer after its start |
| `ROUNDTRIP_SCAN_TIMEOUT` | 600 | how long a library scan, the export, Jellyfin's startup tasks at either start (and Ferrofin's on the scale route), the tasks after its scan, its playlists' refresh, Jellyfin's answers, its probes or a window's row diff may take (the adoption gate's scan timeout); five times that bounds the whole usage stage (two scans, each followed by a settle, and the probes) |
| `ROUNDTRIP_STOP_TIMEOUT` | 60 | how long a server is given to stop after TERM before docker kills it |
| `ROUNDTRIP_DOCKER_TIMEOUT` | 120 | how long any other docker call may take (a stop gets `ROUNDTRIP_STOP_TIMEOUT` more) |

## Stages and checks

| Stage | Check | Fails when |
|---|---|---|
| Ferrofin | ready | the server never answers `/System/Ping` and `/System/Info/Public` (it exited, named with its status, or the deadline passed) |
| | the library scan completes | `POST /Library/Refresh` does not end `Completed` |
| | its startup tasks finish | the scale route only, in place of the scan and the usage stage: no scheduled task runs on two polls in a row within `ROUNDTRIP_SCAN_TIMEOUT` |
| | boot log (generation, migrations, no ERROR) | as in the adoption gate (`adoption_check_boot_log`) |
| | usage stage, then Ferrofin's answers | an action fails (named), an action does not show in the answers, or no answers were written |
| | clean stop | the server does not exit 0 on TERM (judged only when every check before it passed) |
| Export | the export completes | `export-jellyfin`, run from the image under test in a container of its own on the stopped server's data directory, exits non-zero (its last line is the reason) or runs past `ROUNDTRIP_SCAN_TIMEOUT` |
| | every stored path is in the export, none under the directory Ferrofin ran at | a path in `BaseItems.Path`, `BaseItemImageInfos.Path`, `Chapters.ImagePath`, `MediaStreamInfos.Path` or `ImageInfos.Path` of the exported database does not resolve as Jellyfin's image reads it (`/config/…` in the export, `%AppDataPath%` and `%MetadataPath%` under its `data` and `metadata`, `/media/…` in the route's media tree), or lies under a directory Ferrofin ran at (`/data`, and its configuration directory where it is apart) or the copy's own path ([`checks_paths.py`](checks_paths.py) `stored`). A path the same column of the database Ferrofin stopped with already stored without a file where it points, in the copy or the media (a person whose folder Jellyfin never made, a file gone from the media), is not the export's to resolve and is left out of that half of the check; an absolute path outside both never is. Checked before Jellyfin opens the file |
| | its database has Jellyfin's schema, and its loss report every table of Ferrofin's that held rows | the exported database's tables (each column's name, type, `NOT NULL`, default and key) and indexes (columns and uniqueness; a partial index's `WHERE` aside) differ from Jellyfin's pinned schema (`RT_SCHEMA`, the 12.2 dump: 12.3 and 12.4 add no schema migration), as the export's own test compares them, so a table or index of Ferrofin's left behind fails; or a table of Ferrofin's (`Ferrofin*`, `_sqlx_migrations`) that held rows in the database the export read has no `  <table> (<n> rows): ` line in the export's `ferrofin-export-report.txt`; or that database holds no table of Ferrofin's (nothing to find in the report) ([`checks_db.py`](checks_db.py) `export`) |
| Jellyfin | ready | its start failed (`Main: Error while starting server`; it keeps answering 503, so the wait stops at that line), it exited (named with its status), or it is not ready within `ROUNDTRIP_READY_TIMEOUT`. Ready is `/System/Ping` and then `/System/Info/Public` answering: Jellyfin's setup server answers `/System/Info/Public` while Jellyfin still runs its last migrations and optimises the database, and 503 to everything else; the real server then answers only `/System/Ping` until its core startup completes |
| | its startup tasks finish | no scheduled task runs on two polls in a row, within `ROUNDTRIP_SCAN_TIMEOUT` |
| | every migration it began finished | a `Perform migration` with no `was successfully applied`, named with its SQLite error ([`checks_boot.py`](checks_boot.py)); a log with no entry at all fails too |
| | it ran the code migrations it runs on the source, and no schema migration | it applied a schema migration, which the exported history should record; or the code migrations it ran differ from those its own first start on the route's source ran, which `build-fixtures.sh` logged (the route's fourth field in `routes.sh`, `<it>.server.log` under the fixtures: a 12.4 fixture's build on that source, or the first start it gives the source, `oracle/first-start-12.4/<fixture>.server.log`), less those the source's history records (they ran there only after a revert, as on the 10.11.10 route), so the fixtures must have been built with the Jellyfin image the gate runs. A history that records too much shows here: the code migrations it left out do not run |
| | no ERR or FTL outside the allowlist | an error entry outside `checks_boot.py`'s `ALLOWED` (each with its reason: the plugin repository and a remote metadata provider behind the unreachable proxy, and the answers' own wrong-password sign-in), up to its startup tasks |
| | every row its start changed is explained | a row of its database added, removed or changed between the export as written and the end of its start that no allowlist entry explains ([below](#the-row-diff-and-the-allowlist)) |
| | every playlist's full metadata refresh completes | a playlist's `DateLastRefreshed` in its database has not moved within `ROUNDTRIP_SCAN_TIMEOUT` of `POST /Items/{id}/Refresh?metadataRefreshMode=FullRefresh` (jellyfin-web's "Refresh metadata", replacing nothing, for the metadata only), sent for every playlist its database holds ([`api.py`](api.py) `refresh-playlists`); or its database holds no playlist (the check would refresh nothing); or a refresh request is refused or unanswered. A full refresh runs every metadata reader whatever the file's time, so a stale `playlist.xml` in the export would merge its shares back (`PlaylistMetadataService.MergeData`), which the answers then show |
| | answers equal Ferrofin's before the stop | `answers.txt` differs from `oracle.txt` through `adoption_normalise`, which leaves out ids, image sizes and the probes that differ by design; or the `userdata` lines differ with their item ids kept; or no answers came within `ROUNDTRIP_SCAN_TIMEOUT` |
| | it serves every image, trickplay tile, subtitle and attachment its database lists | one of the files its database lists is not answered with 200: each item's each image type, each chapter image, each profile image, the first tile of each trickplay width, each external text subtitle and each attachment; or a file the database Ferrofin stopped with lists is no longer listed for an item or user Jellyfin still holds (a row Jellyfin dropped because the file was missing; an item it removed whole is judged by the answers and the library check instead); or neither database lists a file at all while Jellyfin's has an image table ([`probes.py`](probes.py), which writes `probes.txt`) |
| | every row the gate's requests changed is explained | the same, from the end of its start to the end of the probes: the playlist refresh, the answers and the probes |
| | its library scan completes, and its tasks after it | `POST /Library/Refresh` does not end `Completed`, or a scheduled task still runs after `ROUNDTRIP_SCAN_TIMEOUT` |
| | every row its scan changed is explained | the same, over the scan |
| | its scan kept every item, and the item counts by type | the number of `BaseItems` rows of a type differs before and after the scan, or an item is gone, or one was re-added: another id at a removed item's type and path, or the same id with another `DateCreated` ([`checks_db.py`](checks_db.py) `counts`) |
| | answers after its scan equal Ferrofin's before the stop | `answers-scanned.txt`, captured after the scan, differs from `oracle.txt` as above: a row the scan rewrote must not change an answer either |
| | it stops cleanly | it does not exit 0 on TERM within `ROUNDTRIP_STOP_TIMEOUT` (judged when it answered) |
| | no ERR or FTL after its startup outside the allowlist | an error entry outside `ALLOWED` in the rest of its log: the gate's requests, the scan and the stop |
| | every row changed from then until its stop is explained | the same, from the end of the scan (on the scale route, of its start) to the file it stopped with: the answers again, and the stop |
| | integrity_check and foreign_key_check | as in the adoption gate (`adoption_check_db`), after Jellyfin stopped |
| | every library keeps its folder row and DateCreated through its scan | a library folder row (`CollectionFolder`) of the database Ferrofin stopped with is gone from Jellyfin's, or has another `DateCreated`: Jellyfin's scan deletes a library whose stored folder does not exist, and with it the library's playlist and collection entries ([`checks_paths.py`](checks_paths.py) `libraries`) |
| Jellyfin again | ready, its startup tasks finish | as for its first start, on the file its first start left |
| | it ran no migration | it began a migration (`Perform migration`): its first start applied every one it runs, so a second that runs one repairs something ([`checks_boot.py`](checks_boot.py) `quiet`). Jellyfin's start has no other repair; anything else a second start changes shows in its row diff, the next check |
| | every row its start changed is explained | as for its first start, from the file its first start left to the end of its second start |
| | answers equal Ferrofin's before the stop | `answers-again.txt` differs from `oracle.txt`, as above |
| | it stops cleanly | as for its first start |
| | no ERR or FTL outside the allowlist | an error entry outside `ALLOWED` in its whole log |
| | integrity_check and foreign_key_check | as for its first start |

Jellyfin is booted only on an export of a copy Ferrofin finished with: when a Ferrofin or export check
fails, the route stops there, and only a Jellyfin that answered is started again. On the scale route there are no
answers of Ferrofin's to compare, and no scan (the scale route's line in the routes table says why), so Jellyfin's
checks there are its starts, their logs, the rows its start, the rest until its stop and its second start changed,
its stops and its file; the playlist refresh, the answers, the probes, the scan with its row diff and its counts,
and the library rows do not run.

What a pass covers: after its first start, its playlists' full refresh and its first library scan, and after a
second start on the same file, the Jellyfin release under test answers as Ferrofin did before its stop, serves the
files its database lists, keeps every item and library, and changes no row the allowlist does not explain.

### The row diff and the allowlist

Jellyfin's database is copied (`checks_db.py snapshot`, SQLite's backup, which a running server does not tear) at
the end of each window: its start (from the export as written), the gate's requests (playlist refresh, answers,
probes), its scan, the rest until its stop, and its second start. Each window's copy is compared with the one
before it, table by table, every column as stored (text byte for byte whatever its collation, numbers by value), so
a rewrite to another casing or form shows. A row is keyed by its
primary key: one only before is removed, one only after added, one in both changed, with the columns that changed. `BaseItemImageInfos`, which Jellyfin re-inserts under a new `Id` on every
save, is also paired by item, type and path (`checks_db.py` `NATURAL`).

Every change must be explained by an entry of [`allowlist.py`](allowlist.py) `RULES`: the windows it applies in,
the table, the change, for a changed row the columns it may change (for a number, within a relative tolerance;
for a JSON object such as `BaseItems.Data`, only in the keys it names, at least one of which changed: the same
object written in another order is not a stamp's doing), the rows it applies to (`where`: a
regular expression a column's whole value must match), its kind and its cause. A changed row is explained when
every column it changed is covered. The kinds are `timestamp` (a stamp or key Jellyfin writes anew on every save
or refresh, whatever changed), `session` (what presenting a token writes), `gate` (what the gate's own requests
write), `checked` (a change another check judges one by one, the migrations its start records) and `allowlisted`
(a difference the owner accepted, or an audit verdict). The gate's own writes are told apart from Jellyfin's in
two ways: by window (the answers, probes and playlist refresh run only in the "gate's requests" and "until its
stop" windows; the start, scan and second start windows have no request of the gate's but its polls of the
scheduled tasks and the scan's own `POST /Library/Refresh`), and by the `gate` entries, which apply only in those
two windows and only to the accounts and devices the answers sign in (`answers.GATE_USERS`, `answers.DEVICE_ID`). A change the gate's requests caused
Jellyfin to make, such as a playlist refresh rewriting a playlist row, is Jellyfin's and must be explained like
any other.

The entries today are the artefact rules, each pointing at the upstream code that writes it (the save and refresh
stamps only in the first start's windows: a second start that saves or refreshes an item fails), and two owner
decisions: a trickplay `TrickplayInfos.Bandwidth` within 5% (gl-1596) and `DateLastSaved` on guide programmes
(gl-1628, within the general `DateLastSaved` rule). Differences the audits judge are added as `allowlisted`
entries citing their verdict. An entry is checked before every run (`allowlist.py check --schema
"$RT_SCHEMA"`; `run.sh` refuses a run when one fails): it names a table and columns Jellyfin's schema has, a
window, change and kind the file knows, and a cause (an `allowlisted` one naming its issue), a tolerance only on a
number, JSON keys only on text, and a name of its own, which the row diff's files use.
`rows-<window>.txt` counts the rows of each table, change and set of columns by the entries that explained them (each
by its kind and name; the `.jsonl.gz` gives every change the name, kind and cause of each), so an entry that never
appears there explained nothing in that window.

A failed route keeps its copies under `db/`, so the diff runs again with another allowlist without another run:

```bash
python3 gates/roundtrip/checks_db.py rows --window scan --before <route>/db/captured.db \
  --after <route>/db/scanned.db --out /tmp/rows-scan.txt
```

The windows' copies are `export`, `started`, `captured`, `scanned`, `stopped` and `restarted` (`stop` compares
`scanned`, or `started` on the scale route, with `stopped`; `restart` compares `stopped` with `restarted`).
Metadata refreshes a scan queues are no scheduled task, so one still running when the scan's tasks have settled
lands in the next window.

### Controls

These runs show that each check can fail. None is part of a run; they are done by hand when the history rule or
the checks above change:

- **The history left as the source had it:** run the gate with `--image` of a build whose export does not
  rewrite the history, or, in a scratch copy of `gates/`, have `export_stage` copy the history of
  `$dst/ferrofin/data/jellyfin.db` over the export's after the export. Jellyfin's start fails on the four 10.11
  routes, naming `20260113102337_AddLinkedChildrenTable` (`table "LinkedChildren" already exists`); on the
  `12.0` and both `12.1` routes it starts, and "it ran the code migrations it runs on the source, and no schema
  migration" fails, naming `20261002210711_MakeOwnerIdIndexesPartial`; the 12.2 and later routes pass.
- **Stored paths left as the source had them:** run the gate with `--image` of a build whose export does
  not rewrite the stored paths. On the `10.11.8` route "every stored path is in the export, none under the
  directory Ferrofin ran at" fails, naming `/data/…` paths.
- **A row the allowlist does not explain, an item the scan removes:** in a scratch copy of `gates/`, have
  `export_stage` point a movie's `BaseItems.Path` in `$dst/jellyfin/data/jellyfin.db` at a file the media does not
  have, after its checks. "every row its scan changed is explained" fails naming the movie's rows, and "its scan
  kept every item" fails naming the movie.
- **A second start that repairs something:** in a scratch copy of `gates/`, have `jellyfin_again` delete one
  recorded code migration from the history before it starts Jellyfin again. "it ran no migration" fails, naming it.
- **An unclean stop:** `ROUNDTRIP_STOP_TIMEOUT=1`; Jellyfin is killed before it stops, and "it stops cleanly"
  fails with status 137.
- **A missing configuration document:** in a scratch copy of `gates/`, have `export_stage` delete
  `$dst/jellyfin/config/encoding.xml`. Jellyfin falls back to its defaults, and every answers comparison fails on
  the `config encoding` line (`H264Crf=21` against 23).
- **A table of Ferrofin's left in the export, or one the loss report misses:** in a scratch copy of `gates/`, have
  `export_stage` create a `FerrofinLeft` table in the exported database, or delete a table's line from
  `ferrofin-export-report.txt`, before the export's checks. Its database check fails naming the table.
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
(fixed ports), two agents can run `run.sh` directly at once; `make run-roundtrip` and `gates/run` take
`gates.lock` as for every session.

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
virtualfolder <name> type=<type> locations=<paths> <option>=…  each library as /Library/VirtualFolders reads it from
                                                                the libraries' folders, shortcut files and options
                                                                ("<data>": the server's ProgramDataPath,
                                                                /System/Info; the options answers.LIBRARY_OPTIONS
                                                                names)
config <document> <setting>=<value> …                          settings a dashboard shows, per document: system,
                                                                encoding, network, branding, metadata, xbmcmetadata
                                                                (answers.CONFIGURATION; livetv is not compared until
                                                                the export writes livetv.xml, gl-1556)
collection <Playlist|BoxSet> <item count> <name>               the probed account's playlists and collections
members <Playlist|BoxSet> <name> = <item names>                their items: a playlist's in order, a collection's
                                                                by name
image <Playlist|BoxSet> <name> <image types>                   which images they have (not their sizes: an
                                                                upload is stored 0x0 until gl-1054 is fixed)
shares <owner> <playlist> = <user>:<can edit 0|1>,…            synthetic-adult's playlists (answers.SHARE_OWNER,
                                                                through its own web session in the copy), each with
                                                                the accounts it is shared with, "-" for none,
                                                                "error=<status>" when refused (one it sees but does
                                                                not own); "shares <owner> no session" when the copy
                                                                has no web session of the owner's
item <name> lock=<0|1> fields=<locked fields> overview=<n>     the probed account's movies, by name
stream <movie> <status> bytes=<n> sha256=<hex>                 the first movie by sort name played directly
                                                                (/Videos/<id>/stream?static=true): the first 64 KiB
                                                                of its file, how many came and their hash
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
- rename the playlist, edit and lock a movie's metadata, give the collection a primary image, and change one
  setting of each document the `config` lines compare from its default (`usage.SETTINGS`: the library monitor's
  delay, a transcode quality, UDP discovery, the sign-in disclaimer, how a new file's date added is read, NFO extra
  thumbs), so an export that loses a document, or a Jellyfin that cannot read one and falls back to its defaults,
  answers otherwise (the library the stage adds turns its realtime monitor off, which its `virtualfolder` line shows);
- delete the second playlist and collection: the copy's media is mounted read-only, so the items the
  stage deletes are these, whose files live in the data directory;
- remove every share of the playlist synthetic-adult shared in Jellyfin (the fixtures' `Synthetic mixed
  order`, shared with synthetic-child), as its owner: its `playlist.xml` still lists the share (the stage
  fails when it does not, as the scenario would test nothing), and the export must not carry the file to
  Jellyfin (gl-1903);
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
| [`api.py`](api.py) | the HTTP client, the library scan, the settle and the playlists' refresh |
| [`usage.py`](usage.py) | the usage stage |
| [`answers.py`](answers.py) | the answers both servers give |
| [`checks_boot.py`](checks_boot.py) | Jellyfin's log |
| [`checks_paths.py`](checks_paths.py) | the exported database's stored paths, and the library rows after Jellyfin's scan |
| [`checks_db.py`](checks_db.py) | the exported database's schema and loss report, the copies of Jellyfin's database, the row diff of each window, and the items its scan kept |
| [`allowlist.py`](allowlist.py) | what Jellyfin may change in a window, each entry with its cause, and the check of the entries |
| [`probes.py`](probes.py) | the files Jellyfin serves from the export |
| [`report.py`](report.py) | `report.html` |

The gate reuses the adoption gate's parts: its fixtures, `smoke.sh`, `lib.sh` (`adoption_normalise`,
`adoption_check_boot_log`, `adoption_check_db`, `adoption_smoke_credentials`), the HTTP client in
`metadata.py` and the recorder and page of `adoption_report.py`.

## Tests

`make test-gates-offline` runs [`tests/`](tests) with no docker, image or fixture.

- [`tests/fake_docker`](tests/fake_docker) plays each container with
  [`tests/fake_server.py`](tests/fake_server.py), and the export with a copy, so whole routes run:
  - the `10.11.8` and `12.4-from-123` routes (`--only`) passing every check on the export, Jellyfin
    running on the 10.11.8 route the code migrations the export left to run;
  - an export that leaves the history as it was (the 10.11.8 route fails with the migration named), one
    that records every id (no code migration runs), one that fails, one that never ends and a run stopped
    during the export;
  - a route whose first-start log is missing;
  - a Jellyfin whose setup server answers first;
  - a Jellyfin whose copy lost a write (the comparison fails), one whose playlist refresh shares a
    playlist again (the comparison fails on its `shares` line), one that never answers (given up at the
    deadline) and one that exits (named with its status);
  - a Jellyfin whose scan removes an item (its row diff and its counts fail, and its copies stay), one killed
    at its stop, one that logs an error after its startup, one whose second start migrates (each named), and
    an export that leaves a table of Ferrofin's, misses one in its loss report (Jellyfin never boots) or loses
    the settings (every answers comparison fails);
  - a Ferrofin that exits at once, and one that does not exit 0 on TERM;
  - a run stopped mid-stage (it removes its containers and its tag);
  - the refusals (no route to run among them), and two runs at the same time sharing nothing, run directories
    included;
  - a route of each other kind: the Debian-layout route, its configuration directory moved apart for
    Ferrofin and its export, a messy route on its own media tree (or failing without it) and the scale route,
    its time and size on its line; and `result.json`, the round trip's in the gates' envelope.
- [`tests/test_routes.py`](tests/test_routes.py) checks `routes.sh` against the fixtures `build-fixtures.sh`
  builds: every fixture has a route, and each route its fixture, the generation Ferrofin adopts it as, the
  first-start log it returns to, a configuration directory exactly where the fixture was written in another
  layout, and its kind (a messy or the scale copy, or a path).
- Canned Jellyfin logs exercise `checks_boot.py`, the second start's and the rest of the log's checks included.
- Databases shaped as Jellyfin's tables exercise `checks_paths.py` and what `probes.py` probes; databases with
  Jellyfin's pinned schema exercise `checks_db.py` (a schema or a loss report that misses a table, each kind of
  change explained or not, a tolerance, JSON keys, an image row saved under a new id, a removed or re-added item,
  copies with nothing to compare) and `allowlist.py`'s check of the entries (each kind of entry that cannot apply).
- The same fake server, in process, exercises `usage.py` and `answers.py` (a lost write included, a copy
  with no shared playlist or no stale `playlist.xml`, an owner with no session) and `api.py`'s settle and
  playlist refresh (waited for, never landing, another mode, no playlist at all).
