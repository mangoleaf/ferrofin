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
//! genre/studio ids), `UserData` predicates (favorite / played / liked), and the
//! ordering table are ported directly. The deep recursive-descendant `EXISTS`
//! folders (per-series played/resumable aggregation, box-set collapsing,
//! chapter/subtitle folder roll-ups) are **not** expanded here — they need the
//! `AncestorIds`/`HermitLinkedChildren` recursive CTEs that belong with the library
//! manager (a later unit). Those filters are skipped rather than mistranslated;
//! see the inline `// deferred:` notes. Everything ported matches the C#
//! predicate exactly for non-folder items.
//!
//! `Guid` columns are stored as UPPERCASE hyphenated `TEXT` and datetimes as
//! `YYYY-MM-DD HH:MM:SS.fffffff` (Jellyfin's canonical storage formats), so
//! identity binds use [`guid_to_db`] and datetime binds use [`datetime_to_db`]
//! — byte-identical to Jellyfin-written rows under SQLite `BINARY` collation.

use hermit_db::enums::ItemValueType;
use hermit_db::store::{datetime_to_db, guid_to_db};
use hermit_model::data::BaseItemKind;
use hermit_model::dto::SortOrder;
use hermit_model::live_tv::ItemSortBy;
use sqlx::{QueryBuilder, Sqlite};
use uuid::Uuid;

use crate::item_type_lookup::stored_type_name;
use crate::text_util::get_clean_value;
use hermit_traits::options::InternalItemsQuery;

/// The placeholder item id seeded by the initial migration; every real query
/// excludes it (C# `PlaceholderId`).
pub(crate) const PLACEHOLDER_ID: &str = "00000000-0000-0000-0000-000000000001";

/// The "unowned" predicate. C# treats `OwnerId = Guid.Empty` as "no owner": a
/// real Jellyfin database stores the ZERO GUID on virtually every row
/// (adopted-DB evidence), while Hermit's writer leaves the column NULL — every
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
    /// Select `COUNT(*)` — no `ORDER BY` / paging is appended.
    Count,
    /// Select `bi."Type", COUNT(*)` grouped by type — the per-type counts in one
    /// query, without materializing every matching row. No `ORDER BY` / paging.
    TypeCounts,
}

/// Builds the full translated statement for `filter` in the requested shape.
///
/// The returned [`QueryBuilder`] is ready to `.build_query_as()` /
/// `.build_query_scalar()`. Ordering and paging are appended for
/// [`QueryShape::FullRows`] and [`QueryShape::IdsOnly`]; [`QueryShape::Count`]
/// stops after the `WHERE` clause.
#[must_use]
pub fn build_query<'a>(
    filter: &'a InternalItemsQuery,
    shape: QueryShape,
) -> QueryBuilder<'a, Sqlite> {
    let mut qb: QueryBuilder<'a, Sqlite> = QueryBuilder::new(match shape {
        QueryShape::FullRows => r#"SELECT bi.* FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        QueryShape::IdsOnly => r#"SELECT bi."Id" FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        QueryShape::Count => r#"SELECT COUNT(*) FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        QueryShape::TypeCounts => {
            r#"SELECT bi."Type", COUNT(*) FROM "BaseItems" AS bi WHERE bi."Id" <> "#
        }
    });
    qb.push_bind(PLACEHOLDER_ID);

    append_predicates(&mut qb, filter);

    match shape {
        // Aggregate shapes take no ORDER BY / paging (they collapse the row set).
        QueryShape::Count => {}
        QueryShape::TypeCounts => {
            qb.push(r#" GROUP BY bi."Type""#);
        }
        QueryShape::FullRows | QueryShape::IdsOnly => {
            append_order_by(&mut qb, filter);
            append_paging(&mut qb, filter);
        }
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
    // --- resolution (own-row form; folder EXISTS roll-up is deferred) ---
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
            // Physical children only (delete-cascade): NEVER merge HermitLinkedChildren, so
            // deleting a box-set/playlist removes only the container, not the items it
            // references (linked children are references, not owned children).
            qb.push(r#" AND bi."ParentId" = "#)
                .push_bind(guid_to_db(filter.parent_id));
        } else {
            // Direct children: the physical `ParentId`, plus manually linked
            // members (C# `Folder.GetChildren` merges `LinkedChildren`). Only
            // box-sets and playlists carry `HermitLinkedChildren` rows, so the `IN`
            // subquery is empty for ordinary folders and this stays identical to
            // a plain `ParentId` equality for non-collection browses.
            qb.push(r#" AND (bi."ParentId" = "#)
                .push_bind(guid_to_db(filter.parent_id))
                .push(
                    r#" OR bi."Id" IN (SELECT "ChildId" FROM "HermitLinkedChildren" WHERE "ParentId" = "#,
                )
                .push_bind(guid_to_db(filter.parent_id))
                .push(r#" AND "ChildType" = 0))"#);
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
        if filter.is_resumable == Some(true) {
            qb.push(" AND (")
                .push(NO_OWNER)
                .push(r#" OR bi."ExtraType" IS NOT NULL)"#);
        } else {
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

/// Appends the `IsHD` / `Is4K` own-resolution predicate (the non-folder branch of
/// the C# resolution filter; the folder-descendant EXISTS roll-up is deferred).
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

/// Appends the `UserData`-backed predicates (favorite / favorite-or-liked / liked
/// / played) as `EXISTS` sub-selects scoped to the query's user.
///
/// The played/resumable series-and-boxset aggregation branch is deferred (needs
/// the library manager); this covers the direct per-item `UserData` predicates,
/// which is the C# `else` branch for non-series/boxset queries.
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
        // MinLikeValue upstream is 7 on a 0-10 rating scale.
        push_user_data_exists(qb, &uid, r#"ud."Rating" >= 7"#, want);
    }
    if let Some(want) = filter.is_played {
        push_user_data_exists(qb, &uid, r#"ud."Played" = 1"#, want);
    }
    if let Some(want) = filter.is_resumable {
        // A resumable item is one with an in-progress user-data row
        // (`PlaybackPositionTicks > 0`). C#
        // `BaseItemRepository.TranslateQuery` also has a series-aggregation
        // branch (a series is resumable when it has an in-progress or a
        // partially-watched episode); that aggregation needs the series/episode
        // walk and is deferred with the other series/box-set aggregation, so this
        // covers the direct per-item case (C# `else` branch).
        push_user_data_exists(qb, &uid, r#"ud."PlaybackPositionTicks" > 0"#, want);
    }
}

/// Appends `AND [NOT] EXISTS (SELECT 1 FROM UserData ud WHERE ud.ItemId = bi.Id
/// AND ud.UserId = <uid> AND <cond>)`.
fn push_user_data_exists(qb: &mut QueryBuilder<'_, Sqlite>, user_id: &str, cond: &str, want: bool) {
    qb.push(if want {
        " AND EXISTS "
    } else {
        " AND NOT EXISTS "
    })
    .push(r#"(SELECT 1 FROM "UserData" ud WHERE ud."ItemId" = bi."Id" AND ud."UserId" = "#)
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
        qb.push(
            r#" AND EXISTS (SELECT 1 FROM "PeopleBaseItemMap" pm WHERE pm."ItemId" = bi."Id" AND "#,
        );
        push_in_list(qb, r#"pm."PeopleId""#, &to_guid_strings(&filter.person_ids));
        qb.push(")");
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

/// Appends the ancestor / top-parent predicates.
///
/// `AncestorIds` is an `EXISTS` over the `AncestorIds` closure table; top-parent
/// is a direct `TopParentId` in-list. `LinkedChildAncestorIds` and the
/// item-by-name top-parent widening are deferred (library-manager concerns).
fn append_ancestor_predicates(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
    if !filter.top_parent_ids.is_empty() {
        qb.push(r#" AND bi."TopParentId" IS NOT NULL AND "#);
        push_in_list(
            qb,
            r#"bi."TopParentId""#,
            &to_guid_strings(&filter.top_parent_ids),
        );
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
}

/// Appends the `ORDER BY` clause from `filter.order_by`, mapping each
/// [`ItemSortBy`] to its column (subset of C# `OrderMapper.MapOrderByField`),
/// with `SortName` as the default / tiebreaker.
fn append_order_by(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
    let ordered: Vec<&(ItemSortBy, SortOrder)> = filter
        .order_by
        .iter()
        .filter(|(by, _)| *by != ItemSortBy::Default)
        .collect();

    qb.push(" ORDER BY ");
    if ordered.is_empty() {
        qb.push(r#"bi."SortName""#);
        return;
    }

    let mut first = true;
    let mut has_name_sort = false;
    for (by, order) in &ordered {
        if !first {
            qb.push(", ");
        }
        first = false;
        let col = order_column(*by);
        qb.push(col);
        qb.push(match order {
            SortOrder::Ascending => " ASC",
            SortOrder::Descending => " DESC",
        });
        if matches!(by, ItemSortBy::SortName | ItemSortBy::Name) {
            has_name_sort = true;
        }
    }
    // SortName tiebreaker, matching C# ApplyOrder.
    if !has_name_sort {
        qb.push(r#", bi."SortName" ASC"#);
    }
}

/// Maps an [`ItemSortBy`] to the `BaseItems` column it sorts on.
///
/// Covers the scalar-column cases of `OrderMapper.MapOrderByField`; the
/// user-data-correlated cases (`DatePlayed`, `PlayCount`, …) and the
/// `ItemValues`-correlated cases (`Artist`, `Studio`) fall back to `SortName`,
/// since they need joins the library manager owns. `Random` uses SQLite
/// `RANDOM()`.
fn order_column(by: ItemSortBy) -> &'static str {
    match by {
        ItemSortBy::Random => "RANDOM()",
        ItemSortBy::Runtime => r#"bi."RunTimeTicks""#,
        ItemSortBy::DateCreated => r#"bi."DateCreated""#,
        ItemSortBy::DateLastContentAdded => r#"bi."DateLastMediaAdded""#,
        ItemSortBy::PremiereDate => r#"bi."PremiereDate""#,
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
        // Name/SortName and every deferred (join-backed) case: SortName.
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
fn to_guid_strings(ids: &[Uuid]) -> Vec<String> {
    ids.iter().copied().map(guid_to_db).collect()
}

/// The stored `MediaType` name for a [`hermit_model::data::MediaType`]
/// (C# `mediaType.ToString()`).
fn media_type_name(media: hermit_model::data::MediaType) -> String {
    format!("{media:?}")
}

/// Returns the inner string when the option holds a non-blank value, else
/// [`None`] (mirrors the C# `!string.IsNullOrWhiteSpace` guards).
fn non_blank(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|s| !s.trim().is_empty())
}
