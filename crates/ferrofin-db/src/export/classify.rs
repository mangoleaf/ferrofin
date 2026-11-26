//! Which of the target Jellyfin release's `__EFMigrationsHistory` ids are
//! schema migrations and which are code migrations, and what the export does
//! with each: the table the history rule reads (gl-1549) and the
//! version-drift check checks a new release against (gl-1552).
//!
//! The target is the release `UPSTREAM_TAG` names, Jellyfin 12.4. It declares
//! 56 EF migrations
//! (`src/Jellyfin.Database/Jellyfin.Database.Providers.Sqlite/Migrations/`)
//! and 49 code migrations (`Jellyfin.Server/Migrations/Routines/` and
//! `PreStartupRoutines/`), and records all 105 on a fresh install, a code
//! migration as `yyyyMMddHHmmss_Name` in the same table
//! (`JellyfinMigrationService.cs`). The table also holds the ids of routines
//! earlier releases retired or re-dated, which an upgraded database may still
//! carry: Jellyfin compares ids exactly and never looks up one it does not
//! know, so they are harmless.

use Disposition::{CodeLeftToRun, CodeRecordedAsApplied, Retired, Schema};

/// What the export does with one `__EFMigrationsHistory` id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// An EF schema migration of the target release. Ferrofin's own migration
    /// chain gives every database it exports this release's schema, so the
    /// export records the id whether or not the source's history has it.
    /// Left out, Jellyfin would apply it again and fail on a table or index
    /// that already exists, or, sorting before a recorded one, revert every
    /// later schema migration first.
    Schema,
    /// A code migration Jellyfin runs itself: it repairs data and
    /// configuration in the shapes earlier releases left. Kept when the
    /// source's history has it (it ran before adoption), never added to an
    /// adopted database's (a Ferrofin-native one gets a fresh install's).
    CodeLeftToRun,
    /// A code migration recorded as applied even where the source's history
    /// lacks it: Ferrofin did its work when it adopted the database, and the
    /// round-trip gate showed that running it on a database Ferrofin
    /// maintained damages it (owner decision on gl-1902: only those). The
    /// export refuses an adopted database without the proof of that work
    /// (`super::proof`); a Ferrofin-native one records it, as a fresh install
    /// does.
    CodeRecordedAsApplied,
    /// The id of a routine an earlier release retired or re-dated, which the
    /// target no longer knows. Kept when the source's history has it, never
    /// added.
    Retired,
}

/// The `ProductVersion` Jellyfin 12.4 records for an EF migration: the version
/// of Entity Framework Core it ships.
pub const SCHEMA_PRODUCT_VERSION: &str = "10.0.14";

/// The `ProductVersion` Jellyfin 12.4 records for a code migration: its own
/// assembly version.
pub const CODE_PRODUCT_VERSION: &str = "12.4.0.0";

/// Every id the target release records, and every retired id a database
/// Ferrofin adopts may carry, with its [`Disposition`], in id order.
#[rustfmt::skip] // one row per line, as a table
pub const JELLYFIN_MIGRATIONS: [(&str, Disposition); 109] = [
    ("20200514181226_AddActivityLog", Schema),
    ("20200613202153_AddUsers", Schema),
    ("20200728005145_AddDisplayPreferences", Schema),
    ("20200905220533_FixDisplayPreferencesIndex", Schema),
    ("20201004171403_AddMaxActiveSessions", Schema),
    ("20201204223655_AddCustomDisplayPreferences", Schema),
    ("20210320181425_AddIndexesAndCollations", Schema),
    ("20210407110544_NullableCustomPrefValue", Schema),
    ("20210814002109_AddDevices", Schema),
    ("20221022080052_AddIndexActivityLogsDateCreated", Schema),
    ("20230526173516_RemoveEasyPassword", Schema),
    ("20230626233818_AddTrickplayInfos", Schema),
    ("20230923170422_UserCastReceiver", Schema),
    ("20240729140605_AddMediaSegments", Schema),
    ("20240928082930_MarkSegmentProviderIdNonNullable", Schema),
    ("20241020103111_LibraryDbMigration", Schema),
    ("20241111131257_AddedCustomDataKey", Schema),
    ("20241111135439_AddedCustomDataKeyKey", Schema),
    ("20241112152323_FixAncestorIdConfig", Schema),
    ("20241112232041_FixMediaStreams", Schema),
    ("20241112234144_FixMediaStreams2", Schema),
    ("20241113133548_EnforceUniqueItemValue", Schema),
    ("20250202021306_FixedCollation", Schema),
    ("20250204092455_MakeStartEndDateNullable", Schema),
    ("20250214031148_ChannelIdGuid", Schema),
    ("20250326065026_AddInheritedParentalRatingSubValue", Schema),
    ("20250327101120_AddKeyframeData", Schema),
    ("20250327171413_AddHdr10PlusFlag", Schema),
    ("20250331182844_FixAttachmentMigration", Schema),
    ("20250401142247_FixAncestors", Schema),
    ("20250405075612_FixItemValuesIndices", Schema),
    ("20250420000000_CreateNetworkConfiguration", CodeLeftToRun),
    ("20250420010000_MigrateNetworkConfiguration", CodeLeftToRun),
    ("20250420020000_MigrateMusicBrainzTimeout", CodeLeftToRun),
    ("20250420030000_MigrateEncodingOptions", CodeLeftToRun),
    ("20250420040000_RenameEnableGroupingIntoCollections", CodeLeftToRun),
    ("20250420050000_DisableTranscodingThrottling", CodeLeftToRun),
    ("20250420060000_CreateUserLoggingConfigFile", CodeLeftToRun),
    ("20250420070000_MigrateActivityLogDb", CodeLeftToRun),
    ("20250420080000_RemoveDuplicateExtras", CodeLeftToRun),
    ("20250420090000_AddDefaultPluginRepository", CodeLeftToRun),
    ("20250420100000_MigrateUserDb", CodeLeftToRun),
    ("20250420110000_ReaddDefaultPluginRepository", CodeLeftToRun),
    ("20250420120000_MigrateDisplayPreferencesDb", CodeLeftToRun),
    ("20250420130000_RemoveDownloadImagesInAdvance", CodeLeftToRun),
    ("20250420140000_MigrateAuthenticationDb", CodeLeftToRun),
    ("20250420150000_FixPlaylistOwner", CodeLeftToRun),
    ("20250420160000_AddDefaultCastReceivers", CodeLeftToRun),
    ("20250420170000_UpdateDefaultPluginRepository", CodeLeftToRun),
    ("20250420180000_FixAudioData", CodeLeftToRun),
    ("20250420190000_RemoveDuplicatePlaylistChildren", Retired),
    ("20250420193000_MigrateLibraryDbCompatibilityCheck", CodeLeftToRun),
    ("20250420200000_MigrateLibraryDb", CodeLeftToRun),
    ("20250420210000_MoveExtractedFiles", CodeLeftToRun),
    ("20250420220000_MigrateRatingLevels", Retired),
    ("20250420230000_MoveTrickplayFiles", CodeLeftToRun),
    ("20250420230000_RefreshInternalDateModified", CodeLeftToRun),
    ("20250421000000_MigrateKeyframeData", CodeLeftToRun),
    ("20250609115616_DetachUserDataInsteadOfDelete", Schema),
    ("20250618010000_MigrateLibraryUserData", CodeLeftToRun),
    ("20250620180000_FixDates", CodeLeftToRun),
    ("20250622170802_BaseItemImageInfoDateModifiedNullable", Schema),
    ("20250714044826_ResetJournalMode", Schema),
    ("20250730215000_ReseedFolderFlag", CodeLeftToRun),
    ("20250913211637_AddProperParentChildRelationBaseItemWithCascade", Schema),
    ("20250925203415_ExtendPeopleMapKey", Schema),
    ("20251009200000_CleanMusicArtist", CodeLeftToRun),
    ("20260113102337_AddLinkedChildrenTable", Schema),
    ("20260113120000_MigrateLinkedChildren", CodeRecordedAsApplied),
    ("20260113203012_ChangeOwnerIdToGuid", Schema),
    ("20260113230000_CleanupOrphanedExtras", CodeLeftToRun),
    ("20260113233000_AddForeignKeyToOwnerId", Schema),
    ("20260113233500_DropExtraIdsColumn", Schema),
    ("20260115120000_FixIncorrectOwnerIdRelationships", CodeRecordedAsApplied),
    ("20260116114245_AddLatestItemsDateCreatedIndexes", Schema),
    ("20260118182305_AddIndicesToImageInfo", Schema),
    ("20260130232147_AddBaseItemNameIndex", Schema),
    ("20260206200000_FixLibrarySubtitleDownloadLanguages", CodeLeftToRun),
    ("20260206224832_IndexOptimizations", Schema),
    ("20260215201634_ChangePrimaryVersionIdToGuid", Schema),
    ("20260302090000_MigrateRatingLevels", Retired),
    ("20260308123920_AddTypeCleanNameIndex", Schema),
    ("20260504075755_AddPartialIndexForItemCounts", Schema),
    ("20260508120000_MergeDuplicateMusicArtists", CodeLeftToRun),
    ("20260508130000_MergeDuplicatePeople", CodeLeftToRun),
    ("20260522092303_AddNormalizedUsername", Schema),
    ("20260522092304_UpdateNormalizedUsername", CodeLeftToRun),
    ("20260524120336_AddUniqueNormalizedUsernameIndex", Schema),
    ("20260525010000_CleanupOrphanedExternalData", CodeLeftToRun),
    ("20260531160000_DisableLegacyAuthorization", CodeLeftToRun),
    ("20260610120000_RefreshCleanNamesAndValues", CodeLeftToRun),
    ("20260722120000_RefreshForcedSortNames", CodeLeftToRun),
    ("20260723111547_AllowDuplicatePlaylistChildren", Schema),
    ("20260724185102_AddPrimaryVersionIdIndex", Schema),
    ("20260728170000_AddPeopleNameLowerIndex", Schema),
    ("20260728182152_AddPeopleItemMapCoveringIndex", Schema),
    ("20260729120000_RestorePlaylistChildrenFromMetadata", CodeLeftToRun),
    ("20260812050902_AddMediaStreamFilterIndex", Schema),
    ("20260815063607_RemoveOrphanedUserPermissionsAndPreferences", Schema),
    ("20260821120000_RecomputeSeriesPresentationKey", CodeLeftToRun),
    ("20260825200000_ConsolidateLocalizedUserViews", CodeLeftToRun),
    ("20260831100000_EnableLocalSimilarityProviders", CodeLeftToRun),
    ("20260908120000_RepairAlternateVersionLinks", CodeLeftToRun),
    ("20260910120000_MigrateRatingLevels", Retired),
    ("20260911120000_StripEmbeddedLinkedChildren", CodeLeftToRun),
    ("20260912120000_HarmonizeConflictingUserData", CodeLeftToRun),
    ("20260915104305_FixNullEncoderPreset", CodeLeftToRun),
    ("20260915120000_MigrateRatingLevels", CodeRecordedAsApplied),
    ("20261002210711_MakeOwnerIdIndexesPartial", Schema),
];
