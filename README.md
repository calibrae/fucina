# fucina

*fucina* — Italian for **forge**. A Gitea Actions runner in Rust.

A lean replacement for the official `act_runner` (Go), built to survive macOS quirks and ship as a single static binary. Currently running in production on Gitea at [git.calii.net](https://git.calii.net), executing real CI jobs on macOS ARM64.

> ### ⚠️ macOS deployment gotcha (Sequoia 15 + Tahoe 26)
>
> macOS **Local Network Privacy** silently blocks third-party binaries running as LaunchAgents from connecting to RFC1918 addresses (your LAN Gitea) — you get `EHOSTUNREACH` with no TCC prompt. `curl` works because it's Apple-signed; signing with your own Developer ID is **not** enough.
>
> **The fix, per Apple's DTS engineer:** run fucina as a **LaunchDaemon as root**. Daemons in the global login session are exempt from Local Network Privacy; user-session agents are not. Using `UserName` on a LaunchDaemon to run as a non-root user is an **unsupported mixed context** and fails the same way.
>
> See [macOS Setup](#macos-setup--use-a-launchdaemon-not-a-launchagent) below for the working plist.

## Why

The official `act_runner` (Go) fails to access local network when launched from macOS LaunchAgents. This turned out to be macOS's Local Network Privacy feature, not the Go runtime's fault — but the Go runner had other quirks on macOS anyway.

*fucina* fixes the deployment story by:
- Shipping as a single static binary with no runtime dependencies
- Signing with a Developer ID + `com.apple.security.network.client` entitlement (necessary even if not sufficient on its own)
- Running workflow steps directly on the host (no Docker)
- Documenting the LaunchDaemon-as-root requirement so you don't waste a day rediscovering it
- Being small enough to understand and fix when something breaks

## Quick Start

### Build

```bash
cargo build --release
```

### Sign (macOS only)

```bash
make sign
```

Uses `Developer ID Application` identity and embeds `com.apple.security.network.client` entitlement. The codesign identifier is `net.calii.fucina` — macOS TCC keys off this for local network permissions.

### Configure

Create `config.yaml`:

```yaml
instance: https://git.calii.net
name: my-mac-runner
labels:
  - self-hosted:host
  - macos-arm64:host
capacity: 1
fetch_interval: 2
timeout: 10800
work_dir: /tmp/fucina
```

Label format is `name:schema` — the `:host` suffix is stripped before being sent to Gitea. Gitea matches `runs-on:` against the label name only.

### Register

Generate a registration token from Gitea admin, then:

```bash
fucina -c config.yaml register --token <REGISTRATION_TOKEN>
```

Credentials are saved to `.runner` (path configurable).

### Run

```bash
fucina -c config.yaml daemon
```

The daemon declares capabilities, polls for jobs, executes them, and reports results back. SIGINT/SIGTERM trigger graceful shutdown (in-flight jobs finish before exit).

## Architecture

```
src/
  main.rs       CLI entry point (register, daemon)
  config.rs     YAML config + .runner credentials
  client.rs     Connect protocol HTTP client
  proto.rs      Request/response types (protobuf JSON)
  poller.rs     Async task polling loop
  runner.rs     Workflow YAML parsing + host execution
  reporter.rs   Log streaming + task state reporting
```

### Protocol

*fucina* talks to Gitea via the [Connect protocol](https://connectrpc.com/) — gRPC-compatible over plain HTTP/1.1+, JSON-encoded:

| RPC | Purpose |
|-----|---------|
| `Register` | One-time registration with auth token |
| `Declare` | Announce version + labels before polling |
| `FetchTask` | Poll for new workflow jobs |
| `UpdateTask` | Report job/step state |
| `UpdateLog` | Stream log lines back to Gitea |

Auth: `x-runner-uuid` + `x-runner-token` headers on every post-registration request.

### Protocol Gotchas (so you don't hit them)

Building this turned up three protobuf-JSON subtleties that caused silent failures:

1. **Labels**: strip the `:host` / `:docker://...` suffix before sending. Gitea does exact string match against `runs-on`.
2. **int64**: must be serialized as strings (`"42"`, not `42`). Protobuf JSON encoding rule.
3. **Timestamps**: `google.protobuf.Timestamp` must be RFC 3339 strings (`"2024-01-01T00:00:00Z"`), not `{seconds, nanos}` objects.

All three are documented in the code with custom serde implementations.

### Execution Model

Host-mode only. Each workflow step runs as a shell subprocess:

- `run:` steps execute via bash (default), sh, or python
- `uses: actions/checkout` is handled natively via `git clone`
- Other `uses:` actions are skipped with a warning
- Basic `if:` conditions supported: `always()`, `failure()`, `success()`, `cancelled()`

## Configuration Reference

| Field | Default | Description |
|-------|---------|-------------|
| `instance` | *(required)* | Gitea instance URL |
| `name` | hostname | Runner name |
| `labels` | `["self-hosted:host"]` | Runner capability labels |
| `capacity` | `1` | Max concurrent jobs |
| `fetch_interval` | `2` | Poll interval in seconds |
| `timeout` | `10800` | Job timeout in seconds (3h) |
| `work_dir` | `/tmp/fucina` | Working directory for jobs |
| `runner_file` | `.runner` | Path to credentials file |

## macOS Setup — use a LaunchDaemon, not a LaunchAgent

**Important**: on macOS Sequoia (15) and Tahoe (26), LaunchAgents are subject to Local Network Privacy. Non-Apple-signed binaries calling `connect()` to RFC1918 addresses (your LAN Gitea) silently get `EHOSTUNREACH` with no TCC prompt in headless context. Per Apple's DTS engineer ([forum thread](https://developer.apple.com/forums/thread/763753)): **daemons running as root are exempt; agents are subject**.

`UserName` on a LaunchDaemon to run as a non-root user is explicitly an **unsupported mixed context** and has the same problem.

### The working setup: LaunchDaemon as root

`/Library/LaunchDaemons/net.calii.fucina.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>net.calii.fucina</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/fucina</string>
        <string>--config</string>
        <string>/etc/fucina/config.yaml</string>
        <string>daemon</string>
    </array>
    <key>WorkingDirectory</key><string>/var/lib/fucina</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>/var/log/fucina.log</string>
    <key>StandardErrorPath</key><string>/var/log/fucina.log</string>
</dict>
</plist>
```

Install:

```bash
sudo cp net.calii.fucina.plist /Library/LaunchDaemons/
sudo chown root:wheel /Library/LaunchDaemons/net.calii.fucina.plist
sudo chmod 644 /Library/LaunchDaemons/net.calii.fucina.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/net.calii.fucina.plist
```

### LaunchAgent workaround (only if root daemon is not an option)

If you *must* run per-user in a logged-in GUI session, wrap fucina via osascript→Terminal.app. Terminal inherits the user session's implicit local-network grant. Ugly but works:

```bash
#!/bin/zsh
# ~/gitea-runner-rs/run.sh
osascript -e '
tell application "Terminal"
    do script "cd ~/gitea-runner-rs && exec ./fucina --config config.yaml daemon 2>&1 | tee runner.log"
    delay 2
    set miniaturized of front window to true
end tell'
```

LaunchAgent plist points `ProgramArguments` to `run.sh`.

## Debugging

```bash
RUST_LOG=fucina=debug fucina -c config.yaml daemon
```

Debug level logs every FetchTask response body (truncated) — which is how we found the three protocol bugs listed above.

## Supported

- Runner registration + credential persistence
- Capability declaration (labels, version)
- Job polling with configurable interval
- `run:` step execution (bash, sh, python)
- `actions/checkout` via native git clone
- Log streaming to Gitea
- Task/step state reporting with RFC 3339 timestamps
- `if:` conditions (`always()`, `failure()`, `success()`, `cancelled()`)
- Graceful shutdown (SIGINT/SIGTERM)
- Concurrent job execution with capacity limits

## Not Supported (yet)

- Expression evaluation (`${{ github.ref }}`)
- Arbitrary `uses:` actions (only checkout)
- Matrix strategies
- Artifacts / caching
- Service containers
- Composite actions

## Tests

```bash
cargo test
```

52 unit tests covering: Connect protocol serialization (int64 as string, RFC 3339 timestamps, camelCase, enum variants), config YAML parsing + defaults, credentials roundtrip, workflow YAML parsing, job/step extraction, env construction, `if:` condition evaluation.

## References

- [Gitea act_runner](https://gitea.com/gitea/act_runner) — the Go original
- [Actions proto definitions](https://gitea.com/gitea/actions-proto-def)
- [Connect protocol](https://connectrpc.com/)
- [Gitea Actions docs](https://docs.gitea.com/usage/actions/overview)
