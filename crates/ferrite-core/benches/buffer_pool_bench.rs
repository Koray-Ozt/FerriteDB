use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ferrite_core::buffer_pool::{BufferPoolManager, BufferPoolOptions, EvictionPolicy, PAGE_4K};
use ferrite_core::workload::FastRng;
use std::fs;
use std::path::PathBuf;

fn temp_bpm_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("ferrite-bench-bpm-{name}-{nonce}.fdb"));
    path
}

fn bench_buffer_pool_hits_and_misses(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_pool_access");

    // 1. In-memory Hit Throughput (Working set fits 100% in buffer pool)
    {
        let path = temp_bpm_path("hit");
        let pool_size = 256;
        let opts = BufferPoolOptions::from_budget(pool_size * (PAGE_4K as usize), PAGE_4K);
        let bpm = BufferPoolManager::open_or_create(&path, PAGE_4K, Some(opts)).unwrap();

        // Allocate pages
        let total_pages = 100;
        for _ in 0..total_pages {
            let (_, guard) = bpm.new_page().unwrap();
            drop(guard);
        }

        group.throughput(Throughput::Elements(1000));
        group.bench_function("fetch_page_cache_hit_100pct", |b| {
            let mut rng = FastRng::new(101);
            b.iter(|| {
                for _ in 0..1000 {
                    let page_idx = 1 + (rng.next_range(total_pages) as u64);
                    let guard = bpm.fetch_page(page_idx).unwrap();
                    let _byte = guard[0];
                    drop(guard);
                }
            });
        });

        drop(bpm);
        let _ = fs::remove_file(&path);
    }

    // 2. Cache Eviction / Miss Throughput (CLOCK vs LRU-K under tight budget)
    for (name, policy) in [
        ("eviction_clock", EvictionPolicy::Clock),
        ("eviction_lru_2", EvictionPolicy::LruK(2)),
    ] {
        let path = temp_bpm_path(name);
        let pool_frames = 32; // Constrained 32 frames (128 KiB)
        let opts = BufferPoolOptions::from_budget(pool_frames * (PAGE_4K as usize), PAGE_4K)
            .with_eviction_policy(policy);
        let bpm = BufferPoolManager::open_or_create(&path, PAGE_4K, Some(opts)).unwrap();

        let dataset_pages = 256; // 8x working set relative to buffer pool
        for _ in 0..dataset_pages {
            let (_, guard) = bpm.new_page().unwrap();
            drop(guard);
        }

        group.throughput(Throughput::Elements(500));
        group.bench_function(BenchmarkId::new("constrained_eviction", name), |b| {
            let mut rng = FastRng::new(202);
            b.iter(|| {
                for _ in 0..500 {
                    let page_idx = 1 + (rng.next_range(dataset_pages) as u64);
                    let mut guard = bpm.fetch_page_mut(page_idx).unwrap();
                    guard[0] = guard[0].wrapping_add(1);
                    drop(guard);
                }
            });
        });

        drop(bpm);
        let _ = fs::remove_file(&path);
    }

    group.finish();
}

criterion_group!(benches, bench_buffer_pool_hits_and_misses);
criterion_main!(benches);
