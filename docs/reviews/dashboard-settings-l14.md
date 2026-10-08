# L14: automatic artwork limits and minimum widths

Automatic artwork acquisition now reads the saved per-type `ImageOptions`,
falling back to Jellyfin's per-item defaults. Zero or negative limits disable
acquisition; singular image types have one slot and Backdrop permits the saved
count. Known remote widths below `MinWidth` are rejected; unknown widths remain
eligible. Dynamic/plugin, audio and existing book-cover extraction use the
positive-limit check without applying a remote-image width constraint.

The filesystem scanner, music pass and path-less refresh apply the same policy.
TMDB acquisition uses complete image listings with dimensions; TVDB dimensions
also survive conversion. Normal refresh avoids providers whose supported slots
are all disabled/full. Local images count toward capacity and remain unaffected
by the size filter. The manual image chooser still lists all candidates.

New backdrops occupy unused files until acquisition succeeds; only then are old
internal backdrops pruned. Failed replacements retain existing images. Multiple
backdrops are persisted together, and singular image types have distinct file
stems. Downloads preserve the returned media type. Forbidden/missing candidates
permit fallback, other failures stop that provider's image-type pass, and the
next provider can still fill the slot. The duplicate-length backdrop check is
retained for acquisition without replacement.

Source: Jellyfin `4910aafa1a`, `ItemImageProvider`, `TypeOptions`, `ImageOption`,
`LibraryOptions`, and the TMDB image providers. Limits apply to automatic image
acquisition, not to local metadata readers that import image references.

Validation passes: **960 model tests**, **815 provider tests**, **488 focused
core tests**, real-server HTTP, SQL boundary, formatting, strict workspace Clippy
and the server build. Separate line coverage passes: model **90.15%** (8,841/9,807), providers
**93.60%** (22,240/23,761), core **94.70%** (103,632/109,436).
The core result combines the previous whole-crate profile with current focused
tests and 22 current binaries. LLVM profile mismatch warnings remain in the
evidence; it is accumulated coverage, not a new whole-core test run.
Results are in `/tmp/ferrofin-dashboard-l14-coverage.json`.
The HTTP regression saves two different limit/width configurations, disables
metadata providers, refreshes images and verifies counts and served bytes.
It compares the acquired backdrop set: existing persistence assigns random GUID
row IDs and exposes indices in ID order, not acquisition order.

Related work remains open: L17 controls destinations; S08 covers missing video
embedded extraction. Book image-provider registration, configured ordering and
image-only refresh are a separate open implementation gap: the existing book
cover path is coupled to its metadata read. This change gates that path's image
acquisition but does not certify those missing execution paths.

The native five-phase movie-scan fixture returned identical collection results
before and after. Median scan time was **390 ms before** (364–1,174) and
**461 ms after** (336–529). These unisolated runs overlap other host work and
show no performance improvement; this is a baseline regression measurement
with remote fetchers disabled, not image-download latency. Docker was unavailable.
Evidence: `/tmp/ferrofin-dashboard-l14-{checks,native}.json` and matching logs.
