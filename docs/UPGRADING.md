# Upgrading Ferrofin

Operator-facing notes for upgrades that need a manual step or change behaviour in a
way the release notes do not make obvious. Newest first. `CHANGELOG.md` lists *what*
changed; this file says *what you have to do about it*.

Ferrofin's own database upgrades in place: start the new version against the same
data directory and its migrations run on boot. Back up the data directory before a
major-version upgrade.

## Unreleased — the schema moves to Jellyfin 12.0

Back up the data directory before starting this version. On first boot migration `0032`
rebuilds `BaseItems`, `Users`, `Permissions`, `Preferences` and `MediaStreamInfos` into
Jellyfin 12.0's shape (the file is snapshotted to `jellyfin.db.pre-0032` first, next to
the existing `jellyfin.db.pre-0007`; 12.0 ships the same shapes as fourteen of Ferrofin's
own `BaseItems` indexes, so those are not recreated), `0033` rebuilds one index and drops `sqlite_stat1`,
and `0034` folds Ferrofin's playlist/collection cache table into Jellyfin's
`LinkedChildren`. Expect one longer start proportional to library size; a second boot is
a no-op. Every file-backed boot now runs `PRAGMA foreign_key_check` (about 0.14 s on a
42k-item library) and refuses to open a database that fails it.

Behaviour that changed with the shape:

- A playlist may now hold the same item more than once (Jellyfin 12 semantics); removing
  an entry removes every occurrence of that item.
- Two users whose names differ only by case can no longer coexist (`NormalizedUsername`
  is unique). Creating or renaming into a case-variant returns the error Jellyfin returns.
- `CleanName`/`CleanValue` use Jellyfin 12's punctuation-stripping form; a forced sort
  name goes through the full sort-name pipeline. Both are recomputed once on first boot.
- Localized user views (e.g. a Live TV view created under a translated name) are
  consolidated onto their name-independent id once, with channels, ancestors and
  display preferences moved along.
- **Adopting a Jellyfin database** now accepts 12.0.0 and 12.1.0 as well as 10.11.8–10.11.11
  (exact migration sets, still one-way; a 12.x database baselines `0030` and `0032`, whose
  shape it already has). All six releases passed live adoption tests on 2026-09-16,
  including both 10.11.8 → 12.1.0 and 10.11.8 → 12.0.0 → 12.1.0. See the
  [support matrix and tested build](../adoption/README.md#supported-and-tested-versions).
- A **Playlists** (or Collections) view is listed only when the user can see something in it,
  as in Jellyfin 12.1. A library whose playlists all live inside music album folders — every
  `.m3u` next to an album — has no Playlists view on the home screen; the playlists themselves
  are unchanged and still found by search and `/Items`.
- Alternate-version groups are re-derived once from `LinkedChildren` (Jellyfin 12.1's
  `RepairAlternateVersionLinks`), and a version is hidden only while its primary exists in the
  same library: an item marked as a version of a row that no longer exists reappears in
  listings (144 episodes on the reference library), exactly as on Jellyfin 12.1. A 12.0 database keeps its `LinkedChildren` rows and
  is never re-imported from the frozen JSON copy in `Data`.

## Unreleased — Unicode username matching

Migration `0030_normalized_usernames.sql` owns the `Users.NormalizedUsername` column
and unique index. Older databases run it; adopted Jellyfin 10.11.10/10.11.11 databases
baseline it because they already have those schema objects. Migrations 0001–0029 remain
unchanged. A Rust data-only backfill then writes ICU-based invariant uppercase keys,
recording completion as `normalized_usernames_icu_v1` in `FerrofinMeta` in the same
transaction. It never adds a column or drops/recreates an index.

SQL initially copies the existing unique display names into the keys, so it does not
rely on SQLite's ASCII-only `upper()`. Startup checks for Unicode collisions before
running SQL migrations, and the backfill validates again inside its transaction.
No requests are served until the backfill succeeds. Account IDs, password hashes,
permissions, and watch history are preserved. A failed backfill can retry on the next
startup without reapplying SQL migrations.

Login, creation, and renaming now agree for non-ASCII case variants such as `münchen`
and `MÜNCHEN`. If old accounts normalize to the same key, startup refuses with their IDs
and names. Restore/use your pre-upgrade installation to rename the conflicting accounts,
then retry the upgrade; do not merge accounts. Back up the full data directory first.

For Jellyfin adoption, follow the [complete migration procedure](INSTALL.md#migrate-an-existing-jellyfin-installation),
including the separately stored configuration and copying before first startup.

## 1.0.0 — first public release

No manual steps between Ferrofin releases. The baseline for this file starts here;
pre-1.0 development builds were never published and are not an upgrade path.

**Coming from Jellyfin** is a different matter and is covered in the README under
[Migrating from Jellyfin](../README.md#migrating-from-jellyfin): adoption is one-way,
Ferrofin writes `jellyfin.db.pre-ferrofin` before touching anything, and you should back
up the whole Jellyfin data directory yourself first.

## Unicode metadata casing

Migration `0031_invariant_clean_names` corrects clean-name and derived sort keys
that match Ferrofin's previous full-lowercase mapping. It preserves item IDs,
references, raw names, custom keys, and the original forced sort-title text.
New Jellyfin-mode by-name IDs use .NET-compatible invariant casing; existing
person IDs and year IDs under affected Unicode metadata roots remain in use.

Apply this migration by starting Ferrofin. It uses application-provided Unicode
functions that standalone `sqlite3` and `sqlx migrate` do not register. No
persistent schema objects depend on these functions, so external database
inspection remains possible after migration. Follow the backup and rollback
steps above before upgrading.
