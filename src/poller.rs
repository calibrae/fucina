use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use crate::client::ConnectClient;
use crate::reporter::Reporter;
use crate::runner;

pub struct Poller {
    client: Arc<ConnectClient>,
    capacity: Arc<Semaphore>,
    fetch_interval: std::time::Duration,
    work_dir: PathBuf,
    tasks_version: i64,
}

impl Poller {
    pub fn new(
        client: Arc<ConnectClient>,
        capacity: usize,
        fetch_interval_secs: u64,
        work_dir: PathBuf,
    ) -> Self {
        Self {
            client,
            capacity: Arc::new(Semaphore::new(capacity)),
            fetch_interval: std::time::Duration::from_secs(fetch_interval_secs),
            work_dir,
            tasks_version: 0,
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
                    if let Err(e) = self.poll_once().await {
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

    async fn poll_once(&mut self) -> Result<()> {
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

        // Acquire capacity permit
        let permit = match self.capacity.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!("at capacity, dropping task {}", task.id);
                return Ok(());
            }
        };

        let client = self.client.clone();
        let work_dir = self.work_dir.clone();

        tokio::spawn(async move {
            let reporter = Arc::new(Reporter::new(client, task.id));
            match runner::execute(&task, reporter, &work_dir).await {
                Ok(result) => {
                    info!("task {} completed: {:?}", task.id, result);
                }
                Err(e) => {
                    error!("task {} failed: {:#}", task.id, e);
                }
            }
            drop(permit);
        });

        Ok(())
    }
}
