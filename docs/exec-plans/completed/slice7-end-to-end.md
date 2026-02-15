# Slice 7 — End-to-end multi-agent (v1)

- **Date completed:** 2026-02-15
- **Test file(s):** `tests/slice7_end_to_end.rs`

## What was implemented

Complete workflow integration: multi-agent collaboration + git shipping + external contributions:

1. **Multi-agent file collaboration**
   - Agent A writes `auth.rs`, commits
   - Agent B writes `api.rs`, commits concurrently
   - Both agents see each other's files via `tandem file show`
   - Both files readable with exact byte content

2. **Git shipping from server**
   - Server creates bookmark pointing to merge of both agents' work
   - Server pushes to GitHub (bare git remote)
   - `git clone` of remote contains both `auth.rs` and `api.rs` with correct content

3. **External contribution round-trip**
   - External contributor clones git repo
   - Adds `docs.rs`, commits, pushes back to remote
   - Server fetches from git remote
   - Both agents can immediately `tandem file show` the external file — exact bytes match

4. **Bookmark management**
   - Agents create bookmarks via `tandem bookmark create`
   - Bookmarks visible to all agents
   - Server pushes bookmarks to git remote

## Acceptance coverage

Integration test `end_to_end_multi_agent_git_workflow` validates the complete workflow:

- Two agents write different files concurrently
- Cross-agent file visibility (exact bytes)
- Git push to remote succeeds
- Git clone contains all files with correct content
- External git contribution round-trips through tandem
- All agents can read external contribution

## Architecture notes

This slice validated the complete tandem vision:
- Agents collaborate in real-time on same codebase
- Server is the point of origin for git operations
- External contributors work through normal git workflow
- No impedance mismatch between tandem and git
