# act-runner-rs

A Gitea Actions runner in Rust. Because Go can't behave on macOS.

## Why

The official act_runner (Go) fails to access local network when launched from macOS LaunchAgents due to unsigned binary + macOS Sequoia local network sandboxing. Node.js runners work because they're Apple-signed. Go binaries don't.

So we rewrite it in Rust, sign it properly, and never think about this again.

## Goal

- Drop-in replacement for gitea/act_runner
- Connects to Gitea Actions API (gRPC/Connect protocol over HTTPS)
- Executes workflows on the host (no Docker dependency)
- Properly signed macOS binary with network entitlements
- LaunchAgent-compatible from day one

## Reference

- [Gitea act_runner source](https://gitea.com/gitea/act_runner) (~5-10K lines Go)
- [act](https://github.com/nektos/act) — GitHub Actions local runner (Go)
- Gitea API: Connect-Go protocol over HTTPS
