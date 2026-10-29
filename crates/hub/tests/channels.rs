use ah_common::{ChannelSummary, InvitationSummary};
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
use serde::de::DeserializeOwned;
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
    app(AppState::new(pool, cfg))
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
    body: Option<&str>,
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

fn parse<T: DeserializeOwned>(body: &[u8]) -> T {
    serde_json::from_slice(body).unwrap()
}

async fn create(app: &Router, token: &str, name: &str) -> (StatusCode, Vec<u8>) {
    let body = format!(r#"{{"name":"{name}"}}"#);
    call(app, Method::POST, "/api/channels", token, Some(&body)).await
}

async fn invite(app: &Router, token: &str, channel: Uuid, email: &str) -> StatusCode {
    let body = format!(r#"{{"email":"{email}"}}"#);
    let uri = format!("/api/channels/{channel}/invitations");
    call(app, Method::POST, &uri, token, Some(&body)).await.0
}

async fn invitations(app: &Router, token: &str) -> Vec<InvitationSummary> {
    let (s, b) = call(app, Method::GET, "/api/invitations", token, None).await;
    assert_eq!(s, StatusCode::OK);
    parse(&b)
}

async fn channels(app: &Router, token: &str) -> Vec<ChannelSummary> {
    let (s, b) = call(app, Method::GET, "/api/channels", token, None).await;
    assert_eq!(s, StatusCode::OK);
    parse(&b)
}

async fn respond(app: &Router, token: &str, id: Uuid, accept: bool) -> StatusCode {
    let uri = format!("/api/invitations/{id}/respond");
    let body = format!(r#"{{"accept":{accept}}}"#);
    call(app, Method::POST, &uri, token, Some(&body)).await.0
}

#[sqlx::test(migrations = "./migrations")]
async fn create_and_list(pool: PgPool) {
    let app = hub(pool.clone());
    let t = user(&pool, "a@x.co").await;
    let (s, b) = create(&app, &t, "dev").await;
    assert_eq!(s, StatusCode::CREATED);
    let made: ChannelSummary = parse(&b);
    assert_eq!(
        (
            made.name.as_str(),
            made.role.as_str(),
            made.owner_email.as_str()
        ),
        ("dev", "owner", "a@x.co")
    );
    let list = channels(&app, &t).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, made.id);
}

#[sqlx::test(migrations = "./migrations")]
async fn duplicate_name_409(pool: PgPool) {
    let app = hub(pool.clone());
    let a = user(&pool, "a@x.co").await;
    let b = user(&pool, "b@x.co").await;
    assert_eq!(create(&app, &a, "dev").await.0, StatusCode::CREATED);
    assert_eq!(create(&app, &a, "dev").await.0, StatusCode::CONFLICT);
    // Another owner may reuse the name.
    assert_eq!(create(&app, &b, "dev").await.0, StatusCode::CREATED);
}

#[sqlx::test(migrations = "./migrations")]
async fn invite_unregistered_email_then_first_login_sees_it(pool: PgPool) {
    let app = hub(pool.clone());
    let a = user(&pool, "a@x.co").await;
    let ch: ChannelSummary = parse(&create(&app, &a, "dev").await.1);
    assert_eq!(
        invite(&app, &a, ch.id, "new@x.co").await,
        StatusCode::NO_CONTENT
    );
    // Duplicate pending invite and inviting an existing member are conflicts.
    assert_eq!(
        invite(&app, &a, ch.id, "new@x.co").await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        invite(&app, &a, ch.id, "a@x.co").await,
        StatusCode::CONFLICT
    );
    let n = user(&pool, "new@x.co").await;
    let list = invitations(&app, &n).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].channel_name, "dev");
    assert_eq!(list[0].invited_by_email, "a@x.co");
}

#[sqlx::test(migrations = "./migrations")]
async fn accept_creates_membership(pool: PgPool) {
    let app = hub(pool.clone());
    let a = user(&pool, "a@x.co").await;
    let b = user(&pool, "b@x.co").await;
    let c = user(&pool, "c@x.co").await;
    let ch: ChannelSummary = parse(&create(&app, &a, "dev").await.1);
    invite(&app, &a, ch.id, "b@x.co").await;
    let inv = invitations(&app, &b).await.remove(0);
    // Someone else's invitation looks missing.
    assert_eq!(respond(&app, &c, inv.id, true).await, StatusCode::NOT_FOUND);
    assert_eq!(
        respond(&app, &b, inv.id, true).await,
        StatusCode::NO_CONTENT
    );
    let list = channels(&app, &b).await;
    assert_eq!(list.len(), 1);
    assert_eq!(
        (list[0].role.as_str(), list[0].owner_email.as_str()),
        ("member", "a@x.co")
    );
    assert!(invitations(&app, &b).await.is_empty());
    assert_eq!(respond(&app, &b, inv.id, true).await, StatusCode::CONFLICT);
    // A member can invite further people.
    assert_eq!(
        invite(&app, &b, ch.id, "c@x.co").await,
        StatusCode::NO_CONTENT
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn reject_does_not(pool: PgPool) {
    let app = hub(pool.clone());
    let a = user(&pool, "a@x.co").await;
    let b = user(&pool, "b@x.co").await;
    let ch: ChannelSummary = parse(&create(&app, &a, "dev").await.1);
    invite(&app, &a, ch.id, "b@x.co").await;
    let inv = invitations(&app, &b).await.remove(0);
    assert_eq!(
        respond(&app, &b, inv.id, false).await,
        StatusCode::NO_CONTENT
    );
    assert!(channels(&app, &b).await.is_empty());
    assert!(invitations(&app, &b).await.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn non_member_cannot_invite_403(pool: PgPool) {
    let app = hub(pool.clone());
    let a = user(&pool, "a@x.co").await;
    let b = user(&pool, "b@x.co").await;
    let ch: ChannelSummary = parse(&create(&app, &a, "dev").await.1);
    assert_eq!(
        invite(&app, &b, ch.id, "c@x.co").await,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn leave_removes_membership(pool: PgPool) {
    let app = hub(pool.clone());
    let a = user(&pool, "a@x.co").await;
    let b = user(&pool, "b@x.co").await;
    let ch: ChannelSummary = parse(&create(&app, &a, "dev").await.1);
    invite(&app, &a, ch.id, "b@x.co").await;
    let inv = invitations(&app, &b).await.remove(0);
    respond(&app, &b, inv.id, true).await;
    let leave = format!("/api/channels/{}/leave", ch.id);
    // Owner cannot leave while others remain.
    let (s, _) = call(&app, Method::POST, &leave, &a, None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(&app, Method::POST, &leave, &b, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(channels(&app, &b).await.is_empty());
    // Not a member any more.
    let (s, _) = call(&app, Method::POST, &leave, &b, None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Last member leaving deletes the channel.
    let (s, _) = call(&app, Method::POST, &leave, &a, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let left: i64 = sqlx::query_scalar("select count(*) from channels")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}
