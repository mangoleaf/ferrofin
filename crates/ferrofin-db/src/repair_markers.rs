//! The `FerrofinMeta` keys of the one-shot boot repairs (`ferrofin-core`'s
//! `adoption_repairs` and `item_persistence_service`) that the export reads as
//! the proof of a Jellyfin code migration's work before it records that
//! migration as applied (gl-1902). One definition for both, so a key renamed
//! for one is renamed for the other: a database adopted after a rename would
//! otherwise never carry the key the export asks for, and every export of it
//! would be refused.

/// `rehome_local_versions`: `MigrateLinkedChildren`'s version cleanups, done
/// the data-preserving way. Runs on every database.
pub const REHOME_LOCAL_VERSIONS: &str = "rehome_local_versions_v12";

/// `rederive_adopted_versions`: the counterpart of `MigrateLinkedChildren`'s
/// re-creation of a 10.11 local version of the wrong type
/// (`CleanupWrongTypeAlternateVersions`), which hands the versions back to the
/// scan. Runs on every database adopted from 10.11, the only ones the export
/// records that routine for.
pub const REDERIVE_ADOPTED_VERSIONS: &str = "rederive_adopted_versions_v160";

/// `sweep_missing_items`: `MigrateLinkedChildren`'s sweeps of the items of
/// deleted libraries and of missing files. Runs only on a database whose
/// adoption record names a 10.11 release.
pub const MISSING_ITEM_SWEEPS: &str = "missing_item_sweeps_v12";

/// `fix_owner_id_relationships`: `FixIncorrectOwnerIdRelationships`. Runs on
/// every database.
pub const OWNER_ID_RELATIONSHIPS: &str = "owner_id_relationships_v12";

/// `repair_rating_levels`: `MigrateRatingLevels`, re-dated with the routine
/// (`20260915120000` is 12.2's). Runs on every database.
pub const RATING_LEVELS: &str = "rating_levels_v122";
