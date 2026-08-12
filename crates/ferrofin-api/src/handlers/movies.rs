//! `MoviesController` — movie recommendation categories.
//!
//! Ports `GET /Movies/Recommendations`: a list of "because you watched"-style
//! [`RecommendationDto`] categories seeded from a parent's recent movies, built
//! through the
//! [`SimilarItemsManager`](ferrofin_traits::library::SimilarItemsManager) seam.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::dto::RecommendationDto;
use ferrofin_traits::options::DtoOptions;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::state::AppState;

/// The default number of recommendation categories to return (C#
/// `categoryLimit = 5`).
const DEFAULT_CATEGORY_LIMIT: i32 = 5;
/// The default number of items per recommendation category (C#
/// `itemLimit = 8`).
const DEFAULT_ITEM_LIMIT: i32 = 8;

/// The query parameters honoured by `GET /Movies/Recommendations`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecommendationsQuery {
    /// The target user; scopes visibility and attaches user data when present.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Localizes the recommendations to a specific parent item/folder.
    #[serde(default)]
    parent_id: Option<Uuid>,
    /// The maximum number of categories to return.
    #[serde(default)]
    category_limit: Option<i32>,
    /// The maximum number of items to return per category.
    #[serde(default)]
    item_limit: Option<i32>,
    /// The item fields to project onto each recommendation's DTOs. Absent ⇒
    /// empty ⇒ the base DTO, matching Jellyfin's `new DtoOptions { Fields = fields }`.
    #[serde(default)]
    fields: Option<String>,
}

/// `GET /Movies/Recommendations` — movie recommendation categories.
///
/// Port of `MoviesController.GetMovieRecommendations`.
#[utoipa::path(
    get,
    path = "/Movies/Recommendations",
    // Body schema omitted: `RecommendationDto` embeds the self-referential
    // `BaseItemDto`, which recurses in the OpenAPI generator.
    responses((status = 200, description = "Movie recommendations returned (Vec<RecommendationDto>)")),
    tag = "ferrofin"
)]
async fn get_movie_recommendations(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<RecommendationsQuery>,
) -> Result<Json<Vec<RecommendationDto>>, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id).await?;
    let user_uuid = user.as_ref().and_then(|u| Uuid::parse_str(&u.id).ok());
    // Honour the requested `Fields` like Jellyfin's `new DtoOptions { Fields = fields }`:
    // absent ⇒ empty ⇒ the base DTO, not all 47 fields (which meant a per-item query
    // storm — MediaSources/MediaStreams/Chapters/People — across every recommended item).
    let options = DtoOptions {
        fields: crate::handlers::query_parse::parse_csv_enums_lenient(query.fields.as_deref()),
        ..DtoOptions::default()
    };

    let recommendations = state
        .similar_items
        .get_movie_recommendations(
            user_uuid,
            query.parent_id.unwrap_or_else(Uuid::nil),
            query.category_limit.unwrap_or(DEFAULT_CATEGORY_LIMIT),
            query.item_limit.unwrap_or(DEFAULT_ITEM_LIMIT),
            &options,
        )
        .await?;

    // Project every category's items in one pass (one page prefetch instead of
    // one per category), then reassemble per category by position — order and
    // cross-category duplicates are preserved.
    let all_items: Vec<_> = recommendations
        .iter()
        .flat_map(|rec| rec.items.iter().cloned())
        .collect();
    let mut all_dtos = state
        .dto
        .get_base_item_dtos(&all_items, &options, user.as_ref(), None, true)
        .await?
        .into_iter();
    let mut dtos = Vec::with_capacity(recommendations.len());
    for rec in recommendations {
        let items: Vec<_> = all_dtos.by_ref().take(rec.items.len()).collect();
        dtos.push(RecommendationDto {
            items: Some(items),
            recommendation_type: rec.recommendation_type,
            baseline_item_name: Some(rec.baseline_item_name),
            category_id: rec.category_id,
        });
    }
    Ok(Json(dtos))
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/Movies/Recommendations", get(get_movie_recommendations))
}
