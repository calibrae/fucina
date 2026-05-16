# Failure Lockup Diagnosis — 2026-05-16

## Symptom
After `RESULT_FAILURE`, poller keeps calling FetchTask every 2s but Gitea returns `{tasksVersion: N}` with no task indefinitely. 43 queued `speedwagon` jobs stay unassigned. Kickstart (restart from 0) recovers immediately.

## Root Cause
`tasks_version` in-process state poisoning.

Gitea's FetchTask semantics: if `req.tasks_version >= server_tasks_version`, Gitea returns `{tasksVersion: N}` with no task (runner is considered "current"). Only when `req.tasks_version < server_tasks_version` does Gitea scan for and dispatch a pending task.

Flow that creates the lockup (`poller.rs:poll_once`):
1. Startup: `self.tasks_version = 0` → FetchTask sends 0 → Gitea sees `0 < 172` → assigns task NNN, returns `tasksVersion: 172`
2. Runner stores `self.tasks_version = 172` (line 82, unconditional, runs before checking `resp.task`)
3. Task NNN fails → permit dropped → semaphore freed → capacity restored
4. Next FetchTask sends `tasks_version: 172` → Gitea sees `172 == 172` → returns nothing
5. Loop forever, even with a full queue

Restart works because `Poller::new` initializes `tasks_version: 0`, and `0 < 172` causes Gitea to dispatch.

## Patch

`src/poller.rs` — after `tokio::spawn(...)`:

```rust
// Reset tasks_version so the next FetchTask sends 0 (< server's current version),
// causing Gitea to scan for pending tasks. Without this, after a task completes
// (success or failure), self.tasks_version == server's current version and Gitea
// returns no task indefinitely — even with a full queue of waiting jobs.
self.tasks_version = 0;
```

One line. Patch is in `main` branch as of this session.

## Build & Deploy

```bash
cd ~/Developer/perso/act-runner-rs
cargo build --release
codesign --force --options runtime \
  --sign "Developer ID Application: Nico Bousquet (XJQQCN392F)" \
  --identifier "com.gitea.act-runner-rs" \
  --entitlements entitlements.plist \
  target/release/fucina
cp target/release/fucina /tmp/fucina-new
sudo mv /tmp/fucina-new /usr/local/bin/fucina
launchctl kickstart -k gui/$(id -u)/net.calii.fucina
```

Binary is already built and signed at `target/release/fucina`. Only the sudo mv + kickstart remain.

## Validation
Run #127 on cali/scrytti has 43 queued speedwagon jobs. After kickstart, they should drain immediately and not pause between jobs on any future failure.
