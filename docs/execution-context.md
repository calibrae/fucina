# What a job can and cannot reach

The most expensive class of CI bug on a macOS runner is not a broken command — it is a command
that works perfectly when you type it and fails in a job, with an error that never mentions
why. Almost always the answer is: **the job does not have your login session.**

## The two contexts

| | fucina as `gui/501` LaunchAgent | fucina as root LaunchDaemon (`run_as: <user>`) |
|---|---|---|
| Steps run as | the logged-in user, in their Aqua session | `<user>`, via `sudo -u`, **outside** any Aqua session |
| `SECURITYSESSIONID` | set | empty |
| Login keychain | usable *if unlocked* — i.e. if someone logged in since boot | **never usable** |
| GUI / TCC-gated APIs | available | unavailable |
| Survives logout / headless reboot | no | yes |
| Ambient credentials a malicious job could steal | the user's whole keychain | none |

The daemon column is deliberate: dropping ambient session privileges is the point of the
hardening (see `daemon-mode.md`). Losing login-keychain access is the cost, not a bug.

`sudo -u` is the key subtlety. It changes the uid; it does **not** join the target user's
session. So even `run_as: cali` on a machine where cali is logged in gives a step no Aqua
session and no login keychain.

## How to tell, instantly

fucina logs it at the start of every job:

```
job context: run_as=cali session=headless (steps CANNOT reach the login keychain)
```

and exports it to the steps:

- `FUCINA_SESSION` — `gui` or `headless`
- `FUCINA_RUN_AS` — the user steps are dropped to (absent when steps run as the daemon user)

So a build script can branch instead of failing:

```bash
if [ "$FUCINA_SESSION" = headless ]; then
  ensure_signing        # import certs into a throwaway keychain
fi
```

To confirm from inside a step:

```bash
echo "session=${SECURITYSESSIONID:-<none>} user=$(id -un)"
security find-identity -v -p codesigning    # what can I actually sign with?
```

An empty `SECURITYSESSIONID` means the login keychain will not work, full stop.

## Signing from a job (the pattern that always works)

Do not depend on the login keychain in CI, even where it happens to work — it only works while
someone stays logged in, and it breaks on the next headless reboot. Build a throwaway keychain
per job instead. `set-key-partition-list` is the step that actually makes the key usable
without a UI prompt; without it the import succeeds and the signing still fails.

```bash
KC="$RUNNER_TEMP/build.keychain-db"
KCPW="$(openssl rand -hex 16)"
security create-keychain -p "$KCPW" "$KC"
security set-keychain-settings -lut 21600 "$KC"
security unlock-keychain -p "$KCPW" "$KC"
security list-keychains -d user -s "$KC" $(security list-keychains -d user | sed 's/"//g')
security import cert.p12 -P "$P12PW" -A -t cert -f pkcs12 -k "$KC" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KCPW" "$KC"
```

Certs live in vault (`secret/infra/apple-cert-*`); the App Store Connect API key
(`secret/infra/apple-asc`) covers notarization and TestFlight upload without a keychain or 2FA.
fucina's own `.github/workflows/release.yml` does exactly this, and it is worth copying rather
than re-deriving.

Two traps that follow:

- **Never let Xcode mint certificates in CI** (`-allowProvisioningUpdates`,
  `CODE_SIGN_STYLE=Automatic`). It needs keychain write access it does not have, and it burns
  the team's limited certificate slots. Use `CODE_SIGN_STYLE=Manual` with an explicit
  `PROVISIONING_PROFILE_SPECIFIER`.
- **Stale provisioning profiles**: `xcodebuild` matches profiles by *name* from
  `~/Library/MobileDevice/Provisioning Profiles/`, so an old same-named file silently wins.
  Purge before installing the current pair.

## If a job genuinely needs the session: `FUCINA_SESSION: gui` (v0.5.0)

Some workloads cannot be untangled from the login session — Xcode automatic signing,
fastlane lanes without `setup_ci`, anything TCC-gated. For those, a job can opt in:

```yaml
# workflow- or job-level env
env:
  FUCINA_SESSION: gui
```

When granted, fucina wraps that job's steps in `launchctl asuser <uid> sudo -u <user> …`:
launchd adopts the console user's Mach bootstrap and security session before dropping
privilege, so steps see the real Aqua session — login keychain, ssh-agent, TCC — exactly as
if typed in Terminal.

The grant has three conditions, each refused loudly in the job log:

1. The runner's `config.yaml` sets **`allow_gui_session: true`** (default false — a repo
   must not be able to demand ambient credentials from a hardened runner).
2. The runner has a **`run_as`** user (the daemon user has no session to join).
3. The **console user is the `run_as` user** — someone is actually logged in as them
   (auto-login satisfies this across reboots).

`FUCINA_SESSION` as exported to the steps always states the **granted** reality, never the
request: a denied job sees `FUCINA_SESSION=headless` plus a `⚠ … DENIED: <reason>` line in
its log, so it can fall back to the throwaway keychain instead of failing mysteriously.

Reach for this deliberately. A gui-session job can read everything the console user can —
it is precisely the ambient exposure the daemon default removes. The throwaway keychain
remains the better answer for plain codesign/notarize flows: it needs nobody logged in and
works identically on every runner. The session hatch is for the flows that genuinely cannot.
