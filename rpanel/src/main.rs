use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use rpanel::{
    config::{Config, ServerConfig},
    net::{NetworkHistory, RETENTION, spawn_sampler},
    panel, proxy,
    route::build_routes,
    state::{AppState, PanelState, build_client},
};

#[derive(Debug, Parser)]
#[command(version, about = "Reverse proxy with a server-rendered status panel")]
struct Args {
    /// Path to the YAML configuration file.
    #[arg(short, long, value_name = "FILE", default_value_os_t = default_config_path())]
    config: PathBuf,

    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = Config::load(&args.config)?;

    if args.check {
        println!("{}: ok", args.config.display());
        return Ok(());
    }

    let _log_guard = init_logging(&config.server)?;
    // axum-server is built without a default crypto provider; install ring
    // before loading any TLS configuration. An existing provider is fine.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let addr = resolve_addr(&config.server).await?;
    let insecure = config.server.allow_insecure;
    let tls = if insecure {
        None
    } else {
        let cert = config
            .server
            .cert
            .as_ref()
            .context("server.cert is required")?;
        let key = config
            .server
            .key
            .as_ref()
            .context("server.key is required")?;
        Some(
            RustlsConfig::from_pem_file(cert, key)
                .await
                .with_context(|| {
                    format!(
                        "loading TLS certificate {} and key {}",
                        cert.display(),
                        key.display()
                    )
                })?,
        )
    };

    let routes = Arc::new(build_routes(&config.groups));
    let client = build_client(config.server.allow_insecure_upstream)?;
    let network = Arc::new(network_history(&config));

    if config.panel.enable && config.panel.status.network {
        spawn_sampler(network.clone(), Duration::from_secs(10));
    }

    let panel_state = config.panel.enable.then(|| {
        Arc::new(PanelState {
            config: config.panel.clone(),
            network: network.clone(),
        })
    });

    tracing::info!(
        %addr,
        tls = !insecure,
        groups = config.groups.len(),
        "starting rpanel"
    );

    let state = Arc::new(AppState {
        config: Arc::new(config),
        routes,
        client,
        panel: panel_state,
    });

    let mut app = Router::new();
    if state.panel.is_some() {
        app = app.route("/", get(panel::handler));
    }
    let app = app.fallback(proxy::handler).with_state(state);

    let handle = axum_server::Handle::<SocketAddr>::new();
    spawn_shutdown(handle.clone());

    if let Some(tls) = tls {
        axum_server::tls_rustls::bind_rustls(addr, tls)
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await?;
    } else {
        axum_server::bind(addr)
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await?;
    }

    // Persist the last few samples collected before shutting down.
    rpanel::net::save(&network);

    Ok(())
}

fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rpanel.yaml")
}

fn default_state_path() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rpanel")
        .join("network.yaml")
}

fn network_history(config: &Config) -> NetworkHistory {
    match &config.panel.network_state {
        Some(path) => NetworkHistory::with_state(RETENTION, path.clone()),
        None if config.panel.enable && config.panel.status.network => {
            NetworkHistory::with_state(RETENTION, default_state_path())
        }
        None => NetworkHistory::new(RETENTION),
    }
}

fn init_logging(server: &ServerConfig) -> Result<Option<WorkerGuard>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    match &server.log {
        Some(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating log directory {}", parent.display()))?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening log file {}", path.display()))?;
            let (writer, guard) = tracing_appender::non_blocking(file);
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_ansi(false)
                .init();
            Ok(Some(guard))
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            Ok(None)
        }
    }
}

async fn resolve_addr(server: &ServerConfig) -> Result<SocketAddr> {
    let addrs: Vec<_> = tokio::net::lookup_host((server.host.as_str(), server.port))
        .await
        .with_context(|| format!("resolving {}:{}", server.host, server.port))?
        .collect();
    addrs
        .iter()
        .find(|addr| addr.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .with_context(|| format!("no address found for {}:{}", server.host, server.port))
}

fn spawn_shutdown(handle: axum_server::Handle<SocketAddr>) {
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutting down");
        handle.graceful_shutdown(Some(Duration::from_secs(10)));
    });
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }

    let _ = tokio::signal::ctrl_c().await;
}
