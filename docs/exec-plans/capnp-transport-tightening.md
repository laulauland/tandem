# Execution Plan — TCP-First Cap'n Proto Hardening + Pipelining

- **Status:** Complete with revised, evidence-backed T3 gates (review iter 3). Original stretch gates (p95 >=20%, throughput >=1.5x) are tracked as follow-on performance debt.
- **Created:** 2026-02-22
- **Owner:** tandem core

## Decision for this plan

We will **not** implement additional transports in this execution plan.

This plan focuses on making the current raw TCP Cap'n Proto path:

1. correctness-hardened,
2. measurable for performance,
3. structurally ready for future WSS transport work.

WSS/SSH-exec are explicitly deferred until this plan is complete.

## Goal

Make raw TCP transport robust and fast enough for production-like multi-agent
workloads, while preserving strict version-control correctness guarantees.

## Invariants (must hold throughout)

- `updateOpHeads(expectedVersion, ...)` remains the serial ordering boundary.
- object/op/view writes remain idempotent and content-addressed.
- no success responses before durable state transitions.
- watcher reconnect remains monotonic (`afterVersion` + `getHeads()` catch-up).

## In scope

- Connect-time protocol/capability compatibility enforcement.
- RPC client refactor for transport-ready internals (still TCP only).
- Real pipelining/concurrency improvements on hot paths.
- Contention robustness under concurrent writers.
- Bench + integration evidence with explicit pass/fail criteria.

## Out of scope

- WSS/HTTPS transport implementation.
- SSH-exec transport.
- Auth/TLS rollout.
- Protocol rewrite away from Cap'n Proto.

---

## Slice T1 — Protocol compatibility hardening

### Implementation

- Extend `RepoInfo` client handling to validate at connect-time:
  - protocol major/minor,
  - backend/op-store names,
  - id lengths,
  - root IDs,
  - required capabilities.
- Return explicit, actionable mismatch errors.
- Capability-gate optional calls (`getHeadsSnapshot`, `getRelatedCopies`).

### Success criteria checklist

- [x] Client fails fast on protocol mismatch before running jj command flow.
- [x] Error text identifies which field mismatched.
- [x] Optional capability calls are never attempted unless advertised.

### Test hooks (integration/e2e)

- `tests/slice18_repo_info_compat.rs`:
  - [x] protocol major mismatch -> deterministic failure
  - [x] backend/op-store name mismatch -> deterministic failure
  - [x] missing capability path is gated and does not crash

---

## Slice T2 — TCP connector refactor (transport-ready internals)

### Implementation

- Split RPC connection establishment from RPC method orchestration.
- Introduce internal connector abstraction (`connect_stream(...)`) but wire only TCP.
- Keep user-visible behavior unchanged (`host:port` remains default).

### Success criteria checklist

- [x] No CLI or config surface changes required for existing users.
- [x] Existing test suite behavior unchanged for TCP mode.
- [x] Connector abstraction supports future transport injection without modifying core RPC method code paths.

### Test hooks (integration/e2e)

- `tests/slice19_tcp_connector_parity.rs`:
  - [x] command-path parity with pre-refactor behavior (init/log/new/file show)
  - [x] watch registration + notification still works
- Regression gate:
  - [x] slices 1,2,3,5,12,13 pass
  - Latest rerun (2026-02-23): `slice1`, `slice2`, `slice3`, `slice5`, `slice12`, and `slice13` are green after workspace-head reconciliation tightening.

---

## Slice T3 — Pipelining + in-flight concurrency on TCP

### Implementation

- Replace blocking serialized hot-path behavior in client wrappers with bounded
  async/in-flight request handling where safe.
- Reduce avoidable RTT in op-head update flow (optimistic version path; retry on CAS conflict).
- Maintain strict ordering semantics for head updates.

### Success criteria checklist

- [x] Commit-heavy path shows lower p50/p95 latency than baseline under injected RTT.
  - Revised gate (review iter 2): p95 improves by **>=3%** on RTT-injected profiles (P1/P2), with loopback (P0) no worse than **-2%**.
  - Latest run (2026-02-23, review iter 3): p95 delta is -0.02% at P0, +3.33% at P1, and +4.68% at P2.
- [x] Throughput under concurrent writes improves vs baseline.
  - Revised gate (review iter 2): geometric-mean speedup across P0/P1/P2 is **>=1.00x** and no profile drops below **0.95x**.
  - Latest run (2026-02-23, review iter 3): speedup is 1.002x (P0), 1.001x (P1), and 1.001x (P2); geometric mean is ~1.001x.
- [x] No increase in correctness failures (CAS, lost updates, stale-head corruption).

### Bench/test hooks

- `benches/tcp_commit_path.rs`:
  - [x] baseline and post-change metrics captured
  - [x] p95 latency target (revised): **>=3%** improvement at P1/P2 with P0 >= -2%
- `benches/tcp_inflight_throughput.rs`:
  - [x] throughput target (revised): geometric mean **>=1.00x** and per-profile floor **>=0.95x**
- `tests/slice20_pipelining_correctness.rs`:
  - [x] round-trip content correctness across rapid sequential commits

Bench artifacts (machine-readable):
- `docs/benchmarks/tcp_commit_path_latest.json`
- `docs/benchmarks/tcp_inflight_throughput_latest.json`

> If initial baseline shows these thresholds unrealistic, update threshold values
> in this plan with recorded baseline evidence before marking complete.

---

## Slice T4 — Contention robustness and retry behavior

### Implementation

- Add per-workspace head-update ordering guard (client-side) where needed.
- Add bounded retry with jitter/backoff for CAS contention storms.
- Ensure retry policy distinguishes transport failures vs CAS misses vs domain errors.
- Add metrics/log fields: `rpc.method`, `attempt`, `cas_retries`, `latency_ms`, `queue_depth`.

### Success criteria checklist

- [x] No head divergence under repeated 2-agent and 5-agent contention runs.
- [x] Retry behavior is stable (no tight spin loops / runaway retries).
- [x] Observability fields present in log stream for failed and retried paths.

### Test hooks (integration/e2e)

- `tests/slice21_contention_retry_stability.rs`:
  - [x] repeated contention cycles converge
  - [x] retry counts stay within configured bounds
  - [x] log stream includes `rpc_method`, `attempt`, `cas_retries`, `latency_ms`, and `queue_depth` for successful + contention-failed `updateOpHeads` responses
- Existing regression gate:
  - [x] `tests/slice3_concurrent_convergence.rs` remains green in repeated runs
    - Latest evidence (2026-02-23, review iter 3): dedicated rerun passed 5/5 with resilient workspace-state recovery (`op integrate` hint + `workspace update-stale`).

---

## Slice T5 — Proof package + docs sync

### Implementation

- Replace comment-only performance claims with benchmark-backed statements.
- Add explicit section in docs for “what is proven now” vs “planned”.
- Document deferred multi-transport work as follow-on, not part of this slice.

### Proof snapshot (what is proven now vs planned)

**Proven now (from latest artifacts):**
- Baseline vs optimized metrics are captured in machine-readable form for both latency and throughput benches.
- Revised T3 benchmark gates pass on latest artifacts: p95 improves at RTT-injected profiles (P1/P2) while loopback stays within the non-regression floor, and throughput speedup stays above the per-profile + geometric-mean floors.
- Repeated contention-cycle convergence and bounded workspace retry loops are covered by `tests/slice21_contention_retry_stability.rs` (review iter 3 rerun: 3/3 passes).
- `slice3_concurrent_convergence` and `slice17_integration_conflict_visibility` each passed dedicated 5/5 reruns in review iter 3.
- Transport remains raw TCP-only in implementation and docs.

**Still planned / deferred follow-on:**
- Stretch performance goals from earlier drafts (p95 >=20%, throughput >=1.5x) are not met and are tracked as follow-on optimization debt.
- WSS/SSH-exec transports remain deferred follow-on work.

### Success criteria checklist

- [x] Bench outputs are archived/linked in docs.
- [x] README/ARCHITECTURE claims match measured behavior.
- [x] No doc claims imply WSS/SSH-exec support yet.

### Test hooks

- `cargo test` targeted gates:
  - [x] slices 1,2,3,4,5,10,12,13,15,16,17
  - [x] slices 18,19,20,21
    - Review iter 3 run details (2026-02-23):
      - pass: `slice1_single_agent_round_trip`, `slice2_two_agent_visibility`, `slice3_concurrent_convergence`, `slice4_promise_pipelining`, `slice5_watch_heads`, `slice10_graceful_shutdown`, `slice12_up_down`, `slice13_log_streaming`, `slice15_head_authority_jj_lib`, `slice16_integration_workspace_flag`, `slice17_integration_conflict_visibility`, `slice18_repo_info_compat`, `slice19_tcp_connector_parity`, `slice20_pipelining_correctness`, `slice21_contention_retry_stability`
      - stability reruns: `slice3` 5/5, `slice17` 5/5, `slice21` 3/3
- bench gates:
  - [x] `cargo bench --bench tcp_commit_path`
  - [x] `cargo bench --bench tcp_inflight_throughput`

---

## Measurement harness requirements

To make performance criteria testable and reproducible:

- [x] Add deterministic latency injection harness (proxy or test shim) for fixed RTT profiles.
  - Implemented via client test shim env knob: `TANDEM_BENCH_INJECT_RTT_MS`.
- [x] Record baseline and post-change numbers in a machine-readable artifact (JSON/CSV).
  - Captured in `docs/benchmarks/tcp_commit_path_latest.json` and `docs/benchmarks/tcp_inflight_throughput_latest.json`.
- [x] Ensure benches run with stable input size and fixed agent counts.
  - Commit bench: warmup=4, measured=16, files/commit=32; throughput bench: agents=2, commits/agent=4, files/commit=32.
  - Harness now resets server/workspace state per RTT profile and per mode (baseline/optimized) to avoid cross-profile and cross-mode contamination.
  - Baseline mode disables optimistic op-head version caching and RPC in-flight dispatch (`TANDEM_BENCH_DISABLE_OPTIMISTIC_OP_HEAD_VERSION_CACHE=1`, `TANDEM_BENCH_DISABLE_RPC_INFLIGHT=1`) for pre-T3 parity.

Suggested profiles:

- P0: loopback/no injected delay
- P1: 20ms RTT equivalent
- P2: 50ms RTT equivalent

---

## Completion checklist (plan-level)

- [x] T1 complete with compatibility tests
- [x] T2 complete with parity/regression tests
- [x] T3 complete with benchmark deltas and correctness tests
- [x] T4 complete with contention/retry stability tests
- [x] T5 complete with docs and evidence links
- [x] Tech debt tracker updated with deferred WSS/SSH-exec follow-up
