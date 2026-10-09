# S14: indexed artwork deletion

Deleting `/Items/{id}/Images/Backdrop/1` previously removed every backdrop and
returned all of their paths for deletion. It now removes only the selected slot.
The remaining rows keep their identities and insertion order, with subsequent
slots compacting naturally. The same behavior applies to screenshots and other
image types. An omitted index selects zero; negative and missing indexes do
nothing. Selection, deletion and collection of the removed path use one atomic
writer statement, avoiding a separate read/delete race.

The source oracle is Jellyfin `4910aafa1a`,
`MediaBrowser.Controller/Entities/BaseItem.cs`: `GetImageInfo` uses
`GetImages(type).ElementAtOrDefault(index)` and `DeleteImageAsync` removes only
that image. The existing provider service retains its shared album artwork
ownership rule.

Regression coverage checks descending GUIDs versus insertion order, middle and
last slots, default and invalid indexes, row identity, other image types and
other items. Real HTTP regressions upload backdrops and screenshots, delete a
middle slot, check the surviving files and serve them through the image routes.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary
check, persistence/virtual-folder tests, production build and real HTTP regression
passed. Disposable binary comparisons reproduced the old whole-type deletion
and verified the fixed middle/default/missing-index behavior against Jellyfin
12.1.0. Every owned server was reaped. Response bodies, file facts, timings,
binary hashes and the earlier failed fixture are retained under
`/tmp/ferrofin-extra-two/s14/` and `target/dashboard-test-tmp/`.

The native reference uses backdrops. Its unindexed screenshot uploads replace
one image, while Ferrofin appends; that independent upload gap remains **S39**.
Stored screenshot-slot deletion is covered by persistence and HTTP regressions.
Shared-host timings are observations, not publishable benchmark results.
The final workspace test and fresh core coverage gates run after S20 as part
of this two-finding batch.
