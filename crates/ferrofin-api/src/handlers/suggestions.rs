//! `SuggestionsController` — random per-user item suggestions.
//!
//! Ports `GET /Items/Suggestions`: a random-ordered, media/type-filtered page of
//! non-virtual library items, projected to [`BaseItemDto`]s. The legacy
//! `GET /Users/{userId}/Suggestions` alias is `ApiExplorerSettings(IgnoreApi)` in
//! Jellyfin, so it is absent from the vendored contract (no `501` stub exists);
//! it is registered here directly so clients still calling it don't get a `404`.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::{BaseItemDto, SortOrder};
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_model::querying::QueryResult;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::handlers::query_parse::parse_csv_enums;
use crate::state::AppState;

/// The query parameters honoured by `GET /Items/Suggestions`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SuggestionsQuery {
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Comma-delimited [`MediaType`] set to include.
    #[serde(default)]
    media_type: Option<String>,
    /// Comma-delimited [`BaseItemKind`] set to include.
    #[serde(default)]
    r#type: Option<String>,
    /// The index of the first record to return.
    #[serde(default)]
    start_index: Option<i32>,
    /// The maximum number of records to return.
    #[serde(default)]
    limit: Option<i32>,
    /// Whether to compute the total record count (defaults `false` in C#).
    #[serde(default)]
    enable_total_record_count: Option<bool>,
}

/// `GET /Items/Suggestions` — random per-user item suggestions.
///
/// Port of `SuggestionsController.GetSuggestions`.
#[utoipa::path(
    get,
    path = "/Items/Suggestions",
    // Body schema omitted: `BaseItemDto` recurses in the OpenAPI generator.
    responses((status = 200, description = "Suggestions returned (QueryResult<BaseItemDto>)")),
    tag = "ferrofin"
)]
async fn get_suggestions(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<SuggestionsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let media_types: Vec<MediaType> = parse_csv_enums(query.media_type.as_deref())?;
    let include_item_types: Vec<BaseItemKind> = parse_csv_enums(query.r#type.as_deref())?;

    let internal = InternalItemsQuery {
        user: user.clone(),
        order_by: vec![(ItemSortBy::Random, SortOrder::Descending)],
        media_types,
        include_item_types,
        is_virtual_item: Some(false),
        start_index: query.start_index,
        limit: query.limit,
        recursive: true,
        enable_total_record_count: query.enable_total_record_count.unwrap_or(false),
        ..InternalItemsQuery::default()
    };

    let result = state.library.query_items(&internal).await?;
    let options = DtoOptions::default();
    let dtos = state
        .dto
        .get_base_item_dtos(&result.items, &options, user.as_ref(), None, true)
        .await?;
    Ok(Json(QueryResult::new(
        query.start_index,
        Some(result.total_record_count),
        dtos,
    )))
}

/// `GET /Users/{userId}/Suggestions` — path-scoped form of
/// `GET /Items/Suggestions`, still served (hidden) by upstream.
async fn get_suggestions_for_user(
    state: State<AppState>,
    auth: RequireAuth,
    axum::extract::Path(user_id): axum::extract::Path<Uuid>,
    Query(mut query): Query<SuggestionsQuery>,
) -> Result<Json<QueryResult<BaseItemDto>>, ApiError> {
    query.user_id = Some(user_id);
    get_suggestions(state, auth, Query(query)).await
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Items/Suggestions", get(get_suggestions))
        .route("/Users/{userId}/Suggestions", get(get_suggestions_for_user))
}
