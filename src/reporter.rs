use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::client::ConnectClient;
use crate::proto::{LogRow, StepState, TaskResult, TaskState, Timestamp};

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

    /// Send final log flush with no_more=true
    pub async fn close_logs(&self) -> Result<()> {
        self.flush_logs().await?;
        let index = *self.log_index.lock().await;
        self.client
            .update_log(self.task_id, index, vec![], true)
            .await?;
        Ok(())
    }

    /// Report task state
    pub async fn update_state(&self, state: TaskState) -> Result<()> {
        self.client.update_task(state, HashMap::new()).await?;
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

    /// Report task completed
    pub async fn report_completed(&self, result: TaskResult, steps: Vec<StepState>) -> Result<()> {
        self.close_logs().await?;
        self.update_state(TaskState {
            id: self.task_id,
            result,
            started_at: None,
            stopped_at: Some(Timestamp::now()),
            steps,
        })
        .await
    }
}
