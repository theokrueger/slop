use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use mmux::auth::{self, PamAuthenticator};
use mmux::config::{Config, DEFAULT_CONFIG_TOML};
use mmux::memory::{self, Memory, MemoryReadiness, cli as memcli};
use mmux::persistence;
use mmux::session::SessionStore;
use mmux::web::{self, AppState};
use mmux::workspace::Workspace;

#[derive(Debug, Parser)]
#[command(
    name = "mmux",
    version,
    about = "A persistent web terminal multiplexer for agents"
)]
struct Cli {
    /// Path to a TOML config file (default: ~/.config/mmux.toml).
    #[arg(long, env = "MMUX_CONFIG", value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    /// Override the configured listen address.
    #[arg(long, env = "MMUX_BIND", value_name = "ADDR", global = true)]
    bind: Option<SocketAddr>,

    /// Print the documented default configuration and exit.
    #[arg(long)]
    print_default_config: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect and edit the project memory vault.
    Mem(memcli::MemArgs),
    /// Run the memory MCP server on stdin/stdout (used by pi).
    Mcp,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.print_default_config {
        print!("{DEFAULT_CONFIG_TOML}");
        return Ok(());
    }

    // Configuration lives in the TOML file, so load it before installing the
    // subscriber: `[logging].filter` is the default filter. `RUST_LOG` still
    // wins for one-off debugging.
    let mut config = Config::load(cli.config.as_deref())?;
    init_tracing(&config.logging.filter);

    if let Some(bind) = cli.bind {
        config.server.bind = bind;
    }
    config.validate().map_err(anyhow::Error::msg)?;

    match cli.command {
        Some(Command::Mem(args)) => memcli::run(&config, args),
        Some(Command::Mcp) => memcli::run_mcp(&config),
        None => run(config, cli.config.as_deref()).await,
    }
}

async fn run(config: Config, config_path: Option<&Path>) -> anyhow::Result<()> {
    let workspace =
        Arc::new(Workspace::new(&config.workspace.root).with_context(|| {
            format!("invalid workspace root {}", config.workspace.root.display())
        })?);
    let user = auth::current_username().context("could not determine the current Unix user")?;
    let authenticator = Arc::new(
        PamAuthenticator::new(config.auth.pam_service.clone(), user.clone())
            .context("could not configure PAM")?,
    );
    let state_dir = persistence::default_state_dir();
    let sessions = Arc::new(
        match state_dir.as_ref() {
            Some(path) => SessionStore::with_state_dir(
                config.new_project.panes.clone(),
                (config.terminal.rows, config.terminal.cols),
                path.clone(),
            ),
            None => {
                tracing::warn!("could not determine a state directory; projects will not persist");
                SessionStore::new(
                    config.new_project.panes.clone(),
                    (config.terminal.rows, config.terminal.cols),
                )
            }
        }
        .with_appearance(config.terminal.appearance)
        .with_identity_env(identity_env(config_path)),
    );
    if let Some(path) = state_dir.as_ref() {
        tracing::info!(state = %path.display(), "project persistence enabled");
    }
    sessions.restore().await;

    // Memory is optional: when it is disabled or there is no state directory,
    // the rest of mmux works exactly as before.
    let memory = Memory::from_config(&config.memory, state_dir.as_deref()).map(Arc::new);
    let readiness = if memory.is_some() {
        memory::readiness::check(&config.memory)
    } else {
        MemoryReadiness::NotApplicable
    };
    if let Some(message) = readiness.message() {
        tracing::warn!("{message}");
        if let Some(advice) = readiness.advice() {
            tracing::info!("{advice}");
        }
    }

    let config = Arc::new(config);
    let bind = config.server.bind;

    tracing::info!(
        user = %user,
        workspace = %workspace.root().display(),
        pam_service = %config.auth.pam_service,
        "mmux starting"
    );
    if !bind.ip().is_loopback() {
        tracing::warn!(
            "mmux is listening on a non-loopback address and does not terminate TLS; \
             use a TLS reverse proxy or an SSH tunnel"
        );
    }

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("could not bind {bind}"))?;
    let local_addr = listener.local_addr()?;
    let app = web::router(AppState::new(
        config,
        sessions.clone(),
        workspace,
        authenticator,
        user,
        memory,
        readiness,
    ));

    tracing::info!("listening on http://{local_addr}");
    let result = tokio::select! {
        result = axum::serve(listener, app) => result.context("HTTP server failed"),
        _ = shutdown_signal() => {
            tracing::info!("shutting down; terminating panes");
            Ok(())
        }
    };
    sessions.shutdown().await;
    result
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "could not install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Identity, not configuration: children need to know which file the server
/// was started with so `mmux mem` and `mmux mcp` read the same vault.
fn identity_env(config_path: Option<&Path>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(path) = config_path {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        env.insert("MMUX_CONFIG".into(), path.to_string_lossy().into_owned());
    }
    env
}

fn init_tracing(default_filter: &str) {
    // `RUST_LOG` is an escape hatch for debugging, not the configuration
    // surface; the documented default is `[logging].filter` in mmux.toml.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}
