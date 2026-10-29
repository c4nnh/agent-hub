use ah::{
    bindings::{Binding, Bindings},
    client::HubClient,
    commands::{
        channel::run_join,
        wait::{Refusal, WaitArgs, Waiter, run_wait},
    },
};
use ah_common::{PostMessage, SenderType};
use ah_hub::{
    auth::issue_token,
    config::{HubConfig, HubMode},
    routes::app,
    state::AppState,
    ws::Registry,
};
use sqlx::PgPool;
use std::{path::Path, time::Duration};
use uuid::Uuid;

const ME: &str = "waiter";
const LIMIT: Duration = Duration::from_secs(10);

async fn spawn_hub(pool: PgPool, registry: Registry) -> String {
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

async fn post(c: &HubClient, channel: Uuid, sender: &str, body: &str) {
    let msg = PostMessage {
        body: body.into(),
        reply_to: None,
        mentions: vec![],
        sender_type: SenderType::Agent,
        sender_name: sender.into(),
    };
    c.post_message(channel, &msg).await.unwrap();
}

struct Env {
    c: HubClient,
    channel: Uuid,
    cfg: tempfile::TempDir,
    folder: tempfile::TempDir,
}

async fn env(pool: &PgPool) -> Env {
    env_with(pool, Registry::default()).await
}

async fn env_with(pool: &PgPool, registry: Registry) -> Env {
    let hub = spawn_hub(pool.clone(), registry).await;
    let c = client(pool, &hub, "a@b.co").await;
    let channel = c.create_channel("team").await.unwrap().id;
    let cfg = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let mut b = Bindings::default();
    b.0.insert(
        Bindings::key(folder.path()).unwrap(),
        Binding {
            channel_ids: vec![channel],
            extra_folders: vec![],
        },
    );
    b.save(cfg.path()).unwrap();
    Env {
        c,
        channel,
        cfg,
        folder,
    }
}

async fn wait_once(
    e: &Env,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
    wait_once_with(e, None).await
}

async fn wait_once_with(
    e: &Env,
    drain: Option<Duration>,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
    let args = WaitArgs {
        folder: Some(e.folder.path().to_path_buf()),
        channel: None,
        name: Some(ME.into()),
        drain,
    };
    let mut out = Vec::new();
    tokio::time::timeout(LIMIT, run_wait(e.cfg.path(), &e.c, args, &mut out))
        .await
        .expect("wait did not finish")?;
    Ok(String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect())
}

/// Resolves once the first run has saved its cursors, i.e. wait is about to park.
async fn cursor_saved(cfg: &Path) {
    while !cfg.join("wait-cursors.json").exists() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn first_run_initializes_cursor_and_parks(pool: PgPool) {
    let e = env(&pool).await;
    post(&e.c, e.channel, "other", "old history").await;
    let poster = async {
        cursor_saved(e.cfg.path()).await;
        post(&e.c, e.channel, "other", "fresh").await;
    };
    let (got, ()) = tokio::join!(wait_once(&e), poster);
    let got = got.unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["body"], "fresh");
    assert_eq!(got[0]["channel"], "team");
    assert_eq!(got[0]["sender_name"], "other");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn gapfill_prints_without_ws(pool: PgPool) {
    let e = env(&pool).await;
    // Seed the cursor with a run that wakes on one message.
    let seed = async {
        cursor_saved(e.cfg.path()).await;
        post(&e.c, e.channel, "other", "seed").await;
    };
    let (first, ()) = tokio::join!(wait_once(&e), seed);
    first.unwrap();
    post(&e.c, e.channel, "other", "while offline").await;
    let got = wait_once(&e).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["body"], "while offline");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn self_messages_filtered(pool: PgPool) {
    let e = env(&pool).await;
    let poster = async {
        cursor_saved(e.cfg.path()).await;
        post(&e.c, e.channel, ME, "my own").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        post(&e.c, e.channel, "other", "theirs").await;
    };
    let (got, ()) = tokio::join!(wait_once(&e), poster);
    let got = got.unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["body"], "theirs");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn refusal_exits_2(pool: PgPool) {
    let e = env(&pool).await;
    e.c.logout().await.unwrap();
    let err = wait_once(&e).await.unwrap_err();
    assert!(err.downcast_ref::<Refusal>().is_some(), "{err}");
    assert!(err.to_string().contains("ah auth login"), "{err}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn cursor_advances(pool: PgPool) {
    let e = env(&pool).await;
    let poster = async {
        cursor_saved(e.cfg.path()).await;
        post(&e.c, e.channel, "other", "one").await;
    };
    let (first, ()) = tokio::join!(wait_once(&e), poster);
    assert_eq!(first.unwrap().len(), 1);
    // Nothing new: a second run must park, not reprint "one".
    let second = tokio::time::timeout(Duration::from_millis(800), wait_once(&e)).await;
    assert!(second.is_err(), "second run returned {second:?}");
}

/// The hub closes the socket (revoked token) while wait is draining after the first message.
/// The accepted message must still be printed.
#[sqlx::test(migrations = "../hub/migrations")]
async fn drain_error_still_prints_accepted_message(pool: PgPool) {
    let registry = Registry::with_tuning(256, Duration::from_millis(20));
    let e = env_with(&pool, registry).await;
    let poster = async {
        cursor_saved(e.cfg.path()).await;
        // Presence shows once the waiter is subscribed.
        while e.c.list_agents(e.channel).await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        post(&e.c, e.channel, "other", "last words").await;
        // The drain window is 5s here, so the close always lands inside it. The pause only has
        // to be long enough for the push to reach the waiter first, even on a loaded machine.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        e.c.logout().await.unwrap();
    };
    let (got, ()) = tokio::join!(wait_once_with(&e, Some(Duration::from_secs(5))), poster);
    let got = got.unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["body"], "last words");
}

/// Channel A yields a message, channel B's fetch fails: A's message stays buffered.
#[sqlx::test(migrations = "../hub/migrations")]
async fn gap_check_keeps_messages_when_a_later_channel_fails(pool: PgPool) {
    let e = env(&pool).await;
    let hub_url = e.c.url("");
    let other = client(&pool, hub_url.trim_end_matches('/'), "c@d.co").await;
    let foreign = other.create_channel("foreign").await.unwrap();
    let mine = e.c.list_channels().await.unwrap();
    post(&e.c, e.channel, "other", "on A").await;
    let mut channels = mine;
    channels.push(foreign);
    let mut w = Waiter {
        client: &e.c,
        agent_name: ME.into(),
        folder: String::new(),
        cursors: channels.iter().map(|c| (c.id, 0)).collect(),
        channels,
        pending: Vec::new(),
        drain: Duration::from_millis(200),
    };
    assert!(w.gap_check().await.is_err());
    assert_eq!(w.pending.len(), 1);
    assert_eq!(w.pending[0].body, "on A");
}

/// A message sent after join but before the first `ah wait` run is delivered, not swallowed.
#[sqlx::test(migrations = "../hub/migrations")]
async fn message_after_join_before_first_wait_is_delivered(pool: PgPool) {
    let e = env(&pool).await;
    post(&e.c, e.channel, "other", "before join").await;
    run_join(
        e.cfg.path(),
        &e.c,
        e.folder.path(),
        Some("team"),
        &mut Vec::new(),
    )
    .await
    .unwrap();
    post(&e.c, e.channel, "other", "after join").await;
    let got = wait_once(&e).await.unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["body"], "after join");
}
