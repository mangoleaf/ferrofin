# Gates

The live checks that validate a Ferrofin build, one folder each. Each answers one question, and its
own README defines its numbers:

| Gate | Answers | Needs |
|---|---|---|
| [`verify/`](verify/README.md) | Does a library scan reprocess only what changed? | the image; `ffmpeg` for the scratch clips |
| [`adoption/`](adoption/README.md) | Does every supported Jellyfin database adopt, intact? | the synthetic fixtures (`adoption/build-fixtures.sh`) |
| [`parity/`](parity/README.md) | Do Jellyfin and Ferrofin answer jellyfin-web's requests alike? | the benchmark corpus, prepared for Jellyfin |
| [`bench/`](bench/README.md) | How fast, and how much memory, against Jellyfin or the previous release? | the benchmark corpus; idle cores |

## The output root

Everything a gate creates goes under one directory, `FERROFIN_GATES_DIR`: an absolute path on a local
disk, outside the repository (a root inside it would reach `docker build .`). Nothing is passed by hand:

```
$FERROFIN_GATES_DIR/
  fixtures/      inputs built once: adoption-<key> (adoption/build-fixtures.sh), bench-testdata (bench/testdata/build.sh)
  work/<gate>/   a gate's scratch, reused by its next run
  runs/          one directory per run, never reused: a gates/run session, or <gate>-<name> for a gate run on its own
  gates.lock     held by the gates/run session in progress
```

[`lib.sh`](lib.sh) resolves and checks it. A directory passed to a gate explicitly (`--fixtures`,
`--testdata`, `--out`, `ADOPTION_WORK_DIR`) wins over it.

## Running them

`gates/run` validates one commit with one image, all four gates or the ones named:

```bash
export FERROFIN_GATES_DIR=/srv/gates
gates/run all --dry-run          # the preflight and every gate's command; nothing starts
docker build --build-arg SOURCE_COMMIT="$(git rev-parse HEAD)" -t IMAGE .   # the image the preflight names
gates/run all                    # verify, adoption, parity, bench, in that order
gates/run adoption               # one gate: a session all the same
gates/run bench --previous ghcr.io/mangoleaf/ferrofin:vPREV   # bench as the release A/B gate
```

The image defaults to `ferrofin:<the commit, as git describe names it>`, and must carry the checked-out
commit in its revision label (the `--build-arg` above). Before any gate starts, a preflight names
every cause that would stop it: a missing tool, an image that is missing or built from another commit,
a fixture not built yet, another session holding `gates.lock`, cores that are not idle, or a container
another run left behind (never removed: it is not this session's). One session runs per output root,
holding its `gates.lock`; sessions under two roots on one host are kept apart only by those container
and idle-core checks, so keep one root per host.

Each run is a session folder, `runs/<branch>-<YYYYMMDD-HHMM>/` (a rerun in the same minute gets
`-run2`): `session.json` records the branch, commit, images, the Jellyfin oracle pin and each gate's
status, times and exit status, beside each gate's own run directory and its `<gate>.log`. Every gate,
the release A/B gate included, runs against the Jellyfin release the branch pins: when a newer one is
out, the preflight notes it and `session.json` records the note with the pin, and nothing is refused
for it ([`bench/oracle.py`](bench/oracle.py)). A gate that
fails does not stop the next; `gates/run` exits 1 when one failed and 2 when the preflight refused.
Each gate's own script still runs on its own; its README says how.

Stop a run with Ctrl-C or a TERM: it stops at once, removes what it started and still writes its
report, and exits 130 or 143. `gates/run`, `verify/run.sh`, `verify/scan-behaviour.sh`, `parity/run.sh`
and `bench/run.sh` hand the run to a worker in a session of its own and only wait for it, then pass a
signal on as a TERM, again every second until the worker's cleanup has started ([`lib.sh`](lib.sh),
`gates_supervise`): bash 5.2 drops a trapped signal that lands while it parses a `$( … )`. So what a
run records (`session.json`, a report) says 143 for a Ctrl-C too. Under `set -e` (`parity/run.sh`, and
bench's server) bash 5.2 does not drop that signal but exits at once with status 2. The cleanup still
runs and the run still ends with 143, but the phase or report reason it writes then says exit 2. A
SIGKILL cannot be passed on, so never stop a run with `kill -9`: its worker would run on. Ctrl-Z
suspends only the waiting shell.
`adoption/run.sh` hands its run to a worker too, but its own supervisor stops the worker outright and
builds the report itself ([its README](adoption/README.md)).

The root [`Makefile`](../Makefile) offers the same as targets: `make run-gates` for all four, and
`make run-verify`, `make run-adoption`, `make run-parity` or `make run-bench` for one, with `IMAGE=`
and `PREVIOUS=` passed through from the command line (`make run-gates IMAGE=ferrofin:v1.4.0-rc
PREVIOUS=ghcr.io/mangoleaf/ferrofin:v1.3.2`); a bare `make` only lists them. `make test-gates-offline`
runs the gates' own tests, which need no docker, server or fixtures, as CI's shell-tests job does:
shellcheck, the bats tests and every gate's Python tests. They need `bats`, `shellcheck`, `sqlite3`,
`jq`, `curl` and Python 3.11 or newer with Pillow, PyICU and PyYAML (on Debian or Ubuntu:
`bats shellcheck sqlite3 jq curl python3-pil python3-icu python3-yaml`). There is no `make test`: it
would read as the cargo tests.

## The session report

However a session ends, `gates/run` writes its report, `index.html` and the same as `report.md`
([`report.py`](report.py), which writes it again for any session: `python3 gates/report.py SESSION`).
It shows the commit, images and oracle pin (with its note on a newer release), and one row per gate,
every gate included:

| Gate | Counts | Links to |
|---|---|---|
| verify | checks passed, failed and skipped, over both libraries, from its `result.json` | the verify report; each library's rows, `result.json`, `verify.json` and the server log |
| adoption | from its `result.json`: fixture rows passed, failed, unfinished and skipped, by the adoption report's own rules | the adoption report |
| parity | from its `result.json`: new differences, the groups of them not filed yet, requests sent and planned; the verdict and its reason | the parity report |
| bench | from its `result.json`: phases completed, failed, unfinished and skipped; for the A/B gate (`--previous`), the gated cells regressed, not comparable, improved and within noise, and whether anything blocks | the benchmark or A/B gate report |

A gate the session did not ask for, did not reach, is still running, ended before writing its result,
or left a result that cannot be read is shown as such, never left out. Each gate's own report is its
main link; its raw files and the log follow it. The page opens from disk (every
link is relative) and replaces none of the gates' own reports.

Every gate is moving to one structured result, `<run>/result.json`, in the envelope
[`result.py`](result.py) defines and validates: `schema`, `gate`, `verdict`, `counts`, `source`
(`commit`, `image`, `image_digest`, `oracle`, `host_class` (cores and memory, never a host name),
`fixture`), `started`, `ended`, and `detail`, the gate's own data. Its HTML report, this page and the
viewer read that file, never the gate's internal files. Every gate writes one.

The page also shows the README's graphs for the session's own results ([`publish.py`](publish.py), inline,
in the reader's light or dark theme): the adoption grid, the parity overview, and for a Jellyfin-comparison
benchmark the speed and memory graphs of that one run.

## Publishing the results

The README's Results section shows each release's numbers against Jellyfin, drawn from one small file per
release that stays in the repository, `docs/results/<version>/summary.json`. The release operator chooses the
local runs it is made from; nothing is picked automatically, and nothing here runs a gate:

```bash
make publish-results VERSION=v1.4.0 SESSION="$FERROFIN_GATES_DIR/runs/<session>" BENCH="<run> <run> <run>" [AB=<A/B session>]
```

That runs [`publish.py`](publish.py) twice:
- `summarise` reads each named benchmark run's `result.json`, and the session's `adoption/result.json` and
  `parity/result.json`, and writes the summary. It holds only the numbers the README shows, each with its source: the run
  folders, commit, image digest, Jellyfin version, run count, host class (cores and memory) and fixture
  hash. It also holds the adoption status of every row of the session and its parity overview, with the
  session's commit and date.
  It refuses fewer than three Jellyfin-comparison runs, and runs of different images, hosts or fixtures. It
  also refuses a session of uncommitted code, one that measured another commit than the runs, and one
  whose adoption ran nothing or whose parity run is missing or did not send every request it planned.
- `readme` draws every graph of the newest summary into `docs/results/<version>/`, a light and a dark SVG
  each, and replaces the block between the `BEGIN`/`END GENERATED RESULTS` markers in `README.md`. The same
  summary files give byte-identical output, and `publish.py readme --check` exits 1 when the block or the
  images differ from what they give.

Every graph follows one style: Ferrofin in blue and Jellyfin in grey. Ferrofin's bar turns amber where it is
slower. Each bar carries its value, each axis its unit, and each graph a caption naming its source. Every
comparable cell is drawn, the biggest speed-up first. A cell the servers answered differently, or whose
evidence is incomplete, is withheld and listed with its reason under the graphs. The graphs are:
- p50 per screen and endpoint at the loaded and stress levels;
- time to first screen;
- peak and idle memory;
- CPU per completed screen, where the work matched.

With `AB=` (the release A/B gate's session, `gates/bench/gate.py ab` or `gates/run --previous`), a graph
follows of the release against the previous one: every gated cell as how many times better the candidate
is, coloured by the gate's verdict (improved, within noise, regressed; the biggest gain first), with a table
of every cell and the reason a cell was not comparable.

Below them, a grid gives the adoption status of every supported Jellyfin version and upgrade route:
passed, failed, skipped (a fixture that was not built) or unfinished, with a table of the reasons. Then the
parity overview: of the requests jellyfin-web makes, how many Ferrofin answers identically, with an accepted
difference, with a difference filed as an issue, or with a new one, and how many could not be compared because
Jellyfin itself failed them, as one stacked bar per API area (the busiest twelve, then the rest as one) and a
table of every area. Last, one graph follows the headline numbers (cold start, the median p50 of the comparable
screens at 5 screens/s, peak and idle memory) over every release published, each against the Jellyfin release it
was measured with; a release measured on another host class or benchmark fixture starts a new line, so a moved
host never reads as a regression or a win. It is drawn from every summary file into `docs/results/history.svg`.
