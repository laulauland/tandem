//! The gate metric: how long a file change takes to become durable.
//!
//! What is timed is one call to the daemon's own `snapshot_once` — the
//! filesystem scan, the tree write, the operation, and the head update the
//! server only acknowledges once the write has reached the bucket. It is the
//! library call the daemon makes and not a reimplementation of it, so that a
//! change that slows the product down cannot leave this number alone.
//!
//! The debounce window is deliberately *not* in the measurement. That window is
//! a policy number a person sets (`--debounce-ms`, `TANDEM_DEBOUNCE_MS`) and it
//! would swamp everything else; what this bench answers is the question the
//! window is set against — how much time the machinery itself costs, once it
//! has been told to go.
//!
//! Two tiers, like the bucket tests:
//!
//!   cargo bench --bench snapshot_publish_latency
//!       the filesystem bucket backend — no container needed
//!
//!   docker run -d --name seaweed-test -p 8333:8333 \
//!       chrislusf/seaweedfs:4.42 server -s3
//!   TANDEM_TEST_S3_BUCKET='s3://tandem-bench?endpoint=http://127.0.0.1:8333&anonymous=true' \
//!       cargo bench --bench snapshot_publish_latency
//!       a real S3 API, which is the tier the stage's number is recorded from
//!
//! Either way the report lands under `target/benchmarks/`. Add
//! `TANDEM_BENCH_RECORD=1` to write it to `docs/benchmarks/` instead — that is
//! how a number gets committed, and it is meant to be a decision rather than a
//! side effect of having run the bench.

mod bench_support;

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Context, Result};
use bench_support::{
    ensure_ok, free_addr, isolate_env, isolated_home, now_epoch_secs, run_tandem, tandem_bin_path,
    wait_for_server, write_json_artifact, Stats,
};
use serde::Serialize;
use tempfile::TempDir;

use jj_tandem::daemon::{Daemon, DaemonOptions, SnapshotOutcome};

/// Snapshots taken before any is counted. The first one pays for a cold client
/// cache and a cold connection pool, and neither is what this measures.
const WARMUP_ROUNDS: usize = 3;

/// Enough samples for a p95 to mean something without the bench taking longer
/// than a person will wait for it.
const MEASURED_ROUNDS: usize = 40;

/// How many files each round rewrites. A round that touched one file would
/// measure the round trip and nothing else; a working directory an agent has
/// been in changes several at once.
const FILES_PER_ROUND: usize = 8;

const WORKSPACE: &str = "bench";

#[derive(Debug, Serialize)]
struct Report {
    generated_at_epoch_secs: u64,
    /// `s3` or `filesystem` — the two are not comparable, so the number is
    /// meaningless without it.
    bucket_tier: String,
    /// Where the bucket was, without the prefix this run made up for itself. A
    /// committed artifact naming a temporary directory that no longer exists
    /// tells a later reader nothing.
    bucket_location: String,
    warmup_rounds: usize,
    files_per_round: usize,
    snapshot_publish: Stats,
}

fn main() -> Result<()> {
    let root = TempDir::new().context("create the bench's temp directory")?;
    let home = isolated_home(root.path())?;
    let repo = root.path().join("server-repo");
    fs::create_dir_all(&repo).context("create the server repo directory")?;

    let (tier, location, bucket_spec) = match std::env::var("TANDEM_TEST_S3_BUCKET") {
        Ok(base) => ("s3".to_string(), base.clone(), unique_prefix(&base)),
        Err(_) => {
            let dir = root.path().join("bucket");
            fs::create_dir_all(&dir).context("create the filesystem bucket directory")?;
            (
                "filesystem".to_string(),
                "a temporary directory".to_string(),
                dir.to_string_lossy().into_owned(),
            )
        }
    };
    eprintln!("bucket tier: {tier} ({bucket_spec})");

    let admin_token = jj_tandem::auth::generate_admin_token();
    let addr = free_addr()?;
    let mut server = spawn_server(&repo, &addr, &bucket_spec, &admin_token, &home)?;
    wait_for_server(&addr, &mut server)?;
    let _server = KillOnDrop(server);

    // `tandem clone` is the product's own way in, and the thing that warms the
    // client cache. Measuring against a workspace built some other way would be
    // measuring some other workspace.
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).context("create the workspace directory")?;
    let cloned = run_tandem(
        &workspace,
        &[
            "clone",
            &addr,
            ".",
            "--workspace",
            WORKSPACE,
            "--token",
            &admin_token,
        ],
        &home,
        &[],
    )?;
    ensure_ok(&cloned, "clone the bench workspace")?;

    // The daemon is opened in this process, so the same environment the CLI
    // would have had has to be here too: jj reads the user's identity out of
    // it, and the client cache reads where it lives.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    std::env::set_var("TANDEM_CACHE_DIR", home.join(".cache").join("tandem"));

    let settings = jj_tandem::workspace_init::user_settings_from_environment()?;
    let mut daemon = Daemon::open(&settings, &DaemonOptions::new(&workspace))
        .context("open the workspace the bench just cloned")?;

    let mut samples_ms = Vec::with_capacity(MEASURED_ROUNDS);
    for round in 0..(WARMUP_ROUNDS + MEASURED_ROUNDS) {
        write_round(&workspace, round)?;
        match daemon.snapshot_once()? {
            SnapshotOutcome::Published(published) => {
                if round >= WARMUP_ROUNDS {
                    samples_ms.push(published.elapsed.as_secs_f64() * 1000.0);
                }
            }
            other => {
                return Err(anyhow!(
                    "round {round} published nothing ({other:?}); the bench measures publishes"
                ))
            }
        }
    }

    let snapshot_publish = Stats::from_samples(samples_ms)?;
    println!(
        "snapshot->publish [{tier}]  p50={:.1}ms  p95={:.1}ms  mean={:.1}ms  \
         min={:.1}ms  max={:.1}ms  n={}",
        snapshot_publish.p50_ms,
        snapshot_publish.p95_ms,
        snapshot_publish.mean_ms,
        snapshot_publish.min_ms,
        snapshot_publish.max_ms,
        snapshot_publish.sample_count,
    );

    let report = Report {
        generated_at_epoch_secs: now_epoch_secs(),
        bucket_tier: tier.clone(),
        bucket_location: location,
        warmup_rounds: WARMUP_ROUNDS,
        files_per_round: FILES_PER_ROUND,
        snapshot_publish,
    };
    // Under `target/` unless `TANDEM_BENCH_RECORD` says otherwise, so that
    // running the bench is not itself a change to the revision.
    let artifact = write_json_artifact(
        &format!("docs/benchmarks/snapshot_publish_latency_{tier}_latest.json"),
        &report,
    )?;
    println!("wrote {}", artifact.display());
    Ok(())
}

/// One round's worth of edits: the same file names every time, so that what is
/// measured is a change and not a growing tree.
fn write_round(workspace: &Path, round: usize) -> Result<()> {
    let dir = workspace.join("src");
    fs::create_dir_all(&dir).context("create the round's directory")?;
    for file in 0..FILES_PER_ROUND {
        let path = dir.join(format!("file-{file:02}.txt"));
        fs::write(&path, format!("round {round}, file {file}\n"))
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// A bucket location this run alone writes to, so two runs against one
/// SeaweedFS never read each other's WAL.
fn unique_prefix(base: &str) -> String {
    let unique = format!(
        "snapshot-publish-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    );
    match base.split_once('?') {
        Some((location, query)) => format!("{}/{unique}?{query}", location.trim_end_matches('/')),
        None => format!("{}/{unique}", base.trim_end_matches('/')),
    }
}

fn spawn_server(
    repo: &Path,
    addr: &str,
    bucket_spec: &str,
    admin_token: &str,
    home: &Path,
) -> Result<Child> {
    let mut cmd = Command::new(tandem_bin_path());
    cmd.args([
        "serve",
        "--listen",
        addr,
        "--repo",
        repo.to_string_lossy().as_ref(),
        "--bucket",
        bucket_spec,
        "--log-level",
        "error",
    ]);
    isolate_env(&mut cmd, home);
    cmd.env("TANDEM_ADMIN_TOKEN", admin_token);
    cmd.stdout(Stdio::null()).stderr(Stdio::inherit());
    cmd.spawn().context("spawn the bench's tandem server")
}

/// So that a bench that fails takes its server with it.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
