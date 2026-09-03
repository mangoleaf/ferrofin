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
//! ordering table are ported directly. The remaining folder-descendant
//! `EXISTS` roll-ups (the played filter's, the resolution and subtitle
//! roll-ups) are open work items, each named at its predicate with the
//! v12 reference; they take the `AncestorIds` walk `push_folders_with_leaf`
//! already spells. Everything ported matches the C# predicate exactly for
//! non-folder items.
//!
//! `Guid` columns are stored as UPPERCASE hyphenated `TEXT` and datetimes as
//! `YYYY-MM-DD HH:MM:SS.fffffff` (Jellyfin's canonical storage formats), so
//! identity binds use [`guid_to_db`] and datetime binds use [`datetime_to_db`]
//! — byte-identical to Jellyfin-written rows under SQLite `BINARY` collation.

use ferrofin_db::enums::ItemValueType;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::entities::VideoType;
use ferrofin_model::live_tv::ItemSortBy;
use sqlx::{QueryBuilder, Sqlite};
use uuid::Uuid;

use crate::item_type_lookup::stored_type_name;
use crate::text_util::get_clean_value;
use ferrofin_traits::options::InternalItemsQuery;

/// The placeholder item id seeded by the initial migration; every real query
/// excludes it (C# `PlaceholderId`).
pub(crate) const PLACEHOLDER_ID: &str = "00000000-0000-0000-0000-000000000001";

/// The "unowned" predicate. C# treats `OwnerId = Guid.Empty` as "no owner": a
/// real Jellyfin database stores the ZERO GUID on virtually every row
/// (adopted-DB evidence), while Ferrofin's writer leaves the column NULL — every
/// ownership predicate must accept both spellings of "unowned" or an adopted
/// library filters to nothing.
const NO_OWNER: &str =
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

/// Builds the full translated statement for `filter` in the requested shape.
///
/// The returned [`QueryBuilder`] is ready to `.build_query_as()` /
/// `.build_query_scalar()`. Ordering and paging are appended for
/// [`QueryShape::FullRows`], [`QueryShape::IdsOnly`] and
/// [`QueryShape::IdAndCleanName`]; the count shapes stop after the `WHERE`
/// clause.
#[must_use]
pub fn build_query<'a>(
    filter: &'a InternalItemsQuery,
    shape: QueryShape,
) -> QueryBuilder<'a, Sqlite> {
    let projection = match shape {
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
    let mut qb: QueryBuilder<'a, Sqlite> = QueryBuilder::new(projection);

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

    qb
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
/// The representative is the group's *primary* version where there is one.
/// SQLite reports the bare `"Id"` from whichever row produced the `MIN`, so the
/// aggregate is what picks it; without that a merged movie could be listed
/// under an alternate cut's title.
fn push_group_head(qb: &mut QueryBuilder<'_, Sqlite>) {
    qb.push(r#"bi."Id" IN (SELECT "rep" FROM (SELECT bi."Id" AS "rep", MIN("#)
        .push(r#"CASE WHEN bi."PrimaryVersionId" IS NULL THEN 0 ELSE 1 END) FROM "BaseItems" AS bi WHERE bi."Id" <> "#)
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
fn push_group_tail(qb: &mut QueryBuilder<'_, Sqlite>) {
    qb.push(r#" GROUP BY bi."PresentationUniqueKey"))"#);
}

/// Builds the "latest media" statement for a **tvshows** library — **v10.11.8**
/// `BaseItemRepository.GetLatestItemList` (`BaseItemRepository.cs:328-369`).
///
/// Upstream composes two translations of the **same** filter: a grouped
/// subquery that takes the newest `MAX(DateCreated)` per series (`SeriesName`),
/// keeps the top `filter.limit` groups, and uses the *smallest* of those maxima
/// as a threshold; the main query then returns every row at or above that
/// threshold, ordered by the caller's `order_by` and **unpaged** (upstream nulls
/// `Limit` before `ApplyQueryPaging`, so only a `StartIndex` survives). That is
/// what makes one query return whole groups — the caller groups the rows by
/// container afterwards.
///
/// Both translations share the predicate set, so the filter is appended twice;
/// the inner `bi` alias shadows the outer inside the subquery, exactly as EF's
/// nested query does.
///
/// The **music** arm no longer uses this shape:
/// [`build_latest_music_albums_query`] carries v12's rewritten one. This one
/// stays on 10.11.8 deliberately — v12 replaced the tvshows arm too, with
/// `GetLatestTvShowItems` (`Querying.cs:282+`, which picks a Season or Series
/// container per group), and porting that is its own work item, not part of the
/// music branch.
#[must_use]
pub(crate) fn build_latest_item_list_query(
    filter: &InternalItemsQuery,
) -> QueryBuilder<'_, Sqlite> {
    let mut qb: QueryBuilder<'_, Sqlite> =
        QueryBuilder::new(r#"SELECT bi.* FROM "BaseItems" AS bi WHERE "#);
    // `GetLatestItemList` runs `ApplyGroupingFilter` over the main query too
    // (`BaseItemRepository.cs:363`), so "latest" collapses merged versions the
    // same way a browse does — otherwise the two paths disagree about what a
    // duplicate is, and a four-cut film fills the row.
    let groups = group_by_presentation_unique_key(filter);
    if groups {
        push_group_head(&mut qb);
    } else {
        qb.push(r#"bi."Id" <> "#).push_bind(PLACEHOLDER_ID);
    }
    append_predicates(&mut qb, filter);

    // `mainquery.Where(g => g.DateCreated >= subqueryGrouped.Min(s => s.MaxDateCreated))`
    qb.push(r#" AND bi."DateCreated" >= (SELECT MIN(g."m") FROM ("#);
    qb.push(r#"SELECT MAX(bi."DateCreated") AS "m" FROM "BaseItems" AS bi WHERE bi."Id" <> "#);
    qb.push_bind(PLACEHOLDER_ID);
    append_predicates(&mut qb, filter);
    // `dbQuery.GroupBy(e => e.SeriesName)` — the one grouping key this shape
    // ever had once the music arm moved to [`build_latest_music_albums_query`]
    // (it used to take `bi."Album"` as a parameter).
    qb.push(r#" GROUP BY bi."SeriesName" ORDER BY "m" DESC"#);
    if let Some(limit) = filter.limit {
        qb.push(" LIMIT ").push_bind(i64::from(limit));
    }
    qb.push(") AS g)");
    if groups {
        push_group_tail(&mut qb);
    }

    append_order_by(&mut qb, filter);
    // `filter.Limit = null` upstream — only a start index can page this query.
    if let Some(offset) = filter.start_index.filter(|o| *o > 0) {
        qb.push(" LIMIT -1 OFFSET ").push_bind(i64::from(offset));
    }
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
/// `ApplyParentalRestrictions` (`:172`) is **not** ported, and upstream's
/// comment above it (`:169-171`) says exactly what that costs: neither arm
/// reads the album through the user's filters, so "a matching track does not
/// make an album the user may not see visible" — that call is the only guard,
/// and here the album comes back unguarded. This is not a gap this arm opened:
/// Ferrofin's query layer carries no parental predicates anywhere (see
/// [`ACCESSIBLE_LEAF`], which records the same absence on the leaf-access path,
/// and the standing work item noted in `ferrofin-api` `handlers/mod.rs`). The
/// one user scoping Ferrofin *does* have — `top_parent_ids` — is applied.
///
/// Upstream's closing `LoadLatestByIds` (`:180`) is folded in: it re-selects the
/// same ids under the same `DateCreated DESC, Id DESC` purely to attach EF
/// navigations, and `SELECT bi.*` already carries the whole row.
#[must_use]
pub(crate) fn build_latest_music_albums_query(
    filter: &InternalItemsQuery,
) -> QueryBuilder<'_, Sqlite> {
    let album_type = stored_type_name(BaseItemKind::MusicAlbum).unwrap_or_default();
    let mut qb: QueryBuilder<'_, Sqlite> =
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
    // mirrored into [`build_latest_item_list_query`], which would only move that
    // statement further from both upstream and the generic path.
    if let Some(limit) = filter.limit {
        qb.push(" LIMIT ").push_bind(i64::from(limit).max(0));
    }
    qb
}

/// Appends every `AND <predicate>` clause derived from the filter.
///
/// Each block mirrors one C# `if (filter.X …) baseQuery = baseQuery.Where(…)`.
/// Split out so [`build_query`] and the count path share the exact same WHERE.
#[allow(clippy::too_many_lines)]
pub(crate) fn append_predicates<'a>(
    qb: &mut QueryBuilder<'a, Sqlite>,
    filter: &'a InternalItemsQuery,
) {
    // --- resolution (own-row form; the folder-descendant roll-up is an open
    // work item, see `append_resolution_predicate`) ---
    if filter.is_hd.is_some() || filter.is_4k.is_some() {
        append_resolution_predicate(qb, filter);
    }
    if let Some(w) = filter.min_width {
        qb.push(r#" AND bi."Width" >= "#).push_bind(i64::from(w));
    }
    if let Some(h) = filter.min_height {
        qb.push(r#" AND bi."Height" >= "#).push_bind(i64::from(h));
    }
    if let Some(w) = filter.max_width {
        qb.push(r#" AND bi."Width" <= "#).push_bind(i64::from(w));
    }
    if let Some(h) = filter.max_height {
        qb.push(r#" AND bi."Height" <= "#).push_bind(i64::from(h));
    }

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
        let orig_like = format!("%{term}%");
        qb.push(r#" AND (bi."CleanName" LIKE "#)
            .push_bind(like)
            .push(r#" OR (bi."OriginalTitle" IS NOT NULL AND bi."OriginalTitle" LIKE "#)
            .push_bind(orig_like)
            .push("))");
    }

    if let Some(is_folder) = filter.is_folder {
        qb.push(r#" AND bi."IsFolder" = "#).push_bind(is_folder);
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
            // box-sets and playlists carry `FerrofinLinkedChildren` rows, so the `IN`
            // subquery is empty for ordinary folders and this stays identical to
            // a plain `ParentId` equality for non-collection browses.
            qb.push(r#" AND (bi."ParentId" = "#)
                .push_bind(guid_to_db(filter.parent_id))
                .push(
                    r#" OR bi."Id" IN (SELECT "ChildId" FROM "FerrofinLinkedChildren" WHERE "ParentId" = "#,
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
        // Exclude alternate versions + owned non-extra items from general queries.
        //
        // Two reasons to leave the alternates in, and they are the same
        // predicate: a resume query wants the version that was actually
        // played, and a grouped query collapses the versions itself. Upstream
        // has no `PrimaryVersionId` predicate at all, and dropping the rows
        // before the grouping removes 1,399 of them on a real library where
        // the grouping merges only 299 — the other 1,100 are separate titles
        // that simply have a primary version recorded.
        if filter.is_resumable == Some(true) || group_by_presentation_unique_key(filter) {
            qb.push(" AND (")
                .push(NO_OWNER)
                .push(r#" OR bi."ExtraType" IS NOT NULL)"#);
        } else {
            // Without a user there is no grouping (upstream's rule), so the
            // alternates still have to be excluded somehow. Upstream carries no
            // such predicate; this is the remaining divergence, and it only
            // affects queries no user-facing endpoint issues.
            qb.push(r#" AND bi."PrimaryVersionId" IS NULL AND ("#)
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
    // enum's `i32` value (`ExtraType::Trailer as i32`, …).
    if !filter.extra_types.is_empty() {
        qb.push(r#" AND bi."ExtraType" IS NOT NULL AND "#);
        let values: Vec<i64> = filter
            .extra_types
            .iter()
            .map(|e| i64::from(*e as i32))
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

/// Appends the `IsHD` / `Is4K` own-resolution predicate — the non-folder branch
/// of the C# resolution filter. v12 (`TranslateQuery.cs`, `IsHD`/`Is4K` with
/// `VersionsMatchingDimension` and the folder-descendant `EXISTS` roll-up) also
/// keeps a folder whose leaf descendant, or an alternate version, satisfies the
/// bound; that half is an open work item: port it with the played/resumable
/// descendant helpers (`push_folders_with_leaf`) once a browse needs it.
fn append_resolution_predicate(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
    let standard_def = filter.is_hd == Some(false);
    let high_def = filter.is_hd == Some(true);
    let ultra_hd = filter.is_4k == Some(true);

    qb.push(r#" AND (bi."Width" > 0 AND ("#);
    let mut first = true;
    let mut push_clause = |qb: &mut QueryBuilder<'_, Sqlite>, clause: &str| {
        if !first {
            qb.push(" OR ");
        }
        qb.push(clause);
        first = false;
    };
    if standard_def {
        push_clause(qb, &format!(r#"bi."Width" < {HD_WIDTH}"#));
    }
    if high_def {
        push_clause(
            qb,
            &format!(
                r#"(bi."Width" >= {HD_WIDTH} AND NOT (bi."Width" >= {UHD_WIDTH} OR bi."Height" >= {UHD_HEIGHT}))"#
            ),
        );
    }
    if ultra_hd {
        push_clause(
            qb,
            &format!(r#"(bi."Width" >= {UHD_WIDTH} OR bi."Height" >= {UHD_HEIGHT})"#),
        );
    }
    if first {
        // No sub-clause selected (e.g. IsHD only false-but-nothing): match nothing.
        qb.push("0");
    }
    qb.push("))");
}

/// Appends `IncludeItemTypes` / `ExcludeItemTypes` as `Type` in/not-in lists.
fn append_type_filters<'a>(qb: &mut QueryBuilder<'a, Sqlite>, filter: &'a InternalItemsQuery) {
    if filter.include_item_types.is_empty() {
        let excludes: Vec<String> = filter
            .exclude_item_types
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .map(ToOwned::to_owned)
            .collect();
        if !excludes.is_empty() {
            qb.push(" AND NOT ");
            push_in_list(qb, r#"bi."Type""#, &excludes);
        }
    } else {
        let includes: Vec<String> = filter
            .include_item_types
            .iter()
            .filter_map(|k| stored_type_name(*k))
            .map(ToOwned::to_owned)
            .collect();
        qb.push(" AND ");
        push_in_list(qb, r#"bi."Type""#, &includes);
    }
}

/// Appends the date-range predicates, including the `HasAired` sugar that maps to
/// an end-date bound relative to now.
fn append_date_predicates(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
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
    if let Some(d) = filter
        .min_date_last_saved
        .or(filter.min_date_last_saved_for_user)
    {
        qb.push(r#" AND (bi."DateLastSaved" IS NOT NULL AND bi."DateLastSaved" >= "#)
            .push_bind(datetime_to_db(d))
            .push(")");
    }
    if filter.is_airing == Some(true) {
        qb.push(r#" AND bi."StartDate" <= "#)
            .push_bind(datetime_to_db(now))
            .push(r#" AND bi."EndDate" >= "#)
            .push_bind(datetime_to_db(now));
    } else if filter.is_airing == Some(false) {
        qb.push(r#" AND bi."StartDate" > "#)
            .push_bind(datetime_to_db(now))
            .push(r#" AND bi."EndDate" < "#)
            .push_bind(datetime_to_db(now));
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
    qb: &mut QueryBuilder<'_, Sqlite>,
    filter: &InternalItemsQuery,
    _tags: &[String],
    _exclude_tags: &[String],
) {
    if let Some(name) = non_blank(filter.name.as_ref()) {
        if filter.use_raw_name == Some(true) {
            qb.push(r#" AND lower(bi."Name") = "#)
                .push_bind(name.to_lowercase());
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
            .push(r#" OR bi."OriginalTitle" LIKE "#)
            .push_bind(clean)
            .push(")");
    }

    // NameStartsWith* / NameLessThan compare on SortName (C# ApplyNameFilters).
    if let Some(prefix) = non_blank(filter.name_starts_with.as_ref()) {
        qb.push(r#" AND lower(bi."SortName") LIKE "#)
            .push_bind(format!("{}%", prefix.to_lowercase()));
    }
    if let Some(bound) = non_blank(filter.name_starts_with_or_greater.as_ref()) {
        qb.push(r#" AND lower(bi."SortName") >= "#)
            .push_bind(bound.to_lowercase());
    }
    if let Some(bound) = non_blank(filter.name_less_than.as_ref()) {
        qb.push(r#" AND lower(bi."SortName") < "#)
            .push_bind(bound.to_lowercase());
    }
}

/// Appends the media-attribute predicates: subtitle presence, owned-extra
/// presence (trailer / theme song / theme video / special feature), video
/// types, and 3D.
///
/// Ports of C# `TranslateQuery`'s `HasSubtitles` (`MediaStreams.Any(Subtitle)`),
/// the `ExtraIds`-backed extra filters, and the `Data`-substring `VideoType` /
/// `Video3DFormat` matches. The folder roll-up branch (v12
/// `WhereItemOrDescendantMatches`: a series "has subtitles" when any episode
/// does) is an open work item, to port with `push_folders_with_leaf`'s walk.
fn append_media_attribute_predicates(
    qb: &mut QueryBuilder<'_, Sqlite>,
    filter: &InternalItemsQuery,
) {
    // Subtitle presence: an `EXISTS` over the item's stream rows
    // (`StreamType` 2 = Subtitle).
    if let Some(want) = filter.has_subtitles {
        qb.push(if want {
            " AND EXISTS "
        } else {
            " AND NOT EXISTS "
        })
        .push(
            r#"(SELECT 1 FROM "MediaStreamInfos" ms
                WHERE ms."ItemId" = bi."Id" AND ms."StreamType" = 2)"#,
        );
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
            let name = match vt {
                VideoType::VideoFile => "VideoFile",
                VideoType::Iso => "Iso",
                VideoType::Dvd => "Dvd",
                VideoType::BluRay => "BluRay",
            };
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

/// Appends the `UserData`-backed predicates (favorite / favorite-or-liked / liked
/// / played / resumable) scoped to the query's user — the C# `TranslateQuery`
/// blocks at `BaseItemRepository.TranslateQuery.cs:514-603` (v12).
///
/// `IsPlayed` is the direct per-row rule (`UserData.Played` on the item
/// itself); v12's `BuildIsPlayedFilter` (`:39-56`) additionally counts a
/// folder as played once no leaf descendant is unplayed — an open work item
/// for the played filter, tracked with the other folder roll-ups.
fn append_user_data_predicates(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
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
        push_user_data_exists(qb, &uid, r#"ud."Played" = 1"#, want);
    }
    if let Some(want) = filter.is_resumable {
        push_resumable_predicate(qb, &uid, want);
    }
}

/// The in-progress `UserData` rows of the user: `PlaybackPositionTicks > 0`
/// (v12 `TranslateQuery.cs:558-559`, `inProgress`). Emitted as an
/// uncorrelated `SELECT` so SQLite materializes the user's list once and
/// drives the outer query through the `BaseItems` primary key — the IN form
/// [`push_user_data_exists`] measures at 105× the correlated `EXISTS`.
fn push_in_progress_ids(qb: &mut QueryBuilder<'_, Sqlite>, uid: &str) {
    qb.push(r#"(SELECT ud."ItemId" FROM "UserData" ud WHERE ud."UserId" = "#)
        .push_bind(uid.to_owned())
        .push(r#" AND ud."PlaybackPositionTicks" > 0)"#);
}

/// `DescendantQueryHelper.IsCountableLeaf` (v12 `DescendantQueryHelper.cs:21-22`):
/// real leaf media — neither a folder nor a virtual (missing / unaired) item —
/// on the `l` alias.
const COUNTABLE_LEAF: &str = r#"l."IsFolder" = 0 AND l."IsVirtualItem" = 0"#;

/// [`COUNTABLE_LEAF`] as `GetAccessFilteredLeafItemsQuery(context, user)`
/// (v12 `QueryBuilding.cs:669-679`) returns it without `includeOwnedItems`:
/// `ApplyAccessFiltering` (`:459-476`) drops alternate versions and owned
/// non-extras. (Its other two parts are no-ops for that call: `TopParentIds`
/// is empty on the fresh `InternalItemsQuery(user)`, and parental
/// restrictions are not a predicate Ferrofin's query layer carries.)
const ACCESSIBLE_LEAF: &str = r#"l."IsFolder" = 0 AND l."IsVirtualItem" = 0
    AND l."PrimaryVersionId" IS NULL
    AND (l."OwnerId" IS NULL OR l."OwnerId" = '00000000-0000-0000-0000-000000000000'
         OR l."ExtraType" IS NOT NULL)"#;

/// Pushes `bi."Id" IN (<the folders with a leaf descendant in `leaf_set`>)`:
/// the `AncestorIds` arm of v12 `BuildHasDescendantFilter`
/// (`QueryBuilding.cs:685-698`), driven from the user's `UserData` rows that
/// satisfy `ud_cond` rather than correlated per folder — the user's played /
/// in-progress rows times their ancestors is a few hundred to a few thousand
/// pairs, materialized once, where the per-folder walk costs every folder its
/// descendants.
///
/// The `LinkedChildren` arm (`:695-697`) is not emitted: the only kinds this
/// predicate is ever applied to are `Series` and `Season`
/// (`_resumableFolderKinds`), and neither has linked children — the arm can
/// never match for them.
fn push_folders_with_leaf(
    qb: &mut QueryBuilder<'_, Sqlite>,
    uid: &str,
    leaf_set: &str,
    ud_cond: &str,
) {
    qb.push(format!(
        r#"bi."Id" IN (SELECT a."ParentItemId" FROM "UserData" ud
            JOIN "BaseItems" l ON l."Id" = ud."ItemId" AND {leaf_set}
            JOIN "AncestorIds" a ON a."ItemId" = ud."ItemId"
            WHERE ud."UserId" = "#
    ))
    .push_bind(uid.to_owned())
    .push(format!(" AND {ud_cond})"));
}

/// Pushes the v12 `folderIsResumableFilter` minus its leading `IsFolder`
/// (`TranslateQuery.cs:569-575`): the row is a `Series` or a `Season` and
/// either a leaf descendant is in progress, or it has both a played and an
/// unplayed leaf descendant (partially watched). No percentage threshold;
/// `Played` does not exclude a leaf from the in-progress test.
///
/// Alternate versions keep their own progress, so they count towards the
/// in-progress check ([`COUNTABLE_LEAF`], `includeOwnedItems: true`) but not
/// towards the played/unplayed one ([`ACCESSIBLE_LEAF`]).
fn push_folder_is_resumable(qb: &mut QueryBuilder<'_, Sqlite>, uid: &str) {
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
    push_folders_with_leaf(qb, uid, COUNTABLE_LEAF, r#"ud."PlaybackPositionTicks" > 0"#);
    qb.push(" OR (");
    push_folders_with_leaf(qb, uid, ACCESSIBLE_LEAF, r#"ud."Played" = 1"#);
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
    .push(ACCESSIBLE_LEAF)
    .push(
        r#" AND NOT EXISTS (SELECT 1 FROM "UserData" ud
                    WHERE ud."ItemId" = l."Id" AND ud."UserId" = "#,
    )
    .push_bind(uid.to_owned())
    .push(r#" AND ud."Played" = 1)))))"#);
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
fn push_resumable_predicate(qb: &mut QueryBuilder<'_, Sqlite>, uid: &str, want: bool) {
    if want {
        qb.push(r#" AND ((bi."IsFolder" = 0 AND bi."Id" IN "#);
        push_in_progress_ids(qb, uid);
        qb.push(r#") OR (bi."IsFolder" = 1 AND "#);
        push_folder_is_resumable(qb, uid);
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
        // The short-circuit is `FerrofinIX_BaseItems_PrimaryVersionId`'s one
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
        let push_max_played = |qb: &mut QueryBuilder<'_, Sqlite>, alias: &str| {
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
        qb.push(r#" AND ((bi."IsFolder" = 1 AND NOT "#);
        push_folder_is_resumable(qb, uid);
        qb.push(
            r#") OR (bi."IsFolder" = 0 AND COALESCE(bi."PrimaryVersionId", bi."Id") NOT IN (SELECT COALESCE(x."PrimaryVersionId", x."Id") FROM "UserData" ud JOIN "BaseItems" x ON x."Id" = ud."ItemId" WHERE ud."UserId" = "#,
        )
        .push_bind(uid.to_owned())
        .push(r#" AND ud."PlaybackPositionTicks" > 0)))"#);
    }
}

/// Appends `AND [NOT] EXISTS (SELECT 1 FROM UserData ud WHERE ud.ItemId = bi.Id
/// AND ud.UserId = <uid> AND <cond>)`.
pub(crate) fn push_user_data_exists(
    qb: &mut QueryBuilder<'_, Sqlite>,
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
/// on (port of C# `WhereContainsPerson` / the `people` sub-query).
fn append_people_predicates(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
    if let Some(name) = non_blank(filter.person.as_ref()) {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" pm
                JOIN "Peoples" pp ON pp."Id" = pm."PeopleId"
                WHERE pm."ItemId" = bi."Id" AND pp."Name" = "#,
        );
        qb.push_bind(name.to_owned());
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
        qb.push(")))");
    }
}

/// Appends the `ItemValues`-backed predicates: genres, genre ids, studios, studio
/// ids, artist ids, album-artist ids, tags, and exclude-tags.
///
/// Each is an `EXISTS`/`NOT EXISTS` over `ItemValuesMap ⨝ ItemValues` filtered by
/// the value `Type` discriminant (mirrors the C# `e.ItemValues!.Any(…)`).
fn append_item_value_predicates(
    qb: &mut QueryBuilder<'_, Sqlite>,
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
    qb: &mut QueryBuilder<'_, Sqlite>,
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
    qb: &mut QueryBuilder<'_, Sqlite>,
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
fn append_is_dead_item_value(qb: &mut QueryBuilder<'_, Sqlite>, types: &[ItemValueType]) {
    qb.push(r#" AND NOT EXISTS (SELECT 1 FROM "ItemValues" iv WHERE iv."Value" = bi."Name" AND "#);
    push_in_list(qb, r#"iv."Type""#, &item_value_type_ints(types));
    qb.push(")");
}

/// Appends provider-id presence predicates (`HasImdbId` / `HasTmdbId` /
/// `HasTvdbId`) as `EXISTS` over `BaseItemProviders`.
fn append_provider_predicates(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
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

/// Appends the ancestor / top-parent predicates.
///
/// `AncestorIds` is an `EXISTS` over the `AncestorIds` closure table; top-parent
/// is a direct `TopParentId` in-list, widened for the by-name types when the
/// caller asked for them.
fn append_ancestor_predicates(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
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
        let by_name = item_by_name_types_in_query(filter);
        if filter.include_items_by_name.unwrap_or(false) && !by_name.is_empty() {
            qb.push(" AND (");
            push_in_list(qb, r#"bi."Type""#, &by_name);
            qb.push(r#" OR (bi."TopParentId" IS NOT NULL AND "#);
            push_in_list(
                qb,
                r#"bi."TopParentId""#,
                &to_guid_strings(&filter.top_parent_ids),
            );
            qb.push("))");
        } else {
            qb.push(r#" AND bi."TopParentId" IS NOT NULL AND "#);
            push_in_list(
                qb,
                r#"bi."TopParentId""#,
                &to_guid_strings(&filter.top_parent_ids),
            );
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
    // `FerrofinLinkedChildren`).
    if !filter.linked_child_ancestor_ids.is_empty() {
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "FerrofinLinkedChildren" lc
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
fn push_search_relevance(qb: &mut QueryBuilder<'_, Sqlite>, term: &str) {
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
fn pins_sort_plan(by: ItemSortBy, order: SortOrder, filter: &InternalItemsQuery) -> bool {
    if by == ItemSortBy::SortName && order == SortOrder::Ascending {
        return false;
    }
    orders_by_sort_name_column(by, filter)
}

/// Whether [`push_order_expression`] renders `by` as the bare
/// `bi."SortName"` column (rather than a correlated sub-select or another
/// column).
fn orders_by_sort_name_column(by: ItemSortBy, filter: &InternalItemsQuery) -> bool {
    // The user-data keys become correlated sub-selects when a user is present;
    // without one they fall through to the column like everything else.
    if filter.user_id().is_some()
        && matches!(
            by,
            ItemSortBy::DatePlayed
                | ItemSortBy::SeriesDatePlayed
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
/// with `SortName` as the default / tiebreaker.
///
/// A non-blank `filter.search_term` takes the C# `ApplyOrder` search branch:
/// the relevance rank leads, and `order_by` is evaluated as
/// `[(SortName, Ascending), ..order_by]` — that leading key being `SortName` is
/// also what earns C#'s `ThenBy(e.Name)` tiebreaker. Ranking in SQL rather than
/// after the fact is what makes the `LIMIT` keep the *best* matches instead of
/// the alphabetically first ones.
pub(crate) fn append_order_by(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
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
        qb.push(r#", bi."SortName" ASC, bi."Name" ASC"#);
        for (by, order) in ordered {
            qb.push(", ");
            push_order_expression(qb, *by, filter);
            qb.push(match order {
                SortOrder::Ascending => " ASC",
                SortOrder::Descending => " DESC",
            });
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

    let mut has_name_sort = false;
    // C# `ApplyOrder` appends `ThenBy(e => e.Name)` when the FIRST ordering key
    // is `SortName` — see below.
    let leads_with_sort_name = matches!(ordered.first(), Some((ItemSortBy::SortName, _)));
    // Only the leading term can be satisfied by an index, so it is the only one
    // that ever carries the plan pin.
    let pin_leading_term = pins_sort_plan(ordered[0].0, ordered[0].1, filter);
    for (index, (by, order)) in ordered.iter().enumerate() {
        if index > 0 {
            qb.push(", ");
        } else if pin_leading_term {
            qb.push(SORT_PLAN_PIN);
        }
        push_order_expression(qb, *by, filter);
        qb.push(match order {
            SortOrder::Ascending => " ASC",
            SortOrder::Descending => " DESC",
        });
        if matches!(by, ItemSortBy::SortName | ItemSortBy::Name) {
            has_name_sort = true;
        }
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
        // It fires only for a leading `SortName` (`Default` is filtered out
        // above), never for a leading `Name` — which upstream maps to
        // `CleanName`, not `SortName`.
        if index == 0 && leads_with_sort_name {
            qb.push(match order {
                SortOrder::Ascending => r#", bi."Name" ASC"#,
                SortOrder::Descending => r#", bi."Name" DESC"#,
            });
        }
    }
    if !leads_with_sort_name && !has_name_sort {
        qb.push(r#", bi."SortName" ASC"#);
    }
}

/// Maps an [`ItemSortBy`] to the `BaseItems` column it sorts on.
///
/// Covers the scalar-column cases of `OrderMapper.MapOrderByField`; the
/// user-data-correlated cases (`DatePlayed`, `PlayCount`, …) and the
/// `ItemValues`-correlated cases (`Artist`, `Studio`) fall back to `SortName`,
/// since they need joins the library manager owns. `Random` uses Ferrofin's
/// connection-local `ferrofin_random()`.
/// Pushes the ORDER BY expression for one [`ItemSortBy`] key. The user-data
/// keys (`DatePlayed`, `PlayCount`, played/favorite state) are correlated
/// sub-selects scoped to the query's user (upstream `OrderMapper`'s
/// `UserData`-backed cases); everything else is a plain column.
fn push_order_expression(
    qb: &mut QueryBuilder<'_, Sqlite>,
    by: ItemSortBy,
    filter: &InternalItemsQuery,
) {
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
            // `FerrofinIX_BaseItems_PrimaryVersionId`, then the same index
            // by the alternate's id. `CROSS JOIN` pins that order: left to
            // itself the planner re-drives the arm from `IX_UserData_UserId`
            // and walks the user's rows per candidate again (300 ms).
            //
            // `SeriesDatePlayed` shares the arm: v12 orders it through a
            // pre-aggregated join (`ApplySeriesDatePlayedOrder`) that is not
            // ported here, and its correlated fallback (`OrderMapper.cs:75-84`)
            // keys on `SeriesPresentationUniqueKey`, which is an open work
            // item for the series-sorted browses.
            ItemSortBy::DatePlayed | ItemSortBy::SeriesDatePlayed => {
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
            // `e.UserData.Where(f => f.UserId == user).OrderBy(f => f.CustomDataKey)
            // .FirstOrDefault().<column>` (`OrderMapper.cs:57-61`): the item's
            // own first row — alternates do not take part.
            //
            // Known divergence for `IsPlayed`/`IsUnplayed` WITH a user: v12
            // never reaches this arm for those two keys, because `ApplyOrder`
            // replaces them with `AsOrderKey(BuildIsPlayedFilter(...))`
            // (`QueryBuilding.cs:327-336`) — the folder-aware predicate that
            // counts a folder played once no leaf descendant is unplayed. Here
            // a folder has no `UserData` row of its own and sorts on NULL,
            // which orders ahead of `0`. Open work item, the same folder
            // roll-up the `is_played` FILTER still owes
            // (`append_user_data_predicates`): port `BuildIsPlayedFilter` over
            // the descendant helper `push_resumable_predicate` already uses,
            // then call it from both places and delete this arm's two keys.
            // Reachable only via `sortBy=IsPlayed`/`IsUnplayed`.
            ItemSortBy::PlayCount
            | ItemSortBy::IsPlayed
            | ItemSortBy::IsUnplayed
            | ItemSortBy::IsFavoriteOrLiked => {
                let column = match by {
                    ItemSortBy::PlayCount => r#""PlayCount""#,
                    ItemSortBy::IsFavoriteOrLiked => r#""IsFavorite""#,
                    _ => r#""Played""#,
                };
                // `Select((bool?)f.IsFavorite).FirstOrDefault() ?? false`.
                if by == ItemSortBy::IsFavoriteOrLiked {
                    qb.push("COALESCE(");
                }
                qb.push(format!(
                    r#"(SELECT oud.{column} FROM "UserData" oud
                        WHERE oud."ItemId" = bi."Id" AND oud."UserId" = "#
                ))
                .push_bind(uid)
                .push(r#" ORDER BY oud."CustomDataKey" LIMIT 1)"#);
                if by == ItemSortBy::IsFavoriteOrLiked {
                    qb.push(", 0)");
                }
                // IsUnplayed inverts the played flag (OrderMapper sorts `!IsPlayed`).
                if by == ItemSortBy::IsUnplayed {
                    qb.push(" * -1");
                }
                return;
            }
            _ => {}
        }
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
        // Aired order: (season, episode). Specials (`ParentIndexNumber = 0`) have
        // no air-date positioning here, so they sort *after* the regular seasons
        // (rather than at the very front, where `SortName`/filename order put
        // them and made "Play"/"Play All" start on a special). Port of
        // `AiredEpisodeOrderComparer` with the metadata-less special placed last.
        ItemSortBy::AiredEpisodeOrder => {
            r#"(CASE WHEN bi."ParentIndexNumber" = 0 THEN 1000000
                     ELSE COALESCE(bi."ParentIndexNumber", 0) END) * 100000
               + COALESCE(bi."IndexNumber", 0)"#
        }
        ItemSortBy::IsFolder => r#"bi."IsFolder""#,
        ItemSortBy::OfficialRating => r#"bi."InheritedParentalRatingValue""#,
        ItemSortBy::SeriesSortName => r#"bi."SeriesName""#,
        // Name/SortName, and the join-backed keys (`Artist`, `AlbumArtist`,
        // `Studio`: v12 orders by the first ItemValue of the type — an open work
        // item) fall back to SortName.
        _ => r#"bi."SortName""#,
    }
}

/// Appends `LIMIT`/`OFFSET` from `start_index`/`limit` (C# `ApplyQueryPaging`).
fn append_paging(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
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
pub(crate) fn push_in_list<'a, T>(qb: &mut QueryBuilder<'a, Sqlite>, column: &str, values: &[T])
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
        QueryShape, build_latest_item_list_query, build_latest_music_albums_query, build_query,
    };
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    use ferrofin_traits::options::InternalItemsQuery;

    /// The statement `GetLatestItemList` sends: grouped-threshold subquery
    /// capped at `limit`, the caller's triple ORDER BY verbatim, and no paging
    /// on the outer query (upstream nulls `Limit` before `ApplyQueryPaging`).
    #[test]
    fn latest_item_list_query_groups_thresholds_and_does_not_page() {
        let filter = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Episode],
            limit: Some(3),
            order_by: vec![
                (ItemSortBy::DateCreated, SortOrder::Descending),
                (ItemSortBy::SortName, SortOrder::Descending),
                (ItemSortBy::ProductionYear, SortOrder::Descending),
            ],
            ..InternalItemsQuery::default()
        };
        let qb = build_latest_item_list_query(&filter);
        let sql = qb.sql();

        assert!(
            sql.contains(r#"GROUP BY bi."SeriesName" ORDER BY "m" DESC LIMIT "#),
            "grouped subquery must order the per-group maxima newest-first and take `limit`: {sql}"
        );
        assert!(
            sql.contains(r#"bi."DateCreated" >= (SELECT MIN(g."m") FROM ("#),
            "the threshold is the smallest of the top-N group maxima: {sql}"
        );
        // The type predicate is applied to BOTH translations of the filter.
        assert_eq!(
            sql.matches(r#"bi."Type" IN"#).count(),
            2,
            "the subquery and the main query share the predicate set: {sql}"
        );
        let order = sql.rsplit(" ORDER BY ").next().expect("an outer ORDER BY");
        assert_eq!(
            order, r#"bi."DateCreated" DESC, bi."SortName" DESC, bi."ProductionYear" DESC"#,
            "the caller's ordering rides through untouched, with no trailing LIMIT"
        );
    }

    /// Without a `limit` the grouped subquery is uncapped (C# `Take` is only
    /// applied when `filter.Limit.HasValue`); a start index still pages the
    /// outer query as `LIMIT -1 OFFSET ?`.
    #[test]
    fn latest_item_list_query_without_limit_is_uncapped_and_offsets() {
        let filter = InternalItemsQuery {
            start_index: Some(5),
            ..InternalItemsQuery::default()
        };
        let qb = build_latest_item_list_query(&filter);
        let sql = qb.sql();
        assert!(
            sql.contains(r#"GROUP BY bi."SeriesName" ORDER BY "m" DESC) AS g)"#),
            "{sql}"
        );
        assert!(sql.ends_with(" LIMIT -1 OFFSET ?"), "{sql}");
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
        let sql = build_latest_music_albums_query(&filter).into_sql();

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
        let sql = build_latest_music_albums_query(&filter).into_sql();

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
        let sql = build_query(filter, QueryShape::FullRows).into_sql();
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
        .into_sql();
        assert!(
            filtered.contains(r#"EXISTS (SELECT 1 FROM "BaseItemImageInfos" ii"#),
            "{filtered}"
        );
        assert!(
            filtered.contains(r#"ii."ImageType" IN (?, ?)"#),
            "{filtered}"
        );
        let plain = build_query(&InternalItemsQuery::default(), QueryShape::FullRows).into_sql();
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
        // A key that falls back to the SortName column without being SortName
        // (upstream maps `Name` to `CleanName`, so it gets no Name tiebreaker)
        // is pinned for the same reason as the bare default.
        assert_eq!(
            order_by(&sorted_by(&[(ItemSortBy::Name, SortOrder::Ascending)])),
            r#"ORDER BY +bi."SortName" ASC"#
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

    /// The search branch already carried upstream's `(SortName, Name)` pair;
    /// its relevance rank leads, so no index can serve the ordering and the
    /// leading term must stay unpinned.
    #[test]
    fn search_ordering_is_unchanged() {
        let filter = InternalItemsQuery {
            search_term: Some("blade".to_owned()),
            ..InternalItemsQuery::default()
        };
        let sql = order_by(&filter);
        assert!(
            sql.contains(r#", bi."SortName" ASC, bi."Name" ASC"#),
            "search keeps the SortName/Name pair: {sql}"
        );
        assert!(
            !sql.contains('+'),
            "the relevance rank leads, so nothing is pinned: {sql}"
        );
    }
}
