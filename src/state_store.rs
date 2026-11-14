use std::collections::HashMap;
use std::fs::{create_dir_all, read, write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use url::Url;

/// Directory holding the persistent state inside a git repository.
///
/// Living inside `.git` keeps the state out of `git status` so it never
/// dirties the working tree and therefore never blocks a `--commit` run.
pub const GIT_STATE_DIR: &str = ".git/website-stalker";

/// Fallback directory for the persistent state when not run inside a git
/// repository.
pub const PLAIN_STATE_DIR: &str = ".website-stalker";

const STATE_FILE: &str = "state.json";

/// Validators of a previous response which allow a conditional request.
///
/// See [conditional requests](https://developer.mozilla.org/en-US/docs/Web/HTTP/Conditional_requests).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Validators {
    /// Value of the last `ETag` response header, sent as `If-None-Match`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,

    /// Value of the last `Last-Modified` response header,
    /// sent as `If-Modified-Since`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

/// Persisted state of a single site.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SiteState {
    /// The final URL after all redirects of the last successful request.
    /// Used as the request URL to avoid repeating the redirects.
    pub url: Url,

    #[serde(flatten)]
    pub validators: Validators,
}

/// Persistent storage of the per-site state across runs.
///
/// The state is keyed by the output file base path (the file path without
/// extension) of the site which is stable across runs as long as the
/// configuration does not change.
#[derive(Debug)]
pub struct StateStore {
    path: PathBuf,
    entries: Mutex<HashMap<PathBuf, SiteState>>,
    dirty: Mutex<bool>,
}

impl StateStore {
    /// Load the state from the given state directory.
    /// A missing state file simply results in an empty store.
    pub fn load(state_dir: &Path) -> anyhow::Result<Self> {
        let path = state_dir.join(STATE_FILE);
        let entries = match read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self {
            path,
            entries: Mutex::new(entries),
            dirty: Mutex::new(false),
        })
    }

    pub fn get(&self, key: &Path) -> Option<SiteState> {
        self.entries
            .lock()
            .expect("state lock poisoned")
            .get(key)
            .cloned()
    }

    pub fn put(&self, key: PathBuf, state: SiteState) {
        let _prev = self
            .entries
            .lock()
            .expect("state lock poisoned")
            .insert(key, state);
        *self.dirty.lock().expect("state lock poisoned") = true;
    }

    pub fn remove(&self, key: &Path) {
        if self
            .entries
            .lock()
            .expect("state lock poisoned")
            .remove(key)
            .is_some()
        {
            *self.dirty.lock().expect("state lock poisoned") = true;
        }
    }

    /// Remove state of sites which are no longer configured.
    pub fn prune(&self, keep: &[PathBuf]) {
        let mut entries = self.entries.lock().expect("state lock poisoned");
        let before = entries.len();
        entries.retain(|key, _| keep.contains(key));
        if entries.len() != before {
            *self.dirty.lock().expect("state lock poisoned") = true;
        }
    }

    /// Write the state to disk when it changed during this run.
    ///
    /// Temporary failures never reach [`Self::put`], so the previous
    /// successful state stays untouched on disk.
    pub fn persist(&self) -> anyhow::Result<()> {
        if !*self.dirty.lock().expect("state lock poisoned") {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            create_dir_all(parent)?;
        }
        let bytes = {
            let entries = self.entries.lock().expect("state lock poisoned");
            serde_json::to_vec_pretty(&*entries)?
        };
        write(&self.path, bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(url: &str) -> SiteState {
        SiteState {
            url: Url::parse(url).unwrap(),
            validators: Validators {
                etag: Some("\"abc\"".to_owned()),
                last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
            },
        }
    }

    #[test]
    fn missing_file_is_empty_store() -> anyhow::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let store = StateStore::load(tempdir.path())?;
        assert!(store.get(Path::new("de/example/index")).is_none());
        // Nothing changed: persisting must not even create the file.
        store.persist()?;
        assert!(!tempdir.path().join(STATE_FILE).exists());
        Ok(())
    }

    #[test]
    fn round_trip() -> anyhow::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let key = PathBuf::from("de/example/index");

        let store = StateStore::load(tempdir.path())?;
        store.put(key.clone(), state("http://localhost/page"));
        store.persist()?;
        assert!(tempdir.path().join(STATE_FILE).exists());

        let reloaded = StateStore::load(tempdir.path())?;
        assert_eq!(reloaded.get(&key), Some(state("http://localhost/page")));
        Ok(())
    }

    #[test]
    fn prune_removes_gone_sites() -> anyhow::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let store = StateStore::load(tempdir.path())?;
        let keep = PathBuf::from("de/example/keep");
        let gone = PathBuf::from("de/example/gone");
        store.put(keep.clone(), state("http://localhost/keep"));
        store.put(gone, state("http://localhost/gone"));
        store.prune(std::slice::from_ref(&keep));
        store.persist()?;

        let reloaded = StateStore::load(tempdir.path())?;
        assert!(reloaded.get(&keep).is_some());
        assert!(reloaded.get(Path::new("de/example/gone")).is_none());
        Ok(())
    }

    #[test]
    fn failed_run_keeps_previous_state() -> anyhow::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let key = PathBuf::from("de/example/index");

        let store = StateStore::load(tempdir.path())?;
        store.put(key.clone(), state("http://localhost/page"));
        store.persist()?;
        let before = read(tempdir.path().join(STATE_FILE))?;

        // Another run which only sees failures: no put, no persist changes.
        let store = StateStore::load(tempdir.path())?;
        store.persist()?;
        let after = read(tempdir.path().join(STATE_FILE))?;
        assert_eq!(before, after);
        assert_eq!(store.get(&key), Some(state("http://localhost/page")));
        Ok(())
    }
}
