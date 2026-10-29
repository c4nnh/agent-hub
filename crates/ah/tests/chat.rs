use ah::{
    client::HubClient,
    commands::chat::{ROBOT_PREFIX, render_message, subscribe_loop},
};
use ah_common::{Message, SenderType};
use ah_hub::{
    auth::issue_token,
    config::{HubConfig, HubMode},
    routes::app,
    state::AppState,
    ws::Registry,
};
use sqlx::PgPool;
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

fn msg(sender_type: SenderType, name: &str, body: &str) -> Message {
    Message {
        id: 1,
        channel_id: Uuid::nil(),
        sender_type,
        sender_name: name.into(),
        body: body.into(),
        reply_to: None,
        mentions: vec![],
        created_at: chrono::Utc::now(),
    }
}

#[test]
fn render_agent_has_robot_prefix() {
    let line = render_message(&msg(SenderType::Agent, "bot", "hi"));
    assert_eq!(line, format!("{ROBOT_PREFIX}bot: hi"));
    assert!(ROBOT_PREFIX.starts_with('\u{1F916}'));
}

#[test]
fn render_human_plain() {
    assert_eq!(
        render_message(&msg(SenderType::Human, "ann", "yo")),
        "ann: yo"
    );
}

#[test]
fn render_strips_control_characters() {
    let line = render_message(&msg(
        SenderType::Human,
        "ann\x1b]0;pwned\x07",
        "line one\nline two\x1b[2J\u{9b}31m",
    ));
    assert_eq!(line, "ann]0;pwned: line one line two[2J31m");
    assert!(!line.chars().any(char::is_control), "{line:?}");
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    async fn wait_for(&self, needle: &str) {
        for _ in 0..100 {
            if self.text().contains(needle) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{needle:?} never appeared in {:?}", self.text());
    }
}

async fn spawn_hub(pool: PgPool) -> String {
    spawn_hub_with(pool, Registry::default()).await
}

async fn spawn_hub_with(pool: PgPool, registry: Registry) -> String {
    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: HubMode::Open,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: None,
    };
    let mut state = AppState::new(pool, cfg, std::sync::Arc::new(ah_hub::embed::HashEmbedder));
    state.registry = registry;
    let router = app(state);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    url
}

async fn client(pool: &PgPool, hub: &str, email: &str) -> HubClient {
    let id = Uuid::new_v4();
    sqlx::query("insert into users (id, email, name) values ($1, $2, 'N')")
        .bind(id)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    let token = issue_token(pool, id).await.unwrap();
    HubClient::new(hub.to_string(), Some(token))
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn incoming_message_printed(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let a = client(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    // Posted before connecting: must show up as history.
    a.post_human_message(ch.id, "ann", "earlier").await.unwrap();

    let out = Shared::default();
    let mut writer = out.clone();
    let scenario = async {
        out.wait_for("connected").await;
        assert!(out.text().contains("ann: earlier"));
        let body = serde_json::json!({
            "body": "hello", "sender_type": "agent", "sender_name": "bot"
        });
        let res = a
            .request(
                reqwest::Method::POST,
                &format!("/api/channels/{}/messages", ch.id),
            )
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(res.status().is_success());
        out.wait_for(&format!("{ROBOT_PREFIX}bot: hello")).await;
    };
    tokio::select! {
        r = subscribe_loop(&a, &ch, &mut writer) => panic!("loop ended: {r:?}"),
        r = tokio::time::timeout(Duration::from_secs(10), scenario) => r.expect("timed out")
    }
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn revoked_token_stops_chat_with_readable_error(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let a = client(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();
    a.logout().await.unwrap();

    let mut out = Shared::default();
    let ended = tokio::time::timeout(Duration::from_secs(10), subscribe_loop(&a, &ch, &mut out))
        .await
        .expect("chat kept retrying after the token was revoked");
    let err = ended.unwrap_err().to_string();
    assert!(err.contains("ah auth"), "{err}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn live_logout_stops_chat_with_auth_hint(pool: PgPool) {
    let registry = Registry::with_tuning(256, Duration::from_millis(100));
    let hub = spawn_hub_with(pool.clone(), registry).await;
    let a = client(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();

    let out = Shared::default();
    let mut writer = out.clone();
    let chat = subscribe_loop(&a, &ch, &mut writer);
    tokio::pin!(chat);
    tokio::select! {
        r = &mut chat => panic!("loop ended early: {r:?}"),
        _ = out.wait_for("connected") => {}
    }
    a.logout().await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), chat)
        .await
        .expect("chat kept running after logout");
    let err = ended.unwrap_err().to_string();
    assert!(err.contains("ah auth"), "{err}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn reconnect_on_empty_channel_replays_whole_gap(pool: PgPool) {
    // Queue of one: a burst of pushes evicts the subscriber and forces a reconnect.
    let registry = Registry::with_tuning(1, Duration::from_secs(30));
    let hub = spawn_hub_with(pool.clone(), registry.clone()).await;
    let a = client(&pool, &hub, "a@b.co").await;
    let ch = a.create_channel("team").await.unwrap();

    let out = Shared::default();
    let mut writer = out.clone();
    let scenario = async {
        out.wait_for("connected").await;
        // Id 0 is never printed, so these only overflow the queue.
        let mut filler = msg(SenderType::Human, "x", "filler");
        filler.id = 0;
        filler.channel_id = ch.id;
        for _ in 0..3 {
            registry.publish(ch.id, &filler);
        }
        // Posted during the 1s reconnect backoff, so they arrive by gap-fill, not push.
        for i in 0..25 {
            a.post_human_message(ch.id, "ann", &format!("m{i}"))
                .await
                .unwrap();
        }
        out.wait_for("ann: m24").await;
        assert!(out.text().contains("ann: m0\n"), "{}", out.text());
    };
    tokio::select! {
        r = subscribe_loop(&a, &ch, &mut writer) => panic!("loop ended: {r:?}"),
        r = tokio::time::timeout(Duration::from_secs(10), scenario) => r.expect("timed out")
    }
}
