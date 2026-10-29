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
