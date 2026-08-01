//! `ImageController` — item, by-name, and user image serving (read side).
//!
//! Ports the *read* surface of Jellyfin's `ImageController`: resolve an item (or
//! a by-name genre/studio/person/artist, or a user profile), find the requested
//! [`ItemImageInfo`] by type (and optional index), and serve its file. The image
//! rows come from [`LibraryManager::get_item_images`] /
//! [`UserManager::get_profile_image`], both backed by real `hermit-db` reads.
//!
//! ## Port scope — processed serving
//!
//! Every image request runs through the wired
//! [`ImageProcessor`](hermit_traits::drawing::ImageProcessor) (the real
//! `image`-crate encoder at the composition root): the `maxWidth`/`maxHeight`/
//! `width`/`height`/`fillWidth`/`fillHeight`/`format`/`blur` parameters resize and
//! format-convert the bytes, and the positional-parameter URL lifts its
//! `format`/`maxWidth`/`maxHeight` path segments into the same transform. A plain
//! request (or an unwired processor) serves the **stored original** untouched. The
//! overlay effects (`percentPlayed`/`unplayedCount`) select the file but are not
//! drawn.
//!
//! A missing item (or by-name item, or user), or an item with no image of the
//! requested type/index, or an image whose file is remote/absent, is a `404` —
//! exactly the contract's not-found outcome. `HEAD` shares each handler with
//! `GET` (the file service is `HEAD`-aware).
//!
//! ## Write side
//!
//! The item-image *write* routes are ported too:
//! `POST`/`DELETE /Items/{itemId}/Images/{imageType}[/{imageIndex}]`. The upload
//! body is base64-encoded image bytes with the MIME type in the `Content-Type`
//! header (Jellyfin's `[AcceptsImageFile]`), decoded via the shared
//! [`image_upload`](crate::handlers::image_upload) helpers and handed to
//! [`ProviderManager::save_image`]/[`ProviderManager::delete_image`]. These call
//! through to the image-store seam; the concrete shell manager reports the image
//! pipeline as deferred (as it does for `queue_refresh`), so the request shape,
//! `400`/`404` validation, and the `save_image`/`delete_image` contract here are
//! final while the on-disk pipeline is a later wave.

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use hermit_db::entities::users::UserEntity;
use hermit_model::data::BaseItemKind;
use hermit_model::drawing::ImageFormat;
use hermit_model::dto::ImageInfo;
use hermit_model::entities::ImageType;
use hermit_traits::options::{ImageProcessingOptions, ItemImageInfo};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use uuid::Uuid;

use crate::auth::RequireAuth;
use crate::error::ApiError;
use crate::handlers::image_upload::{
    decode_base64, image_extension_from_content_type, image_mime_from_content_type,
};
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
/// Every field is optional and mirrors Jellyfin's `GetImage*` signature; the
/// size/format/blur fields drive the [`ImageProcessor`](hermit_traits::drawing::ImageProcessor)
/// transform, and `user_id` (for the user-image route) selects which file is served.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImageQuery {
    /// The target user (user-image route only); defaults to the caller.
    #[serde(default)]
    user_id: Option<Uuid>,
    /// Optional explicit image index (query-string form of the route segment).
    #[serde(default)]
    image_index: Option<i32>,
    /// The requested output format (`jpg`/`png`/`webp`/…); the original's format
    /// is kept when omitted.
    #[serde(default)]
    format: Option<String>,
    /// Scale so neither dimension exceeds these (aspect-preserving).
    #[serde(default)]
    max_width: Option<i32>,
    #[serde(default)]
    max_height: Option<i32>,
    /// Scale to an exact width/height (aspect-preserving on the given axis).
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    height: Option<i32>,
    /// Scale to fill an exact box (may crop).
    #[serde(default)]
    fill_width: Option<i32>,
    #[serde(default)]
    fill_height: Option<i32>,
    /// JPEG/WebP quality (1–100); the encoder default is used when omitted.
    #[serde(default)]
    quality: Option<i32>,
    /// Gaussian blur radius.
    #[serde(default)]
    blur: Option<i32>,
}

impl ImageQuery {
    /// Whether the request asks for any transform (resize/format/blur); a plain
    /// request serves the stored original untouched.
    fn wants_transform(&self) -> bool {
        self.format.is_some()
            || self.max_width.is_some()
            || self.max_height.is_some()
            || self.width.is_some()
            || self.height.is_some()
            || self.fill_width.is_some()
            || self.fill_height.is_some()
            || self.blur.is_some()
    }
}

/// The output formats the encoder can produce, used as the default accepted set
/// when the request names no explicit `format` (lets the processor keep a
/// compatible original or convert as needed).
fn default_output_formats() -> Vec<ImageFormat> {
    vec![ImageFormat::Webp, ImageFormat::Jpg, ImageFormat::Png]
}

/// Parses a Jellyfin image-format string into an [`ImageFormat`].
fn parse_image_format(format: &str) -> Option<ImageFormat> {
    match format.trim().to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some(ImageFormat::Jpg),
        "png" => Some(ImageFormat::Png),
        "webp" => Some(ImageFormat::Webp),
        "gif" => Some(ImageFormat::Gif),
        "bmp" => Some(ImageFormat::Bmp),
        _ => None,
    }
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
async fn serve_image_file(
    state: &AppState,
    item_id: Uuid,
    image: &ItemImageInfo,
    index: i32,
    query: &ImageQuery,
    request: Request,
) -> Result<Response, ApiError> {
    if !image.is_local_file() || image.path.is_empty() {
        return Err(ApiError::NotFound(format!(
            "no local file for image {:?}",
            image.image_type
        )));
    }

    // Run the image through the processor for the requested resize/format. It
    // short-circuits to the original file when nothing is asked for (or the source
    // already matches), so a plain request still serves the untouched original.
    let mut serve_path = image.path.clone();
    let mut content_type: Option<String> = None;
    if let (true, Some(processor)) = (query.wants_transform(), state.image_processor.as_ref()) {
        let options = ImageProcessingOptions {
            item_id,
            image: image.clone(),
            image_index: index,
            width: query.width,
            height: query.height,
            max_width: query.max_width,
            max_height: query.max_height,
            fill_width: query.fill_width,
            fill_height: query.fill_height,
            blur: query.blur,
            quality: query.quality.unwrap_or(90),
            supported_output_formats: query
                .format
                .as_deref()
                .and_then(parse_image_format)
                .map_or_else(default_output_formats, |f| vec![f]),
            ..Default::default()
        };
        let processed = processor.process_image(&options).await?;
        serve_path = processed.path;
        content_type = processed.mime_type;
    }

    let mut response = ServeFile::new(&serve_path)
        .oneshot(request)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?
        .map(Body::new);
    // The processor may have converted the format, so trust its MIME over the
    // extension `ServeFile` guessed.
    if let Some(mime) = content_type
        && let Ok(value) = axum::http::HeaderValue::from_str(&mime)
    {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, value);
    }
    Ok(response)
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
    query: &ImageQuery,
    request: Request,
) -> Result<Response, ApiError> {
    let images = state.library.get_item_images(item_id).await?;
    let image = select_image(&images, image_type, index).ok_or_else(|| {
        ApiError::NotFound(format!(
            "item {item_id} has no {image_type:?} image at {index}"
        ))
    })?;
    serve_image_file(state, item_id, image, index, query, request).await
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
    serve_item_image(&state, item_id, image_type, index, &query, request).await
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
    Query(query): Query<ImageQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    let image_type = parse_image_type(&image_type)?;
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    serve_item_image(&state, item_id, image_type, image_index, &query, request).await
}

/// `GET`/`HEAD` for the fully-parametrized item-image alias
/// `/Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}`.
///
/// Port of Jellyfin's positional-parameter image URL (used by some clients that
/// bake the transform into the path). The `format`/`maxWidth`/`maxHeight` path
/// segments are lifted into an [`ImageQuery`] so the transform is applied just as
/// the query-string form would; `tag`/`percentPlayed`/`unplayedCount` are honored
/// for the file selection but their overlay effects are not drawn.
#[allow(clippy::type_complexity)]
async fn get_item_image_parametrized(
    State(state): State<AppState>,
    Path((
        item_id,
        image_type,
        image_index,
        _tag,
        format,
        max_width,
        max_height,
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
    // Lift the positional transform segments into a query (empty/`0` segments mean
    // "unset", matching how clients bake a partial transform into the path).
    let query = ImageQuery {
        format: Some(format).filter(|f| !f.is_empty() && !f.eq_ignore_ascii_case("0")),
        max_width: max_width.parse().ok().filter(|w| *w > 0),
        max_height: max_height.parse().ok().filter(|h| *h > 0),
        ..ImageQuery::default()
    };
    serve_item_image(&state, item_id, image_type, image_index, &query, request).await
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
    // Size is the on-disk file length, stat'd at projection time (Jellyfin's GetImageInfo does the
    // same: `length = fileInfo.Length` for local files). Remote/missing files report 0.
    let size = if image.is_local_file() {
        std::fs::metadata(&image.path)
            .ok()
            .and_then(|m| i64::try_from(m.len()).ok())
            .unwrap_or(0)
    } else {
        0
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
        size,
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
    query: &ImageQuery,
    request: Request,
) -> Result<Response, ApiError> {
    let item = state
        .library
        .get_named_item(kind, name)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("{kind:?} {name}")))?;
    let item_id = Uuid::parse_str(&item.id)
        .map_err(|e| ApiError::NotFound(format!("bad by-name id: {e}")))?;
    serve_item_image(state, item_id, image_type, index, query, request).await
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
            serve_named_image(&state, $kind, &name, image_type, index, &query, request).await
        }

        async fn $indexed(
            State(state): State<AppState>,
            Path((name, image_type, image_index)): Path<(String, String, i32)>,
            Query(query): Query<ImageQuery>,
            request: Request,
        ) -> Result<Response, ApiError> {
            let image_type = parse_image_type(&image_type)?;
            serve_named_image(
                &state,
                $kind,
                &name,
                image_type,
                image_index,
                &query,
                request,
            )
            .await
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
    serve_image_file(&state, user_id, &image, 0, &query, request).await
}

/// `POST /UserImage` — upload a user's profile image.
///
/// Port of `ImageController.PostUserImage`. Resolves the target user (the `userId`
/// query parameter or the caller; a missing user is `404`, a nil effective user a
/// `400`), validates the `Content-Type` is an image type (`400` otherwise),
/// base64-decodes the body, and stores it via
/// [`UserManager::save_profile_image`], which clears any prior image and persists
/// the user. Returns `204`.
#[utoipa::path(
    post,
    path = "/UserImage",
    params(("userId" = Option<String>, Query, description = "The user id")),
    responses(
        (status = 204, description = "Image updated"),
        (status = 400, description = "Incorrect content type or user id not provided"),
        (status = 404, description = "User not found"),
    ),
    tag = "hermit"
)]
async fn post_user_image(
    State(state): State<AppState>,
    RequireAuth(auth): RequireAuth,
    Query(query): Query<ImageQuery>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    let user = resolve_user_opt(&state, &auth, query.user_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("no user for request".to_owned()))?;
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let mime = image_mime_from_content_type(content_type)
        .ok_or_else(|| ApiError::BadRequest("Incorrect ContentType.".to_owned()))?;
    let extension = image_extension_from_content_type(content_type).unwrap_or_default();
    let bytes = decode_base64(body.trim())
        .ok_or_else(|| ApiError::BadRequest("image data is not valid base64".to_owned()))?;
    state
        .users
        .save_profile_image(&user, &bytes, &mime, &extension)
        .await?;
    Ok(StatusCode::NO_CONTENT)
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

/// Reads the raw base64 upload body and its image MIME type, then saves it.
///
/// Shared tail of `SetItemImage`/`SetItemImageByIndex`: validates the
/// `Content-Type` is an image type (`400` otherwise), base64-decodes the body
/// (`400` on invalid base64), and stores the bytes via
/// [`ProviderManager::save_image`] at `image_type`/`index`. Returns `204`.
async fn save_item_image(
    state: &AppState,
    item_id: Uuid,
    image_type: ImageType,
    image_index: Option<i32>,
    content_type: Option<&str>,
    body: &str,
) -> Result<StatusCode, ApiError> {
    let mime = image_mime_from_content_type(content_type)
        .ok_or_else(|| ApiError::BadRequest("Incorrect ContentType.".to_owned()))?;
    let bytes = decode_base64(body.trim())
        .ok_or_else(|| ApiError::BadRequest("image data is not valid base64".to_owned()))?;
    state
        .providers
        .save_image(item_id, &bytes, &mime, image_type, image_index)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /Items/{itemId}/Images/{imageType}` — upload an item's image.
///
/// Port of `ImageController.SetItemImage`. A missing item is `404`, a non-image
/// `Content-Type` (or non-base64 body) is `400`, and on success the image is
/// saved and the handler returns `204`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/Images/{imageType}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("imageType" = String, Path, description = "The image type"),
    ),
    responses(
        (status = 204, description = "Image saved"),
        (status = 400, description = "Incorrect content type"),
        (status = 404, description = "Item not found"),
    ),
    tag = "hermit"
)]
async fn set_item_image(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, image_type)): Path<(Uuid, String)>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    let image_type = parse_image_type(&image_type)?;
    require_item(&state, item_id).await?;
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    save_item_image(&state, item_id, image_type, None, content_type, &body).await
}

/// `POST /Items/{itemId}/Images/{imageType}/{imageIndex}` — indexed upload.
///
/// Port of `ImageController.SetItemImageByIndex`.
#[utoipa::path(
    post,
    path = "/Items/{itemId}/Images/{imageType}/{imageIndex}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("imageType" = String, Path, description = "The image type"),
        ("imageIndex" = i32, Path, description = "The image index"),
    ),
    responses(
        (status = 204, description = "Image saved"),
        (status = 400, description = "Incorrect content type"),
        (status = 404, description = "Item not found"),
    ),
    tag = "hermit"
)]
async fn set_item_image_by_index(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, image_type, image_index)): Path<(Uuid, String, i32)>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    let image_type = parse_image_type(&image_type)?;
    require_item(&state, item_id).await?;
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    save_item_image(
        &state,
        item_id,
        image_type,
        Some(image_index),
        content_type,
        &body,
    )
    .await
}

/// `DELETE /Items/{itemId}/Images/{imageType}` — delete an item's image.
///
/// Port of `ImageController.DeleteItemImage`. A missing item is `404`; the image
/// index comes from the optional `imageIndex` query parameter (default `0`, as in
/// C#'s `imageIndex ?? 0`). On success the handler returns `204`.
#[utoipa::path(
    delete,
    path = "/Items/{itemId}/Images/{imageType}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("imageType" = String, Path, description = "The image type"),
        ("imageIndex" = Option<i32>, Query, description = "The image index"),
    ),
    responses(
        (status = 204, description = "Image deleted"),
        (status = 404, description = "Item not found"),
    ),
    tag = "hermit"
)]
async fn delete_item_image(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, image_type)): Path<(Uuid, String)>,
    Query(query): Query<ImageQuery>,
) -> Result<StatusCode, ApiError> {
    let image_type = parse_image_type(&image_type)?;
    require_item(&state, item_id).await?;
    let index = query.image_index.unwrap_or(0);
    state
        .providers
        .delete_image(item_id, image_type, Some(index))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /Items/{itemId}/Images/{imageType}/{imageIndex}` — indexed delete.
///
/// Port of `ImageController.DeleteItemImageByIndex`.
#[utoipa::path(
    delete,
    path = "/Items/{itemId}/Images/{imageType}/{imageIndex}",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("imageType" = String, Path, description = "The image type"),
        ("imageIndex" = i32, Path, description = "The image index"),
    ),
    responses(
        (status = 204, description = "Image deleted"),
        (status = 404, description = "Item not found"),
    ),
    tag = "hermit"
)]
async fn delete_item_image_by_index(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, image_type, image_index)): Path<(Uuid, String, i32)>,
) -> Result<StatusCode, ApiError> {
    let image_type = parse_image_type(&image_type)?;
    require_item(&state, item_id).await?;
    state
        .providers
        .delete_image(item_id, image_type, Some(image_index))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The query string of the image-reorder route: the destination index.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateImageIndexQuery {
    /// The new image index the image at `imageIndex` should move to.
    new_index: i32,
}

/// `POST /Items/{itemId}/Images/{imageType}/{imageIndex}/Index` — reorder an
/// item image.
///
/// Port of `ImageController.UpdateItemImageIndex`. A missing item is `404`; an
/// image type that does not allow multiple images (anything but Backdrop and
/// Chapter, per C# `AllowsMultipleImages`) is a `400`; otherwise the image at
/// `imageIndex` is swapped with the one at `newIndex` and the handler returns
/// `204` (an out-of-range index is a faithful no-op, still `204`).
#[utoipa::path(
    post,
    path = "/Items/{itemId}/Images/{imageType}/{imageIndex}/Index",
    params(
        ("itemId" = String, Path, description = "The item id"),
        ("imageType" = String, Path, description = "The image type"),
        ("imageIndex" = i32, Path, description = "The old image index"),
        ("newIndex" = i32, Query, description = "The new image index"),
    ),
    responses(
        (status = 204, description = "Image index updated"),
        (status = 400, description = "Image type does not allow reordering"),
        (status = 404, description = "Item not found"),
    ),
    tag = "hermit"
)]
async fn update_item_image_index(
    State(state): State<AppState>,
    RequireAuth(_auth): RequireAuth,
    Path((item_id, image_type, image_index)): Path<(Uuid, String, i32)>,
    Query(query): Query<UpdateImageIndexQuery>,
) -> Result<StatusCode, ApiError> {
    let image_type = parse_image_type(&image_type)?;
    require_item(&state, item_id).await?;
    state
        .library
        .swap_images(item_id, image_type, image_index, query.new_index)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resolves an item, mapping a missing one to a `404`.
///
/// The shared item-existence guard the write handlers run before mutating.
async fn require_item(state: &AppState, item_id: Uuid) -> Result<(), ApiError> {
    state
        .library
        .get_item_by_id(item_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("item {item_id}")))?;
    Ok(())
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
            get(get_item_image)
                .head(get_item_image)
                .post(set_item_image)
                .delete(delete_item_image),
        )
        .route(
            "/Items/{itemId}/Images/{imageType}/{imageIndex}",
            get(get_item_image_by_index)
                .head(get_item_image_by_index)
                .post(set_item_image_by_index)
                .delete(delete_item_image_by_index),
        )
        .route(
            "/Items/{itemId}/Images/{imageType}/{imageIndex}/Index",
            axum::routing::post(update_item_image_index),
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
                .post(post_user_image)
                .delete(delete_user_image),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn img(path: String) -> ItemImageInfo {
        ItemImageInfo {
            path,
            image_type: ImageType::Primary,
            date_modified: Utc::now(),
            width: 0,
            height: 0,
            blur_hash: None,
        }
    }

    #[test]
    fn image_info_size_is_file_length_for_local_files() {
        // Regression (parity): Size was hardcoded to 0; Jellyfin reports the on-disk length.
        let dir = std::env::temp_dir().join(format!("hermit-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("poster.jpg");
        std::fs::write(&file, b"0123456789").unwrap(); // 10 bytes
        let dto = image_info_dto(&img(file.to_string_lossy().into_owned()), None);
        assert_eq!(dto.size, 10);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn image_info_size_is_zero_for_remote_or_missing() {
        assert_eq!(image_info_dto(&img("https://x/y.jpg".into()), None).size, 0);
        assert_eq!(
            image_info_dto(&img("/no/such/file.jpg".into()), None).size,
            0
        );
    }
}
