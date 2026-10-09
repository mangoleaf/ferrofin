# S20: filesystem-root library shortcuts

Libraries configured with `/` previously saved an empty Locations list because
shortcut resolution stripped every slash. Resolution now removes one native
trailing directory separator and preserves the filesystem root. Backslashes
remain literal characters on Unix, and neither whitespace nor line endings
are trimmed. Root-path removal also keeps its separator when matching the
resolved shortcut, so removing `/` removes the shortcut and saved PathInfos.

The source oracle is Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/IO/MbLinkShortcutHandler.cs`, which calls
`Path.TrimEndingDirectorySeparator`, and the root match in
`LibraryManager.RemoveMediaPath`. Existing general path comparison behavior
and the separate physical hierarchy finding S03 remain independent.

Regressions cover root and repeated separators, Unix literal backslashes,
relative paths, blank contents, whitespace and line endings. A real manager
round-trips the configured root, physical paths, owning-library policy and
removal. HTTP tests check saved Locations, PathInfos and library identity.
The disposable native fixture never refreshes after adding the host root.
Its Ferrofin ownership check uses an already scanned disposable movie and
uploads a subtitle after removing its original library: the root library's
SaveSubtitlesWithMedia=false must select internal metadata storage.
The Jellyfin 12.1.0 comparison checks root Locations and removal.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary
check, production build, targeted regressions, real HTTP and disposable native
checks passed. The full source-qualified workspace matrix passed **8,410
tests** across 21 packages and 201 default test targets.
All changed packages and their dependents were rerun; unchanged independent
packages retain their prior passing results after source and dependency checks.
Real FFmpeg tests were enabled. The five existing skipped tests remain in the
DB/provider suites. The WASM guest build was separately disabled explicitly.
These limits remain visible in the retained commands and test summaries.

Fresh, unseeded **ferrofin-core line coverage is 95.40%**,
above its separate 80% gate. These final gates cover both S14 and S20; every
tested Rust/Cargo input and the final production binary is checked by
`/tmp/ferrofin-extra-two/quality-proof.json`. The canonical profile and complete
logs are retained under `/tmp/ferrofin-extra-two/`. Both native fixtures pass
against the final production binary, and every owned server was reaped.
The root ownership fixture reproduced sidecar fallback before the fix and
verified internal subtitle storage afterward. The reference root fixture
checks Locations and removal independently. Shared-host request timings are
observations, not publishable benchmarks. Earlier fixture and lint failures
remain preserved with their corrected reruns.

Only generated build caches were cleaned. The worktree, commits, production
binary, native data and canonical coverage profiles were preserved. After validation cleanup, target
usage is **11.48 GiB**, with **510.84 GiB free** on the host.
