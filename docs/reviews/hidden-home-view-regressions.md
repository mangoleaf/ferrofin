# Hidden home view regressions

Investigated local `main` at `01d48993` (PR #19 merged), in the
`fix/hidden-view-regressions` worktree. All three P2 findings reproduce.
The fixes and regression tests are included in this worktree.

## 1. Legacy views ignore `includeHidden`

**Reproduction:** Hide Movies through `MyMediaExcludes`, then request
`/Users/{userId}/Views?includeHidden=true`. Movies remains absent, while
`/UserViews?userId={userId}&includeHidden=true` returns it.

**Cause:** `get_user_views_for_user` constructs a query with
`include_hidden: false`, ignoring the incoming parameter.

**Fix:** Extract the query parameters, override the target user with the path
user, and forward them to the modern handler. Both routes now support absent,
false, and true values consistently.

**Test:** `hidden_views_can_be_requested_through_both_routes` failed before the
fix (4 views instead of 5 for the legacy request) and passes after it.

## 2. Hiding a home tile suppresses unscoped latest items

**Reproduction:** Seed Movies with one movie, leave `LatestItemsExcludes`
empty, and put Movies in `MyMediaExcludes`. An unscoped latest request changes
from one movie to an empty result.

**Cause:** `latest_parents` uses `get_user_views`, which now applies the home
visibility preference. That accidentally combines two independent settings.

**Fix:** Resolve fallback parents through `get_media_folders`, then apply
`latest_item_excludes`. Explicit-parent handling stays in its existing path.
Root folders also give latest requests the underlying library IDs instead of
derived home view IDs.

**Test:** `hidden_home_library_still_supplies_latest_items` uses a real SQLite
database and item repository. It failed before the fix with an empty result,
and passes after it. It also checks that the home tile stays hidden,
`LatestItemExcludes` still suppresses unscoped results, and an explicit parent
still returns its movie.

## 3. Hidden libraries disappear from grouping options

**Reproduction:** Hide Movies, then request either grouping-options route.
Movies is missing from the list. Jellyfin Web constructs `GroupedFolders`
from the returned checkboxes when saving, so an omitted library loses its
grouping selection on a subsequent save.

**Cause:** `get_grouping_options` also calls the newly filtered
`get_user_views`.

**Fix:** Read `get_media_folders`, retaining the existing collection-type
eligibility filter and name ordering. Grouping remains independent of home
tile visibility.

**Test:** `hidden_libraries_remain_grouping_options_through_both_routes` failed
before the fix with `[Attic, Shows]` instead of `[Attic, Movies, Shows]`, and
passes after it for both routes.

## Upstream evidence

Checked the local Jellyfin 10.11.8 and 12.0 source checkouts:

- `Jellyfin.Api/Controllers/UserViewsController.cs`, `GetUserViewsLegacy`:
  forwards `includeHidden` to `GetUserViews`.
- The same controller, `GetGroupingOptions`: enumerates user-root folder
  children rather than the home view list.
- `Emby.Server.Implementations/Library/UserViewManager.cs`,
  `GetItemsForLatestItems`: fallback parents are user-root children filtered
  by `LatestItemExcludes`.
- Jellyfin Web 10.11.8,
  `src/components/homeScreenSettings/homeScreenSettings.js`, `saveUser`:
  builds `GroupedFolders` from the checked grouping inputs.

Using root media folders for the two non-home callers follows that upstream
behavior. Simply passing `includeHidden=true` would bypass the new filter but
would retain the dependency on derived home views.

## Validation

- Before fixes: all three new regression tests failed with the expected results.
- After fixes: 38 core view-manager tests and 32 API user-library tests passed.
- `cargo fmt --all --check` and `git diff --check` passed.
- Clippy passed for both changed crates with all targets, all features, and
  warnings denied.
- Rebuilt `ferrofin-server` and exercised it over loopback HTTP with a fresh
  temporary database and a generated movie. Verified both views routes with
  omitted/false/true `includeHidden`, both grouping routes, unscoped latest
  results after hiding Movies, latest exclusions, and explicit-parent latest
  results. All checks passed; the test server was stopped afterward.
- Full workspace tests, coverage, and performance benchmarks were not run.

Commands (with the local compiler-cache wrapper disabled):

```sh
RUSTC_WRAPPER= cargo test --offline -p ferrofin-core --lib user_view_manager::tests
RUSTC_WRAPPER= cargo test --offline -p ferrofin-api --test user_library
RUSTC_WRAPPER= cargo clippy --offline -p ferrofin-core -p ferrofin-api --all-targets --all-features -- -D warnings
cargo fmt --all --check
```
