# U07: per-user remote access

The original audit overstated support: ordinary authenticated requests and
password login ignored `EnableRemoteAccess`. Only selected permission policies
enforced it. Existing tokens also bypassed access schedules on ordinary routes.

Default authorization now checks the effective client address against the live
network configuration and enforces remote access before the administrator
exception. Password login shares that network manager, and public user listings
hide remote-disabled accounts from remote clients after setup. The policy uses
U02's cached user snapshot, so warm authorization adds no policy database read.
Schedules are evaluated at request time rather than only when issuing a token.

Compared with Jellyfin `v12.0-rc7`, commit `4910aafa1a`, particularly
`DefaultAuthorizationHandler`, `ApiServiceCollectionExtensions`,
`FirstTimeSetupRequirement`, `UserController` and `UserManager`. Preserved the
explicit schedule exemptions for user details and system information, setup
access, unrestricted API keys, and bare elevation/local-access policies.
Configuration and item-lookup actions combine their controller's default policy
with elevation, so those admin actions still enforce remote access.

All **881 API tests**, **71 focused core tests**, formatting, strict workspace
Clippy and the server build pass. Regressions cover administrator ordering,
schedule exemptions, trusted forwarding, live LAN changes, and password login.
Handler-only test fakes now carry an ordinary authenticated identity and policy;
userless credentials are refused before protected handlers run.

Real HTTP before/after on the same disposable fixture verified:

| Probe | Before | After |
|---|---|---|
| Remote-disabled user's existing token | 200 | 403 |
| Remote-disabled user's password login | 200 | 403 |
| Visible remote-disabled user in remote public list | Included | Omitted |
| Ordinary token outside saved access schedule | 200 | 403 |
| User details and system information outside schedule | 200 | 200 |
| Remote-disabled admin on a default-policy route | 200 | 403 |
| Same admin on bare elevation route (`/Devices`) | 200 | 200 |
| Saved LAN changes and ignored untrusted forwarding | Pass | Pass |

Fifty warm remote `GET /Users/Me` requests after ten warmups measured median
**0.835 → 0.792 ms**, p95 **1.705 → 1.772 ms**. Builds and coverage were running
on the shared host; these observations establish the exercised path, not an
optimization claim. See [batch verification](dashboard-settings-batch-01.md)
for final workspace gates, per-crate coverage, browse consistency checks and the
combined latency-measurement limitation.
