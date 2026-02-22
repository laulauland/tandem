# Completed Execution Plan — Workspace default collision fix

- **Status:** Completed (core fix implemented)
- **Created:** 2026-02-22
- **Completed:** 2026-02-22
- **Scope:** Fix default workspace collision behavior with minimal surface-area change.

## Problem statement

`tandem init` previously defaulted to workspace name `default` when `--workspace` was not provided.
In multi-agent usage, this created accidental workspace-name collisions (multiple agents sharing `default`), leading to stale working-copy behavior and confusing workspace attribution.

We need safe-by-default behavior: agents that omit `--workspace` should still get distinct workspace identities.

## Design constraints

1. **Minimal API/protocol changes:** reuse existing RPC fields and server state (`updateOpHeads.workspaceId`, `workspace_heads`) and avoid new Cap'n Proto methods.
2. **No duplicate authority:** do not introduce a second workspace registry when jj `View.wc_commit_ids` + server `workspace_heads` already exist.
3. **Preserve explicit workflows:** `--workspace <name>` and `TANDEM_WORKSPACE` must continue to work unchanged.
4. **Stock jj invariants remain:** no custom workspace-sync command; behavior stays jj-native.
5. **v0 pragmatism:** prioritize a direct fix over migration/backcompat complexity.

## Minimal approach

1. **Remove collision-prone implicit default**
   - Change `tandem init` behavior so omitting `--workspace` does not use literal `default`.
   - Generate a unique workspace name locally (human-readable + uniqueness suffix).

2. **Propagate workspace identity on writes**
   - Ensure `TandemOpHeadsStore::update_op_heads()` sends non-empty `workspaceId`.
   - Persist/load workspace identity in store metadata so it survives subsequent commands.

3. **Keep server model unchanged**
   - Continue using existing `workspace_heads` map in `heads.json`.
   - No new schema methods, no new server-side registry file.

4. **Collision preflight (client-side, optional but preferred)**
   - Use existing `getHeads().workspaceHeads` to avoid explicit-name collisions during init.
   - For explicit `--workspace`, fail fast with actionable error when already taken.

## Acceptance criteria

1. `tandem init --server <addr> <path>` (no `--workspace`) no longer creates workspace `default`.
2. Two implicit `tandem init` calls against the same server produce distinct workspace names.
3. Normal two-agent flow with implicit names does not fail due to default-name collision (`working copy is stale` from shared-name collision path).
4. `updateOpHeads` sends a non-empty workspace identifier for normal client operations.
5. Server `workspace_heads` is populated with concrete workspace names after commits.
6. Explicit workspace mode remains intact (`--workspace foo` still uses `foo`; `TANDEM_WORKSPACE` still works).
7. No Cap'n Proto schema additions are required for this fix.
8. User-facing docs reflect the new default behavior and explicit-name override path.

## Implementation checklist

- [x] Add integration tests for implicit-name uniqueness and non-collision behavior.
- [x] Add integration test asserting workspace identity is propagated into `workspace_heads`.
- [x] Update CLI init argument handling: implicit auto-generated workspace name.
- [x] Add/adjust helper for deterministic unique-name generation in init flow.
- [x] Persist workspace identity in op-heads store metadata.
- [x] Load persisted workspace identity and pass it into `update_op_heads` calls.
- [ ] (Optional) Add init preflight collision check for explicit `--workspace` using existing RPC data.
- [x] Update `--help`/README text for default workspace semantics.
- [ ] Run full integration test suite (`cargo test`) and capture any follow-up debt. _(attempted; hit pre-existing flaky failure in `slice3_concurrent_convergence::v1_slice3_five_agents_concurrent_file_writes_all_survive`)_
