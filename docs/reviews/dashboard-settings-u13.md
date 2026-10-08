# U13: media deletion — open dependency

The saved deletion flags already reach the per-item permission predicate:
`EnableContentDeletion` permits eligible media generally, and
`EnableContentDeletionFromFolders` permits eligible items in selected libraries.
Box sets use collection-management rights and playlists use their ownership
rules. The endpoint and `CanDelete` DTO share that implementation.

This finding is **not complete**. The settings worktree still removes database
rows without implementing Jellyfin's physical deletion and sidecar/folder rules.
User-scoped item resolution is the separate U19 dependency.

The existing `investigate/item-file-deletion` worktree owns this implementation,
with its canonical plan at `brain/plans/PLAN_ITEM_FILE_DELETION.md`. At review,
its clean HEAD was `c7a1034a`; scanner hierarchy, mixed-folder semantics, video
classification and alternate-version grouping must be correct before deletion
can safely choose the same files and directories as Jellyfin. The plan's
scanner/adoption work and staged deletion patch have not been integrated here.

Continue independent settings findings while preserving U13 as open. Completion
requires integrating the finished deletion work, the shared visibility checks,
and its real-filesystem/adopted-database validation. Do not mark this setting
complete merely because its permission predicate works, or cherry-pick the
stale deletion patch ahead of its prerequisites.
