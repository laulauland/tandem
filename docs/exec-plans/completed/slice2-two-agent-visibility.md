# Slice 2 — Two-agent visibility

- **Date completed:** 2026-02-15
- **Test file(s):** `tests/slice2_two_agent_visibility.rs`

## What was implemented

Multi-workspace support through jj-lib's native workspace model:

1. **Workspace initialization**
   - `tandem init --tandem-server <addr> --workspace <name> <path>`
   - Each agent gets its own workspace backed by the shared tandem server
   - Workspaces tracked in jj's `View.wc_commit_ids` map (standard jj model)

2. **File visibility across workspaces**
   - Agent A writes `auth.rs`, commits via `tandem new`
   - Agent B runs `tandem log` — sees Agent A's commit
   - Agent B runs `tandem file show -r <change-id> auth.rs` — gets exact bytes
   - No special "workspace sync" command — stock jj just works

3. **Backend transparency**
   - All file/tree/commit reads go through TandemBackend RPC
   - Both agents read from same server store
   - Working copies are local, objects are remote

## Acceptance coverage

Integration test `two_agent_file_visibility` validates:

- Agent A writes `auth.rs` with specific content
- Agent B reads it back via `tandem file show` — exact bytes match
- Agent B writes `api.rs`, Agent A reads it back — exact bytes match
- Both agents see each other's commits in `tandem log`

## Architecture notes

This slice proved that jj's workspace model maps cleanly to tandem's server-client architecture:
- No custom workspace protocol needed
- Standard jj `View.wc_commit_ids` tracks which workspace is on which commit
- Backend RPC layer is workspace-agnostic — View/OpStore handle workspace coordination
