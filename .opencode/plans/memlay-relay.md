# Memlay - High Performance In-Memory Nostr Relay

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    Hyper HTTP Server                        │
│              (fastwebsockets upgrade)                       │
├─────────────────────────────────────────────────────────────┤
│                  Connection Manager                         │
│         (per-connection state, subscription mgmt)           │
├─────────────────────────────────────────────────────────────┤
│                    Message Router                           │
│         EVENT / REQ / CLOSE dispatch                        │
├─────────────────┬───────────────────────────────────────────┤
│  Event Store    │           Index Layer                     │
│  Arc<Event>     │  ┌─────────────────────────────────────┐ │
│  + raw JSON     │  │ id_index: HashMap<[u8;32], Arc>     │ │
│                 │  │ pubkey_idx: BTreeMap<PK, BTreeSet>  │ │
│  LRU eviction   │  │ kind_idx: BTreeMap<Kind, BTreeSet>  │ │
│                 │  │ tag_idx: BTreeMap<Tag, BTreeSet>    │ │
│                 │  │ created_at: BTreeMap<ts, HashSet>   │ │
│                 │  └─────────────────────────────────────┘ │
└─────────────────┴───────────────────────────────────────────┘
```

## Design Decisions

- **NIP Support**: NIP-01 only (minimal)
- **Scale Target**: Large (1M+ events)
- **Indexing**: B-Tree indexes for range queries
- **Persistence**: Pure in-memory (volatile)
- **WebSocket**: hyper + fastwebsockets
- **Storage**: Arc<Event> with raw JSON bytes for zero-copy responses
- **Memory Management**: LRU eviction
- **Concurrency**: RwLock<BTreeMap> per index

## Module Structure

```
src/
├── main.rs              # Server bootstrap, config
├── config.rs            # Runtime configuration
├── relay/
│   ├── mod.rs
│   ├── server.rs        # Hyper service, WS upgrade
│   └── connection.rs    # Per-client connection handler
├── protocol/
│   ├── mod.rs
│   ├── message.rs       # CLIENT->RELAY messages (EVENT, REQ, CLOSE)
│   ├── response.rs      # RELAY->CLIENT messages (EVENT, OK, EOSE, NOTICE)
│   ├── filter.rs        # NIP-01 filter parsing & matching
│   └── event.rs         # Event struct with raw bytes
├── store/
│   ├── mod.rs
│   ├── memory.rs        # In-memory event storage
│   ├── index.rs         # B-Tree index structures
│   └── lru.rs           # LRU eviction policy
└── subscription/
    ├── mod.rs
    └── manager.rs       # Subscription tracking & broadcast
```

## Core Types

### Event (`protocol/event.rs`)

```rust
pub struct Event {
    // Parsed fields for indexing
    pub id: [u8; 32],
    pub pubkey: [u8; 32],
    pub created_at: u64,
    pub kind: u32,
    pub tags: Vec<Tag>,
    pub sig: [u8; 64],
    // Original JSON for zero-copy response
    pub raw: Bytes,  // bytes::Bytes for cheap cloning
}

pub struct Tag {
    pub tag_type: char,       // 'e', 'p', 't', etc.
    pub values: Vec<String>,  // tag values
}
```

### EventRef (`store/index.rs`)

```rust
pub struct EventRef {
    // Sort keys (used for Ord)
    created_at: u64,
    id: [u8; 32],
    // Payload (ignored in Ord/Eq)
    pub event: Arc<Event>,
}

impl Ord for EventRef {
    fn cmp(&self, other: &Self) -> Ordering {
        // Descending by time, ascending by id for tiebreaker
        other.created_at.cmp(&self.created_at)
            .then_with(|| self.id.cmp(&other.id))
    }
}
```

### EventIndex (`store/index.rs`)

```rust
pub struct EventIndex {
    // Primary: O(1) lookup by id
    by_id: RwLock<HashMap<[u8;32], Arc<Event>>>,
    
    // Secondary: B-Tree for range queries (sorted by created_at via EventRef)
    by_pubkey: RwLock<BTreeMap<[u8;32], BTreeSet<EventRef>>>,
    by_kind: RwLock<BTreeMap<u32, BTreeSet<EventRef>>>,
    
    // Tag indexes: e-tags and p-tags (most common in NIP-01)
    by_tag_e: RwLock<BTreeMap<[u8;32], BTreeSet<EventRef>>>,
    by_tag_p: RwLock<BTreeMap<[u8;32], BTreeSet<EventRef>>>,
}
```

### LRU Tracker (`store/lru.rs`)

```rust
pub struct LruTracker {
    order: LinkedHashMap<[u8;32], usize>,  // id -> byte size
    max_events: usize,
    current_bytes: usize,
    max_bytes: usize,
}
```

### Filter (`protocol/filter.rs`)

```rust
pub struct Filter {
    pub ids: Option<Vec<[u8;32]>>,
    pub authors: Option<Vec<[u8;32]>>,
    pub kinds: Option<Vec<u32>>,
    pub tags: HashMap<char, Vec<String>>,  // #e, #p, etc.
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<usize>,
}
```

## Dependencies

```toml
[dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }

# HTTP/WebSocket
hyper = { version = "1", features = ["server", "http1"] }
hyper-util = { version = "0.1", features = ["tokio"] }
fastwebsockets = { version = "0.8", features = ["upgrade"] }
http-body-util = "0.1"

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Crypto (signature verification)
secp256k1 = { version = "0.29", features = ["global-context"] }
sha2 = "0.10"

# Data structures
bytes = "1"  # Zero-copy byte buffers
linked-hash-map = "0.5"  # LRU tracking
parking_lot = "0.12"  # Faster RwLock

# Utils
hex = "0.4"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
```

## Implementation Phases

| Phase | Tasks | Effort |
|-------|-------|--------|
| **1. Store** | Event struct, EventIndex, LRU eviction, benchmarks | ~3 hrs |
| **2. Protocol** | Filter, Messages, signature verification | ~2 hrs |
| **3. WebSocket** | Hyper + fastwebsockets server, connection lifecycle | ~2 hrs |
| **4. Query Engine** | Filter matching, index intersection, pagination | ~2 hrs |
| **5. Integration** | REQ/EVENT/CLOSE handling, subscription broadcast | ~2 hrs |
| **6. Testing** | Unit tests, integration tests with nostr client | ~2 hrs |

## Zero-Copy Response Path

```
Client REQ → Filter.query(index) → Vec<Arc<Event>>
                                        │
                                        ▼
                              event.raw.clone()  ← Bytes is refcounted
                                        │
                                        ▼
                              ["EVENT", sub_id, <raw>]  ← Direct write
                                        │
                                        ▼
                              ws.write_frame(Frame::text(payload))
```

## Benchmarks (Phase 1)

Target metrics for the store:
- Insert: < 1μs per event (amortized)
- Lookup by id: < 100ns
- Query by kind (limit 100): < 10μs
- Query by pubkey + since (limit 100): < 10μs
- LRU eviction: < 500ns per eviction

Benchmark scenarios:
1. Sequential insert of 1M events
2. Random id lookups
3. Filter queries with varying selectivity
4. Mixed read/write workload (90% read, 10% write)
5. Memory usage at 1M events
