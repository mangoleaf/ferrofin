//! `InternalItemsQuery` → SQL, built with a [`sqlx::QueryBuilder`].
//!
//! Port of `BaseItemRepository.TranslateQuery` /
//! `BaseItemRepository.QueryBuilding` (the EF-Core LINQ predicate pipeline).
//! Where the C# composes `IQueryable<BaseItemEntity>.Where(e => …)` lambdas,
//! this walks the same filter fields and appends `AND`-joined predicates to a
//! `WHERE` clause over the `BaseItems` table (aliased `bi`), then an `ORDER BY`
//! and `LIMIT`/`OFFSET`. The resulting builder can be finished into a
//! `query_as::<_, BaseItemEntity>()` (full rows), a `SELECT "Id"` (id-only), or a
//! `SELECT COUNT(*)` (counts) — see [`QueryShape`].
//!
//! Faithfulness and scope. The scalar-column filters, type include/exclude,
//! parent / ancestor / top-parent, provider-id presence, name/date/numeric
//! ranges, `ItemValues` joins (genres, tags, studios, artists, album-artists,
//! genre/studio ids), `UserData` predicates (favorite / played / liked /
//! resumable — the last with v12's Series/Season descendant roll-up), and the
//! ordering table are ported directly, as are 12.1's stream-language filters
//! (`AudioLanguages` / `SubtitleLanguages`) and `HasSubtitles` with v12's
//! alternate-version and folder roll-up
//! (`push_item_version_or_folder_matching`), and the resolution filters
//! (`IsHD` / `Is4K`, `Min`/`Max` `Width`/`Height`) with v12's
//! `VersionsMatchingDimension` and, for `IsHD` / `Is4K`, the folder
//! descendant roll-up. The played filter and sort keys take v12.2's
//! `BuildIsPlayedFilter` folder roll-up (`push_is_played`).
//!
//! `Guid` columns are stored as UPPERCASE hyphenated `TEXT` and datetimes as
//! `YYYY-MM-DD HH:MM:SS.fffffff` (Jellyfin's canonical storage formats), so
//! identity binds use [`guid_to_db`] and datetime binds use [`datetime_to_db`]
//! — byte-identical to Jellyfin-written rows under SQLite `BINARY` collation.

use ferrofin_util::string_extensions::lower_invariant;

use ferrofin_db::enums::{ItemValueType, LinkedChildType, MediaStreamTypeEntity};
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use sqlx::{QueryBuilder, Sqlite};
use uuid::Uuid;

use crate::item_type_lookup::stored_type_name;
use crate::text_util::get_clean_value;
use ferrofin_traits::options::InternalItemsQuery;

/// The placeholder item id seeded by the initial migration; every real query
/// excludes it (C# `PlaceholderId`).
pub(crate) const PLACEHOLDER_ID: &str = "00000000-0000-0000-0000-000000000001";

/// 12.1 `ApplyAlternateVersionFiltering`: keep a row unless it is a version
/// of a primary that still exists in the same library.
pub(crate) const ALTERNATE_VERSION_HIDDEN: &str = r#" AND (bi."PrimaryVersionId" IS NULL OR NOT EXISTS (SELECT 1 FROM "BaseItems" p WHERE p."Id" = bi."PrimaryVersionId" AND p."TopParentId" IS bi."TopParentId"))"#;

/// The "unowned" predicate. C# treats `OwnerId = Guid.Empty` as "no owner": a
/// real Jellyfin database stores the ZERO GUID on virtually every row
/// (adopted-DB evidence), while Ferrofin's writer leaves the column NULL — every
/// ownership predicate must accept both spellings of "unowned" or an adopted
/// library filters to nothing.
pub(crate) const NO_OWNER: &str =
    r#"(bi."OwnerId" IS NULL OR bi."OwnerId" = '00000000-0000-0000-0000-000000000000')"#;

/// HD/UHD resolution thresholds used by the `IsHD` / `Is4K` filters
/// (C# `HDWidth` / `UHDWidth` / `UHDHeight`).
const HD_WIDTH: i64 = 1200;
const UHD_WIDTH: i64 = 3800;
const UHD_HEIGHT: i64 = 2100;

/// What the translated query selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryShape {
    /// Select every column (`SELECT bi.*`) — materializes `BaseItemEntity` rows.
    FullRows,
    /// Select only the id column (`SELECT bi."Id"`).
    IdsOnly,
    /// Select the id and clean-name columns (`SELECT bi."Id", bi."CleanName"`) —
    /// the by-name resolvers' projection, which needs the join key back but not
    /// the row.
    IdAndCleanName,
    /// Select `COUNT(*)` over the ungrouped row set — no `ORDER BY` / paging.
    ///
    /// This is C# `GetCount` / `GetItemCounts`, which run `TranslateQuery`
    /// **without** `ApplyGroupingFilter` ("Hack for right now since we
    /// currently don't support filtering out these duplicates within a
    /// query"), so `/Items/Counts` reports rows, not titles.
    Count,
    /// Select `COUNT(*)` over the same row set the paged query returns — the
    /// `TotalRecordCount` of C# `GetItems`, which counts `dbQuery` *after*
    /// `ApplyGroupingFilter`. It must agree with the page it labels.
    GroupedCount,
    /// Select `bi."Type", COUNT(*)` grouped by type — the per-type counts in one
    /// query, without materializing every matching row. No `ORDER BY` / paging.
    TypeCounts,
}

/// Whether this query collapses merged versions into one row — C#
/// `BaseItemRepository.EnableGroupByPresentationUniqueKey`.
///
/// A `PresentationUniqueKey` is shared by every version of the same title, so
/// grouping on it is how upstream shows one entry for a movie that exists in
/// four cuts. The conditions are upstream's, in order: the caller must not have
/// turned grouping off, must not be grouping by *series* key instead, must not
/// have asked for one specific key, and there must be a user — and then either
/// no kind filter at all, or one of the six kinds that can have versions.
pub(crate) fn group_by_presentation_unique_key(filter: &InternalItemsQuery) -> bool {
    // A resume query surfaces the version that was actually PLAYED, which may be
    // an alternate sharing the primary's presentation key — and the predicate
    // above deliberately keeps alternates for exactly that reason. Grouping
    // would then collapse the surfaced version straight back onto the primary,
    // whose user data has no playback position, and the row would come back
    // with no progress on it.
    //
    // 10.11.8 has this bug; upstream fixed it afterwards, in the released
    // `EnableGroupByPresentationUniqueKey` on master, with the same reasoning
    // (and `0fb042b740` for the predicate half, which Ferrofin already had).
    // Taken deliberately, under the project's "don't port Jellyfin bugs" rule
    // — Ferrofin diverges from 10.11.8 here, toward the answer upstream itself
    // now gives.
    if filter.is_resumable == Some(true) {
        return false;
    }
    if !filter.group_by_presentation_unique_key
        || filter.group_by_series_presentation_unique_key
        || non_blank(filter.presentation_unique_key.as_ref()).is_some()
        || filter.user.is_none()
    {
        return false;
    }
    filter.include_item_types.is_empty()
        || filter.include_item_types.iter().any(|k| {
            matches!(
                k,
                BaseItemKind::Episode
                    | BaseItemKind::Video
                    | BaseItemKind::Movie
                    | BaseItemKind::MusicVideo
                    | BaseItemKind::Series
                    | BaseItemKind::Season
            )
        })
}

/// `Folder.Children` uses cached physical children (each physical folder in
/// its saved order), then `GetLinkedChildren` appends every link kind in its
/// saved order. Membership is deduplicated before visibility/filtering; no
/// request predicates, grouping, count or paging belong to this loader.
pub(crate) fn build_folder_children_query(
    parent: Uuid,
    physical_folders: &[Uuid],
    include_linked: bool,
) -> QueryBuilder<Sqlite> {
    let parents: Vec<_> = std::iter::once(parent)
        .chain(physical_folders.iter().copied().filter(|id| *id != parent))
        .collect();
    let mut qb = QueryBuilder::new(
        r#"WITH physical AS (SELECT bi."Id",
        CASE bi."ParentId" "#,
    );
    for (index, id) in parents.iter().enumerate() {
        qb.push("WHEN ")
            .push_bind(guid_to_db(*id))
            .push(" THEN ")
            .push_bind(i64::try_from(index).unwrap_or(i64::MAX))
            .push(" ");
    }
    qb.push(r#"END AS bucket FROM "BaseItems" bi WHERE "#);
    push_in_list(&mut qb, r#"bi."ParentId""#, &to_guid_strings(&parents));
    // The cached repository query hides owned non-extras and existing
    // alternate versions; the separately resolved links do not apply those
    // default query restrictions.
    append_predicates(&mut qb, &InternalItemsQuery::default());
    qb.push(
        r#"), linked AS (SELECT "ChildId", MIN("SortOrder") AS position
        FROM "LinkedChildren" WHERE "ParentId" = "#,
    )
    .push_bind(guid_to_db(parent));
    if !include_linked {
        qb.push(" AND 0 = 1");
    }
    qb.push(
        r#" GROUP BY "ChildId")
        SELECT bi.*, (p."Id" IS NULL) AS "_FolderLinked"
        FROM "BaseItems" bi LEFT JOIN physical p ON p."Id" = bi."Id"
        LEFT JOIN linked l ON l."ChildId" = bi."Id"
        WHERE bi."Id" <> "#,
    )
    .push_bind(PLACEHOLDER_ID)
    .push(
        r#" AND (p."Id" IS NOT NULL OR l."ChildId" IS NOT NULL)
        ORDER BY (p."Id" IS NULL), p.bucket,
        CASE WHEN p."Id" IS NOT NULL THEN +bi."SortName" END ASC, l.position ASC"#,
    );
    qb
}

/// Builds the full translated statement for `filter` in the requested shape.
///
/// The returned [`QueryBuilder`] is ready to `.build_query_as()` /
/// `.build_query_scalar()`. Ordering and paging are appended for
/// [`QueryShape::FullRows`], [`QueryShape::IdsOnly`] and
/// [`QueryShape::IdAndCleanName`]; the count shapes stop after the `WHERE`
/// clause.
#[must_use]
pub fn build_query(filter: &InternalItemsQuery, shape: QueryShape) -> QueryBuilder<Sqlite> {
    if filter.collapse_box_set_items == Some(true)
        && !matches!(shape, QueryShape::Count | QueryShape::TypeCounts)
    {
        return build_box_set_query(filter, shape);
    }
    let page_movie_keys = shape == QueryShape::FullRows && pages_movie_keys(filter);
    let projection = match shape {
        QueryShape::FullRows if page_movie_keys => {
            r#"WITH movie_page AS MATERIALIZED (
                SELECT bi."Id", bi."SortName", bi."Name", bi."ProductionYear"
                FROM "BaseItems" AS bi WHERE "#
        }
        QueryShape::FullRows => r#"SELECT bi.* FROM "BaseItems" AS bi WHERE "#,
        QueryShape::IdsOnly => r#"SELECT bi."Id" FROM "BaseItems" AS bi WHERE "#,
        QueryShape::IdAndCleanName => {
            r#"SELECT bi."Id", bi."CleanName" FROM "BaseItems" AS bi WHERE "#
        }
        QueryShape::Count | QueryShape::GroupedCount => {
            r#"SELECT COUNT(*) FROM "BaseItems" AS bi WHERE "#
        }
        QueryShape::TypeCounts => r#"SELECT bi."Type", COUNT(*) FROM "BaseItems" AS bi WHERE "#,
    };
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(projection);

    // Grouping belongs to the row-returning shapes and to the total that
    // labels them; see [`QueryShape::Count`] for why the count shapes are out.
    let groups = !matches!(shape, QueryShape::Count | QueryShape::TypeCounts)
        && group_by_presentation_unique_key(filter);
    if groups {
        push_group_head(&mut qb);
        append_predicates(&mut qb, filter);
        push_group_tail(&mut qb);
    } else {
        qb.push(r#"bi."Id" <> "#).push_bind(PLACEHOLDER_ID);
        append_predicates(&mut qb, filter);
    }

    match shape {
        // Aggregate shapes take no ORDER BY / paging (they collapse the row set).
        QueryShape::Count | QueryShape::GroupedCount => {}
        QueryShape::TypeCounts => {
            qb.push(r#" GROUP BY bi."Type""#);
        }
        QueryShape::FullRows | QueryShape::IdsOnly | QueryShape::IdAndCleanName => {
            append_order_by(&mut qb, filter);
            append_paging(&mut qb, filter);
        }
    }

    if page_movie_keys {
        // Keep the page as the outer loop: at most LIMIT full rows are read.
        // The final sort sees only the page, and preserves its input order for
        // ties, just as the original full-row sort did. No extra ID tiebreaker.
        qb.push(
            r#") SELECT bi.* FROM movie_page
                CROSS JOIN "BaseItems" AS bi ON bi."Id" = movie_page."Id"
                ORDER BY movie_page."SortName" ASC, movie_page."Name" ASC"#,
        );
        if filter.order_by.len() == 2 {
            qb.push(r#", movie_page."ProductionYear" ASC"#);
        }
    }

    qb
}

/// A grouped movie grid currently sorts full rows (including metadata blobs)
/// before discarding OFFSET rows. Sort only keys, then hydrate the final page.
/// Keep other orderings on their existing plans: search, random and user-data
/// sorts need other expressions, and ungrouped index walks have different tie
/// behavior. The shared builder still applies every predicate BEFORE grouping.
fn pages_movie_keys(filter: &InternalItemsQuery) -> bool {
    filter.limit.is_some_and(|limit| limit > 0)
        && filter.include_item_types == [BaseItemKind::Movie]
        && group_by_presentation_unique_key(filter)
        && non_blank(filter.search_term.as_ref()).is_none()
        && filter.virtual_child_parent_id.is_none()
        && matches!(
            filter.order_by.as_slice(),
            [(ItemSortBy::SortName, SortOrder::Ascending)]
                | [
                    (ItemSortBy::SortName, SortOrder::Ascending),
                    (ItemSortBy::ProductionYear, SortOrder::Ascending)
                ]
        )
}

/// Opens the grouped-representative subquery: everything up to (but not
/// including) the predicates that select the rows being grouped.
///
/// One row per `PresentationUniqueKey`, chosen from the rows the query actually
/// matches — C# groups `dbQuery` (already filtered) and then re-selects
/// `BaseItems` by the representative ids, so a predicate can never leave a
/// group represented by a row it excluded, and the outer query carries no
/// predicates of its own.
///
/// The representative is upstream's `g.Where(e => e.PrimaryVersionId ==
/// null).Min(e => (Guid?)e.Id) ?? g.Min(e => (Guid?)e.Id)`: the smallest id
/// among the group's *primary* versions, so a merged movie is never listed
/// under an alternate cut's title, else the smallest id in the group (when
/// none of the rows the query matched is a primary). Ids are stored as uppercase
/// hyphenated GUID text, so `MIN` is the text comparison EF's translation makes.
/// Before the `Id` tie-break, a group whose rows tie on primary-ness (two
/// seasons of one series with the same number, the keyless group described on
/// [`push_group_tail`], alternates whose primary is gone) was represented by
/// whichever row SQLite scanned first.
fn push_group_head(qb: &mut QueryBuilder<Sqlite>) {
    qb.push(r#"bi."Id" IN (SELECT "rep" FROM (SELECT COALESCE(MIN(CASE WHEN "#)
        .push(r#"bi."PrimaryVersionId" IS NULL THEN bi."Id" END), MIN(bi."Id")) AS "rep" "#)
        .push(r#"FROM "BaseItems" AS bi WHERE bi."Id" <> "#)
        .push_bind(PLACEHOLDER_ID);
}

/// Closes the subquery [`push_group_head`] opened.
///
/// A BARE `GROUP BY` on the column, as upstream's
/// `dbQuery.GroupBy(e => e.PresentationUniqueKey)` translates to (v10.11.8
/// Jellyfin.Server.Implementations/Item/BaseItemRepository.cs:417). SQLite
/// groups NULLs together, so every keyless row collapses into ONE result — and
/// on a real 10.11.8 that is not an accident: it is how the entire Live TV
/// guide shows up as a single `Program` row on an unfiltered recursive page
/// instead of as tens of thousands of airings. Measured on the parity oracle:
/// 338 `LiveTvProgram` rows, all with a NULL key, and exactly one `Program` in
/// the page.
///
/// This used to be `COALESCE(key, Id)`, justified by "Jellyfin always writes
/// one (2 nulls in 40,610 on a real library)". The guide disproves that: the
/// one kind upstream deliberately leaves keyless is the one the COALESCE was
/// hiding, and the COALESCE put the whole guide on every user's home query.
///
/// **The bare `GROUP BY` is only safe while every other row carries a key, and
/// that is a data invariant this statement cannot enforce.** Three Ferrofin
/// write paths used to leave by-name rows keyless; they are fixed at the
/// insert, and existing rows are repaired at boot — migration 0028 for the
/// kinds whose key is their own id, and
/// `ferrofin_db::presentation_key::backfill_by_name_presentation_keys` for the
/// five whose key is `{Type}-{Name}`. The first attempt at that repair
/// (migration 0027) was scoped `TopParentId IS NOT NULL` and therefore missed
/// every by-name row, which is precisely where the keyless rows were: the lane
/// pair measured `GET /Items?userId=…&ids=<three Person ids>` at ONE item
/// against Jellyfin's three until 0028 landed. A new write path that skips the
/// key reintroduces exactly that, and no query-shape test will see it — the
/// guard is `presentation_key_backfill_rescues_a_grouped_query` plus the
/// per-insert tests, not this comment.
fn push_group_tail(qb: &mut QueryBuilder<Sqlite>) {
    qb.push(r#" GROUP BY bi."PresentationUniqueKey"))"#);
}

/// Step 1 of v12's tvshows "latest" (`GetLatestTvShowItems`,
/// `Jellyfin.Server.Implementations/Item/BaseItemRepository.Querying.cs:287-303`):
/// the newest `filter.limit` series, by the newest `DateCreated` among the rows
/// the caller's filter matches, grouped by `SeriesName`, newest first. Each row
/// is `(SeriesName, MaxDate)`.
///
/// No grouping by presentation key: v12's `GetLatestItemList` builds its base
/// query with `TranslateQuery` alone, without `ApplyGroupingFilter`. The
/// caller's `order_by` and `start_index` play no part, as upstream.
#[must_use]
pub(crate) fn build_latest_tv_series_query(filter: &InternalItemsQuery) -> QueryBuilder<Sqlite> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        r#"SELECT bi."SeriesName" AS "SeriesName", MAX(bi."DateCreated") AS "MaxDate"
           FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
    );
    qb.push_bind(PLACEHOLDER_ID);
    append_predicates(&mut qb, filter);
    qb.push(r#" AND bi."SeriesName" IS NOT NULL GROUP BY bi."SeriesName" ORDER BY "MaxDate" DESC"#);
    if let Some(limit) = filter.limit {
        qb.push(" LIMIT ").push_bind(i64::from(limit).max(0));
    }
    qb
}

/// v12's movies "latest" (`GetLatestMovieItems`,
/// `Jellyfin.Server.Implementations/Item/BaseItemRepository.Querying.cs:233-256`):
/// the rows the caller's filter matches that carry a presentation key, newest
/// first (`DateCreated DESC, Id DESC`), as `(Id, PresentationUniqueKey)`. The
/// caller reads it as a stream and stops at `limit` distinct keys, keeping the
/// first row of each.
///
/// No grouping by presentation key and no `LIMIT`: v12's `GetLatestItemList`
/// builds its base query with `TranslateQuery` alone, and the de-duplication
/// is what the limit counts. The caller's `order_by` and `start_index` play no
/// part, as upstream.
#[must_use]
pub(crate) fn build_latest_movies_query(filter: &InternalItemsQuery) -> QueryBuilder<Sqlite> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        r#"SELECT bi."Id" AS "Id", bi."PresentationUniqueKey" AS "PresentationUniqueKey"
           FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
    );
    qb.push_bind(PLACEHOLDER_ID);
    append_predicates(&mut qb, filter);
    qb.push(
        r#" AND bi."PresentationUniqueKey" IS NOT NULL ORDER BY bi."DateCreated" DESC, bi."Id" DESC"#,
    );
    qb
}

/// Step 2 of `GetLatestTvShowItems` (`Querying.cs:313-325`): the rows of
/// `series_names` the caller's filter matches, created at or after `cutoff`
/// when there is one, as the lightweight `(Id, SeriesName, DateCreated,
/// SeasonId, SeriesId)` projection, newest first (`DateCreated DESC, Id
/// DESC`). `series_names` must not be empty.
#[must_use]
pub(crate) fn build_latest_tv_episodes_query(
    filter: &InternalItemsQuery,
    series_names: &[String],
    cutoff: Option<chrono::DateTime<chrono::Utc>>,
) -> QueryBuilder<Sqlite> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
        r#"SELECT bi."Id" AS "Id", bi."SeriesName" AS "SeriesName",
                  bi."DateCreated" AS "DateCreated", bi."SeasonId" AS "SeasonId",
                  bi."SeriesId" AS "SeriesId"
           FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
    );
    qb.push_bind(PLACEHOLDER_ID);
    append_predicates(&mut qb, filter);
    qb.push(r#" AND bi."SeriesName" IN ("#);
    let mut names = qb.separated(", ");
    for name in series_names {
        names.push_bind(name.clone());
    }
    qb.push(")");
    if let Some(cutoff) = cutoff {
        qb.push(r#" AND bi."DateCreated" >= "#)
            .push_bind(ferrofin_db::store::datetime_to_db(cutoff));
    }
    qb.push(r#" ORDER BY bi."DateCreated" DESC, bi."Id" DESC"#);
    qb
}

/// Builds the "latest media" statement for a **music** library — the music arm
/// of v12 `BaseItemRepository.GetLatestItemList`
/// (`Jellyfin.Server.Implementations/Item/BaseItemRepository.Querying.cs:147-180`).
///
/// v12 stopped asking "which albums contain the newest tracks" and asks "which
/// albums are newest": the statement selects `MusicAlbum` rows directly. Two
/// arms, exactly as upstream branches on `filter.TopParentIds`:
///
/// - **library-scoped** (`:150-157`) — `Type == MusicAlbum && !IsVirtualItem &&
///   TopParentId.HasValue`, `WhereOneOrMany` on the top parent ids. No track is
///   read at all: the caller's `baseQuery` (its media types, `IsPlayed`,
///   `IsFolder` and the rest) is deliberately NOT applied, because those
///   describe tracks and the album is what comes back;
/// - **ancestor-scoped fallback** (`:159-167`) — albums that are the
///   `AncestorIds` parent of a row the caller's own query matches. This arm DOES
///   run the caller's track query, as its inner subquery. It has no
///   `IsVirtualItem`/`TopParentId` test upstream, so it has none here.
///
/// The order is upstream's `OrderByDescending(DateCreated)
/// .ThenByDescending(Id)` and the limit caps ALBUMS — the caller's `order_by`
/// and `start_index` are both ignored, as they are in the C#.
///
/// Maximum parental scores apply to the returned albums as well as the
/// tracks in the ancestor-scoped arm.
///
/// Upstream's closing `LoadLatestByIds` (`:180`) is folded in: it re-selects the
/// same ids under the same `DateCreated DESC, Id DESC` purely to attach EF
/// navigations, and `SELECT bi.*` already carries the whole row.
#[must_use]
pub(crate) fn build_latest_music_albums_query(filter: &InternalItemsQuery) -> QueryBuilder<Sqlite> {
    let album_type = stored_type_name(BaseItemKind::MusicAlbum).unwrap_or_default();
    let mut qb: QueryBuilder<Sqlite> =
        QueryBuilder::new(r#"SELECT bi.* FROM "BaseItems" AS bi WHERE bi."Type" = "#);
    qb.push_bind(album_type);
    if filter.top_parent_ids.is_empty() {
        // `lam` is an alias `append_predicates` never opens: the shadowed
        // subquery below already uses `a` (the `ancestor_ids` EXISTS) and `la`
        // (the `linked_child_ancestor_ids` join), and shadowing an alias the
        // outer query still needs to reference is a correctness trap even
        // where SQLite happens to resolve it inside-out.
        qb.push(
            r#" AND bi."Id" IN (SELECT lam."ParentItemId" FROM "AncestorIds" AS lam
                WHERE lam."ItemId" IN (SELECT bi."Id" FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        )
        .push_bind(PLACEHOLDER_ID);
        // `baseQuery`, which upstream builds with `PrepareItemQuery` +
        // `TranslateQuery` only: no grouping filter, no ordering and no paging.
        append_predicates(&mut qb, filter);
        qb.push("))");
    } else {
        qb.push(r#" AND bi."IsVirtualItem" = 0 AND bi."TopParentId" IS NOT NULL AND "#);
        push_in_list(
            &mut qb,
            r#"bi."TopParentId""#,
            &to_guid_strings(&filter.top_parent_ids),
        );
    }
    append_parental_restrictions(&mut qb, filter, "bi");
    qb.push(r#" ORDER BY bi."DateCreated" DESC, bi."Id" DESC"#);
    // `limit.HasValue ? orderedAlbums.Take(limit.Value) : orderedAlbums` — a
    // `limit` of 0 really does mean no albums, as `Take(0)` does.
    //
    // The `max(0)` is deliberate HARDENING, not parity: upstream's `Take` stays
    // deferred (it is `IQueryable`, kept that way so EF emits `WHERE Id IN
    // (<subquery>)`), so a negative limit reaches SQLite there too and SQLite
    // reads a negative `LIMIT` as *unbounded*. Answering a nonsense request with
    // every album in the library is not worth reproducing. Unreachable today —
    // both `/Items/Latest` forms clamp with `.max(0)` in the handler — and NOT
    // mirrored into the tvshows builders, which would only move those
    // statements further from both upstream and the generic path.
    if let Some(limit) = filter.limit {
        qb.push(" LIMIT ").push_bind(i64::from(limit).max(0));
    }
    qb
}

/// Jellyfin's parental score, unrated-kind and inherited-tag restrictions.
pub(crate) fn append_parental_restrictions(
    qb: &mut QueryBuilder<Sqlite>,
    filter: &InternalItemsQuery,
    alias: &str,
) {
    append_max_parental_rating(qb, filter, alias);
    append_inherited_tags(qb, filter, alias);
    if filter.block_unrated_items.is_empty() {
        return;
    }
    qb.push(format!(r#" AND ({alias}."InheritedParentalRatingValue" IS NOT NULL OR COALESCE({alias}."UnratedType", CASE {alias}."Type""#));
    for (kind, unrated) in crate::kinds::UNRATED_KINDS {
        if let Some(name) = stored_type_name(*kind) {
            qb.push(" WHEN ")
                .push_bind(name.to_owned())
                .push(" THEN ")
                .push_bind(unrated.json_name().into_owned());
        }
    }
    // Old Ferrofin rows did not persist UnratedType. Derive their class/source
    // category immediately; a later normal metadata write also stamps the column.
    qb.push(" ELSE CASE WHEN ").push(format!(r#"({alias}."ChannelId" IS NOT NULL AND {alias}."ChannelId" <> '' AND {alias}."ChannelId" <> '00000000-0000-0000-0000-000000000000') OR {alias}."Type" = "#))
        .push_bind(stored_type_name(BaseItemKind::Channel).unwrap_or_default())
        .push(" THEN 'ChannelContent' WHEN ").push(format!(r#"{alias}."Type" = "#))
        .push_bind(stored_type_name(BaseItemKind::Audio).unwrap_or_default())
        .push(" THEN 'Music' ELSE 'Other' END END) NOT IN (");
    let mut values = qb.separated(", ");
    for value in &filter.block_unrated_items {
        values.push_bind(value.json_name().into_owned());
    }
    qb.push("))");
}

/// Whether an access query has any parental restriction.
pub(crate) fn has_parental_restrictions(filter: &InternalItemsQuery) -> bool {
    !filter.block_unrated_items.is_empty()
        || !filter.exclude_inherited_tags.is_empty()
        || !filter.include_inherited_tags.is_empty()
        || filter.max_parental_rating.is_some()
        || filter
            .user
            .as_ref()
            .is_some_and(|user| user.max_parental_rating_score.is_some())
}

// ApplyParentalRestrictions uses four separate indexed membership checks. A
// correlated EXISTS with a four-way OR defeats the tag-map index seeks.
fn append_inherited_tags(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery, alias: &str) {
    for (tags, blocked) in [
        (&filter.exclude_inherited_tags, true),
        (&filter.include_inherited_tags, false),
    ] {
        if tags.is_empty() {
            continue;
        }
        let tags: Vec<_> = tags.iter().map(|tag| get_clean_value(tag)).collect();
        let join = if blocked { " AND " } else { " OR " };
        let membership = if blocked { " NOT IN (" } else { " IN (" };
        qb.push(" AND (");
        for (index, column) in ["Id", "SeriesId", "TopParentId"].into_iter().enumerate() {
            if index != 0 {
                qb.push(join)
                    .push(format!("({alias}.\"{column}\" IS "))
                    .push(if blocked { "NULL OR " } else { "NOT NULL AND " });
            }
            qb.push(format!("{alias}.\"{column}\"")).push(membership);
            append_tagged_item_ids(qb, &tags);
            qb.push(")");
            if index != 0 {
                qb.push(")");
            }
        }
        // `ItemsBelowTaggedAncestor` (v12.2): the items below a tagged
        // ancestor as one uncorrelated set read by `ParentItemId`, built once
        // and probed per row. A correlated `EXISTS` seeks `(ItemId,
        // ParentItemId)` for every tagged id on every row. `AncestorIds.ItemId`
        // is `NOT NULL`, so `NOT IN` is exact.
        qb.push(join)
            .push(format!(r#"{alias}."Id""#))
            .push(membership)
            .push(
                r#"SELECT tag_parent."ItemId" FROM "AncestorIds" tag_parent
            WHERE tag_parent."ParentItemId" IN ("#,
            );
        append_tagged_item_ids(qb, &tags);
        qb.push("))");
        if !blocked {
            qb.push(format!(r#" OR {alias}."Type" = "#))
                .push_bind(stored_type_name(BaseItemKind::Person).unwrap_or_default());
        }
        qb.push(")");
    }
}

fn append_tagged_item_ids(qb: &mut QueryBuilder<Sqlite>, tags: &[String]) {
    qb.push(
        r#"SELECT tag_map."ItemId" FROM "ItemValues" tag_value
        CROSS JOIN "ItemValuesMap" tag_map ON tag_map."ItemValueId" = tag_value."ItemValueId"
        WHERE tag_value."Type" = "#,
    )
    .push_bind(i64::from(i32::from(ItemValueType::Tags)))
    .push(" AND ");
    push_in_list(qb, r#"tag_value."CleanValue""#, tags);
}

fn append_max_parental_rating(
    qb: &mut QueryBuilder<Sqlite>,
    filter: &InternalItemsQuery,
    alias: &str,
) {
    let limit = filter
        .max_parental_rating
        .as_ref()
        .map(|rating| {
            (
                i64::from(rating.score),
                i64::from(rating.sub_score.unwrap_or(0)),
            )
        })
        .or_else(|| {
            filter.user.as_ref().and_then(|user| {
                user.max_parental_rating_score
                    .map(|score| (score, user.max_parental_rating_sub_score.unwrap_or(0)))
            })
        });
    let Some((score, subscore)) = limit else {
        return;
    };
    // v12.2 `BuildMaxParentalRatingFilter` (`QueryBuilding.cs:663-689`): the
    // item's own rating, where it has one, is within the limit, AND a container
    // is only as visible as its members — an item with Manual links needs one
    // member within the limit. Only a Manual link makes an item a container,
    // so alternate-version links never gate a video.
    let manual = i32::from(LinkedChildType::Manual);
    qb.push(" AND (");
    push_rating_limit(qb, alias, score, subscore);
    qb.push(format!(
        r#" AND (NOT EXISTS (SELECT 1 FROM "LinkedChildren" lc
            WHERE lc."ParentId" = {alias}."Id" AND lc."ChildType" = {manual})
        OR EXISTS (SELECT 1 FROM "LinkedChildren" lc JOIN "BaseItems" rated ON rated."Id" = lc."ChildId"
        WHERE lc."ParentId" = {alias}."Id" AND lc."ChildType" = {manual} AND "#
    ));
    push_rating_limit(qb, "rated", score, subscore);
    qb.push(")))");
}

fn push_rating_limit(qb: &mut QueryBuilder<Sqlite>, alias: &str, score: i64, subscore: i64) {
    qb.push(format!(r#"({alias}."InheritedParentalRatingValue" IS NULL OR {alias}."InheritedParentalRatingValue" < "#))
        .push_bind(score)
        .push(format!(r#" OR ({alias}."InheritedParentalRatingValue" = "#))
        .push_bind(score)
        .push(format!(r#" AND COALESCE({alias}."InheritedParentalRatingSubValue", 0) <= "#))
        .push_bind(subscore).push("))");
}

/// Appends every `AND <predicate>` clause derived from the filter.
///
/// Each block mirrors one C# `if (filter.X …) baseQuery = baseQuery.Where(…)`.
/// Split out so [`build_query`] and the count path share the exact same WHERE.
#[allow(clippy::too_many_lines)]
pub(crate) fn append_predicates(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    // TranslateQuery's explicit rating selector bypasses the unrated-kind
    // clause. Access-only queries (counts and related media) retain it.
    if filter.has_parental_rating == Some(true) {
        append_max_parental_rating(qb, filter, "bi");
        append_inherited_tags(qb, filter, "bi");
    } else {
        append_parental_restrictions(qb, filter, "bi");
    }
    // --- resolution: the best any version offers, folders on descendants ---
    if filter.is_hd.is_some() || filter.is_4k.is_some() {
        append_resolution_predicate(qb, filter);
    }
    append_dimension_bounds(qb, filter);

    if let Some(locked) = filter.is_locked {
        qb.push(r#" AND bi."IsLocked" = "#).push_bind(locked);
    }

    // Sports/News/Kids are tag sugar in C#; fold them into the tag sets.
    let mut tags: Vec<String> = filter.tags.clone();
    let mut exclude_tags: Vec<String> = filter.exclude_tags.clone();
    for (flag, name) in [
        (filter.is_sports, "Sports"),
        (filter.is_news, "News"),
        (filter.is_kids, "Kids"),
    ] {
        match flag {
            Some(true) => tags.push(name.to_owned()),
            Some(false) => exclude_tags.push(name.to_owned()),
            None => {}
        }
    }

    if let Some(is_movie) = filter.is_movie {
        // C#: skip the predicate when the query already targets movie-ish types.
        let include_all_movie_types = is_movie
            && (filter.include_item_types.is_empty()
                || filter.include_item_types.contains(&BaseItemKind::Movie)
                || filter.include_item_types.contains(&BaseItemKind::Trailer));
        if !include_all_movie_types {
            qb.push(r#" AND bi."IsMovie" = "#).push_bind(is_movie);
        }
    }
    if let Some(is_series) = filter.is_series {
        qb.push(r#" AND bi."IsSeries" = "#).push_bind(is_series);
    }

    if let Some(term) = non_blank(filter.search_term.as_ref()) {
        let like = format!("%{}%", get_clean_value(term).trim_matches('%'));
        let orig_like = format!("%{}%", lower_invariant(term));
        qb.push(r#" AND (bi."CleanName" LIKE "#)
            .push_bind(like)
            .push(r#" OR (bi."OriginalTitle" IS NOT NULL AND ferrofin_lower_invariant(bi."OriginalTitle") LIKE "#)
            .push_bind(orig_like)
            .push("))");
    }

    if let Some(is_folder) = filter.is_folder {
        // `+`: a plan pin for the `DescendantOfId` shape. Its descendant set
        // (`append_linked_descendant_predicate`) is one container's contents,
        // so it must drive the statement, reaching `BaseItems` by id — as
        // upstream's materialized `WhereOneOrMany(descendantIds, b => b.Id)`
        // does. Left to itself SQLite seeds from `IX_BaseItems_IsFolder_…`
        // (`IsFolder=?`), every non-folder row in the library, and probes the
        // set per row: ~10 ms per leaf count of one series on the bench
        // library against ~0.05 ms driven by the set. Other shapes keep the
        // plain term, which their own plans may use.
        if filter.descendant_of_id.is_some() {
            qb.push(r#" AND +bi."IsFolder" = "#).push_bind(is_folder);
        } else {
            qb.push(r#" AND bi."IsFolder" = "#).push_bind(is_folder);
        }
    }

    append_type_filters(qb, filter);

    if !filter.channel_ids.is_empty() {
        qb.push(r#" AND bi."ChannelId" IS NOT NULL AND "#);
        push_in_list(
            qb,
            r#"bi."ChannelId""#,
            &to_guid_strings(&filter.channel_ids),
        );
    }

    if filter.parent_id != Uuid::nil() {
        if filter.recursive {
            // Recursive browse (e.g. a library's "Episodes" tab): match any
            // descendant via the `AncestorIds` closure, not just direct children.
            // Port of Jellyfin translating a recursive `ParentId` into an ancestor
            // query rather than a `ParentId` equality.
            qb.push(
                r#" AND EXISTS (SELECT 1 FROM "AncestorIds" a WHERE a."ItemId" = bi."Id" AND a."ParentItemId" = "#,
            )
            .push_bind(guid_to_db(filter.parent_id))
            .push(")");
        } else if filter.physical_children_only {
            // Physical children only (delete-cascade): NEVER merge FerrofinLinkedChildren, so
            // deleting a box-set/playlist removes only the container, not the items it
            // references (linked children are references, not owned children).
            qb.push(r#" AND bi."ParentId" = "#)
                .push_bind(guid_to_db(filter.parent_id));
        } else {
            // Direct children: the physical `ParentId`, plus manually linked
            // members (C# `Folder.GetChildren` merges `LinkedChildren`). Only
            // box-sets and playlists carry `LinkedChildren` rows, so the `IN`
            // subquery is empty for ordinary folders and this stays identical to
            // a plain `ParentId` equality for non-collection browses.
            qb.push(r#" AND (bi."ParentId" = "#)
                .push_bind(guid_to_db(filter.parent_id))
                .push(
                    r#" OR bi."Id" IN (SELECT "ChildId" FROM "LinkedChildren" WHERE "ParentId" = "#,
                )
                .push_bind(guid_to_db(filter.parent_id))
                .push(r#" AND "ChildType" = 0)"#);
            // …and, on an adopted Jellyfin database, the children of the
            // library's physical folders. A `CollectionFolder` there is
            // virtual — nothing carries its id as `ParentId` — so the equality
            // above matches nothing and this is the whole answer (C#
            // `CollectionFolder.GetActualChildren`, which unions
            // `GetPhysicalFolders().SelectMany(c => c.Children)`). Empty, and
            // so a no-op, on a Ferrofin-written database.
            if !filter.parent_physical_folder_ids.is_empty() {
                qb.push(" OR ");
                push_in_list(
                    qb,
                    r#"bi."ParentId""#,
                    &to_guid_strings(&filter.parent_physical_folder_ids),
                );
            }
            // …and the `AggregateFolder`'s virtual children, when the parent is
            // the `UserRootFolder` (C#
            // `UserRootFolder.GetEligibleChildrenForRecursiveChildren`, which
            // concatenates `LibraryManager.RootFolder.VirtualChildren`).
            //
            // Narrowed to the plug-in-folder types on purpose:
            // `AddVirtualChild` has exactly ONE call site in 10.11.8
            // (LibraryManager.cs:883, the playlists folder), and the
            // aggregate's other children — the physical library folders,
            // `%AppDataPath%/collections`, the recordings folder — must not
            // leak into the user root's children.
            if let Some(aggregate) = filter.virtual_child_parent_id {
                qb.push(r#" OR (bi."ParentId" = "#)
                    .push_bind(guid_to_db(aggregate))
                    .push(r#" AND bi."Type" IN ("#);
                let mut first = true;
                for kind in [
                    BaseItemKind::PlaylistsFolder,
                    BaseItemKind::ManualPlaylistsFolder,
                ] {
                    if let Some(name) = stored_type_name(kind) {
                        if !first {
                            qb.push(", ");
                        }
                        first = false;
                        qb.push_bind(name.to_owned());
                    }
                }
                qb.push("))");
            }
            qb.push(")");
        }
    }

    if let Some(parent) = filter.descendant_of_id {
        append_linked_descendant_predicate(qb, filter, parent);
    }

    if let Some(path) = non_blank(filter.path.as_ref()) {
        qb.push(r#" AND bi."Path" = "#).push_bind(path.to_owned());
    }
    if let Some(key) = non_blank(filter.presentation_unique_key.as_ref()) {
        qb.push(r#" AND bi."PresentationUniqueKey" = "#)
            .push_bind(key.to_owned());
    }

    if let Some(rating) = filter.min_community_rating {
        qb.push(r#" AND bi."CommunityRating" >= "#)
            .push_bind(rating);
    }
    if let Some(rating) = filter.min_critic_rating {
        qb.push(r#" AND bi."CriticRating" >= "#).push_bind(rating);
    }
    if let Some(n) = filter.min_index_number {
        qb.push(r#" AND bi."IndexNumber" >= "#)
            .push_bind(i64::from(n));
    }
    if let Some((parent, index)) = filter.min_parent_and_index_number {
        qb.push(r#" AND ((bi."ParentIndexNumber" = "#)
            .push_bind(i64::from(parent))
            .push(r#" AND bi."IndexNumber" >= "#)
            .push_bind(i64::from(index))
            .push(r#") OR bi."ParentIndexNumber" > "#)
            .push_bind(i64::from(parent))
            .push(")");
    }
    if let Some(n) = filter.index_number {
        qb.push(r#" AND bi."IndexNumber" = "#)
            .push_bind(i64::from(n));
    }
    if let Some(n) = filter.parent_index_number {
        qb.push(r#" AND bi."ParentIndexNumber" = "#)
            .push_bind(i64::from(n));
    }
    if let Some(n) = filter.parent_index_number_not_equals {
        qb.push(r#" AND (bi."ParentIndexNumber" <> "#)
            .push_bind(i64::from(n))
            .push(r#" OR bi."ParentIndexNumber" IS NULL)"#);
    }

    append_date_predicates(qb, filter);

    // --- identity / external ids ---
    if let Some(id) = non_blank(filter.external_series_id.as_ref()) {
        qb.push(r#" AND bi."ExternalSeriesId" = "#)
            .push_bind(id.to_owned());
    }
    if let Some(id) = non_blank(filter.external_id.as_ref()) {
        qb.push(r#" AND bi."ExternalId" = "#)
            .push_bind(id.to_owned());
    }

    append_name_predicates(qb, filter, &tags, &exclude_tags);

    append_user_data_predicates(qb, filter);
    append_media_attribute_predicates(qb, filter);
    append_stream_language_predicates(qb, filter);
    append_item_value_predicates(qb, filter, &tags, &exclude_tags);
    append_people_predicates(qb, filter);

    if let Some(has) = filter.has_overview {
        if has {
            qb.push(r#" AND (bi."Overview" IS NOT NULL AND bi."Overview" <> '')"#);
        } else {
            qb.push(r#" AND (bi."Overview" IS NULL OR bi."Overview" = '')"#);
        }
    }
    if let Some(has) = filter.has_official_rating {
        if has {
            qb.push(r#" AND (bi."OfficialRating" IS NOT NULL AND bi."OfficialRating" <> '')"#);
        } else {
            qb.push(r#" AND (bi."OfficialRating" IS NULL OR bi."OfficialRating" = '')"#);
        }
    }
    if !filter.official_ratings.is_empty() {
        qb.push(" AND ");
        push_in_list(qb, r#"bi."OfficialRating""#, &filter.official_ratings);
    }

    if let Some(has) = filter.has_owner_id {
        if has {
            qb.push(" AND NOT ").push(NO_OWNER);
        } else {
            qb.push(" AND ").push(NO_OWNER);
        }
    } else if filter.owner_ids.is_empty()
        && filter.extra_types.is_empty()
        && !filter.include_owned_items
    {
        // Exclude alternate versions + owned non-extra items from general
        // queries — 12.1 `BaseItemRepository.ApplyAlternateVersionFiltering`:
        // a row is hidden only while its primary exists *in the same library*
        // (`TopParentId`), so a version whose primary is gone, or was moved to
        // another library, is listed again as an item of its own. (12.0 hid
        // every row with a `PrimaryVersionId`; on the owner's library that was
        // 144 episodes.) A resume query keeps the alternates so the version
        // that was actually played surfaces instead of collapsing onto the
        // primary. The subquery is a primary-key probe per candidate row, and
        // `IS` keeps EF's null-equal semantics for two library-less rows.
        if filter.is_resumable == Some(true) {
            qb.push(" AND (")
                .push(NO_OWNER)
                .push(r#" OR bi."ExtraType" IS NOT NULL)"#);
        } else {
            qb.push(ALTERNATE_VERSION_HIDDEN)
                .push(" AND (")
                .push(NO_OWNER)
                .push(r#" OR bi."ExtraType" IS NOT NULL)"#);
        }
    }
    if !filter.owner_ids.is_empty() {
        qb.push(r#" AND bi."OwnerId" IS NOT NULL AND "#);
        push_in_list(qb, r#"bi."OwnerId""#, &to_guid_strings(&filter.owner_ids));
    }

    // ExtraTypes: restrict to items whose stored `ExtraType` discriminant is one
    // of the requested extra types. C# `BaseItemRepository.TranslateQuery` casts
    // each `ExtraType` to its integer value and matches `e.ExtraType`; the
    // discriminants are stored verbatim as `INTEGER` here, so the cast is the
    // enum's `i32` value (`ExtraType::Trailer.json_value()`, …).
    if !filter.extra_types.is_empty() {
        qb.push(r#" AND bi."ExtraType" IS NOT NULL AND "#);
        let values: Vec<i64> = filter
            .extra_types
            .iter()
            .map(|e| i64::from(e.json_value()))
            .collect();
        push_in_list(qb, r#"bi."ExtraType""#, &values);
    }

    append_provider_predicates(qb, filter);

    if !filter.years.is_empty() {
        qb.push(" AND ");
        let years: Vec<i64> = filter.years.iter().map(|y| i64::from(*y)).collect();
        push_in_list(qb, r#"bi."ProductionYear""#, &years);
    }

    // IsVirtualItem also covers IsMissing (C# `filter.IsVirtualItem ?? filter.IsMissing`).
    if let Some(virt) = filter.is_virtual_item.or(filter.is_missing) {
        qb.push(r#" AND bi."IsVirtualItem" = "#).push_bind(virt);
    }

    if let Some(special) = filter.is_special_season {
        qb.push(if special {
            r#" AND bi."IndexNumber" = 0"#
        } else {
            r#" AND bi."IndexNumber" <> 0"#
        });
    }

    if !filter.media_types.is_empty() {
        qb.push(" AND ");
        let media: Vec<String> = filter
            .media_types
            .iter()
            .copied()
            .map(media_type_name)
            .collect();
        push_in_list(qb, r#"bi."MediaType""#, &media);
    }

    if !filter.item_ids.is_empty() {
        qb.push(" AND ");
        push_in_list(qb, r#"bi."Id""#, &to_guid_strings(&filter.item_ids));
    }
    if !filter.exclude_item_ids.is_empty() {
        qb.push(" AND NOT ");
        push_in_list(qb, r#"bi."Id""#, &to_guid_strings(&filter.exclude_item_ids));
    }

    append_ancestor_predicates(qb, filter);

    if let Some(key) = non_blank(filter.series_presentation_unique_key.as_ref()) {
        qb.push(r#" AND bi."SeriesPresentationUniqueKey" = "#)
            .push_bind(key.to_owned());
    }

    // The `IsDead*` filters only narrow when explicitly `true` (C# `?? false`).
    if filter.is_dead_person == Some(true) {
        qb.push(r#" AND NOT EXISTS (SELECT 1 FROM "Peoples" p WHERE p."Name" = bi."Name")"#);
    }
    if filter.is_dead_genre == Some(true) {
        append_is_dead_item_value(qb, &[ItemValueType::Genre]);
    }
    if filter.is_dead_studio == Some(true) {
        append_is_dead_item_value(qb, &[ItemValueType::Studios]);
    }
    if filter.is_dead_artist == Some(true) {
        append_is_dead_item_value(qb, &[ItemValueType::Artist, ItemValueType::AlbumArtist]);
    }

    if let Some(season) = filter.aired_during_season {
        if season < 1 {
            qb.push(r#" AND bi."ParentIndexNumber" = "#)
                .push_bind(i64::from(season));
        } else {
            let after = format!("\"AirsAfterSeasonNumber\":{season}");
            let before = format!("\"AirsBeforeSeasonNumber\":{season}");
            qb.push(r#" AND (bi."ParentIndexNumber" = "#)
                .push_bind(i64::from(season))
                .push(r#" OR (bi."Data" IS NOT NULL AND (bi."Data" LIKE "#)
                .push_bind(format!("%{after}%"))
                .push(r#" OR bi."Data" LIKE "#)
                .push_bind(format!("%{before}%"))
                .push(")))");
        }
    }
}

/// Composes the resolved closure and linked membership with every outer filter.
/// The repository loads only root folders; leaf IDs stay in this sub-select,
/// and the same resolved query supplies count, sort and page statements.
fn append_linked_descendant_predicate(
    qb: &mut QueryBuilder<Sqlite>,
    filter: &InternalItemsQuery,
    parent: Uuid,
) {
    let Some(roots) = filter
        .descendant_roots
        .as_ref()
        .filter(|r| r.parent_id == parent)
    else {
        // An unresolved logical filter must not silently turn into a global
        // browse. Repository/count entry points resolve it before translation.
        qb.push(" AND 0 = 1");
        return;
    };
    qb.push(
        r#" AND bi."Id" IN (WITH
        closure_roots("Id") AS (SELECT value FROM json_each("#,
    )
    .push_bind(serde_json::json!(to_guid_strings(&roots.closure_roots)).to_string())
    .push(r#")), link_roots("Id") AS (SELECT value FROM json_each("#)
    .push_bind(serde_json::json!(to_guid_strings(&roots.link_roots)).to_string())
    .push(
        r#")), closure_direct("Id") AS (
            SELECT a."ItemId" FROM "AncestorIds" a
            JOIN closure_roots root ON root."Id" = a."ParentItemId"
        ), all_descendants("Id") AS (
            SELECT "Id" FROM closure_direct
            UNION
            SELECT a."ItemId" FROM "AncestorIds" a
            JOIN closure_direct d ON d."Id" = a."ParentItemId"
            UNION
            SELECT link."ChildId" FROM "LinkedChildren" link
            JOIN link_roots root ON root."Id" = link."ParentId"
        ) SELECT "Id" FROM all_descendants WHERE "Id" <> "#,
    )
    .push_bind(guid_to_db(parent))
    .push(")");
}

/// Pushes v12 `VersionsMatchingDimension` (`TranslateQuery.cs:73-79`): the
/// `PrimaryVersionId` of every alternate version satisfying `bound`, which
/// `push_bound` appends over the alternate's `BaseItems` row aliased `v`.
/// Anchored on the partial `PrimaryVersionId` index, so it reads only the few
/// items that are second files. `PrimaryVersionId IS NOT NULL` also keeps a
/// `NOT IN` over it free of SQL's NULL trap.
fn push_versions_matching_dimension(
    qb: &mut QueryBuilder<Sqlite>,
    push_bound: impl FnOnce(&mut QueryBuilder<Sqlite>),
) {
    qb.push(
        r#"SELECT v."PrimaryVersionId" FROM "BaseItems" v WHERE v."PrimaryVersionId" IS NOT NULL AND "#,
    );
    push_bound(qb);
}

/// Appends `MinWidth` / `MinHeight` / `MaxWidth` / `MaxHeight`
/// (v12 `TranslateQuery.cs:169-196`). An item's resolution is the best its
/// version group offers: a lower bound is met when the item or any alternate
/// version meets it, an upper bound only when the item does and no alternate
/// version breaches it.
fn append_dimension_bounds(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    for (column, bound) in [("Width", filter.min_width), ("Height", filter.min_height)] {
        let Some(bound) = bound.map(i64::from) else {
            continue;
        };
        qb.push(format!(r#" AND (bi."{column}" >= "#))
            .push_bind(bound)
            .push(r#" OR bi."Id" IN ("#);
        push_versions_matching_dimension(qb, |qb| {
            qb.push(format!(r#"v."{column}" >= "#)).push_bind(bound);
        });
        qb.push("))");
    }
    for (column, bound) in [("Width", filter.max_width), ("Height", filter.max_height)] {
        let Some(bound) = bound.map(i64::from) else {
            continue;
        };
        qb.push(format!(r#" AND (bi."{column}" <= "#))
            .push_bind(bound)
            .push(r#" AND bi."Id" NOT IN ("#);
        push_versions_matching_dimension(qb, |qb| {
            qb.push(format!(r#"v."{column}" > "#)).push_bind(bound);
        });
        qb.push("))");
    }
}

/// The `IsHD` / `Is4K` buckets a query selects (v12 `includeSD` /
/// `includeHD` / `include4K`).
#[derive(Debug, Clone, Copy)]
struct ResolutionBuckets {
    /// `IsHD = false`.
    standard_def: bool,
    /// `IsHD = true`.
    high_def: bool,
    /// `Is4K = true`.
    ultra_hd: bool,
}

/// One `IsHD` / `Is4K` bucket, for placing an alternate version in it.
#[derive(Debug, Clone, Copy)]
enum Resolution {
    /// Known width below HD.
    Standard,
    /// HD width or wider.
    High,
    /// 4K width or height.
    Ultra,
}

impl Resolution {
    /// v12's `VersionsMatchingDimension` bound for this bucket, over an
    /// alternate aliased `v` (`TranslateQuery.cs:121-123`).
    fn version_bound(self) -> String {
        match self {
            Self::Standard => format!(r#"v."Width" > 0 AND v."Width" < {HD_WIDTH}"#),
            Self::High => format!(r#"v."Width" >= {HD_WIDTH}"#),
            Self::Ultra => format!(r#"(v."Width" >= {UHD_WIDTH} OR v."Height" >= {UHD_HEIGHT})"#),
        }
    }
}

/// Pushes `{id} IN` (or `NOT IN`) the primaries with an alternate version in
/// `bucket`.
fn push_versions_in_bucket(
    qb: &mut QueryBuilder<Sqlite>,
    id: &str,
    negated: bool,
    bucket: Resolution,
) {
    let op = if negated { "NOT IN" } else { "IN" };
    qb.push(format!(" {id} {op} ("));
    push_versions_matching_dimension(qb, |qb| {
        qb.push(bucket.version_bound());
    });
    qb.push(")");
}

/// Pushes the selected buckets, OR-ed, for the row aliased `row` whose id is
/// the expression `id` — one bucket per v12 arm (`TranslateQuery.cs:131-142`):
///
/// - SD: the row (`Width` known and below HD) or an alternate is SD, and no
///   alternate is HD or 4K;
/// - HD: the row or an alternate is HD-wide, the row itself is not 4K, and no
///   alternate is 4K;
/// - 4K: the row or an alternate is 4K.
///
/// The HD arm's "not itself 4K" is a `CASE … THEN 0 ELSE 1`, the form EF
/// lowers it to: an unprobed row (NULL dimensions) reads as not 4K rather
/// than as an unknown that drops the row.
fn push_resolution_buckets(
    qb: &mut QueryBuilder<Sqlite>,
    buckets: ResolutionBuckets,
    row: &str,
    id: &str,
) {
    use Resolution::{High, Standard, Ultra};
    let row_is_4k = format!(r#"{row}."Width" >= {UHD_WIDTH} OR {row}."Height" >= {UHD_HEIGHT}"#);
    let mut separator = "(";
    if buckets.standard_def {
        qb.push(format!(
            r#"{separator}((({row}."Width" > 0 AND {row}."Width" < {HD_WIDTH}) OR"#
        ));
        push_versions_in_bucket(qb, id, false, Standard);
        qb.push(") AND");
        push_versions_in_bucket(qb, id, true, High);
        qb.push(" AND");
        push_versions_in_bucket(qb, id, true, Ultra);
        qb.push(")");
        separator = " OR ";
    }
    if buckets.high_def {
        qb.push(format!(r#"{separator}(({row}."Width" >= {HD_WIDTH} OR"#));
        push_versions_in_bucket(qb, id, false, High);
        qb.push(format!(") AND CASE WHEN {row_is_4k} THEN 0 ELSE 1 END AND"));
        push_versions_in_bucket(qb, id, true, Ultra);
        qb.push(")");
        separator = " OR ";
    }
    if buckets.ultra_hd {
        qb.push(format!("{separator}({row_is_4k} OR"));
        push_versions_in_bucket(qb, id, false, Ultra);
        qb.push(")");
    }
    qb.push(")");
}

/// Appends the `IsHD` / `Is4K` resolution filter (v12
/// `TranslateQuery.cs:95-167`). A non-folder is bucketed on the best its
/// version group offers ([`push_resolution_buckets`]); a folder matches when
/// any closure descendant (`Children`, the `AncestorIds` rows naming it) or
/// any directly linked child (`LinkedChildren`, every link type, as upstream)
/// falls in a selected bucket, bucketed exactly as a top-level item. Selecting
/// no bucket (`Is4K = false` alone) matches nothing, as upstream.
///
/// `+`: the same plan pin as [`push_is_played`] on both `IsFolder` disjuncts,
/// so a browse of three or more kinds keeps its `(Type, TopParentId)` seek
/// instead of a `MULTI-INDEX OR` over every row of every library in scope.
fn append_resolution_predicate(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    let buckets = ResolutionBuckets {
        standard_def: filter.is_hd == Some(false),
        high_def: filter.is_hd == Some(true),
        ultra_hd: filter.is_4k == Some(true),
    };
    if !(buckets.standard_def || buckets.high_def || buckets.ultra_hd) {
        qb.push(" AND 0");
        return;
    }
    qb.push(r#" AND ((+bi."IsFolder" = 0 AND "#);
    push_resolution_buckets(qb, buckets, "bi", r#"bi."Id""#);
    qb.push(
        r#") OR (+bi."IsFolder" = 1 AND (EXISTS (SELECT 1 FROM "AncestorIds" a JOIN "BaseItems" c ON c."Id" = a."ItemId" WHERE a."ParentItemId" = bi."Id" AND "#,
    );
    push_resolution_buckets(qb, buckets, "c", r#"a."ItemId""#);
    qb.push(
        r#") OR EXISTS (SELECT 1 FROM "LinkedChildren" lc JOIN "BaseItems" c ON c."Id" = lc."ChildId" WHERE lc."ParentId" = bi."Id" AND "#,
    );
    push_resolution_buckets(qb, buckets, "c", r#"lc."ChildId""#);
    qb.push("))))");
}

/// Appends `IncludeItemTypes` / `ExcludeItemTypes` as `Type` in/not-in lists.
///
/// With an `Ids` list in the query the `Type` column is written `+bi."Type"`
/// so no `Type`-led index can serve it: 12.0 dropped
/// `IX_BaseItems_Id_Type_IsFolder_IsVirtualItem`, and without it a single
/// `Type` equality ties with the `Id` primary key in the planner's costing and
/// wins — a scan of every row of that type (3,001 movies on the bench corpus)
/// instead of a handful of key lookups. The `Ids` list is always the selective
/// side, so the pin loses nothing.
fn append_type_filters(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    let column = if filter.item_ids.is_empty() {
        r#"bi."Type""#
    } else {
        r#"+bi."Type""#
    };
    if filter.include_item_types.is_empty() {
        let excludes: Vec<String> = filter
            .exclude_item_types
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .map(ToOwned::to_owned)
            .collect();
        if !excludes.is_empty() {
            qb.push(" AND NOT ");
            push_in_list(qb, column, &excludes);
        }
    } else {
        let includes: Vec<String> = filter
            .include_item_types
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .map(ToOwned::to_owned)
            .collect();
        qb.push(" AND ");
        push_in_list(qb, column, &includes);
    }
}

/// The `IsAiring` predicate on the item table aliased `alias` (v12.2
/// `TranslateQuery.cs:433-443`): `true` keeps what is on at `now`
/// (`StartDate <= now && EndDate >= now`), `false` what is not
/// (`StartDate > now || EndDate < now`, the negation jellyfin/jellyfin#17588
/// fixed in v12.0-rc5). `now` is already in the stored date format.
pub(crate) fn push_is_airing(
    qb: &mut QueryBuilder<Sqlite>,
    alias: &str,
    is_airing: bool,
    now: &str,
) {
    if is_airing {
        qb.push(format_args!(r#" AND {alias}."StartDate" <= "#))
            .push_bind(now.to_owned())
            .push(format_args!(r#" AND {alias}."EndDate" >= "#))
            .push_bind(now.to_owned());
    } else {
        qb.push(format_args!(r#" AND ({alias}."StartDate" > "#))
            .push_bind(now.to_owned())
            .push(format_args!(r#" OR {alias}."EndDate" < "#))
            .push_bind(now.to_owned())
            .push(")");
    }
}

/// Appends the date-range predicates, including the `HasAired` sugar that maps to
/// an end-date bound relative to now.
fn append_date_predicates(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    let now = chrono::Utc::now();
    let mut min_end = filter.min_end_date;
    let mut max_end = filter.max_end_date;
    match filter.has_aired {
        Some(true) => max_end = Some(now),
        Some(false) => min_end = Some(now),
        None => {}
    }
    if let Some(d) = min_end {
        qb.push(r#" AND bi."EndDate" >= "#)
            .push_bind(datetime_to_db(d));
    }
    if let Some(d) = max_end {
        qb.push(r#" AND bi."EndDate" <= "#)
            .push_bind(datetime_to_db(d));
    }
    if let Some(d) = filter.min_start_date {
        qb.push(r#" AND bi."StartDate" >= "#)
            .push_bind(datetime_to_db(d));
    }
    if let Some(d) = filter.max_start_date {
        qb.push(r#" AND bi."StartDate" <= "#)
            .push_bind(datetime_to_db(d));
    }
    if let Some(d) = filter.min_premiere_date {
        qb.push(r#" AND bi."PremiereDate" >= "#)
            .push_bind(datetime_to_db(d));
    }
    if let Some(d) = filter.max_premiere_date {
        qb.push(r#" AND bi."PremiereDate" <= "#)
            .push_bind(datetime_to_db(d));
    }
    if let Some(d) = filter.min_date_created {
        qb.push(r#" AND bi."DateCreated" >= "#)
            .push_bind(datetime_to_db(d));
    }
    // Two independent bounds, as upstream's two `Where` clauses: a request
    // carrying both is held to each.
    for d in [
        filter.min_date_last_saved,
        filter.min_date_last_saved_for_user,
    ]
    .into_iter()
    .flatten()
    {
        qb.push(r#" AND (bi."DateLastSaved" IS NOT NULL AND bi."DateLastSaved" >= "#)
            .push_bind(datetime_to_db(d))
            .push(")");
    }
    if let Some(is_airing) = filter.is_airing {
        push_is_airing(qb, "bi", is_airing, &datetime_to_db(now));
    }
    if filter.is_unaired == Some(true) {
        qb.push(r#" AND bi."PremiereDate" >= "#)
            .push_bind(datetime_to_db(now));
    } else if filter.is_unaired == Some(false) {
        qb.push(r#" AND bi."PremiereDate" < "#)
            .push_bind(datetime_to_db(now));
    }
}

/// Appends the exact-name and name-range predicates (`Name`, `NameContains`,
/// `NameStartsWith`, `NameStartsWithOrGreater`, `NameLessThan`).
fn append_name_predicates(
    qb: &mut QueryBuilder<Sqlite>,
    filter: &InternalItemsQuery,
    _tags: &[String],
    _exclude_tags: &[String],
) {
    if let Some(name) = non_blank(filter.name.as_ref()) {
        if filter.use_raw_name == Some(true) {
            qb.push(r#" AND ferrofin_lower_invariant(bi."Name") = "#)
                .push_bind(lower_invariant(name));
        } else {
            qb.push(r#" AND bi."CleanName" = "#)
                .push_bind(get_clean_value(name));
        }
    }

    // Batch form of the exact-name match: `CleanName IN (…)`. Used to resolve a
    // whole page of by-name items (people, years) in one query instead of N.
    let clean_names: Vec<String> = filter
        .names
        .iter()
        .filter_map(|n| non_blank(Some(n)))
        .map(get_clean_value)
        .collect();
    if !clean_names.is_empty() {
        qb.push(r#" AND bi."CleanName" IN ("#);
        let mut sep = qb.separated(", ");
        for clean in &clean_names {
            sep.push_bind(clean.clone());
        }
        qb.push(")");
    }

    if let Some(contains) = non_blank(filter.name_contains.as_ref()) {
        let clean = format!("%{}%", get_clean_value(contains).trim_matches('%'));
        qb.push(r#" AND (bi."CleanName" LIKE "#)
            .push_bind(clean.clone())
            .push(r#" OR ferrofin_lower_invariant(bi."OriginalTitle") LIKE "#)
            .push_bind(format!("%{}%", lower_invariant(contains).trim_matches('%')))
            .push(")");
    }

    if filter.collapse_box_set_items != Some(true) {
        append_name_ranges(qb, filter);
    }
}

/// The three letter-range filters run on the substituted BoxSet row in a
/// recursive collapsed query (pinned ApplyGroupingFilter/ApplyNameFilters).
fn append_name_ranges(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    // Pinned EF translates row.ToLower to SQLite lower; the bound uses the
    // model's ToLowerInvariant. StartsWith is literal, including '%' and '_'.
    if let Some(prefix) = non_blank(filter.name_starts_with.as_ref()) {
        let prefix = lower_invariant(prefix);
        qb.push(r#" AND substr(lower(bi."SortName"), 1, length("#)
            .push_bind(prefix.clone())
            .push(")) = ")
            .push_bind(prefix);
    }
    if let Some(bound) = non_blank(filter.name_starts_with_or_greater.as_ref()) {
        qb.push(r#" AND lower(bi."SortName") >= "#)
            .push_bind(lower_invariant(bound));
    }
    if let Some(bound) = non_blank(filter.name_less_than.as_ref()) {
        qb.push(r#" AND lower(bi."SortName") < "#)
            .push_bind(lower_invariant(bound));
    }
}

/// Keep substitution inside SQL: collapsing precedes sorting, counting and
/// paging, and can return a collection stored outside the queried library.
fn build_box_set_query(filter: &InternalItemsQuery, shape: QueryShape) -> QueryBuilder<Sqlite> {
    let mut qb = QueryBuilder::new(
        r#"WITH candidate_ids AS MATERIALIZED (
        SELECT bi."Id" FROM "BaseItems" bi WHERE "#,
    );
    if group_by_presentation_unique_key(filter) {
        push_group_head(&mut qb);
        append_predicates(&mut qb, filter);
        push_group_tail(&mut qb);
    } else {
        qb.push(r#"bi."Id" <> "#).push_bind(PLACEHOLDER_ID);
        append_predicates(&mut qb, filter);
    }
    qb.push(
        r#"), collapsible_ids AS MATERIALIZED (
        SELECT bi."Id" FROM "BaseItems" bi JOIN candidate_ids c ON c."Id" = bi."Id" WHERE "#,
    );
    let all_types = filter.collapse_box_set_item_types.is_empty();
    if all_types {
        qb.push("1 = 1");
    } else {
        let types: Vec<_> = filter
            .collapse_box_set_item_types
            .iter()
            .filter_map(|kind| stored_type_name(*kind))
            .collect();
        if types.is_empty() {
            qb.push("0 = 1");
        } else {
            qb.push(r#"bi."Type" IN ("#);
            let mut separated = qb.separated(", ");
            for kind in types {
                separated.push_bind(kind);
            }
            qb.push(")");
        }
    }
    qb.push(
        r#"), box_set_links AS MATERIALIZED (
        SELECT DISTINCT lc."ChildId", lc."ParentId" FROM "LinkedChildren" lc
        JOIN "BaseItems" parent ON parent."Id" = lc."ParentId"
        JOIN candidate_ids c ON c."Id" = lc."ChildId"
        WHERE lc."ChildType" = 0 AND parent."Type" = "#,
    )
    .push_bind(stored_type_name(BaseItemKind::BoxSet).expect("box set type"));
    qb.push(
        r#"), collapsed_ids AS (
        SELECT bi."Id" FROM "BaseItems" bi JOIN candidate_ids c ON c."Id" = bi."Id" WHERE "#,
    );
    if !all_types {
        qb.push(r#"bi."Id" NOT IN (SELECT "Id" FROM collapsible_ids) OR ("#);
    }
    qb.push(r#"bi."Type" <> "#)
        .push_bind(stored_type_name(BaseItemKind::BoxSet).expect("box set type"))
        .push(r#" AND bi."Id" NOT IN (SELECT "ChildId" FROM box_set_links)"#);
    if !all_types {
        qb.push(")");
    }
    qb.push(
        r#" UNION SELECT links."ParentId" FROM box_set_links links
        JOIN collapsible_ids c ON c."Id" = links."ChildId") "#,
    );
    qb.push(match shape {
        QueryShape::FullRows => r#"SELECT bi.* FROM "BaseItems" bi WHERE "#,
        QueryShape::IdsOnly => r#"SELECT bi."Id" FROM "BaseItems" bi WHERE "#,
        QueryShape::IdAndCleanName => {
            r#"SELECT bi."Id", bi."CleanName" FROM "BaseItems" bi WHERE "#
        }
        QueryShape::GroupedCount => r#"SELECT COUNT(*) FROM "BaseItems" bi WHERE "#,
        QueryShape::Count | QueryShape::TypeCounts => unreachable!("raw count shapes do not group"),
    });
    qb.push(r#"bi."Id" IN (SELECT "Id" FROM collapsed_ids)"#);
    append_name_ranges(&mut qb, filter);
    if shape != QueryShape::GroupedCount {
        append_order_by(&mut qb, filter);
        append_paging(&mut qb, filter);
    }
    qb
}

/// Appends the media-attribute predicates: subtitle presence, owned-extra
/// presence (trailer / theme song / theme video / special feature), video
/// types, and 3D.
///
/// Ports of C# `TranslateQuery`'s `HasSubtitles`, the `ExtraIds`-backed extra
/// filters, and the `Data`-substring `VideoType` / `Video3DFormat` matches.
fn append_media_attribute_predicates(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    // Subtitle presence (v12 `TranslateQuery.cs:897-918`, criteria
    // `HasSubtitles`): an item matches on its own subtitle streams or an
    // alternate version's, a folder on a matching item anywhere below it.
    // Upstream's negated form is the exact complement of the positive one
    // (`!IsFolder && !own && !versions || IsFolder && !folders`), and every
    // column and id the roll-up reads is NOT NULL, so `NOT (…)` is that
    // complement in SQL too.
    if let Some(want) = filter.has_subtitles {
        qb.push(if want { " AND " } else { " AND NOT " });
        push_item_version_or_folder_matching(qb, &|qb, alias| {
            qb.push(format!(
                r#"{alias}."StreamType" = {}"#,
                i32::from(MediaStreamTypeEntity::Subtitle)
            ));
        });
    }

    // Image presence: an `EXISTS` over the item's image rows whose `ImageType`
    // discriminant is in the requested set (C# `BaseItemRepository.TranslateQuery`:
    // `e.Images!.Any(w => imgTypes.Contains(w.ImageType))`). The dynamic image
    // providers sample their collage sources with `ImageTypes = [Primary]`.
    if !filter.image_types.is_empty() {
        let discs: Vec<i64> = filter
            .image_types
            .iter()
            .map(|t| i64::from(crate::item_repository::image_type_to_disc(*t)))
            .collect();
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "BaseItemImageInfos" ii
                WHERE ii."ItemId" = bi."Id" AND "#,
        );
        push_in_list(qb, r#"ii."ImageType""#, &discs);
        qb.push(")");
    }

    // Owned extras: `ExtraType` discriminants match `extra_type_from_disc`
    // (2 = Trailer, 8 = ThemeSong, 9 = ThemeVideo).
    let mut extra_exists = |want: bool, cond: &str| {
        qb.push(if want {
            " AND EXISTS "
        } else {
            " AND NOT EXISTS "
        })
        .push(format!(
            r#"(SELECT 1 FROM "BaseItems" x
                WHERE x."OwnerId" = bi."Id" AND {cond})"#
        ));
    };
    if let Some(want) = filter.has_trailer {
        extra_exists(want, r#"x."ExtraType" = 2"#);
    }
    if let Some(want) = filter.has_theme_song {
        extra_exists(want, r#"x."ExtraType" = 8"#);
    }
    if let Some(want) = filter.has_theme_video {
        extra_exists(want, r#"x."ExtraType" = 9"#);
    }
    // A "special feature" is any owned extra that is not unknown/trailer/theme
    // (C# `BaseItem.DisplayExtraTypes` complement used by `HasSpecialFeature`).
    if let Some(want) = filter.has_special_feature {
        extra_exists(
            want,
            r#"x."ExtraType" IS NOT NULL AND x."ExtraType" NOT IN (0, 2, 8, 9)"#,
        );
    }

    // Video types / 3D live inside the serialized `Data` blob, matched by
    // substring exactly as C# does (`"VideoType":"BluRay"` / `Video3DFormat`).
    if !filter.video_types.is_empty() {
        qb.push(" AND (");
        for (i, vt) in filter.video_types.iter().enumerate() {
            if i > 0 {
                qb.push(" OR ");
            }
            let name = vt.json_name();
            qb.push(r#"bi."Data" LIKE "#)
                .push_bind(format!("%\"VideoType\":\"{name}\"%"))
                .push(r#" OR bi."Data" LIKE "#)
                .push_bind(format!("%\"IsoType\":\"{name}\"%"));
        }
        qb.push(")");
    }
    if let Some(want) = filter.is_3d {
        if want {
            qb.push(r#" AND bi."Data" LIKE '%Video3DFormat%'"#);
        } else {
            qb.push(r#" AND (bi."Data" IS NULL OR bi."Data" NOT LIKE '%Video3DFormat%')"#);
        }
    }
}

/// The language code standing for "undetermined": a list carrying it also
/// matches a stream with no language at all (v12 DescendantQueryHelper.cs:285,
/// `// und = undetermined`). Compared ordinally, so only the lower-case code
/// turns the empty-language arm on.
const UNDETERMINED_LANGUAGE: &str = "und";

/// Appends 12.1's stream-language filters — `SubtitleLanguages`, then
/// `AudioLanguages` — each as its own `AND`-ed predicate (v12
/// `BaseItemRepository.TranslateQuery.cs:920-944`): an item matches when one
/// of its own streams of that type, or one of an alternate version's, is in
/// any listed language; a folder matches when such an item sits anywhere
/// below it (see [`push_item_version_or_folder_matching`]).
///
/// The lists are matched exactly as stored — no `NormalizeLanguage` (unlike
/// the single-language `HasNo*TrackWithLanguage` filters) and no case folding,
/// so the `StreamType`/`Language` index serves the `IN`. Empty lists push
/// nothing: the statement is byte-identical to one without the filters.
fn append_stream_language_predicates(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    for (languages, stream_type) in [
        (&filter.subtitle_languages, MediaStreamTypeEntity::Subtitle),
        (&filter.audio_languages, MediaStreamTypeEntity::Audio),
    ] {
        if languages.is_empty() {
            continue;
        }
        qb.push(" AND ");
        push_item_version_or_folder_matching(qb, &|qb, alias| {
            push_stream_match(qb, alias, stream_type, languages);
        });
    }
}

/// Pushes the stream rows a v12 `HasMediaStreamType(type, languages)` criteria
/// selects, over the `MediaStreamInfos` row aliased `alias` — the shared
/// definition at v12 `DescendantQueryHelper.cs:280-294`:
/// `StreamType == type && (languages.Contains(Language) ||
/// (languages.Contains("und") && string.IsNullOrEmpty(Language)))`.
///
/// Internal and external streams both count (`IsExternal = null`).
fn push_stream_match(
    qb: &mut QueryBuilder<Sqlite>,
    alias: &str,
    stream_type: MediaStreamTypeEntity,
    languages: &[String],
) {
    qb.push(format!(
        r#"{alias}."StreamType" = {} AND ("#,
        i32::from(stream_type)
    ));
    push_in_list(qb, &format!(r#"{alias}."Language""#), languages);
    if languages.iter().any(|l| l == UNDETERMINED_LANGUAGE) {
        qb.push(format!(
            r#" OR {alias}."Language" IS NULL OR {alias}."Language" = ''"#
        ));
    }
    qb.push(")");
}

/// Pushes v12's item-or-version-or-folder roll-up for a media criteria
/// (`BaseItemRepository.TranslateQuery.cs:920-944`):
///
/// ```text
/// (!IsFolder && (own media matches || Id in PrimaryVersionIdsMatching))
///   || (IsFolder && Id in FolderIdsMatching)
/// ```
///
/// `matches` pushes the criteria over a `MediaStreamInfos` alias. The
/// non-folder arm reads the item's own streams, then the primaries whose
/// alternate version matches ([`push_primary_versions_matching`]). The
/// folder arm is `DescendantQueryHelper.GetFolderIdsMatching`
/// (`DescendantQueryHelper.cs:177-217`): every closure ancestor of a matching
/// item or of a primary reported for it, every container that links to one of
/// those at any depth (`ResolveLinkParents`, `:323-374`, whose frontier loop is
/// the recursive `link_parents` CTE — `UNION` stops a cyclic link on the
/// resolved set exactly as the loop does), those containers' ancestors, and
/// one more closure hop over both chains, which reaches the `UserRootFolder`.
/// Alternate-version links (`LocalAlternateVersion` / `LinkedAlternateVersion`)
/// are second files, not children, so they are never walked.
///
/// Both sub-selects are uncorrelated, so SQLite evaluates each once per
/// statement, and only when a row reaches its arm. `hierarchy` is made
/// `DISTINCT` (a set either way): every matching item contributes each of its
/// ancestors, and the link walk and the last closure hop then seek once per
/// folder rather than once per item — on the 20k-item bench library,
/// `audioLanguages=und` resolves its folders in 32 ms instead of 75.
/// `HasSubtitles` takes the same shape; so will the other v12 media criteria
/// (the `HasNo*TrackWithLanguage` filters, `HasChapterImages`), whose
/// predicates `TranslateQuery` does not translate yet.
///
/// `+`: the same plan pin as [`push_is_played`] on both `IsFolder` disjuncts,
/// so a browse of three or more kinds keeps its `(Type, TopParentId)` seek
/// instead of a `MULTI-INDEX OR` over every row of every library in scope.
fn push_item_version_or_folder_matching(
    qb: &mut QueryBuilder<Sqlite>,
    matches: &dyn Fn(&mut QueryBuilder<Sqlite>, &str),
) {
    let container_link = format!(
        r#"lc."ChildType" NOT IN ({}, {})"#,
        i32::from(LinkedChildType::LocalAlternateVersion),
        i32::from(LinkedChildType::LinkedAlternateVersion)
    );
    qb.push(
        r#"((+bi."IsFolder" = 0 AND (EXISTS (SELECT 1 FROM "MediaStreamInfos" ms WHERE ms."ItemId" = bi."Id" AND "#,
    );
    matches(qb, "ms");
    qb.push(r#") OR bi."Id" IN ("#);
    push_primary_versions_matching(qb, matches);
    qb.push(
        r#"))) OR (+bi."IsFolder" = 1 AND bi."Id" IN (WITH RECURSIVE matched("Id") AS (SELECT ms."ItemId" FROM "MediaStreamInfos" ms WHERE "#,
    );
    matches(qb, "ms");
    qb.push(" UNION ");
    push_primary_versions_matching(qb, matches);
    qb.push(format!(
        r#"), hierarchy("Id") AS (
            SELECT DISTINCT a."ParentItemId" FROM "AncestorIds" a WHERE a."ItemId" IN (SELECT "Id" FROM matched)
        ), link_parents("Id") AS (
            SELECT lc."ParentId" FROM "LinkedChildren" lc
            WHERE {container_link} AND (lc."ChildId" IN (SELECT "Id" FROM matched) OR lc."ChildId" IN (SELECT "Id" FROM hierarchy))
            UNION
            SELECT lc."ParentId" FROM link_parents f
            JOIN "LinkedChildren" lc ON lc."ChildId" = f."Id" WHERE {container_link}
            UNION
            SELECT lc."ParentId" FROM link_parents f
            JOIN "AncestorIds" a ON a."ItemId" = f."Id"
            JOIN "LinkedChildren" lc ON lc."ChildId" = a."ParentItemId" WHERE {container_link}
        ), link_ancestors("Id") AS (
            SELECT a."ParentItemId" FROM "AncestorIds" a WHERE a."ItemId" IN (SELECT "Id" FROM link_parents)
        ) SELECT "Id" FROM hierarchy
        UNION SELECT "Id" FROM link_parents
        UNION SELECT "Id" FROM link_ancestors
        UNION SELECT a."ParentItemId" FROM "AncestorIds" a
            WHERE a."ItemId" IN (SELECT "Id" FROM hierarchy) OR a."ItemId" IN (SELECT "Id" FROM link_ancestors))))"#
    ));
}

/// Pushes v12 `DescendantQueryHelper.GetPrimaryVersionIdsMatching`
/// (`DescendantQueryHelper.cs:231-253`): the `PrimaryVersionId` of every
/// alternate version whose own streams match. Anchored on the alternates, so
/// the partial `IX_BaseItems_PrimaryVersionId` keeps it to the few items that
/// are second files, each costing one seek into the stream index.
fn push_primary_versions_matching(
    qb: &mut QueryBuilder<Sqlite>,
    matches: &dyn Fn(&mut QueryBuilder<Sqlite>, &str),
) {
    qb.push(
        r#"SELECT v."PrimaryVersionId" FROM "BaseItems" v WHERE v."PrimaryVersionId" IS NOT NULL AND EXISTS (SELECT 1 FROM "MediaStreamInfos" ms WHERE ms."ItemId" = v."Id" AND "#,
    );
    matches(qb, "ms");
    qb.push(")");
}

/// Appends the `UserData`-backed predicates (favorite / favorite-or-liked / liked
/// / played / resumable) scoped to the query's user — the C# `TranslateQuery`
/// blocks at `BaseItemRepository.TranslateQuery.cs:514-603` (v12).
///
/// `IsPlayed` is [`push_is_played`]: a leaf's own state, a folder's roll-up.
fn append_user_data_predicates(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    let Some(user_id) = filter.user_id() else {
        return;
    };
    let uid = guid_to_db(user_id);

    if let Some(want) = filter.is_favorite {
        push_user_data_exists(qb, &uid, r#"ud."IsFavorite" = 1"#, want);
    }
    if let Some(want) = filter.is_favorite_or_liked {
        push_user_data_exists(qb, &uid, r#"ud."IsFavorite" = 1"#, want);
    }
    if let Some(want) = filter.is_liked {
        // `UserItemData.MinLikeValue` is 6.5 on the 0-10 rating scale, not 7 — a
        // 6.5 rating is "liked" upstream.
        push_user_data_exists(qb, &uid, r#"ud."Rating" >= 6.5"#, want);
    }
    if let Some(want) = filter.is_played {
        qb.push(if want { " AND " } else { " AND NOT " });
        push_is_played(qb, "bi", &uid, filter);
    }
    if let Some(want) = filter.is_resumable {
        push_resumable_predicate(qb, &uid, want, filter);
    }
}

/// v12.2 `BuildIsPlayedFilter` (`TranslateQuery.cs:38-48`) on the row
/// `alias`, the one predicate the `IsPlayed` filter and the
/// `IsPlayed`/`IsUnplayed` sort keys share: "Folders (Series, Seasons,
/// BoxSets, albums, ...) carry no played state of their own and count as
/// played once no descendant is left unplayed", so a folder's own `UserData`
/// row is never read; a leaf is [`push_leaf_is_played`]. A folder with no
/// accessible leaf at all is played. Never NULL, so `NOT` inverts it.
///
/// `+`: a plan pin on both `IsFolder` terms. Unpinned, a browse of three or
/// more kinds turns the two disjuncts into a `MULTI-INDEX OR` over
/// `IX_BaseItems_TopParentId_IsFolder_…`, whose two halves are every row of
/// every library in scope, instead of its `(Type, TopParentId)` seek.
pub(crate) fn push_is_played(
    qb: &mut QueryBuilder<Sqlite>,
    alias: &str,
    user_id: &str,
    filter: &InternalItemsQuery,
) {
    qb.push(format!(r#"((+{alias}."IsFolder" = 1 AND NOT "#));
    push_has_unplayed_leaf(qb, alias, user_id, filter);
    qb.push(format!(r#") OR (+{alias}."IsFolder" = 0 AND "#));
    push_leaf_is_played(qb, alias, user_id);
    qb.push("))");
}

/// v12 `BuildLeafIsPlayedFilter` (`TranslateQuery.cs:50-67`) on `alias`: the
/// item has a played row of its own, OR it is the primary of a version group
/// in which any version (itself included) has one, OR it is a version whose
/// group's primary is such a primary. Alternates are hidden from general
/// queries, so this is what lets the version that was actually watched mark
/// the title played. Same uncorrelated-`IN` shape as
/// [`push_user_data_exists`]: each list is materialized once per statement.
fn push_leaf_is_played(qb: &mut QueryBuilder<Sqlite>, alias: &str, user_id: &str) {
    qb.push(format!(
        r#"({alias}."Id" IN (SELECT ud."ItemId" FROM "UserData" ud WHERE ud."UserId" = "#
    ))
    .push_bind(user_id.to_owned())
    .push(format!(r#" AND ud."Played" = 1) OR {alias}."Id" IN "#));
    push_played_groups(qb, user_id);
    qb.push(format!(
        r#" OR ({alias}."PrimaryVersionId" IS NOT NULL AND {alias}."PrimaryVersionId" IN "#
    ));
    push_played_groups(qb, user_id);
    qb.push("))");
}

/// `playedGroupIds` of v12 `BuildLeafIsPlayedFilter`: the primaries of every
/// version group holding a played row of the user's, whichever version
/// carries it.
fn push_played_groups(qb: &mut QueryBuilder<Sqlite>, user_id: &str) {
    qb.push(r#"(SELECT v."PrimaryVersionId" FROM "BaseItems" v WHERE v."PrimaryVersionId" IS NOT NULL AND EXISTS (SELECT 1 FROM "UserData" ud WHERE ud."UserId" = "#)
        .push_bind(user_id.to_owned())
        .push(r#" AND ud."Played" = 1 AND (ud."ItemId" = v."Id" OR ud."ItemId" = v."PrimaryVersionId")))"#);
}

/// The ids [`push_leaf_is_played`] holds for, as one uncorrelated list: the
/// user's own played rows, the played groups' primaries, and every version of
/// those primaries.
fn push_played_leaf_ids(qb: &mut QueryBuilder<Sqlite>, user_id: &str) {
    qb.push(r#"(SELECT ud."ItemId" FROM "UserData" ud WHERE ud."UserId" = "#)
        .push_bind(user_id.to_owned())
        .push(r#" AND ud."Played" = 1 UNION SELECT g."PrimaryVersionId" FROM "#);
    push_played_groups(qb, user_id);
    qb.push(r#" g UNION SELECT v2."Id" FROM "BaseItems" v2 WHERE v2."PrimaryVersionId" IN "#);
    push_played_groups(qb, user_id);
    qb.push(")");
}

/// v12.2 `BuildHasDescendantFilter` (`QueryBuilding.cs:707-721`) over the
/// unplayed leaves of `BuildIsPlayedFilter`: `alias` has an
/// [`ACCESSIBLE_LEAF`] that passes the parental restrictions and is not
/// [`push_leaf_is_played`] in its `AncestorIds` closure, among its
/// `LinkedChildren` (a box set's or playlist's members), or in the closure of
/// a linked child (a series in a collection).
///
/// Correlated per folder, unlike [`push_folders_with_leaf`]: the unplayed
/// leaves have no `UserData` row to drive from. Each arm seeks the folder's
/// own rows (`IX_AncestorIds_ParentItemId`, the `LinkedChildren` key) and
/// stops at the first unplayed leaf; `CROSS JOIN` keeps the leaf a primary-key
/// probe, so the planner never starts from every leaf in the library (the
/// pin [`push_folder_is_resumable`] measures).
fn push_has_unplayed_leaf(
    qb: &mut QueryBuilder<Sqlite>,
    alias: &str,
    user_id: &str,
    filter: &InternalItemsQuery,
) {
    let unplayed_leaf = |qb: &mut QueryBuilder<Sqlite>| {
        qb.push(ACCESSIBLE_LEAF);
        append_parental_restrictions(qb, filter, "l");
        qb.push(" AND NOT ");
        push_leaf_is_played(qb, "l", user_id);
    };
    qb.push(format!(
        r#"(EXISTS (SELECT 1 FROM "AncestorIds" a
            CROSS JOIN "BaseItems" l ON l."Id" = a."ItemId"
            WHERE a."ParentItemId" = {alias}."Id" AND "#
    ));
    unplayed_leaf(qb);
    qb.push(format!(
        r#") OR EXISTS (SELECT 1 FROM "LinkedChildren" lc
            CROSS JOIN "BaseItems" l ON l."Id" = lc."ChildId"
            WHERE lc."ParentId" = {alias}."Id" AND "#
    ));
    unplayed_leaf(qb);
    qb.push(format!(
        r#") OR EXISTS (SELECT 1 FROM "LinkedChildren" lc
            CROSS JOIN "AncestorIds" a ON a."ParentItemId" = lc."ChildId"
            CROSS JOIN "BaseItems" l ON l."Id" = a."ItemId"
            WHERE lc."ParentId" = {alias}."Id" AND "#
    ));
    unplayed_leaf(qb);
    qb.push("))");
}

/// The in-progress `UserData` rows of the user: `PlaybackPositionTicks > 0`
/// (v12 `TranslateQuery.cs:558-559`, `inProgress`). Emitted as an
/// uncorrelated `SELECT` so SQLite materializes the user's list once and
/// drives the outer query through the `BaseItems` primary key — the IN form
/// [`push_user_data_exists`] measures at 105× the correlated `EXISTS`.
fn push_in_progress_ids(qb: &mut QueryBuilder<Sqlite>, uid: &str) {
    qb.push(r#"(SELECT ud."ItemId" FROM "UserData" ud WHERE ud."UserId" = "#)
        .push_bind(uid.to_owned())
        .push(r#" AND ud."PlaybackPositionTicks" > 0)"#);
}

/// `DescendantQueryHelper.IsCountableLeaf` (v12 `DescendantQueryHelper.cs:21-22`):
/// real leaf media — neither a folder nor a virtual (missing / unaired) item —
/// on the `l` alias.
const COUNTABLE_LEAF: &str = r#"l."IsFolder" = 0 AND l."IsVirtualItem" = 0"#;

/// [`COUNTABLE_LEAF`] as `GetAccessFilteredLeafItemsQuery(context, user)`
/// (v12.2 `QueryBuilding.cs:691-702`) returns it without `includeOwnedItems`:
/// `ApplyAccessFiltering` (`:459-483`) drops owned non-extras and, through
/// `ApplyAlternateVersionFiltering`, a version only while its primary exists
/// in the same library ([`ALTERNATE_VERSION_HIDDEN`]'s rule): a version whose
/// primary is gone is a leaf of its own. The caller appends parental
/// restrictions for the same leaf. `TopParentIds` is empty on the fresh
/// `InternalItemsQuery(user)`.
const ACCESSIBLE_LEAF: &str = r#"l."IsFolder" = 0 AND l."IsVirtualItem" = 0
    AND (l."PrimaryVersionId" IS NULL OR NOT EXISTS (SELECT 1 FROM "BaseItems" p
         WHERE p."Id" = l."PrimaryVersionId" AND p."TopParentId" IS l."TopParentId"))
    AND (l."OwnerId" IS NULL OR l."OwnerId" = '00000000-0000-0000-0000-000000000000'
         OR l."ExtraType" IS NOT NULL)"#;

/// Pushes `bi."Id" IN (<the folders with a leaf descendant in `leaf_set`>)`:
/// the `AncestorIds` arm of v12 `BuildHasDescendantFilter`
/// (v12.2 `QueryBuilding.cs:707-721`), driven from the user's `UserData` rows that
/// satisfy `ud_cond` rather than correlated per folder — the user's played /
/// in-progress rows times their ancestors is a few hundred to a few thousand
/// pairs, materialized once, where the per-folder walk costs every folder its
/// descendants.
///
/// The `LinkedChildren` arm (`:718-720`) is not emitted: the only kinds this
/// predicate is ever applied to are `Series` and `Season`
/// (`_resumableFolderKinds`), and neither has linked children — the arm can
/// never match for them.
fn push_folders_with_leaf(
    qb: &mut QueryBuilder<Sqlite>,
    uid: &str,
    leaf_set: &str,
    ud_cond: &str,
    filter: &InternalItemsQuery,
) {
    qb.push(format!(
        r#"bi."Id" IN (SELECT a."ParentItemId" FROM "UserData" ud
            JOIN "BaseItems" l ON l."Id" = ud."ItemId" AND {leaf_set}
            JOIN "AncestorIds" a ON a."ItemId" = ud."ItemId"
            WHERE ud."UserId" = "#
    ))
    .push_bind(uid.to_owned())
    .push(format!(" AND {ud_cond}"));
    append_parental_restrictions(qb, filter, "l");
    qb.push(")");
}

/// Pushes the v12 `folderIsResumableFilter` minus its leading `IsFolder`
/// (`TranslateQuery.cs:569-575`): the row is a `Series` or a `Season` and
/// either a leaf descendant is in progress, or it has both a played and an
/// unplayed leaf descendant (partially watched). No percentage threshold;
/// `Played` does not exclude a leaf from the in-progress test.
///
/// Alternate versions keep their own progress, so they count towards the
/// in-progress check ([`COUNTABLE_LEAF`], `includeOwnedItems: true`); towards
/// the played/unplayed one ([`ACCESSIBLE_LEAF`]) only a version whose primary
/// is not in its library does.
fn push_folder_is_resumable(qb: &mut QueryBuilder<Sqlite>, uid: &str, filter: &InternalItemsQuery) {
    // `_resumableFolderKinds` (v12 `BaseItemRepository.cs:66-72`): "the only
    // folder kinds whose children form a single viewing sequence, so playback
    // progress on a child rolls up to them".
    let kinds: Vec<String> = [BaseItemKind::Series, BaseItemKind::Season]
        .into_iter()
        .filter_map(|kind| stored_type_name(kind).map(str::to_owned))
        .collect();
    qb.push("(");
    push_in_list(qb, r#"bi."Type""#, &kinds);
    qb.push(" AND (");
    push_folders_with_leaf(
        qb,
        uid,
        COUNTABLE_LEAF,
        r#"ud."PlaybackPositionTicks" > 0"#,
        filter,
    );
    qb.push(" OR (");
    // The played arm (v12.2 `TranslateQuery.cs:574-577`,
    // `BuildLeafIsPlayedFilter`): a leaf counts as played across its version
    // group, so a play recorded on a hidden alternate reaches the primary.
    // `CROSS JOIN` pins the drive from the materialized played-leaf list
    // through the leaf's primary key, then its closure; as an `IN` test the
    // planner walks every leaf in the library (`IsFolder=?`) instead.
    qb.push(r#"bi."Id" IN (SELECT a."ParentItemId" FROM "#);
    push_played_leaf_ids(qb, uid);
    qb.push(
        r#" pl CROSS JOIN "BaseItems" l ON l."Id" = pl."ItemId"
            CROSS JOIN "AncestorIds" a ON a."ItemId" = l."Id"
            WHERE "#,
    )
    .push(ACCESSIBLE_LEAF);
    append_parental_restrictions(qb, filter, "l");
    qb.push(")");
    // "Has an unplayed leaf descendant" is the one arm that cannot be driven
    // from the user's rows (the leaf has none), so it walks the folder's
    // descendants — evaluated only for the folders the played arm let through,
    // and stopping at the first unplayed leaf. `CROSS JOIN` pins the walk to
    // `IX_AncestorIds_ParentItemId` → the leaf's primary key: left to itself
    // the planner starts from every leaf in the library
    // (`IX_BaseItems_IsFolder_…`, `IsFolder=?`) and probes the closure per
    // leaf per folder — 1.3 s for the unfiltered Resume count on the bench
    // library, 2 ms pinned.
    qb.push(
        r#" AND EXISTS (SELECT 1 FROM "AncestorIds" a
                CROSS JOIN "BaseItems" l ON l."Id" = a."ItemId"
                WHERE a."ParentItemId" = bi."Id" AND "#,
    )
    .push(ACCESSIBLE_LEAF);
    append_parental_restrictions(qb, filter, "l");
    qb.push(" AND NOT ");
    push_leaf_is_played(qb, "l", uid);
    qb.push("))))");
}

/// Appends the v12 `IsResumable` predicate
/// (`BaseItemRepository.TranslateQuery.cs:552-603`).
///
/// A non-folder is resumable when it has an in-progress `UserData` row — per
/// version: a resume query surfaces the version that was actually played,
/// which may be an alternate, so each version is matched on its own progress
/// rather than coalesced onto the primary. A `Series` or `Season` is resumable
/// under [`push_folder_is_resumable`]. This is why v12 counts a partially
/// watched show's seasons and the show itself among the resumable items, where
/// 10.11.8 counted leaves only.
///
/// `want == true` additionally keeps, of several in-progress versions of one
/// item, only the most recently played (`:586-602`) — id as the tiebreaker;
/// `want == false` operates on primaries only (`:604-613`).
///
/// `+`: the same plan pin as [`push_is_played`] on the two `IsFolder`
/// disjuncts of each `want` arm, so a browse of three or more kinds keeps its
/// `(Type, TopParentId)` seek instead of a `MULTI-INDEX OR` over every row of
/// every library in scope. The `want == true` dedupe term needs none: its
/// other disjuncts are not indexable, so it can never become an index OR.
fn push_resumable_predicate(
    qb: &mut QueryBuilder<Sqlite>,
    uid: &str,
    want: bool,
    filter: &InternalItemsQuery,
) {
    if want {
        qb.push(r#" AND ((+bi."IsFolder" = 0 AND bi."Id" IN "#);
        push_in_progress_ids(qb, uid);
        qb.push(r#") OR (+bi."IsFolder" = 1 AND "#);
        push_folder_is_resumable(qb, uid, filter);
        qb.push("))");
        // "When several versions of the same item are in progress, keep only
        // the most recently played one, use id as tiebreaker. Only in-progress
        // siblings can eliminate a candidate: a version without progress has
        // a NULL max LastPlayedDate, which is never greater and never ties.
        // Restricting the sibling scan to the in-progress set keeps this
        // bounded by the user's Continue Watching count instead of forcing a
        // full BaseItems scan (COALESCE keys are non-indexable) per row. Items
        // in no version group at all have no sibling that could eliminate
        // them, so short-circuit the scan for those." (`:586-602`)
        //
        // The short-circuit is `IX_BaseItems_PrimaryVersionId`'s one
        // job — partial on `PrimaryVersionId IS NOT NULL`, exactly as v12's
        // (`BaseItemConfiguration.cs:64-68`), so it can serve nothing else.
        // `IS` is EF's `==` on two nullable dates (both NULL ties); `>` is
        // plain (NULL never greater). `s."Id" < bi."Id"` is `Guid.CompareTo`:
        // .NET compares the fields as unsigned in declaration order, which is
        // the text order of the uppercase hyphenated form stored here.
        qb.push(r#" AND (bi."IsFolder" = 1"#)
            .push(r#" OR (bi."PrimaryVersionId" IS NULL AND NOT EXISTS (SELECT 1 FROM "BaseItems" x WHERE x."PrimaryVersionId" = bi."Id"))"#)
            .push(r#" OR NOT EXISTS (SELECT 1 FROM "BaseItems" s WHERE s."Id" <> bi."Id" AND s."Id" IN "#);
        push_in_progress_ids(qb, uid);
        qb.push(r#" AND COALESCE(s."PrimaryVersionId", s."Id") = COALESCE(bi."PrimaryVersionId", bi."Id")"#);
        // `inProgress.Where(u => u.ItemId == <alias>.Id).Max(u => u.LastPlayedDate)`.
        let push_max_played = |qb: &mut QueryBuilder<Sqlite>, alias: &str| {
            qb.push(format!(
                r#"(SELECT MAX(pu."LastPlayedDate") FROM "UserData" pu
                    WHERE pu."ItemId" = {alias}."Id" AND pu."UserId" = "#
            ))
            .push_bind(uid.to_owned())
            .push(r#" AND pu."PlaybackPositionTicks" > 0)"#);
        };
        qb.push(" AND (");
        push_max_played(qb, "s");
        qb.push(" > ");
        push_max_played(qb, "bi");
        qb.push(" OR (");
        push_max_played(qb, "s");
        qb.push(" IS ");
        push_max_played(qb, "bi");
        // Closes the tie arm, the date test, the sibling scan and the whole
        // dedupe term.
        qb.push(r#" AND s."Id" < bi."Id"))))"#);
    } else {
        // Not-resumable queries operate on primaries only: the id set is the
        // in-progress versions' primaries (`:606-608`), and v12 has already
        // dropped every alternate from the query (`:796-807`). Ferrofin's
        // grouped browse keeps alternates for its collapse (see
        // `append_predicates`), so the row's own primary is what is tested —
        // an in-progress alternate must not resurface as its group's last
        // representative.
        qb.push(r#" AND ((+bi."IsFolder" = 1 AND NOT "#);
        push_folder_is_resumable(qb, uid, filter);
        qb.push(
            r#") OR (+bi."IsFolder" = 0 AND COALESCE(bi."PrimaryVersionId", bi."Id") NOT IN (SELECT COALESCE(x."PrimaryVersionId", x."Id") FROM "UserData" ud JOIN "BaseItems" x ON x."Id" = ud."ItemId" WHERE ud."UserId" = "#,
        )
        .push_bind(uid.to_owned())
        .push(r#" AND ud."PlaybackPositionTicks" > 0)))"#);
    }
}

/// Appends `AND [NOT] EXISTS (SELECT 1 FROM UserData ud WHERE ud.ItemId = bi.Id
/// AND ud.UserId = <uid> AND <cond>)`.
pub(crate) fn push_user_data_exists(
    qb: &mut QueryBuilder<Sqlite>,
    user_id: &str,
    cond: &str,
    want: bool,
) {
    // `bi."Id" [NOT] IN (<uncorrelated subquery over the user's rows>)`, NOT a
    // correlated `EXISTS`: the IN form lets SQLite materialize the tiny
    // per-user list once and drive the outer query through the BaseItems PK,
    // where the EXISTS could not drive the plan at all — every user-data
    // filter forced a full `SCAN bi` with per-row probes (3.9 ms/request on
    // the 9.8k-item bench DB regardless of result size), which is what
    // collapsed /UserItems/Resume to a 22 s p50 at its calibrated 464 req/s.
    // Measured there: 1.364 ms → 0.013 ms (105×); plan `SCAN bi` →
    // `SEARCH bi (Id=?)`. NOT IN is NULL-safe here because UserData.ItemId
    // is NOT NULL (primary-key component in the pinned Jellyfin schema).
    qb.push(if want {
        r#" AND bi."Id" IN "#
    } else {
        r#" AND bi."Id" NOT IN "#
    })
    .push(r#"(SELECT ud."ItemId" FROM "UserData" ud WHERE ud."UserId" = "#)
    .push_bind(user_id.to_owned())
    .push(format!(" AND {cond})"));
}

/// Appends the credited-person predicates: `person` (by name) and `person_ids`
/// (a person's filmography). Each is an `EXISTS` over the `PeopleBaseItemMap`
/// join, so `/Items?PersonIds=<id>` returns everything that person is credited
/// on (port of C# `WhereContainsPerson` / the `people` sub-query). A non-empty
/// `person_types` restricts the matching credit to those types in both arms
/// (`personTypes.Length == 0 || personTypes.Contains(…PersonType)`).
fn append_people_predicates(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    let push_person_types = |qb: &mut QueryBuilder<Sqlite>| {
        if !filter.person_types.is_empty() {
            qb.push(" AND ");
            push_in_list(qb, r#"pp."PersonType""#, &filter.person_types);
        }
    };
    if let Some(name) = non_blank(filter.person.as_ref()) {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" pm
                JOIN "Peoples" pp ON pp."Id" = pm."PeopleId"
                WHERE pm."ItemId" = bi."Id" AND pp."Name" = "#,
        );
        qb.push_bind(name.to_owned());
        push_person_types(qb);
        qb.push(")");
    }
    if !filter.person_ids.is_empty() {
        // `PersonIds` carries browsable `Person` *item* ids (deterministic,
        // per-name), but the map's `PeopleId` is the per-(name,type) `Peoples`
        // row id — a different value. So match a credit either directly (the id
        // *is* a `PeopleId`, e.g. grains that coincide) OR by bridging the
        // requested item id → its `Person` name → the credited `Peoples.Name`.
        // Without the name bridge a person's filmography comes back empty.
        let ids = to_guid_strings(&filter.person_ids);
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" pm
                JOIN "Peoples" pp ON pp."Id" = pm."PeopleId"
                WHERE pm."ItemId" = bi."Id" AND ("#,
        );
        push_in_list(qb, r#"pm."PeopleId""#, &ids);
        qb.push(r#" OR pp."Name" IN (SELECT "Name" FROM "BaseItems" WHERE "#);
        push_in_list(qb, r#""Id""#, &ids);
        qb.push("))");
        push_person_types(qb);
        qb.push(")");
    }
}

/// Appends the `ItemValues`-backed predicates: genres, genre ids, studios, studio
/// ids, artist ids, album-artist ids, contributing-artist ids, tags, and
/// exclude-tags.
///
/// Each is an `EXISTS`/`NOT EXISTS` over `ItemValuesMap ⨝ ItemValues` filtered by
/// the value `Type` discriminant (mirrors the C# `e.ItemValues!.Any(…)`).
fn append_item_value_predicates(
    qb: &mut QueryBuilder<Sqlite>,
    filter: &InternalItemsQuery,
    tags: &[String],
    exclude_tags: &[String],
) {
    if !filter.genres.is_empty() {
        let cleans: Vec<String> = filter.genres.iter().map(|g| get_clean_value(g)).collect();
        push_item_value_exists(qb, &[ItemValueType::Genre], &cleans, true);
    }
    if !filter.genre_ids.is_empty() {
        push_referenced_item(qb, &[ItemValueType::Genre], &filter.genre_ids, false);
    }
    if !filter.studio_ids.is_empty() {
        push_referenced_item(qb, &[ItemValueType::Studios], &filter.studio_ids, false);
    }
    if !filter.artist_ids.is_empty() {
        push_referenced_item(
            qb,
            &[ItemValueType::Artist, ItemValueType::AlbumArtist],
            &filter.artist_ids,
            false,
        );
    }
    if !filter.album_artist_ids.is_empty() {
        push_referenced_item(
            qb,
            &[ItemValueType::AlbumArtist],
            &filter.album_artist_ids,
            false,
        );
    }
    // A contributing artist is credited on the item (`Artist`) but is not one
    // of its album artists (TranslateQuery.cs:627-640) — the artist page's
    // "Appears On" section.
    if !filter.contributing_artist_ids.is_empty() {
        push_referenced_item(
            qb,
            &[ItemValueType::Artist],
            &filter.contributing_artist_ids,
            false,
        );
        push_referenced_item(
            qb,
            &[ItemValueType::AlbumArtist],
            &filter.contributing_artist_ids,
            true,
        );
    }
    if !filter.exclude_artist_ids.is_empty() {
        push_referenced_item(
            qb,
            &[ItemValueType::Artist, ItemValueType::AlbumArtist],
            &filter.exclude_artist_ids,
            true,
        );
    }
    if !filter.album_ids.is_empty() {
        qb.push(r#" AND bi."ParentId" IS NOT NULL AND "#);
        push_in_list(qb, r#"bi."ParentId""#, &to_guid_strings(&filter.album_ids));
    }
    if !tags.is_empty() {
        let cleans: Vec<String> = tags.iter().map(|t| get_clean_value(t)).collect();
        push_item_value_exists(qb, &[ItemValueType::Tags], &cleans, true);
    }
    if !exclude_tags.is_empty() {
        let cleans: Vec<String> = exclude_tags.iter().map(|t| get_clean_value(t)).collect();
        push_item_value_exists(qb, &[ItemValueType::Tags], &cleans, false);
    }
}

/// Appends an `EXISTS`/`NOT EXISTS` over the item's `ItemValues` of the given
/// discriminant types whose `CleanValue` is in `clean_values`.
fn push_item_value_exists(
    qb: &mut QueryBuilder<Sqlite>,
    types: &[ItemValueType],
    clean_values: &[String],
    want: bool,
) {
    qb.push(if want {
        " AND EXISTS "
    } else {
        " AND NOT EXISTS "
    });
    qb.push(
        r#"(SELECT 1 FROM "ItemValuesMap" ivm JOIN "ItemValues" iv ON iv."ItemValueId" = ivm."ItemValueId" WHERE ivm."ItemId" = bi."Id" AND "#,
    );
    push_in_list(qb, r#"iv."Type""#, &item_value_type_ints(types));
    qb.push(" AND ");
    push_in_list(qb, r#"iv."CleanValue""#, clean_values);
    qb.push(")");
}

/// Appends a "referenced item" predicate: keep items whose `ItemValues` (of the
/// given types) reference the *clean names* of the items identified by `ids`.
///
/// Port of C# `WhereReferencedItem` — the by-id form resolves the referenced
/// items' `CleanName` via a sub-select rather than binding the names directly.
fn push_referenced_item(
    qb: &mut QueryBuilder<Sqlite>,
    types: &[ItemValueType],
    ids: &[Uuid],
    exclude: bool,
) {
    qb.push(if exclude {
        " AND NOT EXISTS "
    } else {
        " AND EXISTS "
    });
    qb.push(
        r#"(SELECT 1 FROM "ItemValuesMap" ivm JOIN "ItemValues" iv ON iv."ItemValueId" = ivm."ItemValueId" WHERE ivm."ItemId" = bi."Id" AND "#,
    );
    push_in_list(qb, r#"iv."Type""#, &item_value_type_ints(types));
    qb.push(r#" AND iv."CleanValue" IN (SELECT ref."CleanName" FROM "BaseItems" ref WHERE "#);
    push_in_list(qb, r#"ref."Id""#, &to_guid_strings(ids));
    qb.push("))");
}

/// Appends the `IsDead*` by-name predicate: the by-name item's `Name` no longer
/// appears as any `ItemValues` value of the given types.
fn append_is_dead_item_value(qb: &mut QueryBuilder<Sqlite>, types: &[ItemValueType]) {
    qb.push(r#" AND NOT EXISTS (SELECT 1 FROM "ItemValues" iv WHERE iv."Value" = bi."Name" AND "#);
    push_in_list(qb, r#"iv."Type""#, &item_value_type_ints(types));
    qb.push(")");
}

/// Appends provider-id presence predicates (`HasImdbId` / `HasTmdbId` /
/// `HasTvdbId`) as `EXISTS` over `BaseItemProviders`.
fn append_provider_predicates(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    for (flag, provider) in [
        (filter.has_imdb_id, "imdb"),
        (filter.has_tmdb_id, "tmdb"),
        (filter.has_tvdb_id, "tvdb"),
    ] {
        if let Some(has) = flag {
            qb.push(if has { " AND EXISTS " } else { " AND NOT EXISTS " })
                .push(
                    r#"(SELECT 1 FROM "BaseItemProviders" p WHERE p."ItemId" = bi."Id" AND lower(p."ProviderId") = "#,
                )
                .push_bind(provider.to_owned())
                .push(")");
        }
    }

    // Exact provider-id value match (`AnyProviderIdEquals`): the row qualifies if
    // it has a `BaseItemProviders` entry whose key AND value equal any requested
    // pair (case-insensitive), matching the C# `GetProviderId(..) == value` filter.
    if !filter.any_provider_id_equals.is_empty() {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "BaseItemProviders" p WHERE p."ItemId" = bi."Id" AND ("#,
        );
        let mut first = true;
        for (key, value) in &filter.any_provider_id_equals {
            if !first {
                qb.push(" OR ");
            }
            first = false;
            qb.push(r#"(lower(p."ProviderId") = "#)
                .push_bind(key.to_lowercase())
                .push(r#" AND lower(p."ProviderValue") = "#)
                .push_bind(value.to_lowercase())
                .push(")");
        }
        qb.push("))");
    }
}

/// The stored type names of the by-name kinds this query can return.
///
/// Port of `BaseItemRepository.GetItemByNameTypesInQuery` + `IsTypeInQuery`: a
/// kind counts when it is not excluded and either no include set was given or
/// it is in that set. The order is upstream's (Person, Genre, MusicGenre,
/// MusicArtist, Studio).
fn item_by_name_types_in_query(filter: &InternalItemsQuery) -> Vec<String> {
    let in_query = |kind: BaseItemKind| {
        !filter.exclude_item_types.contains(&kind)
            && (filter.include_item_types.is_empty() || filter.include_item_types.contains(&kind))
    };
    [
        BaseItemKind::Person,
        BaseItemKind::Genre,
        BaseItemKind::MusicGenre,
        BaseItemKind::MusicArtist,
        BaseItemKind::Studio,
    ]
    .into_iter()
    .filter(|kind| in_query(*kind))
    .filter_map(|kind| stored_type_name(kind).map(str::to_owned))
    .collect()
}

/// A by-name row remains visible only while related media sits in the scoped
/// user's libraries and satisfies their parental restrictions
/// (`ApplyItemByNameAccessFiltering`). Skipped when nothing is hidden from the
/// user, as upstream gates it on `UserHasContentRestrictions`.
fn append_by_name_access(
    qb: &mut QueryBuilder<Sqlite>,
    filter: &InternalItemsQuery,
    kinds: &[String],
) {
    if !filter.user_has_content_restrictions {
        return;
    }
    for (kind, values) in [
        (BaseItemKind::Person, &[][..]),
        (BaseItemKind::Genre, &[ItemValueType::Genre][..]),
        (BaseItemKind::MusicGenre, &[ItemValueType::Genre][..]),
        (BaseItemKind::Studio, &[ItemValueType::Studios][..]),
        (
            BaseItemKind::MusicArtist,
            &[ItemValueType::Artist, ItemValueType::AlbumArtist][..],
        ),
    ] {
        let Some(name) = stored_type_name(kind) else {
            continue;
        };
        if !kinds.iter().any(|value| value == name) {
            continue;
        }
        qb.push(r#" AND (bi."Type" <> "#)
            .push_bind(name.to_owned())
            .push(" OR EXISTS (");
        if kind == BaseItemKind::Person {
            qb.push(
                r#"SELECT 1 FROM "Peoples" p
                CROSS JOIN "PeopleBaseItemMap" m ON m."PeopleId" = p."Id"
                CROSS JOIN "BaseItems" related ON related."Id" = m."ItemId"
                WHERE p."Name" = bi."Name""#,
            );
        } else {
            qb.push(
                r#"SELECT 1 FROM "ItemValues" v
                CROSS JOIN "ItemValuesMap" m ON m."ItemValueId" = v."ItemValueId"
                CROSS JOIN "BaseItems" related ON related."Id" = m."ItemId"
                WHERE v."CleanValue" = bi."CleanName" AND "#,
            );
            push_in_list(
                qb,
                r#"v."Type""#,
                &values
                    .iter()
                    .map(|value| i64::from(i32::from(*value)))
                    .collect::<Vec<_>>(),
            );
        }
        qb.push(" AND ");
        push_in_list(
            qb,
            r#"+related."TopParentId""#,
            &to_guid_strings(&filter.top_parent_ids),
        );
        // IncludeOwnedItems is true upstream: credits on alternate versions count.
        append_parental_restrictions(qb, filter, "related");
        qb.push("))");
    }
}

/// Appends the ancestor / top-parent predicates.
///
/// `AncestorIds` is an `EXISTS` over the `AncestorIds` closure table; top-parent
/// is a direct `TopParentId` in-list, widened for the by-name types when the
/// caller asked for them.
fn append_ancestor_predicates(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    if !filter.top_parent_ids.is_empty() {
        // C# `BaseItemRepository.TranslateQuery`
        // (v10.11.8 `Jellyfin.Server.Implementations/Item/BaseItemRepository.cs`):
        //   if (enableItemsByName && includedItemByNameTypes.Count > 0)
        //       e => includedItemByNameTypes.Contains(e.Type)
        //            || queryTopParentIds.Any(w => w == e.TopParentId)
        // A by-name row (Genre/MusicGenre/Studio/Person/MusicArtist) has NO
        // `TopParentId` — it belongs to no library — so without this exemption
        // any user-scoped query silently drops every genre, studio and person.
        // `/Search/Hints` is exactly such a query.
        let by_name: Vec<_> = item_by_name_types_in_query(filter)
            .into_iter()
            .filter(|name| {
                filter.include_items_by_name.unwrap_or(false)
                    || filter
                        .include_item_types
                        .iter()
                        .any(|kind| stored_type_name(*kind) == Some(name.as_str()))
            })
            .collect();
        if by_name.is_empty() {
            qb.push(r#" AND bi."TopParentId" IS NOT NULL AND "#);
            push_in_list(
                qb,
                r#"bi."TopParentId""#,
                &to_guid_strings(&filter.top_parent_ids),
            );
        } else {
            qb.push(" AND (");
            push_in_list(qb, r#"bi."Type""#, &by_name);
            qb.push(r#" OR (bi."TopParentId" IS NOT NULL AND "#);
            push_in_list(
                qb,
                r#"bi."TopParentId""#,
                &to_guid_strings(&filter.top_parent_ids),
            );
            qb.push("))");
            append_by_name_access(qb, filter, &by_name);
        }
    }
    if !filter.ancestor_ids.is_empty() {
        qb.push(r#" AND EXISTS (SELECT 1 FROM "AncestorIds" a WHERE a."ItemId" = bi."Id" AND "#);
        push_in_list(
            qb,
            r#"a."ParentItemId""#,
            &to_guid_strings(&filter.ancestor_ids),
        );
        qb.push(")");
    }
    // Keep folder-like items (box sets, playlists) whose manual linked
    // children descend from any of the requested ancestors — the Collections
    // tab's re-rooted query (C# TranslateQuery `LinkedChildAncestorIds` over
    // `context.LinkedChildren`; the manual links live in Ferrofin's
    // `LinkedChildren`).
    if !filter.linked_child_ancestor_ids.is_empty() {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "LinkedChildren" lc
                JOIN "AncestorIds" la ON la."ItemId" = lc."ChildId"
                WHERE lc."ParentId" = bi."Id" AND "#,
        );
        push_in_list(
            qb,
            r#"la."ParentItemId""#,
            &to_guid_strings(&filter.linked_child_ancestor_ids),
        );
        qb.push(")");
    }
}

/// Pushes C# `OrderMapper.MapSearchRelevanceOrder` — the match-quality rank a
/// search puts ahead of every other sort key.
///
/// `0` an exact `CleanName`, `1` the term followed by a word break, `2` any
/// other prefix, `3` a match found anywhere else (including one that only hit
/// `OriginalTitle`). `substr(…) = ?` is the `StartsWith` translation: it needs
/// no `LIKE` escaping, so a term carrying `%`/`_` still ranks literally.
fn push_search_relevance(qb: &mut QueryBuilder<Sqlite>, term: &str) {
    let clean = get_clean_value(term);
    let with_space = format!("{clean} ");
    let clean_len = i64::try_from(clean.chars().count()).unwrap_or(i64::MAX);
    let space_len = clean_len.saturating_add(1);

    qb.push(r#"CASE WHEN bi."CleanName" = "#)
        .push_bind(clean.clone())
        .push(r#" THEN 0 WHEN substr(bi."CleanName", 1, "#)
        .push_bind(space_len)
        .push(") = ")
        .push_bind(with_space)
        .push(r#" THEN 1 WHEN substr(bi."CleanName", 1, "#)
        .push_bind(clean_len)
        .push(") = ")
        .push_bind(clean)
        .push(" THEN 2 ELSE 3 END ASC");
}

/// SQLite's unary `+`, prefixed to an `ORDER BY` term to stop an index from
/// satisfying that ordering. The term stops being a bare column reference, so
/// the planner must sort — `WHERE`-clause index seeks are unaffected.
///
/// This exists because of `FerrofinIX_BaseItems_SortName_Name` (migration
/// `0018`). That index makes `ORDER BY SortName ASC, Name ASC` an ordered index
/// walk instead of "read every row, sort, take the page" — measured 8.4× on the
/// 100-item mixed browse. But an index walk also *fixes* the order of rows that
/// TIE on the sort key, and Jellyfin (which has no such index) leaves those in
/// whatever order its sort produced. Two orderings therefore have to keep the
/// sort:
///
/// - **descending** `SortName`: the index is walked backwards, so ties come out
///   reversed (126 of 9,679 positions on the bench library);
/// - **`ORDER BY SortName` with no `Name` tiebreaker** — the no-`sortBy`
///   default, and every key that falls back to the `SortName` column: `SortName`
///   alone is not a total order here (7,093 of 9,862 rows are people/studios/
///   genres with a NULL `SortName`), and the index reorders 7,189 positions.
///
/// `Name` (`CleanName`) is pinned for a different index: see [`pins_sort_plan`].
///
/// The ascending `(SortName, Name)` ordering is the one case proven identical:
/// all 9,679 rows come back in the same order with and without the index,
/// because SQLite's sorter and the index agree on rowid order within a tie.
const SORT_PLAN_PIN: &str = "+";

/// Whether the leading `ORDER BY` term must carry [`SORT_PLAN_PIN`].
///
/// True when the term is the bare `BaseItems."SortName"` column *and* the
/// ordering is not the proven-identical ascending `(SortName, Name)` shape.
/// A key that renders as anything else (`DateCreated`, a correlated user-data
/// sub-select, `ferrofin_random()`, …) can never be served by
/// `FerrofinIX_BaseItems_SortName_Name`, so it is left alone.
///
/// `Name` (`CleanName`) is pinned too, keeping the sort it had while it fell
/// back to `SortName`: unpinned, `IX_BaseItems_Type_CleanName` turns a typed
/// browse into an index walk that reads every row of that type, in every
/// library, to fill a page of one.
fn pins_sort_plan(by: ItemSortBy, order: SortOrder, filter: &InternalItemsQuery) -> bool {
    if by == ItemSortBy::SortName && order == SortOrder::Ascending {
        return false;
    }
    by == ItemSortBy::Name || orders_by_sort_name_column(by, filter)
}

/// Whether [`push_order_expression`] renders `by` as the bare
/// `bi."SortName"` column (rather than a correlated sub-select or another
/// column).
fn orders_by_sort_name_column(by: ItemSortBy, filter: &InternalItemsQuery) -> bool {
    // The `ItemValues`-correlated keys are sub-selects of their own, and so
    // is `SeriesDatePlayed`, with or without a user.
    if matches!(
        by,
        ItemSortBy::Artist
            | ItemSortBy::AlbumArtist
            | ItemSortBy::Studio
            | ItemSortBy::SeriesDatePlayed
    ) {
        return false;
    }
    // The user-data keys become correlated sub-selects when a user is present;
    // without one they fall through to the column like everything else.
    if filter.user_id().is_some()
        && matches!(
            by,
            ItemSortBy::DatePlayed
                | ItemSortBy::PlayCount
                | ItemSortBy::IsPlayed
                | ItemSortBy::IsUnplayed
                | ItemSortBy::IsFavoriteOrLiked
        )
    {
        return false;
    }
    order_column(by, filter.user_id().is_some()) == r#"bi."SortName""#
}

/// Appends the `ORDER BY` clause from `filter.order_by`, mapping each
/// [`ItemSortBy`] to its column (subset of C# `OrderMapper.MapOrderByField`),
/// with `SortName` as the default / tiebreaker when there is no search.
///
/// A non-blank `filter.search_term` takes the v12.2 `ApplyOrder` search
/// branch: the relevance rank leads, then the caller's keys exactly as without
/// a search (`ThenBy(e.Name)` after a leading `SortName` included), and no
/// final `SortName` tiebreaker — upstream guards it with `!hasSearch`, so with
/// no `sortBy` the relevance rank is the whole ordering. (10.11.8 evaluated
/// `[(SortName, Ascending), ..order_by]` here; upstream dropped that prepend,
/// which had made an explicit `sortBy` inert among equal-relevance hits.)
/// Ranking in SQL rather than after the fact is what makes the `LIMIT` keep
/// the *best* matches instead of the alphabetically first ones.
pub(crate) fn append_order_by(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    let ordered: Vec<&(ItemSortBy, SortOrder)> = filter
        .order_by
        .iter()
        .filter(|(by, _)| *by != ItemSortBy::Default)
        .collect();

    let search_term = filter
        .search_term
        .as_deref()
        .map(str::trim)
        .filter(|term| !term.is_empty());

    qb.push(" ORDER BY ");

    // A box set listing in its own `Default` order: the members' positions in
    // the container's linked children lead, whatever else is asked.
    if let Some(container) = filter.linked_order_of {
        qb.push(r#"(SELECT lc."SortOrder" FROM "LinkedChildren" lc WHERE lc."ParentId" = "#)
            .push_bind(guid_to_db(container))
            .push(r#" AND lc."ChildId" = bi."Id" LIMIT 1) ASC, "#);
    }

    // `UserRootFolder.GetEligibleChildrenForRecursiveChildren` (UserRootFolder.cs:96-102)
    // does `list.AddRange(LibraryManager.RootFolder.VirtualChildren)` — it
    // APPENDS the aggregate's plug-in folders after its own children rather
    // than merging them into a sort. Measured on 10.11.8, `/Items?parentId=
    // {userRoot}` answers `[Collections, Movies, Music, ParityCRUD, Recordings,
    // Shows, Playlists]` — Playlists LAST although its `SortName` ("playlists")
    // sorts before "shows (synth)". Expressed as a leading ordering term
    // because the concat is a SQL `OR` disjunct here, so without it the row
    // sorts inline.
    //
    // (`GET /Library/MediaFolders` is the opposite and already agrees:
    // LibraryController.cs:547-551 concatenates the same two sets and then
    // `.OrderBy(i => i.SortName)`, so there the playlists folder IS inline.)
    //
    // `virtual_child_parent_id` is set only for a browse of the user root or of
    // the aggregate itself, so no other statement gains this term.
    if let Some(aggregate) = filter.virtual_child_parent_id {
        qb.push(r#"(CASE WHEN bi."ParentId" = "#)
            .push_bind(guid_to_db(aggregate))
            .push(" THEN 1 ELSE 0 END), ");
    }

    if let Some(term) = search_term {
        push_search_relevance(qb, term);
        // The relevance rank leads, so no index can serve what follows and
        // nothing is pinned.
        if !ordered.is_empty() {
            qb.push(", ");
            push_caller_keys(qb, &ordered, filter, false);
        }
        return;
    }

    if ordered.is_empty() {
        // C# `ApplyOrder` returns `query.OrderBy(e => e.SortName)` here and adds
        // no tiebreaker, so rows that tie on `SortName` come back in whatever
        // order the engine's sort produced. `+` keeps that sort — see
        // [`SORT_PLAN_PIN`].
        qb.push(SORT_PLAN_PIN);
        qb.push(r#"bi."SortName""#);
        return;
    }

    // C# `ApplyOrder`: with no search, any `SeriesDatePlayed` key hands the
    // whole ordering to `ApplySeriesDatePlayedOrder`, which orders by the
    // series' played date in that key's direction, then `SortName` ascending,
    // and drops every other key (`QueryBuilding.cs:314-316, 415-419`).
    if let Some((_, order)) = ordered
        .iter()
        .find(|(by, _)| *by == ItemSortBy::SeriesDatePlayed)
    {
        push_series_date_played(qb, filter);
        qb.push(match order {
            SortOrder::Ascending => " ASC",
            SortOrder::Descending | SortOrder::Unrecognized(_) => " DESC",
        });
        qb.push(r#", bi."SortName" ASC"#);
        return;
    }

    // Only the leading term can be satisfied by an index, so it is the only one
    // that ever carries the plan pin.
    let pin_leading_term = pins_sort_plan(ordered[0].0, ordered[0].1, filter);
    push_caller_keys(qb, &ordered, filter, pin_leading_term);
    // C# "Add SortName as final tiebreaker" — only without a search, and only
    // when no key is already `SortName` or `Name`.
    if ordered
        .iter()
        .all(|(by, _)| !matches!(by, ItemSortBy::SortName | ItemSortBy::Name))
    {
        qb.push(r#", bi."SortName" ASC"#);
    }
}

/// Pushes the caller's (non-`Default`) sort keys — the body of C#
/// `ApplyOrder`'s `if (orderBy.Length > 0)` block, shared by the search and
/// the plain branch. `ordered` must not be empty, and the caller has already
/// pushed any separator the first key needs. `pin_leading_term` prefixes the
/// first key with [`SORT_PLAN_PIN`].
fn push_caller_keys(
    qb: &mut QueryBuilder<Sqlite>,
    ordered: &[&(ItemSortBy, SortOrder)],
    filter: &InternalItemsQuery,
    pin_leading_term: bool,
) {
    for (index, (by, order)) in ordered.iter().enumerate() {
        if index > 0 {
            qb.push(", ");
        } else if pin_leading_term {
            qb.push(SORT_PLAN_PIN);
        }
        push_order_expression(qb, *by, filter);
        qb.push(match order {
            SortOrder::Ascending => " ASC",
            SortOrder::Descending | SortOrder::Unrecognized(_) => " DESC",
        });
        // C# `ApplyOrder`:
        //     if (firstOrdering.OrderBy is ItemSortBy.Default or ItemSortBy.SortName)
        //         orderedQuery = ascending ? ThenBy(e => e.Name)
        //                                  : ThenByDescending(e => e.Name);
        //     foreach (var item in orderBy.Skip(1)) { … }
        //
        // The tiebreaker sits INSIDE the first-ordering block, BEFORE the loop
        // over the remaining keys — so it is the SECOND term, not the last.
        // Appending it after every key silently reorders any multi-key sort:
        // `SortBy=SortName,ProductionYear` (jellyfin-web's default Movies view)
        // becomes `SortName, ProductionYear, Name` where upstream is
        // `SortName, Name, ProductionYear`. Because `Name` is near-unique,
        // upstream's third term is effectively inert and ours is not.
        //
        // It fires only for a leading `SortName` (`append_order_by` has
        // already dropped `Default`), never for a leading `Name` — which
        // upstream maps to `CleanName`, not `SortName`.
        if index == 0 && *by == ItemSortBy::SortName {
            qb.push(match order {
                SortOrder::Ascending => r#", bi."Name" ASC"#,
                SortOrder::Descending | SortOrder::Unrecognized(_) => r#", bi."Name" DESC"#,
            });
        }
    }
}

/// Pushes the `SeriesDatePlayed` key: the newest `LastPlayedDate` among the
/// played rows of the items whose `SeriesPresentationUniqueKey` is the row's
/// `PresentationUniqueKey` — the requesting user's rows, or everyone's when
/// there is no user. `NULL` for a series none of whose items was finished.
///
/// The shape is upstream's `ApplySeriesDatePlayedOrder` (v12.2
/// `QueryBuilding.cs:392-413`): the played rows are grouped by series key in
/// ONE pass and the result joined on the key, not a `MAX()` walked per series
/// row. Here the grouped set is a non-correlated `MATERIALIZED` CTE
/// (`series_plays`) that SQLite builds once per statement and searches per
/// row through an automatic index on the key (a plain sub-select ran as a
/// co-routine inside the correlated look-up); the `EXPLAIN QUERY PLAN` test in
/// `tests/item_repository.rs` holds that plan. The series key columns lead
/// with `Type` in every index, so a per-series lookup could not seek them.
fn push_series_date_played(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    qb.push(
        r#"(WITH series_plays AS MATERIALIZED (
            SELECT ep."SeriesPresentationUniqueKey" AS "SeriesKey",
                   MAX(ud."LastPlayedDate") AS "LastPlayed"
            FROM "UserData" ud
                CROSS JOIN "BaseItems" ep ON ep."Id" = ud."ItemId"
            WHERE ud."Played" = 1"#,
    );
    if let Some(user_id) = filter.user_id() {
        qb.push(r#" AND ud."UserId" = "#)
            .push_bind(guid_to_db(user_id));
    }
    qb.push(
        r#" AND ep."SeriesPresentationUniqueKey" IS NOT NULL
            GROUP BY ep."SeriesPresentationUniqueKey")
        SELECT series_plays."LastPlayed" FROM series_plays
        WHERE series_plays."SeriesKey" = bi."PresentationUniqueKey")"#,
    );
}

/// Pushes the ORDER BY expression for one [`ItemSortBy`] key. The user-data
/// keys (`DatePlayed`, `PlayCount`, favorite state) are correlated
/// sub-selects scoped to the query's user (upstream `OrderMapper`'s
/// `UserData`-backed cases), and the played keys are [`push_is_played`];
/// everything else is a plain column.
fn push_order_expression(
    qb: &mut QueryBuilder<Sqlite>,
    by: ItemSortBy,
    filter: &InternalItemsQuery,
) {
    // `SeriesDatePlayed` keys on every user's plays when there is no user
    // (`OrderMapper.cs:84-87`), so it is not one of the keys below.
    if by == ItemSortBy::SeriesDatePlayed {
        push_series_date_played(qb, filter);
        return;
    }
    // The user-data-correlated keys need the requesting user; without one they
    // fall through to SortName (matching the C# `(key, null)` arms).
    if let Some(user_id) = filter.user_id() {
        let uid = guid_to_db(user_id);
        match by {
            // v12 `OrderMapper.cs:32-48`: "An item's played date is the newest
            // of its own progress and that of its alternate versions, which
            // track progress under their own ids. Matching both in one
            // predicate ORs them together, which no index can serve: the
            // user's whole UserData table gets scanned per sorted row. Two
            // indexed lookups combined by MAX cost a seek each instead."
            //
            // The OR form measured 107 ms of a 109 ms Resume request here (60
            // candidate rows × the user's ~5.4k rows). Two arms: the item's
            // own row on `IX_UserData_ItemId_UserId_LastPlayedDate`, and its
            // alternates' rows — `alt` first, on the partial
            // `IX_BaseItems_PrimaryVersionId`, then the same index
            // by the alternate's id. `CROSS JOIN` pins that order: left to
            // itself the planner re-drives the arm from `IX_UserData_UserId`
            // and walks the user's rows per candidate again (300 ms).
            ItemSortBy::DatePlayed => {
                qb.push(
                    r#"(SELECT MAX(d."LastPlayedDate") FROM (
                        SELECT oud."LastPlayedDate" FROM "UserData" oud
                            WHERE oud."ItemId" = bi."Id" AND oud."UserId" = "#,
                )
                .push_bind(uid.clone())
                .push(
                    r#" UNION ALL
                        SELECT oud."LastPlayedDate" FROM "BaseItems" alt
                            CROSS JOIN "UserData" oud
                                ON oud."ItemId" = alt."Id" AND oud."UserId" = "#,
                )
                .push_bind(uid)
                .push(r#" WHERE alt."PrimaryVersionId" = bi."Id") AS d)"#);
                return;
            }
            // "Folders carry no played flag of their own, so these two keys go
            // through the same predicate the isPlayed filter uses rather than
            // through the stored-column lookup in OrderMapper"
            // (`QueryBuilding.cs:327-336`, `AsOrderKey(BuildIsPlayedFilter(…))`
            // and its `.Not()`): ascending puts `false` first.
            ItemSortBy::IsPlayed | ItemSortBy::IsUnplayed => {
                qb.push("CASE WHEN ");
                push_is_played(qb, "bi", &uid, filter);
                qb.push(if by == ItemSortBy::IsPlayed {
                    " THEN 1 ELSE 0 END"
                } else {
                    " THEN 0 ELSE 1 END"
                });
                return;
            }
            // `e.UserData.Where(f => f.UserId == user).OrderBy(f => f.CustomDataKey)
            // .FirstOrDefault().<column>` (`OrderMapper.cs:57-61`): the item's
            // own first row — alternates do not take part.
            ItemSortBy::PlayCount | ItemSortBy::IsFavoriteOrLiked => {
                let column = if by == ItemSortBy::PlayCount {
                    r#""PlayCount""#
                } else {
                    r#""IsFavorite""#
                };
                // `Select((bool?)f.IsFavorite).FirstOrDefault() ?? false`: an
                // item the user has no row for is not a favorite, so it ties
                // with one whose row says so.
                let nullable_flag = by == ItemSortBy::IsFavoriteOrLiked;
                if nullable_flag {
                    qb.push("COALESCE(");
                }
                qb.push(format!(
                    r#"(SELECT oud.{column} FROM "UserData" oud
                        WHERE oud."ItemId" = bi."Id" AND oud."UserId" = "#
                ))
                .push_bind(uid)
                .push(r#" ORDER BY oud."CustomDataKey" LIMIT 1)"#);
                if nullable_flag {
                    qb.push(", 0)");
                }
                return;
            }
            _ => {}
        }
    }
    // `OrderMapper`'s `ItemValues`-correlated keys: the first value of the type
    // by its clean (ordinal) text — `Studio`, `Artist`, `AlbumArtist`.
    //
    // `CROSS JOIN` is the join-order pin: the row's own map rows
    // (`IX_ItemValuesMap_ItemId`), then each value by its primary key, and the
    // handful sorted per row. Left to choose, SQLite drove the sub-select from
    // `IX_ItemValues_Type_CleanValue (Type=?)`, which hands it the `ORDER BY`
    // for free, and probed the map for every value of the type, per row: a
    // recursive `/Items` page sorted by artist cost 1.1 s on the gl-637 corpus.
    if let Some(value_type) = match by {
        ItemSortBy::Artist => Some(0),
        ItemSortBy::AlbumArtist => Some(1),
        ItemSortBy::Studio => Some(3),
        _ => None,
    } {
        qb.push(format!(
            r#"(SELECT iv."CleanValue" FROM "ItemValuesMap" ivm
                CROSS JOIN "ItemValues" iv ON iv."ItemValueId" = ivm."ItemValueId"
                WHERE ivm."ItemId" = bi."Id" AND iv."Type" = {value_type}
                ORDER BY iv."CleanValue" LIMIT 1)"#
        ));
        return;
    }
    qb.push(order_column(by, filter.user_id().is_some()));
}

fn order_column(by: ItemSortBy, _has_user: bool) -> &'static str {
    match by {
        // Not SQLite's `RANDOM()`: that draws from one process-wide PRNG behind
        // a global mutex, taken once per scanned row, which is what capped
        // random-ordered endpoints (`/Items/Suggestions`) at ~450 req/s before
        // collapsing the whole server into kernel lock-wait. Same uniform
        // per-row draw, from thread-local state — see `ferrofin_db::sqlite_random`.
        ItemSortBy::Random => ferrofin_db::sqlite_random::RANDOM_SQL_EXPR,
        ItemSortBy::Runtime => r#"bi."RunTimeTicks""#,
        ItemSortBy::DateCreated => r#"bi."DateCreated""#,
        ItemSortBy::DateLastContentAdded => r#"bi."DateLastMediaAdded""#,
        // Falls back to a Jan-1 date synthesized from ProductionYear when
        // PremiereDate is null (OrderMapper: `PremiereDate ?? MinValue.AddYears
        // (ProductionYear - 1)`) — filename-year-only libraries still sort.
        ItemSortBy::PremiereDate => {
            r#"COALESCE(bi."PremiereDate",
                CASE WHEN bi."ProductionYear" IS NOT NULL
                     THEN printf('%04d-01-01 00:00:00.0000000', bi."ProductionYear") END)"#
        }
        ItemSortBy::StartDate => r#"bi."StartDate""#,
        ItemSortBy::Album => r#"bi."Album""#,
        ItemSortBy::CommunityRating => r#"bi."CommunityRating""#,
        ItemSortBy::CriticRating => r#"bi."CriticRating""#,
        ItemSortBy::ProductionYear => r#"bi."ProductionYear""#,
        ItemSortBy::VideoBitRate => r#"bi."TotalBitrate""#,
        ItemSortBy::ParentIndexNumber => r#"bi."ParentIndexNumber""#,
        ItemSortBy::IndexNumber => r#"bi."IndexNumber""#,
        ItemSortBy::IsFolder => r#"bi."IsFolder""#,
        ItemSortBy::OfficialRating => r#"bi."InheritedParentalRatingValue""#,
        ItemSortBy::SeriesSortName => r#"bi."SeriesName""#,
        // `(ItemSortBy.Name, _) => e => e.CleanName`: a track's or episode's
        // `SortName` leads with its numbers, so "Track name" must not use it.
        ItemSortBy::Name => r#"bi."CleanName""#,
        // Everything else falls back to SortName (the join-backed keys
        // `Artist`, `AlbumArtist` and `Studio` are expressions of their own).
        // That includes `AiredEpisodeOrder`: `OrderMapper` has no arm for it,
        // and `AiredEpisodeOrderComparer` only runs in upstream's in-memory
        // sorts (a folder's children, a season's episodes, Next Up).
        _ => r#"bi."SortName""#,
    }
}

/// Appends `LIMIT`/`OFFSET` from `start_index`/`limit` (C# `ApplyQueryPaging`).
fn append_paging(qb: &mut QueryBuilder<Sqlite>, filter: &InternalItemsQuery) {
    if let Some(limit) = filter.limit {
        qb.push(" LIMIT ").push_bind(i64::from(limit));
        let offset = filter.start_index.unwrap_or(0);
        if offset > 0 {
            qb.push(" OFFSET ").push_bind(i64::from(offset));
        }
    } else if let Some(offset) = filter.start_index.filter(|o| *o > 0) {
        // SQLite requires LIMIT before OFFSET; -1 means "no limit".
        qb.push(" LIMIT -1 OFFSET ").push_bind(i64::from(offset));
    }
}

/// Pushes `<column> IN (?, ?, …)` binding each value, or `1 = 0` for an empty
/// list (an empty `IN ()` is a SQL syntax error and, like C# `Contains` over an
/// empty set, matches nothing).
///
/// Values are cloned into the builder, so the source slice need not outlive it;
/// the cloned owned values (`String`/`i64`/`bool`) satisfy the `'a` encode bound.
pub(crate) fn push_in_list<'a, T>(qb: &mut QueryBuilder<Sqlite>, column: &str, values: &[T])
where
    T: sqlx::Type<Sqlite> + sqlx::Encode<'a, Sqlite> + Clone + 'a,
{
    if values.is_empty() {
        qb.push("1 = 0");
        return;
    }
    qb.push(column).push(" IN (");
    let mut sep = qb.separated(", ");
    for v in values {
        sep.push_bind(v.clone());
    }
    qb.push(")");
}

/// The `i64` discriminants for a set of [`ItemValueType`]s, for `Type IN (…)`.
fn item_value_type_ints(types: &[ItemValueType]) -> Vec<i64> {
    types.iter().map(|t| i64::from(i32::from(*t))).collect()
}

/// Converts item ids to the canonical stored `Guid` `TEXT` form (UPPERCASE
/// hyphenated) for binds.
pub(crate) fn to_guid_strings(ids: &[Uuid]) -> Vec<String> {
    ids.iter().copied().map(guid_to_db).collect()
}

/// The stored `MediaType` name for a [`ferrofin_model::data::MediaType`]
/// (C# `mediaType.ToString()`).
pub(crate) fn media_type_name(media: ferrofin_model::data::MediaType) -> String {
    format!("{media:?}")
}

/// Returns the inner string when the option holds a non-blank value, else
/// [`None`] (mirrors the C# `!string.IsNullOrWhiteSpace` guards).
pub(crate) fn non_blank(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::{
        QueryShape, build_latest_music_albums_query, build_latest_tv_episodes_query,
        build_latest_tv_series_query, build_query,
    };
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    use ferrofin_traits::options::InternalItemsQuery;

    /// The closing sweep's dead genre and studio selections
    /// ([`crate::library_scan::dead_by_name_queries`]) seek the by-name rows
    /// by `Type` and each row's values by `IX_ItemValues_Type_Value`: never a
    /// scan, which on a large library's every validation would be the pass's
    /// whole cost.
    #[tokio::test]
    async fn the_dead_by_name_selections_seek_the_values_by_type_and_value() {
        use sqlx::{Execute, Row};
        let db = crate::test_support::test_db().await;
        for filter in crate::library_scan::dead_by_name_queries() {
            let mut builder = build_query(&filter, QueryShape::IdsOnly);
            let mut query = builder.build();
            let args = query.take_arguments().expect("arguments").expect("binds");
            let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
            let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(db.pool())
                .await
                .expect("explain query plan")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect();
            assert!(
                plan.iter().any(|d| d.starts_with("SEARCH iv")
                    && d.contains("IX_ItemValues_Type_Value")
                    && d.contains("(Type=? AND Value=?)")),
                "{plan:?}"
            );
            assert!(!plan.iter().any(|d| d.starts_with("SCAN")), "{plan:?}");
        }
    }

    /// The movies arm of the grouped Latest (`build_latest_movies_query`), as
    /// the user-view manager sends it for a movies library and for the unscoped
    /// home row: the libraries' rows come off an index seek by top parent,
    /// never a scan of every item.
    #[tokio::test]
    async fn latest_movies_rows_seek_the_libraries() {
        use ferrofin_model::data::MediaType;
        use sqlx::{Execute, Row};
        let db = crate::test_support::test_db().await;
        let filter = InternalItemsQuery {
            is_folder: Some(false),
            is_virtual_item: Some(false),
            media_types: vec![MediaType::Video, MediaType::Audio],
            top_parent_ids: vec![uuid::Uuid::from_u128(0x8691), uuid::Uuid::from_u128(0x8692)],
            limit: Some(20),
            ..InternalItemsQuery::default()
        };
        let mut builder = super::build_latest_movies_query(&filter);
        let mut query = builder.build();
        let args = query.take_arguments().expect("arguments").expect("binds");
        let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
        let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
            .fetch_all(db.pool())
            .await
            .expect("explain query plan")
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect();
        assert!(
            plan.iter()
                .any(|d| d.starts_with("SEARCH bi USING ") && d.contains("TopParentId=?")),
            "{plan:#?}"
        );
        assert!(!plan.iter().any(|d| d.starts_with("SCAN bi")), "{plan:#?}");
    }

    /// The `ItemValues` sort keys (`OrderMapper`'s `Artist`, `AlbumArtist`,
    /// `Studio`) read each row's own values: the map by the row's item, then
    /// each value by its id. Left to choose, SQLite drove the per-row
    /// sub-select from `IX_ItemValues_Type_CleanValue (Type=?)` — the index
    /// that hands it the inner `ORDER BY` for free — and probed the map for
    /// every value of the type, per row: a recursive `/Items` page sorted by
    /// artist cost 1.1 s on the gl-637 corpus.
    #[rstest::rstest]
    #[case(ItemSortBy::Artist)]
    #[case(ItemSortBy::AlbumArtist)]
    #[case(ItemSortBy::Studio)]
    #[tokio::test]
    async fn item_value_sort_keys_read_each_rows_own_values(#[case] by: ItemSortBy) {
        use sqlx::{Execute, Row};
        let db = crate::test_support::test_db().await;
        let filter = InternalItemsQuery {
            order_by: vec![(by, SortOrder::Ascending)],
            limit: Some(20),
            ..InternalItemsQuery::default()
        };
        let mut builder = build_query(&filter, QueryShape::FullRows);
        let mut query = builder.build();
        let args = query.take_arguments().expect("arguments").expect("binds");
        let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
        let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
            .fetch_all(db.pool())
            .await
            .expect("explain query plan")
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect();
        let at = |prefix: &str| {
            plan.iter()
                .position(|d| d.starts_with(prefix))
                .unwrap_or_else(|| panic!("no {prefix}: {plan:#?}"))
        };
        let (ivm, iv) = (at("SEARCH ivm "), at("SEARCH iv "));
        assert!(ivm < iv, "the map drives: {plan:#?}");
        assert!(plan[ivm].ends_with("(ItemId=?)"), "{plan:#?}");
        assert!(plan[iv].ends_with("(ItemValueId=?)"), "{plan:#?}");
    }

    /// The candidate read behind the Playlists/Collections views
    /// (`UserViewManager.HasVisibleItem`, see
    /// [`crate::user_view_manager::visible_item_query`]), as the repository
    /// runs it once the user's top parents are resolved: a seek on a
    /// `(Type, TopParentId)` index and no scan. It is on `GET /UserViews`, the
    /// first call every client makes. The planner picks among the
    /// `IX_BaseItems_Type_TopParentId_*` family — the shared builder's
    /// alternate-version probe and `SortName` order keep any of them from
    /// covering — so the seek is what is pinned.
    #[tokio::test]
    async fn visible_item_candidates_seek_the_type_top_parent_index() {
        use sqlx::{Execute, Row};
        let db = crate::test_support::test_db().await;
        let user =
            crate::test_support::seed_user_with_defaults(&db, uuid::Uuid::from_u128(0x5501)).await;
        for kind in [BaseItemKind::Playlist, BaseItemKind::BoxSet] {
            let filter = crate::user_view_manager::visible_item_query(
                kind,
                &user,
                vec![uuid::Uuid::from_u128(0x5502), uuid::Uuid::from_u128(0x5503)],
            );
            let mut builder = build_query(&filter, QueryShape::IdsOnly);
            let mut query = builder.build();
            let args = query.take_arguments().expect("arguments").expect("binds");
            let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
            let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(db.pool())
                .await
                .expect("explain query plan")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect();
            assert!(
                plan.iter().any(|d| d.starts_with("SEARCH bi USING")
                    && d.contains("IX_BaseItems_Type_TopParentId_")
                    && d.contains("(Type=? AND TopParentId=?)")),
                "{kind:?}: {plan:?}"
            );
            assert!(
                !plan.iter().any(|d| d.starts_with("SCAN")),
                "{kind:?}: {plan:?}"
            );
        }
    }

    /// The folder arm of `push_is_played`, as a Series / BoxSet browse
    /// filters and sorts on it: the page keeps the unfiltered browse's
    /// `(Type, TopParentId)` seek, and each of the three descendant arms seeks
    /// the folder's own `AncestorIds` / `LinkedChildren` rows and probes the
    /// leaf by primary key. Nothing is scanned but the page's own subquery.
    /// A leaf browse, and a three-kind browse, over two libraries keep the same
    /// seek: the `IsFolder` disjuncts must not turn the page into a
    /// `MULTI-INDEX OR` over every folder and every leaf (unpinned, three kinds
    /// do).
    #[tokio::test]
    async fn the_folder_played_roll_up_seeks_each_folders_own_rows() {
        use sqlx::{Execute, Row};
        let db = crate::test_support::test_db().await;
        let user =
            crate::test_support::seed_user_with_defaults(&db, uuid::Uuid::from_u128(0x5511)).await;
        let browse = InternalItemsQuery {
            user: Some(user),
            recursive: true,
            include_item_types: vec![BaseItemKind::Series, BaseItemKind::BoxSet],
            top_parent_ids: vec![uuid::Uuid::from_u128(0x5512)],
            limit: Some(100),
            ..Default::default()
        };
        for filter in [
            InternalItemsQuery {
                is_played: Some(true),
                ..browse.clone()
            },
            InternalItemsQuery {
                is_played: Some(false),
                ..browse.clone()
            },
            InternalItemsQuery {
                order_by: vec![(ItemSortBy::IsUnplayed, SortOrder::Ascending)],
                ..browse.clone()
            },
            InternalItemsQuery {
                is_played: Some(true),
                include_item_types: vec![BaseItemKind::Movie],
                top_parent_ids: vec![uuid::Uuid::from_u128(0x5512), uuid::Uuid::from_u128(0x5513)],
                ..browse.clone()
            },
            InternalItemsQuery {
                is_played: Some(true),
                include_item_types: vec![
                    BaseItemKind::Movie,
                    BaseItemKind::Series,
                    BaseItemKind::BoxSet,
                ],
                top_parent_ids: vec![uuid::Uuid::from_u128(0x5512), uuid::Uuid::from_u128(0x5513)],
                ..browse.clone()
            },
        ] {
            let mut builder = build_query(&filter, QueryShape::FullRows);
            let mut query = builder.build();
            let args = query.take_arguments().expect("arguments").expect("binds");
            let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
            let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(db.pool())
                .await
                .expect("explain query plan")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect();
            let count = |detail: &str| plan.iter().filter(|d| d.starts_with(detail)).count();
            assert!(
                plan.iter().any(|d| d
                    .starts_with("SEARCH bi USING INDEX IX_BaseItems_Type_TopParentId_")
                    && d.contains("(Type=? AND TopParentId=?)")),
                "{plan:?}"
            );
            assert_eq!(
                count("SEARCH a USING INDEX IX_AncestorIds_ParentItemId (ParentItemId=?)"),
                2,
                "the closure and the linked folder's closure: {plan:?}"
            );
            assert_eq!(
                count("SEARCH lc USING INDEX IX_LinkedChildren_ParentId_ChildType (ParentId=?)"),
                2,
                "the members and the linked folders: {plan:?}"
            );
            assert_eq!(
                count("SEARCH l USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)"),
                3,
                "{plan:?}"
            );
            assert!(
                plan.iter().all(|d| !d.contains("MULTI-INDEX OR")
                    && (!d.starts_with("SCAN") || d.starts_with("SCAN (subquery-"))),
                "{plan:?}"
            );
        }
    }

    /// `push_resumable_predicate`'s `IsFolder` disjuncts, both arms, on a
    /// three-kind browse over two libraries: the page keeps its
    /// `(Type, TopParentId)` seek, and nothing is scanned but the page's own
    /// subquery and the played-leaf list. Unpinned, three kinds turn the disjuncts
    /// into a `MULTI-INDEX OR` over `IX_BaseItems_TopParentId_IsFolder_…`,
    /// whose two halves are every row of every library in scope.
    #[tokio::test]
    async fn the_resumable_filter_keeps_the_type_seek_over_three_kinds() {
        use sqlx::{Execute, Row};
        let db = crate::test_support::test_db().await;
        let user =
            crate::test_support::seed_user_with_defaults(&db, uuid::Uuid::from_u128(0x5521)).await;
        let browse = InternalItemsQuery {
            user: Some(user),
            recursive: true,
            include_item_types: vec![
                BaseItemKind::Movie,
                BaseItemKind::Series,
                BaseItemKind::BoxSet,
            ],
            top_parent_ids: vec![uuid::Uuid::from_u128(0x5522), uuid::Uuid::from_u128(0x5523)],
            limit: Some(100),
            ..Default::default()
        };
        for want in [false, true] {
            let filter = InternalItemsQuery {
                is_resumable: Some(want),
                ..browse.clone()
            };
            let mut builder = build_query(&filter, QueryShape::FullRows);
            let mut query = builder.build();
            let args = query.take_arguments().expect("arguments").expect("binds");
            let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
            let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(db.pool())
                .await
                .expect("explain query plan")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect();
            assert!(
                plan.iter().any(|d| d
                    .starts_with("SEARCH bi USING INDEX IX_BaseItems_Type_TopParentId_")
                    && d.contains("(Type=? AND TopParentId=?)")),
                "IsResumable={want}: {plan:#?}"
            );
            assert!(
                plan.iter().all(|d| !d.contains("MULTI-INDEX OR")
                    && !d.contains("IX_BaseItems_TopParentId_IsFolder_")
                    && (!d.starts_with("SCAN")
                        || d.starts_with("SCAN (subquery-")
                        || d == "SCAN pl")),
                "IsResumable={want}: {plan:#?}"
            );
        }
    }

    /// The `IsFolder` disjuncts of `append_resolution_predicate` (`IsHD` /
    /// `Is4K`) and `push_item_version_or_folder_matching` (`HasSubtitles`,
    /// `SubtitleLanguages`, `AudioLanguages`) on a three-kind browse over two
    /// libraries: every access of the page's own `bi` row is its
    /// `(Type, TopParentId)` seek or the hydration's primary key. Unpinned,
    /// three kinds turn the disjuncts into a `MULTI-INDEX OR` over
    /// `IX_BaseItems_TopParentId_IsFolder_…`, whose two halves are every row of
    /// every library in scope. The roll-ups' own `AncestorIds` /
    /// `LinkedChildren` index ORs are theirs to keep.
    #[tokio::test]
    async fn the_media_filters_keep_the_type_seek_over_three_kinds() {
        use sqlx::{Execute, Row};
        type Shape = (&'static str, fn(&mut InternalItemsQuery));
        let db = crate::test_support::test_db().await;
        let user =
            crate::test_support::seed_user_with_defaults(&db, uuid::Uuid::from_u128(0x5531)).await;
        let browse = InternalItemsQuery {
            user: Some(user),
            recursive: true,
            include_item_types: vec![
                BaseItemKind::Movie,
                BaseItemKind::Series,
                BaseItemKind::BoxSet,
            ],
            top_parent_ids: vec![uuid::Uuid::from_u128(0x5532), uuid::Uuid::from_u128(0x5533)],
            limit: Some(100),
            ..Default::default()
        };
        let shapes: [Shape; 9] = [
            ("IsHD=true", |f| f.is_hd = Some(true)),
            ("IsHD=false", |f| f.is_hd = Some(false)),
            ("Is4K=true", |f| f.is_4k = Some(true)),
            ("IsHD=true&Is4K=true", |f| {
                f.is_hd = Some(true);
                f.is_4k = Some(true);
            }),
            ("HasSubtitles=true", |f| f.has_subtitles = Some(true)),
            ("HasSubtitles=false", |f| f.has_subtitles = Some(false)),
            ("SubtitleLanguages=eng", |f| {
                f.subtitle_languages = vec!["eng".to_owned()];
            }),
            ("AudioLanguages=eng", |f| {
                f.audio_languages = vec!["eng".to_owned()];
            }),
            ("AudioLanguages=und", |f| {
                f.audio_languages = vec!["und".to_owned()];
            }),
        ];
        let mut lost = Vec::new();
        for (label, set) in shapes {
            let mut filter = browse.clone();
            set(&mut filter);
            let mut builder = build_query(&filter, QueryShape::FullRows);
            let mut query = builder.build();
            let args = query.take_arguments().expect("arguments").expect("binds");
            let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
            let plan: Vec<String> = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(db.pool())
                .await
                .expect("explain query plan")
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect();
            let type_seek = |d: &str| {
                d.starts_with("SEARCH bi USING INDEX IX_BaseItems_Type_TopParentId_")
                    && d.contains("(Type=? AND TopParentId=?)")
            };
            let bi: Vec<&String> = plan
                .iter()
                .filter(|d| d.starts_with("SEARCH bi ") || d.starts_with("SCAN bi"))
                .collect();
            if !bi.iter().any(|d| type_seek(d))
                || !bi.iter().all(|d| {
                    type_seek(d)
                        || *d == "SEARCH bi USING INDEX sqlite_autoindex_BaseItems_1 (Id=?)"
                })
                || plan
                    .iter()
                    .any(|d| d.contains("IX_BaseItems_TopParentId_IsFolder_"))
            {
                lost.push(format!("{label}: {plan:#?}"));
            }
        }
        assert!(
            lost.is_empty(),
            "lost the (Type, TopParentId) seek:\n{}",
            lost.join("\n")
        );
    }

    /// `GetLatestTvShowItems` step 1: the caller's predicates, grouped by
    /// `SeriesName`, newest maximum first, capped at `limit` — and not the
    /// caller's ORDER BY or start index.
    #[test]
    fn latest_tv_series_query_takes_the_newest_series() {
        let filter = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Episode],
            limit: Some(3),
            start_index: Some(5),
            order_by: vec![(ItemSortBy::DateCreated, SortOrder::Descending)],
            ..InternalItemsQuery::default()
        };
        let sql = build_latest_tv_series_query(&filter)
            .into_sql()
            .as_str()
            .to_owned();
        assert!(sql.contains(r#"bi."Type" IN"#), "{sql}");
        assert!(
            sql.ends_with(
                r#" AND bi."SeriesName" IS NOT NULL GROUP BY bi."SeriesName" ORDER BY "MaxDate" DESC LIMIT ?"#
            ),
            "{sql}"
        );
        let uncapped = build_latest_tv_series_query(&InternalItemsQuery::default())
            .into_sql()
            .as_str()
            .to_owned();
        assert!(
            uncapped.ends_with(r#"ORDER BY "MaxDate" DESC"#),
            "{uncapped}"
        );
    }

    /// Step 2: the named series' rows, bounded by the cutoff when there is
    /// one, newest first by `DateCreated`, then `Id`.
    #[test]
    fn latest_tv_episodes_query_reads_the_named_series_since_the_cutoff() {
        let names = ["A".to_owned(), "B".to_owned()];
        let cutoff = chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let sql =
            build_latest_tv_episodes_query(&InternalItemsQuery::default(), &names, Some(cutoff))
                .into_sql()
                .as_str()
                .to_owned();
        assert!(
            sql.ends_with(
                r#" AND bi."SeriesName" IN (?, ?) AND bi."DateCreated" >= ? ORDER BY bi."DateCreated" DESC, bi."Id" DESC"#
            ),
            "{sql}"
        );
        let unbounded =
            build_latest_tv_episodes_query(&InternalItemsQuery::default(), &names, None)
                .into_sql()
                .as_str()
                .to_owned();
        assert!(!unbounded.contains(r#""DateCreated" >= ?"#), "{unbounded}");
    }

    /// v12's music arm, library-scoped: album rows by their own `TopParentId`,
    /// `DateCreated DESC, Id DESC`, `LIMIT limit` — and NOT one predicate of
    /// the caller's track query, whose `MediaTypes`/`IsFolder`/`order_by` are
    /// all present on the filter and all ignored.
    #[test]
    fn latest_music_albums_query_reads_albums_by_top_parent() {
        let filter = InternalItemsQuery {
            media_types: vec![ferrofin_model::data::MediaType::Audio],
            is_folder: Some(false),
            is_virtual_item: Some(false),
            limit: Some(16),
            start_index: Some(5),
            top_parent_ids: vec![uuid::Uuid::from_u128(1)],
            order_by: vec![
                (ItemSortBy::DateCreated, SortOrder::Descending),
                (ItemSortBy::SortName, SortOrder::Descending),
            ],
            ..InternalItemsQuery::default()
        };
        let sql = build_latest_music_albums_query(&filter)
            .into_sql()
            .as_str()
            .to_owned();

        assert_eq!(
            sql,
            concat!(
                r#"SELECT bi.* FROM "BaseItems" AS bi WHERE bi."Type" = ?"#,
                r#" AND bi."IsVirtualItem" = 0 AND bi."TopParentId" IS NOT NULL"#,
                r#" AND bi."TopParentId" IN (?)"#,
                r#" ORDER BY bi."DateCreated" DESC, bi."Id" DESC LIMIT ?"#,
            ),
            "the album query carries no track predicate, no caller ordering and no offset"
        );
    }

    /// An `Ids` list makes the type predicate `+bi."Type"` (planner pin: the
    /// key lookups must drive the query, not a `Type` index — see
    /// `append_type_filters`); without one the column stays indexable.
    #[test]
    fn an_id_list_pins_the_type_predicate_off_the_index() {
        let with_ids = build_query(
            &InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Movie],
                item_ids: vec![uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2)],
                ..InternalItemsQuery::default()
            },
            QueryShape::FullRows,
        )
        .into_sql()
        .as_str()
        .to_owned();
        assert!(
            with_ids.contains(r#"AND +bi."Type" IN (?)"#)
                && with_ids.contains(r#"bi."Id" IN (?, ?)"#),
            "{with_ids}"
        );

        let without = build_query(
            &InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Movie],
                ..InternalItemsQuery::default()
            },
            QueryShape::FullRows,
        )
        .into_sql()
        .as_str()
        .to_owned();
        assert!(
            without.contains(r#"AND bi."Type" IN (?)"#) && !without.contains(r#"+bi."Type""#),
            "{without}"
        );
    }

    /// The ancestor-scoped fallback: no top parent ids, so the albums are the
    /// `AncestorIds` parents of the rows the caller's own (unpaged, unordered)
    /// query matches.
    #[test]
    fn latest_music_albums_query_falls_back_to_the_ancestor_closure() {
        let filter = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Audio],
            limit: Some(16),
            ancestor_ids: vec![uuid::Uuid::from_u128(2)],
            ..InternalItemsQuery::default()
        };
        let sql = build_latest_music_albums_query(&filter)
            .into_sql()
            .as_str()
            .to_owned();

        assert!(
            sql.contains(r#"bi."Id" IN (SELECT lam."ParentItemId" FROM "AncestorIds" AS lam"#),
            "{sql}"
        );
        assert!(
            sql.contains(r#"bi."Type" IN"#),
            "the caller's predicates select the matching rows: {sql}"
        );
        assert!(
            !sql.contains(r#"bi."TopParentId" IS NOT NULL"#),
            "the fallback has no top-parent test: {sql}"
        );
        assert!(
            sql.ends_with(r#" ORDER BY bi."DateCreated" DESC, bi."Id" DESC LIMIT ?"#),
            "{sql}"
        );
    }
    /// The `ORDER BY …` tail of the statement `filter` translates to.
    fn order_by(filter: &InternalItemsQuery) -> String {
        let sql = build_query(filter, QueryShape::FullRows)
            .into_sql()
            .as_str()
            .to_owned();
        let at = sql.find(" ORDER BY ").expect("statement has an ORDER BY");
        sql[at + 1..].to_owned()
    }

    fn sorted_by(keys: &[(ItemSortBy, SortOrder)]) -> InternalItemsQuery {
        InternalItemsQuery {
            order_by: keys.to_vec(),
            ..InternalItemsQuery::default()
        }
    }

    /// The `Name` tiebreaker is the SECOND term of a multi-key sort, not the
    /// last.
    ///
    /// C# `ApplyOrder` puts it inside the first-ordering block, before the
    /// loop over the remaining keys:
    ///
    /// ```csharp
    /// if (firstOrdering.OrderBy is ItemSortBy.Default or ItemSortBy.SortName)
    ///     orderedQuery = ascending ? ThenBy(e => e.Name) : ThenByDescending(e => e.Name);
    /// foreach (var item in orderBy.Skip(1)) { … }
    /// ```
    ///
    /// Appending it after every key instead silently reorders any multi-key
    /// sort. `SortBy=SortName,ProductionYear` is jellyfin-web's DEFAULT Movies
    /// view: upstream emits `SortName, Name, ProductionYear`, where `Name` is
    /// near-unique so the year term is effectively inert; appending gives
    /// `SortName, ProductionYear, Name`, where the year term actually reorders
    /// the page.
    #[test]
    fn the_name_tiebreaker_follows_the_first_key_not_the_last() {
        let asc = order_by(&sorted_by(&[
            (ItemSortBy::SortName, SortOrder::Ascending),
            (ItemSortBy::ProductionYear, SortOrder::Descending),
        ]));
        let name_at = asc.find(r#"bi."Name""#).expect("tiebreaker present");
        let year_at = asc
            .find(r#"bi."ProductionYear""#)
            .expect("second key present");
        assert!(
            name_at < year_at,
            "Name must precede the second key, got: {asc}"
        );

        // Direction follows the FIRST key, as `ThenByDescending` does.
        let desc = order_by(&sorted_by(&[
            (ItemSortBy::SortName, SortOrder::Descending),
            (ItemSortBy::DateCreated, SortOrder::Ascending),
        ]));
        assert!(
            desc.contains(r#"bi."Name" DESC"#),
            "a descending leading SortName takes a descending tiebreaker: {desc}"
        );

        // It fires only for a LEADING SortName — never for a leading Name,
        // which upstream maps to CleanName rather than SortName.
        let leading_name = order_by(&sorted_by(&[
            (ItemSortBy::Name, SortOrder::Ascending),
            (ItemSortBy::ProductionYear, SortOrder::Ascending),
        ]));
        assert_eq!(
            leading_name.matches(r#"bi."Name""#).count(),
            leading_name.matches(r#"bi."Name" ASC"#).count(),
            "no extra Name tiebreaker for a leading Name key: {leading_name}"
        );
    }

    /// `MinDateLastSaved` and `MinDateLastSavedForUser` are two independent
    /// clauses (`TranslateQuery.cs:357-365`): a query with both meets each.
    #[test]
    fn both_min_date_last_saved_bounds_apply() {
        let date = |s: &str| Some(s.parse::<chrono::DateTime<chrono::Utc>>().expect("date"));
        let sql = build_query(
            &InternalItemsQuery {
                min_date_last_saved: date("2026-01-01T00:00:00Z"),
                min_date_last_saved_for_user: date("2026-02-01T00:00:00Z"),
                ..InternalItemsQuery::default()
            },
            QueryShape::FullRows,
        )
        .into_sql()
        .as_str()
        .to_owned();
        assert_eq!(sql.matches(r#"bi."DateLastSaved" >= "#).count(), 2, "{sql}");
    }

    /// `ImageTypes` is an `EXISTS` over the item's image rows (C#
    /// `e.Images!.Any(w => imgTypes.Contains(w.ImageType))`), and costs nothing
    /// when unset — every other query must stay textually untouched.
    #[test]
    fn image_types_filter_is_an_exists_over_image_rows() {
        use ferrofin_model::entities::ImageType;
        let filtered = build_query(
            &InternalItemsQuery {
                image_types: vec![ImageType::Primary, ImageType::Thumb],
                ..InternalItemsQuery::default()
            },
            QueryShape::FullRows,
        )
        .into_sql()
        .as_str()
        .to_owned();
        assert!(
            filtered.contains(r#"EXISTS (SELECT 1 FROM "BaseItemImageInfos" ii"#),
            "{filtered}"
        );
        assert!(
            filtered.contains(r#"ii."ImageType" IN (?, ?)"#),
            "{filtered}"
        );
        let plain = build_query(&InternalItemsQuery::default(), QueryShape::FullRows)
            .into_sql()
            .as_str()
            .to_owned();
        assert!(!plain.contains("BaseItemImageInfos"), "{plain}");
    }

    /// The exact `ORDER BY` text for each shape, because the *text* is what
    /// decides whether `FerrofinIX_BaseItems_SortName_Name` may serve the
    /// ordering — and a lost `+` is a silent parity change (tie order), not a
    /// failing query. Row-level tests cannot see it: both plans return the same
    /// rows on any fixture without ties.
    #[test]
    fn sort_name_orderings_carry_the_upstream_tiebreaker_and_the_plan_pin() {
        // Explicit ascending SortName — upstream's ThenBy(Name), no pin: this
        // is the one ordering the index is proven to reproduce exactly.
        assert_eq!(
            order_by(&sorted_by(&[(ItemSortBy::SortName, SortOrder::Ascending)])),
            r#"ORDER BY bi."SortName" ASC, bi."Name" ASC"#
        );
        // Explicit descending — ThenByDescending(Name), and pinned: walking the
        // index backwards would reverse the order of tied rows.
        assert_eq!(
            order_by(&sorted_by(&[(ItemSortBy::SortName, SortOrder::Descending)])),
            r#"ORDER BY +bi."SortName" DESC, bi."Name" DESC"#
        );
        // No sortBy at all — upstream returns a bare OrderBy(SortName) with no
        // tiebreaker, so this one is pinned too.
        assert_eq!(
            order_by(&InternalItemsQuery::default()),
            r#"ORDER BY +bi."SortName""#
        );
        // `Name` orders by `CleanName` (upstream `OrderMapper`), takes no Name
        // or SortName tiebreaker, and keeps its pinned sort.
        assert_eq!(
            order_by(&sorted_by(&[(ItemSortBy::Name, SortOrder::Ascending)])),
            r#"ORDER BY +bi."CleanName" ASC"#
        );
        // A key that falls back to the SortName column without being SortName
        // is pinned for the same reason as the bare default, and takes
        // upstream's final SortName tiebreaker.
        assert_eq!(
            order_by(&sorted_by(&[(ItemSortBy::AirTime, SortOrder::Ascending)])),
            r#"ORDER BY +bi."SortName" ASC, bi."SortName" ASC"#
        );
        // `AiredEpisodeOrder` is one of those keys: `OrderMapper` has no arm
        // for it, so the query sorts by SortName (the comparer is only for
        // in-memory sorts), in either direction.
        assert_eq!(
            order_by(&sorted_by(&[(
                ItemSortBy::AiredEpisodeOrder,
                SortOrder::Ascending
            )])),
            r#"ORDER BY +bi."SortName" ASC, bi."SortName" ASC"#
        );
        assert_eq!(
            order_by(&sorted_by(&[(
                ItemSortBy::AiredEpisodeOrder,
                SortOrder::Descending
            )])),
            r#"ORDER BY +bi."SortName" DESC, bi."SortName" ASC"#
        );
        // A leading key on another column can never be served by the index, so
        // it is left unpinned — and takes no Name tiebreaker.
        assert_eq!(
            order_by(&sorted_by(&[(
                ItemSortBy::DateCreated,
                SortOrder::Descending
            )])),
            r#"ORDER BY bi."DateCreated" DESC, bi."SortName" ASC"#
        );
        // SortName as a *secondary* key: the leading key decides both the
        // tiebreaker and the pin.
        assert_eq!(
            order_by(&sorted_by(&[
                (ItemSortBy::ProductionYear, SortOrder::Ascending),
                (ItemSortBy::SortName, SortOrder::Ascending),
            ])),
            r#"ORDER BY bi."ProductionYear" ASC, bi."SortName" ASC"#
        );
    }

    /// The search branch is v12.2 `ApplyOrder`'s: the relevance rank leads,
    /// then the caller's keys in order — with `ThenBy(Name)` only after a
    /// leading `SortName` — and no final `SortName` tiebreaker (it is guarded by
    /// `!hasSearch`). 10.11.8 prepended `(SortName, Ascending)` to the caller's
    /// keys, which upstream dropped, so an explicit `sortBy` decided nothing
    /// among equal-relevance hits. The relevance rank leads, so no index can
    /// serve any of these orderings and nothing is pinned.
    #[test]
    fn search_ordering_follows_v12_apply_order() {
        const RELEVANCE: &str = r#"ORDER BY CASE WHEN bi."CleanName" = ? THEN 0 WHEN substr(bi."CleanName", 1, ?) = ? THEN 1 WHEN substr(bi."CleanName", 1, ?) = ? THEN 2 ELSE 3 END ASC"#;
        let searched = |keys: &[(ItemSortBy, SortOrder)]| {
            order_by(&InternalItemsQuery {
                search_term: Some("blade".to_owned()),
                ..sorted_by(keys)
            })
        };
        // No sortBy: relevance alone. `orderedQuery` is already the relevance
        // ordering, so neither the bare `OrderBy(SortName)` fallback nor the
        // `!hasSearch` tiebreaker applies.
        assert_eq!(searched(&[]), RELEVANCE);
        // An explicit key follows the relevance rank directly.
        assert_eq!(
            searched(&[(ItemSortBy::DateCreated, SortOrder::Descending)]),
            format!(r#"{RELEVANCE}, bi."DateCreated" DESC"#)
        );
        // A leading SortName earns `ThenBy(Name)` in its own direction (the
        // `/Search/Hints` shape)…
        assert_eq!(
            searched(&[(ItemSortBy::SortName, SortOrder::Ascending)]),
            format!(r#"{RELEVANCE}, bi."SortName" ASC, bi."Name" ASC"#)
        );
        assert_eq!(
            searched(&[(ItemSortBy::SortName, SortOrder::Descending)]),
            format!(r#"{RELEVANCE}, bi."SortName" DESC, bi."Name" DESC"#)
        );
        // …a secondary one does not, and nothing is appended after the keys.
        assert_eq!(
            searched(&[
                (ItemSortBy::ProductionYear, SortOrder::Ascending),
                (ItemSortBy::SortName, SortOrder::Ascending),
            ]),
            format!(r#"{RELEVANCE}, bi."ProductionYear" ASC, bi."SortName" ASC"#)
        );
        // `Default` keys are dropped before anything is emitted.
        assert_eq!(
            searched(&[(ItemSortBy::Default, SortOrder::Ascending)]),
            RELEVANCE
        );
    }
}
