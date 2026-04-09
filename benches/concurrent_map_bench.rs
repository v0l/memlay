use criterion::{Criterion, criterion_group, criterion_main};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

fn benchmark_concurrent_map_operations(c: &mut Criterion) {
    let num_threads = num_cpus::get();

    c.bench_function("RwLock_HashMap_concurrent_rw", |b| {
        b.iter(|| {
            let map: Arc<RwLock<HashMap<u32, u32>>> = Arc::new(RwLock::new(HashMap::new()));
            let handles: Vec<_> = (0..num_threads)
                .map(|tid| {
                    let map = Arc::clone(&map);
                    std::thread::spawn(move || {
                        for i in 0..100 {
                            if i % 2 == 0 {
                                let mut guard = map.write();
                                guard.insert((tid * 1000 + i) as u32, (tid * 1000 + i) as u32);
                            } else {
                                let guard = map.read();
                                let _val: Option<&u32> = guard.get(&0);
                            }
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });

    c.bench_function("DashMap_concurrent_rw", |b| {
        b.iter(|| {
            let map: dashmap::DashMap<u32, u32> = dashmap::DashMap::new();
            let handles: Vec<_> = (0..num_threads)
                .map(|tid| {
                    let map = map.clone();
                    std::thread::spawn(move || {
                        for i in 0..100 {
                            if i % 2 == 0 {
                                map.insert((tid * 1000 + i) as u32, (tid * 1000 + i) as u32);
                            } else {
                                let _val = map.get(&0);
                            }
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
        });
    });
}

criterion_group!(benches, benchmark_concurrent_map_operations);
criterion_main!(benches);
