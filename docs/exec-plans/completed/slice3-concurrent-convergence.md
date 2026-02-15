# Slice 3 — Concurrent convergence (v1)

- **Date completed:** 2026-02-15
- **Test file(s):** `tests/v1_slice3_concurrent_convergence.rs`

## What was implemented

Concurrent write convergence via jj's transaction retry mechanism and tandem's CAS op-head coordination:

1. **CAS-based op-head updates**
   - `TandemOpHeadsStore::update_op_heads` uses server's `updateOpHeads` RPC
   - Server implements compare-and-swap on operation heads
   - On conflict, jj-lib's transaction layer automatically retries with merged state

2. **Multi-agent concurrent writes**
   - Multiple agents write different files simultaneously
   - Each agent commits independently (no locks)
   - CAS contention triggers automatic retry
   - All commits converge as operation graph merges

3. **File content preservation**
   - Tests verify that concurrent writes to different files don't lose data
   - Each agent's file survives and is readable via `tandem file show`
   - No merge conflicts at store layer — jj handles operation merging

## Acceptance coverage

Integration tests validate:

- **Two-agent concurrent writes** — both commits succeed, both files readable
- **Five-agent concurrent writes** — all 5 commits succeed, all 5 files readable
- File content assertions verify exact bytes, not just descriptions
- Under full cargo test load, 5-agent test may flake (see tech debt tracker)

## Architecture notes

This slice validated tandem's concurrency model:
- CAS on operation heads provides coordination primitive
- jj-lib's transaction retry handles merge automatically
- No application-level locking needed
- File-level write conflicts are impossible (content-addressed storage)
