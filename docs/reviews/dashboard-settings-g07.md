# G07: honor the parallel image encoding limit

`ParallelImageEncodingLimit` now creates the two startup pools used by Jellyfin
v12.0-rc7 (`4910aafa1a`): resized-image encodes have their own pool, while audio
artwork, video thumbnails and trickplay extraction share another. Positive
values are exact limits; zero and negative values use the usable CPU count.
Cached images and passthrough responses do not acquire an encoding slot.

The pools retain their startup limits, matching the upstream constructors.
Saving a different value flags `HasPendingRestart`; restarting reconstructs the
services with the saved value. A disconnected resized-image request retains
its permit until blocking pixel work actually finishes. Cancelled media jobs
use the existing kill-on-drop process runner and release their pool permit.

## Verification

All 837 drawing/media-encoding tests and the startup default/explicit-limit test
pass, as do formatting, strict workspace Clippy and the server build. New
regressions verify limits 1 and 2 during blocking pixel work after request
cancellation, plus audio/video/trickplay sharing and cancellation release.
Existing cache-hit, fallback, frame extraction and trickplay retry cases pass.

Real HTTP saves now flag a pending restart (**false → true** compared with the
baseline); `POST /System/Restart` reloads the saved limit and clears the flag.
Sixteen uncached resizes of the same 1600×1000 JPEG, using eight HTTP clients,
all returned the requested dimensions. Wall time at limit 1 was
**1.748 → 12.403 s**, and at limit 4 **1.905 → 3.148 s**. The baseline ignored
both values and ran all requests concurrently; the intended resource restriction
now changes throughput. These debug-build smoke timings are not production
capacity estimates. Final workspace tests, doctests, coverage and the combined
browse benchmark are batch-end gates.
