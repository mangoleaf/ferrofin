//! `ImageController` — item, by-name, and user image serving (read side).
//!
//! Ports the *read* surface of Jellyfin's `ImageController`: resolve an item (or
//! a by-name genre/studio/person/artist, or a user profile), find the requested
//! [`ItemImageInfo`] by type (and optional index), and serve its file. The image
//! rows come from [`LibraryManager::get_item_images`] /
//! [`UserManager::get_profile_image`], both backed by real `hermit-db` reads.
//!
//! ## Port scope — original-file serving
//!
//! Jellyfin runs every image through its `IImageProcessor` (resize, format
//! conversion, blur/overlay). No concrete image processor is ported yet, so this
//! layer serves the **stored original file**: the `maxWidth`/`fillHeight`/`format`
//! /`blur`/… query parameters are accepted for contract compatibility but do not
//! transform the bytes — the original image is returned. Parametrized resize and
//! blurhash generation arrive with the image-processor port; the request shape,
//! resolution logic, and `404`/`400` outcomes here are already the final ones.
//!
//! A missing item (or by-name item, or user), or an item with no image of the
//! requested type/index, or an image whose file is remote/absent, is a `404` —
//! exactly the contract's not-found outcome. `HEAD` shares each handler with
//! `GET` (the file service is `HEAD`-aware).

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::response::Response;
use axum::routing::get;
use hermit_db::entities::users::UserEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::dto::ImageInfo;
use hermit_model::entities::ImageType;
use hermit_traits::options::ItemImageInfo;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::items::resolve_user_opt;
use crate::state::AppState;

/// Parses the `{imageType}` path segment into an [`ImageType`].
///
/// Jellyfin serializes the enum by its variant name (case-insensitive over the
/// wire in practice); an unknown value is a `400`.
fn parse_image_type(raw: &str) -> Result<ImageType, ApiError> {
    let parsed = match raw.to_ascii_lowercase().as_str() {
        "primary" => ImageType::Primary,
        "art" => ImageType::Art,
        "backdrop" => ImageType::Backdrop,
        "banner" => ImageType::Banner,
        "logo" => ImageType::Logo,
        "thumb" => ImageType::Thumb,
        "disc" => ImageType::Disc,
        "box" => ImageType::Box,
        "screenshot" => ImageType::Screenshot,
        "menu" => ImageType::Menu,
        "chapter" => ImageType::Chapter,
        "boxrear" => ImageType::BoxRear,
        "profile" => ImageType::Profile,
        other => return Err(ApiError::BadRequest(format!("unknown image type {other}"))),
    };
    Ok(parsed)
}

/// The query parameters accepted by every image-serving route.
///
/// Every field is optional and mirrors Jellyfin's `GetImage*` signature. Under
/// original-file serving none of them transform the bytes (see the module docs),
/// but they are parsed so a client sending them still gets a `200`; only
/// `user_id` (for the user-image route) changes which file is served.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageQuery {
    /// The target user (user-image route only); defaults to the caller.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Optional explicit image index (query-string form of the route segment).
    #[serde(default)]
    image_index: Option<i32>,
}

/// Selects the image of `image_type` at `index` (default `0`) from an item's
/// image rows, mirroring `BaseItem.GetImageInfo(type, index)`.
///
/// Images of a type keep their stored order; the `index`-th one of the requested
/// type is chosen. `None` when the item has no such image.
fn select_image(
    images: &[ItemImageInfo],
    image_type: ImageType,
    index: i32,
) -> Option<&ItemImageInfo> {
    images
        .iter()
        .filter(|i| i.image_type == image_type)
        .nth(usize::try_from(index).unwrap_or(0))
}

/// Serves one [`ItemImageInfo`]'s file, or a `404` when the file is remote or
/// cannot be served.
///
/// This is the shared tail of every image route once an image row is resolved.
/// A remote (`http(s)`) path has no local file to serve without the not-yet-ported
/// image processor, so it is a `404` (the contract's not-found outcome).
async fn serve_image_file(image: &ItemImageInfo, request: Request) -> Result<Response, ApiError> {
    if !image.is_local_file() || image.path.is_empty() {
        return Err(ApiError::NotFound(format!(
            "no local file for image {:?}",
            image.image_type
        )));
    }
    let response = ServeFile::new(&image.path)
        .oneshot(request)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;
    Ok(response.map(Body::new))
}

/// Resolves an item's images, selects the requested one, and serves it.
///
/// The shared body of the item and by-name image routes: `item_id` is the
/// resolved item (real item id or by-name item id), `image_type`/`index` select
/// the image, and a missing item image is a `404`.
async fn serve_item_image(
    state: &AppState,
    item_id: Uuid,
    image_type: ImageType,
    index: i32,
    request: Request,
) -> Result<Response, ApiError> {
    let images = state.library.get_item_images(item_id).await?;
    let image = select_image(&images, image_type, index).ok_or_else(|| {
        ApiError::NotFound(format!(
            "item {item_id} has no {image_type:?} image at {index}"
        ))
    })?;
    serve_image_file(image, request).await
}

/// `GET`/`HEAD /Items/{itemId}/Images/{imageType}` — serve an item's image.
///
/// Port of `ImageController.GetItemImage`. A missing item or image is a `404`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/Images/{imageType}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("imageType" = String, Path, description = "The image type"),
    ),
    responses(
        (status = 200, description = "Image stream returned"),
        (status = 404, description = "Item or image not found"),
    ),
    tag = "hermit"
)]
async fn get_item_image(
    State(state): State<AppState>,
    Path((item_id, image_type)): Path<(Uuid, String)>,
    Query(query): Query<ImageQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    let image_type = parse_image_type(&image_type)?;
    // The item must exist (a missing item is a 404).
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let index = query.image_index.unwrap_or(0);
    serve_item_image(&state, item_id, image_type, index, request).await
}

/// `GET`/`HEAD /Items/{itemId}/Images/{imageType}/{imageIndex}` — indexed variant.
///
/// Port of `ImageController.GetItemImageByIndex` (and the long parametrized
/// alias, whose extra path segments are decorative under original-file serving).
#[utoipa::path(
    get,
    path = "/Items/{itemId}/Images/{imageType}/{imageIndex}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("imageType" = String, Path, description = "The image type"),
        ("imageIndex" = i32, Path, description = "The image index"),
    ),
    responses(
        (status = 200, description = "Image stream returned"),
        (status = 404, description = "Item or image not found"),
    ),
    tag = "hermit"
)]
async fn get_item_image_by_index(
    State(state): State<AppState>,
    Path((item_id, image_type, image_index)): Path<(Uuid, String, i32)>,
    request: Request,
) -> Result<Response, ApiError> {
    let image_type = parse_image_type(&image_type)?;
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    serve_item_image(&state, item_id, image_type, image_index, request).await
}

/// `GET`/`HEAD` for the fully-parametrized item-image alias
/// `/Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}`.
///
/// Port of Jellyfin's positional-parameter image URL (used by some clients that
/// bake the transform into the path). Under original-file serving the trailing
/// `tag`/`format`/size/overlay segments are decorative — only the item, type, and
/// index select the file, so this delegates to the same [`serve_item_image`].
#[allow(clippy::type_complexity)]
async fn get_item_image_parametrized(
    State(state): State<AppState>,
    Path((
        item_id,
        image_type,
        image_index,
        _tag,
        _format,
        _max_width,
        _max_height,
        _percent,
        _unplayed,
    )): Path<(
        Uuid,
        String,
        i32,
        String,
        String,
        String,
        String,
        String,
        String,
    )>,
    request: Request,
) -> Result<Response, ApiError> {
    let image_type = parse_image_type(&image_type)?;
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    serve_item_image(&state, item_id, image_type, image_index, request).await
}

/// `GET /Items/{itemId}/Images` — the item's image infos.
///
/// Port of `ImageController.GetItemImageInfos`, projecting each stored image row
/// into an [`ImageInfo`] DTO. Multi-image types carry a per-type index (mirroring
/// C#'s `AllowsMultipleImages` grouping); single-image types carry `None`. A
/// missing item is a `404`.
#[utoipa::path(
    get,
    path = "/Items/{itemId}/Images",
    params(("itemId" = String, Path, description = "The item id")),
    responses(
        (status = 200, description = "Item images returned"),
        (status = 404, description = "Item not found"),
    ),
    tag = "hermit"
)]
async fn get_item_image_infos(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path(item_id): Path<Uuid>,
) -> Result<axum::Json<Vec<ImageInfo>>, ApiError> {
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    let images = state.library.get_item_images(item_id).await?;
    Ok(axum::Json(project_image_infos(&images)))
}

/// Projects an item's image rows into the [`ImageInfo`] list the API returns.
///
/// Single-instance types (everything except Backdrop/Chapter) get a `None`
/// index; multi-instance types are numbered from `0` in stored order, matching
/// Jellyfin's `GetItemImageInfos` two-pass grouping. Width/height of `0` (the
/// "unknown" sentinel) are nulled out, as in `GetImageInfo`.
fn project_image_infos(images: &[ItemImageInfo]) -> Vec<ImageInfo> {
    let mut list = Vec::with_capacity(images.len());
    for image in images {
        if !allows_multiple_images(image.image_type) {
            list.push(image_info_dto(image, None));
        }
    }
    for multi_type in [ImageType::Backdrop, ImageType::Chapter] {
        for (index, image) in images
            .iter()
            .filter(|i| i.image_type == multi_type)
            .enumerate()
        {
            list.push(image_info_dto(
                image,
                Some(i32::try_from(index).unwrap_or(0)),
            ));
        }
    }
    list
}

/// Whether an image type may have several instances on one item.
///
/// Port of `BaseItem.AllowsMultipleImages`: only Backdrop and Chapter images are
/// multi-instance in the ported surface.
fn allows_multiple_images(image_type: ImageType) -> bool {
    matches!(image_type, ImageType::Backdrop | ImageType::Chapter)
}

/// Builds one [`ImageInfo`] DTO from an image row and its optional index.
fn image_info_dto(image: &ItemImageInfo, image_index: Option<i32>) -> ImageInfo {
    let (width, height) = if image.width > 0 && image.height > 0 {
        (Some(image.width), Some(image.height))
    } else {
        (None, None)
    };
    ImageInfo {
        image_type: image.image_type,
        image_index,
        // The cache tag is computed by the image processor (not yet ported); the
        // stored path already lets a client key its cache, so the tag is omitted.
        image_tag: None,
        path: Some(image.path.clone()),
        blur_hash: image.blur_hash.clone(),
        height,
        width,
        size: 0,
    }
}

/// Serves a by-name item's image after resolving it by kind + name.
///
/// The shared body of the Genres/Studios/Persons/Artists/`MusicGenres` image
/// routes. A missing by-name item is a `404`.
async fn serve_named_image(
    state: &AppState,
    kind: BaseItemKind,
    name: &str,
    image_type: ImageType,
    index: i32,
    request: Request,
) -> Result<Response, ApiError> {
    let item = state
        .library
        .get_named_item(kind, name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("{kind:?} {name}")))?;
    let item_id = Uuid::parse_str(&item.id)
        .map_err(|e| ApiError::NotFound(format!("bad by-name id: {e}")))?;
    serve_item_image(state, item_id, image_type, index, request).await
}

/// Builds a `GET`/`HEAD` handler pair for a by-name image controller of `kind`.
///
/// Each by-name controller (Genres, Studios, Persons, Artists, `MusicGenres`) has
/// the same two routes — `…/{name}/Images/{imageType}` and the `/{imageIndex}`
/// variant — differing only in the resolved [`BaseItemKind`]. This macro emits
/// both handlers so the six controllers reuse one code path instead of copying
/// it, matching the shared `GetImageInternal` fan-in in C#.
macro_rules! by_name_image_handlers {
    ($kind:expr, $base:ident, $indexed:ident) => {
        // The Artists controller exposes only the indexed route, so its `$base`
        // handler is generated-but-unused; the others use both. One `allow`
        // keeps the shared macro simple rather than forking it per controller.
        #[allow(dead_code)]
        async fn $base(
            State(state): State<AppState>,
            Path((name, image_type)): Path<(String, String)>,
            Query(query): Query<ImageQuery>,
            request: Request,
        ) -> Result<Response, ApiError> {
            let image_type = parse_image_type(&image_type)?;
            let index = query.image_index.unwrap_or(0);
            serve_named_image(&state, $kind, &name, image_type, index, request).await
        }

        async fn $indexed(
            State(state): State<AppState>,
            Path((name, image_type, image_index)): Path<(String, String, i32)>,
            request: Request,
        ) -> Result<Response, ApiError> {
            let image_type = parse_image_type(&image_type)?;
            serve_named_image(&state, $kind, &name, image_type, image_index, request).await
        }
    };
}

by_name_image_handlers!(
    BaseItemKind::Genre,
    get_genre_image,
    get_genre_image_by_index
);
by_name_image_handlers!(
    BaseItemKind::MusicGenre,
    get_music_genre_image,
    get_music_genre_image_by_index
);
by_name_image_handlers!(
    BaseItemKind::Studio,
    get_studio_image,
    get_studio_image_by_index
);
by_name_image_handlers!(
    BaseItemKind::Person,
    get_person_image,
    get_person_image_by_index
);
by_name_image_handlers!(
    BaseItemKind::MusicArtist,
    get_artist_image,
    get_artist_image_by_index
);

/// `GET`/`HEAD /UserImage` — serve a user's profile image.
///
/// Port of `ImageController.GetUserImage`. The user is the `userId` query
/// parameter or the authenticated caller; a nil effective user is a `400`, a
/// user with no profile image (or an unresolvable/remote path) is a `404`.
#[utoipa::path(
    get,
    path = "/UserImage",
    params(("userId" = Option<String>, Query, description = "The user id")),
    responses(
        (status = 200, description = "Image stream returned"),
        (status = 400, description = "User id not provided"),
        (status = 404, description = "User or image not found"),
    ),
    tag = "hermit"
)]
async fn get_user_image(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ImageQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    let user: UserEntity =
        crate::handlers::items::resolve_user(&state, &auth, query.user_id).await?;
    let user_id =
        Uuid::parse_str(&user.id).map_err(|e| ApiError::NotFound(format!("bad user id: {e}")))?;
    let image = state
        .users
        .get_profile_image(user_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("user {user_id} has no profile image")))?;
    serve_image_file(&image, request).await
}

/// `DELETE /UserImage` — clear a user's profile image.
///
/// Port of `ImageController.DeleteUserImage`. Resolves the target user (the
/// `userId` query parameter or the caller) and clears its profile-image row via
/// [`UserManager::clear_profile_image`]; a user with no image is still a `204`
/// (idempotent, as in C#). A nil effective user is a `400`, a missing user a
/// `404`.
#[utoipa::path(
    delete,
    path = "/UserImage",
    params(("userId" = Option<String>, Query, description = "The user id")),
    responses(
        (status = 204, description = "Image deleted"),
        (status = 400, description = "User id not provided"),
        (status = 404, description = "User not found"),
    ),
    tag = "hermit"
)]
async fn delete_user_image(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ImageQuery>,
) -> Result<axum::http::StatusCode, ApiError> {
    // `resolve_user_opt` keeps the API-key-with-no-user path a 400 below, while a
    // present-but-missing user is a 404.
    let user = resolve_user_opt(&state, &auth, query.user_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("no user for request".to_owned()))?;
    state.users.clear_profile_image(&user).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/Items/{itemId}/Images",
            get(get_item_image_infos),
        )
        .route(
            "/Items/{itemId}/Images/{imageType}",
            get(get_item_image).head(get_item_image),
        )
        .route(
            "/Items/{itemId}/Images/{imageType}/{imageIndex}",
            get(get_item_image_by_index).head(get_item_image_by_index),
        )
        .route(
            "/Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}",
            get(get_item_image_parametrized).head(get_item_image_parametrized),
        )
        .route(
            "/Genres/{genreName}/Images/{imageType}",
            get(get_genre_image).head(get_genre_image),
        )
        .route(
            "/Genres/{genreName}/Images/{imageType}/{imageIndex}",
            get(get_genre_image_by_index).head(get_genre_image_by_index),
        )
        .route(
            "/MusicGenres/{genreName}/Images/{imageType}",
            get(get_music_genre_image).head(get_music_genre_image),
        )
        .route(
            "/MusicGenres/{genreName}/Images/{imageType}/{imageIndex}",
            get(get_music_genre_image_by_index).head(get_music_genre_image_by_index),
        )
        .route(
            "/Studios/{name}/Images/{imageType}",
            get(get_studio_image).head(get_studio_image),
        )
        .route(
            "/Studios/{name}/Images/{imageType}/{imageIndex}",
            get(get_studio_image_by_index).head(get_studio_image_by_index),
        )
        .route(
            "/Persons/{name}/Images/{imageType}",
            get(get_person_image).head(get_person_image),
        )
        .route(
            "/Persons/{name}/Images/{imageType}/{imageIndex}",
            get(get_person_image_by_index).head(get_person_image_by_index),
        )
        .route(
            "/Artists/{itemId}/Images/{imageType}/{imageIndex}",
            get(get_artist_image_by_index).head(get_artist_image_by_index),
        )
        .route(
            "/UserImage",
            get(get_user_image)
                .head(get_user_image)
                .delete(delete_user_image),
        )
}
