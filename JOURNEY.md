# The act-runner-rs Journey

## How a "quick CI setup" became a 2-day deep dive into macOS internals

### The Goal
Install a Gitea Actions runner on speedwagon (M1 Mac Mini) to get on-premise CI/CD. Code stays on the LAN, builds on our hardware, zero cloud.

### Chapter 1: The Go Runner

Downloaded the official `act_runner` (Go binary, 19MB). Registered fine. Ran perfectly in the foreground. Then tried to run it as a LaunchAgent.

```
No route to host (os error 65)
```

Every. Single. Time.

### Chapter 2: The Debugging Spiral

What we tried:
- **LaunchAgent** → no route to host
- **LaunchDaemon** → no route to host  
- **nohup** → no route to host
- **background with &** → works (???)
- **tmux** → no route to host
- **script -q /dev/null** (fake PTY) → no route to host
- **Full PATH in .zshenv** → no route to host
- **SessionCreate + ProcessType Interactive** → no route to host
- **Wrapping in login -f** → no route to host
- **Wrapping in zsh -l** → no route to host

The ONLY thing that worked: typing the command directly in Terminal.app in the foreground, then backgrounding with `&` and `disown`.

### Chapter 3: The Signing Theory

Discovered the Go binary was signed as `Identifier=a.out` with no Team ID. Meanwhile, GitHub's Node.js runner (which works as a LaunchAgent) is signed by Apple with Team ID `HX7739G8FX`.

Re-signed with Cali's Developer ID cert (XJQQCN392F):
```bash
codesign --force --sign "Developer ID Application: Nico Bousquet (XJQQCN392F)" \
  --identifier "com.gitea.act-runner" /usr/local/bin/act_runner
```

Still failed as LaunchAgent.

### Chapter 4: The TCC Rabbit Hole

Explored macOS TCC (Transparency, Consent, and Control):
- Manually inserted entries in `/Library/Application Support/com.apple.TCC/TCC.db`
- Generated and matched `csreq` blobs
- Wiped and re-created entries
- The entries were correct, csreq matched, auth_value=2 (allowed)

**TCC said yes. macOS said no.**

### Chapter 5: The Rewrite

"How many lines of Go?" → 2,672.

Spawned a Claude agent. Told it to yolo the implementation. 30 minutes later: **1,564 lines of Rust**, 48 tests passing, proper code signing with entitlements.

Binary size: 3.9MB (vs 19MB Go).

### Chapter 6: Same Problem, Different Language

The Rust binary had the exact same issue. Works interactively, fails as LaunchAgent. This ruled out Go-specific issues entirely.

### Chapter 7: The Label Bug

While debugging, discovered the runner was sending labels as `"ubuntu-latest:host"` to Gitea. Gitea stores them as-is, but jobs request `runs-on: ubuntu-latest` (no `:host`). Exact string match fails. 

Fix: strip the `:host` suffix before sending to Gitea's Declare/Register endpoints.

### Chapter 8: The Timestamp Bug  

Runner picked up a job! But UpdateTask returned 400. Protobuf JSON encoding requires timestamps as RFC 3339 strings (`"2026-04-04T15:00:00.000000000Z"`), not objects (`{"seconds": N, "nanos": N}`).

Fixed. **First successful CI job: RESULT_SUCCESS.**

### Chapter 9: The Real Answer

Research confirmed: **this is a known macOS Sequoia bug.** Michael Tsai documented it — "third-party binaries running under a launchd agent are denied local network access despite approving the privacy prompt."

- Apple knows about it
- macOS 15.3 supposedly fixed permission persistence after reboot — but LaunchAgent context is STILL broken on 15.6.1
- The System Settings toggle shows "allowed" but macOS blocks it anyway from LaunchAgent/Daemon context
- Every developer who hits this either runs from Terminal, uses Docker, or downgrades

### Chapter 10: Tahoe Fixes It

Tested on frankenmac (macOS 26.3.1 Tahoe):
- LaunchDaemon with the Rust binary → **WORKS**
- Declares, polls, fetches tasks — no network errors
- The TCC toggle just needs a restart cycle to propagate

**LaunchDaemon on Tahoe is the real fix.** Upgrade speedwagon to Tahoe and the runner runs as a proper daemon.

### Current State

- **Speedwagon (Sequoia 15.6.1)**: Terminal.app LaunchAgent workaround — LaunchAgent spawns Terminal.app via osascript, which runs the daemon in a real TTY context. Window auto-minimizes.
- **Frankenmac (Tahoe 26.3.1)**: LaunchDaemon works natively.
- **Plan**: Upgrade speedwagon to Tahoe, switch to LaunchDaemon.

### Interim Workaround (Sequoia)

The Terminal.app approach:
```bash
# ~/gitea-runner-rs/run.sh
osascript -e '
tell application "Terminal"
    do script "cd ~/gitea-runner-rs && exec /usr/local/bin/act-runner-rs --config config.yaml daemon 2>&1 | tee ~/gitea-runner-rs/runner.log"
    delay 2
    set miniaturized of front window to true
end tell'
```

LaunchAgent fires → Terminal opens → runs the daemon → window minimizes → runner lives.

### What We Learned

1. macOS Sequoia's local network permission is broken for non-Apple binaries in LaunchAgent/Daemon context
2. TCC entries, code signing, entitlements, app bundles — none of it matters on Sequoia
3. The permission toggle in System Settings is a lie for daemon context
4. Tahoe (macOS 26) actually fixes it for LaunchDaemons
5. Go binaries ship with `Identifier=a.out` and no team ID — sign everything properly
6. Gitea's Connect protocol uses protobuf JSON encoding — timestamps are strings, int64 are strings
7. Gitea label matching is exact string — don't send the `:host` execution scheme to the server
8. When in doubt, rewrite it in Rust (3.9MB vs 19MB, 1564 vs 2672 lines)

### The Stack

```
Gitea (git.calii.net, 10.10.0.11)
  ↓ Actions API (Connect/gRPC over HTTPS)
act-runner-rs (speedwagon, 10.10.0.2)
  ↓ Host execution (shell commands)
speedwagon (M1 Mac Mini, cargo/rustc/homebrew)
  ↓ Results + logs
Gitea Actions UI (green checkmark)
```

All on-premise. Zero cloud. Zero Go. Zero Node. 1564 lines of Rust.

🦀
