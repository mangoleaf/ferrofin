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
//! `AncestorIds`/`FerrofinLinkedChildren` recursive CTEs that belong with the library
//! manager (a later unit). Those filters are skipped rather than mistranslated;
//! see the inline `// deferred:` notes. Everything ported matches the C#
//! predicate exactly for non-folder items.
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
/// [`QueryShape::FullRows`], [`QueryShape::IdsOnly`] and
/// [`QueryShape::IdAndCleanName`]; [`QueryShape::Count`] stops after the
/// `WHERE` clause.
#[must_use]
pub fn build_query<'a>(
    filter: &'a InternalItemsQuery,
    shape: QueryShape,
) -> QueryBuilder<'a, Sqlite> {
    let mut qb: QueryBuilder<'a, Sqlite> = QueryBuilder::new(match shape {
        QueryShape::FullRows => r#"SELECT bi.* FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        QueryShape::IdsOnly => r#"SELECT bi."Id" FROM "BaseItems" AS bi WHERE bi."Id" <> "#,
        QueryShape::IdAndCleanName => {
            r#"SELECT bi."Id", bi."CleanName" FROM "BaseItems" AS bi WHERE bi."Id" <> "#
        }
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
        QueryShape::FullRows | QueryShape::IdsOnly | QueryShape::IdAndCleanName => {
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

/// Appends the media-attribute predicates: subtitle presence, owned-extra
/// presence (trailer / theme song / theme video / special feature), video
/// types, and 3D.
///
/// Ports of C# `TranslateQuery`'s `HasSubtitles` (`MediaStreams.Any(Subtitle)`),
/// the `ExtraIds`-backed extra filters, and the `Data`-substring `VideoType` /
/// `Video3DFormat` matches. The folder roll-up branches (a series "has
/// subtitles" when any episode does) are deferred with the other
/// series/box-set aggregation.
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
fn append_order_by(qb: &mut QueryBuilder<'_, Sqlite>, filter: &InternalItemsQuery) {
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
        let correlated = match by {
            // MAX over the item and its merged alternates (OrderMapper:
            // `w.ItemId == e.Id || w.Item.PrimaryVersionId == e.Id`).
            ItemSortBy::DatePlayed | ItemSortBy::SeriesDatePlayed => Some(r#""LastPlayedDate""#),
            ItemSortBy::PlayCount => Some(r#""PlayCount""#),
            ItemSortBy::IsPlayed | ItemSortBy::IsUnplayed => Some(r#""Played""#),
            ItemSortBy::IsFavoriteOrLiked => Some(r#""IsFavorite""#),
            _ => None,
        };
        if let Some(column) = correlated {
            qb.push(format!(
                r#"(SELECT MAX(oud.{column}) FROM "UserData" oud
                    WHERE oud."UserId" = "#
            ));
            qb.push_bind(uid);
            // The alternates arm navigates UserData.ItemId → BaseItems.Id (a
            // PK lookup), exactly as upstream's `w.Item.PrimaryVersionId ==
            // e.Id`. The inverted `IN (SELECT … WHERE PrimaryVersionId = …)`
            // form re-scanned BaseItems per (row × userdata row): 97.5s for a
            // 12-row Resume query on the live DB vs 33ms this way.
            qb.push(
                r#" AND (oud."ItemId" = bi."Id" OR EXISTS
                    (SELECT 1 FROM "BaseItems" alt
                     WHERE alt."Id" = oud."ItemId"
                       AND alt."PrimaryVersionId" = bi."Id")))"#,
            );
            // IsUnplayed inverts the played flag (OrderMapper sorts `!IsPlayed`).
            if by == ItemSortBy::IsUnplayed {
                qb.push(" * -1");
            }
            return;
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

/// The stored `MediaType` name for a [`ferrofin_model::data::MediaType`]
/// (C# `mediaType.ToString()`).
fn media_type_name(media: ferrofin_model::data::MediaType) -> String {
    format!("{media:?}")
}

/// Returns the inner string when the option holds a non-blank value, else
/// [`None`] (mirrors the C# `!string.IsNullOrWhiteSpace` guards).
fn non_blank(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::{QueryShape, build_query};
    use ferrofin_model::dto::SortOrder;
    use ferrofin_model::live_tv::ItemSortBy;
    use ferrofin_traits::options::InternalItemsQuery;

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
