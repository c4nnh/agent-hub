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
    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: HubMode::Open,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: None,
    };
    let router = app(AppState::new(
        pool,
        cfg,
        std::sync::Arc::new(ah_hub::embed::HashEmbedder),
    ));
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
        _ = tokio::time::timeout(Duration::from_secs(10), scenario) => {}
    }
}
