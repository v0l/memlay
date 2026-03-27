use prometheus::{Counter, Encoder, Gauge, Histogram, TextEncoder, register_histogram};

// Gauge: Number of active WebSocket connections
lazy_static::lazy_static! {
    pub static ref ACTIVE_CONNECTIONS: Gauge = Gauge::new(
        "memlay_active_connections",
        "Number of active WebSocket connections",
    ).expect("Failed to register active_connections gauge");

    pub static ref EVENTS_SAVED: Counter = Counter::new(
        "memlay_events_saved_total",
        "Total number of events saved",
    ).expect("Failed to register events_saved counter");

    pub static ref EVENTS_OUTPUT: Counter = Counter::new(
        "memlay_events_output_total",
        "Total number of events output",
    ).expect("Failed to register events_output counter");

    pub static ref WRITE_DELAY: Histogram = register_histogram!(
        "memlay_write_delay_seconds",
        "Histogram of write delay in seconds",
        vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0]
    ).expect("Failed to register write_delay histogram");

    pub static ref TTEOSE: Histogram = register_histogram!(
        "memlay_tteose_seconds",
        "Time To EOSE (End of Stored Events) in seconds",
        vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0]
    ).expect("Failed to register tteose histogram");

    pub static ref DISK_PERSISTENCE_TIME: Histogram = register_histogram!(
        "memlay_disk_persistence_seconds",
        "Time to persist events to disk in seconds",
        vec![0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0]
    ).expect("Failed to register disk_persistence_time histogram");
}

/// Increment the events saved counter
pub fn inc_events_saved() {
    EVENTS_SAVED.inc();
}

/// Increment the events output counter
pub fn inc_events_output() {
    EVENTS_OUTPUT.inc();
}

/// Record a write delay
pub fn observe_write_delay(duration: std::time::Duration) {
    WRITE_DELAY.observe(duration.as_secs_f64());
}

/// Record TTEOSE
pub fn observe_tteose(duration: std::time::Duration) {
    TTEOSE.observe(duration.as_secs_f64());
}

/// Record disk persistence time
pub fn observe_disk_persistence(duration: std::time::Duration) {
    DISK_PERSISTENCE_TIME.observe(duration.as_secs_f64());
}

/// Get Prometheus metrics in text format
pub fn gather_metrics() -> String {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();

    encoder
        .encode(&metric_families, &mut buffer)
        .unwrap_or_default();

    String::from_utf8(buffer).unwrap_or_default()
}
