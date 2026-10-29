#![cfg(unix)]

use ah::{
    bindings::{Binding, Bindings},
    client::HubClient,
    commands::daemon::{WAKE_PROMPT, Watch},
    cursors::{Cursors, PidGuard},
    mcp::agent_name,
};
use ah_common::{PostMessage, SenderType};
use ah_hub::{
    auth::issue_token,
    config::{HubConfig, HubMode},
    routes::app,
    state::AppState,
};
use sqlx::PgPool;
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};
use uuid::Uuid;

async fn spawn_hub(pool: PgPool) -> String {
    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: HubMode::Open,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: None,
    };
    let state = AppState::new(pool, cfg, Arc::new(ah_hub::embed::HashEmbedder));
    let router = app(state);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    url
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
    watch: Watch,
    channel: Uuid,
    cfg: tempfile::TempDir,
    folder: String,
    out: std::path::PathBuf,
    _keep: tempfile::TempDir,
    _workdir: tempfile::TempDir,
}

/// A bound folder whose wake command is a script that records its working directory and argv.
async fn env(pool: &PgPool) -> Env {
    let hub = spawn_hub(pool.clone()).await;
    let id = Uuid::new_v4();
    sqlx::query("insert into users (id, email, name) values ($1, 'a@b.co', 'N')")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    let token = issue_token(pool, id).await.unwrap();
    let client = Arc::new(HubClient::new(hub, Some(token)));
    let channel = client.create_channel("team").await.unwrap().id;

    let cfg = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let keep = tempfile::tempdir().unwrap();
    let out = keep.path().join("invocation.txt");
    let script = keep.path().join("wake.sh");
    let body = format!(
        "#!/bin/sh\npwd > {o}.tmp\nprintf '%s\\n' \"$@\" >> {o}.tmp\nmv {o}.tmp {o}\n",
        o = out.display()
    );
    std::fs::write(&script, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let folder = Bindings::key(workdir.path()).unwrap();
    let command = script.display().to_string();
    let mut b = Bindings::default();
    b.0.insert(
        folder.clone(),
        Binding {
            channel_ids: vec![channel],
            wake_command: Some(command.clone()),
            ..Default::default()
        },
    );
    b.save(cfg.path()).unwrap();
    Cursors::seed(cfg.path(), &folder, &BTreeMap::from([(channel, 0)])).unwrap();

    let channels = client.list_channels().await.unwrap();
    let watch = Watch::new(
        client,
        cfg.path().to_path_buf(),
        folder.clone(),
        command,
        channels,
    );
    Env {
        watch,
        channel,
        cfg,
        folder,
        out,
        _keep: keep,
        _workdir: workdir,
    }
}

async fn appeared(path: &Path) -> String {
    for _ in 0..200 {
        if let Ok(s) = std::fs::read_to_string(path) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("wake command did not run");
}

async fn stays_absent(path: &Path) {
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!path.exists(), "wake command ran");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn wakes_once_without_touching_the_cursor(pool: PgPool) {
    let mut e = env(&pool).await;
    post(&e.watch.client, e.channel, "other", "hi").await;

    e.watch.pass().await.unwrap();
    let got = appeared(&e.out).await;
    let mut lines = got.lines();
    assert_eq!(
        std::fs::canonicalize(lines.next().unwrap()).unwrap(),
        std::fs::canonicalize(&e.folder).unwrap()
    );
    assert_eq!(lines.next(), Some(WAKE_PROMPT));
    assert_eq!(lines.next(), None);
    assert!(e.watch.last_woken_id > 0);
    let cursors = Cursors::load_folder(e.cfg.path(), &e.folder).unwrap();
    assert_eq!(cursors, BTreeMap::from([(e.channel, 0)]));

    // The agent exited without advancing the cursor: no re-spawn until new traffic arrives,
    // even with the cooldown out of the way.
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::remove_file(&e.out).unwrap();
    e.watch.last_spawn = None;
    e.watch.pass().await.unwrap();
    stays_absent(&e.out).await;

    post(&e.watch.client, e.channel, "other", "again").await;
    e.watch.pass().await.unwrap();
    appeared(&e.out).await;
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn own_messages_do_not_wake(pool: PgPool) {
    let mut e = env(&pool).await;
    let own = agent_name("cursor", Path::new(&e.folder));
    post(&e.watch.client, e.channel, &own, "mine").await;
    e.watch.pass().await.unwrap();
    stays_absent(&e.out).await;
    assert_eq!(e.watch.last_woken_id, 0);
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn live_listener_blocks_the_wake(pool: PgPool) {
    let mut e = env(&pool).await;
    let _guard = PidGuard::write(e.cfg.path(), &e.folder).unwrap();
    post(&e.watch.client, e.channel, "other", "hi").await;
    e.watch.pass().await.unwrap();
    stays_absent(&e.out).await;
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn delivered_messages_do_not_wake(pool: PgPool) {
    let mut e = env(&pool).await;
    post(&e.watch.client, e.channel, "other", "hi").await;
    let tail = e.watch.client.list_channels().await.unwrap()[0].last_message_id;
    Cursors::advance(
        e.cfg.path(),
        &e.folder,
        &BTreeMap::from([(e.channel, tail)]),
    )
    .unwrap();
    e.watch.pass().await.unwrap();
    stays_absent(&e.out).await;
}
