use ah::{
    bindings::{Binding, Bindings},
    client::HubClient,
    commands::{
        channel::run_join,
        wait::{CURSOR_REFUSAL, Refusal, WaitArgs, Waiter, run_wait},
    },
    cursors::listener_alive,
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
            ..Default::default()
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
        ..Default::default()
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
    let key = Bindings::key(e.folder.path()).unwrap();
    let poster = async {
        cursor_saved(e.cfg.path()).await;
        // Parked: the pidfile marks the folder as listening.
        assert!(listener_alive(e.cfg.path(), &key));
        post(&e.c, e.channel, "other", "fresh").await;
    };
    let (got, ()) = tokio::join!(wait_once(&e), poster);
    let got = got.unwrap();
    assert!(!listener_alive(e.cfg.path(), &key));
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
async fn cursor_env_refuses_before_pidfile_unless_allowed(pool: PgPool) {
    let e = env(&pool).await;
    let args = |allow_cursor| WaitArgs {
        folder: Some(e.folder.path().to_path_buf()),
        name: Some(ME.into()),
        cursor_env: true,
        allow_cursor,
        ..Default::default()
    };
    let mut out = Vec::new();
    let err = run_wait(e.cfg.path(), &e.c, args(false), &mut out)
        .await
        .unwrap_err();
    let refusal = err.downcast_ref::<Refusal>().expect("a Refusal");
    assert_eq!(refusal.0, CURSOR_REFUSAL);
    assert!(
        refusal
            .0
            .starts_with("on Cursor, a background ah wait swallows messages:")
    );
    assert!(!e.cfg.path().join("wait-pids").exists());
    assert!(out.is_empty());

    // With the override it proceeds: nothing refuses it, so it parks until the timeout.
    let mut out = Vec::new();
    let parked = tokio::time::timeout(
        Duration::from_millis(800),
        run_wait(e.cfg.path(), &e.c, args(true), &mut out),
    )
    .await;
    assert!(parked.is_err(), "{parked:?}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn refusal_exits_2(pool: PgPool) {
    let e = env(&pool).await;
    e.c.logout().await.unwrap();
    let err = wait_once(&e).await.unwrap_err();
    assert!(err.downcast_ref::<Refusal>().is_some(), "{err}");
    assert!(err.to_string().contains("ah auth login"), "{err}");
    let key = Bindings::key(e.folder.path()).unwrap();
    assert!(
        !e.cfg
            .path()
            .join("wait-pids")
            .read_dir()
            .unwrap()
            .any(|_| true)
    );
    assert!(!listener_alive(e.cfg.path(), &key));
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
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut Vec::new(),
    )
    .await
    .unwrap();
    post(&e.c, e.channel, "other", "after join").await;
    let got = wait_once(&e).await.unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0]["body"], "after join");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn cursor_stop_delivers_once_and_shares_the_cursor(pool: PgPool) {
    use ah::{cursors::Cursors, hooks};
    let e = env(&pool).await;
    post(&e.c, e.channel, "other", "history").await;
    let input = hooks::HookInput {
        workspace_roots: vec![e.folder.path().to_path_buf()],
        ..Default::default()
    };
    let stop = || async {
        let mut out = Vec::new();
        hooks::run_cursor_stop(e.cfg.path(), Some(&e.c), &input, &mut out)
            .await
            .unwrap();
        String::from_utf8(out).unwrap()
    };
    // First stop seeds the cursor at the tail: history is not announced.
    assert_eq!(stop().await, "");
    let folder = Bindings::key(e.folder.path()).unwrap();
    let seeded = Cursors::load_folder(e.cfg.path(), &folder).unwrap()[&e.channel];

    post(&e.c, e.channel, "ann", "please review").await;
    let text = stop().await;
    let v: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
    let msg = v["followup_message"].as_str().unwrap();
    let want = format!(
        "{}\nnew message from ann in #team: please review\n",
        hooks::CURSOR_FOLLOWUP_OPEN
    );
    assert!(msg.starts_with(&want), "{msg}");
    assert!(msg.ends_with(hooks::CURSOR_FOLLOWUP_CLOSE), "{msg}");
    let advanced = Cursors::load_folder(e.cfg.path(), &folder).unwrap()[&e.channel];
    assert!(advanced > seeded);

    // Delivered: the next stop is quiet, and ah wait does not reprint it.
    assert_eq!(stop().await, "");
    post(&e.c, e.channel, "ann", "next").await;
    let got = wait_once(&e).await.unwrap();
    let bodies: Vec<_> = got.iter().map(|m| m["body"].as_str().unwrap()).collect();
    assert_eq!(bodies, vec!["next"]);
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn cursor_stop_skips_dropped_followups_and_caps_size(pool: PgPool) {
    use ah::{cursors::Cursors, hooks};
    let e = env(&pool).await;
    let folder = Bindings::key(e.folder.path()).unwrap();
    // A file:// root resolves like the plain path.
    let root: std::path::PathBuf = format!("file://{}", e.folder.path().display()).into();
    let stop = |status: &str, loop_count: u32| {
        let input = hooks::HookInput {
            workspace_roots: vec![root.clone()],
            status: Some(status.into()),
            loop_count: Some(loop_count),
            ..Default::default()
        };
        let (cfg, c) = (e.cfg.path().to_path_buf(), &e.c);
        async move {
            let mut out = Vec::new();
            hooks::run_cursor_stop(&cfg, Some(c), &input, &mut out)
                .await
                .unwrap();
            String::from_utf8(out).unwrap()
        }
    };
    let cursor = || Cursors::load_folder(e.cfg.path(), &folder).unwrap()[&e.channel];
    assert_eq!(stop("completed", 0).await, ""); // seeds
    let seeded = cursor();

    let mut ids = Vec::new();
    for i in 1..=25 {
        let m = PostMessage {
            body: format!("m{i}"),
            reply_to: None,
            mentions: vec![],
            sender_type: SenderType::Agent,
            sender_name: "ann".into(),
        };
        ids.push(e.c.post_message(e.channel, &m).await.unwrap().id);
    }

    // Cursor would drop these follow-ups: silent, cursor untouched.
    assert_eq!(stop("aborted", 0).await, "");
    assert_eq!(stop("error", 0).await, "");
    assert_eq!(stop("completed", hooks::CURSOR_LOOP_LIMIT).await, "");
    assert_eq!(cursor(), seeded);

    // 25 pending: 20 lines, a "5 more" line, cursor at the 20th.
    let text = stop("completed", 0).await;
    let v: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
    let msg = v["followup_message"].as_str().unwrap();
    let announced = msg.lines().filter(|l| l.starts_with("new message")).count();
    assert_eq!(announced, hooks::CURSOR_FOLLOWUP_MAX);
    assert!(msg.contains("and 5 more pending"), "{msg}");
    assert_eq!(cursor(), ids[19]);

    // The next stop delivers the remaining 5.
    let text = stop("completed", 1).await;
    let v: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
    let msg = v["followup_message"].as_str().unwrap();
    let bodies: Vec<_> = msg
        .lines()
        .filter_map(|l| l.strip_prefix("new message from ann in #team: "))
        .collect();
    assert_eq!(bodies, vec!["m21", "m22", "m23", "m24", "m25"]);
    assert!(!msg.contains("more pending"), "{msg}");
    assert_eq!(cursor(), ids[24]);
}
