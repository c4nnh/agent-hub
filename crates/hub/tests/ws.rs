use ah_common::{AgentPresence, ClientHello, Message, SenderType, WsEvent};
use ah_hub::{
    auth::issue_token,
    config::{HubConfig, HubMode},
    routes::app,
    state::AppState,
    ws::Registry,
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
    registry: Registry,
    addr: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(pool: PgPool) -> Server {
    serve_with(pool, Registry::default()).await
}

async fn serve_with(pool: PgPool, registry: Registry) -> Server {
    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: HubMode::Open,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: None,
    };
    let mut state = AppState::new(pool, cfg, std::sync::Arc::new(ah_hub::embed::HashEmbedder));
    state.registry = registry.clone();
    let app = app(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server {
        app,
        registry,
        addr,
        task,
    }
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

/// Next text frame, skipping pings.
async fn next_text(ws: &mut Client) -> String {
    loop {
        let frame = timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap();
        if let tungstenite::Message::Text(text) = frame {
            return text.to_string();
        }
    }
}

async fn next_event(ws: &mut Client) -> WsEvent {
    serde_json::from_str(&next_text(ws).await).unwrap()
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
    assert_eq!(next_event(&mut ws).await, WsEvent::Subscribed);

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

    let text = next_text(&mut ws).await;
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

fn message(channel: Uuid, id: i64) -> Message {
    Message {
        id,
        channel_id: channel,
        sender_type: SenderType::Human,
        sender_name: "alice".into(),
        body: "x".into(),
        reply_to: None,
        mentions: vec![],
        created_at: chrono::Utc::now(),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn subscribed_json_shape(pool: PgPool) {
    let server = serve(pool.clone()).await;
    let token = user(&pool, "a@x.io").await;
    let mut ws = connect(&server, &token).await.unwrap();
    hello(&mut ws, None, None, vec![]).await;
    assert_eq!(next_text(&mut ws).await, r#"{"type":"subscribed"}"#);
}

#[sqlx::test(migrations = "./migrations")]
async fn full_queue_evicts_subscriber(pool: PgPool) {
    let registry = Registry::with_tuning(1, Duration::from_secs(30));
    let server = serve_with(pool.clone(), registry).await;
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&server.app, &token).await;
    let mut ws = connect(&server, &token).await.unwrap();
    hello(&mut ws, Some("bot"), None, vec![channel]).await;
    assert_eq!(next_event(&mut ws).await, WsEvent::Subscribed);

    // No await between publishes, so the capacity-1 queue overflows on the second.
    for id in 1..=3 {
        server.registry.publish(channel, &message(channel, id));
    }
    // The stream ends (close or EOF) instead of silently skipping events.
    timeout(WAIT, async {
        while ws.next().await.is_some_and(|f| f.is_ok()) {}
    })
    .await
    .expect("connection was not closed");
    wait_agents(&server.app, &token, channel, 0).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn server_pings_and_drops_silent_peer(pool: PgPool) {
    let registry = Registry::with_tuning(8, Duration::from_millis(100));
    let server = serve_with(pool.clone(), registry).await;
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&server.app, &token).await;
    let mut ws = connect(&server, &token).await.unwrap();
    hello(&mut ws, Some("bot"), None, vec![channel]).await;
    assert_eq!(next_event(&mut ws).await, WsEvent::Subscribed);
    let ping = timeout(WAIT, async {
        loop {
            if let tungstenite::Message::Ping(_) = ws.next().await.unwrap().unwrap() {
                return;
            }
        }
    })
    .await;
    assert!(ping.is_ok(), "no ping received");

    // The client no longer reads, so it never answers: the hub must drop it.
    wait_agents(&server.app, &token, channel, 0).await;
    drop(ws);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_posts_all_delivered(pool: PgPool) {
    const N: usize = 8;
    let server = serve(pool.clone()).await;
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&server.app, &token).await;
    let mut ws = connect(&server, &token).await.unwrap();
    hello(&mut ws, Some("bot"), None, vec![channel]).await;
    assert_eq!(next_event(&mut ws).await, WsEvent::Subscribed);

    let posts: Vec<_> = (0..N)
        .map(|i| {
            let app = server.app.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let body = serde_json::json!({
                    "body": format!("m{i}"), "sender_type": "human", "sender_name": "alice"
                });
                let uri = format!("/api/channels/{channel}/messages");
                let (status, _) =
                    call(&app, Method::POST, &uri, &token, Some(body.to_string())).await;
                assert_eq!(status, StatusCode::CREATED);
            })
        })
        .collect();
    for p in posts {
        p.await.unwrap();
    }

    // Clients keep a per-channel high-water cursor, so pushes must arrive in id order.
    let mut ids = Vec::new();
    while ids.len() < N {
        match next_event(&mut ws).await {
            WsEvent::Message { message } => ids.push(message.id),
            WsEvent::Subscribed => panic!("unexpected subscribed"),
        }
    }
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
}

/// Reads frames (so pings get answered) until the hub closes; returns the close frame.
async fn wait_close(ws: &mut Client, within: Duration) -> tungstenite::protocol::CloseFrame {
    timeout(within, async {
        loop {
            match ws.next().await {
                Some(Ok(tungstenite::Message::Close(Some(frame)))) => return frame,
                Some(Ok(_)) => continue,
                other => panic!("expected close frame, got {other:?}"),
            }
        }
    })
    .await
    .expect("hub did not close the connection")
}

#[sqlx::test(migrations = "./migrations")]
async fn logout_closes_live_ws(pool: PgPool) {
    let tick = Duration::from_millis(100);
    let server = serve_with(pool.clone(), Registry::with_tuning(8, tick)).await;
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&server.app, &token).await;
    let mut ws = connect(&server, &token).await.unwrap();
    hello(&mut ws, Some("bot"), None, vec![channel]).await;
    assert_eq!(next_event(&mut ws).await, WsEvent::Subscribed);

    let (status, _) = call(&server.app, Method::POST, "/api/auth/logout", &token, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Two ticks plus scheduling slack.
    let frame = wait_close(&mut ws, tick * 5).await;
    assert_eq!(
        frame.code,
        tungstenite::protocol::frame::coding::CloseCode::Policy
    );
    assert_eq!(frame.reason.as_str(), "token revoked");
}

#[sqlx::test(migrations = "./migrations")]
async fn leaving_channel_closes_live_ws(pool: PgPool) {
    let tick = Duration::from_millis(100);
    let server = serve_with(pool.clone(), Registry::with_tuning(8, tick)).await;
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&server.app, &token).await;
    let mut ws = connect(&server, &token).await.unwrap();
    hello(&mut ws, Some("bot"), None, vec![channel]).await;
    assert_eq!(next_event(&mut ws).await, WsEvent::Subscribed);

    let uri = format!("/api/channels/{channel}/leave");
    let (status, _) = call(&server.app, Method::POST, &uri, &token, None).await;
    assert!(status.is_success(), "{status}");
    let frame = wait_close(&mut ws, tick * 5).await;
    assert_eq!(frame.reason.as_str(), "not a channel member");
}

#[sqlx::test(migrations = "./migrations")]
async fn hello_with_too_many_channels_closes(pool: PgPool) {
    let server = serve(pool.clone()).await;
    let token = user(&pool, "a@x.io").await;
    let mut ws = connect(&server, &token).await.unwrap();
    let channels = (0..65).map(|_| Uuid::new_v4()).collect();
    hello(&mut ws, Some("bot"), None, channels).await;
    let frame = wait_close(&mut ws, WAIT).await;
    assert_eq!(
        frame.code,
        tungstenite::protocol::frame::coding::CloseCode::Policy
    );
    assert_eq!(frame.reason.as_str(), "too many channels");
}

#[sqlx::test(migrations = "./migrations")]
async fn post_survives_request_cancellation(pool: PgPool) {
    let server = serve(pool.clone()).await;
    let token = user(&pool, "a@x.io").await;
    let channel = new_channel(&server.app, &token).await;
    let mut ws = connect(&server, &token).await.unwrap();
    hello(&mut ws, Some("bot"), None, vec![channel]).await;
    assert_eq!(next_event(&mut ws).await, WsEvent::Subscribed);

    // Hold the channel's advisory lock so the post parks inside its critical section.
    let lock = "select pg_advisory_lock(hashtext($1::uuid::text))";
    let mut blocker = pool.acquire().await.unwrap();
    sqlx::query(lock)
        .bind(channel)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let body = serde_json::json!({
        "body": "survives", "sender_type": "human", "sender_name": "alice"
    });
    let uri = format!("/api/channels/{channel}/messages");
    let post = call(
        &server.app,
        Method::POST,
        &uri,
        &token,
        Some(body.to_string()),
    );
    let parked = async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "select count(*) from pg_locks l join pg_database d on d.oid = l.database
                 where l.locktype = 'advisory' and not l.granted and d.datname = current_database()",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    // Dropping `post` here is what axum does when the HTTP client disconnects.
    tokio::select! {
        _ = post => panic!("post finished while the lock was held"),
        parked = timeout(WAIT, parked) => parked.expect("post never reached the lock"),
    }
    sqlx::query("select pg_advisory_unlock(hashtext($1::uuid::text))")
        .bind(channel)
        .execute(&mut *blocker)
        .await
        .unwrap();

    match next_event(&mut ws).await {
        WsEvent::Message { message } => assert_eq!(message.body, "survives"),
        other => panic!("expected message, got {other:?}"),
    }
}
