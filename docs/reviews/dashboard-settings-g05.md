# G05: verify the live Quick Connect toggle

`QuickConnectAvailable` already has a correct live reader. Added a regression
using the real configuration manager to save the setting while a Quick Connect
manager holds pending and authorized requests. No production code changed.

Compared with Jellyfin v12.0-rc7 (`4910aafa1a`),
`QuickConnectManager.IsEnabled` / `AssertActive`: disabling refuses initiation,
status polling, authorization and secret exchange immediately. Re-enabling allows
unexpired pending/authorized requests again. The setting gates pairing operations;
it does not revoke sessions already issued through Quick Connect.

## Verification

All six Quick Connect manager tests, formatting, strict workspace Clippy and
the server build pass. Real HTTP checks use separate administrator and pairing
devices. Both builds return **401** for all disabled pairing operations,
including exchange after authorization, and **200** after re-enabling. A token
issued before disabling still authenticates `/Users/Me`.

For the same disposable fixture, `GET /QuickConnect/Enabled` used 10 curl warmups
and 50 samples: median **0.408 → 0.550 ms**, p95 **0.761 → 2.187 ms**. Production
code is unchanged; this spread reflects shared-host noise. Final workspace tests,
doctests and coverage remain batch-end gates.
