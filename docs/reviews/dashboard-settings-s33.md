# S33: ambient UI culture vs configured server culture

Jellyfin has two localized-string lookups. `GetLocalizedString(phrase)` reads
the ambient `CultureInfo.CurrentUICulture` (what `UseRequestLocalization` set
for the request, inherited by work the request starts);
`GetServerLocalizedString(phrase)` reads the configured `UICulture`. Ferrofin
had one lookup that always used the configured culture, so a request's
`Accept-Language`/`culture` never reached chapter names, stream labels or task
text, and nothing could tell the two kinds of caller apart.

Source oracle: Jellyfin `4910aafa1a`, `LocalizationManager.cs:594–603`
(`GetLocalizedString` → `CurrentUICulture.Name`; `GetServerLocalizedString` →
`Configuration.UICulture`), `Startup.cs:133–149` (request localization sets
both `CurrentCulture` and `CurrentUICulture`), consumers:
`FFProbeVideoInfo` `ChapterNameValue`, `ProbeResultNormalizer` and
`MediaStreamRepository` labels, scheduled-task names (ambient);
`UserViewManager` "Folders", `LibraryManager` extra names, `CollectionManager`
"Collections", `SeasonResolver` (server). Evidence note
`brain/knowledge/DASHBOARD_SETTINGS_S33_S34_AMBIENT_CULTURE.md`.

## Behavior

- `ferrofin_util::current_culture` gains an ambient UI culture beside the
  ambient culture: `scope_with_ui(culture, ui, future)` sets both, `scope`
  (used when work is spawned from a request) inherits the creator's UI culture,
  both restore on every poll and on panic, and `ambient_ui_culture()` reads it
  (`None` outside any request).
- The request-localization middleware sets both from the selected request
  culture (query → cookie → `Accept-Language`, with the startup default for an
  unmatched side), as the pinned `UseRequestLocalization` does.
- `LocalizationManager::get_localized_string` uses the ambient UI culture and,
  with none (startup, scheduled work not started from a request), the configured
  culture like an empty `CurrentUICulture.Name`; the new trait method
  `get_server_localized_string` always uses the configured culture. Library
  "Folders" and extra names ("Trailer", "Trailer 2") now use the server lookup.
- Unchanged and matching upstream: a name with no resource (`fr-FR`) falls back
  to `en-US`, as `GetLocalizationDictionary` does for a missing resource.

## Verification

- Util: the UI culture follows its scope, is inherited by a nested `scope`,
  can be replaced by an inner `scope_with_ui`, and does not leak out.
- Core: on a German-configured server a request in French sees French for
  `get_localized_string` (also from work spawned inside the request) and
  German for `get_server_localized_string`; outside a request the ambient
  lookup is the configured culture.
- Production binary, server configured `UICulture=de`, a movie with three
  unnamed chapters, first scan started by a request with `Accept-Language: fr`:

  | | chapter names | library view name |
  |---|---|---|
  | before | `Kapitel 1, 2, 3` (configured) | `Films` |
  | after | **`Chapitre 1, 2, 3`** (the request's) | `Films` |

## Limitations

- **S34 stays open.** Jellyfin's persistent `LimitedConcurrencyLibraryScheduler`
  workers keep the context of the request that first started them, so a later
  request's child refreshes can be named in an earlier request's culture;
  Ferrofin inherits per spawned task and has no persistent worker pool to
  reproduce that. That needs the worker lifecycle traced and ported separately.
- `Collections` and season-name resolution still use fixed English text in
  Ferrofin; only the lookup contract and the consumers that already localized
  changed here.
- Scanner-level chapter naming is covered by the live run, not a unit test: its
  naming path needs a probed file with chapters.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the changed crates passed: util 62, mediaencoding 205,
server 237, livetv 299, wasm 917 (4 skipped), api 954, extensions 96,
providers 933, core 2,459. Real FFmpeg tests enabled; the WASM guest build
remains disabled. Separate line coverage over each changed crate's own sources:
util **96.72%**, core **95.13%**, api **87.87%**, providers **91.53%**, all above
the 80% gate; traits is coverage-exempt. An earlier run failed because three api
integration-test stubs lacked the new trait method; they were fixed and every
gate was rerun from scratch. Instrumented builds were cleaned after the numbers
were taken (`target` about 25 GiB, about 494 GiB free).
