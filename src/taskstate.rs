/// Crash-safe in-flight task tracking.
///
/// On every task pickup fucina writes the task ID to a JSON file.  On task
/// completion (success or failure) it removes the ID.  If the process crashes
/// or is killed mid-job the IDs survive in the file.  On the next startup,
/// `drain_stale` returns those orphaned IDs so the daemon can report them as
/// FAILURE to Gitea — preventing the "stale task" lockup where Gitea keeps a
/// task in `running` state for a runner that no longer exists.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::{debug, warn};

#[derive(Serialize, Deserialize, Default)]
struct State {
    task_ids: Vec<i64>,
}

pub struct TaskStateFile {
    path: PathBuf,
    lock: Mutex<()>,
}

impl TaskStateFile {
    /// Create a handle pointing at `path` (file need not exist yet).
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// Derive the default path from the runner credentials file path.
    /// E.g. `~/gitea-runner-rs/.runner` → `~/gitea-runner-rs/active-tasks.json`
    pub fn alongside(runner_file: &Path) -> Self {
        let path = runner_file
            .parent()
            .unwrap_or(Path::new("."))
            .join("active-tasks.json");
        Self::new(path)
    }

    fn read_locked(&self) -> State {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_locked(&self, state: &State) -> Result<()> {
        let json = serde_json::to_string(state)?;
        // Atomic: write to .tmp then rename so a crash mid-write can't corrupt.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Record that task `id` is now in-flight.
    pub fn add(&self, id: i64) {
        let _g = self.lock.lock().unwrap();
        let mut state = self.read_locked();
        if !state.task_ids.contains(&id) {
            state.task_ids.push(id);
            if let Err(e) = self.write_locked(&state) {
                warn!("taskstate: failed to persist task {}: {:#}", id, e);
            } else {
                debug!("taskstate: recorded task {}", id);
            }
        }
    }

    /// Remove task `id` from the in-flight set (called on completion).
    pub fn remove(&self, id: i64) {
        let _g = self.lock.lock().unwrap();
        let mut state = self.read_locked();
        let before = state.task_ids.len();
        state.task_ids.retain(|&x| x != id);
        if state.task_ids.len() < before {
            if let Err(e) = self.write_locked(&state) {
                warn!("taskstate: failed to remove task {}: {:#}", id, e);
            } else {
                debug!("taskstate: cleared task {}", id);
            }
        }
    }

    /// Return all surviving task IDs and clear the file.
    /// Called once on startup — any IDs still present belong to a previous
    /// process that died without cleaning up.
    pub fn drain_stale(&self) -> Vec<i64> {
        let _g = self.lock.lock().unwrap();
        let state = self.read_locked();
        if state.task_ids.is_empty() {
            return vec![];
        }
        // Clear first — if we crash again after reporting, we don't double-report.
        if let Err(e) = self.write_locked(&State::default()) {
            warn!("taskstate: failed to clear stale state: {:#}", e);
        }
        state.task_ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fucina-taskstate-test-{}.json", name))
    }

    fn tmp_state(name: &str) -> TaskStateFile {
        let p = tmp_path(name);
        let _ = std::fs::remove_file(&p); // clean slate
        TaskStateFile::new(p)
    }

    #[test]
    fn roundtrip_add_remove() {
        let ts = tmp_state("roundtrip");
        ts.add(1);
        ts.add(2);
        ts.add(3);
        ts.remove(2);
        let stale = ts.drain_stale();
        assert!(stale.contains(&1));
        assert!(stale.contains(&3));
        assert!(!stale.contains(&2));
    }

    #[test]
    fn drain_clears_file() {
        let ts = tmp_state("drain");
        ts.add(42);
        let first = ts.drain_stale();
        assert_eq!(first, vec![42]);
        let second = ts.drain_stale();
        assert!(second.is_empty());
    }

    #[test]
    fn missing_file_is_empty() {
        let ts = TaskStateFile::new(PathBuf::from("/tmp/fucina-nonexistent-99999.json"));
        assert!(ts.drain_stale().is_empty());
    }

    #[test]
    fn concurrent_add_remove() {
        let ts = Arc::new(tmp_state("concurrent"));
        let handles: Vec<_> = (0..10i64)
            .map(|i| {
                let ts = ts.clone();
                std::thread::spawn(move || {
                    ts.add(i);
                    if i % 2 == 0 {
                        ts.remove(i);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let stale = ts.drain_stale();
        // Only odd IDs should remain
        for i in 0..10i64 {
            if i % 2 == 1 {
                assert!(stale.contains(&i), "missing odd id {}", i);
            } else {
                assert!(!stale.contains(&i), "even id {} should be gone", i);
            }
        }
    }
}
