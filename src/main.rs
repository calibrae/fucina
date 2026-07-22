mod client;
mod config;
mod expr;
mod poller;
mod proto;
mod reporter;
mod runner;
mod taskstate;

#[cfg(target_os = "macos")]
mod macos_menu;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "fucina", about = "Gitea Actions runner (Rust)")]
struct Cli {
    /// Path to config file. When not given, defaults to
    /// `~/gitea-runner-rs/config.yaml` so the `.app` bundle can be
    /// double-clicked from Finder without args.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Subcommand. When omitted, defaults to `daemon` (Finder double-click UX).
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Register this runner with a Gitea instance
    Register {
        /// Registration token (from Gitea admin)
        #[arg(long)]
        token: String,
        /// Runner name (defaults to hostname)
        #[arg(long)]
        name: Option<String>,
        /// Runner labels (overrides config)
        #[arg(long)]
        labels: Option<Vec<String>>,
    },
    /// Start the runner daemon
    Daemon {
        /// Run the plain daemon loop without the macOS menu-bar host.
        /// Used by the SMAppService LaunchDaemon (no GUI session, no NSApp).
        #[arg(long)]
        headless: bool,
    },
}

fn default_config_path() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join("gitea-runner-rs/config.yaml");
        if p.exists() {
            return p;
        }
    }
    // Root LaunchDaemon (SMAppService) context: launchd sets no usable $HOME.
    // Fall back to the per-user config scaffold the pkg postinstall created —
    // first match in sorted order wins (single-admin machines have one).
    if let Ok(entries) = std::fs::read_dir("/Users") {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("gitea-runner-rs/config.yaml"))
            .filter(|p| p.exists())
            .collect();
        candidates.sort();
        if let Some(p) = candidates.into_iter().next() {
            return p;
        }
    }
    PathBuf::from("config.yaml")
}

/// `/Users/<u>/…/config.yaml` → `/Users/<u>`. Used to give a root daemon a
/// meaningful $HOME (workflow tools — npm, cargo — need one).
fn home_from_config_path(config: &std::path::Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut comps = config.components();
    if comps.next() != Some(Component::RootDir) {
        return None;
    }
    match comps.next()? {
        Component::Normal(c) if c == "Users" => {}
        _ => return None,
    }
    match comps.next()? {
        Component::Normal(user) => Some(PathBuf::from("/Users").join(user)),
        _ => None,
    }
}

/// macOS users will want `Open Log` in the menu bar to open a readable file.
/// Route tracing to `~/Library/Logs/Fucina/fucina.log` in addition to stderr.
pub fn log_file_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join("Library/Logs/Fucina/fucina.log"))
}

fn main() -> Result<()> {
    // Finder/LaunchServices may pass a -psn_X_Y process-serial-number arg
    // when launching .app bundles. Strip it before clap sees argv.
    let args: Vec<String> = std::env::args()
        .filter(|a| !a.starts_with("-psn_"))
        .collect();
    let cli = Cli::parse_from(args);

    let config_path = cli.config.unwrap_or_else(default_config_path);

    // Root LaunchDaemons start with no $HOME (or /var/root). Derive one from
    // the config's /Users/<u>/ prefix — before logging setup, which writes to
    // $HOME/Library/Logs — so the daemon logs and workflow tools behave like
    // the hand-rolled plists that exported HOME explicitly. An explicitly
    // exported non-root HOME always wins.
    if let Some(home) = home_from_config_path(&config_path) {
        match std::env::var("HOME") {
            Ok(h) if !h.is_empty() && h != "/var/root" => {}
            _ => std::env::set_var("HOME", &home),
        }
    }

    // Set up a file appender so the menu-bar Open Log item has something real
    // to open. Leak the guard so logs keep flushing for the program's lifetime.
    let file_guard = if let Some(path) = log_file_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "fucina.log".into());
        let dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let appender = tracing_appender::rolling::never(dir, file_name);
        let (nb, guard) = tracing_appender::non_blocking(appender);
        let filter = tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("fucina=debug".parse().unwrap());
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(nb)
            .with_ansi(false)
            .init();
        Some(guard)
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env()
                    .add_directive("fucina=debug".parse().unwrap()),
            )
            .init();
        None
    };
    // Keep the guard alive for the life of the process
    std::mem::forget(file_guard);

    let config = config::Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    // Default to daemon if no subcommand (Finder double-click launch).
    let command = cli.command.unwrap_or(Commands::Daemon { headless: false });

    match command {
        Commands::Register {
            token,
            name,
            labels,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cmd_register(
                &config,
                &token,
                name.as_deref(),
                labels.as_deref(),
            ))
        }
        Commands::Daemon { headless } => {
            // macOS GUI session: main thread must host NSApplication for
            // LaunchServices registration + Local Network Privacy prompts. The
            // tokio runtime and poller run on a worker thread inside
            // macos_menu::run. Headless (SMAppService root LaunchDaemon, no
            // GUI session) skips NSApp entirely — Tahoe daemons need neither
            // the prompt nor the menu bar.
            #[cfg(target_os = "macos")]
            if !headless {
                return macos_menu::run(config);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = headless;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cmd_daemon(&config))
        }
    }
}

async fn cmd_register(
    config: &config::Config,
    reg_token: &str,
    name: Option<&str>,
    labels: Option<&[String]>,
) -> Result<()> {
    let runner_name = name.unwrap_or(&config.name);
    let runner_labels = labels.unwrap_or(&config.labels);

    info!(
        "registering runner '{}' with {}",
        runner_name, config.instance
    );

    let client = client::ConnectClient::new(&config.api_base())?;
    let runner = client
        .register(runner_name, reg_token, runner_labels)
        .await?;

    info!("registered: id={} uuid={}", runner.id, runner.uuid);

    let creds = config::Credentials {
        uuid: runner.uuid,
        token: runner.token,
        name: runner.name,
    };
    creds.save(&config.runner_file)?;

    info!("credentials saved to {}", config.runner_file.display());
    Ok(())
}

async fn cmd_daemon(config: &config::Config) -> Result<()> {
    // When running inside the macOS menu-bar host, shutdown signals come
    // from NSApplication::terminate via the worker thread. Otherwise set
    // up SIGINT/SIGTERM handlers ourselves.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tx = shutdown_tx.clone();
    tokio::spawn(async move {
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to register SIGINT");
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM");
        tokio::select! {
            _ = sigint.recv() => info!("received SIGINT"),
            _ = sigterm.recv() => info!("received SIGTERM"),
        }
        let _ = tx.send(true);
    });

    spawn_bundle_version_watcher(shutdown_tx.clone());

    run_daemon(config.clone(), shutdown_rx).await
}

/// Path to the enclosing app bundle's Info.plist, when running from one
/// (…/Fucina.app/Contents/MacOS/fucina → …/Fucina.app/Contents/Info.plist).
fn bundle_info_plist() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    (macos_dir.file_name()? == "MacOS" && contents.file_name()? == "Contents")
        .then(|| contents.join("Info.plist"))
}

/// Extract `CFBundleShortVersionString` from an XML Info.plist. The bundle's
/// plist is written from a sed'd template, so it is always XML — no need for
/// a binary-plist parser dependency.
fn plist_version(xml: &str) -> Option<String> {
    let key_at = xml.find("<key>CFBundleShortVersionString</key>")?;
    let rest = &xml[key_at..];
    let start = rest.find("<string>")? + "<string>".len();
    let rest = &rest[start..];
    let end = rest.find("</string>")?;
    Some(rest[..end].trim().to_string())
}

/// Zero-touch updates: when running from Fucina.app, watch the bundle's
/// Info.plist. A self-update pkg (or manual reinstall) replaces the bundle
/// in place; once the version on disk differs from the running one, trigger
/// a graceful shutdown — the poller drains in-flight jobs, the process exits,
/// and launchd's KeepAlive restarts it into the new binary. No sudo, ever.
fn spawn_bundle_version_watcher(tx: tokio::sync::watch::Sender<bool>) {
    let Some(plist) = bundle_info_plist() else {
        return;
    };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let Ok(xml) = tokio::fs::read_to_string(&plist).await else {
                continue;
            };
            if let Some(v) = plist_version(&xml) {
                if v != env!("CARGO_PKG_VERSION") {
                    info!(
                        "bundle updated to {} (running {}) — draining jobs, restarting via launchd",
                        v,
                        env!("CARGO_PKG_VERSION")
                    );
                    let _ = tx.send(true);
                    return;
                }
            }
        }
    });
}

/// Core daemon loop without signal-handling wiring — used both by the plain
/// CLI path and by the macOS menu-bar host (which drives shutdown via NSApp).
pub async fn run_daemon(
    config: config::Config,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let creds = config::Credentials::load(&config.runner_file).with_context(|| {
        format!(
            "no runner credentials at {} — run 'register' first",
            config.runner_file.display()
        )
    })?;

    info!(
        "starting daemon: runner='{}' instance={}",
        creds.name, config.instance
    );

    let client = Arc::new(
        client::ConnectClient::new(&config.api_base())?
            .with_credentials(creds.uuid.clone(), creds.token.clone()),
    );

    // Retry Declare until it succeeds or we're told to shut down. On macOS
    // this also gives Local Network Privacy time to surface its prompt —
    // short-lived processes get silently denied.
    let runner = loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }
        match client.declare(&config.labels).await {
            Ok(r) => break r,
            Err(e) => {
                warn!("declare failed: {:#} — retrying in 10s", e);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { return Ok(()); }
                    }
                }
            }
        }
    };
    info!("declared: id={} labels={:?}", runner.id, runner.labels);

    tokio::fs::create_dir_all(&config.work_dir).await?;

    // Cancel any tasks that were in-flight when the previous process died.
    // Without this, Gitea keeps them as "running" and eventually stops
    // offering new work to this runner (stale task accumulation).
    let task_state = Arc::new(taskstate::TaskStateFile::alongside(&config.runner_file));
    let stale_ids = task_state.drain_stale();
    if !stale_ids.is_empty() {
        warn!(
            "found {} stale in-flight task(s) from previous run: {:?} — reporting as FAILURE",
            stale_ids.len(),
            stale_ids
        );
        for id in stale_ids {
            let state = proto::TaskState {
                id,
                result: proto::TaskResult::Failure,
                started_at: None,
                stopped_at: Some(proto::Timestamp::now()),
                steps: vec![],
            };
            match client
                .update_task(state, std::collections::HashMap::new())
                .await
            {
                Ok(_) => info!("cancelled stale task {}", id),
                Err(e) => warn!("failed to cancel stale task {}: {:#}", id, e),
            }
        }
    }

    let mut poller = poller::Poller::new(
        client,
        config.capacity,
        config.fetch_interval,
        config.work_dir.clone(),
        config.run_as.clone(),
        task_state,
    );
    if let Some(user) = &config.run_as {
        info!("workflow steps will run as user '{}' via sudo", user);
    }

    if let Err(e) = poller.run(shutdown_rx).await {
        error!("poller error: {:#}", e);
        bail!("poller exited with error");
    }

    info!("daemon stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_version_extracts() {
        let xml = r#"<dict>
    <key>CFBundleVersion</key>
    <string>9.9.9</string>
    <key>CFBundleShortVersionString</key>
    <string>0.3.0</string>
</dict>"#;
        assert_eq!(plist_version(xml).as_deref(), Some("0.3.0"));
    }

    #[test]
    fn plist_version_missing_key() {
        assert_eq!(plist_version("<dict></dict>"), None);
        assert_eq!(plist_version(""), None);
    }

    #[test]
    fn plist_version_key_order_independent() {
        // must read the string that FOLLOWS the short-version key, not the
        // first <string> in the file
        let xml = "<key>CFBundleVersion</key><string>1.1.1</string>\
                   <key>CFBundleShortVersionString</key><string>2.2.2</string>";
        assert_eq!(plist_version(xml).as_deref(), Some("2.2.2"));
    }

    #[test]
    fn home_from_config_under_users() {
        assert_eq!(
            home_from_config_path(std::path::Path::new(
                "/Users/cali/gitea-runner-rs/config.yaml"
            )),
            Some(PathBuf::from("/Users/cali"))
        );
    }

    #[test]
    fn home_from_config_elsewhere_is_none() {
        for p in ["/etc/fucina/config.yaml", "config.yaml", "/Users"] {
            assert_eq!(home_from_config_path(std::path::Path::new(p)), None);
        }
    }
}
