use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use mmux::auth::{self, PamAuthenticator};
use mmux::config::{Config, DEFAULT_CONFIG_TOML};
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
    #[arg(long, env = "MMUX_CONFIG", value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override the configured listen address.
    #[arg(long, env = "MMUX_BIND", value_name = "ADDR")]
    bind: Option<SocketAddr>,

    /// Print the documented default configuration and exit.
    #[arg(long)]
    print_default_config: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    if cli.print_default_config {
        print!("{DEFAULT_CONFIG_TOML}");
        return Ok(());
    }

    let mut config = Config::load(cli.config.as_deref())?;
    if let Some(bind) = cli.bind {
        config.server.bind = bind;
    }
    config.validate().map_err(anyhow::Error::msg)?;
    run(config).await
}

async fn run(config: Config) -> anyhow::Result<()> {
    let workspace =
        Arc::new(Workspace::new(&config.workspace.root).with_context(|| {
            format!("invalid workspace root {}", config.workspace.root.display())
        })?);
    let user = auth::current_username().context("could not determine the current Unix user")?;
    let authenticator = Arc::new(
        PamAuthenticator::new(config.auth.pam_service.clone(), user.clone())
            .context("could not configure PAM")?,
    );
    let state_path = persistence::default_state_path();
    let sessions = Arc::new(
        match state_path.as_ref() {
            Some(path) => SessionStore::with_state_file(
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
        .with_appearance(config.terminal.appearance),
    );
    if let Some(path) = state_path.as_ref() {
        tracing::info!(state = %path.display(), "project persistence enabled");
    }
    sessions.restore().await;
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("mmux=info,tower_http=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}
