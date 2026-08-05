use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const MANIFEST_FILE: &str = "manifest.json";
pub const DEFAULT_CHECKPOINT_CAP: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub id: String,
    pub label: Option<String>,
    pub created_at_ms: u64,
    pub change_count: usize,
    pub committed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CheckpointManifest {
    #[serde(default)]
    pub checkpoints: Vec<CheckpointMeta>,
    #[serde(default)]
    pub head: Option<String>,
}

/// On-disk checkpoint chain, persisted atomically (tmp file + rename).
pub struct CheckpointStore {
    path: PathBuf,
    manifest: CheckpointManifest,
    cap: usize,
}

impl CheckpointStore {
    pub fn open(dir: &Path, cap: usize) -> Result<Self> {
        let path = dir.join(MANIFEST_FILE);
        let manifest = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            serde_json::from_str(&raw)?
        } else {
            CheckpointManifest::default()
        };
        Ok(Self {
            path,
            manifest,
            cap,
        })
    }

    pub fn create(&mut self, label: Option<&str>, change_count: usize) -> Result<CheckpointMeta> {
        let meta = CheckpointMeta {
            id: Uuid::new_v4().to_string(),
            label: label.map(str::to_string),
            created_at_ms: crate::session::now_ms(),
            change_count,
            committed: false,
        };
        self.manifest.checkpoints.push(meta.clone());
        self.manifest.head = Some(meta.id.clone());
        self.prune();
        self.persist()?;
        Ok(meta)
    }

    pub fn commit(&mut self, id: &str) -> Result<()> {
        let meta = self
            .manifest
            .checkpoints
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or_else(|| Error::CheckpointNotFound(id.to_string()))?;
        meta.committed = true;
        self.persist()
    }

    pub fn get(&self, id: &str) -> Result<&CheckpointMeta> {
        self.manifest
            .checkpoints
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| Error::CheckpointNotFound(id.to_string()))
    }

    pub fn list(&self) -> Vec<&CheckpointMeta> {
        self.manifest.checkpoints.iter().collect()
    }

    pub fn head(&self) -> Option<&str> {
        self.manifest.head.as_deref()
    }

    fn prune(&mut self) {
        while self.manifest.checkpoints.len() > self.cap {
            let idx = self
                .manifest
                .checkpoints
                .iter()
                .position(|c| !c.committed)
                .unwrap_or(0);
            let removed = self.manifest.checkpoints.remove(idx);
            if self.manifest.head.as_deref() == Some(removed.id.as_str()) {
                self.manifest.head = self.manifest.checkpoints.last().map(|c| c.id.clone());
            }
        }
    }

    fn persist(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&self.manifest)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_chain_persists_and_commits() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CheckpointStore::open(dir.path(), DEFAULT_CHECKPOINT_CAP).unwrap();
        let a = store.create(Some("before npm install"), 0).unwrap();
        let b = store.create(None, 3).unwrap();
        assert_eq!(store.list().len(), 2);
        assert_eq!(store.head(), Some(b.id.as_str()));
        store.commit(&a.id).unwrap();
        drop(store);

        let reloaded = CheckpointStore::open(dir.path(), DEFAULT_CHECKPOINT_CAP).unwrap();
        assert_eq!(reloaded.list().len(), 2);
        assert!(reloaded.get(&a.id).unwrap().committed);
        assert!(!reloaded.get(&b.id).unwrap().committed);
    }

    #[test]
    fn cap_prunes_oldest_uncommitted_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = CheckpointStore::open(dir.path(), 2).unwrap();
        let a = store.create(None, 0).unwrap();
        let b = store.create(None, 0).unwrap();
        let c = store.create(None, 0).unwrap();
        let ids: Vec<String> = store.list().iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids.len(), 2);
        assert!(!ids.contains(&a.id));
        assert_eq!(store.head(), Some(c.id.as_str()));
        assert!(b.id != c.id);
    }
}
