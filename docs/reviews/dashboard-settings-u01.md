# U01: account management and password changes

Password changes now preserve the requesting session while revoking other
sessions, matching Jellyfin's UserController. Previously the current token was
revoked too. Account name, configuration and password edits now enforce
EnableUserPreferenceAccess for ordinary users, and accept elevated API keys.
Administrator reset and deletion keep their upstream behavior. Forgotten-password
provider selection and recovery are covered separately by U06.

All 16 API user integration tests, formatting, strict workspace Clippy and the
server build pass. Real HTTP checks on the same disposable fixture verify account
creation, rename, case-insensitive duplicate rejection, wrong-current-password
rejection, password change, admin reset to an empty password and deletion.
The current session changes from erroneous 401 to 200 after its password change;
other sessions still return 401. With preferences disabled, all three self-edit
operations now return 403 instead of 204. API-key account edits now return 204
instead of 403. Deleting the account invalidates its token.

Ten warmups and 50 `/Users/Me` requests measured median **0.458 → 0.391 ms**,
p95 **0.592 → 0.613 ms**; this is shared-host noise, not a performance claim.
Final workspace tests, doctests and per-crate coverage remain batch-end gates.
