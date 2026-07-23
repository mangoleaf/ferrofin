//! Real ported handlers for the First-Light routes.
//!
//! Each submodule mirrors one `Jellyfin.Api` controller and holds axum handlers
//! that call the [`AppState`](crate::state::AppState) manager traits, project
//! results through [`DtoService`](hermit_traits::dto::DtoService), and return
//! the wire DTOs from `hermit-model`. These are the routes with *real* behaviour
//! (the rest of the contract stays on the shared `not_implemented` `501` stub);
//! [`register`] mounts them over their stub entries.
//!
//! Handlers behind Jellyfin's `[Authorize]` policy take the
//! [`RequireAuth`](crate::auth::RequireAuth) extractor (a missing/invalid token
//! becomes `401`); public routes read the (possibly anonymous)
//! [`AuthorizationInfo`](hermit_traits::options::AuthorizationInfo) extension set
//! by the auth-context middleware.

use axum::Router;

use crate::state::AppState;

pub mod images;
pub mod items;
pub mod media_info;
pub mod system;
pub mod user_views;
pub mod users;
pub mod videos;

/// The `(method, axum_path)` pairs served by a real handler in this unit.
///
/// [`create_router`](crate::router::create_router) skips the shared `501` stub
/// for each of these so the real handler is the sole route for that
/// `(method, path)` (axum panics on two handlers for the same method+path). The
/// paths are the axum-normalized forms (they already use axum's `{param}`
/// capture syntax and match the vendored-contract normalization).
pub const REAL_ROUTES: &[(&str, &str)] = &[
    ("get", "/System/Info"),
    ("get", "/System/Info/Public"),
    ("post", "/Users/AuthenticateByName"),
    ("get", "/Users/Me"),
    ("get", "/UserViews"),
    ("get", "/Items"),
    ("get", "/Items/{itemId}"),
    ("get", "/Items/{itemId}/PlaybackInfo"),
    ("post", "/Items/{itemId}/PlaybackInfo"),
    ("get", "/Videos/{itemId}/stream"),
    ("head", "/Videos/{itemId}/stream"),
    ("get", "/Items/{itemId}/Images/{imageType}"),
    ("head", "/Items/{itemId}/Images/{imageType}"),
];

/// Mounts every real First-Light handler onto `router`, overriding the matching
/// `501` stub entries registered from the vendored contract table.
///
/// Called by [`create_router`](crate::router::create_router) after the stub loop
/// so a `(method, path)` with a real handler wins over its stub.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    let router = system::register(router);
    let router = users::register(router);
    let router = user_views::register(router);
    let router = items::register(router);
    let router = media_info::register(router);
    let router = videos::register(router);
    images::register(router)
}
