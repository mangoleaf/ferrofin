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

   **Benchmark** the release commit as [`bench/README.md`](bench/README.md) describes:
   - Run the full `bench/run.sh` three times, with both Jellyfins in each run, so every
     cell has its Jellyfin comparison and its `⚠` notes.
   - In the same window, interleaved with those runs, measure the previous release as the
     baseline: `bench/run.sh --out bench/runs/vPREV-baseline-N` with
     `ghcr.io/mangoleaf/ferrofin:vPREV` tagged as `ferrofin:bench`.
     - Run names come from the checked-out commit, so without `--out` the baseline would be
       filed as a candidate run.
     - Use a fresh directory for each attempt (`-N`): `run.sh` never reuses a run directory.
     - Keep the two images under their own tags and `docker tag` the right one to
       `ferrofin:bench` before each run.
   - In `python3 bench/report.py --serve`, tick the three release runs and tick the
     baseline directories as the baseline (compared by their median).

   A latency or memory regression against the baseline, or a `⚠` response-shape note that
   the baseline does not have, blocks the release until it is fixed or the maintainer
   accepts it. The viewer shows notes for the release runs only. The baseline's own notes are
   in its `report.md` (`bench/runs/vPREV-baseline-N/report.md`).

   **Validate adoption.** The evidence is a green run of the `Adoption` workflow
   (`.github/workflows/adoption.yml`) on the frozen release commit. It builds the image from
   that commit and runs the full adoption validation in
   [`adoption/README.md`](adoption/README.md): `adoption/run.sh` (eleven paths, including the
   fresh-install 12.2 row) **and** the synthetic preservation matrix (ten paths; the seedless
   fresh-install row is validated by `run.sh` alone). Every push to `release/**` starts it; for a patch
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
5. **Record the results** after the tag, so the record never moves the frozen head: note the
   benchmark result in the release PR (release branch → `main`), or in the GitHub Release for
   a patch release from `main`, and update the adoption test record in `adoption/README.md`
   with the link to the green `Adoption` run from step 2.
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
