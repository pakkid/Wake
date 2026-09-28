//! Crash-safe JSON persistence for a single state file.
//!
//! Writes go to a temp file that is fsynced and renamed over the real one, so a
//! power cut leaves either the old or the new state, never half of each. A file
//! that won't parse is moved aside (never deleted) and the caller starts fresh.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("couldn't write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("couldn't read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("couldn't encode state: {0}")]
    Encode(#[from] serde_json::Error),
}

#[derive(Debug)]
pub enum Loaded<T> {
    /// No state file yet (first run, or a fresh volume).
    Fresh,
    Loaded(T),
    /// The file was unreadable and has been moved to `backup`.
    Recovered {
        backup: PathBuf,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join("state.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn load<T: DeserializeOwned + Send + 'static>(
        &self,
    ) -> Result<Loaded<T>, StoreError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || load_blocking(&path))
            .await
            .expect("store load task panicked")
    }

    pub async fn save<T: Serialize>(&self, value: &T) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec_pretty(value)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || save_blocking(&path, &bytes))
            .await
            .expect("store save task panicked")
    }
}

fn load_blocking<T: DeserializeOwned>(path: &Path) -> Result<Loaded<T>, StoreError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Loaded::Fresh),
        Err(source) => {
            return Err(StoreError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Ok(Loaded::Loaded(v)),
        Err(e) => {
            let stamp = crate::now_ms();
            let backup = path.with_file_name(format!("state.json.corrupt-{stamp}"));
            std::fs::rename(path, &backup).map_err(|source| StoreError::Write {
                path: backup.clone(),
                source,
            })?;
            Ok(Loaded::Recovered {
                backup,
                reason: e.to_string(),
            })
        }
    }
}

fn save_blocking(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let err = |source| StoreError::Write {
        path: path.to_path_buf(),
        source,
    };
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp).map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(err)?;
    // Make the rename itself durable.
    if let Some(dir) = path.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Doc {
        n: u32,
    }

    #[tokio::test]
    async fn fresh_then_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        assert!(matches!(store.load::<Doc>().await.unwrap(), Loaded::Fresh));
        store.save(&Doc { n: 7 }).await.unwrap();
        match store.load::<Doc>().await.unwrap() {
            Loaded::Loaded(d) => assert_eq!(d, Doc { n: 7 }),
            other => panic!("unexpected {other:?}"),
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            leftovers,
            vec!["state.json".to_string()],
            "no temp file left behind"
        );
    }

    #[tokio::test]
    async fn corrupt_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        std::fs::write(store.path(), b"{ not json").unwrap();
        match store.load::<Doc>().await.unwrap() {
            Loaded::Recovered { backup, .. } => {
                assert!(backup.exists());
                assert_eq!(std::fs::read(&backup).unwrap(), b"{ not json");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!store.path().exists());
        assert!(matches!(store.load::<Doc>().await.unwrap(), Loaded::Fresh));
    }
}
