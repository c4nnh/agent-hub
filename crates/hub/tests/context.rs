use ah_common::{ContextEntry, ContextHit};
use ah_hub::{
    auth::issue_token,
    config::{HubConfig, HubMode},
    embed::HashEmbedder,
    routes::app,
    state::AppState,
};
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

fn hub(pool: PgPool, mode: HubMode) -> Router {
    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: mode,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: None,
    };
    app(AppState::new(pool, cfg, Arc::new(HashEmbedder)))
}

async fn user(pool: &PgPool, email: &str, plan: &str) -> String {
    let id = Uuid::new_v4();
    sqlx::query("insert into users (id, email, name, plan) values ($1, $2, 'N', $3)")
        .bind(id)
        .bind(email)
        .bind(plan)
        .execute(pool)
        .await
        .unwrap();
    issue_token(pool, id).await.unwrap()
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn new_channel(app: &Router, token: &str) -> Uuid {
    let (status, body) = call(
        app,
        Method::POST,
        "/api/channels",
        token,
        Some(json!({"name": "dev"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let v: Value = serde_json::from_slice(&body).unwrap();
    v["id"].as_str().unwrap().parse().unwrap()
}

async fn upsert(app: &Router, token: &str, ch: Uuid, body: Value) -> (StatusCode, ContextEntry) {
    let (status, bytes) = call(
        app,
        Method::POST,
        &format!("/api/channels/{ch}/context"),
        token,
        Some(body),
    )
    .await;
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[sqlx::test(migrations = "./migrations")]
async fn create_starts_at_version_1(pool: PgPool) {
    let t = user(&pool, "a@x.com", "free").await;
    let app = hub(pool.clone(), HubMode::Open);
    let ch = new_channel(&app, &t).await;
    let (status, e) = upsert(&app, &t, ch, json!({"title": "T", "body": "B"})).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(e.version, 1);
    assert_eq!(e.updated_by, "a@x.com");
    let has_embedding: bool =
        sqlx::query_scalar("select embedding is not null from context_entries where id = $1")
            .bind(e.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(has_embedding);
    let (status, bytes) = call(
        &app,
        Method::GET,
        &format!("/api/context/{}", e.id),
        &t,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<ContextEntry>(&bytes).unwrap(), e);
    let (status, bytes) = call(
        &app,
        Method::GET,
        &format!("/api/channels/{ch}/context"),
        &t,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Vec<Value>>(&bytes).unwrap().len(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn stale_base_version_409_returns_current_entry(pool: PgPool) {
    let t = user(&pool, "a@x.com", "free").await;
    let app = hub(pool, HubMode::Open);
    let ch = new_channel(&app, &t).await;
    let (_, e) = upsert(&app, &t, ch, json!({"title": "T", "body": "B"})).await;
    let (status, v2) = upsert(
        &app,
        &t,
        ch,
        json!({"id": e.id, "title": "T2", "body": "B2", "base_version": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v2.version, 2);
    let (status, current) = upsert(
        &app,
        &t,
        ch,
        json!({"id": e.id, "title": "T3", "body": "B3", "base_version": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(current, v2);
    // missing base_version with id is a 400
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/channels/{ch}/context"),
        &t,
        Some(json!({"id": e.id, "title": "T", "body": "B"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn same_base_version_second_writer_409(pool: PgPool) {
    let t = user(&pool, "a@x.com", "free").await;
    let app = hub(pool, HubMode::Open);
    let ch = new_channel(&app, &t).await;
    let (_, e) = upsert(&app, &t, ch, json!({"title": "T", "body": "B"})).await;
    let (s1, _) = upsert(
        &app,
        &t,
        ch,
        json!({"id": e.id, "title": "A", "body": "A", "base_version": 1}),
    )
    .await;
    let (s2, _) = upsert(
        &app,
        &t,
        ch,
        json!({"id": e.id, "title": "B", "body": "B", "base_version": 1}),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn search_finds_exact_text_first(pool: PgPool) {
    let t = user(&pool, "a@x.com", "free").await;
    let app = hub(pool, HubMode::Open);
    let ch = new_channel(&app, &t).await;
    for (title, body) in [
        ("alpha", "first body"),
        ("beta", "second body"),
        ("gamma", "third body"),
    ] {
        upsert(&app, &t, ch, json!({"title": title, "body": body})).await;
    }
    let (status, bytes) = call(
        &app,
        Method::POST,
        &format!("/api/channels/{ch}/context/search"),
        &t,
        Some(json!({"query": "beta\nsecond body", "k": 3})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hits: Vec<ContextHit> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].entry.title, "beta");
    assert!(hits[0].score > 0.999);
}

#[sqlx::test(migrations = "./migrations")]
async fn vietnamese_context_embeds_and_roundtrips(pool: PgPool) {
    let t = user(&pool, "a@x.com", "free").await;
    let app = hub(pool, HubMode::Open);
    let ch = new_channel(&app, &t).await;
    let (status, e) = upsert(
        &app,
        &t,
        ch,
        json!({"title": "Quy tắc đặt tên", "body": "Dùng tiếng Việt có dấu: Nguyễn Văn Ạ"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, bytes) = call(
        &app,
        Method::GET,
        &format!("/api/context/{}", e.id),
        &t,
        None,
    )
    .await;
    let got: ContextEntry = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(got.title, "Quy tắc đặt tên");
    assert_eq!(got.body, "Dùng tiếng Việt có dấu: Nguyễn Văn Ạ");
}

#[sqlx::test(migrations = "./migrations")]
async fn free_user_hosted_mode_403_paid_required(pool: PgPool) {
    let t = user(&pool, "a@x.com", "free").await;
    let paid = user(&pool, "p@x.com", "paid").await;
    let app = hub(pool, HubMode::Hosted);
    let ch = new_channel(&app, &t).await;
    let id = Uuid::new_v4();
    let search = format!("/api/channels/{ch}/context/search");
    let list = format!("/api/channels/{ch}/context");
    let one = format!("/api/context/{id}");
    let cases = [
        (Method::GET, list.clone(), None),
        (Method::GET, one, None),
        (Method::POST, list, Some(json!({"title": "T", "body": "B"}))),
        (Method::POST, search, Some(json!({"query": "q"}))),
    ];
    for (m, uri, b) in cases {
        let (status, bytes) = call(&app, m, &uri, &t, b).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["code"], "paid_required");
        assert_eq!(
            v["message"],
            "Context store requires a paid plan on the hosted hub"
        );
    }
    // paid user passes the gate (not a member of the free user's channel -> 403/404 is not paid_required)
    let pch = new_channel(&app, &paid).await;
    let (status, _) = upsert(&app, &paid, pch, json!({"title": "T", "body": "B"})).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrations = "./migrations")]
async fn free_user_open_mode_allowed(pool: PgPool) {
    let t = user(&pool, "a@x.com", "free").await;
    let app = hub(pool, HubMode::Open);
    let ch = new_channel(&app, &t).await;
    let (status, _) = upsert(&app, &t, ch, json!({"title": "T", "body": "B"})).await;
    assert_eq!(status, StatusCode::CREATED);
}
