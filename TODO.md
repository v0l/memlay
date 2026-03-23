# Memlay: Development Roadmap

## Core Protocol (NIP-01)

### Implemented ✅
- WebSocket server with HTTP upgrade
- Message framing (EVENT, REQ, CLOSE, EOSE)
- Subscription management with filters
- Event storage with LRU eviction
- Index query optimization (ids > e-tags > p-tags > authors > kinds)
- OK message support for EVENT publication

### Remaining
- [ ] Rate limiting per connection

## NIP Support

| NIP | Status | Notes |
|-----|--------|-------|
| NIP-01 | ✅ Complete | Core protocol |
| NIP-11 | ✅ Complete | Full relay info document |
| NIP-42 | ⏳ TODO | Authentication (challenge-response) |
| NIP-50 | ⏳ TODO | Search filters |
| NIP-51 | ⏳ TODO | Relay metadata |

## Infrastructure

### Metrics & Observability
- [ ] Connected clients count
- [ ] Events stored count
- [ ] Query throughput (events/sec)
- [ ] Broadcast latency (p50, p95, p99)
- [ ] Memory usage tracking

### Testing
- [ ] Integration tests (complete protocol lifecycle)
- [ ] Load testing scenarios
- [ ] Edge case coverage (malformed messages, concurrent clients)

## Performance Optimizations

### Completed ✅
1. Query result cache (90%+ speedup for repeated queries)
2. HashMap indexes (20% faster lookups vs BTreeMap)
3. Pre-compiled filter hex parsing
4. Optimized query plan selection
5. FIFO eviction (no read-side lock overhead)

### Future Considerations
- Parallel query execution with rayon (multi-core)
- Zero-copy tag storage (memory reduction)
- Connection pooling for persistence layer

---

**Last Updated**: 2026-03-23
**Status**: Core protocol stable. Focus on persistence, auth, and metrics.
