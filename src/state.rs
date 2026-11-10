use crate::config::Mode;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const META_DIR: &str = ".code-backup";
pub const ORPHAN_DIR: &str = ".orphaned";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Repo,
    Wiki,
}

/// What we know about one backed-up repository, keyed by its path relative
/// to the backup directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub full_name: String,
    pub kind: Kind,
    pub mode: Mode,
    /// Settings that affect the stored data; a change forces a fetch.
    pub fingerprint: String,
    /// GitHub's `pushed_at` at the last successful sync.
    pub pushed_at: Option<String>,
    pub last_success: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// The remote does not exist (a wiki that was never created).
    #[serde(default)]
    pub absent: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunSummary {
    pub finished_at: Option<DateTime<Utc>>,
    pub cloned: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
    #[serde(default)]
    pub last_run: Option<RunSummary>,
}

/// The state file plus an exclusive lock on the backup directory, so two
/// scheduled runs never work on the same directory at once.
pub struct Store {
    dir: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join(META_DIR);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock_path = dir.join("lock");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => bail!(
                "another code-backup process is using {} (lock: {})",
                root.display(),
                lock_path.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", lock_path.display()));
            }
        }
        Ok(Store { dir, _lock: lock })
    }

    fn path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn load(&self) -> Result<State> {
        read_state(&self.dir)
    }

    /// Atomic write: temp file, fsync, rename.
    pub fn save(&self, state: &State) -> Result<()> {
        let path = self.path();
        let tmp = self.dir.join("state.json.tmp");
        let json = serde_json::to_vec_pretty(state)?;
        let mut f = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(&json)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

/// Read the state without taking the lock (dry runs, doctor).
pub fn read_state(meta_dir: &Path) -> Result<State> {
    let path = meta_dir.join("state.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn read_state_for(root: &Path) -> Result<State> {
    read_state(&root.join(META_DIR))
}
