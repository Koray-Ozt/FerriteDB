# FerriteDB Storage Engine Performance & Latency Benchmark Report

- **Timestamp**: 2026-08-16
- **Git Commit**: `feat/issue-8-benchmarks`
- **Rust Toolchain**: rustc 1.97.1
- **Total Suites Run**: 14

## 1. Workload Throughput & Latency Distribution

| Benchmark Workload | Category | Ops/sec | Total Ops | p50 (µs) | p90 (µs) | p95 (µs) | p99 (µs) | Avg (µs) |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **ycsb_workload_a (50/50)** | YCSB Workload | **992** | 2500 | 1790.5 | 2047.2 | 2165.1 | 2783.8 | 1005.9 |
| **ycsb_workload_b (95/5)** | YCSB Workload | **9562** | 2500 | 0.3 | 0.8 | 1810.5 | 2038.6 | 104.2 |
| **ycsb_workload_c (100% Read)** | YCSB Workload | **1125341** | 2500 | 0.5 | 0.8 | 1.0 | 1.3 | 0.5 |
| **ycsb_workload_d (Read Latest)** | YCSB Workload | **1366** | 2500 | 0.9 | 2039.4 | 2094.9 | 2478.4 | 730.6 |
| **ycsb_workload_e (Range Scan)** | YCSB Workload | **1704** | 1250 | 9.3 | 2027.4 | 2057.5 | 2456.8 | 585.9 |
| **ycsb_workload_f (RMW)** | YCSB Workload | **721** | 2500 | 1870.9 | 2073.9 | 2217.3 | 2823.0 | 1385.2 |
| **point_lookup (100% Get)** | YCSB Workload | **1429** | 2500 | 0.9 | 2054.3 | 2180.1 | 2710.5 | 698.3 |
| **write_heavy (90% Put)** | YCSB Workload | **485** | 2500 | 2154.6 | 2581.8 | 2944.0 | 3904.4 | 2060.7 |
| **bpm_clock_cache_hit_100** | Buffer Pool | **5713789** | 5000 | 0.1 | 0.1 | 0.1 | 0.2 | 0.1 |
| **bpm_clock_eviction_constrained** | Buffer Pool | **3746** | 5000 | 2.9 | 1444.4 | 1453.3 | 1579.9 | 266.9 |
| **bpm_lru_k2_eviction_constrained** | Buffer Pool | **4541** | 5000 | 2.7 | 1438.4 | 1451.4 | 1578.8 | 220.1 |
| **slotted_16kb_overflow_record** | Storage Engine | **55** | 250 | 17850.2 | 18414.9 | 20444.5 | 22426.7 | 18130.2 |
| **wal_append_1kb** | WAL Engine | **661** | 1000 | 1454.8 | 1649.0 | 1652.9 | 1664.4 | 1511.9 |
| **wal_append_8kb** | WAL Engine | **606** | 500 | 1645.6 | 1655.2 | 1658.6 | 1821.2 | 1650.3 |

## 2. Page Cache & Buffer Pool Metrics

| Benchmark | Frames | Hits | Misses | Hit Ratio | Evictions | Dirty Evicts | WAL Syncs |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| bpm_clock_cache_hit_100 | 128 | 5100 | 0 | **100.00%** | 0 | 0 | 0 |
| bpm_clock_eviction_constrained | 32 | 1254 | 3846 | **24.59%** | 3942 | 1025 | 0 |
| bpm_lru_k2_eviction_constrained | 32 | 1251 | 3849 | **24.53%** | 3945 | 863 | 0 |

## 3. WAL Write Amplification Analysis

| Benchmark | Logical Payload | WAL Written | Write Amplification |
| :--- | :---: | :---: | :---: |
| ycsb_workload_a (50/50) | 620306 B | 760597 B | **1.23x** |
| ycsb_workload_b (95/5) | 311907 B | 379644 B | **1.22x** |
| ycsb_workload_c (100% Read) | 275890 B | 334898 B | **1.21x** |
| ycsb_workload_d (Read Latest) | 527619 B | 646071 B | **1.22x** |
| ycsb_workload_e (Range Scan) | 375252 B | 459519 B | **1.22x** |
| ycsb_workload_f (RMW) | 501019 B | 913284 B | **1.82x** |
| point_lookup (100% Get) | 510405 B | 625593 B | **1.23x** |
| write_heavy (90% Put) | 900012 B | 1107224 B | **1.23x** |
| wal_append_1kb | 1037000 B | 1096008 B | **1.06x** |
| wal_append_8kb | 4102500 B | 4132008 B | **1.01x** |

