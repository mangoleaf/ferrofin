# Releasing Hermit

Hermit ships two artifacts from this repo: the **server image** and the official
**Helm chart** (`charts/hermit/`). They are versioned **independently**. This is
the operational checklist.

## The versioning model

**The git tag is the single source of truth for the version.** CI never edits
version files: the release version is injected at build time — into the image
via `SERVICE_VERSION` and into the chart via `helm package --version/--app-version`
— so the committed `Cargo.toml`/`Chart.yaml` versions stay as dev placeholders.
Never bump them for a release, and never push a hand-made tag: the manual
`create-release` CI job derives the next version from the changelog and pushes
the tag itself (override its choice with `FORCE_VERSION=vX.Y.Z` when needed).

## Tag scheme

| Tag | Triggers | Notes |
|---|---|---|
| `vX.Y.Z` | image `:X.Y.Z` + `:latest`, release notes, chart publish | Final app release |
| `vX.Y.Z-rc.N` | image `:X.Y.Z-rc.N` only | Pre-release; never moves `:latest` |
| `chart-vA.B.C` | chart publish only | Chart-only fix (templates/values), no app change |
| push to `main` | image `:{M.m.p}-{N}-{sha}` | Dev image between releases |

Pre-1.0 SemVer: **minor may break, patch is fixes.** Release tags are
annotated (signed where the runner has a key).

## Cutting an app release `vX.Y.Z`

1. **Ensure `main` is green** — CI lint + tests + the per-crate coverage gate
   must be passing on the commit you're releasing.
2. **Run the manual `create-release` job** on that commit. It derives the next
   version (or honors `FORCE_VERSION`), regenerates the git-cliff notes, and
   pushes the annotated tag — it does **not** commit to `main`.
3. Watch the pipeline: `build` → `release` (git-cliff notes) → `chart`
   (OCI push). Verify the release object and the chart artifact exist.

## Cutting a release candidate `vX.Y.Z-rc.N`

Run `create-release` with `FORCE_VERSION=vX.Y.Z-rc.N`. Only the image is built
(`:X.Y.Z-rc.N`, no `:latest`, no release notes, no chart). Bump `N` for each
respin — never re-point an existing rc tag.

## Cutting a chart-only release `chart-vA.B.C`

For template/values fixes with no app change:

1. `helm lint charts/hermit` and render-test with a real values file.
2. Push the `chart-vA.B.C` tag (via `create-release` with
   `FORCE_VERSION=chart-vA.B.C`). CI publishes the chart only, at version
   `A.B.C`, leaving the app version untouched.

## Verifying a release

```bash
docker pull  <registry>/hermit:X.Y.Z
helm pull    oci://<registry>/hermit/charts/hermit --version A.B.C
```

(Substitute the registry the release pipeline publishes to.)

## Not yet automated (do manually / add later)

- Image + chart signing (cosign) and SBOM generation.
- Artifact Hub listing for the chart.
- `release-x.y` maintenance branches — introduce only when a user pinned to an
  old minor needs a backported patch.
