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
//! Tiers, like the bucket tests:
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
//!   TANDEM_BENCH_SERVER=https://tandem-bench.exe.xyz \
//!   TANDEM_BENCH_TOKEN=tdma_… \
//!       cargo bench --bench snapshot_publish_latency
//!       a server that is already running somewhere else, with a bucket of its
//!       own. This is the real-distance run: the two loopback tiers say what
//!       the machinery costs, and only a client that is actually far from its
//!       server says what the product costs.
//!
//! `TANDEM_BENCH_INJECT_RTT_MS=<n>` adds a fixed delay to every client
//! request. It is a stand-in for distance and is labelled as one — a real
//! network has jitter, loss and a bandwidth-delay product, and an injected
//! constant has none of the three. It is what to reach for when no far-away
//! server is available, and it is never a substitute for the run above.
//!
//! Reports land under `target/benchmarks/`. Set `TANDEM_BENCH_OUTPUT_DIR`
//! to an absolute directory to retain a measurement outside the checkout.

mod bench_support;

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Context, Result};
use bench_support::{
    ensure_ok, free_addr, isolate_env, isolated_home, now_epoch_secs, run_tandem, tandem_bin_path,
    wait_for_server, write_json_artifact, Stats, BENCH_INJECT_RTT_MS_ENV,
};
use serde::Serialize;
use tempfile::TempDir;

use jj_tandem_workspace::{Daemon, DaemonOptions, SnapshotOutcome};

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

/// A server that is already running somewhere else, and the bearer it accepts.
/// Both or neither: an address with no token cannot get past the handshake.
const REMOTE_SERVER_ENV: &str = "TANDEM_BENCH_SERVER";
const REMOTE_TOKEN_ENV: &str = "TANDEM_BENCH_TOKEN";

#[derive(Debug, Serialize)]
struct Report {
    generated_at_epoch_secs: u64,
    /// `s3` or `filesystem` — the two are not comparable, so the number is
    /// meaningless without it. A run against a server the bench did not start
    /// says `the server's own`, because the bench does not know and must not
    /// guess.
    bucket_tier: String,
    /// Where the bucket was, without the prefix this run made up for itself. A
    /// committed artifact naming a temporary directory that no longer exists
    /// tells a later reader nothing.
    bucket_location: String,
    /// Where the server was. A p50 is a different claim depending on whether
    /// the server was a subprocess on loopback or a machine in another
    /// country, and the two must never be read as one series.
    server: String,
    /// Milliseconds added to every client request, if any. A number here means
    /// the run is an emulation of distance and not a measurement of it.
    injected_rtt_ms: u64,
    warmup_rounds: usize,
    files_per_round: usize,
    snapshot_publish: Stats,
}

/// What the run measures against: a server this process starts and owns, or
/// one that was already there.
enum ServerUnderTest {
    /// Started here, killed when the bench ends.
    Spawned {
        addr: String,
        token: String,
        _child: KillOnDrop,
    },
    /// Somebody else's, left alone.
    Remote { addr: String, token: String },
}

impl ServerUnderTest {
    fn addr(&self) -> &str {
        match self {
            Self::Spawned { addr, .. } | Self::Remote { addr, .. } => addr,
        }
    }

    fn token(&self) -> &str {
        match self {
            Self::Spawned { token, .. } | Self::Remote { token, .. } => token,
        }
    }

    /// What the report says about where the server was.
    fn describe(&self) -> String {
        match self {
            Self::Spawned { .. } => "a subprocess on loopback".to_string(),
            Self::Remote { addr, .. } => addr.clone(),
        }
    }
}

/// The server this run measures against, and what its bucket has to be called
/// in the report: the tier, the location, and the server itself.
///
/// Either the run was pointed at a server that is already up — in which case
/// the bucket behind it is the server's business and the bench must not guess —
/// or the bench starts one here, over an S3 bucket when
/// `TANDEM_TEST_S3_BUCKET` names one and a directory otherwise.
fn server_under_test(root: &Path, home: &Path) -> Result<(String, String, ServerUnderTest)> {
    if let Some(addr) = env_value(REMOTE_SERVER_ENV) {
        let token = env_value(REMOTE_TOKEN_ENV).ok_or_else(|| {
            anyhow!("{REMOTE_SERVER_ENV} is set, so {REMOTE_TOKEN_ENV} has to be too")
        })?;
        // The bench did not choose this server's storage and cannot see it, so
        // both halves of the answer are the same disclaimer.
        let the_servers_own = "the server's own".to_string();
        return Ok((
            the_servers_own.clone(),
            the_servers_own,
            ServerUnderTest::Remote { addr, token },
        ));
    }

    let repo = root.join("server-repo");
    fs::create_dir_all(&repo).context("create the server repo directory")?;
    let (tier, location, bucket_spec) = match std::env::var("TANDEM_TEST_S3_BUCKET") {
        Ok(base) => ("s3".to_string(), base.clone(), unique_prefix(&base)),
        Err(_) => {
            let dir = root.join("bucket");
            fs::create_dir_all(&dir).context("create the filesystem bucket directory")?;
            (
                "filesystem".to_string(),
                "a temporary directory".to_string(),
                dir.to_string_lossy().into_owned(),
            )
        }
    };
    let admin_token = jj_tandem_server::generate_admin_token();
    let addr = free_addr()?;
    let mut child = spawn_server(&repo, &addr, &bucket_spec, &admin_token, home)?;
    wait_for_server(&addr, &mut child, Some(&admin_token))?;
    Ok((
        tier,
        location,
        ServerUnderTest::Spawned {
            addr,
            token: admin_token,
            _child: KillOnDrop(child),
        },
    ))
}

fn main() -> Result<()> {
    // Observation only: no requests, file edits, or workload scheduling change.
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter("jj_tandem_workspace=debug,jj_tandem_client=debug")
        .try_init()
        .map_err(|error| anyhow::anyhow!("profile subscriber: {error}"))?;
    let root = TempDir::new().context("create the bench's temp directory")?;
    let home = isolated_home(root.path())?;

    let injected_rtt_ms = env_value(BENCH_INJECT_RTT_MS_ENV)
        .map(|raw| {
            raw.parse::<u64>().with_context(|| {
                format!("{BENCH_INJECT_RTT_MS_ENV} must be a number of milliseconds")
            })
        })
        .transpose()?
        .unwrap_or(0);

    let (tier, location, server) = server_under_test(root.path(), &home)?;
    let label = artifact_label(&tier, &server, injected_rtt_ms);
    eprintln!(
        "bucket tier: {tier}  server: {}  injected rtt: {injected_rtt_ms}ms",
        server.describe()
    );

    let addr = server.addr().to_string();
    let admin_token = server.token().to_string();

    // `tandem clone` is the product's own way in, and the thing that warms the
    // client cache. Measuring against a workspace built some other way would be
    // measuring some other workspace.
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).context("create the workspace directory")?;
    let cloned = run_tandem(
        &workspace,
        &["clone", &addr, ".", "--workspace", WORKSPACE],
        &home,
        &[("TANDEM_TOKEN".to_string(), admin_token)],
    )?;
    ensure_ok(&cloned, "clone the bench workspace")?;

    // The daemon is opened in this process, so the same environment the CLI
    // would have had has to be here too: jj reads the user's identity out of
    // it, and the client cache reads where it lives.
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    std::env::set_var("TANDEM_CACHE_DIR", home.join(".cache").join("tandem"));

    let settings = jj_tandem_workspace::user_settings_from_environment()?;
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
        "snapshot->publish [{label}]  p50={:.1}ms  p95={:.1}ms  mean={:.1}ms  \
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
        bucket_tier: tier,
        bucket_location: location,
        server: server.describe(),
        injected_rtt_ms,
        warmup_rounds: WARMUP_ROUNDS,
        files_per_round: FILES_PER_ROUND,
        snapshot_publish,
    };
    // Under `target/` unless `TANDEM_BENCH_RECORD` says otherwise, so that
    // running the bench is not itself a change to the revision.
    let artifact = write_json_artifact(
        &format!("snapshot_publish_latency_{label}_latest.json"),
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

/// An environment variable with something in it.
fn env_value(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The name the artifact is filed under.
///
/// Each tier gets its own file rather than overwriting one, because these
/// numbers are only meaningful next to each other: a p50 from a loopback
/// subprocess and a p50 from a server across an ocean answer different
/// questions, and a single `..._latest.json` would let the second silently
/// replace the first.
/// The injected delay is part of the name wherever it is part of the run,
/// remote included. `real_distance` names a measurement of a real link; a
/// remote run with 50 ms added to every request is not one, and filing it
/// under that name would let an emulation quietly replace the number the whole
/// tier exists to produce.
fn artifact_label(tier: &str, server: &ServerUnderTest, injected_rtt_ms: u64) -> String {
    let base = match server {
        ServerUnderTest::Remote { .. } => "real_distance",
        ServerUnderTest::Spawned { .. } => tier,
    };
    match injected_rtt_ms {
        0 => base.to_string(),
        rtt => format!("{base}_injected_rtt{rtt}ms"),
    }
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

// This local mode collects client/workspace timings only. Full host attribution
// uses an external host with debug/trace JSON logging captured independently.
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
