# Memlay: Agentic Development Guide

## Project Overview

**memlay** is a high-performance in-memory Nostr relay written in Rust. It provides:
- WebSocket-based Nostr protocol support (NIP-01)
- HTTP relay information endpoint (NIP-11)
- In-memory event storage with LRU eviction
- Event indexing by pubkey, kind, and tag references
- Subscription management with filter optimization

## Architecture

```
src/
├── main.rs          # CLI parsing, config loading, server startup
├── lib.rs           # Library exports
├── relay.rs         # HTTP/WebSocket routing, connection handling
├── message.rs       # Nostr message types (EVENT, REQ, CLOSE, EOSE, OK, NOTICE)
├── event.rs         # Event parsing, verification, serialization
├── subscription.rs  # Filter parsing, subscription management, query optimization
├── config.rs        # Configuration loading (TOML/YAML/JSON + env vars)
├── store/
│   ├── mod.rs       # Event store with LRU eviction
│   ├── index.rs     # Multi-index event storage (id, pubkey, kind, tags)
│   └── lru.rs       # LRU eviction tracker
```

## Key Design Patterns

### Event Storage
- Events stored in `EventIndex` with multiple indexes for fast querying
- `EventRef` wraps `Arc<Event>` for zero-copy access, sorted by `created_at DESC`
- LRU eviction when `max_events` or `max_bytes` exceeded
- Fine-grained `RwLock` per index for concurrent access

### Query Optimization
Query plan picks most selective index first:
1. **IDs** - exact 32-byte match
2. **e-tags** - referenced event IDs  
3. **p-tags** - referenced pubkeys
4. **Other tags** - generic single-letter tag values
5. **Authors** - event pubkeys
6. **Kinds** - event types

### Filter Logic
- Multiple filters within a single REQ: **AND** logic
- Multiple values within a filter: **OR** logic (e.g., `["kind", 1, 2]`)
- Cross-filter: **AND** logic (must match all specified constraints)

### WebSocket Connection Handling
- Per-connection subscription state stored in `conn_subs`
- Relay-wide broadcast channel (`tx: broadcast::Sender`) for new events
- Events pushed to all matching subscriptions across connections
- EOSE sent after initial query results

## Development Commands

```bash
# Build
cargo build                    # Debug build
cargo build --release          # Release build

# Run
cargo run --release            # Start relay on 0.0.0.0:8080
cargo run --release -- --config custom.toml

# Test
cargo test                     # Run all tests
cargo test --lib               # Unit tests only
cargo test --test e2e_test     # Integration tests only

# Benchmarks
cargo bench                    # Run all benchmarks

# Code quality
cargo clippy                   # Linting
cargo fmt                      # Formatting
cargo doc --open               # Generate docs
```

## Code Conventions

### Error Handling
- Use `thiserror` for custom error types in libraries
- Use `anyhow` for application-level errors
- Propagate errors with `?` operator
- Return descriptive error messages to clients

### Concurrency
- Use `parking_lot::RwLock` for read-heavy workloads
- Use `Arc<T>` for shared state across async tasks
- Use `tokio::sync::broadcast` for event fanout
- Keep lock scopes minimal to avoid contention

### Naming
- Structs: `PascalCase` (e.g., `EventStore`, `SubscriptionManager`)
- Functions: `snake_case` (e.g., `query_by_pubkey`)
- Constants: `SCREAMING_SNAKE_CASE` (e.g., `BROADCAST_CAP`)
- Type aliases: `PascalCase` (e.g., `TagLetter`)

### Documentation
- Public API requires doc comments (`///`)
- Use examples in doc comments when helpful
- Keep inline comments for non-obvious logic

## Nostr Protocol References

### NIP-01 (Core)
- **Event structure**: `{id, pubkey, created_at, kind, tags, content, sig}`
- **Event ID**: SHA-256 of `[0, pubkey, created_at, kind, tags, content]`
- **Message types**:
  - `EVENT` - publish event
  - `REQ` - subscribe with filters
  - `CLOSE` - unsubscribe
  - `EOSE` - end of stored events (server→client)
  - `OK` - event acceptance/rejection
  - `NOTICE` - server message
- **Filters**: `ids`, `authors`, `kinds`, `#e`, `#p`, `#t`, `since`, `until`, `limit`

### NIP-11 (Relay Info)
- HTTP GET `/info` with `Accept: application/nostr+json`
- Response: `{name, description, software, version, supported_nips, limitation}`

### NIP-42 (Authentication)
- Challenge-response authentication
- Server sends `AUTH` challenge
- Client signs challenge and sends `AUTH` event

### Event Kinds
- `0`: Metadata (profile)
- `1`: Text note
- `2`: Recommendation relay
- `3`: Contact list
- `4`: Encrypted direct message
- `6`: Repost
- `7`: Reaction
- `40`: Channel creation
- `41`: Channel metadata
- `42`: Channel message
- `43`: Channel hide message
- `44`: Channel mute user
- `10000-19999`: Regular events
- `10000-19999`: Replaceable events
- `20000-29999`: Ephemeral events
- `30000+`: Parameterized replaceable events

## Testing Guidelines

### Unit Tests
- Place in `mod tests` within source files
- Use `Event::from_json_unchecked` for test event creation
- Test all index types: id, pubkey, kind, e-tags, p-tags, generic tags
- Test edge cases: empty filters, duplicate events, LRU eviction

### Integration Tests
- Located in `tests/e2e_test.rs`
- Use real WebSocket connections
- Test complete protocol flow: connect → subscribe → publish → receive → EOSE
- Test concurrent clients and cross-connection event delivery

### Benchmark Tests
- Located in `benches/`
- Use Criterion.rs
- Test query performance at various data sizes
- Measure insertion throughput
- Test multi-threaded query performance

### Performance Testing Protocol

**ALWAYS run benchmarks between task list improvements:**

```bash
cargo bench  # Run before changes
cargo bench  # Run after changes
```

Run benchmarks after **each individual change**, not after batches of modifications. This isolates which specific changes improve or degrade performance, making it clear what worked and what didn't.

## Common Tasks

### Adding a New NIP
1. Read the NIP specification from https://github.com/nostr-protocol/nips
2. Identify required message types and event formats
3. Add message type to `NostrMessage` enum in `message.rs`
4. Implement handler in `relay.rs` WebSocket loop
5. Add NIP number to `supported_nips` in NIP-11 response
6. Write integration tests
7. Update TODO.md

### Adding a New Index Type
1. Add index field to `EventIndex` struct in `store/index.rs`
2. Initialize in `EventIndex::new()`
3. Update `insert()` to maintain new index
4. Update `remove()` to clean up new index
5. Add query method (e.g., `query_by_foo`)
6. Update subscription query plan in `subscription.rs`
7. Add tests

### Optimizing Query Performance
1. Profile with `cargo bench`
2. Identify hot paths with `tracing` spans
3. Consider:
   - Reducing lock contention (split locks, lock-free structures)
   - Memory layout (packed structs, cache-friendly iteration)
   - Algorithm complexity (hash vs tree lookups)
   - Parallel query execution (crossbeam channels, rayon)
4. Measure improvement

### Adding Configuration Options
1. Add field to `Config` struct in `config.rs`
2. Add default function with `#[serde(default = "default_foo")]
3. Update TOML example in `config.toml`
4. Add environment variable override (`MEMLAY_FOO`)
5. Update NIP-11 response if applicable
6. Document in AGENTS.md

## Performance Considerations

### Memory
- Each event: ~300-500 bytes (depends on content size)
- Index overhead: ~50% additional memory for indexes
- LRU eviction prevents unbounded growth
- Use `bytes::Bytes` for zero-copy JSON storage

### CPU
- Event verification: signature verification is dominant cost
- Index lookups: O(1) for hash maps, O(log n) for B-trees
- Query optimization: pick most selective index first
- Broadcast: O(n) per subscription per event

### Concurrency
- Fine-grained locks reduce contention
- Read-heavy workloads benefit from `RwLock`
- Avoid holding locks across await points
- Use `parking_lot` for better performance than standard library

## Debugging Tips

### Enable Tracing
```bash
RUST_LOG=debug cargo run --release
RUST_LOG=trace cargo test -- --nocapture
```

### Inspect Events
```rust
tracing::debug!(event=?event, "processing event");
tracing::trace!(content=%event.content, "event content");
```

### Performance Profiling
```bash
# Use cargo flamegraph
cargo install cargo-flamegraph
cargo flamegraph --bench store_bench

# Use samply (macOS)
samply record cargo bench
```

### WebSocket Debugging
```bash
# Connect with websocat
websocat ws://localhost:8080/

# Use nostr CLI tools
nostr-cli relay ws://localhost:8080/
```

## File Structure Reference

```memlay/
├── AGENTS.md                    # This file
├── Cargo.toml                   # Dependencies and build config
├── README.md                    # User-facing documentation
├── TODO.md                      # Feature tracking
├── config.toml                  # Default configuration
├── src/
│   ├── main.rs                  # Binary entry point
│   ├── lib.rs                   # Library exports
│   ├── relay.rs                 # HTTP/WebSocket server
│   ├── message.rs               # Nostr message parsing
│   ├── event.rs                 # Event parsing and verification
│   ├── subscription.rs          # Subscription and filter logic
│   ├── config.rs                # Configuration loading
│   ├── index.html               # Web UI (template)
│   └── store/
│       ├── mod.rs               # Event store with LRU
│       ├── index.rs             # Multi-index implementation
│       └── lru.rs               # LRU eviction tracker
├── benches/
│   ├── store_bench.rs           # Index performance
│   ├── relay_bench.rs           # Query performance
│   ├── ingest_bench.rs          # Insertion throughput
│   └── pipeline_bench.rs        # End-to-end pipeline
├── tests/
│   └── e2e_test.rs              # Integration tests
└── .opencode/
    └── plans/                   # Agent development plans
```

## Quick Reference

### Build & Run
```bash
cargo build --release && ./target/release/memlay --config config.toml
```

### Add dependencies
Always use `cargo add` to ensure latest compatible versions:
```bash
cargo add <crate_name>  # Always use cargo add to ensure latest compatible versions
```

### Test Everything
```bash
cargo test && cargo clippy && cargo fmt --check
```

### Start Development
```bash
RUST_LOG=debug cargo run --release
```

### Check API
```bash
curl -H "Accept: application/nostr+json" http://localhost:8080/
curl http://localhost:8080/stats
```

## NIP-01 Filter Syntax Examples

```json
// Get last 10 notes from author
{"authors": ["pubkey_hex"], "kinds": [1], "limit": 10}

// Get events referencing another event
{"#e": ["event_id_hex"]}

// Get events from multiple authors
{"authors": ["pubkey1", "pubkey2"]}

// Get events in time range
{"kinds": [1], "since": 1234567890, "until": 1234577890}

// Get specific events by ID prefix
{"ids": ["event_id_prefix"]}

// Multiple filters (OR within, AND across)
[{"kinds": [1]}, {"kinds": [6]}]  // notes OR reposts
```

## Decision Log

### Why BTreeMap over HashMap for secondary indexes?
- Need sorted results (by `created_at DESC`)
- BTreeMap provides ordered iteration
- O(log n) lookup acceptable for secondary indexes

### Why `Arc<Event>` instead of copying?
- Zero-copy serialization (store raw JSON)
- Efficient memory usage for shared references
- Clone is cheap (reference count increment)

### Why broadcast channel for event delivery?
- One-to-many fanout to all connections
- Simple implementation
- Backpressure via `lagged` error detection

### Why separate per-connection subscription state?
- Connections can have multiple subscriptions
- Isolation between connections
- Clean disconnect handling
