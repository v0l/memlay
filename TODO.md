# TODO: Memlay Nostr Relay

## High Priority
- [x] Add Nostr relay protocol support (WebSocket HTTP upgrade)
- [x] Implement message framing (Nostr JSON messages: EVENT, REQ, EOSE, etc.)
- [x] Implement client subscription management (kind filters, e-tags, p-tags)
  - [x] Parse REQ messages with filters
  - [x] Query events by filter criteria with AND logic
  - [x] Query plan optimization (IDs > e-tags > p-tags > authors > kinds)
  - [x] Send back events and EOSE

## Medium Priority
- [ ] Add event persistence layer (optional file/WAL for durability)
- [x] Add HTTP endpoint for relay information (name, description, pubkey, supported_nips)
- [ ] Implement rate limiting per client/connection
- [ ] Implement NIP-01 OK messages for EVENT publication responses

## Low Priority
- [ ] Add metrics/export (connected clients, events stored, throughput)
- [ ] Support NIP-11 (Relay Information Document)
- [ ] Add NIP-42 (Authentication via challenge-response)
- [ ] Implement NIP-51 (Relay Metadata for lists, bookmarks)
- [ ] Add NIP-50 (search filters for content, kinds, pubkeys)
- [x] Add performance benchmarks for evaluating relay implementations
- [ ] Write integration tests for complete relay lifecycle

## Benchmarks Added
- **relay_bench**: Query performance benchmarks (kind, pubkey, since/until filters)
- Multi-filter queries with mixed OR logic
- Concurrent query execution across multiple threads
- Event insertion benchmarks at various load sizes

## NIP References
- [NIP-01](https://github.com/nostr-protocol/nips/blob/master/01.md) - Basic protocol flow description (events, serialization, filtering, message types)
- [NIP-11](https://github.com/nostr-protocol/nips/blob/master/11.md) - Relay Information Document
- [NIP-42](https://github.com/nostr-protocol/nips/blob/master/42.md) - Authentication via challenge-response
- [NIP-50](https://github.com/nostr-protocol/nips/blob/master/50.md) - Search filters
- [NIP-51](https://github.com/nostr-protocol/nips/blob/master/51.md) - Relay Metadata for lists/bookmarks
