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
| adoption | fixture rows passed, failed, unfinished and skipped, by the adoption report's own rules | the adoption report |
| parity | new differences, the groups of them not filed yet, requests sent and planned; the verdict and its reason | the parity report |
| bench | from its `result.json`: phases completed, failed, unfinished and skipped; for the A/B gate (`--previous`), the gated cells regressed, not comparable, improved and within noise, and whether anything blocks | the benchmark or A/B gate report |

A gate the session did not ask for, did not reach, is still running, ended before writing its result,
or left a result that cannot be read is shown as such, never left out. Each gate's own report is its
main link; its raw files and the log follow it. The page opens from disk (every
link is relative) and replaces none of the gates' own reports.

Every gate is moving to one structured result, `<run>/result.json`, in the envelope
[`result.py`](result.py) defines and validates: `schema`, `gate`, `verdict`, `counts`, `source`
(`commit`, `image`, `image_digest`, `oracle`, `host_class` (cores and memory, never a host name),
`fixture`), `started`, `ended`, and `detail`, the gate's own data. Its HTML report, this page and the
viewer read that file, never the gate's internal files. verify and bench write one today.
