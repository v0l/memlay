# Memlay NIP References

## NIP-01: Basic protocol flow description
https://github.com/nostr-protocol/nips/blob/master/01.md

- Events: JSON with id, pubkey, created_at, kind, tags, content, sig
- Event id = sha256 of serialized event data
- Serialization: `[0, pubkey, created_at, kind, tags, content]`
- Tags: `e`, `p`, `a` (standard), all single-letter keys should be indexed
- Kinds: 0 (metadata), 1 (text), 2 (contact list), 3 (meta), 4+ (regular), 10000+ (replaceable), 20000+ (ephemeral), 30000+ (addressable)
- Messages: EVENT (publish), REQ (subscribe), CLOSE (unsubscribe)
- Filters: ids, authors, kinds, #e, #p, since, until, limit (AND within filter, OR between multiple filters)
- Responses: EVENT (with sub_id), OK (for events), EOSE (end of stored events), CLOSED, NOTICE
- Filter attributes with arrays: at least one value must match (OR logic)
- Scalar filters: must be contained in filter list (OR logic between tags)
- Multiple filters: OR conditions between them
