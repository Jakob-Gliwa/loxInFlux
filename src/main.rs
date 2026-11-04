use anyhow::{Context, Result};
use clap::Parser;
use loxtotelegraf::config::Config;
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    version,
    about = "Loxone → Telegraf, compatible with loxInFlux TOML configurations"
)]
struct Args {
    #[arg(long, default_value = "config", env = "CONFIG_DIR")]
    config_dir: PathBuf,
    #[arg(long)]
    config_file: Option<PathBuf>,
    #[arg(long, env = "DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, env = "LOG_LEVEL")]
    log_level: Option<String>,
    #[arg(long)]
    check_config: bool,
    #[arg(long)]
    healthcheck: bool,
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let path = args
        .config_file
        .unwrap_or_else(|| args.config_dir.join("config.toml"));
    let mut config = Config::load(&path)?;
    if let Some(path) = args.data_dir {
        config.paths.data_dir = path.to_string_lossy().into_owned();
    }
    let level = args
        .log_level
        .unwrap_or_else(|| config.logging.level.clone())
        .to_ascii_lowercase();
    let level = match level.as_str() {
        "warning" => "warn",
        "critical" => "error",
        other => other,
    };
    tracing_subscriber::fmt()
        .with_ansi(false)
        .event_format(loxtotelegraf::logging::EventFormat {
            execd: config.telegraf.protocol == loxtotelegraf::config::Protocol::Execd,
        })
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or(EnvFilter::try_new(level)?))
        .with_writer(std::io::stderr)
        .init();
    for key in &config.deprecated_keys {
        tracing::warn!(key = ?key, "Deprecated configuration entry is ignored and has no effect; accepting this entry will no longer be supported in a future version. Remove it from the configuration");
    }
    if args.check_config {
        eprintln!("Configuration valid: {}", path.display());
        return Ok(());
    }
    if args.healthcheck {
        anyhow::ensure!(config.monitoring.enabled, "Monitoring endpoint is disabled");
        let mut addr: std::net::SocketAddr = config.monitoring.listen.parse()?;
        if addr.ip().is_unspecified() {
            addr.set_ip(if addr.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()?
            .get(format!("http://{addr}/health"))
            .send()
            .await?
            .error_for_status()?;
        return Ok(());
    }
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        arch = std::env::consts::ARCH,
        "Starting loxToTelegraf"
    );
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let signals = tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signal.cancel();
    });
    let result = loxtotelegraf::app::run(config, stop)
        .await
        .context("Bridge stopped");
    signals.abort();
    result
}
