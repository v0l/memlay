use clap::Parser;
use memlay::{config::Config, relay::Relay};
use std::fs;
use std::path::Path;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Parser)]
#[command(version, about = "memlay - in-memory Nostr relay")]
struct Cli {
    /// Path to config file (YAML)
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
}

#[tokio::main]
async fn main() {
    // Initialize file logging
    let log_dir = "/data";
    let log_file = Path::new(log_dir).join("memlay.log");
    
    // Ensure log directory exists
    if let Err(e) = fs::create_dir_all(log_dir) {
        eprintln!("Warning: Failed to create log directory {}: {}", log_dir, e);
    }
    
    // Try to open log file, fallback to stdout only if failed
    let file_result = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&log_file);
    
    match file_result {
        Ok(file) => {
            // Set up file logging layer
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(file)
                .with_file(true)
                .with_line_number(true)
                .with_thread_ids(true)
                .with_target(true);
            
            // Set up stdout logging layer
            let stdout_layer = tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_file(true)
                .with_line_number(true)
                .with_thread_ids(true)
                .with_target(true);
            
            // Combine layers with env filter
            tracing_subscriber::registry()
                .with(EnvFilter::from_default_env())
                .with(file_layer)
                .with(stdout_layer)
                .init();
        }
        Err(e) => {
            eprintln!("Warning: Failed to open log file {}: {}", log_file.display(), e);
            // Fallback to stdout only
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env())
                .init();
        }
    }

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

    let cli = Cli::parse();

    let config = Config::load(&cli.config).unwrap_or_else(|e| {
        tracing::warn!(
            "Failed to load config from {}: {e}, using defaults",
            cli.config
        );
        Config::default()
    });

    tracing::info!("Starting memlay relay on {}", config.bind_addr);
    tracing::debug!("Config: {config:?}");

    let relay = Relay::new(config.clone());
    let events = relay.events.clone();
    let router = relay.router();

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .expect("Failed to bind");

    // Graceful shutdown with persistence
    let shutdown = tokio::signal::ctrl_c();

    tokio::select! {
        res = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        ) => {
            if let Err(e) = res {
                tracing::error!("Server error: {}", e);
            }
        }
        _ = shutdown => {
            tracing::info!("Received shutdown signal");
        }
    }

    // Save events to disk on shutdown
    if let Some(ref path) = config.persistence_path {
        tracing::info!(path, "Saving events to disk before shutdown...");
        if let Err(e) = events.save_to_disk() {
            tracing::error!(error = %e, "Failed to save events to disk");
        } else {
            tracing::info!(count = events.len(), "Events saved successfully");
        }
    }
}
