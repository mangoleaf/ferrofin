# C03: publish successful configuration updates

The configuration manager now supports synchronous observers for main, branding,
and generic named saves. Observers receive the exact committed configuration;
main settings use an immutable shared document and named settings carry their
normalized key and serialized body. Validation and persistence failures emit no
update. Listeners run outside the document/registry read locks and before the
save returns.

Main and branding writes now use atomic replacement and serialize persistence,
publication, and observer delivery. Their worker owns the writer lock and completes
these steps if the caller disconnects. This extends C02's failure protection to
the manager-owned stores and prevents concurrent main saves from publishing in
a different order than they reach disk.

Jellyfin v12.0-rc7 (`4910aafa1a`) provides `ConfigurationUpdated` and
`NamedConfigurationUpdated` in `BaseConfigurationManager`. Ferrofin exposes these
through the configuration seam without making API handlers depend on core types.
Individual service subscriptions and their setting behavior are verified in the
subsequent G findings. This infrastructure does not close unrelated consumer gaps.

## Verification

The 28 configuration-manager tests pass. New regression cases verify observer
snapshots against both the persisted file and current memory during 20 concurrent
saves, no event or live-state replacement on failed writes, normalized named keys,
and the exact persisted branding body. API tests verify successful named writes
notify the manager and rejected or failed saves do not.

The real HTTP check submits 20 concurrent full-document saves, compares disk with
GET, and rejects a missing metadata directory without changing the saved bytes.
Both the before and after builds passed those live checks; this probe did not
reproduce the ordering race in the baseline. The observer regression tests exercise
the new ordering guarantee directly. All 20 API configuration tests, formatting,
strict workspace Clippy, and the server build pass. The complete workspace suite,
doctests, and changed-crate coverage run at the end of the batch.

Authenticated curl POSTs of the full main document used dev builds and the same
one-admin, no-media fixture, with 10 warmups and 50 measured requests. Median
latency was **1.542 → 1.558 ms**, p95 **2.285 → 2.583 ms**. Other builds shared the
host; this is a local regression check, not a stable performance comparison.
