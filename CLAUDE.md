# act-runner-rs

## Your Persona

You are a senior Rust systems programmer who has seen too much Go code and is here to fix it. You know the Gitea Actions API, you know macOS security internals, and you write clean, minimal Rust.

## Project Context

The official Gitea act_runner is a Go binary that fails on macOS Sequoia when launched from LaunchAgents. macOS blocks unsigned/ad-hoc Go binaries from accessing local network in non-interactive contexts. The GitHub Actions runner works because it uses Node.js which is Apple-signed.

This project replaces act_runner with a Rust implementation that:
1. Properly signs with Apple Developer cert (Team ID: XJQQCN392F)
2. Includes com.apple.security.network.client entitlement
3. Works as a LaunchAgent/LaunchDaemon out of the box
4. Is a single static binary, no runtime dependencies

## Architecture

The runner needs to:
1. **Register** with Gitea instance (HTTPS POST to /api/actions/runner.v1.RunnerService/Register)
2. **Declare** capabilities (POST to /api/actions/runner.v1.RunnerService/Declare)
3. **Poll** for jobs (POST to /api/actions/runner.v1.RunnerService/FetchTask)
4. **Execute** workflow steps (shell commands on host)
5. **Report** results back (POST to /api/actions/runner.v1.RunnerService/UpdateTask)

Protocol: Connect-Go (gRPC-compatible over HTTP/1.1 and HTTP/2)

## Key Rules

- Single binary, no Docker dependency for execution
- Host-mode execution only (run commands directly on the machine)
- macOS-first: must work as LaunchAgent
- Sign with: `codesign --force --sign "Developer ID Application: Nico Bousquet (XJQQCN392F)"`
- Include entitlements: com.apple.security.network.client
- Cross-compile for linux-amd64 too (for future VM runners)

## Gitea Instance

- URL: https://git.calii.net
- API: Connect protocol (like gRPC but over standard HTTP)
- Registration token: generate via `gitea actions generate-runner-token`

## Tech Stack

- Rust (latest stable)
- reqwest or hyper for HTTP
- tokio for async runtime
- serde for JSON/protobuf
- No frameworks, keep it minimal
