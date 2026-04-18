use anyhow::{Context, Result};
use base64::Engine;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;
use tracing::{error, info, warn};

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

    // Set up working directory
    let job_dir = work_dir.join(format!("task-{}", task.id));
    tokio::fs::create_dir_all(&job_dir)
        .await
        .context("failed to create job directory")?;

    // If steps will run as a different user, hand ownership of the job dir
    // so that user can create target/, caches, etc.
    if let Some(user) = run_as {
        let _ = Command::new("chown")
            .arg("-R")
            .arg(format!("{}:staff", user))
            .arg(&job_dir)
            .status()
            .await;
    }

    // Build environment variables from task context
    let env_vars = build_env(task);

    reporter.report_started().await?;

    let mut step_states = Vec::new();
    let mut overall_result = proto::TaskResult::Success;
    let mut log_index: i64 = 0;

    for (i, step) in steps.iter().enumerate() {
        let step_id = i as i64;
        let step_name = if step.name.is_empty() {
            format!("Step {}", i + 1)
        } else {
            step.name.clone()
        };

        reporter.logf(format!("::group::{}", step_name)).await;

        let step_start = Timestamp::now();

        // Check if step should be skipped (basic `if` support)
        if let Some(condition) = &step.r#if {
            if should_skip(condition, overall_result) {
                reporter
                    .logf(format!("Skipping: if condition '{}' not met", condition))
                    .await;
                reporter.flush_logs().await?;
                let log_end = log_index + 2;
                step_states.push(StepState {
                    id: step_id,
                    result: proto::TaskResult::Skipped,
                    started_at: Some(step_start.clone()),
                    stopped_at: Some(Timestamp::now()),
                    log_index,
                    log_length: log_end - log_index,
                });
                log_index = log_end;
                reporter.log("::endgroup::").await;
                continue;
            }
        }

        let step_result = if let Some(run_cmd) = &step.run {
            execute_run_step(
                run_cmd,
                step,
                &job_dir,
                &env_vars,
                &task.secrets,
                &task.vars,
                run_as,
                &reporter,
            )
            .await
        } else if let Some(uses) = &step.uses {
            execute_uses_step(uses, &job_dir, &env_vars, &reporter).await
        } else {
            reporter.log("Step has no 'run' or 'uses' — skipping").await;
            Ok(proto::TaskResult::Skipped)
        };

        let result = match step_result {
            Ok(r) => r,
            Err(e) => {
                reporter.logf(format!("Step error: {:#}", e)).await;
                proto::TaskResult::Failure
            }
        };

        reporter.log("::endgroup::").await;
        reporter.flush_logs().await?;

        let current_log = reporter.flush_logs().await.unwrap_or(log_index);
        let log_length = current_log - log_index;

        step_states.push(StepState {
            id: step_id,
            result,
            started_at: Some(step_start),
            stopped_at: Some(Timestamp::now()),
            log_index,
            log_length,
        });

        log_index = current_log;

        if result == proto::TaskResult::Failure {
            overall_result = proto::TaskResult::Failure;
            // Continue to report remaining steps as skipped? For now, stop.
            // Mark remaining steps as skipped
            for j in (i + 1)..steps.len() {
                step_states.push(StepState {
                    id: j as i64,
                    result: proto::TaskResult::Skipped,
                    started_at: None,
                    stopped_at: None,
                    log_index,
                    log_length: 0,
                });
            }
            break;
        }
    }

    // Clean up job directory
    if let Err(e) = tokio::fs::remove_dir_all(&job_dir).await {
        warn!("failed to clean up job dir: {}", e);
    }

    reporter
        .report_completed(overall_result, step_states)
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

fn parse_string_map(val: Option<&serde_yaml::Value>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(mapping) = val.and_then(|v| v.as_mapping()) {
        for (k, v) in mapping {
            if let (Some(key), Some(val)) = (k.as_str(), v.as_str()) {
                map.insert(key.to_string(), val.to_string());
            }
        }
    }
    map
}

fn build_env(task: &Task) -> HashMap<String, String> {
    let mut env = HashMap::new();

    // Inject CI standard vars
    env.insert("CI".to_string(), "true".to_string());
    env.insert("GITEA_ACTIONS".to_string(), "true".to_string());
    env.insert("GITHUB_ACTIONS".to_string(), "true".to_string());

    // Extract vars from context. Gitea sends the github context as a flat object
    // at the root of `context` (github.ref → context.ref), but some implementations
    // may nest under a `github` key — handle both.
    let github_ctx = task
        .context
        .get("github")
        .and_then(|v| v.as_object())
        .or_else(|| task.context.as_object());

    if let Some(obj) = github_ctx {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                let key = format!("GITHUB_{}", k.to_uppercase());
                env.insert(key, s.to_string());
            }
        }
    }

    // Task vars
    for (k, v) in &task.vars {
        env.insert(k.clone(), v.clone());
    }

    env
}

#[allow(clippy::too_many_arguments)]
async fn execute_run_step(
    run_cmd: &str,
    step: &Step,
    job_dir: &Path,
    env_vars: &HashMap<String, String>,
    secrets: &HashMap<String, String>,
    vars: &HashMap<String, String>,
    run_as: Option<&str>,
    reporter: &Reporter,
) -> Result<proto::TaskResult> {
    let shell = step.shell.as_deref().unwrap_or("bash");
    let (shell_bin, shell_args) = match shell {
        "bash" => ("bash", vec!["-e", "-o", "pipefail", "-c"]),
        "sh" => ("sh", vec!["-e", "-c"]),
        "python" => ("python3", vec!["-c"]),
        other => (other, vec!["-c"]),
    };

    let work = step
        .working_directory
        .as_ref()
        .map(|d| job_dir.join(d))
        .unwrap_or_else(|| job_dir.to_path_buf());

    tokio::fs::create_dir_all(&work).await?;
    if let Some(user) = run_as {
        let _ = Command::new("chown")
            .arg(format!("{}:staff", user))
            .arg(&work)
            .status()
            .await;
    }

    reporter
        .logf(format!("$ {}", run_cmd.lines().next().unwrap_or("")))
        .await;

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
        .envs(env_vars)
        .envs(&step.env)
        .envs(secrets)
        .envs(vars);

    // When running as a different user, strip daemon-context env vars that
    // point at root-owned paths. sudo -H sets HOME to the target user's home;
    // cargo/rustup then default to $HOME/.cargo and $HOME/.rustup respectively.
    if run_as.is_some() {
        for key in ["HOME", "CARGO_HOME", "RUSTUP_HOME", "USER", "LOGNAME"] {
            if !step.env.contains_key(key) {
                cmd.env_remove(key);
            }
        }
    }

    let output = cmd.output().await.context("failed to execute command")?;

    // Log stdout
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        reporter.logf(line.to_string()).await;
    }

    // Log stderr
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        reporter.logf(line.to_string()).await;
    }

    if output.status.success() {
        reporter
            .logf(format!("Exit code: {}", output.status.code().unwrap_or(0)))
            .await;
        Ok(proto::TaskResult::Success)
    } else {
        let code = output.status.code().unwrap_or(-1);
        reporter.logf(format!("Exit code: {}", code)).await;
        error!("step '{}' failed with exit code {}", step.name, code);
        Ok(proto::TaskResult::Failure)
    }
}

async fn execute_uses_step(
    uses: &str,
    job_dir: &Path,
    env_vars: &HashMap<String, String>,
    reporter: &Reporter,
) -> Result<proto::TaskResult> {
    // Basic support for common actions
    if uses.starts_with("actions/checkout") {
        return execute_checkout(job_dir, env_vars, reporter).await;
    }

    reporter
        .logf(format!(
            "⚠ Action '{}' not supported in host mode — skipping",
            uses
        ))
        .await;
    Ok(proto::TaskResult::Skipped)
}

async fn execute_checkout(
    job_dir: &Path,
    env_vars: &HashMap<String, String>,
    reporter: &Reporter,
) -> Result<proto::TaskResult> {
    let server_url = env_vars
        .get("GITHUB_SERVER_URL")
        .map(|s| s.as_str())
        .unwrap_or("");
    let repository = env_vars
        .get("GITHUB_REPOSITORY")
        .map(|s| s.as_str())
        .unwrap_or("");
    let ref_name = env_vars
        .get("GITHUB_REF")
        .map(|s| s.as_str())
        .unwrap_or("refs/heads/main");

    if server_url.is_empty() || repository.is_empty() {
        reporter
            .log("Cannot checkout: missing GITHUB_SERVER_URL or GITHUB_REPOSITORY")
            .await;
        return Ok(proto::TaskResult::Failure);
    }

    let repo_url = format!("{}/{}.git", server_url, repository);
    reporter.logf(format!("Cloning {}", repo_url)).await;

    let output = Command::new("git")
        .args(["clone", "--depth", "1", "--branch"])
        .arg(ref_name.trim_start_matches("refs/heads/"))
        .arg(&repo_url)
        .arg(job_dir.join("workspace"))
        .output()
        .await
        .context("git clone failed")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        reporter.logf(line.to_string()).await;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        reporter.logf(line.to_string()).await;
    }

    if output.status.success() {
        Ok(proto::TaskResult::Success)
    } else {
        Ok(proto::TaskResult::Failure)
    }
}

fn should_skip(condition: &str, current_result: proto::TaskResult) -> bool {
    let cond = condition.trim();
    match cond {
        "always()" => false,
        "failure()" => current_result != proto::TaskResult::Failure,
        "cancelled()" => current_result != proto::TaskResult::Cancelled,
        "success()" | "" => {
            current_result != proto::TaskResult::Success
                && current_result != proto::TaskResult::Unspecified
        }
        _ => false, // Can't evaluate complex expressions — run by default
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
        // With no matching ID, should return first job
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
    fn parse_steps_no_steps_key() {
        let job: serde_yaml::Value = serde_yaml::from_str("runs-on: ubuntu\n").unwrap();
        assert!(parse_steps(&job).is_err());
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

    // --- build_env ---

    #[test]
    fn build_env_standard_vars() {
        let task = Task {
            context: json!({"github": {"ref": "refs/heads/main", "repository": "owner/repo"}}),
            vars: HashMap::from([("MY_VAR".to_string(), "val".to_string())]),
            ..Default::default()
        };
        let env = build_env(&task);
        assert_eq!(env.get("CI").unwrap(), "true");
        assert_eq!(env.get("GITEA_ACTIONS").unwrap(), "true");
        assert_eq!(env.get("GITHUB_ACTIONS").unwrap(), "true");
        assert_eq!(env.get("GITHUB_REF").unwrap(), "refs/heads/main");
        assert_eq!(env.get("GITHUB_REPOSITORY").unwrap(), "owner/repo");
        assert_eq!(env.get("MY_VAR").unwrap(), "val");
    }

    #[test]
    fn build_env_empty_context() {
        let task = Task::default();
        let env = build_env(&task);
        assert_eq!(env.get("CI").unwrap(), "true");
        assert!(!env.contains_key("GITHUB_REF"));
    }

    // --- should_skip ---

    #[test]
    fn should_skip_always_never_skips() {
        assert!(!should_skip("always()", proto::TaskResult::Success));
        assert!(!should_skip("always()", proto::TaskResult::Failure));
        assert!(!should_skip("always()", proto::TaskResult::Cancelled));
    }

    #[test]
    fn should_skip_failure_skips_on_success() {
        assert!(should_skip("failure()", proto::TaskResult::Success));
        assert!(!should_skip("failure()", proto::TaskResult::Failure));
    }

    #[test]
    fn should_skip_success_skips_on_failure() {
        assert!(should_skip("success()", proto::TaskResult::Failure));
        assert!(!should_skip("success()", proto::TaskResult::Success));
        // Unspecified is treated as "still succeeding"
        assert!(!should_skip("success()", proto::TaskResult::Unspecified));
    }

    #[test]
    fn should_skip_cancelled() {
        assert!(should_skip("cancelled()", proto::TaskResult::Success));
        assert!(!should_skip("cancelled()", proto::TaskResult::Cancelled));
    }

    #[test]
    fn should_skip_empty_is_success() {
        assert!(!should_skip("", proto::TaskResult::Success));
        assert!(should_skip("", proto::TaskResult::Failure));
    }

    #[test]
    fn should_skip_unknown_expression_runs() {
        // Unknown expressions should default to running (not skipping)
        assert!(!should_skip(
            "github.ref == 'main'",
            proto::TaskResult::Success
        ));
        assert!(!should_skip(
            "github.ref == 'main'",
            proto::TaskResult::Failure
        ));
    }

    // --- multiline run command ---

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
}
