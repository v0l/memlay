use prometheus::{
    Counter, Encoder, Gauge, Histogram, TextEncoder, register_counter, register_gauge,
    register_histogram,
};

// NOTE: these must use the `register_*` macros, not `Gauge::new`/`Counter::new`.
// The plain constructors do not attach the metric to the default registry, so
// `prometheus::gather()` never sees them and they silently never appear on
// /metrics.
lazy_static::lazy_static! {
    pub static ref ACTIVE_CONNECTIONS: Gauge = register_gauge!(
        "memlay_active_connections",
        "Number of active WebSocket connections"
    ).expect("Failed to register active_connections gauge");

    pub static ref EVENTS_SAVED: Counter = register_counter!(
        "memlay_events_saved_total",
        "Total number of events saved"
    ).expect("Failed to register events_saved counter");

    pub static ref EVENTS_OUTPUT: Counter = register_counter!(
        "memlay_events_output_total",
        "Total number of events output"
    ).expect("Failed to register events_output counter");

    pub static ref IDLE_DISCONNECTS: Counter = register_counter!(
        "memlay_idle_disconnects_total",
        "Connections closed for sending no REQ or EVENT within the idle timeout"
    ).expect("Failed to register idle_disconnects counter");

    pub static ref SUBS_OVERFLOWED: Counter = register_counter!(
        "memlay_subscriptions_overflowed_total",
        "Subscriptions closed because the client could not keep up with live delivery"
    ).expect("Failed to register subscriptions_overflowed counter");

    pub static ref EVENTS_DROPPED: Counter = register_counter!(
        "memlay_events_dropped_total",
        "Live events dropped because a connection's send queue was full"
    ).expect("Failed to register events_dropped counter");

    pub static ref EVENTS_DELETED: Counter = register_counter!(
        "memlay_events_deleted_total",
        "Events removed by NIP-09 deletion requests"
    ).expect("Failed to register events_deleted counter");

    pub static ref DELETION_REQUESTS: Counter = register_counter!(
        "memlay_deletion_requests_total",
        "NIP-09 kind-5 deletion requests accepted"
    ).expect("Failed to register deletion_requests counter");

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

/// Increment the idle-disconnect counter
pub fn inc_idle_disconnects() {
    IDLE_DISCONNECTS.inc();
}

/// Increment the overflowed-subscription counter
pub fn inc_subs_overflowed() {
    SUBS_OVERFLOWED.inc();
}

/// Increment the dropped live-event counter
pub fn inc_events_dropped() {
    EVENTS_DROPPED.inc();
}

/// Increment the NIP-09 deleted-events counter
pub fn inc_events_deleted() {
    EVENTS_DELETED.inc();
}

/// Increment the NIP-09 accepted-deletion-requests counter
pub fn inc_deletion_requests() {
    DELETION_REQUESTS.inc();
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

/// Force-initialise every lazily-registered metric so they are exported at
/// zero from startup instead of appearing only after the first occurrence.
/// Without this, dashboards and alerts see a missing series rather than 0.
pub fn init() {
    lazy_static::initialize(&ACTIVE_CONNECTIONS);
    lazy_static::initialize(&EVENTS_SAVED);
    lazy_static::initialize(&EVENTS_OUTPUT);
    lazy_static::initialize(&IDLE_DISCONNECTS);
    lazy_static::initialize(&SUBS_OVERFLOWED);
    lazy_static::initialize(&EVENTS_DROPPED);
    lazy_static::initialize(&EVENTS_DELETED);
    lazy_static::initialize(&DELETION_REQUESTS);
    lazy_static::initialize(&WRITE_DELAY);
    lazy_static::initialize(&TTEOSE);
    lazy_static::initialize(&DISK_PERSISTENCE_TIME);
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
