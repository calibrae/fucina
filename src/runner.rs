use anyhow::{Context as _, Result};
use base64::Engine;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::expr::{Context as ExprCtx, JobStatus};
use crate::proto::{self, StepState, Task, Timestamp};
use crate::reporter::Reporter;

/// Represents a parsed workflow step
#[derive(Debug)]
#[allow(dead_code)]
struct Step {
    id: String,
    name: String,
    run: Option<String>,
    uses: Option<String>,
    env: HashMap<String, String>,
    with: HashMap<String, String>,
    working_directory: Option<String>,
    shell: Option<String>,
    r#if: Option<String>,
}

/// Execute a task (one job from a workflow)
pub async fn execute(
    task: &Task,
    reporter: Arc<Reporter>,
    work_dir: &Path,
    run_as: Option<&str>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<proto::TaskResult> {
    // Decode workflow payload (base64-encoded YAML)
    let yaml_bytes = base64::engine::general_purpose::STANDARD
        .decode(&task.workflow_payload)
        .or_else(|_| {
            // Try as raw string (some versions may not base64-encode)
            Ok::<Vec<u8>, anyhow::Error>(task.workflow_payload.as_bytes().to_vec())
        })
        .context("failed to decode workflow payload")?;

    let yaml_str = String::from_utf8(yaml_bytes).context("workflow payload is not valid UTF-8")?;

    info!("workflow payload ({} bytes)", yaml_str.len());

    // Parse workflow YAML
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(&yaml_str).context("failed to parse workflow YAML")?;

    // Find the target job
    let job_id = extract_job_id(&task.context);
    let job = find_job(&workflow, job_id.as_deref()).context("could not find job in workflow")?;

    // Extract steps
    let steps = parse_steps(job)?;
    if steps.is_empty() {
        warn!("job has no steps");
        return Ok(proto::TaskResult::Success);
    }

    // Working directory layout. workspace/ holds the checked-out repo, _temp/
    // is RUNNER_TEMP and also holds per-step GITHUB_OUTPUT/GITHUB_ENV.
    let job_dir = work_dir.join(format!("task-{}", task.id));
    let workspace = job_dir.join("workspace");
    let runner_temp = job_dir.join("_temp");

    // --- Job-level `if:` -------------------------------------------------
    // Gitea resolves the needs DAG server-side, but a job that carries an
    // `if:` is NOT skipped by Gitea when a dependency fails — it is marked
    // runnable and handed to us to evaluate (services/actions/job_emitter.go:
    // a blocked job becomes StatusWaiting rather than StatusSkipped iff it has
    // an `if:`). So honouring the job `if:` is the runner's responsibility;
    // without this we run every job we are handed. Unlike a step `if:`, a job
    // `if:` gets NO implicit `success()` prefix — it is evaluated verbatim,
    // with the status functions reflecting the *needs* results. That is
    // exactly why a failed dependency does not stop a job whose `if:` is
    // otherwise true (the cali/niveau deploy-past-failed-check case).
    {
        let mut job_ctx = build_expr_context(task, &workspace, &runner_temp);
        job_ctx.status = job_status_from_needs(&task.needs);
        if !should_run_job(job.get("if"), &job_ctx) {
            let cond = job.get("if").and_then(|v| v.as_str()).unwrap_or("");
            info!("job skipped — `if` condition not satisfied: {}", cond);
            reporter
                .report_completed(proto::TaskResult::Skipped, vec![], HashMap::new())
                .await?;
            return Ok(proto::TaskResult::Skipped);
        }
    }

    tokio::fs::create_dir_all(&workspace)
        .await
        .context("failed to create workspace directory")?;
    tokio::fs::create_dir_all(&runner_temp)
        .await
        .context("failed to create runner temp directory")?;

    // If steps will run as a different user, hand ownership of the job dir
    // so that user can create target/, caches, GITHUB_OUTPUT files, etc.
    if let Some(user) = run_as {
        let _ = Command::new("chown")
            .arg("-R")
            .arg(format!("{}:staff", user))
            .arg(&job_dir)
            .status()
            .await;
    }

    // --- Environment & expression context -------------------------------
    // Base env: CI vars + GITHUB_*/RUNNER_* derived from the task context.
    let base_env = build_env(task, &workspace, &runner_temp);

    // Job-level / workflow-level env blocks (Gap 3). Precedence is
    // workflow < job < step; we fold workflow then job into `merged_env`
    // here, and apply step-level env per step below.
    let workflow_env = parse_string_map(workflow.get("env"));
    let job_env = parse_string_map(job.get("env"));

    // Expression evaluation context (Gap 1).
    let mut ctx = build_expr_context(task, &workspace, &runner_temp);

    let mut merged_env = base_env.clone();
    ctx.set("env", env_to_json(&merged_env));
    for (k, v) in &workflow_env {
        let rendered = ctx.render(v);
        merged_env.insert(k.clone(), rendered);
        ctx.set("env", env_to_json(&merged_env));
    }
    for (k, v) in &job_env {
        let rendered = ctx.render(v);
        merged_env.insert(k.clone(), rendered);
        ctx.set("env", env_to_json(&merged_env));
    }

    // Keys explicitly defined by the workflow (vs. inherited daemon env).
    // Used to decide which vars to strip when running as another user.
    let mut user_keys: HashSet<String> =
        workflow_env.keys().chain(job_env.keys()).cloned().collect();

    // Env vars exported via `$GITHUB_ENV` accumulate here and apply to all
    // subsequent steps (Gap 4).
    let mut env_overlay: HashMap<String, String> = HashMap::new();
    // Step outputs, keyed by step id, feed the `steps.*` context.
    let mut steps_json = serde_json::Map::new();

    reporter.report_started().await?;

    let mut step_states = Vec::new();
    let mut overall_result = proto::TaskResult::Success;
    let mut log_index: i64 = 0;

    for (i, step) in steps.iter().enumerate() {
        let step_name = if step.name.is_empty() {
            format!("Step {}", i + 1)
        } else {
            step.name.clone()
        };

        reporter.logf(format!("::group::{}", step_name)).await;
        let step_start = Timestamp::now();

        // Refresh the evaluator with current job status + step outputs.
        ctx.status = if overall_result == proto::TaskResult::Failure {
            JobStatus::Failure
        } else {
            JobStatus::Success
        };
        ctx.set("steps", serde_json::Value::Object(steps_json.clone()));
        ctx.set("env", env_to_json(&merged_env));

        let mut step_outputs: HashMap<String, String> = HashMap::new();
        let result;

        if !should_run_step(step, &ctx) {
            reporter
                .logf(format!(
                    "Step skipped — `if` condition not satisfied: {}",
                    step.r#if.as_deref().unwrap_or("")
                ))
                .await;
            result = proto::TaskResult::Skipped;
        } else {
            // --- Build the step environment (Gap 2 + 3 + 4) ---
            let mut step_env = merged_env.clone();
            for (k, v) in &env_overlay {
                step_env.insert(k.clone(), v.clone());
                user_keys.insert(k.clone());
            }
            ctx.set("env", env_to_json(&step_env));
            // Step-level env is the highest-precedence layer; evaluate
            // expressions in its values against the env so far.
            for (k, v) in &step.env {
                let rendered = ctx.render(v);
                step_env.insert(k.clone(), rendered);
                user_keys.insert(k.clone());
            }
            // Secrets and vars sit on top, matching the prior behaviour.
            for (k, v) in &task.secrets {
                step_env.insert(k.clone(), v.clone());
            }
            for (k, v) in &task.vars {
                step_env.insert(k.clone(), v.clone());
            }
            ctx.set("env", env_to_json(&step_env));

            // Per-step writable files for `$GITHUB_OUTPUT` / `$GITHUB_ENV`.
            let out_file = runner_temp.join(format!("step-{}-output", i));
            let env_file = runner_temp.join(format!("step-{}-env", i));
            let _ = tokio::fs::write(&out_file, b"").await;
            let _ = tokio::fs::write(&env_file, b"").await;
            if let Some(user) = run_as {
                for f in [&out_file, &env_file] {
                    let _ = Command::new("chown")
                        .arg(format!("{}:staff", user))
                        .arg(f)
                        .status()
                        .await;
                }
            }
            step_env.insert("GITHUB_OUTPUT".to_string(), out_file.display().to_string());
            step_env.insert("GITHUB_ENV".to_string(), env_file.display().to_string());

            // --- Execute ---
            let r = if let Some(run_cmd) = &step.run {
                let rendered = ctx.render(run_cmd);
                reporter
                    .logf(format!("$ {}", rendered.lines().next().unwrap_or("")))
                    .await;
                execute_run_step(
                    &rendered,
                    step.shell.as_deref(),
                    step.working_directory.as_deref(),
                    &workspace,
                    &step_env,
                    &user_keys,
                    run_as,
                    &reporter,
                    &mut shutdown,
                )
                .await
            } else if let Some(uses) = &step.uses {
                let with = render_map(&step.with, &ctx);
                execute_uses_step(uses, &job_dir, &step_env, &with, run_as, &reporter).await
            } else {
                reporter.log("Step has no 'run' or 'uses' — skipping").await;
                Ok(proto::TaskResult::Skipped)
            };

            result = match r {
                Ok(r) => r,
                Err(e) => {
                    reporter.logf(format!("Step error: {:#}", e)).await;
                    proto::TaskResult::Failure
                }
            };

            // --- Collect step outputs + exported env (Gap 4) ---
            step_outputs = parse_kv_file(&out_file).await;
            for (k, v) in parse_kv_file(&env_file).await {
                user_keys.insert(k.clone());
                env_overlay.insert(k, v);
            }
        }

        reporter.log("::endgroup::").await;
        let current_log = reporter.flush_logs().await.unwrap_or(log_index);

        step_states.push(StepState {
            id: i as i64,
            result,
            started_at: Some(step_start),
            stopped_at: Some(Timestamp::now()),
            log_index,
            log_length: current_log - log_index,
        });
        log_index = current_log;

        // Feed the `steps.<id>.outputs` context for later steps + job outputs.
        let conclusion = result_str(result);
        let entry = serde_json::json!({
            "outputs": step_outputs,
            "outcome": conclusion,
            "conclusion": conclusion,
        });
        if !step.id.is_empty() {
            steps_json.insert(step.id.clone(), entry);
        }

        if result == proto::TaskResult::Failure {
            overall_result = proto::TaskResult::Failure;
        }
    }

    // --- Job outputs (Gap 4) --------------------------------------------
    // The job's `outputs:` block references `${{ steps.<id>.outputs.<x> }}`;
    // evaluate it now that every step's outputs are in the context.
    ctx.status = if overall_result == proto::TaskResult::Failure {
        JobStatus::Failure
    } else {
        JobStatus::Success
    };
    ctx.set("steps", serde_json::Value::Object(steps_json));
    let job_outputs = parse_job_outputs(job, &ctx);
    if !job_outputs.is_empty() {
        info!("job produced {} output(s)", job_outputs.len());
    }

    // Clean up job directory
    if let Err(e) = tokio::fs::remove_dir_all(&job_dir).await {
        warn!("failed to clean up job dir: {}", e);
    }

    reporter
        .report_completed(overall_result, step_states, job_outputs)
        .await?;

    Ok(overall_result)
}

fn extract_job_id(context: &serde_json::Value) -> Option<String> {
    // Gitea sends github context flat at root; some impls nest under "github"
    context
        .get("job")
        .and_then(|j| j.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            context
                .get("github")
                .and_then(|g| g.get("job"))
                .and_then(|j| j.as_str())
                .map(|s| s.to_string())
        })
}

fn find_job<'a>(
    workflow: &'a serde_yaml::Value,
    job_id: Option<&str>,
) -> Option<&'a serde_yaml::Value> {
    let jobs = workflow.get("jobs")?;
    let mapping = jobs.as_mapping()?;

    if let Some(id) = job_id {
        // Try exact match
        if let Some(job) = jobs.get(id) {
            return Some(job);
        }
    }

    // If only one job, use it
    if mapping.len() == 1 {
        return mapping.values().next();
    }

    // Fallback: first job
    mapping.values().next()
}

fn parse_steps(job: &serde_yaml::Value) -> Result<Vec<Step>> {
    let steps_val = job.get("steps").context("job has no 'steps' field")?;
    let steps_seq = steps_val
        .as_sequence()
        .context("'steps' is not a sequence")?;

    let mut result = Vec::new();
    for (i, s) in steps_seq.iter().enumerate() {
        let step = Step {
            id: s
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            name: s
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("Step {}", i + 1))
                .to_string(),
            run: s.get("run").and_then(|v| v.as_str()).map(|s| s.to_string()),
            uses: s
                .get("uses")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            env: parse_string_map(s.get("env")),
            with: parse_string_map(s.get("with")),
            working_directory: s
                .get("working-directory")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            shell: s
                .get("shell")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            r#if: s.get("if").and_then(|v| v.as_str()).map(|s| s.to_string()),
        };
        result.push(step);
    }
    Ok(result)
}

/// Parse a YAML mapping into a string map. Scalar values (numbers, bools)
/// are stringified so e.g. `env: { PORT: 8080 }` survives.
fn parse_string_map(val: Option<&serde_yaml::Value>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(mapping) = val.and_then(|v| v.as_mapping()) {
        for (k, v) in mapping {
            if let Some(key) = k.as_str() {
                let value = match v {
                    serde_yaml::Value::String(s) => s.clone(),
                    serde_yaml::Value::Bool(b) => b.to_string(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                map.insert(key.to_string(), value);
            }
        }
    }
    map
}

/// `RUNNER_OS` value for the host fucina runs on.
fn runner_os() -> String {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    }
    .to_string()
}

/// `RUNNER_ARCH` value for the host fucina runs on.
fn runner_arch() -> String {
    if cfg!(target_arch = "x86_64") {
        "X64"
    } else if cfg!(target_arch = "aarch64") {
        "ARM64"
    } else {
        "X64"
    }
    .to_string()
}

/// Split a full ref into `(GITHUB_REF_NAME, GITHUB_REF_TYPE)`.
fn ref_name_type(r: &str) -> (String, String) {
    if let Some(n) = r.strip_prefix("refs/heads/") {
        (n.to_string(), "branch".to_string())
    } else if let Some(n) = r.strip_prefix("refs/tags/") {
        (n.to_string(), "tag".to_string())
    } else if let Some(n) = r.strip_prefix("refs/pull/") {
        (n.to_string(), "branch".to_string())
    } else {
        (r.to_string(), "branch".to_string())
    }
}

fn build_env(task: &Task, workspace: &Path, runner_temp: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();

    // Inject CI standard vars
    env.insert("CI".to_string(), "true".to_string());
    env.insert("GITEA_ACTIONS".to_string(), "true".to_string());
    env.insert("GITHUB_ACTIONS".to_string(), "true".to_string());

    // Extract vars from context. Gitea sends the github context as a flat
    // object at the root of `context` (github.ref → context.ref), but some
    // implementations nest under a `github` key — handle both. Scalar
    // values (strings, numbers, bools) all become GITHUB_<KEY>.
    let github_ctx = task
        .context
        .get("github")
        .and_then(|v| v.as_object())
        .or_else(|| task.context.as_object());

    if let Some(obj) = github_ctx {
        for (k, v) in obj {
            let key = format!("GITHUB_{}", k.to_uppercase());
            match v {
                serde_json::Value::String(s) => {
                    env.insert(key, s.clone());
                }
                serde_json::Value::Number(n) => {
                    env.insert(key, n.to_string());
                }
                serde_json::Value::Bool(b) => {
                    env.insert(key, b.to_string());
                }
                _ => {}
            }
        }
    }

    // Derived ref vars
    if let Some(r) = env.get("GITHUB_REF").cloned() {
        let (name, typ) = ref_name_type(&r);
        env.insert("GITHUB_REF_NAME".to_string(), name);
        env.insert("GITHUB_REF_TYPE".to_string(), typ);
    }

    // Workspace + runner vars
    env.insert(
        "GITHUB_WORKSPACE".to_string(),
        workspace.display().to_string(),
    );
    env.insert("RUNNER_TEMP".to_string(), runner_temp.display().to_string());
    env.insert("RUNNER_OS".to_string(), runner_os());
    env.insert("RUNNER_ARCH".to_string(), runner_arch());

    // Task vars
    for (k, v) in &task.vars {
        env.insert(k.clone(), v.clone());
    }

    env
}

/// Convert a string map into a JSON object for the expression evaluator.
fn env_to_json(map: &HashMap<String, String>) -> serde_json::Value {
    serde_json::Value::Object(
        map.iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect(),
    )
}

/// Render every value of a string map through the expression evaluator.
fn render_map(map: &HashMap<String, String>, ctx: &ExprCtx) -> HashMap<String, String> {
    map.iter()
        .map(|(k, v)| (k.clone(), ctx.render(v)))
        .collect()
}

fn result_str(r: proto::TaskResult) -> &'static str {
    match r {
        proto::TaskResult::Success | proto::TaskResult::Unspecified => "success",
        proto::TaskResult::Failure => "failure",
        proto::TaskResult::Cancelled => "cancelled",
        proto::TaskResult::Skipped => "skipped",
    }
}

/// Build the `${{ }}` evaluation context from the task.
fn build_expr_context(task: &Task, workspace: &Path, runner_temp: &Path) -> ExprCtx {
    let mut ctx = ExprCtx::new();
    ctx.workspace = workspace.to_path_buf();

    // `github` context: nested object if present, else the flat context.
    let github = task
        .context
        .get("github")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| task.context.clone());

    // `inputs` (workflow_dispatch / workflow_call): github.event.inputs,
    // falling back to a top-level `inputs` key.
    let inputs = github
        .get("event")
        .and_then(|e| e.get("inputs"))
        .cloned()
        .or_else(|| task.context.get("inputs").cloned())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    let matrix = task
        .context
        .get("matrix")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    ctx.set("github", github);
    ctx.set("inputs", inputs);
    ctx.set("matrix", matrix);
    ctx.set("secrets", string_map_to_json(&task.secrets));
    ctx.set("vars", string_map_to_json(&task.vars));
    ctx.set("needs", needs_to_json(&task.needs));
    ctx.set(
        "runner",
        serde_json::json!({
            "os": runner_os(),
            "arch": runner_arch(),
            "name": "fucina",
            "temp": runner_temp.display().to_string(),
        }),
    );
    ctx.set("job", serde_json::json!({ "status": "success" }));
    ctx.set("steps", serde_json::Value::Object(serde_json::Map::new()));
    ctx.set("env", serde_json::Value::Object(serde_json::Map::new()));
    ctx
}

fn string_map_to_json(map: &HashMap<String, String>) -> serde_json::Value {
    serde_json::Value::Object(
        map.iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect(),
    )
}

/// Convert `task.needs` into the `needs.<job>.{outputs,result}` context shape.
fn needs_to_json(needs: &HashMap<String, proto::TaskNeed>) -> serde_json::Value {
    serde_json::Value::Object(
        needs
            .iter()
            .map(|(job, need)| {
                (
                    job.clone(),
                    serde_json::json!({
                        "outputs": need.outputs,
                        "result": result_str(need.result),
                    }),
                )
            })
            .collect(),
    )
}

/// Decide whether a step should run, honouring its `if:` condition.
///
/// GitHub prepends an implicit `success() &&` to any `if:` that does not
/// itself mention a status function — so a plain `if: <expr>` step is also
/// skipped once an earlier step has failed.
fn should_run_step(step: &Step, ctx: &ExprCtx) -> bool {
    match &step.r#if {
        None => ctx.status == JobStatus::Success,
        Some(cond) => {
            let result = ctx.eval_condition(cond);
            if ExprCtx::mentions_status_fn(cond) {
                result
            } else {
                result && ctx.status == JobStatus::Success
            }
        }
    }
}

/// Compute the job-level status from the results of its `needs` dependencies.
/// This drives `success()/failure()/cancelled()` inside a job-level `if:`. A
/// failed dependency makes the job `Failure`, a cancelled one `Cancelled`;
/// otherwise `Success`.
fn job_status_from_needs(needs: &HashMap<String, proto::TaskNeed>) -> JobStatus {
    let mut cancelled = false;
    for need in needs.values() {
        match need.result {
            proto::TaskResult::Failure => return JobStatus::Failure,
            proto::TaskResult::Cancelled => cancelled = true,
            _ => {}
        }
    }
    if cancelled {
        JobStatus::Cancelled
    } else {
        JobStatus::Success
    }
}

/// Decide whether a job runs, honouring its job-level `if:`.
///
/// Gitea hands us a needs-blocked job (rather than skipping it itself) only
/// when the job has an `if:`, delegating the decision to the runner. Unlike a
/// step `if:`, a job `if:` carries NO implicit `success()` prefix: it is
/// evaluated verbatim, so e.g. `if: github.event_name == 'push'` runs even
/// after a needed job failed. A job with no `if:` is always run — Gitea only
/// dispatches such a job once all its needs have succeeded.
fn should_run_job(if_field: Option<&serde_yaml::Value>, ctx: &ExprCtx) -> bool {
    match if_field {
        None => true,
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(v) => match v.as_str() {
            Some(cond) => ctx.eval_condition(cond),
            None => true,
        },
    }
}

/// Read and parse a `$GITHUB_OUTPUT` / `$GITHUB_ENV` file.
async fn parse_kv_file(path: &Path) -> HashMap<String, String> {
    let content = tokio::fs::read_to_string(path).await.unwrap_or_default();
    parse_kv(&content)
}

/// Parse the `key=value` and heredoc (`key<<DELIM ... DELIM`) line forms
/// used by `$GITHUB_OUTPUT` and `$GITHUB_ENV`.
fn parse_kv(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        let eq = line.find('=');
        let hd = line.find("<<");
        let is_heredoc = match (eq, hd) {
            (Some(e), Some(h)) => h < e,
            (None, Some(_)) => true,
            _ => false,
        };
        if is_heredoc {
            let h = hd.unwrap();
            let key = line[..h].trim().to_string();
            let delim = line[h + 2..].trim().to_string();
            if !key.is_empty() && !delim.is_empty() {
                i += 1;
                let mut val = String::new();
                while i < lines.len() && lines[i] != delim {
                    if !val.is_empty() {
                        val.push('\n');
                    }
                    val.push_str(lines[i]);
                    i += 1;
                }
                i += 1; // skip closing delimiter
                map.insert(key, val);
                continue;
            }
        }
        if let Some(e) = eq {
            let key = line[..e].trim().to_string();
            if !key.is_empty() {
                map.insert(key, line[e + 1..].to_string());
            }
        }
        i += 1;
    }
    map
}

/// Evaluate the job's `outputs:` block against the (now fully populated)
/// expression context.
fn parse_job_outputs(job: &serde_yaml::Value, ctx: &ExprCtx) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(mapping) = job.get("outputs").and_then(|v| v.as_mapping()) {
        for (k, v) in mapping {
            if let (Some(key), Some(val)) = (k.as_str(), v.as_str()) {
                out.insert(key.to_string(), ctx.render(val));
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn execute_run_step(
    run_cmd: &str,
    shell: Option<&str>,
    working_directory: Option<&str>,
    workspace: &Path,
    env: &HashMap<String, String>,
    user_keys: &HashSet<String>,
    run_as: Option<&str>,
    reporter: &Reporter,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<proto::TaskResult> {
    let shell = shell.unwrap_or("bash");
    let (shell_bin, shell_args) = match shell {
        "bash" => ("bash", vec!["-e", "-o", "pipefail", "-c"]),
        "sh" => ("sh", vec!["-e", "-c"]),
        "python" => ("python3", vec!["-c"]),
        other => (other, vec!["-c"]),
    };

    // Match GitHub/Gitea Actions semantics: steps run from the checked-out
    // repo (workspace), not from job_dir itself. working_directory is
    // interpreted relative to that workspace root.
    let work = working_directory
        .map(|d| workspace.join(d))
        .unwrap_or_else(|| workspace.to_path_buf());

    tokio::fs::create_dir_all(&work).await?;
    if let Some(user) = run_as {
        let _ = Command::new("chown")
            .arg(format!("{}:staff", user))
            .arg(&work)
            .status()
            .await;
    }

    // Build the command, wrapping in `sudo -u <user> -H -E` when run_as is set.
    // -H sets HOME to target user's home; -E preserves the parent env (PATH, etc.).
    let mut cmd = match run_as {
        Some(user) => {
            let mut c = Command::new("sudo");
            c.args(["-u", user, "-H", "-E", "--", shell_bin]);
            c.args(&shell_args);
            c.arg(run_cmd);
            c
        }
        None => {
            let mut c = Command::new(shell_bin);
            c.args(&shell_args);
            c.arg(run_cmd);
            c
        }
    };
    cmd.current_dir(&work)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // When running as a different user, strip daemon-context env vars that
    // point at root-owned paths. sudo -H sets HOME to the target user's home;
    // cargo/rustup then default to $HOME/.cargo and $HOME/.rustup respectively.
    if run_as.is_some() {
        for key in ["HOME", "CARGO_HOME", "RUSTUP_HOME", "USER", "LOGNAME"] {
            if !user_keys.contains(key) {
                cmd.env_remove(key);
            }
        }
    }

    let mut child = cmd.spawn().context("failed to spawn command")?;

    // Stream stdout and stderr concurrently via a shared channel.
    // This gives live log uploads during long-running steps (playwright, builds, etc.)
    // instead of buffering everything until the command exits.
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);

    let tx1 = tx.clone();
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx1.send(line).await.is_err() {
                break;
            }
        }
    });

    let tx2 = tx.clone();
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx2.send(line).await.is_err() {
                break;
            }
        }
    });
    drop(tx); // channel closes when both IO tasks finish

    let mut line_count: u32 = 0;
    const FLUSH_EVERY: u32 = 20;
    let mut killed = false;

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(line) => {
                        reporter.logf(line).await;
                        line_count += 1;
                        if line_count.is_multiple_of(FLUSH_EVERY) {
                            let _ = reporter.flush_logs().await;
                        }
                    }
                    None => break, // both stdout and stderr drained
                }
            }
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    // Runner is shutting down — kill the child process so the
                    // spawned IO tasks exit, then report failure.
                    let _ = child.kill().await;
                    killed = true;
                    break;
                }
            }
        }
    }

    let _ = out_task.await;
    let _ = err_task.await;
    let _ = reporter.flush_logs().await;

    if killed {
        reporter.log("Runner shutting down — step killed").await;
        let _ = reporter.flush_logs().await;
        return Ok(proto::TaskResult::Failure);
    }

    let status = child.wait().await.context("failed to wait for command")?;

    if status.success() {
        reporter
            .logf(format!("Exit code: {}", status.code().unwrap_or(0)))
            .await;
        Ok(proto::TaskResult::Success)
    } else {
        let code = status.code().unwrap_or(-1);
        reporter.logf(format!("Exit code: {}", code)).await;
        error!("step failed with exit code {}", code);
        Ok(proto::TaskResult::Failure)
    }
}

async fn execute_uses_step(
    uses: &str,
    job_dir: &Path,
    env_vars: &HashMap<String, String>,
    with: &HashMap<String, String>,
    run_as: Option<&str>,
    reporter: &Reporter,
) -> Result<proto::TaskResult> {
    // Basic support for common actions
    if uses.starts_with("actions/checkout") {
        return execute_checkout(job_dir, env_vars, with, run_as, reporter).await;
    }

    reporter
        .logf(format!(
            "⚠ Action '{}' not supported in host mode — skipping",
            uses
        ))
        .await;
    Ok(proto::TaskResult::Skipped)
}

/// Resolve the auth token for checkout. An explicit `with: token:` wins,
/// then the job's `GITHUB_TOKEN`/`GITEA_TOKEN` (Gitea issues one per job,
/// present in the step env via secrets and the github context), else None
/// → anonymous clone.
fn resolve_checkout_token(
    with: &HashMap<String, String>,
    env_vars: &HashMap<String, String>,
) -> Option<String> {
    [
        with.get("token"),
        env_vars.get("GITHUB_TOKEN"),
        env_vars.get("GITEA_TOKEN"),
    ]
    .into_iter()
    .flatten()
    .find(|s| !s.is_empty())
    .cloned()
}

/// A `git` command, optionally dropped to `run_as` and optionally
/// authenticated.
///
/// Auth: an inline credential helper reads the token from the child's
/// environment — the token never appears in argv (visible in `ps`) and never
/// touches disk, so the remote URL in `.git/config` stays clean. The first
/// empty `credential.helper=` clears any ambient helpers (e.g. the macOS
/// keychain, which made anonymous clones "work" on speedwagon by accident) so
/// behaviour is identical on every host.
///
/// Privilege: when `run_as` is set the git process runs as that user via
/// `sudo -u` — the same unprivileged user the steps run as. So checkout never
/// touches the filesystem as the root daemon, and the repo it produces is
/// owned by the user that later steps run as (no cross-user "dubious ownership"
/// or unwritable-`node_modules` surprises). The token rides through
/// `--preserve-env` — a named passthrough, never argv — instead of sudo's
/// default env scrub.
fn git_cmd(token: Option<&str>, run_as: Option<&str>) -> Command {
    // Credential-helper flags, emitted right after `git`.
    let mut cred: Vec<&str> = Vec::new();
    if token.is_some() {
        cred.extend_from_slice(&[
            "-c",
            "credential.helper=",
            "-c",
            "credential.helper=!f() { echo username=x-access-token; echo \"password=${FUCINA_GIT_TOKEN}\"; }; f",
        ]);
    }

    let mut c = match run_as {
        Some(user) => {
            let mut c = Command::new("sudo");
            c.args(["-u", user, "-H"]);
            c.arg(if token.is_some() {
                "--preserve-env=FUCINA_GIT_TOKEN,GIT_TERMINAL_PROMPT"
            } else {
                "--preserve-env=GIT_TERMINAL_PROMPT"
            });
            c.arg("--").arg("git");
            c.args(&cred);
            c
        }
        None => {
            let mut c = Command::new("git");
            c.args(&cred);
            c
        }
    };
    c.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(token) = token {
        c.env("FUCINA_GIT_TOKEN", token);
    }
    c
}

async fn execute_checkout(
    job_dir: &Path,
    env_vars: &HashMap<String, String>,
    with: &HashMap<String, String>,
    run_as: Option<&str>,
    reporter: &Reporter,
) -> Result<proto::TaskResult> {
    let server_url = env_vars
        .get("GITHUB_SERVER_URL")
        .map(|s| s.as_str())
        .unwrap_or("");
    // `with: repository:` / `with: ref:` override the context-derived values.
    let repository = with
        .get("repository")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| env_vars.get("GITHUB_REPOSITORY").map(|s| s.as_str()))
        .unwrap_or("");
    let ref_name = with
        .get("ref")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| env_vars.get("GITHUB_REF").map(|s| s.as_str()))
        .unwrap_or("refs/heads/main");

    if server_url.is_empty() || repository.is_empty() {
        reporter
            .log("Cannot checkout: missing GITHUB_SERVER_URL or GITHUB_REPOSITORY")
            .await;
        return Ok(proto::TaskResult::Failure);
    }

    let repo_url = build_repo_url(server_url, repository);
    let workspace = job_dir.join("workspace");
    let token = resolve_checkout_token(with, env_vars);
    reporter
        .logf(format!(
            "Cloning {} (ref {}, auth: {})",
            repo_url,
            ref_name,
            if token.is_some() {
                "token"
            } else {
                "anonymous"
            }
        ))
        .await;

    // Clone the default branch shallow, then resolve the requested ref
    // explicitly. `git fetch <ref>` + `git checkout FETCH_HEAD` is
    // ref-type-agnostic — it handles branches (refs/heads/…), tags
    // (refs/tags/…) and raw SHAs uniformly. `git clone --branch` only
    // accepts a *short* branch/tag name and chokes on full ref paths.
    // All three git invocations run as `run_as` (when set), so the checkout is
    // produced and owned by the user the steps run as. `execute()` has already
    // chowned the job dir to that user, so the pre-created `workspace/` is
    // writable by the clone.
    let clone = git_cmd(token.as_deref(), run_as)
        .args(["clone", "--depth", "1"])
        .arg(&repo_url)
        .arg(&workspace)
        .output()
        .await
        .context("git clone failed")?;
    log_command_output(reporter, &clone).await;
    if !clone.status.success() {
        return Ok(proto::TaskResult::Failure);
    }

    let fetch = git_cmd(token.as_deref(), run_as)
        .args(["fetch", "--depth", "1", "origin"])
        .arg(ref_name)
        .current_dir(&workspace)
        .output()
        .await
        .context("git fetch failed")?;
    log_command_output(reporter, &fetch).await;
    if !fetch.status.success() {
        return Ok(proto::TaskResult::Failure);
    }

    // Local checkout needs no token, but MUST run as the same user — running it
    // as root against a run_as-owned repo would trip git's dubious-ownership guard.
    let checkout = git_cmd(None, run_as)
        .args(["checkout", "FETCH_HEAD"])
        .current_dir(&workspace)
        .output()
        .await
        .context("git checkout failed")?;
    log_command_output(reporter, &checkout).await;
    if !checkout.status.success() {
        return Ok(proto::TaskResult::Failure);
    }

    Ok(proto::TaskResult::Success)
}

/// Build the clone URL. `GITHUB_SERVER_URL` arrives with a trailing slash,
/// so trim it to avoid a `//` in the resulting path.
fn build_repo_url(server_url: &str, repository: &str) -> String {
    format!("{}/{}.git", server_url.trim_end_matches('/'), repository)
}

/// Stream a finished command's stdout then stderr into the task log.
async fn log_command_output(reporter: &Reporter, output: &std::process::Output) {
    for stream in [&output.stdout, &output.stderr] {
        for line in String::from_utf8_lossy(stream).lines() {
            reporter.logf(line.to_string()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- extract_job_id ---

    #[test]
    fn extract_job_id_from_github_context() {
        let ctx = json!({"github": {"job": "build"}});
        assert_eq!(extract_job_id(&ctx), Some("build".to_string()));
    }

    #[test]
    fn extract_job_id_from_flat_context() {
        let ctx = json!({"job": "deploy"});
        assert_eq!(extract_job_id(&ctx), Some("deploy".to_string()));
    }

    #[test]
    fn extract_job_id_missing() {
        let ctx = json!({"github": {"ref": "main"}});
        assert_eq!(extract_job_id(&ctx), None);
    }

    #[test]
    fn extract_job_id_null_context() {
        assert_eq!(extract_job_id(&serde_json::Value::Null), None);
    }

    // --- find_job ---

    #[test]
    fn find_job_by_id() {
        let wf: serde_yaml::Value = serde_yaml::from_str(
            r#"
jobs:
  build:
    steps:
      - run: echo build
  test:
    steps:
      - run: echo test
"#,
        )
        .unwrap();
        let job = find_job(&wf, Some("test")).unwrap();
        let step = job["steps"][0]["run"].as_str().unwrap();
        assert_eq!(step, "echo test");
    }

    #[test]
    fn find_job_single_job_no_id() {
        let wf: serde_yaml::Value = serde_yaml::from_str(
            r#"
jobs:
  only-job:
    steps:
      - run: echo only
"#,
        )
        .unwrap();
        let job = find_job(&wf, None).unwrap();
        let step = job["steps"][0]["run"].as_str().unwrap();
        assert_eq!(step, "echo only");
    }

    #[test]
    fn find_job_fallback_first() {
        let wf: serde_yaml::Value = serde_yaml::from_str(
            r#"
jobs:
  a:
    steps:
      - run: echo a
  b:
    steps:
      - run: echo b
"#,
        )
        .unwrap();
        let job = find_job(&wf, Some("nonexistent"));
        assert!(job.is_some());
    }

    #[test]
    fn find_job_no_jobs_key() {
        let wf: serde_yaml::Value = serde_yaml::from_str("name: test\n").unwrap();
        assert!(find_job(&wf, None).is_none());
    }

    // --- parse_steps ---

    #[test]
    fn parse_steps_basic() {
        let job: serde_yaml::Value = serde_yaml::from_str(
            r#"
steps:
  - name: Greet
    run: echo hello
  - name: Checkout
    uses: actions/checkout@v4
  - run: echo unnamed
"#,
        )
        .unwrap();
        let steps = parse_steps(&job).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].name, "Greet");
        assert_eq!(steps[0].run.as_deref(), Some("echo hello"));
        assert!(steps[0].uses.is_none());
        assert_eq!(steps[1].name, "Checkout");
        assert_eq!(steps[1].uses.as_deref(), Some("actions/checkout@v4"));
        assert!(steps[1].run.is_none());
        assert_eq!(steps[2].name, "Step 3");
        assert_eq!(steps[2].run.as_deref(), Some("echo unnamed"));
    }

    #[test]
    fn parse_steps_with_env_and_shell() {
        let job: serde_yaml::Value = serde_yaml::from_str(
            r#"
steps:
  - name: Custom
    run: echo $FOO
    shell: sh
    working-directory: sub
    env:
      FOO: bar
      BAZ: qux
"#,
        )
        .unwrap();
        let steps = parse_steps(&job).unwrap();
        assert_eq!(steps[0].shell.as_deref(), Some("sh"));
        assert_eq!(steps[0].working_directory.as_deref(), Some("sub"));
        assert_eq!(steps[0].env.get("FOO").unwrap(), "bar");
        assert_eq!(steps[0].env.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn parse_steps_with_if() {
        let job: serde_yaml::Value = serde_yaml::from_str(
            r#"
steps:
  - name: Always
    run: echo always
    if: always()
  - name: On Failure
    run: echo fail
    if: failure()
"#,
        )
        .unwrap();
        let steps = parse_steps(&job).unwrap();
        assert_eq!(steps[0].r#if.as_deref(), Some("always()"));
        assert_eq!(steps[1].r#if.as_deref(), Some("failure()"));
    }

    #[test]
    fn parse_steps_with_with() {
        let job: serde_yaml::Value = serde_yaml::from_str(
            r#"
steps:
  - uses: actions/checkout@v4
    with:
      ref: refs/tags/v1.0.0
      fetch-depth: 0
"#,
        )
        .unwrap();
        let steps = parse_steps(&job).unwrap();
        assert_eq!(steps[0].with.get("ref").unwrap(), "refs/tags/v1.0.0");
        // numeric `with` values are stringified
        assert_eq!(steps[0].with.get("fetch-depth").unwrap(), "0");
    }

    #[test]
    fn parse_steps_no_steps_key() {
        let job: serde_yaml::Value = serde_yaml::from_str("runs-on: ubuntu\n").unwrap();
        assert!(parse_steps(&job).is_err());
    }

    #[test]
    fn parse_steps_multiline_run() {
        let job: serde_yaml::Value = serde_yaml::from_str(
            r#"
steps:
  - name: Multi
    run: |
      echo line1
      echo line2
      echo line3
"#,
        )
        .unwrap();
        let steps = parse_steps(&job).unwrap();
        let run = steps[0].run.as_ref().unwrap();
        assert!(run.contains("echo line1"));
        assert!(run.contains("echo line3"));
        assert_eq!(run.lines().count(), 3);
    }

    // --- parse_string_map ---

    #[test]
    fn parse_string_map_some() {
        let val: serde_yaml::Value = serde_yaml::from_str("A: one\nB: two\n").unwrap();
        let map = parse_string_map(Some(&val));
        assert_eq!(map.get("A").unwrap(), "one");
        assert_eq!(map.get("B").unwrap(), "two");
    }

    #[test]
    fn parse_string_map_none() {
        let map = parse_string_map(None);
        assert!(map.is_empty());
    }

    #[test]
    fn parse_string_map_scalars() {
        // numbers and bools are stringified, not dropped
        let val: serde_yaml::Value = serde_yaml::from_str("PORT: 8080\nDEBUG: true\n").unwrap();
        let map = parse_string_map(Some(&val));
        assert_eq!(map.get("PORT").unwrap(), "8080");
        assert_eq!(map.get("DEBUG").unwrap(), "true");
    }

    // --- build_env ---

    #[test]
    fn build_env_standard_vars() {
        let task = Task {
            context: json!({"github": {
                "ref": "refs/heads/main",
                "repository": "owner/repo",
                "sha": "deadbeef",
                "run_number": 7,
                "event_name": "push"
            }}),
            vars: HashMap::from([("MY_VAR".to_string(), "val".to_string())]),
            ..Default::default()
        };
        let env = build_env(&task, Path::new("/tmp/ws"), Path::new("/tmp/tmp"));
        assert_eq!(env.get("CI").unwrap(), "true");
        assert_eq!(env.get("GITEA_ACTIONS").unwrap(), "true");
        assert_eq!(env.get("GITHUB_ACTIONS").unwrap(), "true");
        assert_eq!(env.get("GITHUB_REF").unwrap(), "refs/heads/main");
        assert_eq!(env.get("GITHUB_REPOSITORY").unwrap(), "owner/repo");
        assert_eq!(env.get("GITHUB_SHA").unwrap(), "deadbeef");
        assert_eq!(env.get("MY_VAR").unwrap(), "val");
    }

    #[test]
    fn build_env_derived_vars() {
        let task = Task {
            context: json!({"github": {
                "ref": "refs/tags/v2.5.0",
                "run_number": 99,
                "event_name": "push"
            }}),
            ..Default::default()
        };
        let env = build_env(&task, Path::new("/work/ws"), Path::new("/work/tmp"));
        // Gap 2: derived + workspace + runner vars
        assert_eq!(env.get("GITHUB_REF_NAME").unwrap(), "v2.5.0");
        assert_eq!(env.get("GITHUB_REF_TYPE").unwrap(), "tag");
        assert_eq!(env.get("GITHUB_WORKSPACE").unwrap(), "/work/ws");
        assert_eq!(env.get("RUNNER_TEMP").unwrap(), "/work/tmp");
        assert!(env.contains_key("RUNNER_OS"));
        assert!(env.contains_key("RUNNER_ARCH"));
        // numeric context values are stringified
        assert_eq!(env.get("GITHUB_RUN_NUMBER").unwrap(), "99");
        assert_eq!(env.get("GITHUB_EVENT_NAME").unwrap(), "push");
    }

    #[test]
    fn build_env_empty_context() {
        let task = Task::default();
        let env = build_env(&task, Path::new("/tmp/ws"), Path::new("/tmp/tmp"));
        assert_eq!(env.get("CI").unwrap(), "true");
        assert!(!env.contains_key("GITHUB_REF"));
        // workspace/runner vars are present regardless of context
        assert_eq!(env.get("GITHUB_WORKSPACE").unwrap(), "/tmp/ws");
    }

    // --- ref_name_type ---

    #[test]
    fn ref_name_type_branch() {
        assert_eq!(
            ref_name_type("refs/heads/main"),
            ("main".to_string(), "branch".to_string())
        );
    }

    #[test]
    fn ref_name_type_tag() {
        assert_eq!(
            ref_name_type("refs/tags/v1.0.0"),
            ("v1.0.0".to_string(), "tag".to_string())
        );
    }

    #[test]
    fn ref_name_type_bare() {
        assert_eq!(
            ref_name_type("some-branch"),
            ("some-branch".to_string(), "branch".to_string())
        );
    }

    // --- env precedence (Gap 3) ---

    #[test]
    fn env_precedence_workflow_job_step() {
        // Later layers win: workflow < job < step.
        let mut env: HashMap<String, String> = HashMap::new();
        let workflow = HashMap::from([
            ("A".to_string(), "wf".to_string()),
            ("B".to_string(), "wf".to_string()),
            ("C".to_string(), "wf".to_string()),
        ]);
        let job = HashMap::from([
            ("B".to_string(), "job".to_string()),
            ("C".to_string(), "job".to_string()),
        ]);
        let step = HashMap::from([("C".to_string(), "step".to_string())]);
        for layer in [&workflow, &job, &step] {
            for (k, v) in layer {
                env.insert(k.clone(), v.clone());
            }
        }
        assert_eq!(env.get("A").unwrap(), "wf");
        assert_eq!(env.get("B").unwrap(), "job");
        assert_eq!(env.get("C").unwrap(), "step");
    }

    // --- parse_kv (Gap 4) ---

    #[test]
    fn parse_kv_simple_lines() {
        let map = parse_kv("foo=bar\nname=value\n");
        assert_eq!(map.get("foo").unwrap(), "bar");
        assert_eq!(map.get("name").unwrap(), "value");
    }

    #[test]
    fn parse_kv_value_with_equals() {
        // only the first '=' splits
        let map = parse_kv("url=https://x.com/?a=1&b=2\n");
        assert_eq!(map.get("url").unwrap(), "https://x.com/?a=1&b=2");
    }

    #[test]
    fn parse_kv_heredoc() {
        let content = "result<<EOF\nline one\nline two\nEOF\nflag=on\n";
        let map = parse_kv(content);
        assert_eq!(map.get("result").unwrap(), "line one\nline two");
        assert_eq!(map.get("flag").unwrap(), "on");
    }

    #[test]
    fn parse_kv_heredoc_custom_delim() {
        let content = "json<<ghadelimiter\n{\"k\": \"v\"}\nghadelimiter\n";
        let map = parse_kv(content);
        assert_eq!(map.get("json").unwrap(), "{\"k\": \"v\"}");
    }

    #[test]
    fn parse_kv_empty() {
        assert!(parse_kv("").is_empty());
        assert!(parse_kv("\n\n").is_empty());
    }

    // --- should_run_step (Gap 1) ---

    fn empty_ctx(status: JobStatus) -> ExprCtx {
        let mut c = ExprCtx::new();
        c.status = status;
        c.set(
            "github",
            json!({"ref": "refs/tags/v1.0.0", "event_name": "push"}),
        );
        c
    }

    fn step_with_if(cond: Option<&str>) -> Step {
        Step {
            id: String::new(),
            name: "s".to_string(),
            run: Some("echo hi".to_string()),
            uses: None,
            env: HashMap::new(),
            with: HashMap::new(),
            working_directory: None,
            shell: None,
            r#if: cond.map(|s| s.to_string()),
        }
    }

    #[test]
    fn should_run_no_if_runs_on_success() {
        assert!(should_run_step(
            &step_with_if(None),
            &empty_ctx(JobStatus::Success)
        ));
    }

    #[test]
    fn should_run_no_if_skips_after_failure() {
        // implicit success() — a plain step is skipped once the job failed
        assert!(!should_run_step(
            &step_with_if(None),
            &empty_ctx(JobStatus::Failure)
        ));
    }

    #[test]
    fn should_run_always_runs_after_failure() {
        assert!(should_run_step(
            &step_with_if(Some("always()")),
            &empty_ctx(JobStatus::Failure)
        ));
    }

    #[test]
    fn should_run_failure_only_on_failure() {
        assert!(should_run_step(
            &step_with_if(Some("failure()")),
            &empty_ctx(JobStatus::Failure)
        ));
        assert!(!should_run_step(
            &step_with_if(Some("failure()")),
            &empty_ctx(JobStatus::Success)
        ));
    }

    #[test]
    fn should_run_expr_condition() {
        // a non-status `if:` gets an implicit success() prefix
        assert!(should_run_step(
            &step_with_if(Some("startsWith(github.ref, 'refs/tags/')")),
            &empty_ctx(JobStatus::Success)
        ));
        assert!(!should_run_step(
            &step_with_if(Some("startsWith(github.ref, 'refs/heads/')")),
            &empty_ctx(JobStatus::Success)
        ));
        // ...so it is skipped after a failure even when the expr is true
        assert!(!should_run_step(
            &step_with_if(Some("startsWith(github.ref, 'refs/tags/')")),
            &empty_ctx(JobStatus::Failure)
        ));
    }

    #[test]
    fn should_run_expr_with_status_fn_no_implicit_success() {
        // mentioning always() drops the implicit success() prefix
        assert!(should_run_step(
            &step_with_if(Some("always() && startsWith(github.ref, 'refs/tags/')")),
            &empty_ctx(JobStatus::Failure)
        ));
    }

    // --- job-level if: (needs-gate delegation) ---

    fn need(result: proto::TaskResult) -> proto::TaskNeed {
        proto::TaskNeed {
            outputs: HashMap::new(),
            result,
        }
    }

    #[test]
    fn job_status_from_needs_variants() {
        assert_eq!(job_status_from_needs(&HashMap::new()), JobStatus::Success);

        let ok = HashMap::from([("a".to_string(), need(proto::TaskResult::Success))]);
        assert_eq!(job_status_from_needs(&ok), JobStatus::Success);

        let failed = HashMap::from([
            ("a".to_string(), need(proto::TaskResult::Success)),
            ("b".to_string(), need(proto::TaskResult::Failure)),
        ]);
        assert_eq!(job_status_from_needs(&failed), JobStatus::Failure);

        // failure wins over cancellation
        let mixed = HashMap::from([
            ("a".to_string(), need(proto::TaskResult::Cancelled)),
            ("b".to_string(), need(proto::TaskResult::Failure)),
        ]);
        assert_eq!(job_status_from_needs(&mixed), JobStatus::Failure);

        let cancelled = HashMap::from([("a".to_string(), need(proto::TaskResult::Cancelled))]);
        assert_eq!(job_status_from_needs(&cancelled), JobStatus::Cancelled);
    }

    #[test]
    fn should_run_job_no_if_always_runs() {
        // Gitea only dispatches a no-if job once its needs succeeded.
        assert!(should_run_job(None, &empty_ctx(JobStatus::Failure)));
    }

    #[test]
    fn should_run_job_if_is_verbatim_no_implicit_success() {
        // The cali/niveau case: `if: github.event_name == 'push'` still runs
        // after a needed job failed — a job `if:` gets NO implicit success().
        let cond = serde_yaml::Value::String("github.event_name == 'push'".to_string());
        assert!(should_run_job(Some(&cond), &empty_ctx(JobStatus::Failure)));
    }

    #[test]
    fn should_run_job_explicit_success_gate_skips_on_failed_need() {
        let cond = serde_yaml::Value::String("success()".to_string());
        assert!(!should_run_job(Some(&cond), &empty_ctx(JobStatus::Failure)));
        assert!(should_run_job(Some(&cond), &empty_ctx(JobStatus::Success)));
    }

    #[test]
    fn should_run_job_always_and_failure() {
        let always = serde_yaml::Value::String("always()".to_string());
        assert!(should_run_job(
            Some(&always),
            &empty_ctx(JobStatus::Failure)
        ));
        let failure = serde_yaml::Value::String("failure()".to_string());
        assert!(should_run_job(
            Some(&failure),
            &empty_ctx(JobStatus::Failure)
        ));
        assert!(!should_run_job(
            Some(&failure),
            &empty_ctx(JobStatus::Success)
        ));
    }

    #[test]
    fn should_run_job_bool_literal() {
        assert!(should_run_job(
            Some(&serde_yaml::Value::Bool(true)),
            &empty_ctx(JobStatus::Success)
        ));
        assert!(!should_run_job(
            Some(&serde_yaml::Value::Bool(false)),
            &empty_ctx(JobStatus::Success)
        ));
    }

    // --- parse_job_outputs (Gap 4) ---

    #[test]
    fn parse_job_outputs_resolves_step_outputs() {
        let job: serde_yaml::Value = serde_yaml::from_str(
            r#"
outputs:
  version: ${{ steps.build.outputs.ver }}
  static: hello
"#,
        )
        .unwrap();
        let mut ctx = ExprCtx::new();
        ctx.set("steps", json!({"build": {"outputs": {"ver": "1.2.3"}}}));
        let out = parse_job_outputs(&job, &ctx);
        assert_eq!(out.get("version").unwrap(), "1.2.3");
        assert_eq!(out.get("static").unwrap(), "hello");
    }

    // --- needs_to_json (Gap 4) ---

    #[test]
    fn needs_to_json_shape() {
        let needs = HashMap::from([(
            "compile".to_string(),
            proto::TaskNeed {
                outputs: HashMap::from([("artifact".to_string(), "out.tar".to_string())]),
                result: proto::TaskResult::Success,
            },
        )]);
        let v = needs_to_json(&needs);
        assert_eq!(v["compile"]["outputs"]["artifact"], json!("out.tar"));
        assert_eq!(v["compile"]["result"], json!("success"));
    }

    // --- resolve_checkout_token ---

    #[test]
    fn checkout_token_with_overrides_env() {
        let with = HashMap::from([("token".to_string(), "with-tok".to_string())]);
        let env = HashMap::from([("GITHUB_TOKEN".to_string(), "env-tok".to_string())]);
        assert_eq!(
            resolve_checkout_token(&with, &env).as_deref(),
            Some("with-tok")
        );
    }

    #[test]
    fn checkout_token_empty_with_falls_through() {
        // `with: token: ''` (e.g. an unresolved expression) must not shadow
        // the job token
        let with = HashMap::from([("token".to_string(), String::new())]);
        let env = HashMap::from([("GITHUB_TOKEN".to_string(), "env-tok".to_string())]);
        assert_eq!(
            resolve_checkout_token(&with, &env).as_deref(),
            Some("env-tok")
        );
    }

    #[test]
    fn checkout_token_gitea_fallback() {
        let with = HashMap::new();
        let env = HashMap::from([("GITEA_TOKEN".to_string(), "gitea-tok".to_string())]);
        assert_eq!(
            resolve_checkout_token(&with, &env).as_deref(),
            Some("gitea-tok")
        );
    }

    #[test]
    fn checkout_token_none_is_anonymous() {
        assert_eq!(
            resolve_checkout_token(&HashMap::new(), &HashMap::new()),
            None
        );
    }

    #[test]
    fn git_cmd_keeps_token_out_of_argv() {
        let cmd = git_cmd(Some("s3cr3t"), None);
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "git");
        for arg in std.get_args() {
            assert!(
                !arg.to_string_lossy().contains("s3cr3t"),
                "token leaked into argv: {:?}",
                arg
            );
        }
        // token flows via the child env instead
        assert!(std
            .get_envs()
            .any(|(k, v)| k == "FUCINA_GIT_TOKEN" && v.map(|v| v == "s3cr3t").unwrap_or(false)));
    }

    #[test]
    fn git_cmd_anonymous_has_no_helper() {
        let cmd = git_cmd(None, None);
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "git");
        assert_eq!(std.get_args().count(), 0);
        assert!(std.get_envs().all(|(k, _)| k != "FUCINA_GIT_TOKEN"));
    }

    #[test]
    fn git_cmd_run_as_wraps_sudo() {
        let cmd = git_cmd(None, Some("ci"));
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "sudo");
        let args: Vec<String> = std
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // drops to ci, sets its HOME, then runs git
        assert_eq!(
            &args[..4],
            &["-u", "ci", "-H", "--preserve-env=GIT_TERMINAL_PROMPT"]
        );
        assert_eq!(args[4], "--");
        assert_eq!(args[5], "git");
        assert!(std.get_envs().all(|(k, _)| k != "FUCINA_GIT_TOKEN"));
    }

    #[test]
    fn git_cmd_run_as_with_token_keeps_token_out_of_argv() {
        let cmd = git_cmd(Some("s3cr3t"), Some("ci"));
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "sudo");
        for arg in std.get_args() {
            assert!(
                !arg.to_string_lossy().contains("s3cr3t"),
                "token leaked into argv: {:?}",
                arg
            );
        }
        // token named in the sudo passthrough; value lives only in the env
        assert!(std
            .get_args()
            .any(|a| a.to_string_lossy() == "--preserve-env=FUCINA_GIT_TOKEN,GIT_TERMINAL_PROMPT"));
        assert!(std
            .get_envs()
            .any(|(k, v)| k == "FUCINA_GIT_TOKEN" && v.map(|v| v == "s3cr3t").unwrap_or(false)));
    }

    // --- build_repo_url ---

    #[test]
    fn build_repo_url_trims_trailing_slash() {
        assert_eq!(
            build_repo_url("https://git.calii.net/", "cali/scrytti"),
            "https://git.calii.net/cali/scrytti.git"
        );
    }

    #[test]
    fn build_repo_url_no_trailing_slash() {
        assert_eq!(
            build_repo_url("https://git.calii.net", "cali/scrytti"),
            "https://git.calii.net/cali/scrytti.git"
        );
    }
}
