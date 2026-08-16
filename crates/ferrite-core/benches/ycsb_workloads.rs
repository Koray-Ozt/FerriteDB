use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ferrite_core::Database;
use ferrite_core::workload::{WorkloadGenerator, WorkloadOperation, WorkloadPreset};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn temp_db_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("ferrite-bench-ycsb-{name}-{nonce}"));
    path
}

fn bench_ycsb_workloads(c: &mut Criterion) {
    let mut group = c.benchmark_group("ycsb_workloads");
    let key_count = 500;
    let val_size = 256;
    let op_batch = 100;

    let presets = [
        ("workload_a_50_50", WorkloadPreset::YcsbA),
        ("workload_b_95_5", WorkloadPreset::YcsbB),
        ("workload_c_100_read", WorkloadPreset::YcsbC),
        ("workload_d_read_latest", WorkloadPreset::YcsbD),
        ("workload_e_range_scan", WorkloadPreset::YcsbE),
        ("workload_f_rmw", WorkloadPreset::YcsbF),
        ("point_lookup_100", WorkloadPreset::PointLookup),
        ("write_heavy_90", WorkloadPreset::WriteHeavy),
    ];

    for (name, preset) in presets {
        let db_path = temp_db_path(name);
        let _ = fs::remove_dir_all(&db_path);
        let mut db = Database::open(&db_path).expect("failed to create benchmark database");

        // Seed database
        let mut init_gen = WorkloadGenerator::from_preset(preset, key_count, val_size);
        let initial_records = init_gen.generate_initial_dataset();
        for (k, v) in initial_records {
            db.put_key(&k, v).expect("failed to seed record");
        }

        let mut generator = WorkloadGenerator::from_preset(preset, key_count, val_size);

        group.throughput(Throughput::Elements(op_batch as u64));
        group.bench_function(BenchmarkId::new("preset", name), |b| {
            b.iter(|| {
                for _ in 0..op_batch {
                    match generator.next_op() {
                        WorkloadOperation::Read { ref key } => {
                            let _ = db.get(key);
                        }
                        WorkloadOperation::Update { ref key, ref value }
                        | WorkloadOperation::Insert { ref key, ref value } => {
                            let _ = db.put_key(key, value.clone());
                        }
                        WorkloadOperation::Scan { ref prefix, limit } => {
                            let results = db.list(Some(prefix)).unwrap_or_default();
                            let _ = results.into_iter().take(limit).count();
                        }
                        WorkloadOperation::Delete { ref key } => {
                            let _ = db.delete_key(key);
                        }
                        WorkloadOperation::ReadModifyWrite {
                            ref key,
                            ref field,
                            delta,
                        } => {
                            if let Ok(Some(mut val)) = db.get(key) {
                                if let Some(obj) = val.as_object_mut() {
                                    let curr = obj.get(field).and_then(Value::as_i64).unwrap_or(0);
                                    obj.insert(field.clone(), Value::from(curr + delta));
                                }
                                let _ = db.put_key(key, val);
                            }
                        }
                    }
                }
            });
        });

        drop(db);
        let _ = fs::remove_dir_all(&db_path);
    }

    group.finish();
}

criterion_group!(benches, bench_ycsb_workloads);
criterion_main!(benches);
