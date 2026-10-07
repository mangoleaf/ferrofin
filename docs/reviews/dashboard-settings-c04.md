# C04: current Web payload compatibility

The library provider registry computed similarity-provider choices, but the wire
DTO omitted `SimilarItemProviders`. The current Web library editor therefore had
no choices to display. The response now includes the registered providers and
their default-enabled state, matching `LibraryTypeOptionsDto` in Jellyfin
v12.0-rc7 (`4910aafa1a`) and the reviewed Web checkout.

The old suppression was tied to the vendored 10.11.8 response schema, which predates
this property. The server's behavioral parity pin is v12.0-rc7; this response now
follows it. The vendored contract files and route-superset requirements are intact.

Eight payload fixtures use the member names, nesting, and form value types emitted
by Web `1e507c588f353482a00333f84e36ddb7c8fc8221`. The API regression tests post and
read back server, branding, encoding, network, metadata, NFO, Live TV and library
settings. Library tests cover both creation and ID-based updates, including nested
provider choices and image options. Fixture provenance and maintenance instructions
are in `crates/ferrofin-api/tests/fixtures/dashboard-settings.md`.

Web still emits one obsolete constant, `EnableArchiveMediaFiles: false`; there is
no control for it and no property in Jellyfin v12's `LibraryOptions`. The fixture
keeps that actual input, while its readback check explicitly excludes the discarded
constant. No new capability is implied by storing a compatibility field.

This finding covers transport and choice visibility. Individual settings whose
values persist but lack consumers remain open in the living review.

## Verification

All 21 configuration and 18 library-structure API tests, 15 library-options
provider tests, and 21 model configuration tests pass. Formatting, strict workspace
Clippy, and the server build pass. Compilation now uses a worktree-local target
cache after mixed system/rustup artifacts were found in the shared cache.

Real HTTP before/after checks used the same disposable fixture. Before the change,
Movie's `SimilarItemProviders` was absent. Afterward, it contains `Local Genre/Tag`
(default enabled) and `TheMovieDb` (default disabled). Both builds round-trip all
40 checked current library fields through actual creation/update and readback.
Workspace tests, doctests, and per-crate coverage run at the end of the batch.

Authenticated curl GETs of `/Libraries/AvailableOptions?libraryContentType=movies&isNewLibrary=true`
used dev builds, 10 warmups and 50 samples. Median latency was **0.545 → 0.595 ms**,
p95 **1.181 → 1.366 ms**. These are local regression measurements on a shared host.
