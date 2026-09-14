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

use std::time::Duration;
use tracing::debug;

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

#[cfg(test)]
mod tests {
    use super::*;

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
