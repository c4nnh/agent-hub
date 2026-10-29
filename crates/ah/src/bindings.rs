use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Binding {
    pub channel_ids: Vec<Uuid>,
    pub extra_folders: Vec<String>,
    /// Command template `ah daemon` runs to wake an idle agent; `{prompt}` is the wake prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_command: Option<String>,
}

/// Folder bindings stored at `{config_dir}/bindings.json`, keyed by canonical folder path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct Bindings(pub BTreeMap<String, Binding>);

impl Bindings {
    fn file(dir: &Path) -> PathBuf {
        dir.join("bindings.json")
    }

    pub fn load(dir: &Path) -> std::io::Result<Self> {
        match std::fs::read(Self::file(dir)) {
            Ok(b) => serde_json::from_slice(&b).map_err(std::io::Error::other),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let b = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(Self::file(dir), b)
    }

    /// Exact match on the canonicalized path; subdirectories of a bound folder are not bound.
    pub fn for_folder(&self, path: &Path) -> Option<&Binding> {
        let canon = std::fs::canonicalize(path).ok()?;
        self.0.get(&canon.to_string_lossy().into_owned())
    }

    pub fn key(path: &Path) -> std::io::Result<String> {
        Ok(std::fs::canonicalize(path)?.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_json_loads_without_wake_command() {
        let id = Uuid::nil();
        let json = format!(r#"{{"/a":{{"channel_ids":["{id}"],"extra_folders":[]}}}}"#);
        let b: Bindings = serde_json::from_str(&json).unwrap();
        assert_eq!(b.0["/a"].wake_command, None);
    }

    #[test]
    fn no_wake_command_serializes_without_the_key() {
        let b = Binding::default();
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(json, r#"{"channel_ids":[],"extra_folders":[]}"#);
        let b = Binding {
            wake_command: Some("x {prompt}".into()),
            ..Default::default()
        };
        assert!(
            serde_json::to_string(&b)
                .unwrap()
                .contains(r#""wake_command":"x {prompt}""#)
        );
    }
}
