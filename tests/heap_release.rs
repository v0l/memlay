#![cfg(all(target_os = "linux", target_env = "gnu"))]

fn rss_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

#[test]
fn freed_fragmented_heap_goes_back_to_the_os() {
    std::thread::spawn(|| {
        let mut blocks: Vec<Box<[u8; 1024]>> =
            (0..400_000).map(|_| Box::new([1u8; 1024])).collect();
        let kept: Vec<Box<[u8; 1024]>> = blocks.drain(..).step_by(64).collect();
        let before = rss_bytes();

        memlay::store::release_free_heap();

        let released = before.saturating_sub(rss_bytes());
        assert!(
            released > 200 << 20,
            "only {} MB of {} MB freed heap went back to the OS",
            released >> 20,
            ((400_000 - kept.len()) * 1024) >> 20
        );
        drop(kept);
    })
    .join()
    .unwrap();
}
