//! What proves that Ferrofin did the work of each Jellyfin code migration the
//! export records as applied (gl-1902): the marker the one-shot boot repair
//! porting it leaves once it ran (`ferrofin-core`'s `adoption_repairs` and
//! `item_persistence_service`).
//!
//! Recording an id tells Jellyfin the routine ran, so it never runs it. Every
//! start of a Ferrofin release that has the repair runs it once, so a database
//! without the marker was last started by a release before the repair, and its
//! data still waits for that work. The export refuses it rather than record a
//! routine nothing did: start this release on the data directory once, stop it
//! and export again. The check reads the staged copy, never the source.

use sqlx::SqliteConnection;

use super::classify::{Disposition, JELLYFIN_MIGRATIONS};
use super::history::SourceHistory;
use super::paths::has_table;
use crate::{DbError, Result, repair_markers};

/// A marker of a one-shot repair, which each repair leaves only where it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// A `FerrofinMeta` key the repair writes once it is done; it runs on every
    /// database.
    Meta(&'static str),
    /// The same, for a repair that runs only on a database whose adoption
    /// record names a 10.11 release (`sweep_missing_items`).
    From1011(&'static str),
    /// The adoption record's `MembershipImportDone`: the playlist, collection
    /// and version links a 10.11 database keeps in its items' data, moved into
    /// `LinkedChildren` (`import_membership_once`), on a database whose record
    /// names a 10.11 release. One adopted before the record existed has none,
    /// and migration 0034 folded those links into the table.
    MembershipImport,
}

impl Marker {
    fn name(self) -> &'static str {
        match self {
            Marker::Meta(key) | Marker::From1011(key) => key,
            Marker::MembershipImport => "the adoption's membership import",
        }
    }
}

/// Each code migration the export records as applied, with the markers of the
/// repairs that do its work. Not among them: `strip_embedded_linked_children_v121`,
/// which ports 12.1's `StripEmbeddedLinkedChildren`, a routine the export
/// leaves to run (so Jellyfin does its work whatever the marker says); and
/// `user_data_harmonized_v122`, whose `HarmonizeConflictingUserData` is left to
/// run too, the rows it would change counted in the loss report instead.
const PROOFS: [(&str, &[Marker]); 3] = [
    (
        // Its move of the links out of the items' data, its version cleanups
        // (`rehome_local_versions`, which keeps additional parts and local
        // versions where upstream deletes them, then `rederive_adopted_versions`,
        // which hands the versions upstream re-creates back to the scan) and its
        // sweeps of the items of deleted libraries and of missing files.
        "20260113120000_MigrateLinkedChildren",
        &[
            Marker::MembershipImport,
            Marker::Meta(repair_markers::REHOME_LOCAL_VERSIONS),
            Marker::Meta(repair_markers::REDERIVE_ADOPTED_VERSIONS),
            Marker::From1011(repair_markers::MISSING_ITEM_SWEEPS),
        ],
    ),
    (
        "20260115120000_FixIncorrectOwnerIdRelationships",
        &[Marker::Meta(repair_markers::OWNER_ID_RELATIONSHIPS)],
    ),
    (
        "20260915120000_MigrateRatingLevels",
        &[Marker::Meta(repair_markers::RATING_LEVELS)],
    ),
];

/// Refuses a database the export would record a code migration of as applied
/// (one the source's history lacks) without the marker of each repair doing
/// that migration's work.
///
/// # Errors
/// [`DbError::ExportRefused`] naming each routine and the markers it lacks, or
/// a database error.
pub(super) async fn require(conn: &mut SqliteConnection, source: &SourceHistory) -> Result<()> {
    let mut missing = Vec::new();
    for (id, disposition) in JELLYFIN_MIGRATIONS {
        if disposition != Disposition::CodeRecordedAsApplied
            || source.ids.iter().any(|held| held == id)
        {
            continue;
        }
        for &marker in markers(id) {
            if !done(conn, marker).await? {
                missing.push(format!(
                    "{} ({})",
                    id.split_once('_').map_or(id, |(_, name)| name),
                    marker.name()
                ));
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(DbError::ExportRefused {
        reason: format!(
            "the export marks as done the Jellyfin code migrations whose work Ferrofin already did, but this \
             database lacks the proof of that work for: {}. A Ferrofin release older than the repair doing it \
             last started on it: start this release on the data directory once, stop it and export again",
            missing.join(", ")
        ),
    })
}

/// The markers of the repairs doing `id`'s work.
fn markers(id: &str) -> &'static [Marker] {
    PROOFS
        .iter()
        .find(|(proved, _)| *proved == id)
        .map_or(&[], |(_, markers)| markers)
}

/// Whether `marker` says its repair ran on the database, or that the repair
/// does not run on it.
async fn done(conn: &mut SqliteConnection, marker: Marker) -> Result<bool> {
    let key = match marker {
        Marker::Meta(key) => key,
        Marker::From1011(key) => {
            if membership_imported(conn).await?.is_none() {
                return Ok(true);
            }
            key
        }
        Marker::MembershipImport => {
            return Ok(membership_imported(conn)
                .await?
                .is_none_or(|imported| imported));
        }
    };
    Ok(has_table(conn, "FerrofinMeta").await?
        && sqlx::query_scalar::<_, i64>(r#"SELECT 1 FROM "FerrofinMeta" WHERE "Key" = ?1"#)
            .bind(key)
            .fetch_optional(&mut *conn)
            .await?
            .is_some())
}

/// Whether the adoption's membership import is done, on a database whose
/// adoption record names a 10.11 release; `None` for any other. 12.0's own
/// routine moved the links before Ferrofin adopted a 12.x database.
async fn membership_imported(conn: &mut SqliteConnection) -> Result<Option<bool>> {
    if !has_table(conn, "FerrofinAdoption").await? {
        return Ok(None);
    }
    let record: Option<(String, i64)> = sqlx::query_as(
        r#"SELECT "Generation", "MembershipImportDone" FROM "FerrofinAdoption" LIMIT 1"#,
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(record
        .filter(|(generation, _)| generation.starts_with("10.11."))
        .map(|(_, imported)| imported != 0))
}

#[cfg(test)]
mod tests {
    use super::{Disposition, JELLYFIN_MIGRATIONS, PROOFS};

    /// A code migration recorded as applied without a proof would be recorded
    /// whether or not its work was done; a proof of one left to run is dead.
    #[test]
    fn every_code_migration_recorded_as_applied_and_nothing_else_has_a_proof() {
        for (id, disposition) in JELLYFIN_MIGRATIONS {
            let proofs = PROOFS.iter().filter(|(proved, _)| *proved == id).count();
            let recorded = disposition == Disposition::CodeRecordedAsApplied;
            assert_eq!(proofs, usize::from(recorded), "{id}");
        }
        assert!(PROOFS.iter().all(|(_, markers)| !markers.is_empty()));
    }
}
