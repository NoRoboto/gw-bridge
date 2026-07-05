//! Persistent map of (project, lane) key -> claude session id, so every lane resumes
//! its own conversation across daemon restarts.
//!
//! The daemon owns ONE store instance behind a lock and every mutation flushes through
//! [`SessionStore::save`], which writes atomically (temp file + rename in the same
//! directory) so a crash mid-write can never corrupt `sessions.json`. The on-disk
//! format is a plain JSON object keyed by the existing lane-key format.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::state_dir;

/// Default location of the session map: `<state dir>/sessions.json`.
pub fn sessions_file() -> PathBuf {
    state_dir().join("sessions.json")
}

pub struct SessionStore {
    path: PathBuf,
    map: HashMap<String, String>,
}

impl SessionStore {
    /// Load the store from `path`. A missing or corrupt file yields an empty map —
    /// the bridge must keep working even if the state file was damaged.
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, map }
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.map.get(key)
    }

    /// Insert (or replace) a session id and flush to disk. Persistence is best-effort:
    /// a write failure is reported on stderr but never takes the daemon down.
    pub fn insert(&mut self, key: String, session_id: String) {
        self.map.insert(key, session_id);
        if let Err(e) = self.save() {
            eprintln!("gw-bridge: could not persist sessions to {}: {e}", self.path.display());
        }
    }

    /// Atomic flush: write a sibling temp file, then rename over the real one, so
    /// readers (and crashes) never observe a partially written file.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, serde_json::to_string_pretty(&self.map).unwrap_or_default())?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        let mut store = SessionStore::load(path.clone());
        assert!(store.get("/proj").is_none(), "fresh store starts empty");
        store.insert("/proj".into(), "sid-main".into());
        store.insert("/proj\u{1f}brain".into(), "sid-brain".into());

        let reloaded = SessionStore::load(path.clone());
        assert_eq!(reloaded.get("/proj"), Some(&"sid-main".to_string()));
        assert_eq!(reloaded.get("/proj\u{1f}brain"), Some(&"sid-brain".to_string()));

        // On-disk format stays a plain JSON map (backward compatible).
        let raw: HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw.len(), 2);
        // No temp file left behind after the atomic rename.
        assert!(!dir.path().join("sessions.json.tmp").exists());
    }

    #[test]
    fn corrupt_file_loads_as_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let store = SessionStore::load(path.clone());
        assert!(store.get("/proj").is_none());

        // And the store recovers: the next insert rewrites a valid file.
        let mut store = store;
        store.insert("/proj".into(), "sid".into());
        let reloaded = SessionStore::load(path);
        assert_eq!(reloaded.get("/proj"), Some(&"sid".to_string()));
    }
}
