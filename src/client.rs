use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{de::DeserializeOwned, Serialize};

use crate::proto::*;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Connect protocol client for Gitea Actions RunnerService
pub struct ConnectClient {
    http: Client,
    base_url: String,
    uuid: Option<String>,
    token: Option<String>,
}

impl ConnectClient {
    pub fn new(api_base: &str) -> Result<Self> {
        let http = Client::builder()
            .use_rustls_tls()
            .build()
            .context("failed to create HTTP client")?;

        Ok(Self {
            http,
            base_url: api_base.to_string(),
            uuid: None,
            token: None,
        })
    }

    pub fn with_credentials(mut self, uuid: String, token: String) -> Self {
        self.uuid = Some(uuid);
        self.token = Some(token);
        self
    }

    async fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        method: &str,
        request: &Req,
    ) -> Result<Resp> {
        let url = format!("{}/runner.v1.RunnerService/{}", self.base_url, method);

        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json");

        if let (Some(uuid), Some(token)) = (&self.uuid, &self.token) {
            req = req
                .header("x-runner-uuid", uuid)
                .header("x-runner-token", token);
        }

        let resp = req
            .json(request)
            .send()
            .await
            .with_context(|| format!("request to {} failed", method))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            bail!("{} returned {}: {}", method, status, body);
        }

        tracing::debug!(
            "{} -> {} : {}",
            method,
            status,
            &body[..body.len().min(500)]
        );

        serde_json::from_str(&body)
            .with_context(|| format!("failed to decode {} response: {}", method, body))
    }

    pub async fn register(&self, name: &str, reg_token: &str, labels: &[String]) -> Result<Runner> {
        let req = RegisterRequest {
            name: name.to_string(),
            token: reg_token.to_string(),
            version: VERSION.to_string(),
            labels: labels
                .iter()
                .map(|l| l.split(":").next().unwrap_or(l).to_string())
                .collect(),
        };
        let resp: RegisterResponse = self.call("Register", &req).await?;
        resp.runner.context("register returned no runner")
    }

    pub async fn declare(&self, labels: &[String]) -> Result<Runner> {
        let req = DeclareRequest {
            version: VERSION.to_string(),
            labels: labels
                .iter()
                .map(|l| l.split(":").next().unwrap_or(l).to_string())
                .collect(),
        };
        let resp: DeclareResponse = self.call("Declare", &req).await?;
        resp.runner.context("declare returned no runner")
    }

    pub async fn fetch_task(&self, tasks_version: i64) -> Result<FetchTaskResponse> {
        let req = FetchTaskRequest { tasks_version };
        self.call("FetchTask", &req).await
    }

    pub async fn update_task(
        &self,
        state: TaskState,
        outputs: std::collections::HashMap<String, String>,
    ) -> Result<UpdateTaskResponse> {
        let req = UpdateTaskRequest { state, outputs };
        self.call("UpdateTask", &req).await
    }

    pub async fn update_log(
        &self,
        task_id: i64,
        index: i64,
        rows: Vec<LogRow>,
        no_more: bool,
    ) -> Result<UpdateLogResponse> {
        let req = UpdateLogRequest {
            task_id,
            index,
            rows,
            no_more,
        };
        self.call("UpdateLog", &req).await
    }
}
