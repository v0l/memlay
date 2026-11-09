use crate::subscription::Filter;
use axum::http::HeaderMap;
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BUCKET_BOUNDS_US: [u64; 14] = [
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 1_000_000,
    5_000_000,
];
const BUCKET_COUNT: usize = BUCKET_BOUNDS_US.len() + 1;
const SOURCE_CAP: usize = 64;
const USER_AGENT_MAX: usize = 200;
const TRACKED_HEADERS: [&str; 6] = [
    "user-agent",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
    "accept-language",
    "accept-encoding",
];

#[cfg(unix)]
pub fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        return Duration::ZERO;
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[cfg(not(unix))]
pub fn thread_cpu_time() -> Duration {
    Duration::ZERO
}

pub fn measure_cpu<R>(f: impl FnOnce() -> R) -> (R, Duration) {
    let start = thread_cpu_time();
    let out = f();
    (out, thread_cpu_time().saturating_sub(start))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fnv1a(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0xff)) {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    hash
}

const KIND_BUCKET_EDGES: [usize; 6] = [1, 2, 3, 5, 10, 25];
const SET_BUCKET_EDGES: [usize; 5] = [1, 10, 50, 200, 1000];

fn count_bucket(n: usize, edges: &[usize]) -> String {
    let mut low = 1;
    for &edge in edges {
        if n <= edge {
            return if low == edge {
                edge.to_string()
            } else {
                format!("{low}-{edge}")
            };
        }
        low = edge + 1;
    }
    format!("{low}+")
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct CanonFilter {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<u32>,
    #[serde(flatten)]
    pub tags: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl CanonFilter {
    fn from_filter(f: &Filter) -> Self {
        let hex_list = |v: &Option<Vec<crate::subscription::Hex32>>| {
            let mut out: Vec<String> = v
                .as_ref()
                .map(|v| v.iter().map(|h| h.as_hex()).collect())
                .unwrap_or_default();
            out.sort();
            out.dedup();
            out
        };
        let mut kinds = f.kinds.clone().unwrap_or_default();
        kinds.sort_unstable();
        kinds.dedup();
        let tags = f
            .tag_filters
            .iter()
            .map(|(letter, values)| {
                let mut values: Vec<String> = if matches!(letter, 'e' | 'p') {
                    values.iter().map(|v| v.to_ascii_lowercase()).collect()
                } else {
                    values.clone()
                };
                values.sort();
                values.dedup();
                (format!("#{letter}"), values)
            })
            .collect();
        Self {
            ids: hex_list(&f.ids),
            authors: hex_list(&f.authors),
            kinds,
            tags,
            since: f.since,
            until: f.until,
            limit: f.limit,
        }
    }

    fn shape(&self) -> String {
        let mut parts = Vec::new();
        if !self.ids.is_empty() {
            parts.push(format!(
                "ids:{}",
                count_bucket(self.ids.len(), &SET_BUCKET_EDGES)
            ));
        }
        if !self.authors.is_empty() {
            parts.push(format!(
                "authors:{}",
                count_bucket(self.authors.len(), &SET_BUCKET_EDGES)
            ));
        }
        if !self.kinds.is_empty() {
            parts.push(format!(
                "kinds:{}",
                count_bucket(self.kinds.len(), &KIND_BUCKET_EDGES)
            ));
        }
        for (key, values) in &self.tags {
            parts.push(format!(
                "{key}:{}",
                count_bucket(values.len(), &SET_BUCKET_EDGES)
            ));
        }
        if self.since.is_some() {
            parts.push("since".to_string());
        }
        if self.until.is_some() {
            parts.push("until".to_string());
        }
        if let Some(limit) = self.limit {
            parts.push(format!("limit:{limit}"));
        }
        if parts.is_empty() {
            "{}".to_string()
        } else {
            parts.join(" ")
        }
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalReq {
    pub filters: Vec<CanonFilter>,
    pub json: String,
    pub key: u64,
    pub shape: String,
}

impl CanonicalReq {
    pub fn from_filters(filters: &[Filter]) -> Self {
        let mut canon: Vec<CanonFilter> = filters.iter().map(CanonFilter::from_filter).collect();
        canon.sort();
        canon.dedup();
        let json = serde_json::to_string(&canon).unwrap_or_default();
        let key = fnv1a(&[&json]);
        let shape = canon
            .iter()
            .map(CanonFilter::shape)
            .collect::<Vec<_>>()
            .join(" | ");
        Self {
            filters: canon,
            json,
            key,
            shape,
        }
    }

    pub fn key_hex(&self) -> String {
        format!("{:016x}", self.key)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    pub user_agent: String,
    pub origin: String,
    pub header_names: String,
    pub fingerprint: u64,
}

impl ClientInfo {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let header_value = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let mut names: Vec<&str> = headers.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        let header_names = names.join(",");
        let mut user_agent = header_value("user-agent");
        let fingerprint_values: Vec<String> =
            TRACKED_HEADERS.iter().map(|h| header_value(h)).collect();
        let mut parts: Vec<&str> = vec![&header_names];
        parts.extend(fingerprint_values.iter().map(String::as_str));
        let fingerprint = fnv1a(&parts);
        user_agent.truncate(
            user_agent
                .char_indices()
                .nth(USER_AGENT_MAX)
                .map(|(i, _)| i)
                .unwrap_or(user_agent.len()),
        );
        Self {
            user_agent,
            origin: header_value("origin"),
            header_names,
            fingerprint,
        }
    }

    pub fn fingerprint_hex(&self) -> String {
        format!("{:016x}", self.fingerprint)
    }
}

#[derive(Default)]
pub struct Hist {
    buckets: [AtomicU64; BUCKET_COUNT],
}

impl Hist {
    pub fn observe(&self, d: Duration) {
        let us = d.as_micros().min(u64::MAX as u128) as u64;
        let idx = BUCKET_BOUNDS_US
            .iter()
            .position(|&b| us <= b)
            .unwrap_or(BUCKET_COUNT - 1);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
    }

    fn counts(&self) -> Vec<u64> {
        self.buckets
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect()
    }

    pub fn snapshot(&self) -> Value {
        let counts = self.counts();
        let total: u64 = counts.iter().sum();
        let percentile = |p: f64| -> Value {
            if total == 0 {
                return Value::Null;
            }
            let rank = (total as f64 * p).ceil().max(1.0) as u64;
            let mut seen = 0;
            for (i, c) in counts.iter().enumerate() {
                seen += c;
                if seen >= rank {
                    return BUCKET_BOUNDS_US.get(i).map_or(Value::Null, |b| json!(b));
                }
            }
            Value::Null
        };
        let buckets: Vec<Value> = counts
            .iter()
            .enumerate()
            .map(|(i, c)| json!({"le_us": BUCKET_BOUNDS_US.get(i), "count": c}))
            .collect();
        json!({
            "count": total,
            "p50_us": percentile(0.50),
            "p95_us": percentile(0.95),
            "p99_us": percentile(0.99),
            "buckets": buckets,
        })
    }
}

#[derive(Default)]
pub struct Usage {
    started: AtomicU64,
    completed: AtomicU64,
    items: AtomicU64,
    cpu_ns: AtomicU64,
    wall_ns: AtomicU64,
    max_wall_us: AtomicU64,
    last_seen: AtomicU64,
    cpu: Hist,
    wall: Hist,
}

impl Usage {
    pub fn start(&self) {
        self.started.fetch_add(1, Ordering::Relaxed);
        self.last_seen.store(now_secs(), Ordering::Relaxed);
    }

    pub fn finish(&self, items: u64, cpu: Duration, wall: Duration) {
        self.completed.fetch_add(1, Ordering::Relaxed);
        self.items.fetch_add(items, Ordering::Relaxed);
        self.cpu_ns
            .fetch_add(cpu.as_nanos() as u64, Ordering::Relaxed);
        self.wall_ns
            .fetch_add(wall.as_nanos() as u64, Ordering::Relaxed);
        self.max_wall_us
            .fetch_max(wall.as_micros() as u64, Ordering::Relaxed);
        self.cpu.observe(cpu);
        self.wall.observe(wall);
    }

    pub fn started(&self) -> u64 {
        self.started.load(Ordering::Relaxed)
    }

    pub fn last_seen(&self) -> u64 {
        self.last_seen.load(Ordering::Relaxed)
    }

    fn metric(&self, sort: Sort) -> u64 {
        match sort {
            Sort::Count => self.started(),
            Sort::Cpu => self.cpu_ns.load(Ordering::Relaxed),
            Sort::Wall => self.wall_ns.load(Ordering::Relaxed),
            Sort::Events => self.items.load(Ordering::Relaxed),
        }
    }

    pub fn snapshot(&self) -> Value {
        let started = self.started();
        let completed = self.completed.load(Ordering::Relaxed);
        let cpu_ns = self.cpu_ns.load(Ordering::Relaxed);
        let wall_ns = self.wall_ns.load(Ordering::Relaxed);
        let avg_us = |ns: u64| {
            if completed == 0 {
                0.0
            } else {
                ns as f64 / completed as f64 / 1000.0
            }
        };
        json!({
            "started": started,
            "completed": completed,
            "items": self.items.load(Ordering::Relaxed),
            "cpu_total_ms": cpu_ns as f64 / 1e6,
            "wall_total_ms": wall_ns as f64 / 1e6,
            "cpu_avg_us": avg_us(cpu_ns),
            "wall_avg_us": avg_us(wall_ns),
            "wall_max_us": self.max_wall_us.load(Ordering::Relaxed),
            "last_seen_secs_ago": now_secs().saturating_sub(self.last_seen()),
            "cpu_hist": self.cpu.snapshot(),
            "wall_hist": self.wall.snapshot(),
        })
    }
}

#[derive(Default)]
pub struct Sources {
    ips: Mutex<HashSet<IpAddr>>,
    fingerprints: Mutex<HashSet<u64>>,
}

impl Sources {
    fn add(&self, ip: IpAddr, fingerprint: u64) {
        let mut ips = self.ips.lock();
        if ips.len() < SOURCE_CAP {
            ips.insert(ip);
        }
        drop(ips);
        let mut fps = self.fingerprints.lock();
        if fps.len() < SOURCE_CAP {
            fps.insert(fingerprint);
        }
    }

    fn snapshot(&self) -> Value {
        let mut ips: Vec<String> = self.ips.lock().iter().map(|i| i.to_string()).collect();
        ips.sort();
        let mut fps: Vec<String> = self
            .fingerprints
            .lock()
            .iter()
            .map(|f| format!("{f:016x}"))
            .collect();
        fps.sort();
        json!({"ips": ips, "fingerprints": fps})
    }
}

pub struct ReqEntry {
    canonical: String,
    shape: String,
    usage: Usage,
    sources: Sources,
}

#[derive(Default)]
pub struct Account {
    connections: AtomicU64,
    active: AtomicU64,
    bytes_in: AtomicU64,
    rejected: AtomicU64,
    req: Usage,
    publish: Usage,
    sources: Sources,
}

impl Account {
    fn last_seen(&self) -> Option<u64> {
        if self.active.load(Ordering::Relaxed) > 0 {
            return None;
        }
        Some(self.req.last_seen().max(self.publish.last_seen()))
    }

    fn metric(&self, sort: Sort) -> u64 {
        self.req.metric(sort) + self.publish.metric(sort)
    }

    fn snapshot(&self) -> Value {
        json!({
            "connections": self.connections.load(Ordering::Relaxed),
            "active": self.active.load(Ordering::Relaxed),
            "bytes_in": self.bytes_in.load(Ordering::Relaxed),
            "rejected": self.rejected.load(Ordering::Relaxed),
            "req": self.req.snapshot(),
            "publish": self.publish.snapshot(),
            "sources": self.sources.snapshot(),
        })
    }
}

pub struct ClientEntry {
    info: ClientInfo,
    account: Account,
}

pub struct ConnStats {
    id: u64,
    ip: IpAddr,
    fingerprint: u64,
    connected_at: u64,
    bytes_in: AtomicU64,
    rejected: AtomicU64,
    req: Usage,
    publish: Usage,
}

impl ConnStats {
    fn snapshot(&self) -> Value {
        json!({
            "id": self.id,
            "ip": self.ip.to_string(),
            "fingerprint": format!("{:016x}", self.fingerprint),
            "connected_secs": now_secs().saturating_sub(self.connected_at),
            "bytes_in": self.bytes_in.load(Ordering::Relaxed),
            "rejected": self.rejected.load(Ordering::Relaxed),
            "req": self.req.snapshot(),
            "publish": self.publish.snapshot(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Count,
    Cpu,
    Wall,
    Events,
}

impl Sort {
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("cpu") => Sort::Cpu,
            Some("wall") | Some("response") => Sort::Wall,
            Some("events") => Sort::Events,
            _ => Sort::Count,
        }
    }
}

pub struct Analytics {
    max_entries: usize,
    started_at: u64,
    next_conn: AtomicU64,
    total_connections: AtomicU64,
    pruning: AtomicBool,
    global_req: Usage,
    global_publish: Usage,
    reqs: DashMap<u64, Arc<ReqEntry>>,
    clients: DashMap<u64, Arc<ClientEntry>>,
    ips: DashMap<IpAddr, Arc<Account>>,
    conns: DashMap<u64, Arc<ConnStats>>,
}

impl Analytics {
    pub fn new(max_entries: usize) -> Arc<Self> {
        Arc::new(Self {
            max_entries: max_entries.max(16),
            started_at: now_secs(),
            next_conn: AtomicU64::new(1),
            total_connections: AtomicU64::new(0),
            pruning: AtomicBool::new(false),
            global_req: Usage::default(),
            global_publish: Usage::default(),
            reqs: DashMap::new(),
            clients: DashMap::new(),
            ips: DashMap::new(),
            conns: DashMap::new(),
        })
    }

    pub fn open_connection(self: &Arc<Self>, ip: IpAddr, info: ClientInfo) -> ConnHandle {
        let ip = ip.to_canonical();
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.total_connections.fetch_add(1, Ordering::Relaxed);

        let fingerprint = info.fingerprint;
        let client = self
            .clients
            .entry(fingerprint)
            .or_insert_with(|| {
                Arc::new(ClientEntry {
                    info,
                    account: Account::default(),
                })
            })
            .clone();
        let ip_account = self.ips.entry(ip).or_default().clone();
        for account in [&client.account, &*ip_account] {
            account.connections.fetch_add(1, Ordering::Relaxed);
            account.active.fetch_add(1, Ordering::Relaxed);
            account.sources.add(ip, fingerprint);
            account.req.last_seen.store(now_secs(), Ordering::Relaxed);
        }

        let stats = Arc::new(ConnStats {
            id,
            ip,
            fingerprint,
            connected_at: now_secs(),
            bytes_in: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            req: Usage::default(),
            publish: Usage::default(),
        });
        self.conns.insert(id, stats.clone());

        self.prune(&self.clients, self.max_entries, |e| e.account.last_seen());
        self.prune(&self.ips, self.max_entries, |a| a.last_seen());

        ConnHandle {
            analytics: self.clone(),
            client,
            ip: ip_account,
            stats,
            ip_addr: ip,
            fingerprint,
        }
    }

    fn req_entry(&self, req: &CanonicalReq) -> Arc<ReqEntry> {
        let entry = self
            .reqs
            .entry(req.key)
            .or_insert_with(|| {
                Arc::new(ReqEntry {
                    canonical: req.json.clone(),
                    shape: req.shape.clone(),
                    usage: Usage::default(),
                    sources: Sources::default(),
                })
            })
            .clone();
        self.prune(&self.reqs, self.max_entries, |e| Some(e.usage.last_seen()));
        entry
    }

    fn prune<K, V>(
        &self,
        map: &DashMap<K, Arc<V>>,
        cap: usize,
        last_seen: impl Fn(&V) -> Option<u64>,
    ) where
        K: Eq + Hash + Clone,
    {
        if map.len() <= cap {
            return;
        }
        if self
            .pruning
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let mut candidates: Vec<(u64, K)> = map
            .iter()
            .filter_map(|e| last_seen(e.value()).map(|t| (t, e.key().clone())))
            .collect();
        candidates.sort_by_key(|(t, _)| *t);
        let excess = map.len().saturating_sub(cap - cap / 10);
        for (_, key) in candidates.into_iter().take(excess) {
            map.remove(&key);
        }
        self.pruning.store(false, Ordering::Release);
    }

    pub fn report(&self, top: usize, sort: Sort) -> Value {
        let top = top.clamp(1, 1000);

        let mut reqs: Vec<(u64, u64, Arc<ReqEntry>)> = self
            .reqs
            .iter()
            .map(|e| (e.usage.metric(sort), *e.key(), e.value().clone()))
            .collect();
        reqs.sort_by_key(|r| std::cmp::Reverse(r.0));

        let mut shapes: HashMap<String, ShapeAgg> = HashMap::new();
        for (_, _, entry) in &reqs {
            shapes
                .entry(entry.shape.clone())
                .or_default()
                .add(&entry.usage);
        }
        let mut shapes: Vec<(String, ShapeAgg)> = shapes.into_iter().collect();
        shapes.sort_by_key(|(_, s)| std::cmp::Reverse(s.metric(sort)));

        let req_views: Vec<Value> = reqs
            .iter()
            .take(top)
            .map(|(_, key, e)| {
                let mut v = e.usage.snapshot();
                v["key"] = json!(format!("{key:016x}"));
                v["filters"] = serde_json::from_str(&e.canonical).unwrap_or(Value::Null);
                v["shape"] = json!(e.shape);
                v["sources"] = e.sources.snapshot();
                v
            })
            .collect();

        let mut clients: Vec<Arc<ClientEntry>> =
            self.clients.iter().map(|e| e.value().clone()).collect();
        clients.sort_by_key(|c| std::cmp::Reverse(c.account.metric(sort)));
        let client_views: Vec<Value> = clients
            .iter()
            .take(top)
            .map(|c| {
                let mut v = c.account.snapshot();
                v["fingerprint"] = json!(c.info.fingerprint_hex());
                v["user_agent"] = json!(c.info.user_agent);
                v["origin"] = json!(c.info.origin);
                v["headers"] = json!(c.info.header_names);
                v
            })
            .collect();

        let mut ips: Vec<(IpAddr, Arc<Account>)> = self
            .ips
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        ips.sort_by_key(|(_, a)| std::cmp::Reverse(a.metric(sort)));
        let ip_views: Vec<Value> = ips
            .iter()
            .take(top)
            .map(|(ip, a)| {
                let mut v = a.snapshot();
                v["ip"] = json!(ip.to_string());
                v
            })
            .collect();

        let mut conns: Vec<Arc<ConnStats>> = self.conns.iter().map(|e| e.value().clone()).collect();
        conns.sort_by_key(|c| std::cmp::Reverse(c.req.metric(sort) + c.publish.metric(sort)));
        let conn_views: Vec<Value> = conns.iter().take(top).map(|c| c.snapshot()).collect();

        json!({
            "uptime_secs": now_secs().saturating_sub(self.started_at),
            "sort": format!("{sort:?}").to_lowercase(),
            "connections": {
                "active": self.conns.len(),
                "total": self.total_connections.load(Ordering::Relaxed),
            },
            "tracked": {
                "reqs": self.reqs.len(),
                "clients": self.clients.len(),
                "ips": self.ips.len(),
                "max_entries": self.max_entries,
            },
            "req": self.global_req.snapshot(),
            "publish": self.global_publish.snapshot(),
            "top_reqs": req_views,
            "top_shapes": shapes.iter().take(top).map(|(shape, s)| s.snapshot(shape)).collect::<Vec<_>>(),
            "top_clients": client_views,
            "top_ips": ip_views,
            "top_connections": conn_views,
        })
    }
}

#[derive(Default)]
struct ShapeAgg {
    variants: u64,
    started: u64,
    items: u64,
    cpu_ns: u64,
    wall_ns: u64,
}

impl ShapeAgg {
    fn add(&mut self, u: &Usage) {
        self.variants += 1;
        self.started += u.started();
        self.items += u.metric(Sort::Events);
        self.cpu_ns += u.metric(Sort::Cpu);
        self.wall_ns += u.metric(Sort::Wall);
    }

    fn metric(&self, sort: Sort) -> u64 {
        match sort {
            Sort::Count => self.started,
            Sort::Cpu => self.cpu_ns,
            Sort::Wall => self.wall_ns,
            Sort::Events => self.items,
        }
    }

    fn snapshot(&self, shape: &str) -> Value {
        json!({
            "shape": shape,
            "distinct_reqs": self.variants,
            "started": self.started,
            "events": self.items,
            "cpu_total_ms": self.cpu_ns as f64 / 1e6,
            "wall_total_ms": self.wall_ns as f64 / 1e6,
        })
    }
}

pub struct ConnHandle {
    analytics: Arc<Analytics>,
    client: Arc<ClientEntry>,
    ip: Arc<Account>,
    stats: Arc<ConnStats>,
    ip_addr: IpAddr,
    fingerprint: u64,
}

impl ConnHandle {
    pub fn id(&self) -> u64 {
        self.stats.id
    }

    pub fn add_bytes(&self, n: usize) {
        let n = n as u64;
        self.stats.bytes_in.fetch_add(n, Ordering::Relaxed);
        self.client.account.bytes_in.fetch_add(n, Ordering::Relaxed);
        self.ip.bytes_in.fetch_add(n, Ordering::Relaxed);
    }

    pub fn reject(&self) {
        self.stats.rejected.fetch_add(1, Ordering::Relaxed);
        self.client.account.rejected.fetch_add(1, Ordering::Relaxed);
        self.ip.rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn start_req(&self, req: &CanonicalReq) -> ReqTicket {
        let entry = self.analytics.req_entry(req);
        entry.sources.add(self.ip_addr, self.fingerprint);
        for usage in [
            &self.analytics.global_req,
            &entry.usage,
            &self.client.account.req,
            &self.ip.req,
            &self.stats.req,
        ] {
            usage.start();
        }
        ReqTicket { entry }
    }

    pub fn finish_req(&self, ticket: &ReqTicket, events: u64, cpu: Duration, wall: Duration) {
        for usage in [
            &self.analytics.global_req,
            &ticket.entry.usage,
            &self.client.account.req,
            &self.ip.req,
            &self.stats.req,
        ] {
            usage.finish(events, cpu, wall);
        }
        crate::metrics::observe_request("req", cpu, wall);
    }

    pub fn start_publish(&self) {
        for usage in self.publish_usages() {
            usage.start();
        }
    }

    pub fn finish_publish(&self, accepted: bool, cpu: Duration, wall: Duration) {
        for usage in self.publish_usages() {
            usage.finish(accepted as u64, cpu, wall);
        }
        crate::metrics::observe_request("event", cpu, wall);
    }

    fn publish_usages(&self) -> [&Usage; 4] {
        [
            &self.analytics.global_publish,
            &self.client.account.publish,
            &self.ip.publish,
            &self.stats.publish,
        ]
    }
}

pub struct ReqTicket {
    entry: Arc<ReqEntry>,
}

impl Drop for ConnHandle {
    fn drop(&mut self) {
        self.analytics.conns.remove(&self.stats.id);
        self.client.account.active.fetch_sub(1, Ordering::Relaxed);
        self.ip.active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::net::Ipv4Addr;

    fn filter(json: &str) -> Filter {
        serde_json::from_str(json).unwrap()
    }

    fn localhost() -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    #[test]
    fn canonical_req_ignores_order_and_duplicates() {
        let a = CanonicalReq::from_filters(&[
            filter(r##"{"kinds":[3,1,1],"#t":["b","a"],"limit":10}"##),
            filter(r#"{"kinds":[7]}"#),
        ]);
        let b = CanonicalReq::from_filters(&[
            filter(r#"{"kinds":[7]}"#),
            filter(r##"{"limit":10,"#t":["a","b","b"],"kinds":[1,3]}"##),
            filter(r#"{"kinds":[7]}"#),
        ]);
        assert_eq!(a.key, b.key);
        assert_eq!(a.json, b.json);
        assert_eq!(a.filters.len(), 2);
    }

    #[test]
    fn canonical_req_differs_on_values_but_shape_ignores_them() {
        let a = CanonicalReq::from_filters(&[filter(r#"{"kinds":[1],"since":100}"#)]);
        let b = CanonicalReq::from_filters(&[filter(r#"{"kinds":[1],"since":200}"#)]);
        assert_ne!(a.key, b.key);
        assert_eq!(a.shape, b.shape);
        assert_eq!(a.shape, "kinds:1 since");
    }

    #[test]
    fn shape_buckets_kinds_finer_than_authors_and_tags() {
        let authors = |n: usize| {
            (0..n)
                .map(|i| format!("\"{i:064x}\""))
                .collect::<Vec<_>>()
                .join(",")
        };
        let kinds = |n: u32| (1..=n).map(|k| k.to_string()).collect::<Vec<_>>().join(",");
        let shape = |a: usize, k: u32| {
            CanonicalReq::from_filters(&[filter(&format!(
                r#"{{"authors":[{}],"kinds":[{}]}}"#,
                authors(a),
                kinds(k)
            ))])
            .shape
        };
        assert_eq!(shape(4, 2), shape(9, 2));
        assert_ne!(shape(4, 2), shape(4, 3));
        assert_eq!(shape(4, 2), "authors:2-10 kinds:2");
        assert_eq!(shape(11, 4), "authors:11-50 kinds:4-5");
        assert_eq!(shape(2000, 30), "authors:1001+ kinds:26+");
    }

    #[test]
    fn canonical_req_lowercases_e_and_p_tags() {
        let upper = "AB".repeat(32);
        let lower = "ab".repeat(32);
        let a = CanonicalReq::from_filters(&[filter(&format!(r##"{{"#e":["{upper}"]}}"##))]);
        let b = CanonicalReq::from_filters(&[filter(&format!(r##"{{"#e":["{lower}"]}}"##))]);
        assert_eq!(a.key, b.key);
    }

    #[test]
    fn client_fingerprint_depends_on_stack_not_origin() {
        let mut h1 = HeaderMap::new();
        h1.insert("user-agent", HeaderValue::from_static("bot/1.0"));
        h1.insert("origin", HeaderValue::from_static("https://a.example"));
        let mut h2 = h1.clone();
        h2.insert("origin", HeaderValue::from_static("https://b.example"));
        let mut h3 = h1.clone();
        h3.insert("user-agent", HeaderValue::from_static("bot/2.0"));
        let c1 = ClientInfo::from_headers(&h1);
        let c2 = ClientInfo::from_headers(&h2);
        let c3 = ClientInfo::from_headers(&h3);
        assert_eq!(c1.fingerprint, c2.fingerprint);
        assert_ne!(c1.fingerprint, c3.fingerprint);
        assert_eq!(c2.origin, "https://b.example");
    }

    #[test]
    fn histogram_percentiles_follow_buckets() {
        let h = Hist::default();
        for _ in 0..90 {
            h.observe(Duration::from_micros(80));
        }
        for _ in 0..10 {
            h.observe(Duration::from_millis(30));
        }
        let snap = h.snapshot();
        assert_eq!(snap["count"], 100);
        assert_eq!(snap["p50_us"], 100);
        assert_eq!(snap["p99_us"], 50_000);
    }

    #[test]
    fn requests_aggregate_across_connections_and_report() {
        let an = Analytics::new(100);
        let info = ClientInfo {
            user_agent: "scraper".into(),
            fingerprint: 7,
            ..Default::default()
        };
        let a = an.open_connection(localhost(), info.clone());
        let b = an.open_connection(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), info);
        let req = CanonicalReq::from_filters(&[filter(r#"{"kinds":[1]}"#)]);
        for handle in [&a, &b] {
            let t = handle.start_req(&req);
            handle.finish_req(&t, 5, Duration::from_micros(200), Duration::from_millis(2));
        }
        a.start_publish();
        a.finish_publish(true, Duration::from_micros(900), Duration::from_millis(3));

        let report = an.report(10, Sort::Count);
        assert_eq!(report["connections"]["active"], 2);
        assert_eq!(report["req"]["started"], 2);
        assert_eq!(report["req"]["items"], 10);
        assert_eq!(report["publish"]["completed"], 1);
        assert_eq!(report["top_reqs"][0]["started"], 2);
        assert_eq!(
            report["top_reqs"][0]["sources"]["ips"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(report["top_clients"][0]["connections"], 2);
        assert_eq!(report["top_shapes"][0]["shape"], "kinds:1");

        drop(a);
        drop(b);
        let report = an.report(10, Sort::Count);
        assert_eq!(report["connections"]["active"], 0);
        assert_eq!(report["top_clients"][0]["active"], 0);
    }

    #[test]
    fn tracking_is_bounded() {
        let an = Analytics::new(16);
        let conn = an.open_connection(localhost(), ClientInfo::default());
        for i in 0..200u64 {
            let req = CanonicalReq::from_filters(&[filter(&format!(r#"{{"since":{i}}}"#))]);
            conn.start_req(&req);
        }
        assert!(
            an.report(10, Sort::Count)["tracked"]["reqs"]
                .as_u64()
                .unwrap()
                <= 16
        );
    }

    #[test]
    fn thread_cpu_time_advances_under_load() {
        let (_, cpu) = measure_cpu(|| {
            let mut x = 0u64;
            for i in 0..5_000_000u64 {
                x = x.wrapping_add(std::hint::black_box(i));
            }
            x
        });
        if cfg!(unix) {
            assert!(cpu > Duration::ZERO);
        }
    }
}
