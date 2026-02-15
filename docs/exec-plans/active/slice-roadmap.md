# Completed Execution Plan: Slice Roadmap

**Status:** All slices completed as of 2026-02-15.
**See:** `docs/exec-plans/completed/` for detailed completion notes.

**Stock `jj` on the client, tandem as a remote jj store backend.**

The client and server store real jj objects (commits with tree pointers,
trees with file entries, file blobs) so that `jj` itself is the client CLI.

## Invariant

Every slice must pass its acceptance criteria using **stock `jj` commands**
on the client side. No custom `tandem new/log/describe/diff` CLI.
The only tandem-specific commands are `tandem serve` and `tandem watch`.

---

## Slice 1 — Single-agent file round-trip ✓

**Completed:** 2026-02-15
**Test file:** `tests/slice1_single_agent_round_trip.rs`

Goal: one agent uses stock `jj` with tandem as the remote store backend.
Files written locally survive the round-trip through the server.

Acceptance:
- Agent creates a jj workspace backed by tandem server
- Agent writes `src/hello.rs` with known content, runs `jj new -m "add hello"`
- Under the hood: `putObject(file, <bytes>)`, `putObject(tree, ...)`,
  `putObject(commit, ...)`, `putOperation`, `putView`, `updateOpHeads`
  all go over Cap'n Proto to the server
- `jj log` shows commit with correct description
- `jj diff -r @-` shows `src/hello.rs` was added (file-level diff)
- `jj cat -r @- src/hello.rs` returns exact file bytes from server
- Server restart: reconnect, `jj log` still works, file still readable
- Server-side `jj log` matches client-side `jj log`
- Server-side `jj cat` returns same file bytes

## Slice 2 — Two-agent file visibility ✓

**Completed:** 2026-02-15
**Test file:** `tests/slice2_two_agent_visibility.rs`

Goal: two agents on separate workspaces see each other's files.

Acceptance:
- Agent A writes `src/auth.rs`, commits
- Agent B (different workspace) runs `jj log` — sees Agent A's commit
- Agent B runs `jj cat -r <agent-a-commit> src/auth.rs` — gets exact bytes
- Agent B writes `src/api.rs`, commits
- Agent A runs `jj cat -r <agent-b-commit> src/api.rs` — gets exact bytes
- Both agents see both files through jj's normal tree traversal
- `jj diff` between the two workspace heads shows both files

## Slice 3 — Concurrent file writes converge ✓

**Completed:** 2026-02-15
**Test file:** `tests/slice3_concurrent_convergence.rs`

Goal: concurrent commits with different files don't lose data.

Acceptance:
- Agent A writes `src/a.rs` and commits simultaneously with Agent B writing `src/b.rs`
- CAS contention triggers retries
- After convergence: both commits exist as heads
- `jj cat src/a.rs` works from both agents' perspectives
- `jj cat src/b.rs` works from both agents' perspectives
- No file content is lost or corrupted
- 5-agent variant: each writes a unique file, all 5 files survive

## Slice 4 — Promise pipelining for object writes ✓

**Completed:** 2026-02-15
**Test file:** `tests/slice4_promise_pipelining.rs`

Goal: `putObject(file) → putObject(tree) → putObject(commit) → putOperation → putView → updateOpHeads`
pipelines without waiting for each response.

Acceptance:
- Commit with files completes in fewer RTTs than sequential calls
- Latency benchmark under artificial RPC delay proves pipelining
- All slice 1-3 tests still pass

## Slice 5 — WatchHeads with file awareness ✓

**Completed:** 2026-02-15
**Test file:** `tests/slice5_watch_heads.rs`

Goal: agents receive real-time notifications when new commits (with files) land.

Acceptance:
- Agent A watches, Agent B writes `src/new.rs` and commits
- Agent A's watcher fires with the new head version
- Agent A can immediately `jj cat -r <new-head> src/new.rs` — gets bytes
- Multiple watchers all receive updates
- Reconnect after server restart catches up

## Slice 6 — Git round-trip with real files ✓

**Completed:** 2026-02-15
**Test file:** `tests/slice6_git_round_trip.rs`

Goal: files written through tandem survive push to git and fetch back.

Acceptance:
- Agent writes `src/feature.rs` via tandem-backed jj
- Server-side: `jj bookmark create main -r <tip>`, `jj git push --bookmark main`
- Clone bare git remote: `git show HEAD:src/feature.rs` returns exact file bytes
- External git contributor adds `src/contrib.rs`, pushes to remote
- Server-side: `jj git fetch`
- Agent runs `jj cat -r <fetched-commit> src/contrib.rs` — gets exact bytes
- File content is byte-identical at every stage of the round-trip

## Slice 7 — End-to-end multi-agent with git shipping ✓

**Completed:** 2026-02-15
**Test file:** `tests/slice7_end_to_end.rs`

Goal: two agents collaborate on real files, ship via git, external contributor
round-trips back.

Acceptance:
- Agent A writes `src/auth.rs`, commits
- Agent B writes `src/api.rs`, commits concurrently
- Both see each other's files via `jj cat`
- Server pushes to GitHub (bare git remote)
- `git clone` of remote contains both `src/auth.rs` and `src/api.rs`
  with correct content
- External contributor clones, adds `src/docs.rs`, pushes back
- `jj git fetch` on server, agents see `src/docs.rs` via `jj cat`

## Slice 8 — Bookmark management via RPC ✓

**Completed:** 2026-02-15 (via stock jj bookmark commands)
**Test coverage:** `tests/slice7_end_to_end.rs` (includes bookmark creation)

Goal: agents manage bookmarks through tandem without server-side shell access.

Acceptance:
- Agent runs `jj bookmark create feature-x` — routed through tandem RPC
- Other agent runs `jj bookmark list` — sees `feature-x`
- `jj git push --bookmark feature-x` works from client side
  (or via RPC command that triggers server-side push)
- Bookmark state is consistent across agents

## Slice 9 — CLI help and agent discoverability ✓

**Completed:** 2026-02-15
**Implementation:** `src/main.rs` (clap help text, AFTER_HELP constants)

Goal: agents can discover tandem server commands without reading source code.

Acceptance:
- `tandem --help` prints usage without requiring server connection
- `tandem serve --help` explains flags
- Error messages suggest valid alternatives ("unknown command X, did you mean Y?")
- `TANDEM_SERVER` env var works as fallback for `--server` flag
- `TANDEM_WORKSPACE` env var works (already exists, just needs documentation)

---

## Implementation notes

### Client architecture

The client is a **jj-lib Backend impl**:

```rust
struct TandemBackend { store: store::Client }

impl Backend for TandemBackend {
    fn read_file(&self, id: &FileId) -> BackendResult<Box<dyn Read>> {
        // getObject(file, id) over Cap'n Proto
    }
    fn write_file(&self, contents: &mut dyn Read) -> BackendResult<FileId> {
        // putObject(file, data) over Cap'n Proto
    }
    fn read_tree(&self, id: &TreeId) -> BackendResult<Tree> {
        // getObject(tree, id) over Cap'n Proto
    }
    // ... etc for commit, symlink, copy
}

struct TandemOpStore { store: store::Client }
impl OpStore for TandemOpStore { /* putOperation, putView, etc */ }

struct TandemOpHeadsStore { store: store::Client }
impl OpHeadsStore for TandemOpHeadsStore { /* getHeads, updateOpHeads */ }
```

The client binary:
- `tandem serve --listen <addr> --repo <path>` — server mode
- `tandem watch` — head change notifications
- `tandem --help` — local-only help
- All other commands: use **stock `jj`** configured to use TandemBackend

### Server storage

The server stores real jj-compatible object bytes via direct content-addressed
storage that IS the jj store:
- `objects/commit/<id>` — jj protobuf commit (with tree_id, parent_ids)
- `objects/tree/<id>` — jj protobuf tree (with file entries)
- `objects/file/<id>` — raw file bytes
- `operations/<id>` — jj protobuf operation
- `views/<id>` — jj protobuf view
