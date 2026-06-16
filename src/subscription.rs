use crate::event::Event;
use crate::store::EventStore;
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

/// Trait for filter matching against an event
pub trait FilterMatch {
    /// Returns true if the event matches this filter
    fn matches_event(&self, event: &Event) -> bool;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub filters: Vec<Filter>,
}

/// Pre-parsed 32-byte hex value that serializes/deserializes as hex string.
#[derive(Debug, Clone, Copy, Default, Hash, PartialEq, Eq)]
pub struct Hex32(pub [u8; 32]);

impl Hex32 {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn as_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn starts_with(&self, prefix: &str) -> bool {
        if prefix.len() > 64 {
            return false;
        }
        let hex = self.as_hex();
        hex.starts_with(prefix)
    }
}

impl From<&str> for Hex32 {
    fn from(s: &str) -> Self {
        let bytes = hex::decode(s).expect("invalid hex string");
        if bytes.len() != 32 {
            panic!("expected 32 bytes, got {}", bytes.len());
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Hex32(arr)
    }
}

impl From<String> for Hex32 {
    fn from(s: String) -> Self {
        s.as_str().into()
    }
}

impl Serialize for Hex32 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Hex32 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Hex32Visitor;

        impl<'de> Visitor<'de> for Hex32Visitor {
            type Value = Hex32;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a 64-character hex string")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Hex32, E> {
                let bytes = hex::decode(v).map_err(de::Error::custom)?;
                if bytes.len() != 32 {
                    return Err(de::Error::custom("expected 32 bytes"));
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Ok(Hex32(arr))
            }
        }

        d.deserialize_str(Hex32Visitor)
    }
}

/// NIP-01 subscription filter.
///
/// Tag filters are stored in `tag_filters` keyed by the single-letter tag name.
/// On the wire they appear as `#e`, `#p`, `#t`, etc.  The dedicated `e` and `p`
/// entries are backed by fast 32-byte indexes; every other letter uses the
/// generic string index.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Event IDs (stored as parsed bytes, serialized as hex strings)
    pub ids: Option<Vec<Hex32>>,
    pub kinds: Option<Vec<u32>>,
    /// Authors (stored as parsed bytes, serialized as hex strings)
    pub authors: Option<Vec<Hex32>>,
    /// Keyed by single-letter tag name (e.g. `'e'`, `'p'`, `'t'`).
    /// For 'e' and 'p' tags, values are also stored as bytes in e_tags_bytes/p_tags_bytes.
    pub tag_filters: HashMap<char, Vec<String>>,
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<usize>,
    /// Pre-parsed e-tag values (32-byte event IDs)
    pub e_tags_bytes: Option<Vec<Hex32>>,
    /// Pre-parsed p-tag values (32-byte pubkeys)
    pub p_tags_bytes: Option<Vec<Hex32>>,
}

impl Filter {
    /// Convenience accessor for `#e` tag filter values.
    pub fn e_tags(&self) -> Option<&Vec<String>> {
        self.tag_filters.get(&'e')
    }

    /// Convenience accessor for `#p` tag filter values.
    pub fn p_tags(&self) -> Option<&Vec<String>> {
        self.tag_filters.get(&'p')
    }

    /// Pre-parse hex filter values to bytes for fast lookups.
    /// This converts e-tag and p-tag string values to Hex32 for fast index lookups.
    pub fn parse_hex_values(&mut self) {
        // Parse e-tags
        if let Some(e_vals) = self.tag_filters.get(&'e') {
            let e_bytes: Vec<Hex32> = e_vals
                .iter()
                .filter_map(|val| {
                    let bytes = hex::decode(val).ok()?;
                    if bytes.len() != 32 {
                        return None;
                    }
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&bytes);
                    Some(Hex32(arr))
                })
                .collect();
            if !e_bytes.is_empty() {
                self.e_tags_bytes = Some(e_bytes);
            }
        }

        // Parse p-tags
        if let Some(p_vals) = self.tag_filters.get(&'p') {
            let p_bytes: Vec<Hex32> = p_vals
                .iter()
                .filter_map(|val| {
                    let bytes = hex::decode(val).ok()?;
                    if bytes.len() != 32 {
                        return None;
                    }
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&bytes);
                    Some(Hex32(arr))
                })
                .collect();
            if !p_bytes.is_empty() {
                self.p_tags_bytes = Some(p_bytes);
            }
        }
    }
}

// ── Custom Serialize ────────────────────────────────────────────────────────

impl Serialize for Filter {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // Count non-None / non-empty fields so we can size the map.
        let tag_count = self.tag_filters.len();
        let fixed = [
            self.ids.is_some(),
            self.kinds.is_some(),
            self.authors.is_some(),
            self.since.is_some(),
            self.until.is_some(),
            self.limit.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        let mut map = s.serialize_map(Some(fixed + tag_count))?;
        if let Some(v) = &self.ids {
            map.serialize_entry("ids", v)?;
        }
        if let Some(v) = &self.kinds {
            map.serialize_entry("kinds", v)?;
        }
        if let Some(v) = &self.authors {
            map.serialize_entry("authors", v)?;
        }
        for (letter, values) in &self.tag_filters {
            let key = format!("#{letter}");
            map.serialize_entry(&key, values)?;
        }
        if let Some(v) = self.since {
            map.serialize_entry("since", &v)?;
        }
        if let Some(v) = self.until {
            map.serialize_entry("until", &v)?;
        }
        if let Some(v) = self.limit {
            map.serialize_entry("limit", &v)?;
        }
        map.end()
    }
}

// ── Custom Deserialize ───────────────────────────────────────────────────────

impl<'de> Deserialize<'de> for Filter {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct FilterVisitor;

        impl<'de> Visitor<'de> for FilterVisitor {
            type Value = Filter;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a NIP-01 filter object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Filter, A::Error> {
                let mut filter = Filter::default();

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "ids" => filter.ids = Some(map.next_value()?),
                        "kinds" => filter.kinds = Some(map.next_value()?),
                        "authors" => filter.authors = Some(map.next_value()?),
                        "since" => filter.since = Some(map.next_value()?),
                        "until" => filter.until = Some(map.next_value()?),
                        "limit" => filter.limit = Some(map.next_value()?),
                        k if k.starts_with('#') => {
                            let letter = k[1..]
                                .chars()
                                .next()
                                .ok_or_else(|| de::Error::custom("empty tag filter key"))?;
                            if k[1..].chars().count() == 1 {
                                let values: Vec<String> = map.next_value()?;
                                filter.tag_filters.insert(letter, values);
                            } else {
                                // multi-char after '#' – skip
                                let _ = map.next_value::<serde_json::Value>()?;
                            }
                        }
                        _ => {
                            // unknown field – skip
                            let _ = map.next_value::<serde_json::Value>()?;
                        }
                    }
                }
                Ok(filter)
            }
        }

        d.deserialize_map(FilterVisitor)
    }
}

pub struct SubscriptionManager {
    subscriptions: parking_lot::RwLock<HashMap<String, Subscription>>,
    pub store: Arc<EventStore>,
}

impl SubscriptionManager {
    pub fn new(store: Arc<EventStore>) -> Self {
        Self {
            subscriptions: parking_lot::RwLock::new(HashMap::new()),
            store,
        }
    }

    pub fn add_subscription(&self, mut sub: Subscription) {
        let sub_id = sub.id.clone();
        let mut subs = self.subscriptions.write();

        // Pre-parse e-tag and p-tag values once at subscription time
        for filter in &mut sub.filters {
            filter.parse_hex_values();
        }

        subs.insert(sub_id, sub);
    }

    pub fn remove_subscription(&self, id: &str) {
        let mut subs = self.subscriptions.write();
        subs.remove(id);
    }

    pub fn query_subscriptions(&self) -> Vec<Arc<Event>> {
        let subs = self.subscriptions.read();

        let mut all_events = Vec::new();

        for sub in subs.values() {
            for filter in &sub.filters {
                let events = self.query_filter(filter);
                all_events.extend(events);
            }
        }

        all_events
    }

    pub fn query_filter(&self, filter: &Filter) -> Vec<Arc<Event>> {
        if filter.kinds.is_none()
            && filter.authors.is_none()
            && filter.tag_filters.is_empty()
            && filter.since.is_none()
            && filter.until.is_none()
            && filter.ids.is_none()
        {
            return Vec::new();
        }

        self.query_filter_internal(filter)
    }

    fn query_filter_internal(&self, filter: &Filter) -> Vec<Arc<Event>> {
        let mut results: Vec<Arc<Event>> = Vec::new();
        let mut result_ids: HashSet<[u8; 32]> = HashSet::new();
        let fetch_limit = filter.limit.unwrap_or(usize::MAX);

        // Pre-allocate with hint
        let mut candidates: Vec<Arc<Event>> = Vec::with_capacity(32);

        // Determine the best index to start with based on selectivity
        // Priority: ids (most selective) > e-tags > p-tags > generic tags > authors > kinds (least selective)
        let use_ids = filter.ids.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
        let use_e_tags = filter
            .e_tags_bytes
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false)
            || filter.e_tags().map(|v| !v.is_empty()).unwrap_or(false);
        let use_p_tags = filter
            .p_tags_bytes
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false)
            || filter.p_tags().map(|v| !v.is_empty()).unwrap_or(false);
        let has_generic_tags = filter
            .tag_filters
            .iter()
            .any(|(&l, v)| l != 'e' && l != 'p' && !v.is_empty());
        let use_authors = filter
            .authors
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let use_kinds = filter
            .kinds
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        // Fetch from most selective index first
        if use_ids {
            if let Some(ids) = &filter.ids {
                for id in ids {
                    if let Some(event) = self.store.get(id.as_bytes())
                        && filter.since.is_none_or(|l| event.created_at >= l)
                        && filter.until.is_none_or(|u| event.created_at <= u)
                    {
                        candidates.push(event);
                    }
                }
            }
        } else if use_e_tags {
            if let Some(e_bytes) = &filter.e_tags_bytes {
                for bytes in e_bytes {
                    candidates.extend(self.store.query_by_e_tag(bytes.as_bytes(), fetch_limit));
                }
            } else if let Some(e_vals) = filter.e_tags() {
                // Fallback: parse e-tag strings on-demand (for direct filter usage like in tests)
                for val in e_vals {
                    if let Ok(b) = hex::decode(val)
                        && b.len() == 32
                    {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&b);
                        candidates.extend(self.store.query_by_e_tag(&arr, fetch_limit));
                    }
                }
            }
        } else if use_p_tags {
            if let Some(p_bytes) = &filter.p_tags_bytes {
                for bytes in p_bytes {
                    candidates.extend(self.store.query_by_p_tag(bytes.as_bytes(), fetch_limit));
                }
            } else if let Some(p_vals) = filter.p_tags() {
                // Fallback: parse p-tag strings on-demand
                for val in p_vals {
                    if let Ok(b) = hex::decode(val)
                        && b.len() == 32
                    {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&b);
                        candidates.extend(self.store.query_by_p_tag(&arr, fetch_limit));
                    }
                }
            }
        } else if has_generic_tags {
            if let Some(letter) = filter
                .tag_filters
                .keys()
                .find(|&l| *l != 'e' && *l != 'p')
                .copied()
            {
                if let Some(values) = filter.tag_filters.get(&letter) {
                    for val in values {
                        candidates.extend(self.store.query_by_tag(letter, val, fetch_limit));
                    }
                }
            }
        } else if use_authors {
            if let Some(authors) = &filter.authors {
                for author in authors {
                    candidates.extend(self.store.query_by_pubkey(author.as_bytes(), fetch_limit));
                }
            } else if let Some(p_vals) = filter.p_tags() {
                for val in p_vals {
                    if let Ok(b) = hex::decode(val)
                        && b.len() == 32
                    {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&b);
                        candidates.extend(self.store.query_by_pubkey(&arr, fetch_limit));
                    }
                }
            }
        } else if use_kinds {
            if let Some(kinds) = &filter.kinds {
                for kind in kinds {
                    candidates.extend(self.store.query_by_kind(*kind, fetch_limit));
                }
            }
        }

        // Pre-compute author set for filtering if needed
        let author_set: Option<HashSet<[u8; 32]>> = filter
            .authors
            .as_ref()
            .map(|authors| authors.iter().map(|a| *a.as_bytes()).collect());

        // Apply AND filters: since, until, kinds, authors, and non-primary tag filters
        for event in candidates {
            let matches = filter.since.is_none_or(|s| event.created_at >= s)
                && filter.until.is_none_or(|u| event.created_at <= u)
                && filter
                    .kinds
                    .as_ref()
                    .is_none_or(|kinds| kinds.contains(&event.kind))
                && author_set
                    .as_ref()
                    .is_none_or(|authors| authors.contains(&event.pubkey))
                && filter.tag_filters.iter().all(|(&letter, values)| {
                    // Skip the primary tag index we already queried
                    if use_ids {
                        return true;
                    }
                    if use_e_tags && letter == 'e' {
                        return true;
                    }
                    if use_p_tags && letter == 'p' {
                        return true;
                    }
                    if has_generic_tags && letter != 'e' && letter != 'p' {
                        return true;
                    }
                    event.tags.iter().any(|tag| {
                        let mut chars = tag.name.chars();
                        matches!((chars.next(), chars.next()), (Some(l), None) if l == letter)
                            && tag.value().is_some_and(|v| values.iter().any(|fv| fv == v))
                    })
                });

            if matches && result_ids.insert(event.id) {
                results.push(event);
            }
        }

        if let Some(lim) = filter.limit {
            results.truncate(lim);
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use crate::store::StoreConfig;
    use std::sync::Arc;

    fn make_event(id: u8, pubkey: u8, kind: u32, created_at: u64) -> Arc<Event> {
        let json = format!(
            r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[],"content":"test","sig":"{:0>128}"}}"#,
            format!("{:x}", id),
            format!("{:x}", pubkey),
            created_at,
            kind,
            "0"
        );
        Arc::new(Event::from_json_unchecked(json.as_bytes()).unwrap())
    }

    fn make_event_with_raw_tags(
        id: u8,
        pubkey: u8,
        kind: u32,
        created_at: u64,
        raw_tags: &[&str],
    ) -> Arc<Event> {
        let tags_json = raw_tags.join(",");
        let json = format!(
            r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[{}],"content":"test","sig":"{:0>128}"}}"#,
            format!("{:x}", id),
            format!("{:x}", pubkey),
            created_at,
            kind,
            tags_json,
            "0"
        );
        Arc::new(Event::from_json_unchecked(json.as_bytes()).unwrap())
    }

    fn make_event_with_tags(
        id: u8,
        pubkey: u8,
        kind: u32,
        created_at: u64,
        e_tags: &[u8],
        p_tags: &[u8],
    ) -> Arc<Event> {
        let raw: Vec<String> = e_tags
            .iter()
            .map(|e| format!(r#"["e","{}"]"#, format!("{:0>64}", format!("{:x}", e))))
            .chain(
                p_tags
                    .iter()
                    .map(|p| format!(r#"["p","{}"]"#, format!("{:0>64}", format!("{:x}", p)))),
            )
            .collect();
        let raw_refs: Vec<&str> = raw.iter().map(|s| s.as_str()).collect();
        make_event_with_raw_tags(id, pubkey, kind, created_at, &raw_refs)
    }

    fn tag_filter(letter: char, values: Vec<String>) -> Filter {
        let mut f = Filter::default();
        f.tag_filters.insert(letter, values);
        f
    }

    #[test]
    fn test_query_empty_filter() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        let filter = Filter::default();
        let results = sm.query_filter(&filter);
        assert!(results.is_empty());
    }

    #[test]
    fn test_query_by_kind() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event(1, 1, 1, 1000));
        sm.store.insert(make_event(2, 1, 1, 2000));
        sm.store.insert(make_event(3, 1, 2, 3000));

        let filter = Filter {
            kinds: Some(vec![1]),
            ..Default::default()
        };

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].created_at, 2000);
        assert_eq!(results[1].created_at, 1000);
    }

    #[test]
    fn test_query_by_author() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event(1, 1, 1, 1000));
        sm.store.insert(make_event(2, 1, 1, 2000));
        sm.store.insert(make_event(3, 2, 1, 3000));

        let mut author = [0u8; 32];
        author[31] = 1;

        let filter = Filter {
            authors: Some(vec![Hex32::from(hex::encode(author))]),
            ..Default::default()
        };

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_query_by_id() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        let event1 = make_event(1, 1, 1, 1000);
        let event2 = make_event(2, 1, 1, 2000);
        sm.store.insert(event1.clone());
        sm.store.insert(event2);

        let filter = Filter {
            ids: Some(vec![Hex32::from(hex::encode(event1.id))]),
            ..Default::default()
        };

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, event1.id);
    }

    #[test]
    fn test_query_and_kind_author() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event(1, 1, 1, 1000));
        sm.store.insert(make_event(2, 2, 1, 2000));
        sm.store.insert(make_event(3, 1, 2, 3000));

        let mut author = [0u8; 32];
        author[31] = 1;

        let filter = Filter {
            kinds: Some(vec![1]),
            authors: Some(vec![Hex32::from(hex::encode(author))]),
            ..Default::default()
        };

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id[31], 1);
    }

    #[test]
    fn test_query_and_kind_author_p_tag() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store
            .insert(make_event_with_tags(1, 1, 1, 1000, &[], &[3]));
        sm.store
            .insert(make_event_with_tags(2, 1, 1, 2000, &[], &[4]));
        sm.store
            .insert(make_event_with_tags(3, 2, 1, 3000, &[], &[3]));

        let mut author = [0u8; 32];
        author[31] = 1;
        let mut p_tag = [0u8; 32];
        p_tag[31] = 3;

        let mut filter = Filter {
            kinds: Some(vec![1]),
            authors: Some(vec![Hex32::from(hex::encode(author))]),
            ..Default::default()
        };
        filter.tag_filters.insert('p', vec![hex::encode(p_tag)]);

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id[31], 1);
    }

    #[test]
    fn test_query_since_until() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event(1, 1, 1, 1000));
        sm.store.insert(make_event(2, 1, 1, 2000));
        sm.store.insert(make_event(3, 1, 1, 3000));

        let filter = Filter {
            kinds: Some(vec![1]),
            since: Some(1500),
            until: Some(2500),
            ..Default::default()
        };

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].created_at, 2000);
    }

    #[test]
    fn test_query_e_tag() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        let event1 = make_event(1, 1, 1, 1000);
        let e_ref = format!(r#"["e","{}"]"#, hex::encode(event1.id));
        let event2 = make_event_with_raw_tags(2, 1, 1, 2000, &[&e_ref]);
        sm.store.insert(event1);
        sm.store.insert(event2.clone());

        let mut e_tag = [0u8; 32];
        e_tag[31] = 1;

        let filter = tag_filter('e', vec![hex::encode(e_tag)]);

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, event2.id);
    }

    #[test]
    fn test_query_limit() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event(1, 1, 1, 1000));
        sm.store.insert(make_event(2, 1, 1, 2000));
        sm.store.insert(make_event(3, 1, 1, 3000));

        let filter = Filter {
            kinds: Some(vec![1]),
            limit: Some(2),
            ..Default::default()
        };

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_query_multiple_authors() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event(1, 1, 1, 1000));
        sm.store.insert(make_event(2, 2, 1, 2000));
        sm.store.insert(make_event(3, 3, 1, 3000));

        let mut author1 = [0u8; 32];
        author1[31] = 1;
        let mut author2 = [0u8; 32];
        author2[31] = 2;

        let filter = Filter {
            authors: Some(vec![
                Hex32::from(hex::encode(author1)),
                Hex32::from(hex::encode(author2)),
            ]),
            ..Default::default()
        };

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_query_multiple_kinds() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event(1, 1, 1, 1000));
        sm.store.insert(make_event(2, 1, 2, 2000));
        sm.store.insert(make_event(3, 1, 3, 3000));

        let filter = Filter {
            kinds: Some(vec![1, 2]),
            ..Default::default()
        };

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_query_p_tag() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        let event1 = make_event_with_tags(1, 2, 1, 1000, &[], &[1]);
        let event2 = make_event_with_tags(2, 2, 1, 2000, &[], &[2]);

        sm.store.insert(event1);
        sm.store.insert(event2);

        let mut p_tag = [0u8; 32];
        p_tag[31] = 1;

        let filter = tag_filter('p', vec![hex::encode(p_tag)]);

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id[31], 1);
    }

    #[test]
    fn test_query_generic_t_tag() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event_with_raw_tags(
            1,
            1,
            1,
            1000,
            &[r#"["t","nostr"]"#],
        ));
        sm.store.insert(make_event_with_raw_tags(
            2,
            1,
            1,
            2000,
            &[r#"["t","bitcoin"]"#],
        ));
        sm.store.insert(make_event_with_raw_tags(
            3,
            1,
            1,
            3000,
            &[r#"["t","nostr"]"#, r#"["t","bitcoin"]"#],
        ));

        let filter = tag_filter('t', vec!["nostr".to_string()]);
        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 2);
        // both event 1 and 3 have #t=nostr
        let mut ids: Vec<u8> = results.iter().map(|e| e.id[31]).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn test_repeated_query_returns_same_results() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event(1, 1, 1, 1000));
        sm.store.insert(make_event(2, 1, 1, 2000));
        sm.store.insert(make_event(3, 1, 2, 3000));

        let filter = Filter {
            kinds: Some(vec![1]),
            limit: Some(10),
            ..Default::default()
        };

        let results1 = sm.query_filter(&filter);
        assert_eq!(results1.len(), 2);

        // Querying again returns identical results
        let results2 = sm.query_filter(&filter);
        assert_eq!(results2.len(), 2);
        assert_eq!(results1[0].id, results2[0].id);
        assert_eq!(results1[1].id, results2[1].id);
    }

    #[test]
    fn test_new_event_visible_in_subsequent_query() {
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        sm.store.insert(make_event(1, 1, 1, 1000));
        sm.store.insert(make_event(2, 1, 1, 2000));

        let filter = Filter {
            kinds: Some(vec![1]),
            limit: Some(10),
            ..Default::default()
        };

        let results1 = sm.query_filter(&filter);
        assert_eq!(results1.len(), 2);

        let new_event = make_event(3, 1, 1, 3000);
        let event_id = new_event.id;
        sm.store.insert(new_event);

        // A newly inserted event is immediately visible (no stale cache)
        let results2 = sm.query_filter(&filter);
        assert_eq!(results2.len(), 3);
        let ids: Vec<[u8; 32]> = results2.iter().map(|e| e.id).collect();
        assert!(ids.contains(&event_id));
    }

    #[test]
    fn test_query_respects_varying_limits() {
        // Regression guard: the removed cache ignored `limit`, so the same
        // filter shape with different limits could return wrong-sized results.
        let store = EventStore::new(StoreConfig::default());
        let sm = SubscriptionManager::new(Arc::new(store));

        for i in 0..5u8 {
            sm.store.insert(make_event(i + 1, 1, 1, 1000 + i as u64));
        }

        let small = Filter {
            kinds: Some(vec![1]),
            limit: Some(2),
            ..Default::default()
        };
        let large = Filter {
            kinds: Some(vec![1]),
            limit: Some(10),
            ..Default::default()
        };

        assert_eq!(sm.query_filter(&small).len(), 2);
        assert_eq!(sm.query_filter(&large).len(), 5);
    }
}

impl FilterMatch for Filter {
    fn matches_event(&self, event: &Event) -> bool {
        if let Some(since) = self.since
            && event.created_at < since
        {
            return false;
        }
        if let Some(until) = self.until
            && event.created_at > until
        {
            return false;
        }
        if let Some(kinds) = &self.kinds
            && !kinds.contains(&event.kind)
        {
            return false;
        }
        if let Some(ids) = &self.ids {
            if !ids.iter().any(|id| &event.id == id.as_bytes()) {
                return false;
            }
        }
        if let Some(authors) = &self.authors {
            if !authors.iter().any(|a| &event.pubkey == a.as_bytes()) {
                return false;
            }
        }
        if !self.tag_filters.is_empty() {
            for (&letter, values) in &self.tag_filters {
                let matched = event.tags.iter().any(|tag| {
                    let mut chars = tag.name.chars();
                    matches!((chars.next(), chars.next()), (Some(l), None) if l == letter)
                        && tag.value().is_some_and(|v| values.iter().any(|fv| fv == v))
                });
                if !matched {
                    return false;
                }
            }
        }
        true
    }
}
