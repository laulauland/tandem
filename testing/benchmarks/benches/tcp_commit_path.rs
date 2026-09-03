mod bench_support;

use anyhow::Result;
use bench_support::{
    measure_commit_latencies, now_epoch_secs, write_json_artifact, BenchHarness, ClientMode,
    FILES_PER_COMMIT, RTT_PROFILES,
};
use serde::Serialize;

const WARMUP_COMMITS: usize = 4;
const MEASURED_COMMITS: usize = 16;

#[derive(Debug, Serialize)]
struct ProfileLatencyResult {
    profile: String,
    rtt_ms: u64,
    baseline: bench_support::Stats,
    optimized: bench_support::Stats,
    p50_improvement_percent: f64,
    p95_improvement_percent: f64,
}

#[derive(Debug, Serialize)]
struct CommitLatencyReport {
    generated_at_epoch_secs: u64,
    warmup_commits: usize,
    measured_commits: usize,
    files_per_commit: usize,
    results: Vec<ProfileLatencyResult>,
}

fn main() -> Result<()> {
    let mut results = Vec::new();

    for profile in RTT_PROFILES {
        let baseline_harness = BenchHarness::start()?;
        let baseline = measure_commit_latencies(
            &baseline_harness,
            profile.rtt_ms,
            &format!("lat-{}-baseline", profile.name),
            ClientMode::Baseline,
            WARMUP_COMMITS,
            MEASURED_COMMITS,
        )?;
        drop(baseline_harness);

        let optimized_harness = BenchHarness::start()?;
        let optimized = measure_commit_latencies(
            &optimized_harness,
            profile.rtt_ms,
            &format!("lat-{}-optimized", profile.name),
            ClientMode::Optimized,
            WARMUP_COMMITS,
            MEASURED_COMMITS,
        )?;

        let p50_improvement_percent =
            ((baseline.p50_ms - optimized.p50_ms) / baseline.p50_ms) * 100.0;
        let p95_improvement_percent =
            ((baseline.p95_ms - optimized.p95_ms) / baseline.p95_ms) * 100.0;

        println!(
            "profile={} rtt_ms={} baseline_p95_ms={:.2} optimized_p95_ms={:.2} p95_improvement={:.2}%",
            profile.name,
            profile.rtt_ms,
            baseline.p95_ms,
            optimized.p95_ms,
            p95_improvement_percent
        );

        results.push(ProfileLatencyResult {
            profile: profile.name.to_string(),
            rtt_ms: profile.rtt_ms,
            baseline,
            optimized,
            p50_improvement_percent,
            p95_improvement_percent,
        });
    }

    let report = CommitLatencyReport {
        generated_at_epoch_secs: now_epoch_secs(),
        warmup_commits: WARMUP_COMMITS,
        measured_commits: MEASURED_COMMITS,
        files_per_commit: FILES_PER_COMMIT,
        results,
    };

    let artifact = write_json_artifact("docs/benchmarks/tcp_commit_path_latest.json", &report)?;
    println!("wrote {}", artifact.display());
    Ok(())
}
