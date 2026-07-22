# Field notes — the giorno double-instance and the private-repo checkout failures

*Written after the first real production pipeline ran on fucina (capucine CI, 2026-07-22).
Everything below was observed live, not theorized.*

## TL;DR

1. **giorno was running TWO fucina instances at once** — a stale root LaunchDaemon binary
   (built May 29) and a newer `Fucina.app` — so identical jobs randomly passed or failed
   depending on which instance grabbed the task.
2. **The checkout step cannot authenticate**: it does a bare anonymous HTTPS `git clone`.
   On a private repo it only works where ambient git credentials happen to exist
   (speedwagon's user keychain) and fails everywhere else. Resolution for the LAN Gitea:
   make the repo public. Proper fix: token support in `execute_checkout`.
3. Bonus gap found the same day: workflow-level `env:` is not applied to run steps.

---

## 1. The double instance on giorno

### Symptom

Same workflow, same commit, same label (`macos-arm64`): the `front-terrain` job succeeded
while `front-bureau` failed with `npm ci` printing its own help text and exiting 1.

The npm debug logs on giorno told the real story via their `cwd` lines:

```
task-809 (failed):   cwd /Users/cali/gitea-runner-rs/work/task-809/frontend
task-810 (passed):   cwd /Users/cali/gitea-runner-rs/work/task-810/workspace/terrain
```

`task-809` ran the step in `<job_dir>/frontend` — **without the `workspace/` segment** —
an empty directory with no `package-lock.json`, hence npm's usage dump. `task-810` used
the correct `workspace/`-relative path. Same machine, two different path-join behaviours.

### Cause

Two runners were live simultaneously on giorno:

| Instance | Binary | Built | Behaviour |
|---|---|---|---|
| LaunchDaemon `net.calii.fucina` (root, pid 846) | `/usr/local/bin/fucina` | **May 29** | joins `working-directory` onto the *task* dir → broken |
| `Fucina.app` (pid 1467) | app bundle | newer | correct `workspace.join(...)` |

The May 29 binary predates the current `src/runner.rs` logic, which is already correct:

```rust
// Match GitHub/Gitea Actions semantics: steps run from the checked-out
// repo (workspace), not from job_dir itself.
let work = working_directory
    .map(|d| workspace.join(d))
    .unwrap_or_else(|| workspace.to_path_buf());
```

So this was a **binary deployment problem, not a source bug** — plus an operational one:
two registered runners polling the same Gitea with the same labels is a lottery.

### Aggravating factor: logging has been dead since May 16

The LaunchDaemon plist points `StandardOutPath`/`StandardErrorPath` at
`/Users/cali/gitea-runner-rs/runner.log`, but that file's last write is May 16
(a SIGTERM shutdown of the *old user-level* agent). The root daemon has produced no
readable log since. Diagnosis had to go through:

- Gitea API: `GET /api/v1/repos/{owner}/{repo}/actions/runs/{id}/jobs` →
  `GET /api/v1/repos/{owner}/{repo}/actions/jobs/{job_id}/logs`
- npm's own debug logs (`~/.npm/_logs/*-debug-0.log`, `verbose cwd` lines) on the runner host

### Remediation checklist for giorno

1. Rebuild fucina from current source and sign it:
   `codesign --force --options runtime --sign "Developer ID Application: Nico Bousquet (XJQQCN392F)" --identifier "com.gitea.fucina" --entitlements entitlements.plist target/release/fucina`
2. Replace `/usr/local/bin/fucina`, restart the LaunchDaemon.
3. **Kill the duplicate**: exactly one runner instance per machine. Decide whether the
   LaunchDaemon or the app bundle is the canonical one, remove the other from launch.
4. Fix the log redirection (or give fucina its own file logger so launchd redirection
   stops being a single point of diagnosis failure).
5. Only then un-pin consumer workflows from `runs-on: speedwagon` back to `macos-arm64`.

---

## 2. The private-repo checkout fuckery

### Symptom

On a **private** Gitea repo, every giorno job died at checkout:

```
could not read Username for 'https://git.calii.net': terminal prompts disabled
```

while speedwagon jobs checked out fine — *not because fucina did anything right*, but
because its LaunchAgent runs in cali's GUI session, whose keychain happens to hold a
cached HTTPS credential for git.calii.net. Root on giorno has no keychain, no credential,
no luck.

Passing `with: token: ${{ secrets.GITHUB_TOKEN }}` to `actions/checkout@v4` changes
nothing: fucina's reimplemented checkout (`execute_checkout`) performs a bare
`git clone https://…` and reads **no token, no credential, no `with:` input**.

### Resolution chosen (2026-07-22)

The Gitea instance is LAN-only, so the repo was simply made **public**
(`PATCH /api/v1/repos/{owner}/{repo} {"private": false}`). Anonymous clone then works
identically on every runner, and the workflows carry no tokens at all.

This is a policy decision that fits a private LAN forge. It is **not** a substitute for
real auth support the day a repo must stay private.

### Proper fix (v2 candidate)

`execute_checkout` should authenticate the clone, in order of preference:

1. Use the job's `GITEA/GITHUB_TOKEN` provided in the task context (Gitea issues one per
   job) — inject it into the clone URL (`https://x-access-token:<token>@host/owner/repo`)
   or via an ephemeral `GIT_ASKPASS` helper; never write it to disk.
2. Honor an explicit `with: token:` on the checkout step as an override.
3. Fall back to the current anonymous clone.

## 3. Bonus gap: workflow-level `env:` is ignored by run steps

A workflow-wide `env: PATH: /opt/homebrew/bin:…` never reached the step shells
(`npm: command not found` on giorno despite `/opt/homebrew/bin/npm` existing). Workflows
currently work around it by exporting PATH inline in every step. `build_env`/step-env
composition should merge workflow- and job-level `env:` maps into each step's
environment.

**Correction**: this was already fixed in source before these notes were written —
commit `7150aaf` folds workflow- and job-level `env:` into every step. What giorno was
running was the May 29 binary, which predates it. Deploying current HEAD *is* the fix.

---

## Remediation status (2026-07-22, same day)

- **Checkout auth (§2 proper fix)**: implemented in `8ccca8f`, released as **v0.2.9**.
  Token resolution: `with: token:` → `GITHUB_TOKEN` → `GITEA_TOKEN` → anonymous.
  Injection via inline credential helper reading the child env — never argv, never disk.
  Ambient helpers are cleared, so the speedwagon-keychain accident can't mask breakage.
- **speedwagon-rs**: v0.2.9 deployed (binary swap + LaunchAgent bootstrap), declared and
  polling.
- **giorno duplicate (§1)**: Fucina.app instance (pid 1467) killed, "Fucina" login item
  removed. Only the root LaunchDaemon remains. v0.2.9 binary staged at
  `/tmp/fucina-0.2.9` (sha256 verified) — root swap + `launchctl kickstart` pending.
- **giorno logging (§1)**: fucina ≥0.2.9 writes its own log to
  `$HOME/Library/Logs/Fucina/fucina.log` (the daemon plist sets `HOME=/Users/cali`),
  ending the dependence on launchd redirection.
- **Un-pinning capucine** from `runs-on: speedwagon`: pending giorno verification.

## Post-deploy note (2026-07-22, evening) — v0.2.9 fallout on speedwagon

First real run after the v0.2.9 rollout: backend job failed at checkout on speedwagon with
`fatal: detected dubious ownership in repository at …/work/task-N/workspace`. The clone
succeeds, but the follow-up git invocation runs with the cleaned environment introduced by
the credential-helper work — and without the ambient config, git's ownership check goes
strict. giorno was unaffected. Unblocked host-side with
`git config --global --add safe.directory '*'` for the runner user (standard practice for
self-hosted runners). Proper fucina fix: pass `-c safe.directory=<workspace>` (or
`GIT_CONFIG_COUNT` equivalents) on every git invocation of `execute_checkout`, so the
runner never depends on host git config. Verified green cluster-wide afterwards
(run 561: backend on speedwagon-rs, both fronts on giorno-rs).
