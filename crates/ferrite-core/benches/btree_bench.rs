use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ferrite_core::btree::BTree;
use ferrite_core::buffer_pool::{BufferPoolManager, BufferPoolOptions, EvictionPolicy, PAGE_4K};
use ferrite_core::pager::Pager;
use std::fs;
use std::path::PathBuf;

fn temp_btree_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("ferrite-bench-btree-{name}-{nonce}.fdb"));
    path
}

fn bench_btree_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("btree_index");

    // Benchmark sequential insertions
    group.throughput(Throughput::Elements(1000));
    group.bench_function("insert_1000_sequential_keys", |b| {
        b.iter_with_setup(
            || {
                let path = temp_btree_path("insert_seq");
                let pager = Pager::create(&path, PAGE_4K).unwrap();
                let opts = BufferPoolOptions::from_budget(64 * 1024 * 1024, PAGE_4K)
                    .with_eviction_policy(EvictionPolicy::Clock);
                let bpm = BufferPoolManager::with_options(pager, opts);
                let tree = BTree::create(bpm).unwrap();
                (tree, path)
            },
            |(tree, path)| {
                for i in 0..1000 {
                    let k = format!("k_{:06}", i).into_bytes();
                    let v = format!("v_{:06}", i).into_bytes();
                    tree.insert(&k, &v).unwrap();
                }
                let _ = fs::remove_file(&path);
            },
        );
    });

    // Benchmark point lookups over pre-populated 5,000 keys
    {
        let path = temp_btree_path("lookup");
        let pager = Pager::create(&path, PAGE_4K).unwrap();
        let opts = BufferPoolOptions::from_budget(64 * 1024 * 1024, PAGE_4K)
            .with_eviction_policy(EvictionPolicy::Clock);
        let bpm = BufferPoolManager::with_options(pager, opts);
        let tree = BTree::create(bpm).unwrap();

        for i in 0..5000 {
            let k = format!("search_key_{:06}", i).into_bytes();
            let v = format!("search_val_{:06}", i).into_bytes();
            tree.insert(&k, &v).unwrap();
        }

        group.throughput(Throughput::Elements(500));
        group.bench_function("lookup_500_random_keys", |b| {
            b.iter(|| {
                for i in (0..5000).step_by(10) {
                    let k = format!("search_key_{:06}", i).into_bytes();
                    let val = tree.get(&k).unwrap();
                    assert!(val.is_some());
                }
            });
        });

        // Benchmark range scan
        group.throughput(Throughput::Elements(100));
        group.bench_function("range_scan_100_keys", |b| {
            b.iter(|| {
                let start = b"search_key_002000";
                let results = tree.scan(start, 100).unwrap();
                assert_eq!(results.len(), 100);
            });
        });

        let _ = fs::remove_file(&path);
    }

    group.finish();
}

criterion_group!(benches, bench_btree_operations);
criterion_main!(benches);
