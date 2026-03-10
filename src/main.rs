use memlay::{config::Config, relay::Relay};

#[tokio::main]
async fn main() {
    let config = Config::default();
    let relay = Relay::new(config.clone());
    
    let router = relay.router();
    
    tracing::info!("Starting memlay relay on {}", config.bind_addr);
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await.unwrap();
    axum::serve(listener, router).await.unwrap();
}
