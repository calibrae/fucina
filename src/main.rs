mod client;
mod config;
mod poller;
mod proto;
mod reporter;
mod runner;

#[cfg(target_os = "macos")]
mod macos_menu;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "fucina", about = "Gitea Actions runner (Rust)")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Commands,
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
    Daemon,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("fucina=debug".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    let config = config::Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;

    match cli.command {
        Commands::Register { token, name, labels } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cmd_register(
                &config,
                &token,
                name.as_deref(),
                labels.as_deref(),
            ))
        }
        Commands::Daemon => {
            // macOS: main thread must host NSApplication for LaunchServices
            // registration + Local Network Privacy prompts. The tokio runtime
            // and poller run on a worker thread inside macos_menu::run.
            #[cfg(target_os = "macos")]
            {
                macos_menu::run(config)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(cmd_daemon(&config))
            }
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

#[cfg(not(target_os = "macos"))]
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

    run_daemon(config.clone(), shutdown_rx).await
}

/// Core daemon loop without signal-handling wiring — used both by the plain
/// CLI path and by the macOS menu-bar host (which drives shutdown via NSApp).
pub async fn run_daemon(
    config: config::Config,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
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
        client::ConnectClient::new(&config.api_base())?.with_credentials(creds.uuid, creds.token),
    );

    let runner = client.declare(&config.labels).await?;
    info!("declared: id={} labels={:?}", runner.id, runner.labels);

    tokio::fs::create_dir_all(&config.work_dir).await?;

    let mut poller = poller::Poller::new(
        client,
        config.capacity,
        config.fetch_interval,
        config.work_dir.clone(),
        config.run_as.clone(),
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
