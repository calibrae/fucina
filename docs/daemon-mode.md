# Daemon mode (v0.3) — SMAppService, zero-touch updates

v0.3.0 replaces the hand-rolled `/Library/LaunchDaemons` + ssh + sudo deployment
dance with the Apple-blessed flow: the app bundle carries its own daemon
definition and macOS manages the rest.

## How it works

- `Fucina.app/Contents/Library/LaunchDaemons/net.calii.fucina.daemon.plist` is
  embedded in the bundle (and covered by its code signature). `BundleProgram`
  points at `Contents/MacOS/fucina`, args are `daemon --headless`.
- The menu-bar item **System Daemon…** registers/unregisters it via
  `SMAppService.daemonServiceWithPlistName:` (macOS 13+). First install needs
  one approval: System Settings → General → Login Items & Extensions → *Allow
  in the Background* → Fucina (admin auth). The menu offers to open that pane.
- The daemon runs as **root**, in the LaunchDaemons domain — which on Tahoe is
  exactly the context where Local Network access just works (JOURNEY.md,
  chapter 10). No prompt roulette.
- `--headless` skips the NSApplication/menu-bar host entirely: no NSApp as
  root, no Bonjour prompt trigger, just the tokio daemon with SIGTERM/SIGINT
  handling.

## Single instance, guaranteed

Installing the daemon removes the "Fucina" login item and stops the in-app
runner (graceful drain). While the daemon is enabled, the menu app starts in
**controller mode** — status line shows "(daemon mode)", no worker is spawned,
and "Launch at Login" refuses to enable. This makes the giorno double-instance
class of failure (see the 2026-07-22 field notes) structurally impossible.

## Zero-touch updates

The headless daemon watches the bundle's `Info.plist` every 30 s. When a pkg
self-update (menu → Check for Updates) or manual reinstall bumps the version,
the daemon logs it, drains in-flight jobs, and exits cleanly — launchd's
`KeepAlive` relaunches it into the new binary. Nobody ssh'es, nobody sudos.

## Config and $HOME resolution (root context)

The embedded plist contains **no user paths**. A root daemon has no usable
`$HOME`, so fucina resolves things itself:

1. Config: `$HOME/gitea-runner-rs/config.yaml` if `$HOME` is set and the file
   exists; otherwise the first (sorted) match of
   `/Users/*/gitea-runner-rs/config.yaml` — the scaffold the pkg postinstall
   creates; otherwise `./config.yaml`.
2. `$HOME`: if unset (or `/var/root`) and the config lives under
   `/Users/<u>/…`, fucina sets `HOME=/Users/<u>` before logging setup — so the
   daemon log lands in `/Users/<u>/Library/Logs/Fucina/fucina.log` and
   workflow tools (npm, cargo) find their caches. An explicitly exported
   non-root `HOME` always wins.

Jobs run as root unless `run_as: <user>` is set in config.yaml — set it on
machines where that matters (the `sudo -u` + chown plumbing has existed since
the giorno root daemon).

## Migrating a hand-rolled daemon (giorno, speedwagon)

```bash
# 1. Remove the old hand-rolled daemon (as admin, on the machine):
sudo launchctl bootout system/net.calii.fucina
sudo rm /Library/LaunchDaemons/net.calii.fucina.plist
sudo rm /usr/local/bin/fucina          # if it's a copied binary, not the pkg symlink

# 2. Install the v0.3 pkg (puts Fucina.app in /Applications + bin symlink).
# 3. Open Fucina.app → menu bar 🔨 → System Daemon… → Install.
# 4. Approve in System Settings when prompted.
```

The old label (`net.calii.fucina`) and the new one (`net.calii.fucina.daemon`)
are distinct on purpose — both can never collide, and a half-migrated machine
fails loudly (two pollers) rather than subtly.

For a speedwagon-style LaunchAgent setup, the equivalent cleanup is
`launchctl bootout gui/501/net.calii.fucina` +
`rm ~/Library/LaunchAgents/net.calii.fucina.plist`, then steps 2–4.
