use ah::{
    client::HubClient,
    commands::auth::{run_auth, run_logout},
    config::{CliConfig, Credentials},
};
use ah_hub::{
    config::{HubConfig, HubMode},
    routes::app,
    state::AppState,
};
use axum::{
    Json, Router,
    routing::{get, post},
};
use sqlx::PgPool;
use std::{
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Serves the real hub router on a TCP port; returns its base URL.
async fn spawn_hub(pool: PgPool) -> String {
    let google = Router::new()
        .route(
            "/token",
            post(|| async { Json(serde_json::json!({"access_token": "at"})) }),
        )
        .route(
            "/userinfo",
            get(|| async {
                Json(serde_json::json!({"email": "a@b.co", "email_verified": true, "name": "Ann"}))
            }),
        );
    let gl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let google_url = format!("http://{}", gl.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(gl, google).await.unwrap() });

    let cfg = HubConfig {
        database_url: String::new(),
        hub_mode: HubMode::Open,
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        public_url: "http://hub.test".into(),
        google_base_url: Some(google_url),
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

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `ah auth login --no-browser` while a helper plays the browser: it reads the printed URL, GETs
/// the confirm page, checks it shows the printed verification code, confirms (hub redirects to
/// Google), then completes the Google callback.
async fn login(hub: &str, dir: &Path, claude: Option<&Path>) -> String {
    let cfg = CliConfig {
        hub_url: hub.to_string(),
    };
    let mut out = SharedBuf::default();
    let seen = out.clone();
    let browser = tokio::spawn(async move {
        let url = loop {
            let text = String::from_utf8(seen.0.lock().unwrap().clone()).unwrap();
            if let Some(l) = text.lines().find(|l| l.contains("/auth/start?code=")) {
                break l.trim().to_string();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let page = http.get(&url).send().await.unwrap();
        assert!(page.status().is_success());
        let page = page.text().await.unwrap();
        let (base, code) = url.split_once("/auth/start?code=").unwrap();
        let printed = String::from_utf8(seen.0.lock().unwrap().clone()).unwrap();
        let user_code = ah_common::user_code(code);
        assert!(
            printed.contains(&format!("Verification code: {user_code}")),
            "{printed}"
        );
        assert!(page.contains(&user_code), "{page}");
        let res = http
            .post(format!("{base}/auth/confirm"))
            .header("origin", "http://hub.test")
            .form(&[("code", code)])
            .send()
            .await
            .unwrap();
        assert!(res.status().is_redirection());
        let cb = format!("{base}/auth/callback?code=gc&state={code}");
        let res = http.get(&cb).send().await.unwrap();
        assert!(res.status().is_success());
    });
    run_auth(
        dir,
        &cfg,
        true,
        Duration::from_millis(20),
        Duration::from_secs(10),
        claude,
        &mut out,
    )
    .await
    .unwrap();
    browser.await.unwrap();
    String::from_utf8(out.0.lock().unwrap().clone()).unwrap()
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn auth_flow_saves_credentials(pool: PgPool) {
    let hub = spawn_hub(pool).await;
    let tmp = tempfile::tempdir().unwrap();
    let claude = tempfile::tempdir().unwrap();
    let text = login(&hub, tmp.path(), Some(claude.path())).await;
    assert!(text.contains("Logged in as a@b.co"), "{text}");
    assert!(text.contains("claude code hooks installed"), "{text}");
    let settings = std::fs::read_to_string(claude.path().join("settings.json")).unwrap();
    assert!(
        settings.contains("\"ah hook stop\"") && settings.contains("\"ah hook session-start\""),
        "{settings}"
    );

    let creds = Credentials::load(tmp.path()).unwrap().unwrap();
    assert_eq!(creds.email, "a@b.co");
    let me = HubClient::new(hub.clone(), Some(creds.token))
        .me()
        .await
        .unwrap();
    assert_eq!(me.email, "a@b.co");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(tmp.path().join("credentials.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Second run short-circuits without contacting the hub, but still installs missing hooks.
    let cfg = CliConfig { hub_url: hub };
    let again = |claude: std::path::PathBuf| {
        let cfg = cfg.clone();
        let dir = tmp.path().to_path_buf();
        async move {
            let mut out = Vec::new();
            run_auth(
                &dir,
                &cfg,
                true,
                Duration::from_millis(20),
                Duration::from_secs(1),
                Some(&claude),
                &mut out,
            )
            .await
            .unwrap();
            String::from_utf8(out).unwrap()
        }
    };
    let fresh = tempfile::tempdir().unwrap();
    let text = again(fresh.path().to_path_buf()).await;
    assert!(text.contains("Already logged in as a@b.co"), "{text}");
    assert!(text.contains("claude code hooks installed"), "{text}");
    assert_eq!(ah::hooks::status(Some(fresh.path())).unwrap(), "installed");
    // Already installed: no line.
    let text = again(fresh.path().to_path_buf()).await;
    assert!(!text.contains("hooks installed"), "{text}");
}

#[sqlx::test(migrations = "../hub/migrations")]
async fn logout_deletes_and_revokes(pool: PgPool) {
    let hub = spawn_hub(pool).await;
    let tmp = tempfile::tempdir().unwrap();
    login(&hub, tmp.path(), None).await;
    let token = Credentials::load(tmp.path()).unwrap().unwrap().token;
    let client = HubClient::new(hub.clone(), Some(token));
    client.me().await.unwrap();

    let cfg = CliConfig { hub_url: hub };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    run_logout(tmp.path(), &cfg, &mut out, &mut err)
        .await
        .unwrap();
    assert!(err.is_empty());
    assert!(!tmp.path().join("credentials.json").exists());
    let e = client.me().await.unwrap_err();
    assert!(
        matches!(e, ah::client::ClientError::Status { status: 401, .. }),
        "{e}"
    );
}

#[tokio::test]
async fn logout_works_offline() {
    let tmp = tempfile::tempdir().unwrap();
    Credentials {
        token: "t".into(),
        email: "a@b.co".into(),
    }
    .save(tmp.path())
    .unwrap();
    let cfg = CliConfig {
        hub_url: "http://127.0.0.1:1".into(),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    run_logout(tmp.path(), &cfg, &mut out, &mut err)
        .await
        .unwrap();
    assert!(!tmp.path().join("credentials.json").exists());
    assert!(String::from_utf8(err).unwrap().contains("warning"));
}

#[tokio::test]
async fn poll_times_out() {
    // Hub that accepts start and never completes.
    let router = Router::new()
        .route(
            "/api/auth/cli/start",
            post(|| async { axum::http::StatusCode::NO_CONTENT }),
        )
        .route(
            "/api/auth/cli/poll",
            get(|| async { axum::http::StatusCode::NO_CONTENT }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    let tmp = tempfile::tempdir().unwrap();
    let cfg = CliConfig { hub_url: hub };
    let mut out = Vec::new();
    let r = run_auth(
        tmp.path(),
        &cfg,
        true,
        Duration::from_millis(10),
        Duration::from_millis(100),
        None,
        &mut out,
    )
    .await;
    assert!(r.unwrap_err().to_string().contains("timed out"));
}
