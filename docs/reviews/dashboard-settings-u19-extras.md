# U19 follow-up: owned extras on adopted databases

U22 adoption validation exposed a visibility integration regression:
`/Items/{id}/SpecialFeatures` returned an empty list for adopted extras with an
`OwnerId` but no `TopParentId`. Adding user context in U19 exposed an older
query-scoping omission. Jellyfin `4910aafa1a`,
`LibraryManager.AddUserToQuery`, treats `OwnerIds` as an existing scope; Ferrofin
incorrectly added library IDs and excluded the extras.

Restored the eighth upstream scope exception. User parental restrictions still
apply to the returned extras, and the owner still has its U19 visibility check.
The extended repository regression, formatting, strict workspace Clippy and
server build pass. Native HTTP returns the owned extra after the fix and hides
it when its rating exceeds the user's limit. The 10.11.8 adoption and restart
stages now pass where both previously reported missing extras. The remaining
adoption matrix is recorded with U22.

Native debug-build median for SpecialFeatures: 2.245 → 3.252 ms, p95
2.787 → 4.811 ms, 50 measured requests after ten warmups. Before returned zero
extras; after returned one, so this is a cost observation, not a speed comparison
of equivalent work. Measurements: `/tmp/ferrofin-dashboard-u19-extras-{before,after}-results.json`.

This follow-up does not increment the completed-finding count.
