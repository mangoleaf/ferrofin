# U14: collection management

Verified supported against Jellyfin `v12.0-rc7` (`4910aafa1a`):
`CollectionController` uses the collection-management policy on create, add and
remove membership. The default authorization handler permits administrators;
ordinary users require `EnableCollectionManagement`. This is independent of
`EnableContentDeletion`. Box-set deletion uses the same collection permission
through the shared item-deletion predicate.

No production change is needed for the setting. A disposable native server
verified all three operations with saved **off → on → off** policy changes for
both ordinary users and administrators: **18 HTTP checks** pass. Denied ordinary
requests return 403; permitted creation returns 200 and membership edits return
204. Created collections were removed; their source media file remained intact.
Fifty warm duplicate-membership additions after ten warmups measured median
**0.762 ms**, p95 **1.084 ms** on the shared host, without a performance claim.

The pre-existing item-visibility dependency remains U19. This verification does
not establish isolation for collections containing item IDs outside the user's
visible library, or complete U13's general physical-deletion implementation.
