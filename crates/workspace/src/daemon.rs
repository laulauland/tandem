//! The workspace daemon: file changes in, published operations out.
//!
//! What edits a working directory — an agent, a compiler, a person — is
//! outside tandem's model. So nothing tells tandem that a change happened;
//! tandem watches for it. The daemon is the whole of that: a filesystem
//! watcher, a debounce window, and a snapshot that publishes one jj operation
//! when the tree really did change.
//!
//! Three things it is deliberately not:
//!
//! - It is not a timer. A poll interval would be a second floor on how late a
//!   snapshot can be, on top of the debounce window that is meant to be the
//!   only one.
//! - It is not a command. There is no checkpoint verb, here or anywhere: a
//!   snapshot nobody asked for is the point, because an agent that has to
//!   remember to ask will not.
//! - It does not resolve anything. A head change somewhere else marks this
//!   workspace stale and stops there. `workspace update-stale` moves files
//!   under whoever is editing them, and that is a person's decision.
//!
//! The debounce window is a durability parameter and not a comfort setting:
//! work done inside one window is work that a machine dying takes with it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::object_id::ObjectId as _;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::{SnapshotOptions, WorkingCopyFreshness};
use jj_lib::workspace::{default_working_copy_factories, Workspace};
use pollster::FutureExt as _;
use serde::{Deserialize, Serialize};

use crate::watch;
use jj_tandem_client::{
    http_client::{TandemClient, WriterClaim},
    repo_link,
};

/// How long the daemon waits after the first file change of a burst before
/// snapshotting.
///
/// Counted from the first change and not from the last: a window that
/// restarted on every keystroke would never close while an agent is writing,
/// and the durability window is what this number is.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(1_000);

/// Where a caller who is not passing `--debounce-ms` says it instead.
pub const DEBOUNCE_ENV: &str = "TANDEM_DEBOUNCE_MS";

/// The description every daemon snapshot operation carries.
///
/// One fixed sentence, on purpose. What a change is *about* belongs in the
/// change description, which a person or an agent writes with `tandem
/// describe`; an operation that invented a description from the files that
/// moved would be guessing, and `op log` would fill with guesses.
pub const SNAPSHOT_OPERATION_DESCRIPTION: &str = "tandem daemon: snapshot working copy";

/// What the daemon leaves inside `.jj/` for anyone who asks how it is doing.
pub const STATUS_FILE: &str = "tandem-daemon.json";

/// How often the writer role is renewed, as a fraction of its lifetime. Three
/// renewals per lifetime means two may be lost before the role is.
const RENEWALS_PER_TTL: u32 = 3;

/// How long to wait before subscribing again after the event stream drops.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(1);

/// The longest that wait grows to while the server stays unreachable.
///
/// A server that is down for ten minutes must not cost ten minutes of
/// reconnection attempts and ten minutes of identical warning lines. The cap
/// keeps the daemon coming back within half a minute of the server returning,
/// which is well inside the staleness question it is subscribed for.
const RESUBSCRIBE_DELAY_MAX: Duration = Duration::from_secs(30);

/// How long a head event subscription that carried nothing has to last before
/// it counts as having worked.
///
/// Well above the delay a reconnect loop would run at, and well below how long
/// a real subscription to a quiet server stays open.
const HEALTHY_SUBSCRIPTION: Duration = Duration::from_secs(30);

// ─── Options ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// The workspace directory to watch.
    pub workspace_path: PathBuf,
    /// How long a burst of file changes is collected before snapshotting.
    pub debounce: Duration,
    /// How long each writer-role claim lasts.
    pub writer_ttl: Duration,
}

impl DaemonOptions {
    pub fn new(workspace_path: impl Into<PathBuf>) -> Self {
        Self {
            workspace_path: workspace_path.into(),
            debounce: DEFAULT_DEBOUNCE,
            writer_ttl: Duration::from_secs(jj_tandem_protocol::http::DEFAULT_WRITER_TTL_SECONDS),
        }
    }
}

/// The debounce window an explicit flag, the environment, or the default asks
/// for, in that order.
pub fn resolve_debounce(explicit_ms: Option<u64>) -> Duration {
    if let Some(ms) = explicit_ms {
        return Duration::from_millis(ms);
    }
    match std::env::var(DEBOUNCE_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(ms) => Duration::from_millis(ms),
            Err(_) => {
                eprintln!("warning: ignoring {DEBOUNCE_ENV}={raw:?}, which is not a number of milliseconds");
                DEFAULT_DEBOUNCE
            }
        },
        Err(_) => DEFAULT_DEBOUNCE,
    }
}

// ─── Status ───────────────────────────────────────────────────────────────────

/// What a running daemon knows about itself, as a file anybody can read.
///
/// A file rather than a socket because the question is per workspace and the
/// answer is small: `tandem daemon --status` reads it, and so can a test or a
/// person with `cat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    pub pid: u32,
    pub workspace: String,
    pub workspace_root: String,
    pub server: String,
    pub debounce_ms: u64,
    /// The heads moved somewhere this daemon did not publish. It is a fact,
    /// not an instruction: nothing here runs `workspace update-stale`.
    pub stale: bool,
    /// Whether this daemon currently holds the workspace's writer role.
    pub writer: bool,
    pub writer_holder: String,
    /// Why the writer role is not held, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_detail: Option<String>,
    pub published_ops: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_published_op: Option<String>,
    pub updated_at_unix_ms: u64,
}

/// Where a workspace's daemon writes its status.
pub fn status_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".jj").join(STATUS_FILE)
}

/// Read a workspace's daemon status, or say there is none.
pub fn read_status(workspace_root: &Path) -> Result<DaemonStatus> {
    let path = status_path(workspace_root);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no daemon status at {} — is `td daemon` running for this workspace?",
            path.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| format!("cannot read {}", path.display()))
}

/// Whether the daemon that wrote a status is still running.
///
/// The file is the last thing a daemon said, not proof that it is still
/// saying it: a killed daemon leaves its status behind exactly as it was, and
/// the one scenario this stage is built around is a killed daemon. Every
/// number in a status file — writer held, not stale, three operations
/// published — is a claim about a process, so a reader has to ask whether the
/// process is there.
///
/// A recycled pid can make this answer yes about the wrong process. That is
/// the standard limit of a pid file and not worth a lock file here: the cost
/// of being wrong is one misleading status line, and the daemon a person is
/// asking about is one they just started.
pub fn is_running(status: &DaemonStatus) -> bool {
    process_is_alive(status.pid)
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Signal 0 checks for the process without sending anything. `EPERM` means
    // it is there and belongs to somebody else, which is still there.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

// ─── What one snapshot did ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Published {
    pub operation_id: String,
    pub commit_id: String,
    /// From the start of the snapshot to the operation being acknowledged —
    /// the durability hop included. This is the gate metric.
    pub elapsed: Duration,
}

#[derive(Debug, Clone)]
pub enum SnapshotOutcome {
    /// The tree on disk is the tree in the working-copy commit. Nothing was
    /// published, and no operation was minted: an idle workspace is silent.
    Unchanged,
    /// One operation, carrying the new tree.
    Published(Published),
    /// The working copy is behind the repo and only a person may fix that.
    Stale,
    /// Somebody else holds the writer role, so nothing was published.
    NotTheWriter { detail: String },
}

// ─── The daemon ───────────────────────────────────────────────────────────────

pub struct Daemon {
    workspace: Workspace,
    repo: Arc<ReadonlyRepo>,
    workspace_name: WorkspaceNameBuf,
    client: Arc<TandemClient>,
    workspace_id: String,
    holder: String,
    writer_ttl: Duration,
    debounce: Duration,
    /// Every operation head this daemon knows the provenance of: the ones that
    /// were there when it started, and the ones it published itself. A head
    /// outside this set is somebody else's, which is what stale means here.
    known_heads: BTreeSet<Vec<u8>>,
    /// A window fired but nothing could be published — no writer role, or a
    /// failed publish. The next tick tries again, because no further file
    /// change is guaranteed to come.
    pending: bool,
    status: DaemonStatus,
    status_path: PathBuf,
}

// Carried from 45b65022: a shortest shared ancestor is not an ancestry proof
// when merges have shortcut parents. Only unchanged trees can use this
// metadata-only fallback.
fn unchanged_tree_ancestor(
    head: &jj_lib::operation::Operation,
    checkout: &jj_lib::op_store::OperationId,
    same_tree: bool,
) -> Result<bool> {
    if !same_tree {
        return Ok(false);
    }
    let mut frontier = std::collections::VecDeque::from([head.clone()]);
    let mut seen = BTreeSet::new();
    while let Some(operation) = frontier.pop_front() {
        if operation.id() == checkout {
            return Ok(true);
        }
        if !seen.insert(operation.id().clone()) {
            continue;
        }
        for parent in operation.parents() {
            frontier.push_back(parent?);
        }
    }
    Ok(false)
}

impl Daemon {
    /// Open the workspace at `options.workspace_path` and take the writer role
    /// for it.
    ///
    /// A refused claim is not a failure to start: the daemon runs, does not
    /// publish, and keeps asking. The previous holder's claim runs out on its
    /// own, and a daemon that exited instead would need somebody to restart it
    /// at exactly the right moment.
    pub fn open(settings: &UserSettings, options: &DaemonOptions) -> Result<Self> {
        let workspace_path = options
            .workspace_path
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", options.workspace_path.display()))?;

        let workspace = Workspace::load(
            settings,
            &workspace_path,
            &jj_tandem_client::tandem_factories_with_defaults(),
            &default_working_copy_factories(),
        )
        .with_context(|| format!("cannot load the workspace at {}", workspace_path.display()))?;

        let workspace_name = workspace.workspace_name().to_owned();
        let repo = workspace
            .repo_loader()
            .load_at_head()
            .context("cannot load the repository at its head")?;

        // The three facts a store on disk already knows. Read from the
        // op-heads store, which is the one that writes down which workspace
        // this checkout speaks for.
        let op_heads_path = workspace.repo_path().join("op_heads");
        let server_addr = repo_link::read_server_address(&op_heads_path)
            .map_err(|e| anyhow!("cannot read this workspace's server address: {e}"))?;
        let token = repo_link::read_token(&op_heads_path)
            .map_err(|e| anyhow!("cannot read this workspace's token: {e}"))?;
        let workspace_id = repo_link::read_workspace_id(&op_heads_path)
            .map_err(|e| anyhow!("cannot read this workspace's name: {e}"))?;

        let client = TandemClient::connect(&server_addr, &token)
            .with_context(|| format!("cannot reach the tandem server at {server_addr}"))?;

        // Every head that exists before this daemon does is one it is already
        // up to date with: it just loaded the repo at it.
        let known_heads = client
            .get_heads_state()
            .context("cannot read the server's operation heads")?
            .heads
            .into_iter()
            .collect();

        let holder = holder_name(&workspace_path);
        let status = DaemonStatus {
            pid: std::process::id(),
            workspace: workspace_id.clone(),
            workspace_root: workspace_path.display().to_string(),
            server: server_addr.clone(),
            debounce_ms: options.debounce.as_millis() as u64,
            stale: false,
            writer: false,
            writer_holder: holder.clone(),
            writer_detail: None,
            published_ops: 0,
            last_published_op: None,
            updated_at_unix_ms: now_unix_ms(),
        };

        let mut daemon = Self {
            workspace,
            repo,
            workspace_name,
            client,
            workspace_id,
            holder,
            writer_ttl: options.writer_ttl,
            debounce: options.debounce,
            known_heads,
            pending: false,
            status,
            status_path: status_path(&workspace_path),
        };

        daemon.claim_writer_role();
        daemon.write_status();
        Ok(daemon)
    }

    pub fn status(&self) -> &DaemonStatus {
        &self.status
    }

    pub fn workspace_root(&self) -> &Path {
        self.workspace.workspace_root()
    }

    pub fn server_addr(&self) -> &str {
        &self.status.server
    }

    pub fn debounce(&self) -> Duration {
        self.debounce
    }

    // ─── The writer role ──────────────────────────────────────────────

    /// Take the role, or keep it. Answers whether it is held afterwards.
    fn claim_writer_role(&mut self) -> bool {
        match self
            .client
            .claim_writer_role(&self.workspace_id, &self.holder, Some(self.writer_ttl))
        {
            Ok(WriterClaim::Held { .. }) => {
                if !self.status.writer {
                    println!(
                        "writer=held workspace={} holder={} ttl_seconds={}",
                        self.workspace_id,
                        self.holder,
                        self.writer_ttl.as_secs()
                    );
                }
                self.status.writer = true;
                self.status.writer_detail = None;
                true
            }
            Ok(WriterClaim::Refused { detail }) => {
                if self.status.writer || self.status.writer_detail.as_deref() != Some(&detail) {
                    println!("writer=refused detail={detail:?}");
                }
                self.status.writer = false;
                self.status.writer_detail = Some(detail);
                false
            }
            Err(err) => {
                let detail = format!("{err:#}");
                if self.status.writer || self.status.writer_detail.as_deref() != Some(&detail) {
                    eprintln!("warning: cannot renew the writer role: {detail}");
                }
                self.status.writer = false;
                self.status.writer_detail = Some(detail);
                false
            }
        }
    }

    // ─── Staleness ────────────────────────────────────────────────────

    /// Look at the server's heads and decide whether this workspace is behind.
    ///
    /// Nothing follows from a stale workspace except the flag and the line on
    /// stdout. Updating a working copy under an agent that is mid-edit is a
    /// policy decision, and this is not where policy lives.
    pub fn refresh_staleness(&mut self) -> Result<bool> {
        let state = self
            .client
            .get_heads_state()
            .context("cannot read the server's operation heads")?;
        let stale = state
            .heads
            .iter()
            .any(|head| !self.known_heads.contains(head));

        if stale != self.status.stale {
            self.status.stale = stale;
            if stale {
                println!(
                    "stale=true workspace={} hint=\"run `td workspace update-stale` when you \
                     want the files moved\"",
                    self.workspace_id
                );
            } else {
                println!("stale=false workspace={}", self.workspace_id);
            }
            self.write_status();
        }
        Ok(stale)
    }

    // ─── Snapshot and publish ─────────────────────────────────────────

    /// Snapshot the working copy and publish an operation if the tree moved.
    ///
    /// Public because the latency bench drives exactly this, and a benchmark
    /// of a private reimplementation would be measuring the benchmark.
    pub fn snapshot_once(&mut self) -> Result<SnapshotOutcome> {
        if !self.status.writer && !self.claim_writer_role() {
            let detail = self
                .status
                .writer_detail
                .clone()
                .unwrap_or_else(|| "the writer role is held elsewhere".to_string());
            return Ok(SnapshotOutcome::NotTheWriter { detail });
        }

        let started = Instant::now();

        // Everything published since the last snapshot, merged into the view
        // this one builds on. This is where a concurrent publish from another
        // workspace is absorbed — jj merges divergent op heads on load.
        let mut repo = self
            .repo
            .reload_at_head()
            .context("cannot reload the repository at its head")?;

        let mut locked_ws = self
            .workspace
            .start_working_copy_mutation()
            .context("cannot lock the working copy")?;

        let Some(wc_commit_id) = repo.view().get_wc_commit_id(&self.workspace_name).cloned() else {
            // The workspace was forgotten server-side. Nothing to snapshot
            // into, and inventing a commit would resurrect it behind whoever
            // did the forgetting.
            drop(locked_ws);
            self.repo = repo;
            return Ok(SnapshotOutcome::Unchanged);
        };
        let mut wc_commit = repo
            .store()
            .get_commit(&wc_commit_id)
            .context("cannot load the working-copy commit")?;

        let mut freshness =
            WorkingCopyFreshness::check_stale(locked_ws.locked_wc(), &wc_commit, &repo)
                .context("cannot tell whether the working copy is up to date")?;
        if matches!(freshness, WorkingCopyFreshness::SiblingOperation) {
            let locked_wc = locked_ws.locked_wc();
            if unchanged_tree_ancestor(
                repo.operation(),
                locked_wc.old_operation_id(),
                locked_wc.old_tree().tree_ids_and_labels()
                    == wc_commit.tree().tree_ids_and_labels(),
            )
            .context("cannot verify the working copy's operation ancestry")?
            {
                freshness = WorkingCopyFreshness::Fresh;
            }
        }
        match freshness {
            WorkingCopyFreshness::Fresh => {}
            WorkingCopyFreshness::Updated(wc_operation) => {
                // The working copy is ahead of the repo this daemon holds:
                // load the repo where the working copy already is.
                repo = repo
                    .reload_at(&wc_operation)
                    .context("cannot reload the repository at the working copy's operation")?;
                let Some(id) = repo.view().get_wc_commit_id(&self.workspace_name).cloned() else {
                    drop(locked_ws);
                    self.repo = repo;
                    return Ok(SnapshotOutcome::Unchanged);
                };
                wc_commit = repo
                    .store()
                    .get_commit(&id)
                    .context("cannot load the working-copy commit")?;
            }
            WorkingCopyFreshness::WorkingCopyStale | WorkingCopyFreshness::SiblingOperation => {
                // Somebody moved this workspace's commit. Recovering it means
                // writing files under whoever is editing them, so it is a
                // person's call and not this loop's.
                drop(locked_ws);
                self.repo = repo;
                if !self.status.stale {
                    self.status.stale = true;
                    println!(
                        "stale=true workspace={} hint=\"the working copy was moved elsewhere; run \
                         `td workspace update-stale`\"",
                        self.workspace_id
                    );
                    self.write_status();
                }
                return Ok(SnapshotOutcome::Stale);
            }
        }

        let options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            // Everything new is tracked, which is what jj does by default. A
            // daemon has nobody to ask about a file it has not seen before.
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
            // No ceiling: a refused large file would leave the workspace
            // permanently unable to publish, and nobody would be watching the
            // error. `.gitignore` inside the tree is still honoured.
            max_new_file_size: u64::MAX,
        };

        let (new_tree, _stats) = locked_ws
            .locked_wc()
            .snapshot(&options)
            .block_on()
            .context("cannot snapshot the working copy")?;

        // The whole of "an idle workspace publishes nothing": no transaction,
        // no operation, no request.
        if new_tree.tree_ids_and_labels() == wc_commit.tree().tree_ids_and_labels() {
            locked_ws
                .finish(repo.op_id().clone())
                .context("cannot release the working copy")?;
            self.repo = repo;
            return Ok(SnapshotOutcome::Unchanged);
        }

        let mut tx = repo.start_transaction();
        tx.set_is_snapshot(true);
        let commit = tx
            .repo_mut()
            .rewrite_commit(&wc_commit)
            .set_tree(new_tree)
            .write()
            .context("cannot write the snapshotted commit")?;
        tx.repo_mut()
            .set_wc_commit(self.workspace_name.clone(), commit.id().clone())
            .context("cannot move the workspace to the snapshotted commit")?;
        tx.repo_mut()
            .rebase_descendants()
            .context("cannot rebase rewritten descendants")?;

        // The op-heads store does the CAS against the server here, retrying a
        // lost race itself. What comes back has been acknowledged, which for
        // this server means it is in the bucket.
        let updated_repo = tx
            .commit(SNAPSHOT_OPERATION_DESCRIPTION)
            .context("cannot publish the snapshot operation")?;

        locked_ws
            .finish(updated_repo.op_id().clone())
            .context("cannot release the working copy")?;

        let elapsed = started.elapsed();
        let operation_id = updated_repo.op_id().hex();
        let commit_id = commit.id().hex();

        self.known_heads
            .insert(updated_repo.op_id().as_bytes().to_vec());
        self.repo = updated_repo;
        self.status.published_ops += 1;
        self.status.last_published_op = Some(operation_id.clone());

        Ok(SnapshotOutcome::Published(Published {
            operation_id,
            commit_id,
            elapsed,
        }))
    }

    // ─── The loop ─────────────────────────────────────────────────────

    /// Watch, debounce, snapshot, publish — until the process is stopped.
    pub fn run(mut self) -> Result<()> {
        let (tx, rx) = mpsc::channel();

        let root = self.workspace_root().to_path_buf();
        let _watcher = watch_files(&root, tx.clone())?;
        spawn_event_subscription(
            self.server_addr().to_string(),
            self.client.token().to_string(),
            tx.clone(),
        );
        spawn_ticker(self.writer_ttl / RENEWALS_PER_TTL, tx);

        eprintln!(
            "watching {} (workspace {}, server {}, debounce {}ms)",
            root.display(),
            self.workspace_id,
            self.status.server,
            self.debounce.as_millis()
        );

        // Catch up on whatever moved while nothing was watching. A daemon that
        // waited for the next file change would hold a machine's last edits
        // hostage to somebody touching the tree again, and after a crash there
        // may be nobody left to. On a workspace that really is idle this
        // publishes nothing: the tree matches the commit and the snapshot is a
        // no-op, which is the same guard every other pass goes through.
        self.publish_now(Instant::now());

        loop {
            let Ok(wake) = rx.recv() else {
                // Every sender is gone: the watcher failed and the threads
                // with it. Nothing more will ever arrive. The status file goes
                // with the daemon — a status left behind by a daemon that is
                // not there reads as a running one.
                let _ = std::fs::remove_file(&self.status_path);
                return Ok(());
            };

            match wake {
                Wake::Files { first_seen } => {
                    // One window per burst, counted from the first change in
                    // it. Wake-ups of other kinds are answered while it runs
                    // but do not extend it.
                    let deadline = first_seen + self.debounce;
                    loop {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        match rx.recv_timeout(left) {
                            Ok(Wake::Files { .. }) => {}
                            Ok(Wake::Other(interrupt)) => self.handle(interrupt),
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    self.publish_now(first_seen);
                }
                Wake::Other(interrupt) => self.handle(interrupt),
            }
        }
    }

    fn handle(&mut self, interrupt: Interrupt) {
        match interrupt {
            // No window, and no scan now. The next renewal tick takes one
            // snapshot for the whole build instead of one per burst, and on a
            // tree where nothing tracked moved it publishes nothing.
            Interrupt::IgnoredFiles => self.pending = true,
            Interrupt::Heads => {
                if let Err(err) = self.refresh_staleness() {
                    eprintln!("warning: cannot read the server's heads: {err:#}");
                }
            }
            Interrupt::Tick => {
                self.claim_writer_role();
                if self.pending {
                    self.publish_now(Instant::now());
                }
                self.write_status();
            }
        }
    }

    fn publish_now(&mut self, changed_at: Instant) {
        match self.snapshot_once() {
            Ok(SnapshotOutcome::Published(published)) => {
                self.pending = false;
                println!(
                    "published op={} commit={} snapshot_publish_ms={} since_change_ms={}",
                    published.operation_id,
                    published.commit_id,
                    published.elapsed.as_millis(),
                    changed_at.elapsed().as_millis()
                );
                // The publish moved the heads to one this daemon knows, so a
                // workspace that was behind no longer is.
                if let Err(err) = self.refresh_staleness() {
                    eprintln!("warning: cannot read the server's heads: {err:#}");
                }
                self.write_status();
            }
            Ok(SnapshotOutcome::Unchanged) => {
                self.pending = false;
            }
            Ok(SnapshotOutcome::Stale) => {
                self.pending = true;
            }
            Ok(SnapshotOutcome::NotTheWriter { detail }) => {
                self.pending = true;
                eprintln!("warning: not publishing: {detail}");
                self.write_status();
            }
            Err(err) => {
                // A publish that failed is retried on the next tick rather
                // than dropped: the files are still there, and the next
                // attempt sees the same difference.
                self.pending = true;
                eprintln!("warning: snapshot failed, will retry: {err:#}");
            }
        }
    }

    fn write_status(&mut self) {
        self.status.updated_at_unix_ms = now_unix_ms();
        let Ok(json) = serde_json::to_string_pretty(&self.status) else {
            return;
        };
        // Written beside itself and renamed, so a reader never sees half a
        // status.
        let temp = self.status_path.with_extension("json.tmp");
        if std::fs::write(&temp, json).is_ok() {
            let _ = std::fs::rename(&temp, &self.status_path);
        }
    }
}

// ─── Wake-ups ─────────────────────────────────────────────────────────────────

/// Everything that can interrupt the daemon's wait, on one channel: the loop
/// is single-threaded so that the flags it keeps need no lock and no ordering
/// argument.
///
/// A file change is its own variant rather than one more [`Interrupt`] because
/// it is the only wake-up the loop answers with a debounce window, and the only
/// one the window then swallows. Splitting them says so in the type: `handle`
/// takes an `Interrupt`, so there is no arm in it for a file change to fall
/// into and no way to reach one.
enum Wake {
    /// Something under the workspace root changed, first noticed then.
    Files { first_seen: Instant },
    /// Anything else. Answered where it arrives, window or no window.
    Other(Interrupt),
}

/// A wake-up the daemon answers at once.
enum Interrupt {
    /// Something changed that the workspace's `.gitignore` disowns. It opens
    /// no window, but it is remembered: jj lets a file be both tracked and
    /// gitignored, and that file's change would otherwise be lost rather than
    /// merely late.
    IgnoredFiles,
    /// The server's heads moved.
    Heads,
    /// Time to renew the writer role and retry anything left pending.
    Tick,
}

/// What one changed path is to this daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// A file the workspace publishes. It opens a debounce window.
    Watched,
    /// A file the workspace's own `.gitignore` says is not interesting —
    /// build output, a dependency directory. It does not open a window.
    Ignored,
    /// Repository machinery, or something outside the root entirely.
    NotOurs,
}

/// Watch the workspace root, ignoring what tandem and git write themselves and
/// what the workspace's own `.gitignore` disowns.
///
/// The `.jj` and `.git` filter is not tidiness: a snapshot writes to
/// `.jj/working_copy`, so a watcher that reported those writes would snapshot
/// again because it snapshotted, forever.
///
/// The `.gitignore` filter is about cost. A build writes thousands of files
/// into `target/` in a few seconds; each one would open a debounce window, and
/// each window would run a full working-copy scan that ends up finding nothing
/// — because the snapshot honours `.gitignore` too. That is the most expensive
/// thing the daemon can do, done at the moment the machine is busiest, over
/// and over for as long as the build runs.
///
/// Ignored writes are not dropped, though. They mark the daemon as having
/// something to look at, and the next writer-role renewal snapshots — so a
/// file that is both tracked and gitignored, which jj allows, is published
/// late rather than never. Only the workspace root's own `.gitignore` is read,
/// and it is read once at startup: a per-directory chain would have to be
/// rebuilt on every event, and it is the root file that names `target/`.
fn watch_files(root: &Path, tx: Sender<Wake>) -> Result<notify::RecommendedWatcher> {
    use notify::{EventKind, RecursiveMode, Watcher as _};

    let root_owned = root.to_path_buf();
    let ignores = match GitIgnoreFile::empty().chain_with_file("", root.join(".gitignore")) {
        Ok(ignores) => ignores,
        Err(err) => {
            eprintln!("warning: ignoring this workspace's .gitignore, which cannot be read: {err}");
            GitIgnoreFile::empty()
        }
    };

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                eprintln!("warning: the filesystem watcher reported an error: {err}");
                return;
            }
        };

        // Reading a file is not a change. inotify reports opens and reads too,
        // and a build that reads the tree would otherwise look like an edit.
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }

        let mut ignored = false;
        for path in &event.paths {
            match classify_change(&root_owned, &ignores, path) {
                Change::Watched => {
                    let _ = tx.send(Wake::Files {
                        first_seen: Instant::now(),
                    });
                    return;
                }
                Change::Ignored => ignored = true,
                Change::NotOurs => {}
            }
        }

        if ignored {
            let _ = tx.send(Wake::Other(Interrupt::IgnoredFiles));
        }
    })
    .context("cannot start the filesystem watcher")?;

    watcher
        .watch(root, RecursiveMode::Recursive)
        .with_context(|| format!("cannot watch {}", root.display()))?;
    Ok(watcher)
}

/// What a path that changed is: a watched file, an ignored one, or none of
/// this workspace's business.
fn classify_change(root: &Path, ignores: &GitIgnoreFile, path: &Path) -> Change {
    if !is_workspace_content(root, path) {
        return Change::NotOurs;
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return Change::NotOurs;
    };
    let mut name = relative.to_string_lossy().replace('\\', "/");
    if name.is_empty() {
        return Change::NotOurs;
    }
    // `matches` reads a trailing slash as "this is a directory", which is what
    // makes a rule like `target/` match the directory itself. It also matches
    // on any parent, so a file deep inside an ignored directory is ignored
    // whether or not the directory is still there to be looked at.
    if path.is_dir() {
        name.push('/');
    }
    if ignores.matches(&name) {
        Change::Ignored
    } else {
        Change::Watched
    }
}

/// Whether a path that changed is one of the workspace's own files, rather
/// than repository machinery.
fn is_workspace_content(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        // Outside the root: not ours to care about.
        return false;
    };
    !matches!(
        relative.components().next(),
        Some(std::path::Component::Normal(first))
            if first == std::ffi::OsStr::new(".jj") || first == std::ffi::OsStr::new(".git")
    )
}

/// Follow the server's head events, reconnecting for as long as the daemon
/// lives.
///
/// The events are wake-ups and nothing else — the version they carry is not
/// even read here. What a head change means for this workspace is decided by
/// asking the server, on the loop's own thread.
///
/// A server that is down is a state, not an event: the delay between attempts
/// grows to [`RESUBSCRIBE_DELAY_MAX`], and the reason is printed when it
/// changes rather than once per attempt. A daemon runs for days beside an
/// agent, and a line a second is a log nobody can read the rest of.
///
/// "Down" is judged on what the stream did, not on whether the connection was
/// accepted. A server that accepts the request and drops the stream at once —
/// a proxy answering while the server behind it restarts, a load balancer with
/// no upstream, a server shutting down — would otherwise look like success on
/// every attempt: the backoff would reset each time and no line would ever be
/// printed, leaving a daemon reconnecting once a second in silence for as long
/// as it lasts. See [`classify_stream`].
fn spawn_event_subscription(server_addr: String, token: String, tx: Sender<Wake>) {
    std::thread::spawn(move || {
        let mut backoff = ResubscribeBackoff::new();
        loop {
            let attempt = match watch::subscribe(&server_addr, &token) {
                Ok(events) => {
                    let opened = Instant::now();
                    let mut delivered = 0usize;
                    let mut ended_with = None;
                    for event in events {
                        if let Err(err) = event {
                            ended_with = Some(format!("{err:#}"));
                            break;
                        }
                        delivered += 1;
                        if tx.send(Wake::Other(Interrupt::Heads)).is_err() {
                            return;
                        }
                    }
                    classify_stream(delivered, opened.elapsed(), ended_with)
                }
                Err(err) => Attempt::Failed(format!("{err:#}")),
            };

            let (wait, line) = backoff.next(&attempt);
            if let Some(line) = line {
                eprintln!("{line}");
            }
            std::thread::sleep(wait);
        }
    });
}

/// How long to wait before the next attempt, and what to say about it.
///
/// The policy lives apart from the loop that obeys it for the same reason
/// [`classify_stream`] does: the loop is a thread with a socket and two
/// `sleep`s in it, and neither the growth of the wait nor the rule that a
/// repeated reason is silent can be asked about there. Here they are a
/// function of the attempts that came in, so the tests can ask the daemon's
/// own policy rather than a copy of it.
struct ResubscribeBackoff {
    /// How long the next failed attempt waits.
    delay: Duration,
    /// The failure currently being reported, so a repeat of it is silent.
    reported: Option<String>,
}

impl ResubscribeBackoff {
    fn new() -> Self {
        Self {
            delay: RESUBSCRIBE_DELAY,
            reported: None,
        }
    }

    /// Fold one attempt in: answer how long to wait before trying again, and
    /// the line to print, when this attempt is one worth a line.
    fn next(&mut self, attempt: &Attempt) -> (Duration, Option<String>) {
        match attempt {
            Attempt::Healthy => {
                let line = self
                    .reported
                    .take()
                    .map(|_| "head events: subscribed again".to_string());
                self.delay = RESUBSCRIBE_DELAY;
                (RESUBSCRIBE_DELAY, line)
            }
            Attempt::Failed(detail) => {
                let wait = self.delay;
                let line = if self.reported.as_deref() == Some(detail.as_str()) {
                    None
                } else {
                    self.reported = Some(detail.clone());
                    Some(format!(
                        "warning: cannot follow head events, retrying every {}s or so: {detail}",
                        wait.as_secs().max(1)
                    ))
                };
                self.delay = (self.delay * 2).min(RESUBSCRIBE_DELAY_MAX);
                (wait, line)
            }
        }
    }
}

/// What one attempt at following the head events came to.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    /// The subscription worked. Whatever ended it, it is not a reason to wait
    /// longer before trying again.
    Healthy,
    /// It did not, for this reason.
    Failed(String),
}

/// Whether a stream that has ended was a working subscription.
///
/// A stream proves itself either by delivering an event or by staying open:
/// a quiet server sends nothing for hours, so silence alone says nothing, and
/// a subscription that lasted [`HEALTHY_SUBSCRIPTION`] was plainly connected to
/// something. What is left — ended at once, carrying nothing — is a failed
/// connection wearing a successful handshake, and it is treated as the failure
/// it is so that the backoff grows and the reason is printed.
fn classify_stream(delivered: usize, lasted: Duration, ended_with: Option<String>) -> Attempt {
    if delivered > 0 || lasted >= HEALTHY_SUBSCRIPTION {
        return Attempt::Healthy;
    }
    Attempt::Failed(match ended_with {
        Some(detail) => format!("the head event stream ended as soon as it opened: {detail}"),
        None => "the head event stream ended as soon as it opened".to_string(),
    })
}

fn spawn_ticker(every: Duration, tx: Sender<Wake>) {
    let every = every.max(Duration::from_secs(1));
    std::thread::spawn(move || loop {
        std::thread::sleep(every);
        if tx.send(Wake::Other(Interrupt::Tick)).is_err() {
            return;
        }
    });
}

// ─── Odds and ends ────────────────────────────────────────────────────────────

/// Who this daemon says it is when it claims the writer role.
///
/// The host and the directory, because that is what distinguishes two daemons
/// that would both like to write the same workspace, and the pid so that a
/// daemon that died is not confused with the one that replaced it.
fn holder_name(workspace_path: &Path) -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|host| !host.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    format!("{host}:{}:{}", workspace_path.display(), std::process::id())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// `tandem daemon` itself: open the workspace and run until stopped.
pub fn run_daemon(settings: &UserSettings, options: &DaemonOptions) -> Result<()> {
    Daemon::open(settings, options)?.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Turn the daemon's own reconnect policy over a made-up sequence of
    /// attempts, answering how long it waited before each one and what it
    /// printed.
    ///
    /// The policy is [`ResubscribeBackoff`], the very object the loop uses;
    /// this only turns the crank. The delay it derives and the reason it
    /// reports are the whole observable behaviour of a daemon whose server is
    /// not answering, and the loop around it is a thread with a socket and two
    /// `sleep`s in it that a test cannot ask.
    fn waits_for(attempts: &[Attempt]) -> (Vec<Duration>, Vec<String>) {
        let mut backoff = ResubscribeBackoff::new();
        let mut waits = Vec::new();
        let mut printed = Vec::new();
        for attempt in attempts {
            let (wait, line) = backoff.next(attempt);
            waits.push(wait);
            printed.extend(line);
        }
        (waits, printed)
    }

    #[test]
    fn shortcut_merge_ancestry_requires_tree_equality_and_actual_reachability() {
        use jj_lib::backend::CommitId;
        use jj_lib::op_store::{OpStore, OperationId, RootOperationData};
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn OpStore> = Arc::new(
            jj_lib::simple_op_store::SimpleOpStore::init(
                dir.path(),
                RootOperationData {
                    root_commit_id: CommitId::new(vec![0; 20]),
                },
            )
            .unwrap(),
        );
        let root = store.root_operation_id().clone();
        let write = |name: &str, parents: Vec<OperationId>| {
            let mut operation = store.read_operation(&root).block_on().unwrap();
            operation.metadata.description = name.to_string();
            operation.parents = parents;
            let id = store.write_operation(&operation).block_on().unwrap();
            jj_lib::operation::Operation::new(store.clone(), id, operation)
        };
        let checkout = write("checkout", vec![root.clone()]);
        let first = write("first", vec![checkout.id().clone()]);
        let second = write("second", vec![first.id().clone()]);
        let head = write("shortcut merge", vec![second.id().clone(), root.clone()]);
        let sibling = write("genuine sibling", vec![root.clone()]);
        assert!(unchanged_tree_ancestor(&head, checkout.id(), true).unwrap());
        assert!(!unchanged_tree_ancestor(&head, checkout.id(), false).unwrap());
        assert!(!unchanged_tree_ancestor(&head, sibling.id(), true).unwrap());
    }

    #[test]
    fn a_head_event_stream_that_ends_as_soon_as_it_opens_is_a_failed_connection() {
        // A proxy that accepts the request while the server behind it is
        // restarting: the handshake succeeds, the stream carries nothing, and
        // it is over before the next line of code runs.
        let attempt = classify_stream(0, Duration::from_millis(3), None);
        assert!(
            matches!(attempt, Attempt::Failed(_)),
            "a stream that opened and ended carrying nothing is not a working \
             subscription: {attempt:?}"
        );

        // Treating it as one is what the daemon must not do: taken as success,
        // every attempt would reset the delay, so the daemon would reconnect at
        // the shortest interval it has, for as long as the server stays that
        // way, and print nothing about why. Taken as the failure it is, the
        // wait grows and the reason is said once.
        let attempts: Vec<Attempt> = (0..6)
            .map(|_| classify_stream(0, Duration::from_millis(3), None))
            .collect();
        let (waits, printed) = waits_for(&attempts);
        assert_eq!(
            waits,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
            ],
            "the wait grows and stops at the cap"
        );
        assert_eq!(
            printed.len(),
            1,
            "the reason is printed once, not once an attempt: {printed:?}"
        );
        assert!(
            printed[0].contains("ended as soon as it opened"),
            "the reason says what happened: {}",
            printed[0]
        );
    }

    #[test]
    fn a_stream_that_worked_resets_the_wait_however_it_ended() {
        // One that carried an event, however briefly.
        assert_eq!(
            classify_stream(1, Duration::from_millis(3), None),
            Attempt::Healthy
        );
        // And one that carried nothing but stayed open, which is what a
        // subscription to a server nobody is publishing to looks like.
        assert_eq!(
            classify_stream(0, HEALTHY_SUBSCRIPTION, None),
            Attempt::Healthy
        );
        // An error that ends a stream which had been working is still the end
        // of a working stream: the next attempt goes in at once.
        assert_eq!(
            classify_stream(4, Duration::from_secs(1), Some("connection reset".into())),
            Attempt::Healthy
        );

        let (waits, printed) = waits_for(&[
            classify_stream(0, Duration::from_millis(1), None),
            classify_stream(0, Duration::from_millis(1), None),
            classify_stream(3, Duration::from_secs(2), None),
            classify_stream(0, Duration::from_millis(1), None),
        ]);
        assert_eq!(
            waits,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                RESUBSCRIBE_DELAY,
                Duration::from_secs(1),
            ],
            "a subscription that worked puts the wait back to the shortest one"
        );
        assert_eq!(
            printed.len(),
            3,
            "the outage, the recovery, and the next outage: {printed:?}"
        );
        assert_eq!(printed[1], "head events: subscribed again");
    }

    #[test]
    fn the_repositorys_own_writes_are_not_file_changes() {
        let root = Path::new("/w");
        assert!(is_workspace_content(root, Path::new("/w/src/main.rs")));
        assert!(is_workspace_content(root, Path::new("/w/README.md")));
        assert!(!is_workspace_content(
            root,
            Path::new("/w/.jj/working_copy/tree_state")
        ));
        assert!(!is_workspace_content(root, Path::new("/w/.git/index")));
        assert!(!is_workspace_content(root, Path::new("/elsewhere/file")));
        // A file whose name merely starts the same way is content.
        assert!(is_workspace_content(root, Path::new("/w/.jjignore")));
    }

    #[test]
    fn a_build_writing_into_an_ignored_directory_opens_no_window() {
        let root = Path::new("/w");
        let ignores = GitIgnoreFile::empty()
            .chain(
                "",
                Path::new("/w/.gitignore"),
                b"target/\nnode_modules/\n*.log\n",
            )
            .expect("a .gitignore that parses");

        // What the workspace publishes.
        assert_eq!(
            classify_change(root, &ignores, Path::new("/w/src/main.rs")),
            Change::Watched
        );
        // What a build writes. The rule names the directory; the event names a
        // file several levels inside it.
        assert_eq!(
            classify_change(
                root,
                &ignores,
                Path::new("/w/target/debug/deps/libfoo-abc123.rlib")
            ),
            Change::Ignored
        );
        assert_eq!(
            classify_change(
                root,
                &ignores,
                Path::new("/w/node_modules/left-pad/index.js")
            ),
            Change::Ignored
        );
        assert_eq!(
            classify_change(root, &ignores, Path::new("/w/build.log")),
            Change::Ignored
        );
        // And the repository's own writes stay what they were.
        assert_eq!(
            classify_change(root, &ignores, Path::new("/w/.jj/working_copy/tree_state")),
            Change::NotOurs
        );
        assert_eq!(
            classify_change(root, &ignores, Path::new("/elsewhere/file")),
            Change::NotOurs
        );
    }

    #[test]
    fn a_workspace_with_no_gitignore_watches_everything() {
        let root = Path::new("/w");
        let ignores = GitIgnoreFile::empty();
        assert_eq!(
            classify_change(root, &ignores, Path::new("/w/target/debug/x")),
            Change::Watched
        );
        assert_eq!(
            classify_change(root, &ignores, Path::new("/w/a.txt")),
            Change::Watched
        );
    }

    #[test]
    fn the_debounce_window_comes_from_the_flag_then_the_environment() {
        // The flag wins outright, whatever the environment says — which is
        // what makes it usable to override a value baked into an image.
        assert_eq!(resolve_debounce(Some(250)), Duration::from_millis(250));

        // The environment is read only when no flag was passed. Asserted
        // rather than set, because these tests share one process and a
        // `set_var` here would be a race with every other test in it.
        match std::env::var(DEBOUNCE_ENV) {
            Ok(raw) => assert_eq!(
                resolve_debounce(None),
                raw.trim()
                    .parse::<u64>()
                    .map(Duration::from_millis)
                    .unwrap_or(DEFAULT_DEBOUNCE)
            ),
            Err(_) => assert_eq!(resolve_debounce(None), DEFAULT_DEBOUNCE),
        }
    }

    #[test]
    fn a_snapshot_operation_says_nothing_about_what_changed() {
        // The description is a constant, not a template: nothing in it can
        // vary with the files that moved.
        assert!(!SNAPSHOT_OPERATION_DESCRIPTION.contains('{'));
        assert_eq!(
            SNAPSHOT_OPERATION_DESCRIPTION,
            "tandem daemon: snapshot working copy"
        );
    }
}
