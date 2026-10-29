use ah_hub::{config::HubConfig, routes::app, state::AppState};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let cfg = HubConfig::from_env();
    let pool = sqlx::PgPool::connect(&cfg.database_url)
        .await
        .expect("connect database");
    sqlx::migrate!().run(&pool).await.expect("run migrations");
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("bind");
    axum::serve(listener, app(AppState { pool, cfg }))
        .await
        .expect("serve");
}
