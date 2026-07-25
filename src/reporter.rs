use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::client::ConnectClient;
use crate::proto::{LogRow, StepState, TaskResult, TaskState, Timestamp};

/// Attempts for a result-reporting RPC before giving up.
const REPORT_ATTEMPTS: usize = 5;

/// Retry a reporting RPC with exponential backoff.
///
/// Reporting the *verdict* is the one thing a runner must not lose to a
/// transient blip: Gitea restarting, a dropped keep-alive, a sleep/wake on the
/// Mac Minis. A single failed attempt used to abort the job and get it recorded
/// as a failure — see `report_completed`.
async fn with_retry<F, Fut, T>(what: &str, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = Duration::from_millis(500);
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=REPORT_ATTEMPTS {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                warn!(
                    "{} attempt {}/{} failed: {:#}",
                    what, attempt, REPORT_ATTEMPTS, e
                );
                last = Some(e);
                if attempt < REPORT_ATTEMPTS {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(8));
                }
            }
        }
    }
    Err(last.expect("at least one attempt ran"))
}

/// Gitea answers `UpdateLog` with this once it has archived a task's log — it
/// means the log is already finalized, which is exactly the state `close_logs`
/// wants. Benign, so don't warn about it.
fn is_already_archived(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("log file has been archived") || s.contains("AlreadyExists")
}

/// Buffers log lines and reports task state back to Gitea
pub struct Reporter {
    client: Arc<ConnectClient>,
    task_id: i64,
    log_index: Arc<Mutex<i64>>,
    log_buffer: Arc<Mutex<Vec<LogRow>>>,
}

impl Reporter {
    pub fn new(client: Arc<ConnectClient>, task_id: i64) -> Self {
        Self {
            client,
            task_id,
            log_index: Arc::new(Mutex::new(0)),
            log_buffer: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn log(&self, content: &str) {
        let row = LogRow {
            time: Timestamp::now(),
            content: content.to_string(),
        };
        self.log_buffer.lock().await.push(row);
    }

    pub async fn logf(&self, content: String) {
        let row = LogRow {
            time: Timestamp::now(),
            content,
        };
        self.log_buffer.lock().await.push(row);
    }

    /// Flush buffered logs to Gitea. Returns the new log index.
    pub async fn flush_logs(&self) -> Result<i64> {
        let rows: Vec<LogRow> = {
            let mut buf = self.log_buffer.lock().await;
            std::mem::take(&mut *buf)
        };
        if rows.is_empty() {
            return Ok(*self.log_index.lock().await);
        }

        let index = *self.log_index.lock().await;
        let count = rows.len() as i64;

        let resp = self
            .client
            .update_log(self.task_id, index, rows, false)
            .await?;

        let mut idx = self.log_index.lock().await;
        *idx = resp.ack_index.max(index + count);
        Ok(*idx)
    }

    /// Send the final log flush with `no_more=true`, finalizing the task's log.
    ///
    /// Gitea's `UpdateLog` handler short-circuits on `len(rows) == 0` *before*
    /// it honors `no_more` (routers/api/actions/runner/runner.go), so a bare
    /// empty `no_more=true` request never reaches the `TransferLogs` call that
    /// sets `log_in_storage` — the log is stranded in Gitea's `dbfs` forever
    /// (it stays viewable, but `dbfs_data` leaks and never moves to the storage
    /// backend). The fix, mirroring the official act_runner: make the final
    /// request carry at least one row, injecting an empty sentinel line when
    /// nothing is buffered. (Upstream fix: gitea PR #37631, ~v1.28 — drop the
    /// sentinel once every Gitea we target includes it.)
    pub async fn close_logs(&self) -> Result<()> {
        let mut rows: Vec<LogRow> = {
            let mut buf = self.log_buffer.lock().await;
            std::mem::take(&mut *buf)
        };
        if rows.is_empty() {
            rows.push(LogRow {
                time: Timestamp::now(),
                content: String::new(),
            });
        }
        let index = *self.log_index.lock().await;
        let count = rows.len() as i64;
        let resp = self
            .client
            .update_log(self.task_id, index, rows, true)
            .await?;
        let mut idx = self.log_index.lock().await;
        *idx = resp.ack_index.max(index + count);
        Ok(())
    }

    /// Report task state
    pub async fn update_state(&self, state: TaskState) -> Result<()> {
        with_retry("UpdateTask", || {
            self.client.update_task(state.clone(), HashMap::new())
        })
        .await?;
        Ok(())
    }

    /// Report task started
    pub async fn report_started(&self) -> Result<()> {
        self.update_state(TaskState {
            id: self.task_id,
            result: TaskResult::Unspecified,
            started_at: Some(Timestamp::now()),
            stopped_at: None,
            steps: vec![],
        })
        .await
    }

    /// Report task completed. `outputs` carries the job's resolved
    /// `outputs:` block back to Gitea so downstream jobs see them in
    /// their `needs.<job>.outputs` context.
    ///
    /// Finalizing the log is best-effort and deliberately cannot fail the
    /// call. It used to run as `self.close_logs().await?`, which meant a
    /// transient `UpdateLog` error (observed live: Gitea 500 `log file has
    /// been archived`) propagated out of `execute()` and made the poller's
    /// catch-all record the task as FAILURE — a job whose every step exited 0,
    /// with `Exit code: 0` visible at the end of its own Gitea log, reported
    /// as failed. A log-finalization hiccup must never change the verdict.
    pub async fn report_completed(
        &self,
        result: TaskResult,
        steps: Vec<StepState>,
        outputs: HashMap<String, String>,
    ) -> Result<()> {
        if let Err(e) = self.close_logs().await {
            if is_already_archived(&e) {
                debug!("task {}: log already archived by Gitea", self.task_id);
            } else {
                warn!(
                    "task {}: failed to finalize logs: {:#} — reporting {:?} anyway",
                    self.task_id, e, result
                );
            }
        }
        let state = TaskState {
            id: self.task_id,
            result,
            started_at: None,
            stopped_at: Some(Timestamp::now()),
            steps,
        };
        with_retry("UpdateTask", || {
            self.client.update_task(state.clone(), outputs.clone())
        })
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn retry_succeeds_after_transient_failures() {
        let calls = AtomicUsize::new(0);
        let out: Result<&str> = with_retry("Test", || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n < 3 {
                    Err(anyhow::anyhow!("transient blip"))
                } else {
                    Ok("reported")
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), "reported");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_gives_up_after_all_attempts() {
        let calls = AtomicUsize::new(0);
        let out: Result<()> = with_retry("Test", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(anyhow::anyhow!("still down")) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), REPORT_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_returns_immediately_on_success() {
        let calls = AtomicUsize::new(0);
        let out: Result<u8> = with_retry("Test", || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(7) }
        })
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn already_archived_is_recognised() {
        // the exact shape observed from Gitea on task 754
        let e = anyhow::anyhow!(
            "UpdateLog returned 500 Internal Server Error: {{\"code\":\"unknown\",\
             \"message\":\"rpc error: code = AlreadyExists desc = log file has been archived\"}}"
        );
        assert!(is_already_archived(&e));
        assert!(!is_already_archived(&anyhow::anyhow!("connection reset")));
    }
}
