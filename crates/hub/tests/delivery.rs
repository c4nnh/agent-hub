use ah_common::{ClientHello, DeliveryState, SenderType, WsEvent};
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
use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use std::time::Duration;
use tokio::{net::TcpListener, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite};
use tower::ServiceExt;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(5);

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

async fn put(app: &Router, token: &str, channel: Uuid, body: serde_json::Value) -> StatusCode {
    let uri = format!("/api/channels/{channel}/agent-cursor");
    call(app, Method::PUT, &uri, token, Some(body.to_string()))
        .await
        .0
}

async fn delivery(app: &Router, token: &str, channel: Uuid) -> DeliveryState {
    let uri = format!("/api/channels/{channel}/delivery");
    let (status, body) = call(app, Method::GET, &uri, token, None).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&body).unwrap()
}

async fn post_message(app: &Router, token: &str, channel: Uuid) -> i64 {
    let uri = format!("/api/channels/{channel}/messages");
    let body = r#"{"body":"hi","sender_type":"human","sender_name":"ann"}"#;
    let (status, b) = call(app, Method::POST, &uri, token, Some(body.into())).await;
    assert_eq!(status, StatusCode::CREATED);
    serde_json::from_slice::<ah_common::Message>(&b).unwrap().id
}

#[sqlx::test(migrations = "./migrations")]
async fn member_reports_and_reads_back(pool: PgPool) {
    let app = hub(pool.clone());
    let t = user(&pool, "a@x.co").await;
    let ch = new_channel(&app, &t).await;
    let id = post_message(&app, &t, ch).await;
    let status = put(
        &app,
        &t,
        ch,
        serde_json::json!({"agent_name": "bot", "last_read_id": id}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let state = delivery(&app, &t, ch).await;
    assert_eq!(state.last_message_id, id);
    assert_eq!(state.agents.len(), 1);
    assert_eq!(state.agents[0].agent_name, "bot");
    assert_eq!(state.agents[0].last_read_id, id);
    assert!(state.agents[0].updated_at.is_some());
    assert!(!state.agents[0].online);
}

#[sqlx::test(migrations = "./migrations")]
async fn non_member_is_rejected(pool: PgPool) {
    let app = hub(pool.clone());
    let a = user(&pool, "a@x.co").await;
    let b = user(&pool, "b@x.co").await;
    let ch = new_channel(&app, &a).await;
    let status = put(
        &app,
        &b,
        ch,
        serde_json::json!({"agent_name": "bot", "last_read_id": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let uri = format!("/api/channels/{ch}/delivery");
    let (status, _) = call(&app, Method::GET, &uri, &b, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let rows: i64 = sqlx::query_scalar("select count(*) from agent_cursors")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn cursor_never_moves_back(pool: PgPool) {
    let app = hub(pool.clone());
    let t = user(&pool, "a@x.co").await;
    let ch = new_channel(&app, &t).await;
    for value in [10, 4, 12] {
        let body = serde_json::json!({"agent_name": "bot", "last_read_id": value});
        assert_eq!(put(&app, &t, ch, body).await, StatusCode::NO_CONTENT);
    }
    assert_eq!(delivery(&app, &t, ch).await.agents[0].last_read_id, 12);
    let body = serde_json::json!({"agent_name": "bot", "last_read_id": 3});
    assert_eq!(put(&app, &t, ch, body).await, StatusCode::NO_CONTENT);
    assert_eq!(delivery(&app, &t, ch).await.agents[0].last_read_id, 12);
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_bodies_are_rejected(pool: PgPool) {
    let app = hub(pool.clone());
    let t = user(&pool, "a@x.co").await;
    let ch = new_channel(&app, &t).await;
    let long = "n".repeat(129);
    let bad = [
        serde_json::json!({"agent_name": "", "last_read_id": 1}),
        serde_json::json!({"agent_name": long, "last_read_id": 1}),
        serde_json::json!({"agent_name": "bot", "last_read_id": -1}),
    ];
    for body in bad {
        assert_eq!(put(&app, &t, ch, body).await, StatusCode::BAD_REQUEST);
    }
    let missing = serde_json::json!({"agent_name": "bot"});
    assert_eq!(
        put(&app, &t, ch, missing).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let exact = "n".repeat(128);
    let ok = serde_json::json!({"agent_name": exact, "last_read_id": 0});
    assert_eq!(put(&app, &t, ch, ok).await, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "./migrations")]
async fn delivery_merges_cursors_with_presence_ordered_by_name(pool: PgPool) {
    let state = AppState::new(
        pool.clone(),
        HubConfig {
            database_url: String::new(),
            hub_mode: HubMode::Open,
            google_client_id: "cid".into(),
            google_client_secret: "sec".into(),
            public_url: "http://hub.test".into(),
            google_base_url: None,
        },
        std::sync::Arc::new(ah_hub::embed::HashEmbedder),
    );
    let router = app(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = router.clone();
    let task = tokio::spawn(async move { axum::serve(listener, serving).await.unwrap() });

    let t = user(&pool, "a@x.co").await;
    let ch = new_channel(&router, &t).await;
    let id = post_message(&router, &t, ch).await;
    // "zed" reported and is online; "amy" reported and is offline; "mia" is online, never reported.
    for name in ["zed", "amy"] {
        let body = serde_json::json!({"agent_name": name, "last_read_id": id});
        assert_eq!(put(&router, &t, ch, body).await, StatusCode::NO_CONTENT);
    }
    let mut sockets = Vec::new();
    for name in ["zed", "mia"] {
        let (mut ws, _) = connect_async(format!("ws://{addr}/api/ws?token={t}"))
            .await
            .unwrap();
        let hello = ClientHello {
            agent_name: Some(name.into()),
            sender_type: SenderType::Agent,
            folder: None,
            channels: vec![ch],
        };
        ws.send(tungstenite::Message::text(
            serde_json::to_string(&hello).unwrap(),
        ))
        .await
        .unwrap();
        loop {
            let frame = timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap();
            if let tungstenite::Message::Text(text) = frame {
                assert_eq!(
                    serde_json::from_str::<WsEvent>(&text).unwrap(),
                    WsEvent::Subscribed
                );
                break;
            }
        }
        sockets.push(ws);
    }

    let got = delivery(&router, &t, ch).await;
    let rows: Vec<(&str, i64, bool, bool)> = got
        .agents
        .iter()
        .map(|a| {
            (
                a.agent_name.as_str(),
                a.last_read_id,
                a.online,
                a.updated_at.is_some(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("amy", id, false, true),
            ("mia", 0, true, false),
            ("zed", id, true, true)
        ]
    );
    drop(sockets);
    task.abort();
}

#[sqlx::test(migrations = "./migrations")]
async fn cursors_are_deleted_with_the_channel(pool: PgPool) {
    let app = hub(pool.clone());
    let t = user(&pool, "a@x.co").await;
    let ch = new_channel(&app, &t).await;
    let body = serde_json::json!({"agent_name": "bot", "last_read_id": 1});
    assert_eq!(put(&app, &t, ch, body).await, StatusCode::NO_CONTENT);
    let leave = format!("/api/channels/{ch}/leave");
    let (status, _) = call(&app, Method::POST, &leave, &t, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let rows: i64 = sqlx::query_scalar("select count(*) from agent_cursors")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}
