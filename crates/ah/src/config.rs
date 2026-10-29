use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_HUB_URL: &str = "https://ah.canngo.us";

/// Config directory: `AH_CONFIG_DIR` if set, else `<platform config dir>/ah`.
/// Resolve once in main and pass the result down.
pub fn config_dir() -> PathBuf {
    match std::env::var_os("AH_CONFIG_DIR") {
        Some(d) => PathBuf::from(d),
        None => dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ah"),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        // An existing file keeps its old mode on open; force 0600.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CliConfig {
    pub hub_url: String,
}

impl CliConfig {
    pub fn load(dir: &Path) -> std::io::Result<Self> {
        match std::fs::read(dir.join("config.json")) {
            Ok(b) => serde_json::from_slice(&b).map_err(std::io::Error::other),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                hub_url: DEFAULT_HUB_URL.into(),
            }),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let b = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        write_private(&dir.join("config.json"), &b)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Credentials {
    pub token: String,
    pub email: String,
}

impl Credentials {
    /// `Ok(None)` when not logged in.
    pub fn load(dir: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read(dir.join("credentials.json")) {
            Ok(b) => Ok(Some(
                serde_json::from_slice(&b).map_err(std::io::Error::other)?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let b = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        write_private(&dir.join("credentials.json"), &b)
    }

    pub fn delete(dir: &Path) -> std::io::Result<()> {
        match std::fs::remove_file(dir.join("credentials.json")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}
