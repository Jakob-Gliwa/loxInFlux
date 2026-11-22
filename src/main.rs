use anyhow::Result;
use clap::Parser;
use loxinflux::config::Config;
use std::{path::PathBuf, process::ExitCode, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Loxone Miniserver → Telegraf (Influx line protocol)")]
struct Args {
    #[arg(long, default_value = "config", env = "CONFIG_DIR")]
    config_dir: PathBuf,
    #[arg(long)]
    config_file: Option<PathBuf>,
    #[arg(long, env = "DATA_DIR", hide = true)]
    data_dir: Option<PathBuf>,
    #[arg(long, env = "LOG_LEVEL")]
    log_level: Option<String>,
    #[arg(long)]
    check_config: bool,
    #[arg(long)]
    healthcheck: bool,
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode> {
    let args = Args::parse();
    let path = args
        .config_file
        .unwrap_or_else(|| args.config_dir.join("config.toml"));
    let config = Config::load(&path)?;
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
        .event_format(loxinflux::logging::EventFormat {
            execd: config.telegraf.protocol == loxinflux::config::Protocol::Execd,
        })
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or(EnvFilter::try_new(level)?))
        .with_writer(std::io::stderr)
        .init();
    for key in &config.deprecated_keys {
        tracing::warn!(key = ?key, "Deprecated configuration entry is ignored and has no effect; accepting this entry will no longer be supported in a future version. Remove it from the configuration");
    }
    if args.data_dir.is_some() {
        tracing::warn!(
            "--data-dir / DATA_DIR is ignored: the Miniserver configuration is no longer stored on disk. Files left in that directory by earlier versions can be deleted"
        );
    }
    if args.check_config {
        eprintln!("Configuration valid: {}", path.display());
        return Ok(ExitCode::SUCCESS);
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
        return Ok(ExitCode::SUCCESS);
    }
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        arch = std::env::consts::ARCH,
        "Starting loxInFlux"
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
    let result = loxinflux::app::run(config, stop).await;
    signals.abort();
    match result {
        Ok(()) => {
            tracing::info!("loxInFlux stopped");
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            tracing::error!(error = %format_args!("{error:#}"), "loxInFlux stopped");
            Ok(ExitCode::FAILURE)
        }
    }
}
