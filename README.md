# memlay

High Performance In-Memory Nostr Relay

## Features

- WebSocket-based Nostr relay protocol support on `/`
- Nostr message framing (EVENT, REQ, EOSE, NOTICE, OK, ERROR, CLOSED)
- Event parsing from Nostr JSON
- In-memory event storage with LRU eviction
- Event indexing by pubkey, kind, and tag references
- Subscription management with filters (kinds, authors, e-tags, p-tags, since/until)
- HTTP information endpoint at `/info`
- Integration tests

## Query Plan Optimization

The subscription manager uses a query plan that picks the most selective index first:
1. **IDs** - exact match, most selective
2. **e-tags** - referenced events
3. **p-tags** - referenced pubkeys  
4. **authors** - event creators
5. **kinds** - event types

All filters within a single REQ are combined with AND logic.

## Building

```bash
cargo build --release
```

## Running

```bash
cargo run --release
```

The relay will start on `127.0.0.1:8080` by default.

### HTTP Endpoints

- `/` - WebSocket upgrade for Nostr protocol
- `/info` - Relay information (name, description, supported NIPs)

## Testing

```bash
cargo test
```

## Example Usage

```bash
cargo run --release
```

Then connect with a Nostr client using WebSocket to `ws://localhost:8080/`
