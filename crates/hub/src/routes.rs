use crate::{auth, state::AppState};
use axum::{
    Router,
    routing::{get, post},
};

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/api/auth/cli/start", post(auth::cli_start))
        .route("/api/auth/cli/poll", get(auth::cli_poll))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/me", get(auth::me))
        .route("/auth/start", get(auth::google_start))
        .route("/auth/callback", get(auth::google_callback))
        .with_state(state)
}
