use ah::{
    bindings::Bindings,
    client::HubClient,
    commands::channel::{
        ConfigArgs, run_config, run_create, run_invitation, run_invite, run_join, run_leave,
    },
};
use ah_hub::{
    auth::issue_token,
    config::{HubConfig, HubMode},
    routes::app,
    state::AppState,
};
use sqlx::PgPool;
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

fn text(out: Vec<u8>) -> String {
    String::from_utf8(out).unwrap()
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn create_join_binds_cwd(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let c = client(&pool, &hub, "a@b.co").await;
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let mut out = Vec::new();
    run_create(&c, "team", &mut out).await.unwrap();
    let channels = c.list_channels().await.unwrap();
    assert!(text(out).contains(&channels[0].id.to_string()));

    for _ in 0..2 {
        let mut out = Vec::new();
        run_join(dir.path(), &c, cwd.path(), Some("team"), &mut out)
            .await
            .unwrap();
        assert!(text(out).contains("no active agents"));
    }
    let b = Bindings::load(dir.path()).unwrap();
    let binding = b.for_folder(cwd.path()).unwrap();
    assert_eq!(binding.channel_ids, vec![channels[0].id]);
    assert!(binding.extra_folders.is_empty());
    // Exact match only: a subdirectory is not bound.
    let sub = cwd.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    assert!(b.for_folder(&sub).is_none());
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn invitation_accept_via_flags(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let owner = client(&pool, &hub, "a@b.co").await;
    let guest = client(&pool, &hub, "g@b.co").await;
    let mut out = Vec::new();
    run_create(&owner, "team", &mut out).await.unwrap();
    run_invite(&owner, "team", "g@b.co", &mut out)
        .await
        .unwrap();

    let mut out = Vec::new();
    run_invitation(&guest, None, &mut out).await.unwrap();
    let listing = text(out);
    assert!(
        listing.contains("team") && listing.contains("a@b.co"),
        "{listing}"
    );

    let inv = guest.list_invitations().await.unwrap();
    let mut out = Vec::new();
    run_invitation(&guest, Some((inv[0].id, true)), &mut out)
        .await
        .unwrap();
    let names: Vec<_> = guest
        .list_channels()
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, vec!["team"]);
    assert!(guest.list_invitations().await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn leave_unbinds(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let owner = client(&pool, &hub, "a@b.co").await;
    let guest = client(&pool, &hub, "g@b.co").await;
    let dir = tempfile::tempdir().unwrap();
    let (f1, f2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut out = Vec::new();
    run_create(&owner, "team", &mut out).await.unwrap();
    run_create(&owner, "other", &mut out).await.unwrap();
    run_invite(&owner, "team", "g@b.co", &mut out)
        .await
        .unwrap();
    let inv = guest.list_invitations().await.unwrap();
    run_invitation(&guest, Some((inv[0].id, true)), &mut out)
        .await
        .unwrap();

    run_join(dir.path(), &owner, f1.path(), Some("team"), &mut out)
        .await
        .unwrap();
    run_join(dir.path(), &owner, f1.path(), Some("other"), &mut out)
        .await
        .unwrap();
    run_join(dir.path(), &owner, f2.path(), Some("team"), &mut out)
        .await
        .unwrap();
    let team_id = owner
        .list_channels()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.name == "team")
        .unwrap()
        .id;

    // Owner with members: server says 409, bindings untouched.
    let mut out = Vec::new();
    let r = run_leave(dir.path(), &owner, Some("team"), true, &mut out).await;
    assert!(r.is_err());
    let b = Bindings::load(dir.path()).unwrap();
    assert!(
        b.for_folder(f2.path())
            .unwrap()
            .channel_ids
            .contains(&team_id)
    );

    // Guest leaves: removed from every folder binding it appears in.
    run_join(dir.path(), &guest, f1.path(), Some("team"), &mut out)
        .await
        .unwrap();
    let mut out = Vec::new();
    run_leave(dir.path(), &guest, Some("team"), true, &mut out)
        .await
        .unwrap();
    let b = Bindings::load(dir.path()).unwrap();
    assert!(!b.0.values().any(|v| v.channel_ids.contains(&team_id)));
    // Other channels stay bound.
    assert_eq!(b.for_folder(f1.path()).unwrap().channel_ids.len(), 1);
    assert!(guest.list_channels().await.unwrap().is_empty());
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn config_adds_extra_folder(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let c = client(&pool, &hub, "a@b.co").await;
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let mut out = Vec::new();
    run_create(&c, "team", &mut out).await.unwrap();
    run_join(dir.path(), &c, cwd.path(), Some("team"), &mut out)
        .await
        .unwrap();

    let args = |add: Option<&std::path::Path>, rm: Option<&std::path::Path>| ConfigArgs {
        folder: None,
        add_extra_folder: add.map(|p| p.to_path_buf()),
        remove_extra_folder: rm.map(|p| p.to_path_buf()),
    };
    for _ in 0..2 {
        let mut out = Vec::new();
        run_config(
            dir.path(),
            cwd.path(),
            args(Some(extra.path()), None),
            &mut out,
        )
        .unwrap();
    }
    let canon = std::fs::canonicalize(extra.path()).unwrap();
    let b = Bindings::load(dir.path()).unwrap();
    assert_eq!(
        b.for_folder(cwd.path()).unwrap().extra_folders,
        vec![canon.to_string_lossy().into_owned()]
    );

    let mut out = Vec::new();
    run_config(
        dir.path(),
        cwd.path(),
        args(None, Some(extra.path())),
        &mut out,
    )
    .unwrap();
    let b = Bindings::load(dir.path()).unwrap();
    assert!(b.for_folder(cwd.path()).unwrap().extra_folders.is_empty());

    // Rebind: binding moves to the new folder.
    let newf = tempfile::tempdir().unwrap();
    let mut out = Vec::new();
    run_config(
        dir.path(),
        cwd.path(),
        ConfigArgs {
            folder: Some(newf.path().to_path_buf()),
            add_extra_folder: None,
            remove_extra_folder: None,
        },
        &mut out,
    )
    .unwrap();
    let b = Bindings::load(dir.path()).unwrap();
    assert!(b.for_folder(cwd.path()).is_none());
    assert_eq!(b.for_folder(newf.path()).unwrap().channel_ids.len(), 1);
}
