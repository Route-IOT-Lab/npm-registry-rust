//! Router assembly. Each module owns one surface:
//!
//! * `npm`  — the registry protocol consumed by the npm CLI
//! * `api`  — the JSON API backing the dashboard
//! * `ui`   — the embedded single-page dashboard

pub mod api;
pub mod npm;
pub mod ui;

use axum::Router;

use crate::state::AppState;

/// Build the full application router. Layers are applied by the caller so that
/// `main.rs` stays the single place where cross-cutting concerns are wired.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .merge(ui::routes())
        .merge(api::routes())
        .merge(npm::routes())
        // Anything that is not an API/UI/`/-/`-prefixed route is a package path.
        .fallback(npm::dispatch)
        .with_state(state)
}
