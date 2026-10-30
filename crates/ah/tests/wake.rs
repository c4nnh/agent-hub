use ah::{
    bindings::{Binding, Bindings},
    commands::channel::{WakeArgs, run_wake},
};
use uuid::Uuid;

fn bound() -> (tempfile::TempDir, tempfile::TempDir) {
    let cfg = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let mut b = Bindings::default();
    b.0.insert(
        Bindings::key(folder.path()).unwrap(),
        Binding {
            channel_ids: vec![Uuid::new_v4()],
            ..Default::default()
        },
    );
    b.save(cfg.path()).unwrap();
    (cfg, folder)
}

fn wake(
    cfg: &tempfile::TempDir,
    folder: &tempfile::TempDir,
    harness: Option<&str>,
    command: Option<&str>,
    off: bool,
) -> Result<String, String> {
    let args = WakeArgs {
        folder: Some(folder.path().to_path_buf()),
        harness: harness.map(Into::into),
        command: command.map(Into::into),
        off,
        notify: false,
    };
    let mut out = Vec::new();
    run_wake(cfg.path(), std::path::Path::new("/"), args, &mut out).map_err(|e| e.to_string())?;
    Ok(String::from_utf8(out).unwrap())
}

fn stored(cfg: &tempfile::TempDir, folder: &tempfile::TempDir) -> Option<String> {
    Bindings::load(cfg.path())
        .unwrap()
        .for_folder(folder.path())
        .unwrap()
        .wake_command
        .clone()
}

#[test]
fn prints_none_then_sets_harness_custom_and_off() {
    let (cfg, folder) = bound();
    assert_eq!(wake(&cfg, &folder, None, None, false).unwrap(), "none\n");

    wake(&cfg, &folder, Some("codex"), None, false).unwrap();
    assert_eq!(
        stored(&cfg, &folder).as_deref(),
        Some("codex exec {prompt}")
    );
    assert_eq!(
        wake(&cfg, &folder, None, None, false).unwrap(),
        "codex exec {prompt}\n"
    );

    wake(&cfg, &folder, None, Some("my-agent --go {prompt}"), false).unwrap();
    assert_eq!(
        stored(&cfg, &folder).as_deref(),
        Some("my-agent --go {prompt}")
    );

    wake(&cfg, &folder, None, None, true).unwrap();
    assert_eq!(stored(&cfg, &folder), None);
    assert_eq!(wake(&cfg, &folder, None, None, false).unwrap(), "none\n");
}

#[test]
fn unbound_folder_names_join() {
    let cfg = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let err = wake(&cfg, &folder, Some("cursor"), None, false).unwrap_err();
    assert!(err.contains("ah channel join"), "{err}");
}

#[test]
fn conflicting_flags_are_rejected() {
    let (cfg, folder) = bound();
    assert!(wake(&cfg, &folder, Some("cursor"), Some("x"), false).is_err());
    assert!(wake(&cfg, &folder, Some("cursor"), None, true).is_err());
    assert_eq!(stored(&cfg, &folder), None);
}

#[test]
fn empty_command_is_rejected() {
    let (cfg, folder) = bound();
    assert!(wake(&cfg, &folder, None, Some("   "), false).is_err());
    assert_eq!(stored(&cfg, &folder), None);
}

#[test]
fn notify_flag_stores_the_reserved_command() {
    let (cfg, folder) = bound();
    let args = |notify, command: Option<&str>| WakeArgs {
        folder: Some(folder.path().to_path_buf()),
        harness: None,
        command: command.map(Into::into),
        off: false,
        notify,
    };
    let run = |a| run_wake(cfg.path(), std::path::Path::new("/"), a, &mut Vec::new());
    run(args(true, None)).unwrap();
    assert_eq!(stored(&cfg, &folder).as_deref(), Some("notify"));
    run(args(false, Some("echo {prompt}"))).unwrap();
    run(args(false, Some("notify"))).unwrap();
    assert_eq!(stored(&cfg, &folder).as_deref(), Some("notify"));
}
