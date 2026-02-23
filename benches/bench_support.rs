use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use tempfile::TempDir;

pub const BENCH_DISABLE_OPTIMISTIC_VERSION_ENV: &str =
    "TANDEM_BENCH_DISABLE_OPTIMISTIC_OP_HEAD_VERSION_CACHE";
pub const BENCH_DISABLE_RPC_INFLIGHT_ENV: &str = "TANDEM_BENCH_DISABLE_RPC_INFLIGHT";
pub const BENCH_INJECT_RTT_MS_ENV: &str = "TANDEM_BENCH_INJECT_RTT_MS";

#[derive(Clone, Copy, Debug)]
pub struct RttProfile {
    pub name: &'static str,
    pub rtt_ms: u64,
}

pub const RTT_PROFILES: [RttProfile; 3] = [
    RttProfile {
        name: "p0_loopback",
        rtt_ms: 0,
    },
    RttProfile {
        name: "p1_rtt20ms",
        rtt_ms: 20,
    },
    RttProfile {
        name: "p2_rtt50ms",
        rtt_ms: 50,
    },
];

#[derive(Clone, Copy, Debug)]
pub enum ClientMode {
    Baseline,
    Optimized,
}

pub const FILES_PER_COMMIT: usize = 32;

impl ClientMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientMode::Baseline => "baseline",
            ClientMode::Optimized => "optimized",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub sample_count: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub mean_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub samples_ms: Vec<f64>,
}

impl Stats {
    pub fn from_samples(samples_ms: Vec<f64>) -> Result<Self> {
        if samples_ms.is_empty() {
            return Err(anyhow!("stats require at least one sample"));
        }

        let mut sorted = samples_ms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let sum: f64 = sorted.iter().sum();
        let mean_ms = sum / sorted.len() as f64;
        let min_ms = *sorted.first().unwrap();
        let max_ms = *sorted.last().unwrap();

        Ok(Self {
            sample_count: sorted.len(),
            p50_ms: percentile(&sorted, 0.50),
            p95_ms: percentile(&sorted, 0.95),
            mean_ms,
            min_ms,
            max_ms,
            samples_ms,
        })
    }
}

pub fn percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let clamped = percentile.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * clamped).round() as usize;
    sorted[idx]
}

pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

pub fn write_json_artifact<T: Serialize>(relative_path: &str, value: &T) -> Result<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create artifact dir {}", parent.display()))?;
    }
    fs::write(&path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write artifact {}", path.display()))?;
    Ok(path)
}

pub struct BenchHarness {
    root: TempDir,
    pub home: PathBuf,
    pub server_addr: String,
    server: Child,
}

impl BenchHarness {
    pub fn start() -> Result<Self> {
        let root = TempDir::new().context("create temp dir for bench")?;
        let home = isolated_home(root.path())?;
        let repo = root.path().join("server-repo");
        fs::create_dir_all(&repo).context("create server repo dir")?;

        let server_addr = free_addr()?;
        let mut cmd = Command::new(tandem_bin_path());
        cmd.args([
            "serve",
            "--listen",
            &server_addr,
            "--repo",
            repo.to_string_lossy().as_ref(),
            "--log-level",
            "error",
        ]);
        isolate_env(&mut cmd, &home);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let mut server = cmd.spawn().context("spawn tandem server")?;

        wait_for_server(&server_addr, &mut server)?;

        Ok(Self {
            root,
            home,
            server_addr,
            server,
        })
    }

    pub fn init_workspace(
        &self,
        endpoint: &str,
        workspace_name: &str,
        extra_env: &[(String, String)],
    ) -> Result<PathBuf> {
        let dir = self
            .root
            .path()
            .join(format!("workspace-{}", workspace_name.replace('/', "_")));
        fs::create_dir_all(&dir).context("create workspace dir")?;

        let output = run_tandem(
            &dir,
            &[
                "init",
                "--server",
                endpoint,
                "--workspace",
                workspace_name,
                ".",
            ],
            &self.home,
            extra_env,
        )?;
        ensure_ok(&output, &format!("init workspace {workspace_name}"))?;
        Ok(dir)
    }
}

impl Drop for BenchHarness {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

pub fn measure_commit_latencies(
    harness: &BenchHarness,
    profile_rtt_ms: u64,
    workspace_name: &str,
    mode: ClientMode,
    warmup_commits: usize,
    measured_commits: usize,
) -> Result<Stats> {
    let env = bench_env(profile_rtt_ms, mode);
    let workspace = harness.init_workspace(&harness.server_addr, workspace_name, &env)?;
    let src_dir = workspace.join("src");
    fs::create_dir_all(&src_dir).context("create src dir")?;

    for i in 0..warmup_commits {
        run_commit_cycle(&workspace, &harness.home, &src_dir, i, mode, &env)
            .with_context(|| format!("warmup commit {i} ({})", mode.as_str()))?;
    }

    let mut samples_ms = Vec::with_capacity(measured_commits);
    for i in 0..measured_commits {
        let start = Instant::now();
        run_commit_cycle(
            &workspace,
            &harness.home,
            &src_dir,
            warmup_commits + i,
            mode,
            &env,
        )
        .with_context(|| format!("measured commit {i} ({})", mode.as_str()))?;
        samples_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    Stats::from_samples(samples_ms)
}

pub fn measure_parallel_throughput(
    harness: &BenchHarness,
    profile_rtt_ms: u64,
    workspace_prefix: &str,
    mode: ClientMode,
    agents: usize,
    commits_per_agent: usize,
) -> Result<f64> {
    if agents == 0 || commits_per_agent == 0 {
        return Err(anyhow!("agents and commits_per_agent must be > 0"));
    }

    let env = bench_env(profile_rtt_ms, mode);
    let mut workspaces = Vec::with_capacity(agents);
    for i in 0..agents {
        workspaces.push(harness.init_workspace(
            &harness.server_addr,
            &format!("{workspace_prefix}-{}-{i}", mode.as_str()),
            &env,
        )?);
    }

    let started = Instant::now();
    let mut handles = Vec::with_capacity(agents);

    for (agent_index, workspace) in workspaces.into_iter().enumerate() {
        let home = harness.home.clone();
        let env_for_thread = env.clone();
        handles.push(thread::spawn(move || -> Result<()> {
            let src_dir = workspace.join("src");
            fs::create_dir_all(&src_dir).context("create src dir")?;
            for commit_index in 0..commits_per_agent {
                let global_index = agent_index * commits_per_agent + commit_index;
                run_commit_cycle_resilient(
                    &workspace,
                    &home,
                    &src_dir,
                    global_index,
                    mode,
                    &env_for_thread,
                )
                .with_context(|| {
                    format!(
                        "agent {agent_index} commit {commit_index} ({})",
                        mode.as_str()
                    )
                })?;
            }
            Ok(())
        }));
    }

    for handle in handles {
        handle
            .join()
            .map_err(|_| anyhow!("throughput worker thread panicked"))??;
    }

    let elapsed_secs = started.elapsed().as_secs_f64();
    let total_commits = (agents * commits_per_agent) as f64;
    Ok(total_commits / elapsed_secs)
}

fn bench_env(profile_rtt_ms: u64, mode: ClientMode) -> Vec<(String, String)> {
    let mut env = vec![(
        BENCH_INJECT_RTT_MS_ENV.to_string(),
        profile_rtt_ms.to_string(),
    )];
    if matches!(mode, ClientMode::Baseline) {
        env.push((
            BENCH_DISABLE_OPTIMISTIC_VERSION_ENV.to_string(),
            "1".to_string(),
        ));
        env.push((BENCH_DISABLE_RPC_INFLIGHT_ENV.to_string(), "1".to_string()));
    }
    env
}

fn run_commit_cycle(
    workspace: &Path,
    home: &Path,
    src_dir: &Path,
    index: usize,
    mode: ClientMode,
    extra_env: &[(String, String)],
) -> Result<()> {
    fs::create_dir_all(src_dir).context("ensure src dir")?;
    write_payload_set(src_dir, index, mode, "bench")?;

    let desc = format!("bench {} commit {index}", mode.as_str());
    run_tandem_checked(
        workspace,
        &["describe", "-m", &desc],
        home,
        extra_env,
        "describe",
    )?;
    run_tandem_checked(workspace, &["new"], home, extra_env, "new")?;
    Ok(())
}

fn run_commit_cycle_resilient(
    workspace: &Path,
    home: &Path,
    src_dir: &Path,
    index: usize,
    mode: ClientMode,
    extra_env: &[(String, String)],
) -> Result<()> {
    fs::create_dir_all(src_dir).context("ensure src dir")?;
    write_payload_set(src_dir, index, mode, "throughput")?;

    let desc = format!("throughput {} commit {index}", mode.as_str());
    run_tandem_resilient(
        workspace,
        &["describe", "-m", &desc],
        home,
        extra_env,
        "describe",
    )?;
    run_tandem_resilient(workspace, &["new"], home, extra_env, "new")?;
    Ok(())
}

fn write_payload_set(src_dir: &Path, index: usize, mode: ClientMode, label: &str) -> Result<()> {
    for file_slot in 0..FILES_PER_COMMIT {
        let file_path = src_dir.join(format!("payload_{file_slot}.rs"));
        let content = format!(
            "pub fn payload_{file_slot}_{index}() -> &'static str {{\n    \"{label} {} commit {index} file {file_slot}\"\n}}\n",
            mode.as_str()
        );
        fs::write(&file_path, content).with_context(|| {
            format!(
                "write payload file {} for commit {index}",
                file_path.display()
            )
        })?;
    }

    Ok(())
}

pub fn run_tandem(
    dir: &Path,
    args: &[&str],
    home: &Path,
    extra_env: &[(String, String)],
) -> Result<Output> {
    let mut cmd = Command::new(tandem_bin_path());
    cmd.current_dir(dir);
    isolate_env(&mut cmd, home);
    for arg in args {
        cmd.arg(arg);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().with_context(|| {
        format!(
            "run tandem {:?} in {}",
            args,
            dir.as_os_str().to_string_lossy()
        )
    })
}

fn run_tandem_checked(
    dir: &Path,
    args: &[&str],
    home: &Path,
    extra_env: &[(String, String)],
    context: &str,
) -> Result<()> {
    let output = run_tandem(dir, args, home, extra_env)?;
    ensure_ok(&output, context)
}

fn run_tandem_resilient(
    dir: &Path,
    args: &[&str],
    home: &Path,
    extra_env: &[(String, String)],
    context: &str,
) -> Result<()> {
    const MAX_RETRIES: usize = 10;

    for attempt in 1..=MAX_RETRIES {
        let output = run_tandem(dir, args, home, extra_env)?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if !is_retriable_workspace_state_error(&stderr) || attempt == MAX_RETRIES {
            return ensure_ok(
                &output,
                &format!("{context} (attempt {attempt}/{MAX_RETRIES})"),
            );
        }

        if let Some(op_id) = hinted_op_integrate_id(&stderr) {
            let _ = run_tandem(dir, &["op", "integrate", &op_id], home, extra_env)?;
        }

        let _ = run_tandem(dir, &["workspace", "update-stale"], home, extra_env)?;

        thread::sleep(Duration::from_millis(25 * attempt as u64));
    }

    Err(anyhow!("{context} exceeded retry budget"))
}

fn is_retriable_workspace_state_error(stderr: &str) -> bool {
    stderr.contains("working copy is stale")
        || stderr.contains("update-stale")
        || stderr.contains("seems to be a sibling of the working copy's operation")
        || (stderr.contains("reconcile divergent operation heads")
            && stderr.contains("already exists"))
}

fn hinted_op_integrate_id(stderr: &str) -> Option<String> {
    let marker = "jj op integrate ";
    let line = stderr.lines().find(|line| line.contains(marker))?;
    let after = line.split(marker).nth(1)?;
    let id = after.split('`').next()?.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn ensure_ok(output: &Output, context: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }

    Err(anyhow!(
        "{context} failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn isolated_home(root: &Path) -> Result<PathBuf> {
    let home = root.join("fake-home");
    fs::create_dir_all(&home).context("create fake home")?;

    let config_dir = home.join(".config/jj");
    fs::create_dir_all(&config_dir).context("create jj config dir")?;
    fs::write(
        config_dir.join("config.toml"),
        "user.name = \"Bench User\"\nuser.email = \"bench@tandem.dev\"\n[fsmonitor]\nbackend = \"none\"\n",
    )
    .context("write jj config")?;
    Ok(home)
}

fn isolate_env(cmd: &mut Command, home: &Path) {
    cmd.env("HOME", home);
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
}

fn free_addr() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind free addr")?;
    let port = listener.local_addr().context("read local addr")?.port();
    Ok(format!("127.0.0.1:{port}"))
}

fn wait_for_server(addr: &str, child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
    Err(anyhow!("server {addr} failed to start before deadline"))
}

fn tandem_bin_path() -> &'static PathBuf {
    static TANDEM_BIN: OnceLock<PathBuf> = OnceLock::new();
    TANDEM_BIN.get_or_init(resolve_tandem_bin_path)
}

fn resolve_tandem_bin_path() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_tandem") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        root.join("target/release/tandem"),
        root.join("target/debug/tandem"),
    ];
    for candidate in candidates {
        if candidate.exists() {
            return candidate;
        }
    }

    let status = Command::new("cargo")
        .current_dir(&root)
        .args(["build", "--release", "--bin", "tandem"])
        .status();

    if let Ok(s) = status {
        if s.success() {
            let release_bin = root.join("target/release/tandem");
            if release_bin.exists() {
                return release_bin;
            }
        }
    }

    panic!("unable to locate tandem binary for benchmarks")
}
