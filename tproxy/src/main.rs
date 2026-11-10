use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tokio::net::lookup_host;
use tproxy::{
    cli,
    config::{Config, default_path, warn_if_insecure, warn_if_insecure_key},
    proxy::{self, AppState},
    usage::Usage,
};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// OpenAI-compatible proxy.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to the config file.
    #[arg(short, long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy server (default).
    Serve,
    /// Add a user and print their API key.
    #[command(name = "useradd")]
    UserAdd { name: String },
    /// Interactively add a model.
    #[command(name = "modeladd")]
    ModelAdd,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(default_path);

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(&path).await,
        Command::UserAdd { name } => {
            let mut config = Config::load(&path)?;
            let key = cli::add_user(&mut config, &name)?;
            config.save(&path)?;
            println!("user {name} added\nAPI key: {key}");
            Ok(())
        }
        Command::ModelAdd => {
            let mut config = Config::load(&path)?;
            cli::prompt_model(&mut config)?;
            config.save(&path)
        }
    }
}

async fn serve(path: &Path) -> Result<()> {
    refuse_root()?;

    let config = Config::load(path)?;
    let _guard = init_logging(config.server.log.as_deref())?;
    warn_if_insecure(path);
    let usage_path = tproxy::usage::default_path();
    tracing::info!(path = %usage_path.display(), "usage data");
    let usage = Usage::load(usage_path);
    let state = AppState::with_usage(config.clone(), usage.clone())?;
    let flush = tokio::spawn(usage.clone().flush_loop());

    let port: u16 = config
        .server
        .port
        .parse()
        .with_context(|| format!("invalid server.port {:?}", config.server.port))?;
    let addr = lookup_host((config.server.host.as_str(), port))
        .await
        .context("resolve server address")?
        .next()
        .context("server address resolved to nothing")?;
    let tls = config.server.cert.is_some();
    check_transport(addr, tls, config.server.allow_insecure)?;
    if let Some(key) = &config.server.key {
        warn_if_insecure_key(key);
    }

    for (alias, model) in &config.models {
        if model.endpoint.starts_with("http://") {
            tracing::warn!(model = alias, "upstream endpoint uses plaintext HTTP");
        }
    }
    for (name, key) in &config.users {
        if key.len() < 16 {
            tracing::warn!(user = name, "API key is shorter than 16 characters");
        }
    }
    if !tls {
        tracing::warn!(%addr, "TLS is disabled");
    }

    tracing::info!(
        %addr,
        tls,
        models = config.models.len(),
        users = config.users.len(),
        "starting tproxy"
    );

    let app = proxy::router(state);
    let server = async {
        if let (Some(cert), Some(key)) = (&config.server.cert, &config.server.key) {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                .await
                .context("load TLS certificate")?;
            axum_server::bind_rustls(addr, tls)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .context("server error")
        } else {
            axum_server::bind(addr)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .context("server error")
        }
    };

    let result = tokio::select! {
        result = server => result,
        _ = shutdown() => {
            tracing::info!("shutting down");
            Ok(())
        }
    };

    flush.abort();
    let _ = flush.await;
    if let Err(error) = usage.save() {
        tracing::warn!(%error, "failed to save usage data");
    }
    result
}

async fn shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(error) => {
            tracing::warn!(%error, "failed to listen for SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

fn refuse_root() -> Result<()> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    refuse_uid(unsafe { libc::geteuid() })
}

fn refuse_uid(uid: libc::uid_t) -> Result<()> {
    if uid == 0 {
        bail!("refusing to run as root; run tproxy as an unprivileged user");
    }
    Ok(())
}

fn check_transport(addr: SocketAddr, tls: bool, allow_insecure: bool) -> Result<()> {
    if !tls && !allow_insecure && !addr.ip().is_loopback() {
        bail!(
            "refusing plaintext HTTP on non-loopback address {addr}; configure server.cert/server.key or set server.allow_insecure: true"
        );
    }
    Ok(())
}

fn init_logging(path: Option<&Path>) -> Result<Option<WorkerGuard>> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("tproxy=info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    match path {
        Some(path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("open log {}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("set permissions on {}", path.display()))?;
            let (writer, guard) = tracing_appender::non_blocking(file);
            builder.with_writer(writer).with_ansi(false).init();
            Ok(Some(guard))
        }
        None => {
            builder.init();
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_root_uid() {
        assert!(refuse_uid(0).is_err());
        assert!(refuse_uid(1000).is_ok());
    }

    #[test]
    fn refuses_public_plaintext() {
        let public: SocketAddr = "0.0.0.0:8001".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:8001".parse().unwrap();
        assert!(check_transport(public, false, false).is_err());
        assert!(check_transport(public, true, false).is_ok());
        assert!(check_transport(public, false, true).is_ok());
        assert!(check_transport(local, false, false).is_ok());
    }
}
