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

1. **Ensure `main` is green** — the `CI` workflow (lint, tests, per-crate coverage gate)
   must have succeeded on the commit you are releasing; the workflow refuses otherwise.
2. **Actions → Release → Run workflow** on `main`. Leave `force_version` empty to accept
   the derived version, or set it to `vX.Y.Z`. The job pushes the annotated tag; it does
   **not** commit to `main`.
3. The tag push re-triggers the workflow: `docker-image` → `github-release` → `helm-chart`.
   Watch it finish, then verify (below).

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
