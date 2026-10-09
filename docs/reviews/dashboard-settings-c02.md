# C02: validate and atomically save named configurations

The dashboard's generic named save bypassed `EncodingConfigurationStore`.
It accepted nonexistent transcoding directories and changes to the encoder path,
and truncated the destination before knowing that the replacement would succeed.

The composition root now registers the existing encoding validator. The API
normalizes types, validates against the previous typed configuration, and writes
a private temporary file beside the destination. It flushes the completed file
before renaming it over the destination. Failed writes and renames clean up the
temporary file and preserve the previous destination. Saves share a lock through
the live network update, so concurrent requests cannot leave disk and runtime
policy in different orders. Accepted saves finish even if the client disconnects.

This follows Jellyfin v12.0-rc7 (`4910aafa1a`),
`BaseConfigurationManager.SaveConfiguration`, `EncodingConfigurationStore.Validate`,
and `ExceptionMiddleware.GetStatusCode`: a changed nonblank transcoding path must
exist (404 otherwise); a changed nonblank encoder path is rejected (500).
Unchanged paths and blank resets retain upstream behavior. Atomic replacement
adds failure protection to Ferrofin's JSON storage. This does not wire individual
encoding settings to their runtime consumers; those findings remain separate.

## Verification

The 19 API configuration tests pass, including validator dispatch, error categories,
unchanged bytes after rejection, temporary-file cleanup after rename failure,
unchanged live policy on failure, and agreement between persisted and effective
network configuration after 20 concurrent saves. The file utility also injects a
mid-write failure and verifies the original bytes survive.

Real HTTP checks used the C01 binary and this change on the same disposable
one-admin, no-media fixture. Before the change, both invalid path changes returned
204 and replaced the file. Afterward, missing directories return 404 and encoder
path changes return 500, with byte-for-byte preservation in both cases. Valid
existing directories, unchanged paths that no longer exist, and blank resets still
return 204. A forced network-file write failure returns 500 and leaves the running
remote-access denial in place (503 for a forwarded remote client).

Formatting, strict workspace Clippy, and the server build pass. Per-crate line
coverage: util **96.22%** (178 tests), common **87.18%** (74 tests),
mediaencoding **93.71%** (917 tests), and API **87.12%** (873 tests).
The API coverage run required socket access for its WebSocket integration test
and rebuilding one malformed instrumented test binary; the final run includes
all tests and profiles. The complete workspace
test suite and doctests will also run after the first 20 findings.

## Before/after timings

Authenticated curl POSTs on loopback, dev builds, same disposable data directory:
10 warmups followed by 50 samples for each named section. The shared host had
concurrent builds, so these are local regression measurements. Saves now include
semantic validation where registered and a file flush before atomic replacement.

| Section | Median before / after (ms) | p95 before / after (ms) |
|---|---|---|

| Encoding | 1.058 / 1.325 | 3.628 / 1.841 |
| Metadata | 0.917 / 1.502 | 1.608 / 2.917 |
| Network | 1.966 / 2.281 | 3.552 / 5.792 |
