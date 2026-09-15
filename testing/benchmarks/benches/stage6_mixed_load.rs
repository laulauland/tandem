//! One native hosted process, ten named repositories, and an explicit load mix.
//! See docs/benchmarks/stage6-workload.md for the frozen measurement contract.
mod bench_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bench_support::{
    free_addr, isolate_env, isolated_home, tandem_bin_path, wait_for_server, write_json_artifact,
};
use reqwest::blocking::{Client, Response};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Host {
    origin: String,
    owner: String,
    namespace: String,
    child: Option<Child>,
    log: Option<PathBuf>,
    ssh_host: Option<String>,
    local_secret: Option<String>,
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Host {
    fn start(root: &Path, home: &Path) -> Result<Self> {
        if let Ok(origin) = std::env::var("TANDEM_STAGE6_HOST") {
            let url = reqwest::Url::parse(&origin)?;
            let ssh_host = url
                .host_str()
                .context("host URL has no hostname")?
                .to_string();
            if !ssh_host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
                || ssh_host.starts_with('-')
            {
                bail!("invalid monitoring hostname");
            }
            return Ok(Self {
                origin: origin.trim_end_matches('/').to_string(),
                owner: std::env::var("TANDEM_STAGE6_OWNER_TOKEN")
                    .context("missing owner environment credential")?,
                namespace: namespace()?,
                child: None,
                log: None,
                ssh_host: Some(ssh_host),
                local_secret: None,
            });
        }
        let addr = free_addr()?;
        let local_secret = jj_tandem_server::generate_admin_token();
        let log_path = root.join("server.jsonl");
        let log = fs::File::create(&log_path)?;
        let mut command = Command::new(tandem_bin_path());
        command
            .args(["serve", "--hosted", "--listen", &addr, "--repo"])
            .arg(root.join("server-cache"))
            .arg("--bucket")
            .arg(root.join("bucket"))
            .args(["--log-level", "trace", "--log-format", "json"]);
        isolate_env(&mut command, home);
        command
            .env("TANDEM_ADMIN_TOKEN", &local_secret)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        let child = command.spawn()?;
        let mut host = Self {
            origin: format!("http://{addr}"),
            owner: String::new(),
            namespace: namespace()?,
            child: Some(child),
            log: Some(log_path),
            ssh_host: None,
            local_secret: Some(local_secret),
        };
        wait_for_server(&addr, host.child.as_mut().unwrap(), None)?;
        // Authenticated mint proves readiness belongs to this newly spawned host.
        host.owner = client()?
            .post(format!("{}/api/owners", host.origin))
            .bearer_auth(host.local_secret.as_ref().unwrap())
            .send()?
            .error_for_status()?
            .json::<Value>()?
            .get("token")
            .and_then(Value::as_str)
            .context("owner response")?
            .to_string();
        Ok(host)
    }
    fn repository(&self, index: usize) -> String {
        format!("{}/{}/load-{index:02}", self.origin, self.namespace)
    }
    fn provision(&self) -> Result<Vec<String>> {
        let client = client()?;
        let mut tokens = Vec::new();
        for index in 0..10 {
            let repository = self.repository(index);
            client
                .put(&repository)
                .bearer_auth(&self.owner)
                .send()?
                .error_for_status()?;
            let body = client
                .post(format!("{repository}/api/tokens"))
                .bearer_auth(&self.owner)
                .json(&json!({"workspaceId":format!("load-{index:02}"),"ttlSeconds":7200}))
                .send()?
                .error_for_status()?
                .json::<Value>()?;
            tokens.push(
                body.get("token")
                    .and_then(Value::as_str)
                    .context("scoped token response")?
                    .to_string(),
            );
        }
        Ok(tokens)
    }
    fn warm_all(&self, tokens: &[String]) -> Result<()> {
        let client = client()?;
        for (index, token) in tokens.iter().enumerate() {
            client
                .get(format!("{}/api/info", self.repository(index)))
                .bearer_auth(token)
                .send()?
                .error_for_status()?;
        }
        Ok(())
    }
    fn rss(&self) -> Result<u64> {
        self.memory("VmRSS:")
    }
    fn memory(&self, field: &str) -> Result<u64> {
        let text = if let Some(child) = &self.child {
            fs::read_to_string(format!("/proc/{}/status", child.id()))?
        } else {
            let output = Command::new("ssh").args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", self.ssh_host.as_ref().unwrap(),
                "pid=$(systemctl show tandem.service --property=MainPID --value); cat /proc/$pid/status"]).output()?;
            if !output.status.success() {
                bail!("remote memory probe failed");
            }
            String::from_utf8(output.stdout)?
        };
        text.lines()
            .find_map(|line| {
                line.strip_prefix(field)
                    .and_then(|v| v.split_whitespace().next())
                    .and_then(|v| v.parse().ok())
            })
            .context("process memory counter unavailable")
    }
    fn restart(&mut self, home: &Path) -> Result<()> {
        if let Some(child) = &mut self.child {
            child.kill()?;
            child.wait()?;
            let root = self.log.as_ref().unwrap().parent().unwrap();
            let log = fs::OpenOptions::new()
                .append(true)
                .open(self.log.as_ref().unwrap())?;
            let addr = self
                .origin
                .strip_prefix("http://")
                .context("local origin")?;
            let mut command = Command::new(tandem_bin_path());
            command
                .args(["serve", "--hosted", "--listen", addr, "--repo"])
                .arg(root.join("server-cache"))
                .arg("--bucket")
                .arg(root.join("bucket"))
                .args(["--log-level", "trace", "--log-format", "json"]);
            isolate_env(&mut command, home);
            command
                .env("TANDEM_ADMIN_TOKEN", self.local_secret.as_ref().unwrap())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log));
            *child = command.spawn()?;
            wait_for_server(addr, child, None)?;
        } else {
            let status = Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    self.ssh_host.as_ref().unwrap(),
                    "sudo systemctl restart tandem.service",
                ])
                .status()?;
            if !status.success() {
                bail!("supervised restart failed");
            }
            let status = Command::new("curl")
                .args([
                    "--fail",
                    "--silent",
                    "--show-error",
                    "--retry",
                    "12",
                    "--retry-all-errors",
                    "--retry-delay",
                    "1",
                    &format!("{}/healthz", self.origin),
                ])
                .stdout(Stdio::null())
                .status()?;
            if !status.success() {
                bail!("restarted host did not become ready");
            }
        }
        Ok(())
    }
    fn log_cursor(&self) -> Result<String> {
        if let Some(path) = &self.log {
            return Ok(fs::metadata(path)?.len().to_string());
        }
        let output = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                self.ssh_host.as_ref().unwrap(),
                "sudo journalctl -u tandem.service -n 0 --show-cursor",
            ])
            .output()?;
        if !output.status.success() {
            bail!("journal cursor probe failed");
        }
        String::from_utf8(output.stdout)?
            .split_once("-- cursor: ")
            .map(|(_, c)| c.trim().to_string())
            .context("journal cursor absent")
    }
    fn observations(&self, cursor: &str) -> Result<Value> {
        let text = if let Some(path) = &self.log {
            let bytes = fs::read(path)?;
            String::from_utf8_lossy(&bytes[cursor.parse::<usize>()?..]).into_owned()
        } else {
            if !cursor
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b";=-".contains(&b))
            {
                bail!("unexpected journal cursor syntax");
            }
            let command =
                format!("sudo journalctl -u tandem.service --output cat --after-cursor '{cursor}'");
            let output = Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    self.ssh_host.as_ref().unwrap(),
                    &command,
                ])
                .output()?;
            if !output.status.success() {
                bail!("journal collection failed");
            }
            String::from_utf8(output.stdout)?
        };
        if text.contains(&self.owner) {
            bail!("credential appeared in journal; refusing report");
        }
        let mut lock_wait = Vec::new();
        let mut lock_hold = Vec::new();
        let mut admission_wait = Vec::new();
        let mut queue_depth_samples = Vec::new();
        let mut queue_max = 0_u64;
        let mut bucket_calls = 0_u64;
        let mut bucket_read = 0_u64;
        let mut bucket_write = 0_u64;
        let mut bucket_mutations = 0_u64;
        let mut operation_attempts = std::collections::BTreeMap::<String, usize>::new();
        let mut publishes = 0;
        let mut wal_calls = 0;
        let mut wal_bytes = 0;
        let mut messages = std::collections::BTreeMap::<String, usize>::new();
        for line in text.lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let fields = &event["fields"];
            let message = fields["message"].as_str().unwrap_or("");
            if let Some(value) = fields["lock_wait_ms"].as_f64() {
                lock_wait.push(value);
            }
            if let Some(value) = fields["lock_hold_ms"].as_f64() {
                lock_hold.push(value);
            }
            if message == "rpc request" && fields["rpc_method"] == "updateOpHeads" {
                if let Some(value) = fields["admission_wait_ms"].as_f64() {
                    admission_wait.push(value);
                }
                if let Some(depth) = fields["queue_depth"].as_u64() {
                    queue_depth_samples.push(depth as f64);
                    queue_max = queue_max.max(depth);
                }
                if let Some(id) = fields["new_id"].as_str() {
                    *operation_attempts.entry(id.to_string()).or_default() += 1;
                }
            }
            if message == "bucket operation" {
                bucket_calls += fields["bucket_calls"].as_u64().unwrap_or(0);
                bucket_read += fields["bucket_read_bytes"].as_u64().unwrap_or(0);
                bucket_write += fields["bucket_write_bytes"].as_u64().unwrap_or(0);
                if matches!(
                    fields["bucket_operation"].as_str(),
                    Some("put_immutable" | "put_overwrite" | "compare_and_put")
                ) {
                    bucket_mutations += 1;
                }
            }

            *messages.entry(message.to_string()).or_default() += 1;
            if message == "rpc response"
                && fields["rpc_method"] == "updateOpHeads"
                && fields["ok"] == true
            {
                publishes += 1;
            }
            if message.to_lowercase().contains("wal") {
                wal_calls += fields["wal_bucket_calls"]
                    .as_u64()
                    .or_else(|| fields["bucket_calls"].as_u64())
                    .unwrap_or(0);
                wal_bytes += fields["wal_bucket_bytes"]
                    .as_u64()
                    .or_else(|| fields["bucket_bytes"].as_u64())
                    .unwrap_or(0);
            }
        }
        Ok(
            json!({"lock_wait":percentiles(&lock_wait),"lock_hold":percentiles(&lock_hold),"admission_wait":percentiles(&admission_wait),"queue_depth":percentiles(&queue_depth_samples),"maximum_queue_depth":queue_max,"observed_operation_attempts":operation_attempts,"maximum_observed_cas_retries":operation_attempts.values().map(|v|v.saturating_sub(1)).max().unwrap_or(0),"bucket_calls":bucket_calls,"bucket_read_payload_bytes":bucket_read,"bucket_attempted_write_payload_bytes":bucket_write,"bucket_mutating_calls":bucket_mutations,"successful_publishes":publishes,"wal_scoped_calls":wal_calls,"wal_scoped_bytes":wal_bytes,"event_counts":messages,"coverage":"bucket_calls/read/write count emitted ObjectStore boundary operations and payload bytes, not transport headers or backend-internal retries. wal_scoped fields cover WAL only. Zero absent fields do not establish instrumentation availability."}),
        )
    }
}
fn namespace() -> Result<String> {
    let value = std::env::var("TANDEM_STAGE6_NAMESPACE")
        .unwrap_or_else(|_| format!("load-{}", std::process::id()));
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("invalid load namespace");
    }
    Ok(value)
}
fn client() -> Result<Client> {
    Ok(Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?)
}
fn measure_idle(
    host: &Host,
    tokens: &[String],
    repositories: usize,
    count: usize,
) -> Result<Value> {
    let before = host.rss()?;
    let cursor = host.log_cursor()?;
    let (tx, rx) = mpsc::channel();
    let mut threads = Vec::new();
    for index in 0..count {
        let tx = tx.clone();
        let url = format!("{}/api/events", host.repository(index % repositories));
        let token = tokens[index % repositories].clone();
        threads.push(thread::spawn(move || {
            let result = (|| -> Result<Response> {
                Ok(Client::builder()
                    .timeout(Duration::from_secs(30))
                    .http1_only()
                    .build()?
                    .get(url)
                    .bearer_auth(token)
                    .send()?
                    .error_for_status()?)
            })();
            let _ = tx.send(result);
        }));
    }
    drop(tx);
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut responses = Vec::new();
    let mut failures = Vec::new();
    for _ in 0..count {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(response))
                if response
                    .headers()
                    .get("content-type")
                    .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream")) =>
            {
                responses.push(response)
            }
            Ok(Ok(_)) => failures.push("response is not an event stream".to_string()),
            Ok(Err(error)) => failures.push(error.to_string()),
            Err(_) => {
                failures.push("event stream establishment deadline".to_string());
                break;
            }
        }
    }
    for handle in threads {
        let _ = handle.join();
    }
    // A declared observation interval, not a readiness delay. Responses remain alive.
    let (_keep_open, timer) = mpsc::channel::<()>();
    let _ = timer.recv_timeout(Duration::from_secs(2));
    let during = host.rss()?;
    let observations = host.observations(&cursor)?;
    let passed = responses.len() == count
        && during.saturating_sub(before) <= 128 * 1024
        && observations["successful_publishes"] == 0
        && observations["wal_scoped_calls"] == 0
        && observations["bucket_mutating_calls"] == 0;
    let report = json!({"connections":count,"repositories":repositories,"established":responses.len(),"failures":failures,"server_rss_before_kib":before,"server_rss_during_kib":during,"server_rss_delta_kib":during as i64-before as i64,"hold_ms":2000,"observations":observations,"passed":passed});
    drop(responses);
    Ok(report)
}
fn main() -> Result<()> {
    // Observation only: no requests, file edits, or workload scheduling change.
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter("jj_tandem_workspace=debug,jj_tandem_client=debug")
        .try_init()
        .map_err(|error| anyhow::anyhow!("profile subscriber: {error}"))?;
    let root = TempDir::new()?;
    let home = isolated_home(root.path())?;
    let mut host = Host::start(root.path(), &home)?;
    let tokens = host.provision()?;
    let counts = match std::env::var("TANDEM_STAGE6_IDLE_CONNECTIONS") {
        Ok(raw) => vec![raw.parse::<usize>()?],
        Err(_) => vec![1, 10, 100],
    };
    if counts.iter().any(|v| *v == 0 || *v > 100) {
        bail!("idle connections must be in 1..=100");
    }
    let mut idle = Vec::new();
    for repositories in [1, 10] {
        for count in &counts {
            idle.push(measure_idle(&host, &tokens, repositories, *count)?);
        }
    }
    let active = if std::env::var("TANDEM_STAGE6_SKIP_ACTIVE").as_deref() == Ok("1") {
        Vec::new()
    } else {
        measure_active(&mut host, &tokens, root.path(), &home)?
    };
    let report = json!({"generated_at_epoch_secs":bench_support::now_epoch_secs(),"host":host.origin,"namespace":host.namespace,"idle":idle,"active":active,"limitations":["Filesystem event generation is open-loop; snapshot scheduling uses the declared one-second benchmark policy.","Stage 5 bucket trace coverage is scoped, not total traffic."]});
    let artifact = write_json_artifact("docs/benchmarks/stage6-mixed-load.json", &report)?;
    println!("wrote {}", artifact.display());
    if idle
        .iter()
        .chain(active.iter())
        .any(|r| r["passed"] != true)
    {
        bail!("load workload failed; see report");
    }
    Ok(())
}

use jj_tandem_workspace::{Daemon, DaemonOptions, SnapshotOutcome};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
struct Edits {
    pending: bool,
    done: bool,
    generations: Vec<(u64, f64)>,
    error: Option<String>,
}
struct WriterResult {
    workspace: PathBuf,
    writer: usize,
    attempts: Vec<Value>,
    edits: Vec<(u64, f64)>,
    final_generation: u64,
    large: bool,
}
fn wait_until(deadline: Instant) {
    // An offered-load timer, never a readiness or correctness wait.
    let (_sender, receiver) = mpsc::channel::<()>();
    let _ = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
}
fn small_payload(writer: usize, file: usize, generation: u64) -> Vec<u8> {
    let mut bytes = vec![0x6b; 64];
    bytes[..8].copy_from_slice(&generation.to_le_bytes());
    bytes[8..16].copy_from_slice(&(writer as u64).to_le_bytes());
    bytes[16..24].copy_from_slice(&(file as u64).to_le_bytes());
    bytes
}
fn write_small(path: &Path, writer: usize, generation: u64) -> Result<()> {
    fs::create_dir_all(path.join("src"))?;
    for file in 0..8 {
        let target = path.join(format!("src/file-{file}.bin"));
        // Publish complete file bytes to the scanner. The eight paths may
        // legitimately contain different generations during a scan.
        let temporary = path.join(".jj/stage6-write.tmp");
        fs::write(&temporary, small_payload(writer, file, generation))?;
        fs::rename(&temporary, &target)?;
    }
    Ok(())
}
fn large_payload(generation: u64) -> Vec<u8> {
    let mut bytes = vec![0; 32 * 1024 * 1024];
    let mut state = generation.wrapping_add(0x9e3779b97f4a7c15);
    for chunk in bytes.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    bytes
}
fn snapshot(daemon: &mut Daemon, epoch: Instant, ordinal: usize) -> Value {
    let started = Instant::now();
    match daemon.snapshot_once() {
        Ok(SnapshotOutcome::Published(p)) => {
            json!({"attempt":ordinal,"status":"published","started_ms":started.duration_since(epoch).as_secs_f64()*1000.0,"ack_ms":epoch.elapsed().as_secs_f64()*1000.0,"snapshot_ms":p.elapsed.as_secs_f64()*1000.0,"operation_id":p.operation_id,"commit_id":p.commit_id})
        }
        Ok(other) => {
            json!({"attempt":ordinal,"status":format!("{other:?}"),"started_ms":started.duration_since(epoch).as_secs_f64()*1000.0,"ack_ms":epoch.elapsed().as_secs_f64()*1000.0})
        }
        Err(error) => {
            json!({"attempt":ordinal,"status":"error","error":format!("{error:#}"),"started_ms":started.duration_since(epoch).as_secs_f64()*1000.0,"ack_ms":epoch.elapsed().as_secs_f64()*1000.0})
        }
    }
}
fn small_writer(
    mut daemon: Daemon,
    workspace: PathBuf,
    writer: usize,
    epoch: Instant,
    duration: Duration,
    interval: Duration,
    stop: Arc<AtomicBool>,
) -> Result<WriterResult> {
    let state = Arc::new((Mutex::new(Edits::default()), Condvar::new()));
    let generator_state = state.clone();
    let generator_path = workspace.clone();
    let generator = thread::spawn(move || {
        let count = (duration.as_secs_f64() / interval.as_secs_f64()).round() as u64;
        for index in 0..count {
            wait_until(epoch + interval.mul_f64(index as f64));
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let seen = epoch.elapsed().as_secs_f64() * 1000.0;
            let generation = index + 4;
            let result = write_small(&generator_path, writer, generation);
            let mut guard = generator_state.0.lock().unwrap();
            if let Err(error) = result {
                guard.error = Some(error.to_string());
                break;
            }
            guard.generations.push((generation, seen));
            guard.pending = true;
            generator_state.1.notify_one();
        }
        generator_state.0.lock().unwrap().done = true;
        generator_state.1.notify_one();
    });
    let drain_deadline = epoch + duration + Duration::from_secs(60);
    let mut attempts = Vec::new();
    let mut next_snapshot = epoch;
    loop {
        let mut guard = state.0.lock().unwrap();
        while !guard.pending && !guard.done {
            let (next, timeout) = state
                .1
                .wait_timeout(
                    guard,
                    drain_deadline.saturating_duration_since(Instant::now()),
                )
                .unwrap();
            guard = next;
            if timeout.timed_out() {
                break;
            }
        }
        if Instant::now() >= drain_deadline {
            attempts.push(json!({"status":"drain_deadline"}));
            break;
        }
        if !guard.pending && guard.done {
            break;
        }
        drop(guard);
        wait_until(next_snapshot);
        let mut guard = state.0.lock().unwrap();
        guard.pending = false;
        drop(guard);
        let began = Instant::now();
        attempts.push(snapshot(&mut daemon, epoch, attempts.len()));
        next_snapshot = began + Duration::from_secs(1);
    }
    generator
        .join()
        .map_err(|_| anyhow!("edit generator panicked"))?;
    let guard = state.0.lock().unwrap();
    if let Some(error) = &guard.error {
        bail!("edit generator: {error}");
    }
    Ok(WriterResult {
        workspace,
        writer,
        attempts,
        edits: guard.generations.clone(),
        final_generation: guard.generations.last().map_or(3, |g| g.0),
        large: false,
    })
}
fn measure_active(
    host: &mut Host,
    tokens: &[String],
    root: &Path,
    home: &Path,
) -> Result<Vec<Value>> {
    // Set process configuration before any worker starts; no process-wide test
    // knobs are changed while workers are live.
    std::env::set_var("HOME", home);
    std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    std::env::set_var("TANDEM_CACHE_DIR", home.join(".cache/tandem"));
    std::env::set_var("TANDEM_DISABLE_CACHE", "true");
    let profiles = match std::env::var("TANDEM_STAGE6_ACTIVE_PROFILE") {
        Ok(p) => vec![p],
        Err(_) => vec!["burst".to_string(), "steady".to_string()],
    };
    let mut reports = Vec::new();
    for profile in profiles {
        host.warm_all(tokens)?;
        let (offset, interval, duration) = match profile.as_str() {
            "burst" => (0, Duration::from_millis(200), Duration::from_secs(240)),
            "steady" => (4, Duration::from_secs(5), Duration::from_secs(200)),
            _ => bail!("unknown active profile"),
        };
        let smoke_seconds = std::env::var("TANDEM_STAGE6_SMOKE_SECONDS")
            .ok()
            .map(|v| v.parse::<u64>())
            .transpose()?;
        let duration = smoke_seconds.map_or(duration, Duration::from_secs);
        let mut workspaces = Vec::new();
        for index in offset..offset + 4 {
            let workspace = root.join(format!("workspace-{index}"));
            let output = bench_support::run_tandem(
                root,
                &[
                    "clone",
                    &host.repository(index),
                    workspace.to_str().unwrap(),
                    "--workspace",
                    &format!("load-{index:02}"),
                ],
                home,
                &[("TANDEM_TOKEN".to_string(), tokens[index].clone())],
            )?;
            bench_support::ensure_ok(&output, "clone load workspace")?;
            workspaces.push(workspace);
        }
        let cursor = host.log_cursor()?;
        let rss_before = host.rss()?;
        let gate = Arc::new((Mutex::new(None::<Instant>), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut handles = Vec::new();
        for (position, workspace) in workspaces.into_iter().enumerate() {
            let gate = gate.clone();
            let stop = stop.clone();
            let ready_tx = ready_tx.clone();
            handles.push(thread::spawn(move || -> Result<WriterResult> {
                let prepared = (|| -> Result<Daemon> {
                    let settings = jj_tandem_workspace::user_settings_from_environment()?;
                    let mut options = DaemonOptions::new(&workspace);
                    options.writer_ttl = Duration::from_secs(3600);
                    let mut daemon = Daemon::open(&settings, &options)?;
                    if position < 3 {
                        for generation in 1..=3 {
                            write_small(&workspace, position, generation)?;
                            if !matches!(daemon.snapshot_once()?, SnapshotOutcome::Published(_)) {
                                bail!("warmup did not publish");
                            }
                        }
                    }
                    Ok(daemon)
                })();
                let mut daemon = match prepared {
                    Ok(daemon) => daemon,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return Err(error);
                    }
                };
                ready_tx
                    .send(Ok(()))
                    .map_err(|_| anyhow!("ready receiver closed"))?;
                drop(ready_tx);
                let mut guard = gate.0.lock().unwrap();
                while guard.is_none() {
                    guard = gate.1.wait(guard).unwrap();
                }
                let epoch = guard.unwrap();
                drop(guard);
                if stop.load(Ordering::Relaxed) {
                    bail!("peer failed during readiness");
                }
                if position < 3 {
                    return small_writer(
                        daemon, workspace, position, epoch, duration, interval, stop,
                    );
                }
                let mut attempts = Vec::new();
                let mut generation = 0;
                while !stop.load(Ordering::Relaxed)
                    && Instant::now() < epoch + duration + Duration::from_secs(60)
                {
                    generation += 1;
                    fs::write(workspace.join("large.bin"), large_payload(generation))?;
                    let mut result = snapshot(&mut daemon, epoch, attempts.len());
                    result["generation"] = json!(generation);
                    let published = result["status"] == "published";
                    attempts.push(result);
                    if !published {
                        break;
                    }
                }
                Ok(WriterResult {
                    workspace,
                    writer: position,
                    attempts,
                    edits: Vec::new(),
                    final_generation: generation,
                    large: true,
                })
            }));
        }
        drop(ready_tx);
        let mut ready_error = None;
        for _ in 0..4 {
            match ready_rx.recv_timeout(Duration::from_secs(180)) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    ready_error = Some(e);
                    break;
                }
                Err(e) => {
                    ready_error = Some(e.to_string());
                    break;
                }
            }
        }
        if ready_error.is_some() {
            stop.store(true, Ordering::Relaxed);
        }
        let measurement_started_epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();
        let epoch = Instant::now();
        *gate.0.lock().unwrap() = Some(epoch);
        gate.1.notify_all();
        let mut writers = Vec::new();
        let mut failures = Vec::new();
        // Small writers finish first. Stop the uploader only after all three
        // have completed their offered-load and drain windows.
        for (position, handle) in handles.into_iter().enumerate() {
            match handle.join() {
                Ok(Ok(w)) => writers.push(w),
                Ok(Err(e)) => failures.push(e.to_string()),
                Err(_) => failures.push("writer panicked".to_string()),
            }
            if position == 2 {
                stop.store(true, Ordering::Relaxed);
            }
        }
        if let Some(error) = ready_error {
            failures.push(error);
        }
        let measurement_end_ms = epoch.elapsed().as_secs_f64() * 1000.0;
        let rss_after = host.rss()?;
        let rss_peak = host.memory("VmHWM:")?;
        let observations = host.observations(&cursor)?;
        let require_metrics = std::env::var("TANDEM_STAGE6_REQUIRE_METRICS").as_deref() == Ok("1");
        let metrics_present = observations["bucket_calls"].as_u64().unwrap_or(0) > 0
            && !observations["lock_wait"].is_null()
            && !observations["lock_hold"].is_null()
            && !observations["admission_wait"].is_null()
            && !observations["queue_depth"].is_null();
        let metrics_passed = (!require_metrics || metrics_present)
            && observations["maximum_queue_depth"]
                .as_u64()
                .is_some_and(|v| v <= 8)
            && observations["maximum_observed_cas_retries"]
                .as_u64()
                .unwrap_or(0)
                <= 8
            && (observations["admission_wait"].is_null()
                || observations["admission_wait"]["p99_ms"]
                    .as_f64()
                    .is_some_and(|v| v <= 2000.0));
        let checkpoint = json!({"profile":profile,"namespace":host.namespace,"state":"drained_before_restart","writers":writers.iter().map(|w|json!({"writer":w.writer,"large":w.large,"attempts":w.attempts,"edits":w.edits,"final_generation":w.final_generation})).collect::<Vec<_>>()});
        write_json_artifact("docs/benchmarks/stage6-active.partial", &checkpoint)?;
        host.restart(home)?;
        let mut results = Vec::new();
        let oracles: Vec<_> = writers
            .into_iter()
            .map(|writer| {
                let identity = writer.writer;
                (
                    identity,
                    thread::spawn(move || validate_writer(writer, smoke_seconds.is_some())),
                )
            })
            .collect();
        for (identity, oracle) in oracles {
            match oracle.join() {
                Ok(Ok(result)) => results.push(result),
                other => {
                    let error = match other {
                        Ok(Err(error)) => error.to_string(),
                        _ => "oracle panicked".to_string(),
                    };
                    failures.push(error.clone());
                    results.push(json!({"writer":identity,"passed":false,"oracle_error":error}));
                }
            }
        }
        let aggregate = |field: &str| {
            let values: Vec<f64> = results
                .iter()
                .filter(|w| w["large"] == false)
                .filter_map(|w| w[field]["samples_ms"].as_array())
                .flatten()
                .filter_map(Value::as_f64)
                .collect();
            percentiles(&values)
        };
        let aggregate_snapshot = aggregate("snapshot_to_ack");
        let aggregate_edit = aggregate("edit_to_durable");
        let aggregate_full_snapshot = aggregate("full_snapshot_to_ack");
        let aggregate_full_edit = aggregate("full_edit_to_durable");
        let passed = failures.is_empty()
            && results.len() == 4
            && results.iter().all(|r| r["passed"] == true)
            && rss_peak <= 2 * 1024 * 1024
            && metrics_passed;
        reports.push(json!({"profile":profile,"duration_ms":duration.as_millis(),"input_interval_ms":interval.as_millis(),"measurement_started_epoch_ms":measurement_started_epoch_ms,"measurement_end_ms":measurement_end_ms,"coordination_metric_coverage":"Trace interval includes warmups and the full active window, excludes restart and oracle reads.","required_metrics_present":metrics_present,"coordination_budgets_passed":metrics_passed,"smoke_override":smoke_seconds,"writers":results,"failures":failures,"server_rss_before_kib":rss_before,"server_rss_after_kib":rss_after,"server_peak_rss_kib":rss_peak,"restarted_before_oracle":true,"aggregate_snapshot_to_ack":aggregate_snapshot,"aggregate_edit_to_durable":aggregate_edit,"aggregate_full_snapshot_to_ack":aggregate_full_snapshot,"aggregate_full_edit_to_durable":aggregate_full_edit,"unavailable_baseline_metrics":if metrics_present { Vec::<String>::new() } else { vec!["repository lock wait/hold".to_string(),"admission queue depth/wait".to_string(),"total bucket traffic".to_string()] },"observations":observations,"passed":passed}));
    }
    Ok(reports)
}
fn read_bytes(repo: &jj_lib::repo::ReadonlyRepo, commit: &str, path: &str) -> Result<Vec<u8>> {
    use jj_lib::repo::Repo as _;
    use pollster::FutureExt as _;
    use tokio::io::AsyncReadExt as _;
    let commit = repo.store().get_commit(
        &jj_lib::backend::CommitId::try_from_hex(commit).context("invalid commit id")?,
    )?;
    let path = jj_lib::repo_path::RepoPathBuf::from_internal_string(path.to_string())?;
    let value = commit.tree().path_value(&path)?;
    let Some(jj_lib::backend::TreeValue::File { id, .. }) = value.as_normal() else {
        bail!("expected file in published tree");
    };
    let mut reader = repo.store().read_file(&path, id).block_on()?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).block_on()?;
    Ok(bytes)
}
fn percentiles(samples: &[f64]) -> Value {
    if samples.is_empty() {
        return Value::Null;
    }
    let mut ordered = samples.to_vec();
    ordered.sort_by(f64::total_cmp);
    let p = |q: f64| {
        ordered[((ordered.len() as f64 * q).ceil() as usize)
            .saturating_sub(1)
            .min(ordered.len() - 1)]
    };
    json!({"sample_count":samples.len(),"p50_ms":p(0.50),"p95_ms":p(0.95),"p99_ms":p(0.99),"samples_ms":samples})
}
fn validate_writer(mut writer: WriterResult, smoke: bool) -> Result<Value> {
    use jj_lib::object_id::ObjectId as _;
    let settings = jj_tandem_workspace::user_settings_from_environment()?;
    let loader = jj_lib::repo::RepoLoader::init_from_file_system(
        &settings,
        &writer.workspace.join(".jj/repo"),
        &jj_tandem_client::tandem_factories_with_defaults(),
    )?;
    let repo = loader.load_at_head()?;
    let mut seen = std::collections::BTreeSet::new();
    let mut frontier = vec![repo.operation().clone()];
    while let Some(operation) = frontier.pop() {
        if seen.insert(operation.id().hex()) {
            for parent in operation.parents() {
                frontier.push(parent?);
            }
        }
    }
    let reachable = writer
        .attempts
        .iter()
        .filter_map(|a| a["operation_id"].as_str())
        .all(|id| seen.contains(id));
    let mut previous = [3u64; 8];
    let mut snapshot_samples = Vec::new();
    let mut edit_samples = Vec::new();
    let mut full_snapshot_samples = Vec::new();
    let mut full_edit_samples = Vec::new();
    let mut bytes_ok = true;
    if !writer.large {
        for (ordinal, attempt) in writer.attempts.iter_mut().enumerate() {
            let Some(commit) = attempt["commit_id"].as_str().map(str::to_string) else {
                continue;
            };
            let mut generations = Vec::new();
            let mut oldest = None::<f64>;
            for (file, prior) in previous.iter_mut().enumerate() {
                let bytes = read_bytes(&repo, &commit, &format!("src/file-{file}.bin"))?;
                if bytes.len() != 64 {
                    bytes_ok = false;
                    continue;
                }
                let generation = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                bytes_ok &=
                    bytes == small_payload(writer.writer, file, generation) && generation >= *prior;
                if generation > *prior {
                    if let Some((_, time)) = writer.edits.iter().find(|(g, _)| *g == *prior + 1) {
                        oldest = Some(oldest.map_or(*time, |v| v.min(*time)));
                    }
                }
                *prior = generation;
                generations.push(generation);
            }
            attempt["captured_generations"] = json!(generations);
            if let Some(oldest) = oldest {
                let elapsed = attempt["ack_ms"].as_f64().unwrap() - oldest;
                full_edit_samples.push(elapsed);
                if ordinal < 40 {
                    edit_samples.push(elapsed);
                }
                attempt["edit_to_durable_ms"] = json!(elapsed);
            }
            full_snapshot_samples.push(attempt["snapshot_ms"].as_f64().unwrap());
            if ordinal < 40 {
                snapshot_samples.push(attempt["snapshot_ms"].as_f64().unwrap());
            }
        }
    }
    let last = writer
        .attempts
        .iter()
        .rev()
        .find(|a| a["status"] == "published");
    let final_bytes = if let Some(last) = last {
        let commit = last["commit_id"].as_str().unwrap();
        if writer.large {
            read_bytes(&repo, commit, "large.bin")? == large_payload(writer.final_generation)
        } else {
            (0..8)
                .map(|file| {
                    Ok(read_bytes(&repo, commit, &format!("src/file-{file}.bin"))?
                        == small_payload(writer.writer, file, writer.final_generation))
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .all(|v| v)
        }
    } else {
        false
    };
    let samples = percentiles(&snapshot_samples);
    let edits = percentiles(&edit_samples);
    let full_samples = percentiles(&full_snapshot_samples);
    let full_edits = percentiles(&full_edit_samples);
    let remote = std::env::var_os("TANDEM_STAGE6_HOST").is_some();
    let injected = std::env::var("TANDEM_BENCH_INJECT_RTT_MS").as_deref() == Ok("50");
    let within_budget = |snapshots: &Value, edits: &Value| {
        if remote {
            snapshots["p95_ms"]
                .as_f64()
                .is_some_and(|v| v <= 5067.064042)
                && snapshots["p99_ms"].as_f64().is_some_and(|v| v <= 15000.0)
        } else if injected {
            snapshots["p95_ms"].as_f64().is_some_and(|v| v <= 2000.0)
                && snapshots["p99_ms"].as_f64().is_some_and(|v| v <= 5000.0)
                && edits["p95_ms"].as_f64().is_some_and(|v| v <= 3000.0)
                && edits["p99_ms"].as_f64().is_some_and(|v| v <= 6000.0)
        } else {
            true
        }
    };
    let latency_ok = writer.large
        || smoke
        || (within_budget(&samples, &edits) && within_budget(&full_samples, &full_edits));
    let enough =
        writer.large || smoke || (snapshot_samples.len() == 40 && edit_samples.len() == 40);
    let attempts_ok = writer
        .attempts
        .iter()
        .all(|a| a["status"] == "published" || a["status"] == "Unchanged");
    let passed = reachable && bytes_ok && final_bytes && latency_ok && enough && attempts_ok;
    Ok(
        json!({"writer":writer.writer,"large":writer.large,"attempts":writer.attempts,"edits":writer.edits,"final_generation":writer.final_generation,"exact_final_bytes":final_bytes,"captured_bytes_valid":if writer.large { Value::Null } else { json!(bytes_ok) },"content_verification_scope":if writer.large { "Exact final generation; intermediate acknowledged operations checked for reachability only" } else { "Exact bytes and per-file generations for every captured operation, plus exact final generation" },"all_acknowledged_operations_reachable":reachable,"snapshot_to_ack":samples,"edit_to_durable":edits,"full_snapshot_to_ack":full_samples,"full_edit_to_durable":full_edits,"latency_budget_passed":latency_ok,"enough_samples":enough,"passed":passed}),
    )
}
