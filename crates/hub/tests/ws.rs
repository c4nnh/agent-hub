use ah_common::{AgentPresence, ClientHello, Message, SenderType, WsEvent};
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
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite};
use tower::ServiceExt;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(5);

struct Server {
    app: Router,
    addr: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(pool: PgPool) -> Server {
    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: HubMode::Open,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: None,
    };
    let app = app(AppState::new(pool, cfg));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server { app, addr, task }
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

async fn agents(app: &Router, token: &str, channel: Uuid) -> Vec<AgentPresence> {
    let (status, body) = call(
        app,
        Method::GET,
        &format!("/api/channels/{channel}/agents"),
        token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&body).unwrap()
}

/// Polls the agents endpoint until it has `count` entries.
async fn wait_agents(app: &Router, token: &str, channel: Uuid, count: usize) -> Vec<AgentPresence> {
    timeout(WAIT, async {
        loop {
            let found = agents(app, token, channel).await;
            if found.len() == count {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("agents endpoint never reached expected count")
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(server: &Server, token: &str) -> Result<Client, tungstenite::Error> {
    let url = format!("ws://{}/api/ws?token={token}", server.addr);
    connect_async(url).await.map(|(ws, _)| ws)
}

async fn hello(ws: &mut Client, name: Option<&str>, folder: Option<&str>, channels: Vec<Uuid>) {
    let hello = ClientHello {
        agent_name: name.map(str::to_string),
        sender_type: if name.is_some() {
            SenderType::Agent
        } else {
            SenderType::Human
        },
        folder: folder.map(str::to_string),
        channels,
    };
    let text = serde_json::to_string(&hello).unwrap();
    ws.send(tungstenite::Message::text(text)).await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn subscriber_receives_posted_message(pool: PgPool) {
    let server = serve(pool.clone()).await;
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&server.app, &token).await;
    let mut ws = connect(&server, &token).await.unwrap();
    hello(&mut ws, Some("bot"), None, vec![channel]).await;
    // Presence is registered after the subscription, so it is a safe readiness signal.
    wait_agents(&server.app, &token, channel, 1).await;

    let body = serde_json::json!({
        "body": "Xin chào", "sender_type": "human", "sender_name": "alice", "mentions": ["bot"]
    });
    let (status, posted) = call(
        &server.app,
        Method::POST,
        &format!("/api/channels/{channel}/messages"),
        &token,
        Some(body.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let posted: Message = serde_json::from_slice(&posted).unwrap();

    let frame = timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap();
    let text = frame.into_text().unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["type"], "message");
    assert_eq!(json["message"]["body"], "Xin chào");
    let event: WsEvent = serde_json::from_str(&text).unwrap();
    assert_eq!(event, WsEvent::Message { message: posted });
}

#[sqlx::test(migrations = "./migrations")]
async fn bad_token_ws_rejected(pool: PgPool) {
    let server = serve(pool).await;
    let err = connect(&server, "nope").await.unwrap_err();
    match err {
        tungstenite::Error::Http(res) => assert_eq!(res.status(), 401),
        other => panic!("expected http 401, got {other:?}"),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn revoked_token_ws_rejected(pool: PgPool) {
    let server = serve(pool.clone()).await;
    let token = user(&pool, "a@x.io").await;
    let ws = connect(&server, &token).await;
    assert!(ws.is_ok());
    let (status, _) = call(&server.app, Method::POST, "/api/auth/logout", &token, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let err = connect(&server, &token).await.unwrap_err();
    match err {
        tungstenite::Error::Http(res) => assert_eq!(res.status(), 401),
        other => panic!("expected http 401, got {other:?}"),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn agent_hello_appears_in_agents_endpoint_and_disconnect_removes_it(pool: PgPool) {
    let server = serve(pool.clone()).await;
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&server.app, &token).await;
    assert!(agents(&server.app, &token, channel).await.is_empty());

    let mut agent = connect(&server, &token).await.unwrap();
    hello(
        &mut agent,
        Some("alice-agent"),
        Some("/work/app"),
        vec![channel],
    )
    .await;
    let mut human = connect(&server, &token).await.unwrap();
    hello(&mut human, None, None, vec![channel]).await;

    let found = wait_agents(&server.app, &token, channel, 1).await;
    assert_eq!(found[0].name, "alice-agent");
    assert_eq!(found[0].folder.as_deref(), Some("/work/app"));

    agent.close(None).await.unwrap();
    wait_agents(&server.app, &token, channel, 0).await;
    drop(human);
}

#[sqlx::test(migrations = "./migrations")]
async fn hello_for_foreign_channel_closes_with_policy_violation(pool: PgPool) {
    let server = serve(pool.clone()).await;
    let owner = user(&pool, "a@x.io").await;
    let outsider = user(&pool, "b@x.io").await;
    let channel = new_channel(&server.app, &owner).await;

    let mut ws = connect(&server, &outsider).await.unwrap();
    hello(&mut ws, Some("spy"), None, vec![channel]).await;
    let frame = timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap();
    match frame {
        tungstenite::Message::Close(Some(close)) => {
            assert_eq!(
                close.code,
                tungstenite::protocol::frame::coding::CloseCode::Policy
            );
        }
        other => panic!("expected close frame, got {other:?}"),
    }
    assert!(agents(&server.app, &owner, channel).await.is_empty());
}
