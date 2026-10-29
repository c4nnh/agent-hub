use ah_common::{AuthPollResponse, MeResponse};
use ah_hub::{
    config::{HubConfig, HubMode},
    routes::app,
    state::AppState,
};
use axum::{
    Json, Router,
    body::Body,
    http::{Method, Request, StatusCode},
    routing::{get, post},
};
use sqlx::PgPool;
use tower::ServiceExt;

const C1: &str = "11111111-1111-1111-1111-111111111111";
const C2: &str = "22222222-2222-2222-2222-222222222222";

async fn fake_google() -> String {
    let router = Router::new()
        .route(
            "/token",
            post(|| async { Json(serde_json::json!({"access_token": "at"})) }),
        )
        .route(
            "/userinfo",
            get(|| async { Json(serde_json::json!({"email": "a@b.co", "name": "Ann"})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

async fn hub(pool: PgPool) -> Router {
    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: HubMode::Open,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: Some(fake_google().await),
    };
    app(AppState::new(
        pool,
        cfg,
        std::sync::Arc::new(ah_hub::embed::HashEmbedder),
    ))
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    body: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = bearer {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let res = app
        .clone()
        .oneshot(
            req.body(Body::from(body.unwrap_or("").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn login(app: &Router, code: &str) -> AuthPollResponse {
    let (s, _) = call(
        app,
        Method::POST,
        "/api/auth/cli/start",
        None,
        Some(&format!(r#"{{"code":"{code}"}}"#)),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let poll = format!("/api/auth/cli/poll?code={code}");
    let (s, _) = call(app, Method::GET, &poll, None, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, body) = call(
        app,
        Method::GET,
        &format!("/auth/callback?code=gc&state={code}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, b"You can close this tab.");
    let (s, body) = call(app, Method::GET, &poll, None, None).await;
    assert_eq!(s, StatusCode::OK);
    serde_json::from_slice(&body).unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn cli_flow_issues_token(pool: PgPool) {
    let app = hub(pool).await;
    let r = login(&app, C1).await;
    assert_eq!(r.email, "a@b.co");
    assert_eq!(r.token.len(), 43);
    // The token is handed out once.
    let (s, _) = call(
        &app,
        Method::GET,
        &format!("/api/auth/cli/poll?code={C1}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // Callback for an unknown state is rejected.
    let (s, _) = call(
        &app,
        Method::GET,
        "/auth/callback?code=gc&state=nope",
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn auth_start_redirects_to_google(pool: PgPool) {
    let app = hub(pool).await;
    let req = Request::builder()
        .uri("/auth/start?code=c9")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert!(res.status().is_redirection());
    let loc = res.headers()["location"].to_str().unwrap();
    assert!(
        loc.contains("/auth?") && loc.contains("state=c9") && loc.contains("client_id=cid"),
        "{loc}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn bearer_rejects_unknown_token(pool: PgPool) {
    let app = hub(pool).await;
    let (s, _) = call(&app, Method::GET, "/api/me", Some("bogus"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&app, Method::GET, "/api/me", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn logout_revokes_token(pool: PgPool) {
    let app = hub(pool).await;
    let t = login(&app, C2).await.token;
    let (s, body) = call(&app, Method::GET, "/api/me", Some(&t), None).await;
    assert_eq!(s, StatusCode::OK);
    let me: MeResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        (me.email.as_str(), me.name.as_str(), me.plan.as_str()),
        ("a@b.co", "Ann", "free")
    );
    let (s, _) = call(&app, Method::POST, "/api/auth/logout", Some(&t), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&app, Method::GET, "/api/me", Some(&t), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn short_code_rejected_400(pool: PgPool) {
    let app = hub(pool).await;
    let short = format!(r#"{{"code":"{}"}}"#, "a".repeat(31));
    let (s, _) = call(
        &app,
        Method::POST,
        "/api/auth/cli/start",
        None,
        Some(&short),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let ok = format!(r#"{{"code":"{}"}}"#, "a".repeat(32));
    let (s, _) = call(&app, Method::POST, "/api/auth/cli/start", None, Some(&ok)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_auth_request_poll_204_and_token_revoked(pool: PgPool) {
    let app = hub(pool.clone()).await;
    // Completed login whose token was never polled.
    let (s, _) = call(
        &app,
        Method::POST,
        "/api/auth/cli/start",
        None,
        Some(&format!(r#"{{"code":"{C1}"}}"#)),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(
        &app,
        Method::GET,
        &format!("/auth/callback?code=gc&state={C1}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let tokens: i64 = sqlx::query_scalar("select count(*) from tokens")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tokens, 1);
    // Pending request that expires before the callback.
    let (s, _) = call(
        &app,
        Method::POST,
        "/api/auth/cli/start",
        None,
        Some(&format!(r#"{{"code":"{C2}"}}"#)),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    sqlx::query("update auth_requests set created_at = now() - interval '11 minutes'")
        .execute(&pool)
        .await
        .unwrap();

    let (s, _) = call(
        &app,
        Method::GET,
        &format!("/auth/callback?code=gc&state={C2}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(
        &app,
        Method::GET,
        &format!("/api/auth/cli/poll?code={C1}"),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let tokens: i64 = sqlx::query_scalar("select count(*) from tokens")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tokens, 0);
    let reqs: i64 = sqlx::query_scalar("select count(*) from auth_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reqs, 0);
}
