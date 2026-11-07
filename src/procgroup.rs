//! Reclaiming a step's whole process tree.
//!
//! fucina never holds the process that does the work: a step runs through
//! `sudo -u <user> -H -E -- bash -c …`, which forks the shell, which forks
//! `mvn`, which forks a surefire JVM. Signalling the pid fucina owns kills
//! the head and orphans everything under it — `SIGKILL` in particular can't
//! even be forwarded by `sudo`.
//!
//! Observed on giorno (2026-09-14, capucine runs 434/435): two cancelled runs
//! left 12 GB of surefire JVMs and a container reaper alive; a `bench-sink
//! --secs 6` from another repo was still running two and a half days later.
//! Orphans keep their grip on the step's stdout pipe too, so the log-forwarding
//! tasks never see EOF and job finalization can hang behind them.
//!
//! The fix is to put every step in its own process group (`process_group(0)`
//! at spawn) and signal the group, which reaches the entire tree.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Poll interval while waiting for a signalled group to go away.
const POLL: Duration = Duration::from_millis(100);

/// Send `sig` to every process in `pgid`, returning false when the group is
/// already gone (`ESRCH`) — the normal outcome for a step that exited cleanly.
#[cfg(unix)]
fn signal_group(pgid: i32, sig: i32) -> bool {
    // SAFETY: `killpg` is always defined for a valid pgid; a group that no
    // longer exists is reported as ESRCH rather than being undefined.
    unsafe { libc::killpg(pgid, sig) == 0 }
}

#[cfg(not(unix))]
fn signal_group(_pgid: i32, _sig: i32) -> bool {
    false
}

/// True while any process in the group still exists (signal 0 probes without
/// delivering). A group whose leader is an unreaped zombie still counts as
/// alive, which only ever costs `reap_group` its grace period.
pub fn group_alive(pgid: i32) -> bool {
    #[cfg(unix)]
    {
        usable(pgid) && signal_group(pgid, 0)
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

/// Guard against signalling something that isn't a step's own group. `killpg(0)`
/// targets *the caller's* group — for the root daemon that is the daemon and
/// every step it supervises — so a pgid that never got set must never be used.
fn usable(pgid: i32) -> bool {
    pgid > 1
}

/// `SIGTERM` the group, give it `grace` to unwind, then `SIGKILL` the
/// remainder. Returns true when something was actually signalled.
pub async fn reap_group(pgid: i32, grace: Duration) -> bool {
    #[cfg(unix)]
    {
        if !usable(pgid) || !signal_group(pgid, libc::SIGTERM) {
            return false;
        }
        debug!("sent SIGTERM to process group {pgid}");
        let deadline = tokio::time::Instant::now() + grace;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            if !signal_group(pgid, 0) {
                return true;
            }
        }
        debug!("process group {pgid} survived SIGTERM — sending SIGKILL");
        signal_group(pgid, libc::SIGKILL);
        true
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, grace);
        false
    }
}

/// Process groups that still have a live member whose command line mentions
/// `needle`.
///
/// This is the guard that makes reclaiming *persisted* groups safe. A pgid
/// recorded before a reboot may since have been recycled by something
/// unrelated, and signalling it blind would kill a stranger. Requiring a live
/// member to reference the runner's own work directory keeps that from ever
/// happening — a recycled pgid simply won't match.
fn groups_referencing(ps_output: &str, needle: &str) -> HashSet<i32> {
    let mut out = HashSet::new();
    for line in ps_output.lines() {
        let line = line.trim_start();
        let Some((pgid, command)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pgid) = pgid.parse::<i32>() else {
            continue;
        };
        if usable(pgid) && command.contains(needle) {
            out.insert(pgid);
        }
    }
    out
}

/// Reclaim step process groups a previous daemon left running.
///
/// Called once at startup. A daemon that is killed outright never runs its own
/// end-of-job sweep — launchd restarts the job whenever a pkg replaces the
/// bundle, which is exactly how capucine run 437 lost its backend job — and
/// since 0.5.5 a step's group no longer dies with the daemon that spawned it.
/// Anything still running from the last process is therefore reclaimed here,
/// guarded by `groups_referencing` so a recycled pgid is never signalled.
pub async fn reap_stale_groups(groups: &[i32], work_dir: &Path) {
    if groups.is_empty() {
        return;
    }
    let needle = work_dir.to_string_lossy().to_string();
    let ps = match tokio::process::Command::new("ps")
        .args(["-axo", "pgid=,command="])
        .output()
        .await
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => {
            warn!("could not list processes to reclaim stale step groups: {e}");
            return;
        }
    };
    let live = groups_referencing(&ps, &needle);
    for pgid in groups.iter().copied().filter(|p| live.contains(p)) {
        info!("reclaiming step process group {pgid} left by a previous run");
        reap_group(pgid, Duration::from_secs(5)).await;
    }
}

/// Env var set on every step process: the task id. Children inherit it
/// through `setsid`, Node's `detached: true` spawns and reparenting to launchd
/// — exactly the cases a process-group kill can't reach. Observed on
/// speedwagon: a dg-gestion e2e server started via `pnpm … start` in its own
/// process group outlived its cancelled task (4187) by a month, holding port
/// 3990 and failing every later e2e run.
pub const TASK_TAG: &str = "FUCINA_TASK_ID";

/// Does one process environment (as `KEY=value` items) belong to us?
/// `task: Some(id)` matches that task only; `None` matches any task, but then
/// `workspace_prefix` must match too, so a startup sweep only ever claims
/// processes from this runner's own work_dir.
fn env_matches<'a>(
    mut items: impl Iterator<Item = &'a str>,
    task: Option<i64>,
    workspace_prefix: Option<&str>,
) -> bool {
    let want = task.map(|id| format!("{TASK_TAG}={id}"));
    let tag_prefix = format!("{TASK_TAG}=");
    let (mut tag_ok, mut ws_ok) = (false, workspace_prefix.is_none());
    for item in items.by_ref() {
        match &want {
            Some(w) if item == w => tag_ok = true,
            None if item.starts_with(&tag_prefix) => tag_ok = true,
            _ => {}
        }
        if let Some(prefix) = workspace_prefix {
            if item
                .strip_prefix("GITHUB_WORKSPACE=")
                .is_some_and(|ws| ws.starts_with(prefix))
            {
                ws_ok = true;
            }
        }
    }
    tag_ok && ws_ok
}

/// Parse `ps -axE -ww -o pid=,command=` (macOS appends each process's
/// environment to its command line) into the pids that match.
fn tagged_in_ps(ps_output: &str, task: Option<i64>, workspace_prefix: Option<&str>) -> Vec<i32> {
    ps_output
        .lines()
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let pid: i32 = tokens.next()?.parse().ok()?;
            env_matches(tokens, task, workspace_prefix).then_some(pid)
        })
        .collect()
}

/// Every pid currently carrying the tag (see `env_matches`).
async fn list_tagged(task: Option<i64>, workspace_prefix: Option<&str>) -> Vec<i32> {
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        if let Ok(dir) = std::fs::read_dir("/proc") {
            for entry in dir.flatten() {
                let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                    continue;
                };
                let Ok(raw) = std::fs::read(entry.path().join("environ")) else {
                    continue;
                };
                let text = String::from_utf8_lossy(&raw);
                if env_matches(text.split('\0'), task, workspace_prefix) {
                    out.push(pid);
                }
            }
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        match tokio::process::Command::new("ps")
            .args(["-axE", "-ww", "-o", "pid=,command="])
            .output()
            .await
        {
            Ok(o) => tagged_in_ps(&String::from_utf8_lossy(&o.stdout), task, workspace_prefix),
            Err(e) => {
                warn!("could not list processes to sweep tagged steps: {e}");
                Vec::new()
            }
        }
    }
}

/// Reclaim every process still tagged for `task` (or, with `None`, any task
/// whose workspace lies under `workspace_prefix`), together with the process
/// group each one now leads or belongs to. Runs after the step-group kill, so
/// it only finds what escaped it. Never touches fucina's own process or group.
/// Returns how many groups or lone processes were signalled.
pub async fn reap_tagged(
    task: Option<i64>,
    workspace_prefix: Option<&str>,
    grace: Duration,
) -> usize {
    #[cfg(unix)]
    {
        let me = std::process::id() as i32;
        // SAFETY: getpgid(0) / getpgid(pid) only read process metadata.
        let my_pgid = unsafe { libc::getpgid(0) };
        let mut groups: Vec<i32> = Vec::new();
        let mut lone: Vec<i32> = Vec::new();
        for pid in list_tagged(task, workspace_prefix).await {
            if pid == me || pid <= 1 {
                continue;
            }
            let pgid = unsafe { libc::getpgid(pid) };
            if usable(pgid) && pgid != my_pgid {
                if !groups.contains(&pgid) {
                    groups.push(pgid);
                }
            } else if !lone.contains(&pid) {
                lone.push(pid);
            }
        }
        let mut signalled = 0;
        for pgid in groups {
            info!("reclaiming escaped process group {pgid} (tagged {TASK_TAG})");
            if reap_group(pgid, grace).await {
                signalled += 1;
            }
        }
        for pid in lone {
            // SAFETY: plain signal delivery to a pid we just found.
            if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                signalled += 1;
                tokio::time::sleep(grace).await;
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        signalled
    }
    #[cfg(not(unix))]
    {
        let _ = (task, workspace_prefix, grace);
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_sweep_finds_the_task_even_after_setsid() {
        // ps -axE appends the environment to the command line.
        let ps = "\
  101 /usr/sbin/cfprefsd agent
58747 node /opt/homebrew/bin/pnpm --filter @dg/api start FUCINA_TASK_ID=4187 GITHUB_WORKSPACE=/Users/cali/gitea-runner-rs/work/task-4187/workspace
58777 /opt/homebrew/bin/node src/index.ts FUCINA_TASK_ID=4187 GITHUB_WORKSPACE=/Users/cali/gitea-runner-rs/work/task-4187/workspace
60001 node other.js FUCINA_TASK_ID=4188 GITHUB_WORKSPACE=/Users/cali/gitea-runner-rs/work/task-4188/workspace
";
        assert_eq!(tagged_in_ps(ps, Some(4187), None), vec![58747, 58777]);
        // a task id that merely starts with ours is someone else's
        assert!(tagged_in_ps("1 x FUCINA_TASK_ID=41870\n", Some(4187), None).is_empty());
    }

    #[test]
    fn startup_sweep_only_claims_this_runners_work_dir() {
        let ps = "\
  700 node a.js FUCINA_TASK_ID=1 GITHUB_WORKSPACE=/Users/cali/gitea-runner-rs/work/task-1/workspace
  701 node b.js FUCINA_TASK_ID=2 GITHUB_WORKSPACE=/Users/Shared/other-runner/task-2/workspace
  702 node c.js GITHUB_WORKSPACE=/Users/cali/gitea-runner-rs/work/task-3/workspace
";
        // any task, but only under our work_dir, and only if tagged
        assert_eq!(
            tagged_in_ps(ps, None, Some("/Users/cali/gitea-runner-rs/work")),
            vec![700]
        );
    }

    #[test]
    fn only_groups_touching_the_work_dir_are_reclaimable() {
        let ps = "\
  501 /usr/libexec/secd
 2350 sudo -u ci -H -E -- bash -c cd /Users/Shared/fucina-work/task-9/workspace
 2350 java -cp /Users/Shared/fucina-work/task-9/workspace/target/classes Foo
 4242 /Applications/Safari.app/Contents/MacOS/Safari
";
        let found = groups_referencing(ps, "/Users/Shared/fucina-work");
        assert_eq!(found, HashSet::from([2350]));
        // A recycled pgid running something unrelated is never signalled.
        assert!(!found.contains(&4242));
        assert!(!found.contains(&501));
    }

    #[test]
    fn a_recycled_pgid_without_a_live_match_is_left_alone() {
        let found = groups_referencing("  777 /usr/bin/ssh-agent\n", "/Users/Shared/fucina-work");
        assert!(found.is_empty());
    }

    #[test]
    fn never_targets_the_callers_own_group() {
        // 0 means "my own group" to killpg — the daemon and every step it
        // supervises. 1 and negatives are equally not ours to signal.
        assert!(!usable(0));
        assert!(!usable(1));
        assert!(!usable(-42));
        assert!(usable(2));
    }

    #[tokio::test]
    async fn reaping_an_absent_group_is_a_no_op() {
        assert!(!reap_group(0, Duration::from_millis(10)).await);
        assert!(!group_alive(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reaps_a_whole_tree_including_grandchildren() {
        use std::process::Stdio;
        // A shell that forks a sleep and waits on it: killing only the shell
        // would leave the sleep behind, which is the bug this module exists
        // for. The marker file lets us see the grandchild really started.
        let marker = std::env::temp_dir().join(format!("fucina-pg-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!(
            "( : > {m}; exec sleep 300 ) & sleep 300",
            m = marker.display()
        );
        let mut child = tokio::process::Command::new("bash")
            .args(["-c", &script])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let pgid = child.id().expect("pid") as i32;

        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        assert!(marker.exists(), "grandchild never started");
        assert!(group_alive(pgid));

        assert!(reap_group(pgid, Duration::from_millis(300)).await);
        let _ = child.wait().await;
        // The leader is reaped; nothing in the group may survive it.
        assert!(!group_alive(pgid), "process group outlived reap_group");
        let _ = std::fs::remove_file(&marker);
    }
}
