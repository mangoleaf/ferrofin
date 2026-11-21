//! What the operator is told about the database an export writes (gl-1553):
//! what each table it removed held ([`table_note`]), and what Jellyfin's code
//! migrations the exported history leaves to run will change on its first
//! start ([`first_start_effects`]). `ferrofin-server export-jellyfin` writes
//! them into its loss report.

/// What each Ferrofin table holds and where Jellyfin keeps the same, if
/// anywhere, by name.
const TABLE_NOTES: [(&str, &str); 20] = [
    (
        "FerrofinAdoption",
        "which Jellyfin release Ferrofin adopted the database from; nothing Jellyfin needs",
    ),
    (
        "FerrofinExternalIdentities",
        "the links from LDAP-Auth and Single Sign-On identities to users. LDAP-Auth's are written into its settings for Jellyfin's LDAP-Auth plugin (LdapUsers in plugins/configurations/LDAP-Auth.xml). Jellyfin does not know Single Sign-On's provider, so an account Single Sign-On created cannot sign in there until it has the default provider, which saving its profile page in Jellyfin's dashboard gives it; from then on it signs in with a password an administrator sets after saving (one set before is ignored), and with no other, an empty one included",
    ),
    (
        "FerrofinIntroSkipperDisabledEpisodes",
        "the episodes Intro Skipper was told to skip",
    ),
    (
        "FerrofinIntroSkipperSeasonStates",
        "Intro Skipper's analysis state per season; Jellyfin's Intro Skipper plugin analyses again",
    ),
    (
        "FerrofinIntroSkipperSegments",
        "Intro Skipper's segments as detected and edited; those it published stay in Jellyfin's MediaSegments",
    ),
    (
        "FerrofinLiveTvChannels",
        "Ferrofin's own copy of the Live TV channels; the channel items Jellyfin reads, which hold the users' favourites, are kept, and Jellyfin's guide refresh rebuilds the rest from the tuners",
    ),
    (
        "FerrofinLiveTvListingProviders",
        "Live TV guide providers, written to config/livetv.xml, where Jellyfin keeps them",
    ),
    (
        "FerrofinLiveTvPrograms",
        "Ferrofin's own copy of the Live TV guide; the programme items Jellyfin reads are kept, and Jellyfin's guide refresh downloads the guide again",
    ),
    (
        "FerrofinLiveTvRecordings",
        "Ferrofin's records of DVR recordings; each completed one's file and programme facts are written into its timer in data/livetv/timers.json, as Jellyfin keeps them, and the recorded files are where they were written",
    ),
    (
        "FerrofinLiveTvSeriesTimers",
        "series recording rules, written to data/livetv/seriestimers.json, where Jellyfin keeps them",
    ),
    (
        "FerrofinLiveTvTimers",
        "scheduled and completed recordings, written to data/livetv/timers.json, where Jellyfin keeps them",
    ),
    (
        "FerrofinLiveTvTunerHosts",
        "Live TV tuners, written to config/livetv.xml, where Jellyfin keeps them",
    ),
    (
        "FerrofinMeta",
        "Ferrofin's settings for this database and the one-time repairs it ran; nothing Jellyfin needs",
    ),
    (
        "FerrofinPlaybackSessions",
        "Ferrofin's log of playback decisions (direct play or transcode, and why); Jellyfin keeps none",
    ),
    (
        "FerrofinPlaylistShares",
        "playlist shares; each playlist's item data, which Jellyfin reads, holds the same",
    ),
    (
        "FerrofinPlaylists",
        "playlist owners; each playlist's item data, which Jellyfin reads, holds the same",
    ),
    (
        "FerrofinPluginOfferedItems",
        "the items each WASM plugin's media analysis was offered; WASM plugins do not run on Jellyfin",
    ),
    (
        "FerrofinUserDataRetentionKeys",
        "the keys of the watch state FerrofinUserDataRetentionSnapshots holds",
    ),
    (
        "FerrofinUserDataRetentionSnapshots",
        "watch state Ferrofin kept for items that left the library, to give back if they return",
    ),
    (
        "_sqlx_migrations",
        "Ferrofin's own migration history; nothing Jellyfin needs",
    ),
];

/// What the table `name` held, for an operator; a table with no note (one a
/// later migration added without one) is named as Ferrofin's.
#[must_use]
pub fn table_note(name: &str) -> &'static str {
    TABLE_NOTES
        .iter()
        .find(|(table, _)| *table == name)
        .map_or("a table of Ferrofin's own", |(_, note)| note)
}

/// What each code migration of Jellyfin 12.4 a source can leave to run
/// changes on Jellyfin's first start, in the order the report prints them:
/// what an operator would notice first. Read from the routines at `v12.4`
/// (`Jellyfin.Server/Migrations/Routines/`).
const FIRST_START: [(&str, &str); 19] = [
    (
        "20260531160000_DisableLegacyAuthorization",
        "turns EnableLegacyAuthorization off in system.xml, so clients that sign in the legacy way stop working until it is turned on again",
    ),
    (
        "20260113120000_MigrateLinkedChildren",
        "reads alternate versions and playlist and collection entries from item data again, and deletes the items of deleted libraries, stray alternate versions, every item whose file is missing from a readable library folder and, when every library folder is readable, every item outside them all: the media must be mounted where Ferrofin saw it",
    ),
    (
        "20260825200000_ConsolidateLocalizedUserViews",
        "moves views such as Playlists and Live TV to the ids Jellyfin derives; where a view type has several, the display preferences of all but one are deleted",
    ),
    (
        "20260508120000_MergeDuplicateMusicArtists",
        "merges artists whose names differ only by case into one and deletes the others, with their metadata folders",
    ),
    (
        "20260508130000_MergeDuplicatePeople",
        "merges people whose names differ only by case into one and deletes the others, with their metadata folders",
    ),
    (
        "20260831100000_EnableLocalSimilarityProviders",
        "adds the local similar-items providers to each library's options",
    ),
    (
        "20260113230000_CleanupOrphanedExtras",
        "deletes items owned by the placeholder item",
    ),
    (
        "20260525010000_CleanupOrphanedExternalData",
        "deletes the attachment, subtitle, trickplay and chapter-image folders of items no longer in the database",
    ),
    (
        "20260912120000_HarmonizeConflictingUserData",
        "where one item has several play-state rows for a user that disagree, gives them all the most recently played one's values",
    ),
    (
        "20260915120000_MigrateRatingLevels",
        "scores each item's parental rating from its own rating only: an item without one loses the score it inherited",
    ),
    (
        "20260115120000_FixIncorrectOwnerIdRelationships",
        "deletes items that share a path with another, clears owner links and sets the primary version of alternate versions again",
    ),
    (
        "20260729120000_RestorePlaylistChildrenFromMetadata",
        "fills each playlist that has no entries from its playlist.xml, which only playlists in media libraries keep: the export leaves out those in data/playlists",
    ),
    (
        "20260915104305_FixNullEncoderPreset",
        "sets an empty encoder preset to auto in encoding.xml",
    ),
    (
        "20260522092304_UpdateNormalizedUsername",
        "recomputes stored values from the same inputs; nothing should change",
    ),
    (
        "20260610120000_RefreshCleanNamesAndValues",
        "recomputes stored values from the same inputs; nothing should change",
    ),
    (
        "20260722120000_RefreshForcedSortNames",
        "recomputes stored values from the same inputs; nothing should change",
    ),
    (
        "20260821120000_RecomputeSeriesPresentationKey",
        "recomputes stored values from the same inputs; nothing should change",
    ),
    (
        "20260908120000_RepairAlternateVersionLinks",
        "recomputes stored values from the same inputs; nothing should change",
    ),
    (
        "20260911120000_StripEmbeddedLinkedChildren",
        "removes keys from item data that Jellyfin no longer reads; nothing should change",
    ),
];

/// The routine name and effect of each code migration in `left_to_run`, in
/// the order the report prints them; one this table does not know comes
/// last, by its name alone.
#[must_use]
pub fn first_start_effects(left_to_run: &[&'static str]) -> Vec<(&'static str, &'static str)> {
    let known = FIRST_START
        .iter()
        .filter(|(id, _)| left_to_run.contains(id))
        .copied();
    let unknown = left_to_run
        .iter()
        .filter(|id| !FIRST_START.iter().any(|(known, _)| known == *id))
        .map(|id| (*id, "a code migration this report has no description for"));
    known
        .chain(unknown)
        .map(|(id, effect)| (id.split_once('_').map_or(id, |(_, name)| name), effect))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{FIRST_START, TABLE_NOTES, first_start_effects, table_note};
    use crate::Database;
    use crate::database::JELLYFIN_GENERATIONS;
    use crate::export::{Disposition, JELLYFIN_MIGRATIONS};

    /// A table a migration adds must say what it holds, or the loss report
    /// names it without saying what is lost.
    #[tokio::test]
    async fn every_ferrofin_table_has_a_note() {
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND (name GLOB 'Ferrofin*' OR name = '_sqlx_migrations')",
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        assert!(tables.len() > 10, "{tables:?}");
        for table in tables
            .iter()
            .map(String::as_str)
            .chain(["FerrofinAdoption"])
        {
            assert!(
                TABLE_NOTES.iter().any(|(name, _)| *name == table),
                "{table} has no note"
            );
        }
        assert_eq!(table_note("FerrofinNew"), "a table of Ferrofin's own");
        // The identity links are LDAP-Auth's and Single Sign-On's.
        let links = table_note("FerrofinExternalIdentities");
        assert!(
            links.contains("LDAP-Auth") && links.contains("Single Sign-On"),
            "{links}"
        );
    }

    /// Every code migration a supported source can lack has an effect to
    /// print, and every effect is a code migration Jellyfin runs.
    #[test]
    fn every_code_migration_a_source_can_leave_to_run_is_described() {
        for generation in &JELLYFIN_GENERATIONS {
            for (id, disposition) in JELLYFIN_MIGRATIONS {
                if disposition == Disposition::CodeLeftToRun
                    && !generation.migration_ids.contains(&id)
                {
                    assert!(
                        FIRST_START.iter().any(|(known, _)| *known == id),
                        "{} can leave {id} to run, which has no effect to print",
                        generation.name
                    );
                }
            }
        }
        for (id, _) in FIRST_START {
            assert!(
                JELLYFIN_MIGRATIONS.contains(&(id, Disposition::CodeLeftToRun)),
                "{id}"
            );
        }
    }

    #[test]
    fn effects_come_in_the_report_order_and_unknown_ids_last() {
        let effects = first_start_effects(&[
            "20260912120000_HarmonizeConflictingUserData",
            "20990101000000_Future",
            "20260531160000_DisableLegacyAuthorization",
        ]);
        let names: Vec<&str> = effects.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                "DisableLegacyAuthorization",
                "HarmonizeConflictingUserData",
                "Future"
            ]
        );
        assert_eq!(first_start_effects(&[]), []);
    }
}
