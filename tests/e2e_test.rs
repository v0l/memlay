use memlay::{config::Config, relay::Relay};

#[test]
fn test_relay_creation() {
    let config = Config::default();
    let relay = Relay::new(config);

    assert!(relay.events.len() == 0);
}

#[test]
fn test_subscription_manager() {
    let config = Config::default();
    let relay = Relay::new(config);

    // Test query_filter with empty filter returns empty
    let filter = memlay::subscription::Filter::default();
    let events = relay.subscriptions.query_filter(&filter);
    assert!(events.is_empty());
}
