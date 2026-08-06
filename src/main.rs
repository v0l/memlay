use clap::Parser;
use memlay::{config::Config, relay::Relay};
use std::fs;
use std::path::Path;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(version, about = "memlay - in-memory Nostr relay")]
struct Cli {
    /// Path to config file (YAML)
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
}

/// Initialize tracing. Always logs to stdout; additionally writes to
/// `<log_dir>/memlay.log` when `log_dir` is configured.
fn init_logging(log_dir: Option<&str>) {
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(true);

    let file_layer = log_dir.and_then(|dir| {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("Warning: failed to create log directory {dir}: {e}");
            return None;
        }
        let log_file = Path::new(dir).join("memlay.log");
        match fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&log_file)
        {
            Ok(file) => Some(
                tracing_subscriber::fmt::layer()
                    .with_writer(file)
                    .with_file(true)
                    .with_line_number(true)
                    .with_thread_ids(true)
                    .with_target(true),
            ),
            Err(e) => {
                eprintln!(
                    "Warning: failed to open log file {}: {e}",
                    log_file.display()
                );
                None
            }
        }
    });

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(stdout_layer)
        .with(file_layer)
        .init();
}

/// Resolve when either SIGINT (Ctrl-C) or SIGTERM (container stop) arrives.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!("failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let config = Config::load(&cli.config).unwrap_or_else(|e| {
        eprintln!(
            "Failed to load config from {}: {e}, using defaults",
            cli.config
        );
        Config::default()
    });

    init_logging(config.log_dir.as_deref());

    // Set file descriptor limits
    if let Ok((soft, hard)) = rlimit::getrlimit(rlimit::Resource::NOFILE) {
        if soft < 65536 {
            if let Err(e) = rlimit::setrlimit(rlimit::Resource::NOFILE, 65536, hard) {
                tracing::warn!("Failed to set file descriptor limit: {}", e);
            }
        }
        let (soft, hard) = rlimit::getrlimit(rlimit::Resource::NOFILE).unwrap_or((soft, hard));
        tracing::info!("File descriptor limits: soft={}, hard={}", soft, hard);
    }

    tracing::info!("Starting memlay relay on {}", config.bind_addr);
    tracing::debug!("Config: {config:?}");

    let relay = Relay::new(config.clone());
    let events = relay.events.clone();
    let router = relay.router();

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .expect("Failed to bind");

    tokio::select! {
        res = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        ) => {
            if let Err(e) = res {
                tracing::error!("Server error: {}", e);
            }
        }
        _ = shutdown_signal() => {
            tracing::info!("Shutting down");
        }
    }

    // Persist on shutdown. A checkpoint folds the WAL into a fresh snapshot so
    // the next start skips replaying a large log; fall back to a plain flush.
    if config.persistence_path.is_some() {
        tracing::info!("Persisting events to disk before shutdown...");
        let result = events.checkpoint().or_else(|e| {
            tracing::warn!(error = %e, "checkpoint failed, falling back to WAL flush");
            events.save_to_disk()
        });
        match result {
            Ok(_) => tracing::info!(count = events.len(), "events persisted successfully"),
            Err(e) => tracing::error!(error = %e, "failed to persist events to disk"),
        }
    }
}
