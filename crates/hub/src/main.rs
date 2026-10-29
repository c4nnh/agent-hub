use ah_hub::{config::HubConfig, embed::FastEmbedder, routes::app, state::AppState};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let cfg = HubConfig::from_env();
    let pool = sqlx::PgPool::connect(&cfg.database_url)
        .await
        .expect("connect database");
    sqlx::migrate!().run(&pool).await.expect("run migrations");
    let embedder = tokio::task::spawn_blocking(FastEmbedder::new)
        .await
        .expect("embedder task")
        .expect("load embedding model");
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind");
    axum::serve(
        listener,
        app(AppState::new(pool, cfg, std::sync::Arc::new(embedder))),
    )
    .await
    .expect("serve");
}
