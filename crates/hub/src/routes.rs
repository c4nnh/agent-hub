use crate::{auth, channels, context, messages, state::AppState, ws};
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
        .route("/api/channels", get(channels::list).post(channels::create))
        .route("/api/channels/{id}/leave", post(channels::leave))
        .route("/api/channels/{id}/invitations", post(channels::invite))
        .route(
            "/api/channels/{id}/messages",
            get(messages::list).post(messages::post),
        )
        .route("/api/channels/{id}/agents", get(ws::agents))
        .route(
            "/api/channels/{id}/context",
            get(context::list).post(context::upsert),
        )
        .route("/api/channels/{id}/context/search", post(context::search))
        .route("/api/context/{id}", get(context::get))
        .route("/api/ws", get(ws::handler))
        .route("/api/invitations", get(channels::list_invitations))
        .route("/api/invitations/{id}/respond", post(channels::respond))
        .route("/auth/start", get(auth::google_start))
        .route("/auth/confirm", post(auth::google_confirm))
        .route("/auth/callback", get(auth::google_callback))
        .with_state(state)
}
