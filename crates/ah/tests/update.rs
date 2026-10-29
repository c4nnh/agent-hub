use ah::commands::update::{install_binary, run_update, target_for, target_triple};
use axum::{Router, extract::State, http::header, routing::get};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Fake {
    tag: String,
    with_asset: bool,
    archive: Vec<u8>,
    asset_hits: AtomicUsize,
    base: std::sync::OnceLock<String>,
}

/// Archive with a single root entry named like the release binary.
fn make_archive(content: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let bin = if cfg!(windows) { "ah.exe" } else { "ah" };
    std::fs::write(dir.path().join(bin), content).unwrap();
    let archive = dir.path().join("a.tar.gz");
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(dir.path())
        .arg(bin)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    std::fs::read(archive).unwrap()
}

fn asset_name() -> String {
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    format!("ah-{}.{ext}", target_triple().unwrap())
}

async fn spawn(tag: &str, with_asset: bool, content: &str) -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake {
        tag: tag.into(),
        with_asset,
        archive: make_archive(content),
        asset_hits: AtomicUsize::new(0),
        base: Default::default(),
    });
    async fn release(State(f): State<Arc<Fake>>) -> axum::Json<serde_json::Value> {
        let assets = if f.with_asset {
            let name = asset_name();
            let url = format!("{}/dl/{name}", f.base.get().unwrap());
            serde_json::json!([{ "name": name, "browser_download_url": url }])
        } else {
            serde_json::json!([])
        };
        axum::Json(serde_json::json!({ "tag_name": f.tag, "assets": assets }))
    }
    async fn asset(State(f): State<Arc<Fake>>) -> impl axum::response::IntoResponse {
        f.asset_hits.fetch_add(1, Ordering::SeqCst);
        (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            f.archive.clone(),
        )
    }
    let app = Router::new()
        .route("/repos/c4nnh/agent-hub/releases/latest", get(release))
        .route("/repos/c4nnh/agent-hub/releases/tags/{tag}", get(release))
        .route("/dl/{name}", get(asset))
        .with_state(fake.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    fake.base.set(base.clone()).unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (base, fake)
}

#[test]
fn target_triple_maps_known_platforms() {
    assert_eq!(
        target_for("macos", "aarch64").unwrap(),
        "aarch64-apple-darwin"
    );
    assert_eq!(
        target_for("macos", "x86_64").unwrap(),
        "x86_64-apple-darwin"
    );
    assert_eq!(
        target_for("linux", "x86_64").unwrap(),
        "x86_64-unknown-linux-gnu"
    );
    assert_eq!(
        target_for("windows", "x86_64").unwrap(),
        "x86_64-pc-windows-msvc"
    );
    let err = target_for("freebsd", "riscv64").unwrap_err().to_string();
    assert!(err.contains("freebsd") && err.contains("riscv64"), "{err}");
    assert!(target_triple().is_ok());
}

#[tokio::test]
async fn update_downloads_and_installs() {
    let (base, fake) = spawn("v9.9.9", true, "new-binary").await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("ah");
    std::fs::write(&dest, "old-binary").unwrap();
    let mut out = Vec::new();
    run_update(&base, None, &dest, &mut out).await.unwrap();
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new-binary");
    assert_eq!(fake.asset_hits.load(Ordering::SeqCst), 1);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains(&format!("updated {} -> 9.9.9", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
}

#[tokio::test]
async fn already_up_to_date_noop() {
    let current = env!("CARGO_PKG_VERSION");
    let (base, fake) = spawn(&format!("v{current}"), true, "new-binary").await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("ah");
    std::fs::write(&dest, "old-binary").unwrap();
    let mut out = Vec::new();
    run_update(&base, None, &dest, &mut out).await.unwrap();
    assert_eq!(fake.asset_hits.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old-binary");
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains(&format!("already up to date ({current})")),
        "{text}"
    );
}

#[tokio::test]
async fn missing_asset_is_readable_error() {
    let (base, _fake) = spawn("v9.9.9", false, "x").await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("ah");
    std::fs::write(&dest, "old-binary").unwrap();
    let err = run_update(&base, None, &dest, &mut Vec::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(&asset_name()), "{err}");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old-binary");
}

#[cfg(unix)]
#[test]
fn install_binary_remove_first() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("ah");
    let new = dir.path().join("ah.new");
    std::fs::write(&dest, "old").unwrap();
    std::fs::write(&new, "new").unwrap();
    install_binary(&new, &dest).unwrap();
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
    assert_eq!(
        std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!new.exists());
}

#[cfg(unix)]
#[test]
fn install_binary_failure_removes_staged_file() {
    let dir = tempfile::tempdir().unwrap();
    // A non-empty directory at dest makes remove_file fail.
    let dest = dir.path().join("ah");
    std::fs::create_dir(&dest).unwrap();
    std::fs::write(dest.join("x"), "x").unwrap();
    let new = dir.path().join("ah.new");
    std::fs::write(&new, "new").unwrap();
    assert!(install_binary(&new, &dest).is_err());
    assert!(!new.exists());
}

#[tokio::test]
async fn explicit_version_without_v_prefix_resolves() {
    let (base, fake) = spawn("v9.9.9", true, "new-binary").await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("ah");
    std::fs::write(&dest, "old-binary").unwrap();
    run_update(&base, Some("9.9.9"), &dest, &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(fake.asset_hits.load(Ordering::SeqCst), 1);
}
