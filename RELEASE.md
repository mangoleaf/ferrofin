# Releasing Ferrofin

Ferrofin ships three artifacts from this repo: the **server image**
(`ghcr.io/mangoleaf/ferrofin`), the **Helm chart** (`oci://ghcr.io/mangoleaf/ferrofin/charts/ferrofin`)
and **binary tarballs** attached to the GitHub Release. Image and chart are versioned
**independently**. This is the operational checklist.

**GitHub is the source of truth.** Public releases are cut by the `Release` workflow in
`.github/workflows/release.yml`. The maintainer's homelab GitLab (`.gitlab-ci.yml`) runs
an internal copy of the same pipeline; its tags are never pushed to GitHub.

## The versioning model

**The git tag is the single source of truth for the version.** CI never edits
version files: the release version is injected at build time — into the image
via `SERVICE_VERSION` and into the chart via `helm package --version/--app-version`
— so the committed `Cargo.toml`/`Chart.yaml` versions stay as dev placeholders.
Never bump them for a release, and never push a hand-made tag: the workflow derives
the next version from Conventional Commits since the last tag and pushes the tag
itself (override its choice with the `force_version` input when needed).

## Tag scheme

| Tag | Triggers | Notes |
|---|---|---|
| `vX.Y.Z` | image `:vX.Y.Z` + `:latest`, GitHub Release with notes + binaries, chart `X.Y.Z` | Final release |
| `vX.Y.Z-rc.N` | image `:vX.Y.Z-rc.N` and binaries only | Pre-release; never moves `:latest`, no Release, no chart |
| `chart-vA.B.C` | chart `A.B.C` only | Chart-only fix (templates/values), no app change |

SemVer: **breaking → major, feature → minor, fix → patch.** Release tags are annotated.
Note that `scripts/version.sh` never bumps to the next *major* on its own while the
major is `0`; use `force_version` for that.

## Cutting an app release `vX.Y.Z`

Pick the branch to release from:
- **Feature releases** (a new minor or major) are cut from the current `release/vX.Y.Z` branch.
  Only one exists at a time.
- **Patch releases** of the version already out (`vX.Y.Z+1`, bug fixes only) can be cut
  directly from `main`, which carries the released line.

Either way, the steps are the same; "the release branch" below means the branch you release
from.

1. **Ensure the branch is green** — the `CI` workflow (lint, tests, per-crate coverage gate)
   must have succeeded on the commit you are releasing; the workflow refuses otherwise. `main`
   and `release/vX.Y.Z` get CI on every push, and the workflow reads that push run. The
   release PR (release branch → `main`) shows the same run's checks and runs no CI of its own.
2. **Freeze and measure the release branch.** PRs are not benchmarked individually, so this
   is the only performance gate.

   **Freeze.** From here until the tag exists, nothing merges into the release branch. If its
   head moves anyway, start again from step 1: the workflow tags the head at the time it
   runs.

   **Benchmark gate.** Compare the release commit against the previous Ferrofin release with
   the interleaved A/B gate in [`bench/gate.py`](bench/gate.py). The gate measures Ferrofin
   only; Jellyfin is not part of it.

   ```bash
   docker build -t ferrofin:vX.Y.Z-rc .                    # the frozen release commit
   docker pull ghcr.io/mangoleaf/ferrofin:vPREV            # the previous release
   python3 bench/gate.py ab ghcr.io/mangoleaf/ferrofin:vPREV ferrofin:vX.Y.Z-rc
   ```

   - **What it runs.** Six runs in the order previous, candidate, previous, candidate,
     previous, candidate (three adjacent pairs). Each is
     `bench/run.sh --servers ferrofin --only shape,unloaded,loaded`. The driver retags
     `ferrofin:bench` before each run, checks that the run measured the intended image, and
     restores the tag at the end. A failed run stops the session. It is never retried: start
     a new session.
   - **Wall time.** Not yet measured on the shared host. The fixed waits alone add up to
     8 minutes per run: four 30 s settles (after startup and before each window and the
     steady phase), two 30 s warm-ups with 120 s windows, and the 60 s steady window. Six
     runs therefore take at least 48 minutes, before boot and the shape pass. Each run also
     needs the server and client cores 90 % idle (`run.sh` refuses otherwise), so plan for a
     quiet hour. TODO: replace this estimate with the
     measured time of the first gate session (`time python3 bench/gate.py ab …`).
   - **Output.** `bench/runs/gate-ab-<date>-<time>/` holds `verdict.json`, `report.md` and a
     self-contained `report.html` that opens from disk. The command exits `1` when the gate
     blocks.

   **Thresholds.** A cell regressed when the median of the candidate's runs exceeds the
   median of the previous release's runs by more than **both** thresholds of its row **and**
   the candidate was slower in at least 2 of the 3 pairs. *Improved* is the mirror image,
   and anything else is *within noise*. The table lives in [`bench/gate.toml`](bench/gate.toml),
   which the harness reads; the values below are copied from it:

   | Row | Measures | Relative | Absolute |
   |---|---|---:|---:|
   | `screens` | screen p50 | 15 % | 2 ms |
   | `endpoints` | endpoint p50 | 15 % | 0.5 ms |
   | `memory` | peak (under load) and steady idle anon memory | 25 % | 40 MiB |
   | `soak` | one-hour anon-memory slope (not a median rule: see *Memory soak*) | limit 20 MiB/h | |

   A `[cells."…"]` entry in `gate.toml` overrides its row for a single cell. Change the table
   only by a reviewed commit, after re-measuring the noise floor with
   `python3 bench/gate.py aa <image>` (six runs of one image), and keep this table in step
   with it.

   **What blocks.** A gated cell that *regressed*, or one that is *not comparable* because a
   side's evidence is flagged, a side has missing values, or the candidate's responses
   differ from the previous release's. Either blocks the release until it is fixed or the
   maintainer accepts it. Gated cells are the screen and endpoint p50s at the unloaded and
   loaded levels, plus peak and steady memory, and the soak's `soak/slope` when the soak was
   run. The stress level and endpoint cells whose medians are both under 1 ms are reported
   but never block.

   **Memory soak.** Anon memory that creeps up over hours is invisible in a two-minute window,
   so measure it separately, in the same window as the gate:
   `python3 bench/gate.py soak ghcr.io/mangoleaf/ferrofin:vPREV ferrofin:vX.Y.Z-rc`
   ([`bench/README.md`](bench/README.md#memory-soak)). It runs the previous release and then the
   candidate back to back, one hour each at the loaded rate (`run.sh --only soak`), and fits a
   least-squares line through the anon samples after the first 10 minutes. The candidate
   *regressed* when its slope exceeds **20 MiB/h** (`[soak]` in
   [`bench/gate.toml`](bench/gate.toml)) and the previous release's does not; *improved* is
   the mirror image; anything else, including both over the limit, is *within noise*. A soak
   that left no slope cannot be judged, and a regressed one blocks, both like any other gated
   cell. It takes about two hours of a quiet host (two soaks of an hour each, plus startup
   and the settle and steady phases), so book it with the rest of the gate. The limit and the
   10 minutes are starting values, to be re-derived after two soaks.

   **The Jellyfin comparison is publication only.** The three-server `bench/run.sh` runs
   (Ferrofin and both Jellyfins, three runs, median) produce the published tables in the root
   README and `docs/BENCHMARKS.md`. Run them when those numbers are refreshed. They never
   decide a release.

   **Validate adoption.** The evidence is a green run of the `Adoption` workflow
   (`.github/workflows/adoption.yml`) on the frozen release commit. It builds the image from
   that commit and runs the full adoption validation in
   [`adoption/README.md`](adoption/README.md): `adoption/run.sh` **and** the synthetic
   preservation matrix, all ten paths. Every push to `release/**` starts it; for a patch
   release from `main`, start it with **Actions → Adoption → Run workflow** on `main`. Keep
   the run's link for step 5. A manual run of the same commands is for debugging only and
   does not replace the CI run. An adoption failure is never waived; fix it, which means
   starting again from step 1. Dropping a Jellyfin
   version instead is also a code change: the adoption gate in
   `crates/ferrofin-db/src/database.rs` must refuse it, and versions that share a
   migration set go together.
3. **Actions → Release → Run workflow** on the branch you are releasing from. Leave `force_version` empty
   to accept the derived version, or set it to `vX.Y.Z`. The job pushes the annotated tag;
   it does **not** commit to the branch. A release branch merges back into `main` with a merge
   commit (never squash or rebase), so the release tag stays reachable from `main`.
4. The tag push re-triggers the workflow: `docker-image` → `github-release` → `helm-chart`.
   Watch it finish, then verify (below).
5. **Record the results** after the tag, so the record never moves the frozen head: attach
   the benchmark gate's `report.md` to the release PR (release branch → `main`), or to the
   GitHub Release for a patch release from `main`, and update the adoption test record in
   `adoption/README.md` with the link to the green `Adoption` run from step 2.
   After a patch release from `main`, merge `main` into the open release branch so the fix
   is carried forward.

The workflow needs the repository secret `RELEASE_TOKEN`: a fine-grained personal
access token for this repository with **Contents: read and write**. Tags pushed with the
default `GITHUB_TOKEN` do not trigger workflows, so without it the tag push publishes
nothing.

## Cutting a release candidate `vX.Y.Z-rc.N`

Run the workflow with `force_version = vX.Y.Z-rc.N`. Only the image and binaries are
built (no `:latest`, no Release, no chart). Bump `N` for each respin — never re-point an
existing rc tag.

## Cutting a chart-only release `chart-vA.B.C`

For template/values fixes with no app change:

1. `helm lint charts/ferrofin` and render-test with a real values file.
2. Run the workflow with `force_version = chart-vA.B.C`. CI publishes the chart only, at
   version `A.B.C`, leaving the app version untouched.

## Verifying a release

```bash
docker pull ghcr.io/mangoleaf/ferrofin:vX.Y.Z
docker run --rm -p 8096:8096 ghcr.io/mangoleaf/ferrofin:vX.Y.Z &
curl -s localhost:8096/System/Info/Public | jq .Version      # prints vX.Y.Z
helm pull oci://ghcr.io/mangoleaf/ferrofin/charts/ferrofin --version X.Y.Z
gh release view vX.Y.Z --json assets -q '.assets[].name'    # tarballs + checksums
```

## Not yet automated (do manually / add later)

- Image + chart signing (cosign) and SBOM generation.
- Artifact Hub listing for the chart.
- `release-x.y` maintenance branches — introduce only when a user pinned to an
  old minor needs a backported patch.
