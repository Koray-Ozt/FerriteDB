//! FerriteDB continuous performance, latency, and regression benchmark suite runner.

use ferrite_core::Database;
use ferrite_core::bench_metrics::{
    BenchmarkResult, BenchmarkSuiteReport, BufferPoolStatsSummary, LatencyHistogram,
    WriteAmpSummary,
};
use ferrite_core::buffer_pool::{BufferPoolManager, BufferPoolOptions, EvictionPolicy, PAGE_4K};
use ferrite_core::pager::Pager;
use ferrite_core::slotted_page::{get_record, put_record};
use ferrite_core::wal::Wal;
use ferrite_core::workload::{FastRng, WorkloadGenerator, WorkloadOperation, WorkloadPreset};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn temp_bench_path(prefix: &str) -> PathBuf {
    let mut path = env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("ferrite-bench-run-{prefix}-{nonce}"));
    path
}

fn run_ycsb_preset(
    name: &str,
    preset: WorkloadPreset,
    key_count: usize,
    val_size: usize,
    op_count: usize,
) -> BenchmarkResult {
    let db_path = temp_bench_path(name);
    let _ = fs::remove_dir_all(&db_path);
    let mut db = Database::open(&db_path).expect("failed to create benchmark database");

    // Pre-populate database
    let mut init_gen = WorkloadGenerator::from_preset(preset, key_count, val_size);
    let initial_data = init_gen.generate_initial_dataset();
    let mut logical_bytes_written = 0u64;
    for (k, v) in initial_data {
        let bytes_len = serde_json::to_vec(&v).map(|b| b.len()).unwrap_or(val_size) as u64;
        logical_bytes_written += k.len() as u64 + bytes_len;
        db.put_key(&k, v).expect("failed to seed initial dataset");
    }

    let mut generator = WorkloadGenerator::from_preset(preset, key_count, val_size);

    // Warmup phase (50 ops) to prime page caches and memory structures
    for _ in 0..50 {
        let op = generator.next_op();
        match op {
            WorkloadOperation::Read { ref key } => {
                let _ = db.get(key);
            }
            WorkloadOperation::Update { ref key, ref value }
            | WorkloadOperation::Insert { ref key, ref value } => {
                let _ = db.put_key(key, value.clone());
            }
            WorkloadOperation::Scan { ref prefix, limit } => {
                let _ = db
                    .list(Some(prefix))
                    .unwrap_or_default()
                    .into_iter()
                    .take(limit)
                    .count();
            }
            _ => {}
        }
    }

    let mut histogram = LatencyHistogram::with_capacity(op_count);

    let start = Instant::now();
    for _ in 0..op_count {
        let op = generator.next_op();
        let op_start = Instant::now();
        match op {
            WorkloadOperation::Read { ref key } => {
                let _ = db.get(key);
            }
            WorkloadOperation::Update { ref key, ref value }
            | WorkloadOperation::Insert { ref key, ref value } => {
                let bytes_len = serde_json::to_vec(value)
                    .map(|b| b.len())
                    .unwrap_or(val_size) as u64;
                logical_bytes_written += key.len() as u64 + bytes_len;
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
        histogram.record(op_start.elapsed());
    }
    let duration = start.elapsed();

    // Check WAL size for write amplification
    let wal_path = db_path.join("data.wal");
    let wal_len = fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    let write_amp = if logical_bytes_written > 0 {
        Some(WriteAmpSummary::new(logical_bytes_written, wal_len))
    } else {
        None
    };

    drop(db);
    let _ = fs::remove_dir_all(&db_path);

    let duration_ms = duration.as_secs_f64() * 1000.0;
    let ops_per_sec = (op_count as f64) / duration.as_secs_f64();

    BenchmarkResult {
        name: name.to_string(),
        category: "YCSB Workload".into(),
        total_ops: op_count,
        duration_ms,
        ops_per_sec,
        latency: histogram.summary(),
        cache_stats: None,
        write_amp,
    }
}

fn run_buffer_pool_bench(
    name: &str,
    policy: EvictionPolicy,
    frames: usize,
    pages_count: usize,
    op_count: usize,
) -> BenchmarkResult {
    let path = temp_bench_path(name).with_extension("fdb");
    let opts = BufferPoolOptions::from_budget(frames * (PAGE_4K as usize), PAGE_4K)
        .with_eviction_policy(policy);
    let bpm = BufferPoolManager::open_or_create(&path, PAGE_4K, Some(opts)).unwrap();

    // Pre-allocate pages
    for _ in 0..pages_count {
        let (_, guard) = bpm.new_page().unwrap();
        drop(guard);
    }

    let mut rng = FastRng::new(555);

    // Warmup phase (100 ops)
    for _ in 0..100 {
        let page_idx = 1 + (rng.next_range(pages_count) as u64);
        if let Ok(guard) = bpm.fetch_page(page_idx) {
            let _ = guard[0];
        }
    }

    let mut histogram = LatencyHistogram::with_capacity(op_count);

    let start = Instant::now();
    for _ in 0..op_count {
        let page_idx = 1 + (rng.next_range(pages_count) as u64);
        let op_start = Instant::now();
        if rng.next_f64() < 0.80 {
            let guard = bpm.fetch_page(page_idx).unwrap();
            let _ = guard[0];
            drop(guard);
        } else {
            let mut guard = bpm.fetch_page_mut(page_idx).unwrap();
            guard[0] = guard[0].wrapping_add(1);
            drop(guard);
        }
        histogram.record(op_start.elapsed());
    }
    let duration = start.elapsed();

    let stats = bpm.stats();
    let cache_summary = BufferPoolStatsSummary {
        pool_size: stats.pool_size,
        page_size: stats.page_size,
        hits: stats.hits,
        misses: stats.misses,
        hit_ratio_pct: stats.hit_ratio() * 100.0,
        evictions: stats.evictions,
        dirty_evictions: stats.dirty_evictions,
        wal_syncs: stats.wal_syncs,
    };

    drop(bpm);
    let _ = fs::remove_file(&path);

    let duration_ms = duration.as_secs_f64() * 1000.0;
    let ops_per_sec = (op_count as f64) / duration.as_secs_f64();

    BenchmarkResult {
        name: name.to_string(),
        category: "Buffer Pool".into(),
        total_ops: op_count,
        duration_ms,
        ops_per_sec,
        latency: histogram.summary(),
        cache_stats: Some(cache_summary),
        write_amp: None,
    }
}

fn run_slotted_overflow_bench(name: &str, op_count: usize) -> BenchmarkResult {
    let path = temp_bench_path(name).with_extension("fdb");
    let mut pager = Pager::create(&path, PAGE_4K).unwrap();
    let payload = vec![0xBAu8; 16 * 1024]; // 16 KiB record

    let mut histogram = LatencyHistogram::with_capacity(op_count);
    let start = Instant::now();

    for _ in 0..op_count {
        let op_start = Instant::now();
        let rec_id = put_record(&mut pager, &payload).unwrap();
        let retrieved = get_record(&mut pager, rec_id).unwrap();
        assert_eq!(retrieved.len(), payload.len());
        histogram.record(op_start.elapsed());
    }
    let duration = start.elapsed();

    drop(pager);
    let _ = fs::remove_file(&path);

    let duration_ms = duration.as_secs_f64() * 1000.0;
    let ops_per_sec = (op_count as f64) / duration.as_secs_f64();

    BenchmarkResult {
        name: name.to_string(),
        category: "Storage Engine".into(),
        total_ops: op_count,
        duration_ms,
        ops_per_sec,
        latency: histogram.summary(),
        cache_stats: None,
        write_amp: None,
    }
}

fn run_wal_append_bench(name: &str, payload_size: usize, op_count: usize) -> BenchmarkResult {
    let path = temp_bench_path(name).with_extension("wal");
    let mut wal = Wal::create(&path).unwrap();
    let payload = vec![0xFEu8; payload_size];
    let key = b"bench/wal_key";
    let logical_bytes = (key.len() + payload_size) as u64 * op_count as u64;

    let mut histogram = LatencyHistogram::with_capacity(op_count);
    let start = Instant::now();

    for tx_id in 1..=(op_count as u64) {
        let op_start = Instant::now();
        wal.begin(tx_id).unwrap();
        wal.put(tx_id, key, &payload).unwrap();
        wal.commit(tx_id).unwrap();
        histogram.record(op_start.elapsed());
    }
    let duration = start.elapsed();

    let wal_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let write_amp = Some(WriteAmpSummary::new(logical_bytes, wal_len));

    drop(wal);
    let _ = fs::remove_file(&path);

    let duration_ms = duration.as_secs_f64() * 1000.0;
    let ops_per_sec = (op_count as f64) / duration.as_secs_f64();

    BenchmarkResult {
        name: name.to_string(),
        category: "WAL Engine".into(),
        total_ops: op_count,
        duration_ms,
        ops_per_sec,
        latency: histogram.summary(),
        cache_stats: None,
        write_amp,
    }
}

fn run_full_suite(quick: bool) -> BenchmarkSuiteReport {
    let factor = if quick { 1 } else { 5 };
    let ycsb_keys = if quick { 300 } else { 1000 };
    let ycsb_ops = 500 * factor;

    let mut results = Vec::new();

    println!("Running YCSB Workloads...");
    results.push(run_ycsb_preset(
        "ycsb_workload_a (50/50)",
        WorkloadPreset::YcsbA,
        ycsb_keys,
        256,
        ycsb_ops,
    ));
    results.push(run_ycsb_preset(
        "ycsb_workload_b (95/5)",
        WorkloadPreset::YcsbB,
        ycsb_keys,
        256,
        ycsb_ops,
    ));
    results.push(run_ycsb_preset(
        "ycsb_workload_c (100% Read)",
        WorkloadPreset::YcsbC,
        ycsb_keys,
        256,
        ycsb_ops,
    ));
    results.push(run_ycsb_preset(
        "ycsb_workload_d (Read Latest)",
        WorkloadPreset::YcsbD,
        ycsb_keys,
        256,
        ycsb_ops,
    ));
    results.push(run_ycsb_preset(
        "ycsb_workload_e (Range Scan)",
        WorkloadPreset::YcsbE,
        ycsb_keys,
        256,
        ycsb_ops / 2,
    ));
    results.push(run_ycsb_preset(
        "ycsb_workload_f (RMW)",
        WorkloadPreset::YcsbF,
        ycsb_keys,
        256,
        ycsb_ops,
    ));
    results.push(run_ycsb_preset(
        "point_lookup (100% Get)",
        WorkloadPreset::PointLookup,
        ycsb_keys,
        256,
        ycsb_ops,
    ));
    results.push(run_ycsb_preset(
        "write_heavy (90% Put)",
        WorkloadPreset::WriteHeavy,
        ycsb_keys,
        256,
        ycsb_ops,
    ));

    println!("Running Buffer Pool Benchmarks...");
    // 100% hit in-memory
    results.push(run_buffer_pool_bench(
        "bpm_clock_cache_hit_100",
        EvictionPolicy::Clock,
        128,
        50,
        1000 * factor,
    ));
    // Constrained RAM CLOCK eviction (working set 4x pool size)
    results.push(run_buffer_pool_bench(
        "bpm_clock_eviction_constrained",
        EvictionPolicy::Clock,
        32,
        128,
        1000 * factor,
    ));
    // Constrained RAM LRU-K(2) eviction
    results.push(run_buffer_pool_bench(
        "bpm_lru_k2_eviction_constrained",
        EvictionPolicy::LruK(2),
        32,
        128,
        1000 * factor,
    ));

    println!("Running Storage & WAL Benchmarks...");
    results.push(run_slotted_overflow_bench(
        "slotted_16kb_overflow_record",
        50 * factor,
    ));
    results.push(run_wal_append_bench("wal_append_1kb", 1024, 200 * factor));
    results.push(run_wal_append_bench("wal_append_8kb", 8192, 100 * factor));

    BenchmarkSuiteReport {
        title: "FerriteDB Baseline Performance Report".into(),
        timestamp: "2026-08-16".into(),
        git_commit: "feat/issue-8-benchmarks".into(),
        rustc_version: "rustc 1.97.1".into(),
        results,
    }
}

fn print_usage() {
    eprintln!(
        "Usage: ferrite-bench [OPTIONS]\n\n\
        Options:\n  \
          --quick                     Run fast benchmark suite (for CI / regression tests)\n  \
          --output-json <PATH>        Write benchmark results as JSON\n  \
          --output-markdown <PATH>    Write benchmark report as Markdown\n  \
          --check <BASELINE_JSON>     Compare run with baseline JSON and detect >10% regression\n  \
          --threshold <PERCENT>       Allowed regression percentage threshold (default: 10.0)\n  \
          --help                      Show this help message\n"
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut quick = false;
    let mut output_json: Option<PathBuf> = None;
    let mut output_markdown: Option<PathBuf> = None;
    let mut check_baseline: Option<PathBuf> = None;
    let mut threshold_pct: f64 = 10.0;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--quick" => quick = true,
            "--output-json" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Error: --output-json requires a path");
                    std::process::exit(1);
                }
                output_json = Some(PathBuf::from(&args[i]));
            }
            "--output-markdown" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Error: --output-markdown requires a path");
                    std::process::exit(1);
                }
                output_markdown = Some(PathBuf::from(&args[i]));
            }
            "--check" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Error: --check requires a baseline JSON path");
                    std::process::exit(1);
                }
                check_baseline = Some(PathBuf::from(&args[i]));
            }
            "--threshold" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("Error: --threshold requires a percentage value");
                    std::process::exit(1);
                }
                threshold_pct = args[i].parse().unwrap_or(10.0);
            }
            "--help" | "-h" => {
                print_usage();
                return;
            }
            unknown => {
                eprintln!("Unknown argument: {unknown}");
                print_usage();
                std::process::exit(1);
            }
        }
        i += 1;
    }

    println!("============================================================");
    println!("  FerriteDB Storage Engine Performance & Regression Suite   ");
    println!("============================================================");

    let report = run_full_suite(quick);
    let md = report.to_markdown();
    println!("\n{}", md);

    if let Some(ref path) = output_json {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let json_data = report.to_json().expect("failed to serialize JSON report");
        fs::write(path, json_data).expect("failed to write output JSON");
        println!("Saved JSON report to {}", path.display());
    }

    if let Some(ref path) = output_markdown {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(path, &md).expect("failed to write output Markdown");
        println!("Saved Markdown report to {}", path.display());
    }

    if let Some(ref base_path) = check_baseline {
        println!(
            "\nChecking performance regression against baseline {}...",
            base_path.display()
        );
        let base_content = fs::read_to_string(base_path).expect("failed to read baseline JSON");
        let baseline =
            BenchmarkSuiteReport::from_json(&base_content).expect("failed to parse baseline JSON");

        let reg_report = report.compare(&baseline, threshold_pct);
        println!("{}", reg_report.to_markdown_summary());

        if !reg_report.passed {
            eprintln!(
                "❌ PERFORMANCE REGRESSION DETECTED! {} metric(s) dropped > {:.1}%.",
                reg_report.regressions.len(),
                threshold_pct
            );
            std::process::exit(2);
        } else {
            println!("✅ Performance regression check passed!");
        }
    }
}
