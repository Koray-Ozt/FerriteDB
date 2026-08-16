//! Benchmark metrics, latency histograms, write amplification trackers,
//! and automated performance regression detection.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Precise latency sample collector for percentile computations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LatencyHistogram {
    samples_us: Vec<f64>,
    min_us: f64,
    max_us: f64,
    sum_us: f64,
}

impl LatencyHistogram {
    /// Create a new empty histogram with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            samples_us: Vec::with_capacity(capacity),
            min_us: f64::MAX,
            max_us: 0.0,
            sum_us: 0.0,
        }
    }

    /// Record an elapsed operation duration.
    pub fn record(&mut self, duration: Duration) {
        let us = duration.as_nanos() as f64 / 1_000.0;
        self.record_us(us);
    }

    /// Record latency directly in microseconds.
    pub fn record_us(&mut self, us: f64) {
        self.samples_us.push(us);
        if us < self.min_us {
            self.min_us = us;
        }
        if us > self.max_us {
            self.max_us = us;
        }
        self.sum_us += us;
    }

    /// Total number of samples recorded.
    pub fn count(&self) -> usize {
        self.samples_us.len()
    }

    /// Compute detailed latency summary percentiles (in microseconds).
    pub fn summary(&mut self) -> LatencySummary {
        if self.samples_us.is_empty() {
            return LatencySummary::default();
        }

        self.samples_us
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let count = self.samples_us.len();
        let avg_us = self.sum_us / count as f64;

        let percentile = |p: f64| -> f64 {
            let rank = ((count as f64) * (p / 100.0)).round() as usize;
            let idx = rank.saturating_sub(1).min(count - 1);
            self.samples_us[idx]
        };

        LatencySummary {
            count,
            min_us: self.min_us,
            avg_us,
            p50_us: percentile(50.0),
            p90_us: percentile(90.0),
            p95_us: percentile(95.0),
            p99_us: percentile(99.0),
            p99_9_us: percentile(99.9),
            max_us: self.max_us,
        }
    }
}

/// Latency statistics summary for a benchmark run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LatencySummary {
    pub count: usize,
    pub min_us: f64,
    pub avg_us: f64,
    pub p50_us: f64,
    pub p90_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub p99_9_us: f64,
    pub max_us: f64,
}

/// Buffer pool cache statistics summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BufferPoolStatsSummary {
    pub pool_size: usize,
    pub page_size: u32,
    pub hits: u64,
    pub misses: u64,
    pub hit_ratio_pct: f64,
    pub evictions: u64,
    pub dirty_evictions: u64,
    pub wal_syncs: u64,
}

/// WAL and storage write amplification tracker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WriteAmpSummary {
    /// Logical application payload bytes written.
    pub logical_bytes: u64,
    /// Physical bytes appended to the WAL file.
    pub wal_bytes: u64,
    /// Physical write amplification ratio (wal_bytes / logical_bytes).
    pub amplification_ratio: f64,
}

impl WriteAmpSummary {
    pub fn new(logical_bytes: u64, wal_bytes: u64) -> Self {
        let amplification_ratio = if logical_bytes == 0 {
            0.0
        } else {
            wal_bytes as f64 / logical_bytes as f64
        };
        Self {
            logical_bytes,
            wal_bytes,
            amplification_ratio,
        }
    }
}

/// Individual benchmark outcome containing throughput, latency, cache, and write amp.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub name: String,
    pub category: String,
    pub total_ops: usize,
    pub duration_ms: f64,
    pub ops_per_sec: f64,
    pub latency: LatencySummary,
    pub cache_stats: Option<BufferPoolStatsSummary>,
    pub write_amp: Option<WriteAmpSummary>,
}

/// Complete benchmark suite report.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BenchmarkSuiteReport {
    pub title: String,
    pub timestamp: String,
    pub git_commit: String,
    pub rustc_version: String,
    pub results: Vec<BenchmarkResult>,
}

impl BenchmarkSuiteReport {
    /// Convert benchmark results to a machine-readable JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse benchmark report from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Render a GitHub Flavored Markdown benchmark report.
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str("# FerriteDB Storage Engine Performance & Latency Benchmark Report\n\n");
        md.push_str(&format!("- **Timestamp**: {}\n", self.timestamp));
        md.push_str(&format!("- **Git Commit**: `{}`\n", self.git_commit));
        md.push_str(&format!("- **Rust Toolchain**: {}\n", self.rustc_version));
        md.push_str(&format!(
            "- **Total Suites Run**: {}\n\n",
            self.results.len()
        ));

        md.push_str("## 1. Workload Throughput & Latency Distribution\n\n");
        md.push_str("| Benchmark Workload | Category | Ops/sec | Total Ops | p50 (µs) | p90 (µs) | p95 (µs) | p99 (µs) | Avg (µs) |\n");
        md.push_str("| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |\n");

        for res in &self.results {
            md.push_str(&format!(
                "| **{}** | {} | **{:.0}** | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |\n",
                res.name,
                res.category,
                res.ops_per_sec,
                res.total_ops,
                res.latency.p50_us,
                res.latency.p90_us,
                res.latency.p95_us,
                res.latency.p99_us,
                res.latency.avg_us,
            ));
        }
        md.push('\n');

        let has_cache = self.results.iter().any(|r| r.cache_stats.is_some());
        if has_cache {
            md.push_str("## 2. Page Cache & Buffer Pool Metrics\n\n");
            md.push_str("| Benchmark | Frames | Hits | Misses | Hit Ratio | Evictions | Dirty Evicts | WAL Syncs |\n");
            md.push_str("| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |\n");
            for res in &self.results {
                if let Some(ref c) = res.cache_stats {
                    md.push_str(&format!(
                        "| {} | {} | {} | {} | **{:.2}%** | {} | {} | {} |\n",
                        res.name,
                        c.pool_size,
                        c.hits,
                        c.misses,
                        c.hit_ratio_pct,
                        c.evictions,
                        c.dirty_evictions,
                        c.wal_syncs
                    ));
                }
            }
            md.push('\n');
        }

        let has_wal = self.results.iter().any(|r| r.write_amp.is_some());
        if has_wal {
            md.push_str("## 3. WAL Write Amplification Analysis\n\n");
            md.push_str("| Benchmark | Logical Payload | WAL Written | Write Amplification |\n");
            md.push_str("| :--- | :---: | :---: | :---: |\n");
            for res in &self.results {
                if let Some(ref w) = res.write_amp {
                    md.push_str(&format!(
                        "| {} | {} B | {} B | **{:.2}x** |\n",
                        res.name, w.logical_bytes, w.wal_bytes, w.amplification_ratio
                    ));
                }
            }
            md.push('\n');
        }

        md
    }

    /// Compare against a baseline report and detect performance regressions exceeding `threshold_pct` (e.g. 10.0%).
    pub fn compare(&self, baseline: &BenchmarkSuiteReport, threshold_pct: f64) -> RegressionReport {
        let mut comparisons = Vec::new();
        let mut regressions = Vec::new();

        for current in &self.results {
            let base_opt = baseline.results.iter().find(|b| b.name == current.name);
            let Some(base) = base_opt else {
                continue;
            };

            // 1. Throughput comparison (regression if current < baseline by > threshold_pct)
            let delta_throughput = if base.ops_per_sec > 0.0 {
                ((current.ops_per_sec - base.ops_per_sec) / base.ops_per_sec) * 100.0
            } else {
                0.0
            };
            let throughput_regression = delta_throughput < -threshold_pct;
            let throughput_comp = MetricComparison {
                bench_name: current.name.clone(),
                metric_name: "Throughput (ops/sec)".into(),
                baseline_val: base.ops_per_sec,
                current_val: current.ops_per_sec,
                delta_pct: delta_throughput,
                is_regression: throughput_regression,
            };
            if throughput_regression {
                regressions.push(throughput_comp.clone());
            }
            comparisons.push(throughput_comp);

            // Minimum absolute latency increase required to avoid false positives on background OS I/O scheduler noise
            const MIN_NOISE_FLOOR_US: f64 = 100.0;

            // 2. p95 Latency comparison (regression if current > baseline by > threshold_pct * 1.5 and > noise floor)
            let delta_p95 = if base.latency.p95_us > 0.0 {
                ((current.latency.p95_us - base.latency.p95_us) / base.latency.p95_us) * 100.0
            } else {
                0.0
            };
            let abs_diff_p95 = current.latency.p95_us - base.latency.p95_us;
            let p95_regression =
                delta_p95 > (threshold_pct * 1.5) && abs_diff_p95 > MIN_NOISE_FLOOR_US;
            let p95_comp = MetricComparison {
                bench_name: current.name.clone(),
                metric_name: "p95 Latency (µs)".into(),
                baseline_val: base.latency.p95_us,
                current_val: current.latency.p95_us,
                delta_pct: delta_p95,
                is_regression: p95_regression,
            };
            if p95_regression {
                regressions.push(p95_comp.clone());
            }
            comparisons.push(p95_comp);

            // 3. p99 Latency comparison (tracked in report; regression if current > baseline by > threshold_pct * 2.0 and > 200µs)
            let delta_p99 = if base.latency.p99_us > 0.0 {
                ((current.latency.p99_us - base.latency.p99_us) / base.latency.p99_us) * 100.0
            } else {
                0.0
            };
            let abs_diff_p99 = current.latency.p99_us - base.latency.p99_us;
            let p99_regression =
                delta_p99 > (threshold_pct * 2.0) && abs_diff_p99 > (MIN_NOISE_FLOOR_US * 2.0);
            let p99_comp = MetricComparison {
                bench_name: current.name.clone(),
                metric_name: "p99 Latency (µs)".into(),
                baseline_val: base.latency.p99_us,
                current_val: current.latency.p99_us,
                delta_pct: delta_p99,
                is_regression: p99_regression,
            };
            if p99_regression {
                regressions.push(p99_comp.clone());
            }
            comparisons.push(p99_comp);
        }

        let passed = regressions.is_empty();
        RegressionReport {
            passed,
            threshold_pct,
            comparisons,
            regressions,
        }
    }
}

/// Comparison entry for a single metric across runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricComparison {
    pub bench_name: String,
    pub metric_name: String,
    pub baseline_val: f64,
    pub current_val: f64,
    pub delta_pct: f64,
    pub is_regression: bool,
}

/// Report summarizing baseline regression check.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegressionReport {
    pub passed: bool,
    pub threshold_pct: f64,
    pub comparisons: Vec<MetricComparison>,
    pub regressions: Vec<MetricComparison>,
}

impl RegressionReport {
    /// Render markdown summary of the comparison diff.
    pub fn to_markdown_summary(&self) -> String {
        let mut md = String::new();
        md.push_str("### Performance Regression Check Summary\n\n");
        md.push_str(&format!(
            "- **Status**: {}\n",
            if self.passed {
                "✅ PASSED (No regressions exceeding threshold)"
            } else {
                "❌ FAILED (Performance regression detected)"
            }
        ));
        md.push_str(&format!(
            "- **Regression Threshold**: ±{:.1}%\n\n",
            self.threshold_pct
        ));

        md.push_str("| Benchmark | Metric | Baseline | Current | Delta | Status |\n");
        md.push_str("| :--- | :--- | :---: | :---: | :---: | :---: |\n");

        for comp in &self.comparisons {
            let status = if comp.is_regression {
                "❌ REGRESSION"
            } else if comp.delta_pct.abs() <= self.threshold_pct {
                "✅ STABLE"
            } else if (comp.metric_name.contains("Throughput") && comp.delta_pct > 0.0)
                || (comp.metric_name.contains("Latency") && comp.delta_pct < 0.0)
            {
                "🚀 FASTER"
            } else {
                "⚠️ SHIFT"
            };

            let sign = if comp.delta_pct > 0.0 { "+" } else { "" };
            md.push_str(&format!(
                "| {} | {} | {:.2} | {:.2} | {}{:.1}% | {} |\n",
                comp.bench_name,
                comp.metric_name,
                comp.baseline_val,
                comp.current_val,
                sign,
                comp.delta_pct,
                status
            ));
        }

        md
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_histogram_computes_percentiles_accurately() {
        let mut hist = LatencyHistogram::with_capacity(1000);
        for i in 1..=1000 {
            hist.record_us(i as f64);
        }

        let summary = hist.summary();
        assert_eq!(summary.count, 1000);
        assert_eq!(summary.min_us, 1.0);
        assert_eq!(summary.max_us, 1000.0);
        assert_eq!(summary.p50_us, 500.0);
        assert_eq!(summary.p90_us, 900.0);
        assert_eq!(summary.p95_us, 950.0);
        assert_eq!(summary.p99_us, 990.0);
        assert_eq!(summary.p99_9_us, 999.0);
    }

    #[test]
    fn write_amp_calculates_ratio() {
        let wa = WriteAmpSummary::new(1000, 2500);
        assert_eq!(wa.amplification_ratio, 2.5);
    }

    #[test]
    fn regression_detector_flags_slowdowns() {
        let baseline = BenchmarkSuiteReport {
            title: "Baseline".into(),
            timestamp: "2026-08-16".into(),
            git_commit: "base123".into(),
            rustc_version: "rustc 1.97".into(),
            results: vec![BenchmarkResult {
                name: "ycsb_a".into(),
                category: "YCSB".into(),
                total_ops: 1000,
                duration_ms: 100.0,
                ops_per_sec: 10000.0,
                latency: LatencySummary {
                    count: 1000,
                    min_us: 10.0,
                    avg_us: 100.0,
                    p50_us: 90.0,
                    p90_us: 150.0,
                    p95_us: 180.0,
                    p99_us: 200.0,
                    p99_9_us: 250.0,
                    max_us: 300.0,
                },
                cache_stats: None,
                write_amp: None,
            }],
        };

        let mut current = baseline.clone();
        // 20% throughput drop (below -10% threshold)
        current.results[0].ops_per_sec = 8000.0;
        let report = current.compare(&baseline, 10.0);
        assert!(!report.passed);
        assert_eq!(report.regressions.len(), 1);
        assert_eq!(report.regressions[0].metric_name, "Throughput (ops/sec)");
    }
}
