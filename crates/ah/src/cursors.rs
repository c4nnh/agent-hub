//! `{config_dir}/wait-cursors.json`: last delivered message id per channel, per canonical folder.
//! Shared by `ah wait`, the MCP server and `ah channel join`, so a message is delivered once per
//! folder by whichever listener runs first.
//! ponytail: every update is load-modify-save without a lock; two simultaneous listeners on one
//! folder may re-deliver, e.g. `ah wait` and an MCP session running on the same folder at once.
//! Per-folder locking is the upgrade path.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Cursors(BTreeMap<String, BTreeMap<Uuid, i64>>);

impl Cursors {
    fn file(dir: &Path) -> PathBuf {
        dir.join("wait-cursors.json")
    }

    fn load(dir: &Path) -> std::io::Result<Self> {
        match std::fs::read(Self::file(dir)) {
            Ok(b) => serde_json::from_slice(&b).map_err(std::io::Error::other),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    fn save(&self, dir: &Path) -> std::io::Result<()> {
        let b = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(Self::file(dir), b)
    }

    /// The folder's cursors; empty when it has none yet.
    pub fn load_folder(dir: &Path, folder: &str) -> std::io::Result<BTreeMap<Uuid, i64>> {
        Ok(Self::load(dir)?.0.remove(folder).unwrap_or_default())
    }

    /// Records `seeds` for channels the folder has no cursor for. Existing cursors stay: they
    /// mark delivery history.
    pub fn seed(dir: &Path, folder: &str, seeds: &BTreeMap<Uuid, i64>) -> std::io::Result<()> {
        let mut all = Self::load(dir)?;
        let entry = all.0.entry(folder.to_string()).or_default();
        for (id, cursor) in seeds {
            entry.entry(*id).or_insert(*cursor);
        }
        all.save(dir)
    }

    /// Moves the folder's cursors forward to `cursors`; never moves one back, so a slower
    /// listener cannot undo another's progress.
    pub fn advance(dir: &Path, folder: &str, cursors: &BTreeMap<Uuid, i64>) -> std::io::Result<()> {
        let mut all = Self::load(dir)?;
        let entry = all.0.entry(folder.to_string()).or_default();
        for (id, cursor) in cursors {
            let slot = entry.entry(*id).or_insert(0);
            *slot = (*slot).max(*cursor);
        }
        all.save(dir)
    }

    /// Forgets the folder's cursor for one channel; drops the folder entry when it ends empty.
    pub fn remove(dir: &Path, folder: &str, channel: Uuid) -> std::io::Result<()> {
        let mut all = Self::load(dir)?;
        if let Some(entry) = all.0.get_mut(folder) {
            entry.remove(&channel);
            if entry.is_empty() {
                all.0.remove(folder);
            }
        }
        all.save(dir)
    }
}

/// `{config_dir}/wait-pids/<first 16 hex of sha256(folder)>.pid`: marks a running `ah wait`.
fn pid_file(dir: &Path, folder: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(folder.as_bytes());
    let name: String = hash[..8].iter().map(|b| format!("{b:02x}")).collect();
    dir.join("wait-pids").join(format!("{name}.pid"))
}

/// Holds the pidfile of a running `ah wait`; removes it on drop. SIGKILL and Ctrl-C skip the
/// drop, which `listener_alive` covers by treating a dead PID as stale.
pub struct PidGuard(PathBuf);

impl PidGuard {
    pub fn write(dir: &Path, folder: &str) -> std::io::Result<Self> {
        let path = pid_file(dir, folder);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        // A second `ah wait` on the folder may have taken the file over; leave it to that one.
        let ours = std::fs::read_to_string(&self.0)
            .is_ok_and(|s| s.trim() == std::process::id().to_string());
        if ours {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// True when an `ah wait` for `folder` (canonical key) is running. Deletes a stale pidfile.
pub fn listener_alive(dir: &Path, folder: &str) -> bool {
    let path = pid_file(dir, folder);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let alive = text.trim().parse::<u32>().is_ok_and(pid_alive);
    if !alive {
        let _ = std::fs::remove_file(&path);
    }
    alive
}

/// ponytail: a reused PID reads as alive until that process exits; the Stop hook then skips
/// one nudge. Storing the process start time next to the PID is the upgrade path.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists; nothing is delivered.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// ponytail: no liveness probe on Windows; an existing pidfile counts as alive, so a killed
/// `ah wait` silences the Stop hook until the file is removed. OpenProcess is the upgrade path.
#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}
