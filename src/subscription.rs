use crate::event::Event;
use crate::store::EventStore;
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub filters: Vec<Filter>,
}

/// NIP-01 subscription filter.
///
/// Tag filters are stored in `tag_filters` keyed by the single-letter tag name.
/// On the wire they appear as `#e`, `#p`, `#t`, etc.  The dedicated `e` and `p`
/// entries are backed by fast 32-byte indexes; every other letter uses the
/// generic string index.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub ids: Option<Vec<String>>,
    pub kinds: Option<Vec<u32>>,
    pub authors: Option<Vec<String>>,
    /// Keyed by single-letter tag name (e.g. `'e'`, `'p'`, `'t'`).
    pub tag_filters: HashMap<char, Vec<String>>,
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<usize>,
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

    pub fn add_subscription(&self, sub: Subscription) {
        let mut subs = self.subscriptions.write();
        subs.insert(sub.id.clone(), sub);
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

        let mut results: Vec<Arc<Event>> = Vec::new();
        let mut result_ids: HashSet<[u8; 32]> = HashSet::new();
        // Use the filter's limit as the index fetch cap when set; otherwise
        // fetch everything — the explicit truncation at the end handles it.
        let fetch_limit = filter.limit.unwrap_or(usize::MAX);

        // Priority order: IDs > #e > #p > other tags > authors > kinds
        let mut candidates: Vec<Arc<Event>> = Vec::new();

        if let Some(ids) = &filter.ids {
            for id in ids {
                if let Ok(event_id) = hex::decode(id) {
                    if event_id.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&event_id);
                        if let Some(event) = self.store.get(&arr) {
                            if filter.since.map_or(true, |s| event.created_at >= s)
                                && filter.until.map_or(true, |u| event.created_at <= u)
                            {
                                candidates.push(event);
                            }
                        }
                    }
                }
            }
        } else if let Some(e_vals) = filter.tag_filters.get(&'e') {
            for val in e_vals {
                if let Ok(bytes) = hex::decode(val) {
                    if bytes.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&bytes);
                        candidates.extend(self.store.query_by_e_tag(&arr, fetch_limit));
                    }
                }
            }
        } else if let Some(p_vals) = filter.tag_filters.get(&'p') {
            for val in p_vals {
                if let Ok(bytes) = hex::decode(val) {
                    if bytes.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&bytes);
                        candidates.extend(self.store.query_by_p_tag(&arr, fetch_limit));
                    }
                }
            }
        } else if let Some((&letter, values)) = filter
            .tag_filters
            .iter()
            .find(|&(&l, _)| l != 'e' && l != 'p')
        {
            for val in values {
                candidates.extend(self.store.query_by_tag(letter, val, fetch_limit));
            }
        } else if let Some(authors) = &filter.authors {
            for author in authors {
                if let Ok(pk) = hex::decode(author) {
                    if pk.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&pk);
                        candidates.extend(self.store.query_by_pubkey(&arr, fetch_limit));
                    }
                }
            }
        } else if let Some(kinds) = &filter.kinds {
            for kind in kinds {
                candidates.extend(self.store.query_by_kind(*kind, fetch_limit));
            }
        }

        // Intersect with all remaining filter criteria (AND logic)
        for event in candidates {
            let matches = filter.since.map_or(true, |s| event.created_at >= s)
                && filter.until.map_or(true, |u| event.created_at <= u)
                && filter
                    .kinds
                    .as_ref()
                    .map_or(true, |kinds| kinds.contains(&event.kind))
                && filter.authors.as_ref().map_or(true, |authors| {
                    authors.iter().any(|a| {
                        hex::decode(a)
                            .map_or(false, |pk| pk.len() == 32 && pk.as_slice() == event.pubkey)
                    })
                })
                && filter.tag_filters.iter().all(|(&letter, values)| {
                    // For every tag constraint ALL specified letters must match (AND).
                    // Within each letter the event must have at least one matching value (OR).
                    event.tags.iter().any(|tag| {
                        let mut chars = tag.name.chars();
                        let matches_letter = matches!(
                            (chars.next(), chars.next()),
                            (Some(l), None) if l == letter
                        );
                        matches_letter
                            && tag
                                .value()
                                .map_or(false, |v| values.iter().any(|fv| fv == v))
                    })
                });

            if matches {
                if result_ids.insert(event.id) {
                    results.push(event);
                }
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
            authors: Some(vec![hex::encode(author)]),
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
            ids: Some(vec![hex::encode(event1.id)]),
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
            authors: Some(vec![hex::encode(author)]),
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
            authors: Some(vec![hex::encode(author)]),
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
            authors: Some(vec![hex::encode(author1), hex::encode(author2)]),
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
}
