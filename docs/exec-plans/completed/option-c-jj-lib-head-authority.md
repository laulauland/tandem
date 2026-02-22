# Completed Execution Plan — Option C: jj-lib as Single Head Authority

- **Status:** Completed
- **Created:** 2026-02-22
- **Completed:** 2026-02-22
- **Owner:** tandem core

## Goal

Make tandem server state transitions fully jj-lib-driven.

**Design assumption for this plan:** greenfield semantics only.
- No backward compatibility work.
- No migration logic for existing projects.
- No dual-write fallback paths.

## Problem

Today the server maintains tandem head state in `.jj/repo/tandem/heads.json` and also mirrors to jj op-head files.
That split makes correctness harder and introduces drift risk.

## Target model (Option C)

1. **Head set authority:** jj-lib repository state (op-heads via jj-lib), not tandem JSON.
2. **Tandem metadata sidecar:** `.jj/repo/tandem/heads.json` stores only tandem-specific metadata:
   - monotonic CAS `version`
   - `workspace_heads` map (workspace -> commit)
3. **All server mutations use jj-lib APIs** for repo state changes.
4. **No manual op-head file sync code path.**

## In scope

- Server head update path (`updateOpHeads`) rewritten to jj-lib authority flow.
- Server head read path (`getHeads`) sourced from jj-lib.
- Remove manual `op_heads/heads` file sync implementation.
- Keep RPC shape unchanged for now.

## Out of scope

- Integration workspace/continuous merge feature.
- New RPC methods.
- Policy/orchestration behavior.
- Backward compatibility and migrations.

## Implementation slices

### Slice C1 — Server repo authority setup
- Refactor server initialization to keep jj-lib repo loader/readonly repo handles as first-class state.
- Remove assumptions that head truth comes from tandem JSON.

### Slice C2 — `getHeads` from jj-lib
- Replace `get_heads_sync()` head source with jj-lib op-head resolution.
- Continue returning tandem `version` + `workspace_heads` from metadata sidecar.

### Slice C3 — `updateOpHeads` jj-lib mutation path
- Under server lock:
  1. read metadata sidecar and CAS-check `expected_version`
  2. apply head transition using jj-lib op-heads APIs (no raw file writes)
  3. read resulting heads from jj-lib
  4. persist incremented metadata `version` + updated `workspace_heads`
  5. notify watchers
- Any failure in steps 2–4 fails the RPC; no warn-and-continue split state.

### Slice C4 — Remove dual-authority code
- Delete manual helpers that read/write op-head files directly.
- Remove “sync op-heads to jj” fallback warnings and dead paths.
- Keep only metadata persistence logic in tandem sidecar.

## Acceptance criteria

1. Server has exactly one head authority path: jj-lib.
2. No direct filesystem op-head mutation code remains in tandem server.
3. `updateOpHeads` success implies:
   - jj-lib-visible head update,
   - metadata sidecar `version` increment,
   - watcher notification.
4. `updateOpHeads` failure leaves state unchanged for both jj-lib heads and metadata.
5. Existing lifecycle and log streaming behavior still passes tests.

## Test plan

- Run existing tests:
  - `cargo test --test slice11_control_socket -- --nocapture`
  - `cargo test --test slice12_up_down -- --nocapture`
  - `cargo test --test slice13_log_streaming -- --nocapture`
  - `cargo test --test slice3_concurrent_convergence -- --nocapture`
- Add new integration test file (suggested: `tests/slice15_head_authority_jj_lib.rs`) proving:
  - server-local jj head view and tandem `getHeads` are consistent after concurrent updates,
  - no divergence after repeated CAS conflicts.

## Agent kickoff task (copy/paste)

Implemented from this plan as written (greenfield behavior, no migration/backward-compat).

Sequence:
1. Add failing integration tests for C2/C3/C4 acceptance.
2. Refactor server head read/write paths to jj-lib authority.
3. Remove manual op-head file sync code.
4. Make tests pass and run the listed test plan.
5. Update docs touched by this change (`ARCHITECTURE.md`, `docs/design-docs/rpc-protocol.md`, `docs/design-docs/jj-lib-integration.md`).
