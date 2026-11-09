use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};

type ReceiptQueue = Mutex<BTreeSet<(u32, [u8; 32])>>;

/// Per-kind byte caps, with each capped kind's events queued in the order the
/// relay received them so the earliest-received go first when over the cap.
#[derive(Default)]
pub(super) struct KindQuota {
    share: HashMap<u32, f64>,
    queues: HashMap<u32, ReceiptQueue>,
}

impl KindQuota {
    pub fn new(share: &HashMap<u32, f64>) -> Self {
        Self {
            share: share.clone(),
            queues: share
                .keys()
                .map(|&kind| (kind, Mutex::new(BTreeSet::new())))
                .collect(),
        }
    }

    pub fn shares(&self) -> &HashMap<u32, f64> {
        &self.share
    }

    pub fn is_empty(&self) -> bool {
        self.share.is_empty()
    }

    pub fn limit(&self, kind: u32, budget: usize) -> Option<usize> {
        self.share
            .get(&kind)
            .map(|share| (budget as f64 * share) as usize)
    }

    pub fn track(&self, kind: u32, received_at: u32, id: [u8; 32]) {
        if let Some(queue) = self.queues.get(&kind) {
            queue.lock().insert((received_at, id));
        }
    }

    pub fn pop_oldest(&self, kind: u32, max: usize) -> Vec<[u8; 32]> {
        let Some(queue) = self.queues.get(&kind) else {
            return Vec::new();
        };
        let mut queue = queue.lock();
        let mut out = Vec::with_capacity(max.min(queue.len()));
        while out.len() < max {
            match queue.pop_first() {
                Some((_, id)) => out.push(id),
                None => break,
            }
        }
        out
    }

    pub fn kinds(&self) -> impl Iterator<Item = u32> + '_ {
        self.share.keys().copied()
    }

    pub fn tracked(&self, kind: u32) -> usize {
        self.queues.get(&kind).map_or(0, |q| q.lock().len())
    }

    pub fn retain(&self, kind: u32, mut keep: impl FnMut(&[u8; 32]) -> bool) {
        if let Some(queue) = self.queues.get(&kind) {
            queue.lock().retain(|(_, id)| keep(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pops_earliest_received_first_per_kind() {
        let q = KindQuota::new(&HashMap::from([(1059, 0.25)]));
        assert_eq!(q.limit(1059, 1000), Some(250));
        assert_eq!(q.limit(1, 1000), None);

        q.track(1059, 30, [3; 32]);
        q.track(1059, 10, [1; 32]);
        q.track(1059, 20, [2; 32]);
        q.track(1, 5, [9; 32]);

        assert_eq!(q.tracked(1), 0, "kind without a quota was queued");
        assert_eq!(q.pop_oldest(1059, 2), vec![[1; 32], [2; 32]]);
        assert_eq!(q.tracked(1059), 1);
        q.retain(1059, |id| id[0] != 3);
        assert_eq!(q.pop_oldest(1059, 10), Vec::<[u8; 32]>::new());
    }
}
