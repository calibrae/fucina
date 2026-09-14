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

#[cfg(test)]
mod tests {
    use super::*;

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
