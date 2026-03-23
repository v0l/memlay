# Micro-Benchmarks for Query/Response Pipeline

This directory contains micro-benchmarks for each stage of the memlay Nostr relay's query/response pipeline.

## Benchmarks Overview

### 1. Message Parsing
- `message_parse_event`: Parses EVENT messages from JSON
- `message_parse_req`: Parses REQ messages with filters
- `message_parse_req_complex`: Parses REQ with multiple complex filters

### 2. Filter Processing
- `filter_deserialize_simple`: Deserializes simple filters
- `filter_deserialize_complex`: Deserializes complex filters with multiple conditions
- `filter_match_event_matches`: Matches events against filters (positive case)
- `filter_match_event_no_match`: Matches events against filters (negative case)

### 3. Store Queries
- `store_query_by_kind/{10,100,1000}`: Query by event kind with different limits
- `store_query_by_pubkey/{10,100,1000}`: Query by author with different limits
- `store_query_by_e_tag/{10,100,1000}`: Query by referenced event (e-tag)
- `store_query_by_p_tag/{10,100,1000}`: Query by mentioned pubkey (p-tag)
- `store_query_by_tag/{10,100,1000}`: Query by generic tag values
- `store_get_by_id`: Direct event lookup by ID

### 4. Subscription Management
- `subscription_add_remove`: Add and remove subscriptions
- `subscription_query_filter`: Query with single filter
- `subscription_query_multiple_filters`: Query with multiple filters

### 5. End-to-End Pipeline
- `full_request_pipeline`: Full REQ message processing pipeline
- `full_event_ingest_pipeline`: Full EVENT ingestion pipeline

## Running Benchmarks

```bash
# Run all benchmarks
cargo bench --bench pipeline_bench --features unchecked

# Run with HTML report
cargo bench --bench pipeline_bench --features unchecked -- --html

# Run specific benchmark
cargo bench --bench pipeline_bench --features unchecked -- message_parse_event
```

## Performance Highlights

The benchmarks measure performance across the entire pipeline:

1. **Message Parsing**: ~1-6µs per message
2. **Filter Deserialization**: ~170-300ns per filter
3. **Filter Matching**: ~2-100ns per event
4. **Store Queries**: ~10ns per query (cache-friendly)
5. **Full Pipeline**: End-to-end latency including all processing

## Running the Store Benchmarks

The store benchmarks also include more comprehensive tests:

```bash
cargo bench --bench store_bench
cargo bench --bench relay_bench
cargo bench --bench ingest_bench
```
