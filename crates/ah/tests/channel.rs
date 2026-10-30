use ah::{
    bindings::Bindings,
    client::HubClient,
    commands::channel::{
        ConfigArgs, run_config, run_create, run_invitation, run_invite, run_join, run_leave,
        run_leave_account, run_status,
    },
    cursors::Cursors,
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
        run_join(
            dir.path(),
            &c,
            cwd.path(),
            Some("team"),
            &ah::hooks::HookDirs::default(),
            ah::commands::guide::Harness::Unknown,
            &mut out,
        )
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
    run_invitation(&guest, None, false, &mut out).await.unwrap();
    let listing = text(out);
    assert!(
        listing.contains("team") && listing.contains("a@b.co"),
        "{listing}"
    );

    let inv = guest.list_invitations().await.unwrap();
    let mut out = Vec::new();
    run_invitation(&guest, Some((inv[0].id, true)), false, &mut out)
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
    let c = client(&pool, &hub, "a@b.co").await;
    let dir = tempfile::tempdir().unwrap();
    let (f1, f2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let team = c.create_channel("team").await.unwrap();
    let other = c.create_channel("other").await.unwrap();
    let mut out = Vec::new();
    for (f, ch) in [(&f1, "team"), (&f1, "other"), (&f2, "team")] {
        run_join(
            dir.path(),
            &c,
            f.path(),
            Some(ch),
            &ah::hooks::HookDirs::default(),
            ah::commands::guide::Harness::Unknown,
            &mut out,
        )
        .await
        .unwrap();
    }
    let k1 = Bindings::key(f1.path()).unwrap();

    // Two channels bound: --channel picks one; the other binding and cursor stay.
    let mut out = Vec::new();
    run_leave(dir.path(), &c, f1.path(), Some("team"), &mut out)
        .await
        .unwrap();
    assert_eq!(text(out), format!("unbound {k1} from channel team\n"));
    let b = Bindings::load(dir.path()).unwrap();
    assert_eq!(b.for_folder(f1.path()).unwrap().channel_ids, vec![other.id]);
    assert!(
        b.for_folder(f2.path())
            .unwrap()
            .channel_ids
            .contains(&team.id)
    );
    let cursors = Cursors::load_folder(dir.path(), &k1).unwrap();
    assert!(!cursors.contains_key(&team.id) && cursors.contains_key(&other.id));
    // Membership untouched.
    assert_eq!(c.list_channels().await.unwrap().len(), 2);

    // Not bound to it any more: refused.
    let r = run_leave(dir.path(), &c, f1.path(), Some("team"), &mut Vec::new()).await;
    assert!(
        r.unwrap_err()
            .to_string()
            .contains("not bound to channel team")
    );

    // Last channel: one bound, no name needed; the folder entry is dropped.
    run_leave(dir.path(), &c, f1.path(), None, &mut Vec::new())
        .await
        .unwrap();
    assert!(
        Bindings::load(dir.path())
            .unwrap()
            .for_folder(f1.path())
            .is_none()
    );
    assert!(Cursors::load_folder(dir.path(), &k1).unwrap().is_empty());
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn leave_account_removes_membership(pool: PgPool) {
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
    run_invitation(&guest, Some((inv[0].id, true)), false, &mut out)
        .await
        .unwrap();

    run_join(
        dir.path(),
        &owner,
        f1.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut out,
    )
    .await
    .unwrap();
    run_join(
        dir.path(),
        &owner,
        f1.path(),
        Some("other"),
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut out,
    )
    .await
    .unwrap();
    run_join(
        dir.path(),
        &owner,
        f2.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut out,
    )
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
    let r = run_leave_account(dir.path(), &owner, Some("team"), true, &mut out).await;
    assert!(r.is_err());
    let b = Bindings::load(dir.path()).unwrap();
    assert!(
        b.for_folder(f2.path())
            .unwrap()
            .channel_ids
            .contains(&team_id)
    );

    // Guest leaves: removed from every folder binding it appears in.
    run_join(
        dir.path(),
        &guest,
        f1.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut out,
    )
    .await
    .unwrap();
    let mut out = Vec::new();
    run_leave_account(dir.path(), &guest, Some("team"), true, &mut out)
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
    run_join(
        dir.path(),
        &c,
        cwd.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut out,
    )
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

#[sqlx::test(migrations = "../hub/migrations")]
async fn invitation_list_prints_without_tty(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let owner = client(&pool, &hub, "a@b.co").await;
    let guest = client(&pool, &hub, "g@b.co").await;
    let mut out = Vec::new();
    run_create(&owner, "team", &mut out).await.unwrap();
    run_invite(&owner, "team", "g@b.co", &mut out)
        .await
        .unwrap();
    let inv = guest.list_invitations().await.unwrap();

    let mut out = Vec::new();
    run_invitation(&guest, None, false, &mut out).await.unwrap();
    let listing = text(out);
    assert!(listing.contains(&inv[0].id.to_string()), "{listing}");
    assert!(
        listing.contains("team") && listing.contains("a@b.co"),
        "{listing}"
    );
    // Listing alone does not answer the invitation.
    assert_eq!(guest.list_invitations().await.unwrap().len(), 1);
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn config_folder_refuses_overwrite(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let c = client(&pool, &hub, "a@b.co").await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut out = Vec::new();
    run_create(&c, "team", &mut out).await.unwrap();
    run_create(&c, "other", &mut out).await.unwrap();
    run_join(
        dir.path(),
        &c,
        a.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut out,
    )
    .await
    .unwrap();
    run_join(
        dir.path(),
        &c,
        b.path(),
        Some("other"),
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut out,
    )
    .await
    .unwrap();
    let before = Bindings::load(dir.path()).unwrap();

    let r = run_config(
        dir.path(),
        a.path(),
        ConfigArgs {
            folder: Some(b.path().to_path_buf()),
            add_extra_folder: None,
            remove_extra_folder: None,
        },
        &mut out,
    );
    assert!(r.unwrap_err().to_string().contains("already bound"));
    assert_eq!(Bindings::load(dir.path()).unwrap(), before);
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn join_seeds_cursor_once(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let c = client(&pool, &hub, "a@b.co").await;
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let folder = Bindings::key(cwd.path()).unwrap();
    let ch = c.create_channel("team").await.unwrap();
    c.post_human_message(ch.id, "ann", "old").await.unwrap();
    let last = c
        .post_human_message(ch.id, "ann", "older")
        .await
        .unwrap()
        .id;

    run_join(
        dir.path(),
        &c,
        cwd.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut Vec::new(),
    )
    .await
    .unwrap();
    let seeded = Cursors::load_folder(dir.path(), &folder).unwrap();
    assert_eq!(seeded.get(&ch.id), Some(&last));

    // Joining again after more posts keeps the cursor: it marks delivery history.
    c.post_human_message(ch.id, "ann", "newer").await.unwrap();
    run_join(
        dir.path(),
        &c,
        cwd.path(),
        Some("team"),
        &ah::hooks::HookDirs::default(),
        ah::commands::guide::Harness::Unknown,
        &mut Vec::new(),
    )
    .await
    .unwrap();
    let again = Cursors::load_folder(dir.path(), &folder).unwrap();
    assert_eq!(again.get(&ch.id), Some(&last));
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn join_installs_claude_code_hooks(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let c = client(&pool, &hub, "a@b.co").await;
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir(&claude).unwrap();
    c.create_channel("team").await.unwrap();

    let mut out = Vec::new();
    run_join(
        dir.path(),
        &c,
        cwd.path(),
        Some("team"),
        &ah::hooks::HookDirs {
            claude: Some(claude.clone()),
            cursor: None,
        },
        ah::commands::guide::Harness::Unknown,
        &mut out,
    )
    .await
    .unwrap();
    assert!(text(out).contains("claude code hooks installed"));
    let settings = std::fs::read_to_string(claude.join("settings.json")).unwrap();
    assert!(settings.contains("ah hook stop") && settings.contains("ah hook session-start"));

    // Second join: nothing to install, a "present" line.
    let mut out = Vec::new();
    run_join(
        dir.path(),
        &c,
        cwd.path(),
        Some("team"),
        &ah::hooks::HookDirs {
            claude: Some(claude.clone()),
            cursor: None,
        },
        ah::commands::guide::Harness::Unknown,
        &mut out,
    )
    .await
    .unwrap();
    let out = text(out);
    assert!(!out.contains("hooks installed"), "{out}");
    assert!(out.contains("claude code hooks present"), "{out}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn join_reports_reachability_per_harness(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let c = client(&pool, &hub, "a@b.co").await;
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    c.create_channel("team").await.unwrap();
    let home = tempfile::tempdir().unwrap();
    let join = |dirs: ah::hooks::HookDirs| {
        let (dir, cwd, c) = (dir.path().to_path_buf(), cwd.path().to_path_buf(), &c);
        async move {
            let mut out = Vec::new();
            run_join(
                &dir,
                c,
                &cwd,
                Some("team"),
                &dirs,
                ah::commands::guide::Harness::Unknown,
                &mut out,
            )
            .await
            .unwrap();
            text(out)
        }
    };
    use ah::hooks::HookDirs;

    // Neither app dir: the manual listening rule.
    let out = join(HookDirs::detect_in(home.path(), false, false)).await;
    assert!(out.contains("no editor hooks installed"), "{out}");
    assert!(
        out.contains("wait_for_message") && out.contains("ah daemon"),
        "{out}"
    );

    // Only ~/.cursor: the cursor hook, no fallback.
    std::fs::create_dir(home.path().join(".cursor")).unwrap();
    let out = join(HookDirs::detect_in(home.path(), false, false)).await;
    assert!(
        out.contains("cursor hook installed (ah hook cursor-stop)"),
        "{out}"
    );
    assert!(
        !out.contains("claude") && !out.contains("no editor hooks"),
        "{out}"
    );

    // Both: both named.
    std::fs::create_dir(home.path().join(".claude")).unwrap();
    let out = join(HookDirs::detect_in(home.path(), false, false)).await;
    assert!(out.contains("claude code hooks installed"), "{out}");
    assert!(
        out.contains("cursor hook present (ah hook cursor-stop)"),
        "{out}"
    );

    // AH_NO_HOOKS=1: nothing applies, the rule again.
    let out = join(HookDirs::detect_in(home.path(), true, true)).await;
    assert!(out.contains("no editor hooks installed"), "{out}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn join_prints_harness_protocol(pool: PgPool) {
    use ah::commands::guide::{Harness, guide_text};
    let hub = spawn_hub(pool.clone()).await;
    let c = client(&pool, &hub, "a@b.co").await;
    c.create_channel("team").await.unwrap();

    for (harness, first_step) in [
        (
            Harness::ClaudeCode,
            "start `ah wait` as a background Bash command",
        ),
        (
            Harness::Cursor,
            "injected automatically at the end of each of your turns",
        ),
        (Harness::Unknown, "loop the wait_for_message MCP tool"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        run_join(
            dir.path(),
            &c,
            cwd.path(),
            Some("team"),
            &ah::hooks::HookDirs::default(),
            harness,
            &mut out,
        )
        .await
        .unwrap();
        let out = text(out);
        assert!(out.contains("AGENT PROTOCOL"), "{out}");
        assert!(out.contains(first_step), "{out}");
        // Report lines first, then one blank line, then the shared block at the very end.
        assert!(
            out.ends_with(&format!("\n\n{}\n", guide_text(harness))),
            "{out}"
        );
    }
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn status_prints_delivery_per_bound_channel(pool: PgPool) {
    let hub = spawn_hub(pool.clone()).await;
    let c = client(&pool, &hub, "a@b.co").await;
    let dir = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let ch = c.create_channel("team").await.unwrap();
    let other = c.create_channel("other").await.unwrap();
    let mut bindings = Bindings::default();
    bindings.0.insert(
        Bindings::key(folder.path()).unwrap(),
        ah::bindings::Binding {
            channel_ids: vec![ch.id],
            ..Default::default()
        },
    );
    bindings.save(dir.path()).unwrap();
    c.post_human_message(ch.id, "ann", "hi").await.unwrap();
    let latest = c
        .post_human_message(ch.id, "ann", "again")
        .await
        .unwrap()
        .id;
    c.report_agent_cursor(ch.id, "caught-up", latest).await;
    c.report_agent_cursor(ch.id, "laggard", latest - 1).await;

    let mut out = Vec::new();
    run_status(dir.path(), &c, folder.path(), None, &mut out)
        .await
        .unwrap();
    assert_eq!(
        text(out),
        format!(
            "#team latest message id {latest}\n\
             caught-up: read up to {latest} of {latest}, offline, unreachable while idle\n\
             laggard: read up to {} of {latest}, offline, behind, unreachable while idle\n",
            latest - 1
        )
    );

    // Only bound channels are accepted by name.
    let refused = run_status(
        dir.path(),
        &c,
        folder.path(),
        Some(&other.name),
        &mut Vec::new(),
    )
    .await;
    assert!(refused.is_err());
}

#[test]
fn delivery_lines_name_coverage_for_offline_agents_only() {
    let agent = |name: &str, online, covered| ah_common::AgentDelivery {
        agent_name: name.into(),
        last_read_id: 5,
        updated_at: None,
        online,
        covered,
    };
    let state = ah_common::DeliveryState {
        last_message_id: 5,
        agents: vec![
            agent("a", true, false),
            agent("b", true, true),
            agent("c", false, true),
            agent("d", false, false),
        ],
    };
    assert_eq!(
        ah::commands::channel::format_delivery("t", &state),
        "#t latest message id 5\n\
         a: read up to 5 of 5, online\n\
         b: read up to 5 of 5, online\n\
         c: read up to 5 of 5, offline, covered\n\
         d: read up to 5 of 5, offline, unreachable while idle"
    );
}
