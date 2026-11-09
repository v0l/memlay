use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) fn now_secs() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

#[derive(Default)]
pub(super) struct Retention {
    ttl: HashMap<u32, u32>,
    queue: Mutex<BTreeSet<(u32, [u8; 32])>>,
}

impl Retention {
    pub fn new(ttl: &HashMap<u32, Duration>) -> Self {
        Self {
            ttl: ttl
                .iter()
                .map(|(&kind, d)| (kind, d.as_secs().min(u32::MAX as u64) as u32))
                .collect(),
            queue: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn ttl(&self) -> &HashMap<u32, u32> {
        &self.ttl
    }

    pub fn expires_at(&self, kind: u32, received_at: u32) -> Option<u32> {
        self.ttl
            .get(&kind)
            .map(|ttl| received_at.saturating_add(*ttl))
    }

    pub fn track(&self, expires_at: u32, id: [u8; 32]) {
        self.queue.lock().insert((expires_at, id));
    }

    pub fn is_empty(&self) -> bool {
        self.ttl.is_empty()
    }

    pub fn take_due(&self, now: u32, max: usize) -> Vec<(u32, [u8; 32])> {
        let mut queue = self.queue.lock();
        let mut due = Vec::new();
        while due.len() < max {
            match queue.first() {
                Some(&(at, _)) if at <= now => {
                    due.push(queue.pop_first().unwrap());
                }
                _ => break,
            }
        }
        due
    }

    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    pub fn retain(&self, mut keep: impl FnMut(&[u8; 32]) -> bool) {
        self.queue.lock().retain(|(_, id)| keep(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_due_pops_only_expired_in_order() {
        let r = Retention::new(&HashMap::from([(1059, Duration::from_secs(10))]));
        assert_eq!(r.expires_at(1059, 100), Some(110));
        assert_eq!(r.expires_at(1, 100), None);

        r.track(120, [2; 32]);
        r.track(110, [1; 32]);
        r.track(130, [3; 32]);

        assert_eq!(r.take_due(109, 10), Vec::<(u32, [u8; 32])>::new());
        assert_eq!(r.take_due(125, 10), vec![(110, [1; 32]), (120, [2; 32])]);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn take_due_respects_batch_size() {
        let r = Retention::new(&HashMap::new());
        for i in 0..5u8 {
            r.track(i as u32, [i; 32]);
        }
        assert_eq!(r.take_due(100, 2).len(), 2);
        assert_eq!(r.len(), 3);
        r.retain(|id| id[0] == 4);
        assert_eq!(r.take_due(100, 10), vec![(4, [4; 32])]);
    }
}
