use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const CUT_PERCENT: usize = 90;
const REARM_PERCENT: u64 = 90;

pub(super) struct RssPressure {
    ceiling: AtomicUsize,
    rss_at_cut: AtomicU64,
}

impl RssPressure {
    pub fn new() -> Self {
        Self {
            ceiling: AtomicUsize::new(0),
            rss_at_cut: AtomicU64::new(0),
        }
    }

    pub fn payload_ceiling(
        &self,
        payload: usize,
        rss: u64,
        max_bytes: usize,
        floor: usize,
    ) -> Option<usize> {
        let max_bytes = max_bytes as u64;
        if rss < max_bytes * REARM_PERCENT / 100 {
            self.ceiling.store(0, Ordering::Relaxed);
            self.rss_at_cut.store(0, Ordering::Relaxed);
            return None;
        }

        if self.rss_at_cut.load(Ordering::Relaxed) == 0 && rss > max_bytes {
            let cut = (payload * CUT_PERCENT / 100).max(floor).max(1);
            self.ceiling.store(cut, Ordering::Relaxed);
            self.rss_at_cut.store(rss, Ordering::Relaxed);
        }

        match self.ceiling.load(Ordering::Relaxed) {
            0 => None,
            ceiling => Some(ceiling),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 1000 * 1000;

    #[test]
    fn no_ceiling_under_the_limit() {
        let pressure = RssPressure::new();
        assert_eq!(pressure.payload_ceiling(500, MAX as u64 / 2, MAX, 0), None);
    }

    #[test]
    fn stuck_rss_cuts_once() {
        let pressure = RssPressure::new();
        let rss = MAX as u64 + 1;
        assert_eq!(pressure.payload_ceiling(10_000, rss, MAX, 0), Some(9_000));
        for _ in 0..100 {
            assert_eq!(pressure.payload_ceiling(9_000, rss, MAX, 0), Some(9_000));
        }
    }

    #[test]
    fn rss_growth_after_a_cut_does_not_drain_the_store() {
        let pressure = RssPressure::new();
        let mut rss = MAX as u64 + 1;
        let mut payload = pressure.payload_ceiling(10_000, rss, MAX, 1_000).unwrap();
        for _ in 0..20 {
            rss += MAX as u64 / 100 + 1;
            payload = pressure.payload_ceiling(payload, rss, MAX, 1_000).unwrap();
        }
        assert_eq!(
            payload, 9_000,
            "payload is held at the ceiling, so further RSS growth is not events"
        );
    }

    #[test]
    fn never_cuts_below_the_floor() {
        let pressure = RssPressure::new();
        assert_eq!(
            pressure.payload_ceiling(10_000, MAX as u64 + 1, MAX, 9_500),
            Some(9_500)
        );
    }

    #[test]
    fn hovering_near_the_limit_keeps_the_ceiling() {
        let pressure = RssPressure::new();
        pressure.payload_ceiling(10_000, MAX as u64 + 1, MAX, 0);
        assert_eq!(
            pressure.payload_ceiling(9_000, MAX as u64 - 1, MAX, 0),
            Some(9_000)
        );
        assert_eq!(
            pressure.payload_ceiling(9_000, MAX as u64 + 1, MAX, 0),
            Some(9_000)
        );
    }

    #[test]
    fn falling_well_under_the_limit_clears_the_ceiling() {
        let pressure = RssPressure::new();
        pressure.payload_ceiling(10_000, MAX as u64 + 1, MAX, 0);
        assert_eq!(
            pressure.payload_ceiling(9_000, MAX as u64 / 2, MAX, 0),
            None
        );
    }
}
