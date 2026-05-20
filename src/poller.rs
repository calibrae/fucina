use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, info, warn};

use crate::client::ConnectClient;
use crate::proto::Task;
use crate::reporter::Reporter;
use crate::runner;

/// Upper bound on tasks held in the pending queue. A task that arrives when
/// all worker slots are busy is queued rather than dropped; only a queue this
/// deep (a genuinely overloaded runner) falls back to dropping.
const MAX_PENDING: usize = 128;

pub struct Poller {
    client: Arc<ConnectClient>,
    capacity: Arc<Semaphore>,
    fetch_interval: std::time::Duration,
    work_dir: PathBuf,
    run_as: Option<String>,
    tasks_version: i64,
    /// Tasks received while at capacity, awaiting a free worker slot.
    pending: VecDeque<Task>,
    /// IDs of tasks currently executing — for deduping Gitea re-offers.
    active: Arc<Mutex<HashSet<i64>>>,
}

/// True if `id` is already running or queued — used to dedupe Gitea
/// re-offering a task the runner is already holding.
fn task_known(active: &HashSet<i64>, pending: &VecDeque<Task>, id: i64) -> bool {
    active.contains(&id) || pending.iter().any(|t| t.id == id)
}

impl Poller {
    pub fn new(
        client: Arc<ConnectClient>,
        capacity: usize,
        fetch_interval_secs: u64,
        work_dir: PathBuf,
        run_as: Option<String>,
    ) -> Self {
        Self {
            client,
            capacity: Arc::new(Semaphore::new(capacity)),
            fetch_interval: std::time::Duration::from_secs(fetch_interval_secs),
            work_dir,
            run_as,
            tasks_version: 0,
            pending: VecDeque::new(),
            active: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub async fn run(&mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        info!(
            "poller started (interval={}s)",
            self.fetch_interval.as_secs()
        );

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("poller received shutdown signal");
                        break;
                    }
                }
                _ = tokio::time::sleep(self.fetch_interval) => {
                    if let Err(e) = self.poll_once(shutdown.clone()).await {
                        warn!("poll error: {:#}", e);
                    }
                }
            }
        }

        // Wait for in-flight jobs to finish (all permits returned)
        info!("waiting for in-flight jobs to complete...");
        let max = self.capacity.available_permits();
        let _ = self.capacity.acquire_many(max as u32).await;
        info!("all jobs completed");

        Ok(())
    }

    async fn poll_once(&mut self, shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        // First, give any queued tasks a chance to run — a worker may have
        // freed up since the last tick.
        self.drain_pending(&shutdown);

        debug!("fetching tasks (version={})", self.tasks_version);
        let resp = self.client.fetch_task(self.tasks_version).await?;

        let version_changed = resp.tasks_version != self.tasks_version;
        if version_changed {
            info!(
                "tasks version changed: {} -> {} (task present: {})",
                self.tasks_version,
                resp.tasks_version,
                resp.task.is_some()
            );
        }
        self.tasks_version = resp.tasks_version;

        let task = match resp.task {
            Some(t) => t,
            None => {
                if version_changed {
                    warn!("tasks version bumped but no task assigned — possible label mismatch or task already claimed");
                }
                debug!("no task available");
                return Ok(());
            }
        };

        info!("received task {}", task.id);

        // Dedupe: Gitea may re-offer a task we already hold queued or running.
        if task_known(&self.active.lock().unwrap(), &self.pending, task.id) {
            debug!("task {} already queued or running — ignoring duplicate", task.id);
            self.tasks_version = 0;
            return Ok(());
        }

        // Dispatch immediately if a worker slot is free, otherwise queue it.
        match self.capacity.clone().try_acquire_owned() {
            Ok(permit) => self.spawn_task(task, permit, shutdown),
            Err(_) => {
                if self.pending.len() >= MAX_PENDING {
                    warn!(
                        "pending queue full ({}), dropping task {}",
                        MAX_PENDING, task.id
                    );
                } else {
                    info!(
                        "at capacity — queued task {} ({} now pending)",
                        task.id,
                        self.pending.len() + 1
                    );
                    self.pending.push_back(task);
                }
            }
        }

        // Reset tasks_version so the next FetchTask sends 0 (< server's current
        // version), causing Gitea to scan for pending tasks. Without this, after
        // a task completes self.tasks_version == server's current version and
        // Gitea returns no task indefinitely — even with a full queue of jobs.
        self.tasks_version = 0;

        Ok(())
    }

    /// Move queued tasks into free worker slots, oldest first.
    fn drain_pending(&mut self, shutdown: &tokio::sync::watch::Receiver<bool>) {
        while !self.pending.is_empty() {
            match self.capacity.clone().try_acquire_owned() {
                Ok(permit) => {
                    let task = self.pending.pop_front().expect("pending non-empty");
                    info!(
                        "dispatching queued task {} ({} still pending)",
                        task.id,
                        self.pending.len()
                    );
                    self.spawn_task(task, permit, shutdown.clone());
                }
                Err(_) => break, // still at capacity
            }
        }
    }

    /// Spawn a worker for `task`, holding `permit` for its lifetime.
    fn spawn_task(
        &self,
        task: Task,
        permit: OwnedSemaphorePermit,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let client = self.client.clone();
        let work_dir = self.work_dir.clone();
        let run_as = self.run_as.clone();
        let active = self.active.clone();
        let task_id = task.id;

        active.lock().unwrap().insert(task_id);

        tokio::spawn(async move {
            let reporter = Arc::new(Reporter::new(client, task_id));
            match runner::execute(&task, reporter.clone(), &work_dir, run_as.as_deref(), shutdown)
                .await
            {
                Ok(result) => {
                    info!("task {} completed: {:?}", task_id, result);
                }
                Err(e) => {
                    error!("task {} failed: {:#}", task_id, e);
                    // Best-effort: tell Gitea the task failed so the job doesn't
                    // stay as a zombie in_progress forever.
                    let _ = reporter
                        .report_completed(crate::proto::TaskResult::Failure, vec![], HashMap::new())
                        .await;
                }
            }
            active.lock().unwrap().remove(&task_id);
            drop(permit);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: i64) -> Task {
        Task {
            id,
            ..Default::default()
        }
    }

    #[test]
    fn task_known_detects_running() {
        let mut active = HashSet::new();
        active.insert(7);
        let pending = VecDeque::new();
        assert!(task_known(&active, &pending, 7));
        assert!(!task_known(&active, &pending, 8));
    }

    #[test]
    fn task_known_detects_queued() {
        let active = HashSet::new();
        let mut pending = VecDeque::new();
        pending.push_back(task(42));
        pending.push_back(task(43));
        assert!(task_known(&active, &pending, 42));
        assert!(task_known(&active, &pending, 43));
        assert!(!task_known(&active, &pending, 99));
    }

    #[test]
    fn task_known_empty() {
        assert!(!task_known(&HashSet::new(), &VecDeque::new(), 1));
    }

    #[test]
    fn pending_queue_is_fifo() {
        // queued tasks drain oldest-first
        let mut pending: VecDeque<Task> = VecDeque::new();
        pending.push_back(task(1));
        pending.push_back(task(2));
        pending.push_back(task(3));
        assert_eq!(pending.pop_front().unwrap().id, 1);
        assert_eq!(pending.pop_front().unwrap().id, 2);
        assert_eq!(pending.pop_front().unwrap().id, 3);
    }
}
