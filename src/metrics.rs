use prometheus::{
    Counter, CounterVec, Encoder, Gauge, Histogram, HistogramVec, TextEncoder, register_counter,
    register_counter_vec, register_gauge, register_histogram, register_histogram_vec,
};

const TRACKED_KINDS: [u32; 16] = [
    0, 1, 3, 4, 5, 6, 7, 16, 1059, 1111, 9734, 9735, 10002, 30023, 30078, 39000,
];

fn kind_label(kind: u32) -> String {
    if TRACKED_KINDS.contains(&kind) {
        kind.to_string()
    } else {
        "other".to_string()
    }
}

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

    pub static ref BYTES_SENT: Counter = register_counter!(
        "memlay_transmit_bytes_total",
        "Bytes of WebSocket text frames written to clients"
    ).expect("Failed to register transmit_bytes counter");

    pub static ref BYTES_RECEIVED: Counter = register_counter!(
        "memlay_receive_bytes_total",
        "Bytes of WebSocket text frames received from clients"
    ).expect("Failed to register receive_bytes counter");

    pub static ref EVENTS_EVICTED: Counter = register_counter!(
        "memlay_events_evicted_total",
        "Events evicted to stay inside the memory budget"
    ).expect("Failed to register events_evicted counter");

    pub static ref EVENTS_EXPIRED: CounterVec = register_counter_vec!(
        "memlay_events_expired_total",
        "Events removed when their kind retention ran out, by kind",
        &["kind"]
    ).expect("Failed to register events_expired counter");

    pub static ref SAVED_BY_KIND: CounterVec = register_counter_vec!(
        "memlay_events_saved_by_kind_total",
        "Events saved, by kind (untracked kinds are grouped as other)",
        &["kind"]
    ).expect("Failed to register saved_by_kind counter");

    pub static ref SAVED_BYTES_BY_KIND: CounterVec = register_counter_vec!(
        "memlay_event_bytes_saved_by_kind_total",
        "Raw event bytes saved, by kind (untracked kinds are grouped as other)",
        &["kind"]
    ).expect("Failed to register saved_bytes_by_kind counter");

    pub static ref STORE_EVENTS: Gauge = register_gauge!(
        "memlay_store_events",
        "Events currently held in the store"
    ).expect("Failed to register store_events gauge");

    pub static ref STORE_BYTES: Gauge = register_gauge!(
        "memlay_store_payload_bytes",
        "Payload bytes currently held in the store"
    ).expect("Failed to register store_bytes gauge");

    pub static ref PAYLOAD_BUDGET: Gauge = register_gauge!(
        "memlay_store_payload_budget_bytes",
        "Payload bytes the store may hold before eviction"
    ).expect("Failed to register payload_budget gauge");

    pub static ref MEMORY_LIMIT: Gauge = register_gauge!(
        "memlay_memory_limit_bytes",
        "Process memory budget the eviction logic works against"
    ).expect("Failed to register memory_limit gauge");

    pub static ref PROCESS_MEMORY: Gauge = register_gauge!(
        "memlay_process_memory_bytes",
        "Resident memory of the relay process"
    ).expect("Failed to register process_memory gauge");

    pub static ref AMPLIFICATION: Gauge = register_gauge!(
        "memlay_memory_amplification_ratio",
        "Measured process memory over payload bytes"
    ).expect("Failed to register amplification gauge");

    pub static ref TOMBSTONES: Gauge = register_gauge!(
        "memlay_tombstones",
        "NIP-09 tombstones currently held"
    ).expect("Failed to register tombstones gauge");

    pub static ref REQUEST_CPU: HistogramVec = register_histogram_vec!(
        "memlay_request_cpu_seconds",
        "CPU time spent handling a client request, by kind (req or event)",
        &["kind"],
        vec![0.00005, 0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1.0]
    ).expect("Failed to register request_cpu histogram");

    pub static ref REQUEST_WALL: HistogramVec = register_histogram_vec!(
        "memlay_request_response_seconds",
        "Wall clock response time of a client request, by kind (req or event)",
        &["kind"],
        vec![0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
    ).expect("Failed to register request_wall histogram");

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

pub fn observe_saved(kind: u32, bytes: usize) {
    let label = kind_label(kind);
    SAVED_BY_KIND.with_label_values(&[&label]).inc();
    SAVED_BYTES_BY_KIND
        .with_label_values(&[&label])
        .inc_by(bytes as f64);
}

pub fn add_bytes_sent(n: usize) {
    BYTES_SENT.inc_by(n as f64);
}

pub fn add_bytes_received(n: usize) {
    BYTES_RECEIVED.inc_by(n as f64);
}

pub fn inc_events_evicted(n: usize) {
    EVENTS_EVICTED.inc_by(n as f64);
}

pub fn inc_events_expired(kind: u32) {
    EVENTS_EXPIRED.with_label_values(&[&kind_label(kind)]).inc();
}

pub fn observe_request(kind: &str, cpu: std::time::Duration, wall: std::time::Duration) {
    REQUEST_CPU
        .with_label_values(&[kind])
        .observe(cpu.as_secs_f64());
    REQUEST_WALL
        .with_label_values(&[kind])
        .observe(wall.as_secs_f64());
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
    lazy_static::initialize(&EVENTS_EVICTED);
    lazy_static::initialize(&BYTES_SENT);
    lazy_static::initialize(&BYTES_RECEIVED);
    lazy_static::initialize(&STORE_EVENTS);
    lazy_static::initialize(&STORE_BYTES);
    lazy_static::initialize(&PAYLOAD_BUDGET);
    lazy_static::initialize(&MEMORY_LIMIT);
    lazy_static::initialize(&PROCESS_MEMORY);
    lazy_static::initialize(&AMPLIFICATION);
    lazy_static::initialize(&TOMBSTONES);
    for kind in TRACKED_KINDS {
        let label = kind_label(kind);
        SAVED_BY_KIND.with_label_values(&[&label]);
        SAVED_BYTES_BY_KIND.with_label_values(&[&label]);
    }
    SAVED_BY_KIND.with_label_values(&["other"]);
    SAVED_BYTES_BY_KIND.with_label_values(&["other"]);
    for kind in ["req", "event"] {
        REQUEST_CPU.with_label_values(&[kind]);
        REQUEST_WALL.with_label_values(&[kind]);
    }
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
