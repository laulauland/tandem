mod bench_support;

use anyhow::Result;
use bench_support::{
    measure_parallel_throughput, now_epoch_secs, write_json_artifact, BenchHarness, ClientMode,
    FILES_PER_COMMIT, RTT_PROFILES,
};
use serde::Serialize;

const AGENTS: usize = 2;
const COMMITS_PER_AGENT: usize = 4;

#[derive(Debug, Serialize)]
struct ProfileThroughputResult {
    profile: String,
    rtt_ms: u64,
    agents: usize,
    commits_per_agent: usize,
    baseline_commits_per_sec: f64,
    optimized_commits_per_sec: f64,
    speedup: f64,
}

#[derive(Debug, Serialize)]
struct ThroughputReport {
    generated_at_epoch_secs: u64,
    agents: usize,
    commits_per_agent: usize,
    files_per_commit: usize,
    results: Vec<ProfileThroughputResult>,
}

fn main() -> Result<()> {
    let mut results = Vec::new();

    for profile in RTT_PROFILES {
        let baseline_harness = BenchHarness::start()?;
        let baseline_commits_per_sec = measure_parallel_throughput(
            &baseline_harness,
            profile.rtt_ms,
            &format!("throughput-{}", profile.name),
            ClientMode::Baseline,
            AGENTS,
            COMMITS_PER_AGENT,
        )?;
        drop(baseline_harness);

        let optimized_harness = BenchHarness::start()?;
        let optimized_commits_per_sec = measure_parallel_throughput(
            &optimized_harness,
            profile.rtt_ms,
            &format!("throughput-{}", profile.name),
            ClientMode::Optimized,
            AGENTS,
            COMMITS_PER_AGENT,
        )?;

        let speedup = optimized_commits_per_sec / baseline_commits_per_sec;

        println!(
            "profile={} rtt_ms={} baseline_cps={:.3} optimized_cps={:.3} speedup={:.3}x",
            profile.name,
            profile.rtt_ms,
            baseline_commits_per_sec,
            optimized_commits_per_sec,
            speedup
        );

        results.push(ProfileThroughputResult {
            profile: profile.name.to_string(),
            rtt_ms: profile.rtt_ms,
            agents: AGENTS,
            commits_per_agent: COMMITS_PER_AGENT,
            baseline_commits_per_sec,
            optimized_commits_per_sec,
            speedup,
        });
    }

    let report = ThroughputReport {
        generated_at_epoch_secs: now_epoch_secs(),
        agents: AGENTS,
        commits_per_agent: COMMITS_PER_AGENT,
        files_per_commit: FILES_PER_COMMIT,
        results,
    };

    let artifact = write_json_artifact(
        "docs/benchmarks/tcp_inflight_throughput_latest.json",
        &report,
    )?;
    println!("wrote {}", artifact.display());
    Ok(())
}
