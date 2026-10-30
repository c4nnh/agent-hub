use ah::{
    bindings::{Binding, Bindings},
    cursors::{PidGuard, listener_alive},
    hooks::{self, HookInput, HookStatus},
};
use serde_json::{Value, json};
use std::path::Path;

fn settings(claude: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(claude.join("settings.json")).unwrap()).unwrap()
}

fn commands(v: &Value, event: &str) -> Vec<String> {
    v["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
        .filter_map(|h| h["command"].as_str().map(String::from))
        .collect()
}

#[test]
fn fresh_install_then_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    assert_eq!(
        hooks::ensure_hooks(Some(&claude)).unwrap(),
        HookStatus::Installed
    );
    let v = settings(&claude);
    assert_eq!(commands(&v, "Stop"), vec!["ah hook stop"]);
    assert_eq!(commands(&v, "SessionStart"), vec!["ah hook session-start"]);
    assert!(v["hooks"]["Stop"][0].get("matcher").is_none());

    let before = std::fs::read(claude.join("settings.json")).unwrap();
    assert_eq!(
        hooks::ensure_hooks(Some(&claude)).unwrap(),
        HookStatus::Unchanged
    );
    assert_eq!(std::fs::read(claude.join("settings.json")).unwrap(), before);
}

#[test]
fn install_preserves_unrelated_entries_and_order() {
    let claude = tempfile::tempdir().unwrap();
    let original = json!({
        "zeta": 1,
        "model": "opus",
        "hooks": {
            "Stop": [
                {"hooks": [{"type": "command", "command": "say done"}]},
                {"hooks": [{"type": "command", "command": "yeah hook x"}]}
            ],
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "lint"}]}]
        },
        "alpha": true
    });
    std::fs::write(
        claude.path().join("settings.json"),
        serde_json::to_vec_pretty(&original).unwrap(),
    )
    .unwrap();
    assert_eq!(
        hooks::ensure_hooks(Some(claude.path())).unwrap(),
        HookStatus::Installed
    );
    let v = settings(claude.path());
    let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, vec!["zeta", "model", "hooks", "alpha"]);
    assert_eq!(
        commands(&v, "Stop"),
        vec!["say done", "yeah hook x", "ah hook stop"]
    );
    assert_eq!(v["hooks"]["PreToolUse"], original["hooks"]["PreToolUse"]);

    // Uninstall removes only ours and restores the original document.
    let removed = hooks::uninstall(Some(claude.path())).unwrap();
    assert_eq!(removed, vec!["ah hook stop", "ah hook session-start"]);
    assert_eq!(settings(claude.path()), original);
    assert!(hooks::uninstall(Some(claude.path())).unwrap().is_empty());
}

#[test]
fn existing_ah_hook_entry_counts_as_installed() {
    let claude = tempfile::tempdir().unwrap();
    let custom = json!({"hooks": {
        "Stop": [{"hooks": [{"type": "command", "command": "/opt/bin/ah hook stop"}]}],
        "SessionStart": [{"hooks": [{"type": "command", "command": "ah hook session-start"}]}]
    }});
    let bytes = serde_json::to_vec(&custom).unwrap();
    std::fs::write(claude.path().join("settings.json"), &bytes).unwrap();
    assert_eq!(
        hooks::ensure_hooks(Some(claude.path())).unwrap(),
        HookStatus::Unchanged
    );
    assert_eq!(
        std::fs::read(claude.path().join("settings.json")).unwrap(),
        bytes
    );
}

#[test]
fn unparseable_settings_left_alone() {
    let claude = tempfile::tempdir().unwrap();
    std::fs::write(claude.path().join("settings.json"), "{ not json").unwrap();
    assert!(hooks::ensure_hooks(Some(claude.path())).is_err());
    assert_eq!(
        std::fs::read_to_string(claude.path().join("settings.json")).unwrap(),
        "{ not json"
    );
}

#[test]
fn detection_and_opt_out() {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    // No ~/.claude, no claude on PATH: nothing to do.
    assert_eq!(hooks::detect(claude.clone(), false, false), None);
    assert_eq!(
        hooks::ensure_hooks(None).unwrap(),
        HookStatus::NotApplicable
    );
    // On PATH only: target ~/.claude (created on install).
    assert_eq!(
        hooks::detect(claude.clone(), true, false),
        Some(claude.clone())
    );
    std::fs::create_dir(&claude).unwrap();
    assert_eq!(
        hooks::detect(claude.clone(), false, false),
        Some(claude.clone())
    );
    // AH_NO_HOOKS=1 wins.
    assert_eq!(hooks::detect(claude.clone(), true, true), None);
    assert!(hooks::opt_out(Some("1".as_ref())));
    assert!(!hooks::opt_out(Some("0".as_ref())));
    assert!(!hooks::opt_out(None));
}

#[test]
fn pidfile_alive_and_stale() {
    let dir = tempfile::tempdir().unwrap();
    let folder = "/some/folder";
    assert!(!listener_alive(dir.path(), folder));
    let guard = PidGuard::write(dir.path(), folder).unwrap();
    assert!(listener_alive(dir.path(), folder));
    drop(guard);
    assert!(!listener_alive(dir.path(), folder));

    // A dead PID: stale, reported as not listening and deleted.
    let guard = PidGuard::write(dir.path(), folder).unwrap();
    let path = guard.path().to_path_buf();
    std::mem::forget(guard);
    std::fs::write(&path, "999999999").unwrap();
    assert!(!listener_alive(dir.path(), folder));
    assert!(!path.exists());
}

#[test]
fn pid_guard_keeps_a_newer_listeners_file() {
    let dir = tempfile::tempdir().unwrap();
    let first = PidGuard::write(dir.path(), "/f").unwrap();
    let path = first.path().to_path_buf();
    std::fs::write(&path, "1").unwrap(); // another `ah wait` took over
    drop(first);
    assert!(path.exists());
}

fn bound_env() -> (tempfile::TempDir, tempfile::TempDir) {
    let cfg = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let mut b = Bindings::default();
    b.0.insert(
        Bindings::key(folder.path()).unwrap(),
        Binding {
            channel_ids: vec![uuid::Uuid::nil()],
            extra_folders: vec![],
            ..Default::default()
        },
    );
    b.save(cfg.path()).unwrap();
    (cfg, folder)
}

#[tokio::test]
async fn stop_hook_blocks_once_without_listener() {
    let (cfg, folder) = bound_env();
    let input = |active| HookInput {
        cwd: Some(folder.path().to_path_buf()),
        stop_hook_active: active,
        ..Default::default()
    };
    let mut out = Vec::new();
    hooks::run_stop(cfg.path(), None, &input(false), &mut out)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["decision"], "block");
    let reason = v["reason"].as_str().unwrap();
    assert!(
        reason.contains("run `ah wait` as a background Bash command"),
        "{reason}"
    );
    assert!(reason.contains(&uuid::Uuid::nil().to_string()), "{reason}");

    // Already a forced continuation: silent.
    let mut out = Vec::new();
    hooks::run_stop(cfg.path(), None, &input(true), &mut out)
        .await
        .unwrap();
    assert!(out.is_empty());

    // Listener alive: silent.
    let key = Bindings::key(folder.path()).unwrap();
    let _g = PidGuard::write(cfg.path(), &key).unwrap();
    let mut out = Vec::new();
    hooks::run_stop(cfg.path(), None, &input(false), &mut out)
        .await
        .unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn hooks_silent_in_unbound_folder() {
    let cfg = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let input = HookInput {
        cwd: Some(folder.path().to_path_buf()),
        stop_hook_active: false,
        ..Default::default()
    };
    let mut out = Vec::new();
    hooks::run_stop(cfg.path(), None, &input, &mut out)
        .await
        .unwrap();
    hooks::run_session_start(cfg.path(), None, &input, &mut out)
        .await
        .unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn session_start_offline_still_gives_context() {
    let (cfg, folder) = bound_env();
    let input = HookInput {
        cwd: Some(folder.path().to_path_buf()),
        stop_hook_active: false,
        ..Default::default()
    };
    let mut out = Vec::new();
    hooks::run_session_start(cfg.path(), None, &input, &mut out)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        ctx.contains("ah wait") && ctx.contains("new message from"),
        "{ctx}"
    );
}

fn cursor_doc(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(dir.join("hooks.json")).unwrap()).unwrap()
}

fn stop_commands(v: &Value) -> Vec<String> {
    v["hooks"]["stop"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| h["command"].as_str().map(String::from))
        .collect()
}

#[test]
fn cursor_fresh_install_then_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let cursor = home.path().join(".cursor");
    std::fs::create_dir(&cursor).unwrap();
    assert_eq!(
        hooks::ensure_cursor_hooks(Some(&cursor)).unwrap(),
        HookStatus::Installed
    );
    let v = cursor_doc(&cursor);
    assert_eq!(
        v,
        json!({"version": 1, "hooks": {"stop": [
            {"command": "ah hook cursor-stop", "loop_limit": hooks::CURSOR_LOOP_LIMIT}
        ]}})
    );

    let before = std::fs::read(cursor.join("hooks.json")).unwrap();
    assert_eq!(
        hooks::ensure_cursor_hooks(Some(&cursor)).unwrap(),
        HookStatus::Unchanged
    );
    assert_eq!(std::fs::read(cursor.join("hooks.json")).unwrap(), before);
    assert_eq!(hooks::cursor_status(Some(&cursor)).unwrap(), "installed");
    assert_eq!(
        hooks::ensure_cursor_hooks(None).unwrap(),
        HookStatus::NotApplicable
    );
}

#[test]
fn cursor_install_preserves_unrelated_entries_and_uninstalls_only_ours() {
    let cursor = tempfile::tempdir().unwrap();
    let original = json!({
        "version": 1,
        "custom": {"keep": true},
        "hooks": {
            "stop": [{"command": "./audit.sh", "loop_limit": 10}],
            "afterFileEdit": [{"command": "./format.sh"}]
        }
    });
    std::fs::write(
        cursor.path().join("hooks.json"),
        serde_json::to_vec_pretty(&original).unwrap(),
    )
    .unwrap();
    assert_eq!(
        hooks::cursor_status(Some(cursor.path())).unwrap(),
        "not installed"
    );
    assert_eq!(
        hooks::ensure_cursor_hooks(Some(cursor.path())).unwrap(),
        HookStatus::Installed
    );
    let v = cursor_doc(cursor.path());
    let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, vec!["version", "custom", "hooks"]);
    assert_eq!(stop_commands(&v), vec!["./audit.sh", "ah hook cursor-stop"]);
    assert_eq!(
        v["hooks"]["afterFileEdit"],
        original["hooks"]["afterFileEdit"]
    );
    assert_eq!(v["hooks"]["stop"][0], original["hooks"]["stop"][0]);

    let removed = hooks::uninstall_cursor(Some(cursor.path())).unwrap();
    assert_eq!(removed, vec!["ah hook cursor-stop"]);
    assert_eq!(cursor_doc(cursor.path()), original);
    assert!(
        hooks::uninstall_cursor(Some(cursor.path()))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cursor_existing_ah_hook_counts_as_installed() {
    let cursor = tempfile::tempdir().unwrap();
    let bytes = br#"{"version":1,"hooks":{"stop":[{"command":"/opt/bin/ah hook cursor-stop"}]}}"#;
    std::fs::write(cursor.path().join("hooks.json"), bytes).unwrap();
    assert_eq!(
        hooks::ensure_cursor_hooks(Some(cursor.path())).unwrap(),
        HookStatus::Unchanged
    );
    assert_eq!(
        std::fs::read(cursor.path().join("hooks.json")).unwrap(),
        bytes
    );
    // Uninstall of an only-ours file drops the emptied stop list and hooks object.
    hooks::uninstall_cursor(Some(cursor.path())).unwrap();
    assert_eq!(cursor_doc(cursor.path()), json!({"version": 1}));
}

#[test]
fn report_names_each_target_or_falls_back() {
    let home = tempfile::tempdir().unwrap();
    let report = |dirs: &hooks::HookDirs| {
        let mut out = Vec::new();
        hooks::ensure_and_report(dirs, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    };
    let none = report(&hooks::HookDirs::default());
    assert_eq!(none.trim_end(), hooks::NO_HOOKS);
    assert!(none.contains("wait_for_message") && none.contains("ah daemon"));

    let cursor_only = hooks::HookDirs {
        claude: None,
        cursor: Some(home.path().join(".cursor")),
    };
    let text = report(&cursor_only);
    assert!(
        text.contains("cursor hook installed (ah hook cursor-stop)"),
        "{text}"
    );
    assert!(
        !text.contains("claude") && !text.contains("no editor hooks"),
        "{text}"
    );
    assert!(
        text.contains("ah channel wake --harness cursor")
            && text.contains("keep ah daemon running")
            && text.contains("do not run ah wait in the background"),
        "{text}"
    );

    let both = hooks::HookDirs {
        claude: Some(home.path().join(".claude")),
        cursor: Some(home.path().join(".cursor")),
    };
    let text = report(&both);
    assert!(text.contains("claude code hooks installed"), "{text}");
    assert!(
        text.contains("cursor hook present (ah hook cursor-stop)"),
        "{text}"
    );
    assert!(
        text.contains("do not run ah wait in the background"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 3, "{text}");
    let text = report(&both);
    assert!(text.contains("claude code hooks present") && text.contains("cursor hook present"));
}

#[tokio::test]
async fn cursor_stop_silent_when_unbound_or_offline() {
    let (cfg, folder) = bound_env();
    let other = tempfile::tempdir().unwrap();
    for roots in [
        vec![],
        vec![other.path().to_path_buf()],
        vec![folder.path().to_path_buf()],
        vec![format!("file://{}", folder.path().display()).into()],
    ] {
        let input = HookInput {
            workspace_roots: roots,
            ..Default::default()
        };
        let mut out = Vec::new();
        hooks::run_cursor_stop(cfg.path(), None, &input, &mut out)
            .await
            .unwrap();
        assert!(out.is_empty());
    }
    // A Cursor stop payload parses: no cwd, workspace_roots given.
    let raw = r#"{"status":"completed","loop_count":0,"conversation_id":"c","workspace_roots":["/w"],"hook_event_name":"stop"}"#;
    let input: HookInput = serde_json::from_str(raw).unwrap();
    assert_eq!(input.workspace_roots, vec![std::path::PathBuf::from("/w")]);
    assert!(input.cwd.is_none());
}
