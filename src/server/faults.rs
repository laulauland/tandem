//! The seam a test drives the server's failure paths through.
//!
//! These used to be environment variables, read fresh at each call site. That
//! only worked because every test ran the server as its own process: an
//! environment variable is process-wide, so two servers in one address space
//! could not be given different faults, and a generator could not change one
//! between two steps of a schedule.
//!
//! So the faults are a value now. A server holds one, the binary holds an inert
//! one, and the simulation harness holds the one it is currently driving. That
//! is what lets a seeded schedule pick a fault per step and replay it exactly.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A point in the publish path where a test may cut the server off.
///
/// The list is the whole path, not a sample of it: a publish writes the WAL
/// entry, compare-and-swaps the index, applies the operation locally, records
/// the metadata, and finally publishes whatever head set the reconcile derived.
/// A crash sits between each pair, so five windows name every gap there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CrashWindow {
    /// Nothing is durable yet.
    BeforeWalWrite,
    /// The WAL entry is in the bucket; the index still names the old heads.
    AfterWalWrite,
    /// The index committed; the local repo has not applied the operation.
    AfterIndexWrite,
    /// The operation is a local head; the metadata still holds the old version.
    AfterLocalApply,
    /// Everything but the derived head set is recorded.
    AfterMetadataWrite,
}

impl CrashWindow {
    /// Every window, in publish order.
    pub const ALL: [CrashWindow; 5] = [
        CrashWindow::BeforeWalWrite,
        CrashWindow::AfterWalWrite,
        CrashWindow::AfterIndexWrite,
        CrashWindow::AfterLocalApply,
        CrashWindow::AfterMetadataWrite,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            CrashWindow::BeforeWalWrite => "before-wal-write",
            CrashWindow::AfterWalWrite => "after-wal-write",
            CrashWindow::AfterIndexWrite => "after-index-write",
            CrashWindow::AfterLocalApply => "after-local-apply",
            CrashWindow::AfterMetadataWrite => "after-metadata-write",
        }
    }
}

/// The faults one server is currently under. Every field is inert by default,
/// so a server built without a test behind it behaves exactly as it always has.
#[derive(Default)]
pub struct FaultPoints {
    /// Index writes still to be answered with a synthetic CAS conflict.
    index_cas_conflicts: AtomicU64,
    /// Where the next publish should stop, if anywhere.
    crash_window: Mutex<Option<CrashWindow>>,
    /// Whether a crash has already fired. A crashed process answers nothing,
    /// so neither does a halted server.
    halted: AtomicBool,
    /// Whether writing a derived op head's WAL entry should fail.
    fail_derived_head_wal: AtomicBool,
    /// WAL entry writes still to be answered with a synthetic bucket failure.
    wal_write_failures: AtomicU64,
    /// Content for the object a second client "writes" during a retried publish.
    object_on_index_conflict: Mutex<Option<Vec<u8>>>,
}

impl FaultPoints {
    /// A fault set that does nothing — what the binary always runs with.
    pub fn inert() -> Arc<Self> {
        Arc::new(Self::default())
    }

    // ── Index CAS conflicts ──

    /// Force the next `count` index writes to look like CAS conflicts, so the
    /// retryable path can be exercised without a second real writer.
    pub fn set_index_cas_conflicts(&self, count: u64) {
        self.index_cas_conflicts.store(count, Ordering::Relaxed);
    }

    pub(super) fn take_index_cas_conflict(&self) -> bool {
        let remaining = self.index_cas_conflicts.load(Ordering::Relaxed);
        if remaining == 0 {
            return false;
        }
        self.index_cas_conflicts.fetch_sub(1, Ordering::Relaxed);
        tracing::warn!(remaining, "injecting an index CAS conflict");
        true
    }

    // ── An object written between two attempts of a retried publish ──

    /// Stand in for a second client whose object write lands between two
    /// attempts of a retried publish — the window in which a drained staging
    /// buffer can lose objects.
    pub fn stage_object_on_index_conflict(&self, content: Option<Vec<u8>>) {
        *self.object_on_index_conflict.lock().expect("fault lock") =
            content.filter(|bytes| !bytes.is_empty());
    }

    pub(super) fn object_for_index_conflict(&self) -> Option<Vec<u8>> {
        self.object_on_index_conflict
            .lock()
            .expect("fault lock")
            .clone()
    }

    // ── A bucket that refuses a WAL entry ──

    /// Make the next `count` WAL entry writes fail, as a bucket answering 500
    /// would. This is the window restaging exists for: by the time the write is
    /// attempted the publish has already drained the staging buffer, so every
    /// object it took has to go back on the queue or be durable nowhere.
    pub fn fail_wal_writes(&self, count: u64) {
        self.wal_write_failures.store(count, Ordering::Relaxed);
    }

    pub(super) fn take_wal_write_failure(&self) -> bool {
        let remaining = self.wal_write_failures.load(Ordering::Relaxed);
        if remaining == 0 {
            return false;
        }
        self.wal_write_failures.fetch_sub(1, Ordering::Relaxed);
        tracing::warn!(remaining, "injecting a WAL write failure");
        true
    }

    // ── A bucket that fails on a derived head's WAL entry ──

    /// Stand in for a bucket that fails while the server writes the WAL entry
    /// of a merge operation it minted itself. That write happens after the
    /// publish is durable and applied, so the failure must never reach the
    /// client.
    pub fn fail_derived_head_wal(&self, fail: bool) {
        self.fail_derived_head_wal.store(fail, Ordering::Relaxed);
    }

    pub(super) fn derived_head_wal_fault(&self) -> Option<anyhow::Error> {
        if !self.fail_derived_head_wal.load(Ordering::Relaxed) {
            return None;
        }
        Some(anyhow::anyhow!(
            "injected bucket failure while writing a derived op head"
        ))
    }

    // ── Crashes ──

    /// Stop the next publish at `window`. The server halts there: it answers
    /// nothing afterwards, which is what a dead process does.
    pub fn crash_at(&self, window: Option<CrashWindow>) {
        *self.crash_window.lock().expect("fault lock") = window;
        self.halted.store(false, Ordering::Relaxed);
    }

    /// Whether an armed crash has fired.
    pub fn halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    /// Fire, if this is the armed window. The error stands in for the
    /// connection failure a client sees when the server dies mid-request.
    pub(super) fn crash(&self, window: CrashWindow) -> anyhow::Result<()> {
        let armed = *self.crash_window.lock().expect("fault lock");
        if armed != Some(window) {
            return Ok(());
        }
        *self.crash_window.lock().expect("fault lock") = None;
        self.halted.store(true, Ordering::Relaxed);
        tracing::warn!(window = window.as_str(), "injected crash");
        anyhow::bail!("the server stopped at the {} window", window.as_str())
    }

    /// Refuse to serve once a crash has fired.
    pub(super) fn refuse_if_halted(&self) -> anyhow::Result<()> {
        if self.halted() {
            anyhow::bail!("the server has stopped");
        }
        Ok(())
    }
}
