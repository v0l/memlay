use crate::event::Event;
use crate::store::EventStore;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub filters: Vec<Filter>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filter {
    pub ids: Option<Vec<String>>,
    pub kinds: Option<Vec<u32>>,
    pub authors: Option<Vec<String>>,
    pub e_tags: Option<Vec<String>>,
    pub p_tags: Option<Vec<String>>,
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<usize>,
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
            && filter.e_tags.is_none()
            && filter.p_tags.is_none()
            && filter.since.is_none()
            && filter.until.is_none()
            && filter.ids.is_none()
        {
            return Vec::new();
        }

        let mut results: Vec<Arc<Event>> = Vec::new();
        let mut result_ids: HashSet<[u8; 32]> = HashSet::new();

        // Priority order: IDs > e-tags > p-tags > authors > kinds
        // First get candidates from the most selective index
        let mut candidates: Vec<Arc<Event>> = Vec::new();

        if let Some(ids) = &filter.ids {
            for id in ids {
                if let Ok(event_id) = hex::decode(id) {
                    if event_id.len() == 32 {
                        let mut event_id_arr = [0u8; 32];
                        event_id_arr.copy_from_slice(&event_id);
                        if let Some(event) = self.store.get(&event_id_arr) {
                            if filter.since.map_or(true, |s| event.created_at >= s)
                                && filter.until.map_or(true, |u| event.created_at <= u)
                            {
                                candidates.push(event);
                            }
                        }
                    }
                }
            }
        } else if let Some(e_tags) = &filter.e_tags {
            for e_tag in e_tags {
                if let Ok(event_id) = hex::decode(e_tag) {
                    if event_id.len() == 32 {
                        let mut event_id_arr = [0u8; 32];
                        event_id_arr.copy_from_slice(&event_id);
                        let events = self
                            .store
                            .query_by_e_tag(&event_id_arr, filter.limit.unwrap_or(100));
                        candidates.extend(events);
                    }
                }
            }
        } else if let Some(p_tags) = &filter.p_tags {
            for p_tag in p_tags {
                if let Ok(pubkey) = hex::decode(p_tag) {
                    if pubkey.len() == 32 {
                        let mut pubkey_arr = [0u8; 32];
                        pubkey_arr.copy_from_slice(&pubkey);
                        let events = self
                            .store
                            .query_by_p_tag(&pubkey_arr, filter.limit.unwrap_or(100));
                        candidates.extend(events);
                    }
                }
            }
        } else if let Some(authors) = &filter.authors {
            for author in authors {
                if let Ok(pubkey) = hex::decode(author) {
                    if pubkey.len() == 32 {
                        let mut pubkey_arr = [0u8; 32];
                        pubkey_arr.copy_from_slice(&pubkey);
                        let events = self
                            .store
                            .query_by_pubkey(&pubkey_arr, filter.limit.unwrap_or(100));
                        candidates.extend(events);
                    }
                }
            }
        } else if let Some(kinds) = &filter.kinds {
            for kind in kinds {
                let events = self.store.query_by_kind(*kind, filter.limit.unwrap_or(100));
                candidates.extend(events);
            }
        }

        // Now intersect with remaining filters (AND logic within filter)
        for event in candidates {
            let matches = filter.since.map_or(true, |s| event.created_at >= s)
                && filter.until.map_or(true, |u| event.created_at <= u)
                && filter.authors.as_ref().map_or(true, |authors| {
                    authors.iter().any(|a| {
                        hex::decode(a).map_or(false, |pk| pk.len() == 32 && pk == event.pubkey)
                    })
                })
                && filter.authors.as_ref().map_or(true, |authors| {
                    authors.iter().any(|a| {
                        hex::decode(a).map_or(false, |pk| {
                            pk.len() == 32 && pk.as_slice() == event.pubkey.as_slice()
                        })
                    })
                })
                && filter
                    .kinds
                    .as_ref()
                    .map_or(true, |kinds| kinds.contains(&event.kind))
                && filter.e_tags.as_ref().map_or(true, |e_tags| {
                    e_tags.iter().any(|e| {
                        hex::decode(e).map_or(false, |eid| {
                            eid.len() == 32
                                && event.e_tags().any(|et| eid.as_slice() == et.as_slice())
                        })
                    })
                })
                && filter.p_tags.as_ref().map_or(true, |p_tags| {
                    p_tags.iter().any(|p| {
                        hex::decode(p).map_or(false, |pk| {
                            pk.len() == 32
                                && event.p_tags().any(|ep| pk.as_slice() == ep.as_slice())
                        })
                    })
                });

            if matches {
                if result_ids.insert(event.id) {
                    results.push(event);
                }
            }
        }

        // Apply limit
        if let Some(limit) = filter.limit {
            results.truncate(limit);
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
        Arc::new(Event::from_json(json.as_bytes()).unwrap())
    }

    fn make_event_with_tags(
        id: u8,
        pubkey: u8,
        kind: u32,
        created_at: u64,
        e_tags: &[u8],
        p_tags: &[u8],
    ) -> Arc<Event> {
        let e_tags_str: Vec<String> = e_tags
            .iter()
            .map(|e| format!("{:0>64}", format!("{:x}", e)))
            .collect();
        let p_tags_str: Vec<String> = p_tags
            .iter()
            .map(|p| format!("{:0>64}", format!("{:x}", p)))
            .collect();

        let e_tags_json = e_tags_str
            .iter()
            .map(|e| format!(r#"["e","{}"]"#, e))
            .collect::<Vec<_>>()
            .join(",");
        let p_tags_json = p_tags_str
            .iter()
            .map(|p| format!(r#"["p","{}"]"#, p))
            .collect::<Vec<_>>()
            .join(",");

        let json = format!(
            r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[{}],"content":"test","sig":"{:0>128}"}}"#,
            format!("{:x}", id),
            format!("{:x}", pubkey),
            created_at,
            kind,
            if e_tags_json.is_empty() && p_tags_json.is_empty() {
                String::new()
            } else if e_tags_json.is_empty() {
                p_tags_json
            } else if p_tags_json.is_empty() {
                e_tags_json
            } else {
                format!("{},{}", e_tags_json, p_tags_json)
            },
            "0"
        );
        Arc::new(Event::from_json(json.as_bytes()).unwrap())
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

        let filter = Filter {
            kinds: Some(vec![1]),
            authors: Some(vec![hex::encode(author)]),
            p_tags: Some(vec![hex::encode(p_tag)]),
            ..Default::default()
        };

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
        let event2_json = format!(
            r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[["e","{:0>64}"]],"content":"reply","sig":"{:0>128}"}}"#,
            format!("{:x}", 2),
            format!("{:x}", 1),
            2000,
            1,
            format!("{:x}", event1.id[31]),
            "0"
        );
        let event2 = Arc::new(Event::from_json(event2_json.as_bytes()).unwrap());
        sm.store.insert(event1);
        sm.store.insert(event2.clone());

        let mut e_tag = [0u8; 32];
        e_tag[31] = 1;

        let filter = Filter {
            e_tags: Some(vec![hex::encode(e_tag)]),
            ..Default::default()
        };

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

        let filter = Filter {
            p_tags: Some(vec![hex::encode(p_tag)]),
            ..Default::default()
        };

        let results = sm.query_filter(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id[31], 1);
    }
}
