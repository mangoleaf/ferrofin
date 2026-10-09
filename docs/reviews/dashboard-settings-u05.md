# U05: selected authentication provider

The saved provider now governs password changes and resets as well as login.
External providers own their credential store; an unavailable provider no longer
silently overwrites the local password. Enabled-provider selection remains
case-insensitive and unknown IDs fail authentication. Successful authentication
records the provider identity and reloads provider changes; newly provisioned
users are resolved by the returned canonical name and receive the provider's
new-user policy. The server currently registers the built-in provider; this does
not add native .NET plugin loading.

All 48 focused user/provider tests, formatting, strict workspace Clippy and the
build pass. Registered-provider fakes verify enabled/disabled choices, password
dispatch, provisioning and policy assignment. Real HTTP verifies the built-in
choice with case variants, an empty selection, unknown-ID rejection, and restoring
the built-in choice after a password operation against an unavailable provider.
An empty selected ID now becomes the successful provider's ID; the unavailable
provider no longer changes the local password.

Single cold login observations on the same fixture were **4.878 → 11.542 ms**
(case variant), **5.268 → 17.432 ms** (empty selection), and
**2.099 → 5.701 ms** (missing provider). Builds/coverage were running on the shared
host; these are functional-run timings, not stable latency estimates. The login
path now reloads provider changes and persists a newly selected provider. Final
workspace tests, doctests, per-crate coverage and the combined browse measurement
remain batch-end gates.
