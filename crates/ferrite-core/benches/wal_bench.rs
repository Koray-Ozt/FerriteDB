use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ferrite_core::wal::{Recovery, Wal};
use std::fs;
use std::path::PathBuf;

fn temp_wal_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("ferrite-bench-wal-{name}-{nonce}.wal"));
    path
}

fn bench_wal_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_engine");

    // 1. Append Throughput for various payload sizes
    for size in [100, 1024, 8192] {
        let name = format!("payload_{size}b");
        let payload = vec![0x33u8; size];
        let key = b"bench/record";

        group.throughput(Throughput::Bytes((size * 50) as u64));
        group.bench_function(BenchmarkId::new("append_50_records", &name), |b| {
            b.iter(|| {
                let path = temp_wal_path(&name);
                let mut wal = Wal::create(&path).unwrap();
                for tx_id in 1..=50 {
                    wal.begin(tx_id).unwrap();
                    wal.put(tx_id, key, &payload).unwrap();
                    wal.commit(tx_id).unwrap();
                }
                drop(wal);
                let _ = fs::remove_file(&path);
            });
        });
    }

    // 2. Transaction Batching (100 operations in a single transaction)
    {
        let payload = vec![0x77u8; 128];
        let key = b"bench/batch";

        group.throughput(Throughput::Elements(100));
        group.bench_function("transaction_batch_100_ops", |b| {
            b.iter(|| {
                let path = temp_wal_path("batch_100");
                let mut wal = Wal::create(&path).unwrap();
                wal.begin(1).unwrap();
                for _ in 0..100 {
                    wal.put(1, key, &payload).unwrap();
                }
                wal.commit(1).unwrap();
                drop(wal);
                let _ = fs::remove_file(&path);
            });
        });
    }

    // 3. Recovery Replay Speed
    {
        let path = temp_wal_path("recovery_replay");
        let mut wal = Wal::create(&path).unwrap();
        let payload = vec![0x88u8; 256];

        for tx_id in 1..=500 {
            wal.begin(tx_id).unwrap();
            wal.put(tx_id, format!("key_{tx_id}").as_bytes(), &payload)
                .unwrap();
            wal.commit(tx_id).unwrap();
        }
        drop(wal);

        group.throughput(Throughput::Elements(500));
        group.bench_function("recovery_replay_500_transactions", |b| {
            b.iter(|| {
                let recovery = Recovery::read(&path).unwrap();
                assert_eq!(recovery.committed().len(), 500);
            });
        });

        let _ = fs::remove_file(&path);
    }

    group.finish();
}

criterion_group!(benches, bench_wal_operations);
criterion_main!(benches);
