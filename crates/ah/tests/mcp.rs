use ah::{
    bindings::{Binding, Bindings},
    client::HubClient,
    commands::{
        channel::run_join,
        wait::{WaitArgs, run_wait},
    },
    mcp::{Session, tools::AgentHub, wsclient},
};
use ah_common::ChannelSummary;
use ah_hub::{
    auth::issue_token,
    config::{HubConfig, HubMode},
    routes::app,
    state::AppState,
    ws::Registry,
};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    service::RunningService,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

const AGENT: &str = "claude-code@test";
const BACKOFF: Duration = Duration::from_millis(50);
const LIMIT: Duration = Duration::from_secs(10);

async fn spawn_hub(pool: PgPool, mode: HubMode) -> String {
    spawn_hub_with(pool, mode, Registry::default()).await
}

async fn spawn_hub_with(pool: PgPool, mode: HubMode, registry: Registry) -> String {
    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: mode,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: None,
    };
    let mut state = AppState::new(pool, cfg, Arc::new(ah_hub::embed::HashEmbedder));
    state.registry = registry;
    let router = app(state);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    url
}

async fn user(pool: &PgPool, hub: &str, email: &str) -> HubClient {
    let id = Uuid::new_v4();
    sqlx::query("insert into users (id, email, name, plan) values ($1, $2, 'N', 'free')")
        .bind(id)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    let token = issue_token(pool, id).await.unwrap();
    HubClient::new(hub.to_string(), Some(token))
}

/// Another token for an existing user, as a second `ah mcp` on the same machine would have.
async fn same_user(pool: &PgPool, hub: &str, email: &str) -> HubClient {
    let id: Uuid = sqlx::query_scalar("select id from users where email = $1")
        .bind(email)
        .fetch_one(pool)
        .await
        .unwrap();
    let token = issue_token(pool, id).await.unwrap();
    HubClient::new(hub.to_string(), Some(token))
}

/// A bound folder plus the MCP client talking to the server over an in-process duplex pipe.
struct Harness {
    session: Arc<Session>,
    ws: tokio::task::JoinHandle<()>,
    mcp: RunningService<RoleClient, ()>,
    /// Config dir (bindings, cursors) and the bound folder.
    cfg: tempfile::TempDir,
    dir: tempfile::TempDir,
}

async fn harness(token_owner: HubClient, channels: &[&ChannelSummary]) -> Harness {
    let cfg = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    harness_in(cfg, dir, token_owner, channels).await
}

async fn harness_in(
    cfg: tempfile::TempDir,
    dir: tempfile::TempDir,
    token_owner: HubClient,
    channels: &[&ChannelSummary],
) -> Harness {
    let mut bindings = Bindings::default();
    bindings.0.insert(
        Bindings::key(dir.path()).unwrap(),
        Binding {
            channel_ids: channels.iter().map(|c| c.id).collect(),
            extra_folders: vec!["/srv/shared".into()],
            ..Default::default()
        },
    );
    let session = tokio::time::timeout(
        LIMIT,
        Session::start(token_owner, &bindings, cfg.path(), dir.path(), AGENT.into()),
    )
    .await
    .unwrap()
    .unwrap();
    let ws = wsclient::spawn(session.clone(), BACKOFF);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = AgentHub::new(session.clone());
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    let mcp = tokio::time::timeout(LIMIT, ().serve(client_io))
        .await
        .unwrap()
        .unwrap();
    Harness {
        session,
        ws,
        mcp,
        cfg,
        dir,
    }
}

async fn call(h: &Harness, tool: &'static str, args: Value) -> CallToolResult {
    let params = CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone());
    tokio::time::timeout(LIMIT, h.mcp.call_tool(params))
        .await
        .unwrap()
        .unwrap()
}

fn text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_error(r: &CallToolResult) -> bool {
    r.is_error == Some(true)
}

async fn post_human(client: &HubClient, ch: &ChannelSummary, body: &str) {
    tokio::time::timeout(LIMIT, client.post_human_message(ch.id, "ann", body))
        .await
        .unwrap()
        .unwrap();
}

/// Second user `b` joins channel `ch` owned by `a`.
async fn add_member(a: &HubClient, b: &HubClient, ch: &ChannelSummary, email: &str) {
    a.invite(ch.id, email).await.unwrap();
    let inv = b.list_invitations().await.unwrap();
    b.respond_invitation(inv[0].id, true).await.unwrap();
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn send_then_get_messages(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    let h = harness(a, &[&ch]).await;

    let instructions = h
        .mcp
        .peer_info()
        .and_then(|i| i.instructions.clone())
        .unwrap_or_default();
    assert!(instructions.contains("/srv/shared"), "{instructions}");
    assert!(instructions.contains("wait_for_message"), "{instructions}");

    let sent = call(
        &h,
        "send_message",
        json!({"channel": "team", "body": "hello @bob", "mentions": ["bob"]}),
    )
    .await;
    assert!(!is_error(&sent), "{}", text(&sent));

    let got = call(&h, "get_messages", json!({"channel": "team"})).await;
    let body = text(&got);
    assert!(body.contains("hello @bob"), "{body}");
    assert!(body.contains(AGENT), "{body}");

    for body in ["m1", "m2", "m3"] {
        call(&h, "send_message", json!({"channel": "team", "body": body})).await;
    }
    let recent = text(&call(&h, "get_messages", json!({"channel": "team", "limit": 2})).await);
    assert!(!recent.contains("m1"), "{recent}");
    assert!(
        recent.find("m2").unwrap() < recent.find("m3").unwrap(),
        "{recent}"
    );

    let unknown = call(&h, "send_message", json!({"channel": "nope", "body": "x"})).await;
    assert!(is_error(&unknown));
    assert!(text(&unknown).contains("team"), "{}", text(&unknown));
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn wait_returns_when_other_session_posts(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let b = user(&pool, &hub, "b@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    add_member(&a, &b, &ch, "b@b.co").await;
    let h = harness(a, &[&ch]).await;

    let poster = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        post_human(&b, &ch, "ping from b").await;
    });
    let started = std::time::Instant::now();
    let r = call(&h, "wait_for_message", json!({"timeout_secs": 30})).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    let body = text(&r);
    assert!(body.contains("ping from b"), "{body}");
    assert!(body.contains("\"channel\":\"team\""), "{body}");
    poster.await.unwrap();
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn wait_times_out_empty(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    let h = harness(a, &[&ch]).await;

    // Own messages are not echoed back to the sender.
    call(
        &h,
        "send_message",
        json!({"channel": "team", "body": "mine"}),
    )
    .await;
    let started = std::time::Instant::now();
    let r = call(&h, "wait_for_message", json!({"timeout_secs": 1})).await;
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(!is_error(&r));
    let body = text(&r);
    assert!(body.contains("no new messages"), "{body}");
    assert!(!body.contains("mine"), "{body}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn reconnect_recovers_missed_messages(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let b = user(&pool, &hub, "b@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    add_member(&a, &b, &ch, "b@b.co").await;
    let mut h = harness(a, &[&ch]).await;

    post_human(&b, &ch, "before drop").await;
    let first = call(&h, "wait_for_message", json!({"timeout_secs": 5})).await;
    assert!(text(&first).contains("before drop"), "{}", text(&first));

    // Drop the websocket; messages posted now reach no live subscriber of this session.
    h.ws.abort();
    let _ = (&mut h.ws).await;
    post_human(&b, &ch, "while down 1").await;
    post_human(&b, &ch, "while down 2").await;

    let _ws = wsclient::spawn(h.session.clone(), BACKOFF);
    let r = call(&h, "wait_for_message", json!({"timeout_secs": 5})).await;
    let body = text(&r);
    let one = body.find("while down 1").expect(&body);
    let two = body.find("while down 2").expect(&body);
    assert!(one < two, "{body}");
    assert!(!body.contains("before drop"), "{body}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn update_context_stale_returns_current(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    let h = harness(a, &[&ch]).await;

    let created = call(
        &h,
        "update_context",
        json!({"channel": "team", "title": "overview", "body": "v1"}),
    )
    .await;
    assert!(!is_error(&created), "{}", text(&created));
    let entry: Value = serde_json::from_str(text(&created).lines().next().unwrap()).unwrap();
    let id = entry["id"].as_str().unwrap().to_string();

    let updated = call(
        &h,
        "update_context",
        json!({"id": id, "channel": "team", "title": "overview", "body": "v2", "base_version": 1}),
    )
    .await;
    assert!(!is_error(&updated), "{}", text(&updated));

    let stale = call(
        &h,
        "update_context",
        json!({"id": id, "channel": "team", "title": "overview", "body": "v3", "base_version": 1}),
    )
    .await;
    let body = text(&stale);
    assert!(
        body.contains("stale version: merge with the current entry and retry with its version"),
        "{body}"
    );
    assert!(body.contains("\"version\":2"), "{body}");
    assert!(body.contains("v2"), "{body}");

    let got = call(&h, "get_context", json!({"id": id})).await;
    assert!(text(&got).contains("v2"), "{}", text(&got));
    let listed = call(&h, "list_context", json!({"channel": "team"})).await;
    assert!(text(&listed).contains("overview"), "{}", text(&listed));
    let found = call(
        &h,
        "search_context",
        json!({"channel": "team", "query": "overview"}),
    )
    .await;
    assert!(text(&found).contains("overview"), "{}", text(&found));
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn free_plan_context_tool_returns_paid_required_error(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Hosted).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    let h = harness(a, &[&ch]).await;

    let r = call(&h, "list_context", json!({"channel": "team"})).await;
    assert!(is_error(&r));
    assert!(
        text(&r).contains("Context store requires a paid plan"),
        "{}",
        text(&r)
    );
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn unread_hint_appended(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let b = user(&pool, &hub, "b@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    add_member(&a, &b, &ch, "b@b.co").await;
    let h = harness(a, &[&ch]).await;

    let quiet = call(&h, "list_agents", json!({"channel": "team"})).await;
    assert!(!text(&quiet).contains("unread:"), "{}", text(&quiet));

    post_human(&b, &ch, "one").await;
    post_human(&b, &ch, "two").await;
    let mut hinted = String::new();
    for _ in 0..100 {
        let r = call(&h, "list_agents", json!({"channel": "team"})).await;
        hinted = text(&r);
        if hinted.contains("unread: 2") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(hinted.ends_with("unread: 2"), "{hinted}");
    assert!(hinted.contains(AGENT), "{hinted}");

    // Errors carry the hint too.
    let err = call(&h, "get_messages", json!({"channel": "nope"})).await;
    assert!(is_error(&err));
    assert!(text(&err).ends_with("unread: 2"), "{}", text(&err));
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn unbound_folder_tools_return_join_hint(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let h = harness(a, &[]).await;

    let r = call(&h, "wait_for_message", json!({"timeout_secs": 1})).await;
    assert!(is_error(&r));
    assert!(text(&r).contains("ah channel join"), "{}", text(&r));
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn two_sessions_same_name_both_receive(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let a2 = same_user(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    // Both sessions run under the same agent name, like two folders with an explicit --name.
    let one = harness(a, &[&ch]).await;
    let two = harness(a2, &[&ch]).await;

    let sent = call(
        &one,
        "send_message",
        json!({"channel": "team", "body": "from one"}),
    )
    .await;
    assert!(!is_error(&sent), "{}", text(&sent));
    let got = text(&call(&two, "wait_for_message", json!({"timeout_secs": 5})).await);
    assert!(got.contains("from one"), "{got}");

    call(
        &two,
        "send_message",
        json!({"channel": "team", "body": "from two"}),
    )
    .await;
    let got = text(&call(&one, "wait_for_message", json!({"timeout_secs": 5})).await);
    assert!(got.contains("from two"), "{got}");
    // Each session still filters its own sends.
    assert!(!got.contains("from one"), "{got}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn revoked_token_stops_wait_with_reason(pool: PgPool) {
    let registry = Registry::with_tuning(256, Duration::from_millis(100));
    let hub = spawn_hub_with(pool.clone(), HubMode::Open, registry).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    let h = harness(a, &[&ch]).await;

    h.session.client.logout().await.unwrap();
    let started = std::time::Instant::now();
    let r = call(&h, "wait_for_message", json!({"timeout_secs": 30})).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(is_error(&r), "{}", text(&r));
    let body = text(&r);
    assert!(body.contains("message feed stopped"), "{body}");
    assert!(body.contains("ah auth login"), "{body}");
}

/// Live bug: a session started after a send used to begin at "now" and miss it.
#[sqlx::test(migrations = "../hub/migrations")]
async fn session_started_after_send_delivers_since_join(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    post_human(&a, &ch, "before join").await;
    let cfg = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    run_join(
        cfg.path(),
        &a,
        dir.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        &mut Vec::new(),
    )
    .await
    .unwrap();
    post_human(&a, &ch, "sent before the session").await;

    let h = harness_in(cfg, dir, a, &[&ch]).await;
    let body = text(&call(&h, "wait_for_message", json!({"timeout_secs": 5})).await);
    assert!(
        body.starts_with("new message from ann in #team: sent before the session\n"),
        "{body}"
    );
    assert!(body.contains("\"channel\":\"team\""), "{body}");
    assert!(!body.contains("before join"), "{body}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn wait_delivery_advances_shared_cursor(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    run_join(
        cfg.path(),
        &a,
        dir.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        &mut Vec::new(),
    )
    .await
    .unwrap();
    post_human(&a, &ch, "once only").await;

    let h = harness_in(cfg, dir, a, &[&ch]).await;
    let body = text(&call(&h, "wait_for_message", json!({"timeout_secs": 5})).await);
    assert!(body.contains("once only"), "{body}");

    // `ah wait` on the same folder must not print what the MCP session already delivered.
    let args = WaitArgs {
        folder: Some(h.dir.path().to_path_buf()),
        channel: None,
        name: Some("waiter".into()),
        drain: None,
    };
    let mut out = Vec::new();
    let second = tokio::time::timeout(
        Duration::from_millis(800),
        run_wait(h.cfg.path(), &h.session.client, args, &mut out),
    )
    .await;
    assert!(second.is_err(), "ah wait returned {second:?}");
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
}

/// A restarted session must not announce the agent's own reply as a new message.
#[sqlx::test(migrations = "../hub/migrations")]
async fn restart_does_not_redeliver_own_reply(pool: PgPool) {
    let hub = spawn_hub(pool.clone(), HubMode::Open).await;
    let a = user(&pool, &hub, "a@b.co").await;
    let a2 = same_user(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    let cfg = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    run_join(
        cfg.path(),
        &a,
        dir.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        &mut Vec::new(),
    )
    .await
    .unwrap();
    post_human(&a, &ch, "question").await;

    let h = harness_in(cfg, dir, a, &[&ch]).await;
    let got = text(&call(&h, "wait_for_message", json!({"timeout_secs": 5})).await);
    assert!(got.contains("question"), "{got}");
    let sent = call(
        &h,
        "send_message",
        json!({"channel": "team", "body": "my reply"}),
    )
    .await;
    assert!(!is_error(&sent), "{}", text(&sent));
    // Let the echo of the reply reach the session before it goes away.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let Harness { cfg, dir, ws, .. } = h;
    ws.abort();

    post_human(&a2, &ch, "follow-up").await;
    let h2 = harness_in(cfg, dir, a2, &[&ch]).await;
    let got = text(&call(&h2, "wait_for_message", json!({"timeout_secs": 5})).await);
    assert!(got.contains("follow-up"), "{got}");
    assert!(!got.contains("my reply"), "{got}");
}
