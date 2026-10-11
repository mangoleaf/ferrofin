# S04: unlocked metadata after a library rename and an explicit scan — owner decision needed

Not a closure: this records what the native oracle shows and the decision it
puts to the project owner. No behavior was changed.

The L02 fixture found that native Jellyfin 12.1 resets an item's edited title and
overview on a later explicit scan, where Ferrofin keeps them. The finding asked
to check the pinned refresh/merge rules before changing anything. Replaying the
scenario against a live Jellyfin 12.2 (portable build) and the current Ferrofin
shows what actually triggers it.

## The scenario (one movie; metadata edited through `POST /Items/{id}`, played)

| step | Jellyfin 12.2 | Ferrofin |
|---|---|---|
| edit title and overview, mark played | kept | kept |
| rename the library (`POST /Library/VirtualFolders/Name`, no refresh) | kept | kept |
| **explicit library scan** | **title reset to the parsed name `Allowed (2020)`, overview cleared**; same item id, still played | kept |
| second scan | stays reset | kept |
| *same steps without the rename*: explicit scan | **kept** | kept |

So an explicit scan alone never resets an unlocked edit in either server
(`MetadataRefreshMode.Default` re-reads nothing for an unchanged file). The reset
is specific to **renaming a library**: Jellyfin replaces the library's collection
folder (its id derives from the new name's path), the old one and the items under
it are removed, and the next scan resolves them again from disk — a fresh row
with the file's parsed name, no overview, no manual edits — while the item id
(path-derived) and the user data (keyed, retained) survive.

## What porting it would mean

Ferrofin keeps the library's items across a rename (the row, its edits and its
locks). Matching Jellyfin means a library rename discards every manually edited
field of every item in the library — titles, overviews, cast, images the user
chose — which the user must re-enter. That is data loss caused by a rename, which
the user did not ask for, and which Jellyfin's own web client does not warn about.

The repository's standing guidance is not to port Jellyfin behavior that is a
defect where Ferrofin is correct, and only the owner may add an accepted
divergence (`CLAUDE.md`, *Nothing is deferred*; "don't port Jellyfin bugs").

## Decision needed

1. **Keep Ferrofin's behavior and record it as an accepted divergence** (library
   rename keeps the library's items and their edits), or
2. **Port the reset** (a rename removes the old library's items; the next scan
   re-resolves them), including user-data retention across it.

Recommendation: 1. Until it is decided S04 stays open.

Reproduce: `/tmp/ferrofin-st04/oracle_s04.py` style probe — edit via `POST /Items`,
`POST /Library/VirtualFolders/Name?...&refreshLibrary=false`, `POST /Library/Refresh`,
read the item back — against the portable Jellyfin 12.2 and the dev server.
