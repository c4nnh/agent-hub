use ah_common::{Message, SenderType};
use ah_hub::{
    auth::issue_token,
    config::{HubConfig, HubMode},
    routes::app,
    state::AppState,
};
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

fn hub(pool: PgPool) -> Router {
    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: HubMode::Open,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: None,
    };
    app(AppState::new(
        pool,
        cfg,
        std::sync::Arc::new(ah_hub::embed::HashEmbedder),
    ))
}

async fn user(pool: &PgPool, email: &str) -> String {
    let id = Uuid::new_v4();
    sqlx::query("insert into users (id, email, name) values ($1, $2, 'N')")
        .bind(id)
        .bind(email)
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
    body: Option<String>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.unwrap_or_default())).unwrap())
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
        Some(r#"{"name":"dev"}"#.into()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    v["id"].as_str().unwrap().parse().unwrap()
}

fn post_body(body: &str) -> String {
    serde_json::json!({
        "body": body,
        "sender_type": "agent",
        "sender_name": "alice-agent",
        "mentions": ["bob"],
    })
    .to_string()
}

async fn post(app: &Router, token: &str, channel: Uuid, body: &str) -> (StatusCode, Vec<u8>) {
    call(
        app,
        Method::POST,
        &format!("/api/channels/{channel}/messages"),
        token,
        Some(post_body(body)),
    )
    .await
}

async fn list(app: &Router, token: &str, channel: Uuid, query: &str) -> Vec<Message> {
    let (status, body) = call(
        app,
        Method::GET,
        &format!("/api/channels/{channel}/messages{query}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&body).unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn post_then_list(pool: PgPool) {
    let app = hub(pool.clone());
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&app, &token).await;

    let (status, body) = post(&app, &token, channel, "hello").await;
    assert_eq!(status, StatusCode::CREATED);
    let posted: Message = serde_json::from_slice(&body).unwrap();
    assert_eq!(posted.channel_id, channel);
    assert_eq!(posted.body, "hello");
    assert_eq!(posted.sender_type, SenderType::Agent);
    assert_eq!(posted.sender_name, "alice-agent");
    assert_eq!(posted.mentions, vec!["bob".to_string()]);
    assert_eq!(posted.reply_to, None);

    post(&app, &token, channel, "second").await;
    let listed = list(&app, &token, channel, "").await;
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0], posted);
    assert_eq!(listed[1].body, "second");
    assert!(listed[0].id < listed[1].id);
}

#[sqlx::test(migrations = "./migrations")]
async fn since_id_filters(pool: PgPool) {
    let app = hub(pool.clone());
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&app, &token).await;
    for body in ["one", "two", "three"] {
        post(&app, &token, channel, body).await;
    }
    let all = list(&app, &token, channel, "").await;
    let after_first = list(&app, &token, channel, &format!("?since_id={}", all[0].id)).await;
    let bodies: Vec<_> = after_first.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, ["two", "three"]);
    let limited = list(&app, &token, channel, "?limit=1").await;
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].body, "one");
}

#[sqlx::test(migrations = "./migrations")]
async fn non_member_post_403(pool: PgPool) {
    let app = hub(pool.clone());
    let owner = user(&pool, "a@x.io").await;
    let outsider = user(&pool, "b@x.io").await;
    let channel = new_channel(&app, &owner).await;
    post(&app, &owner, channel, "secret").await;

    let (status, _) = post(&app, &outsider, channel, "hi").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &app,
        Method::GET,
        &format!("/api/channels/{channel}/messages"),
        &outsider,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(list(&app, &owner, channel, "").await.len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn vietnamese_body_roundtrips(pool: PgPool) {
    let app = hub(pool.clone());
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&app, &token).await;
    let text = "Xin chào, tôi là trợ lý: Nguyễn Văn Ấn đang kiểm tra lỗi đăng nhập ứơ";
    let (status, _) = post(&app, &token, channel, text).await;
    assert_eq!(status, StatusCode::CREATED);
    let listed = list(&app, &token, channel, "").await;
    assert_eq!(listed[0].body.as_bytes(), text.as_bytes());
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_messages_rejected(pool: PgPool) {
    let app = hub(pool.clone());
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&app, &token).await;
    let (status, _) = post(&app, &token, channel, "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post(&app, &token, channel, &"x".repeat(64 * 1024 + 1)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post(&app, &token, channel, &"x".repeat(64 * 1024)).await;
    assert_eq!(status, StatusCode::CREATED);
    let bad_name = serde_json::json!({"body":"hi","sender_type":"human","sender_name":""});
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/channels/{channel}/messages"),
        &token,
        Some(bad_name.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
