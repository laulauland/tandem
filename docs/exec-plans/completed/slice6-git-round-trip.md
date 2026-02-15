# Slice 6 — Git round-trip

- **Date completed:** 2026-02-15
- **Test file(s):** `tests/slice6_git_round_trip.rs`

## What was implemented

Full git interop via server-side jj+git colocated repo:

1. **Server storage via Git backend**
   - Server uses jj-lib's Git backend (not Simple backend)
   - Objects stored as native git objects (SHA-1 hashes)
   - `jj git push` and `jj git fetch` work on server repo

2. **Git push from server**
   - Agent writes file via tandem, commits
   - Server-side: `jj bookmark create main -r <commit>`
   - Server-side: `jj git push --bookmark main`
   - Git remote contains commit with correct file content

3. **Git fetch to server**
   - External contributor pushes to git remote
   - Server-side: `jj git fetch`
   - Agent runs `tandem file show` on fetched commit — exact bytes match

## Acceptance coverage

Integration test `git_round_trip_with_real_files` validates:

- Agent writes `feature.rs` via tandem
- Server pushes to bare git repo
- `git show HEAD:feature.rs` returns exact bytes
- External commit to git repo with `contrib.rs`
- Server fetches, agent reads `contrib.rs` via tandem — exact bytes match
- File content is byte-identical at every stage

## Architecture notes

This slice proved that tandem is transparent to git:
- Server repo is a normal jj+git colocated repo
- No special git layer needed in tandem
- All git operations are server-side only (orchestrator responsibility)
- Agents never need git access
