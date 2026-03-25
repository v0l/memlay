use prometheus::{
    Histogram, Gauge, register_histogram, Encoder, TextEncoder,
};
use std::sync::atomic::{AtomicU64, Ordering};

// Gauge: Number of active WebSocket connections
lazy_static::lazy_static! {
    pub static ref ACTIVE_CONNECTIONS: Gauge = Gauge::new(
        "memlay_active_connections",
        "Number of active WebSocket connections",
    ).expect("Failed to register active_connections gauge");

    pub static ref EVENTS_SAVED_RATE: Gauge = Gauge::new(
        "memlay_events_saved_rate",
        "Events saved per second",
    ).expect("Failed to register events_saved_rate gauge");

    pub static ref EVENTS_OUTPUT_RATE: Gauge = Gauge::new(
        "memlay_events_output_rate",
        "Events output per second",
    ).expect("Failed to register events_output_rate gauge");

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

// Atomic counters for rate calculation with delta tracking
static EVENTS_SAVED_TOTAL: AtomicU64 = AtomicU64::new(0);
static EVENTS_OUTPUT_TOTAL: AtomicU64 = AtomicU64::new(0);
static LAST_SAVED: AtomicU64 = AtomicU64::new(0);
static LAST_OUTPUT: AtomicU64 = AtomicU64::new(0);
static LAST_CHECK_TIME: AtomicU64 = AtomicU64::new(0);

/// Increment the events saved counter
pub fn inc_events_saved() {
    EVENTS_SAVED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Increment the events output counter
pub fn inc_events_output() {
    EVENTS_OUTPUT_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Update the rate gauges (should be called periodically, e.g., every second)
pub fn update_rates() {
    let now = std::time::UNIX_EPOCH
        .elapsed()
        .unwrap()
        .as_secs();
    
    let last_check = LAST_CHECK_TIME.load(Ordering::Relaxed);
    if last_check == 0 {
        LAST_CHECK_TIME.store(now, Ordering::Relaxed);
        return;
    }
    
    let elapsed = now.saturating_sub(last_check) as f64;
    if elapsed <= 0.0 {
        return;
    }
    
    // Calculate delta for saved events
    let total_saved = EVENTS_SAVED_TOTAL.load(Ordering::Relaxed);
    let last_saved = LAST_SAVED.load(Ordering::Relaxed);
    let saved_delta = total_saved.saturating_sub(last_saved);
    EVENTS_SAVED_RATE.set(saved_delta as f64 / elapsed);
    LAST_SAVED.store(total_saved, Ordering::Relaxed);
    
    // Calculate delta for output events
    let total_output = EVENTS_OUTPUT_TOTAL.load(Ordering::Relaxed);
    let last_output = LAST_OUTPUT.load(Ordering::Relaxed);
    let output_delta = total_output.saturating_sub(last_output);
    EVENTS_OUTPUT_RATE.set(output_delta as f64 / elapsed);
    LAST_OUTPUT.store(total_output, Ordering::Relaxed);
    
    LAST_CHECK_TIME.store(now, Ordering::Relaxed);
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
    
    encoder.encode(&metric_families, &mut buffer).unwrap_or_default();
    
    String::from_utf8(buffer).unwrap_or_default()
}
