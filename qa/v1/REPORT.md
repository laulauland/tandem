# Tandem v1 QA Report — Agent Usability Evaluation

**Date:** 2026-02-15
**Tester:** Automated agent (Claude opus)
**Binary:** `target/debug/tandem` (cargo build, clean)
**Method:** Manual agent-perspective testing of all documented workflows
**Server:** `tandem serve --listen 127.0.0.1:13099 --repo /tmp/tandem-qa-v1-repo`

---

## Executive Summary

**Tandem v1 is a massive improvement over v0.** The v0 QA found agents spending 50% of time guessing commands with no `--help`, no file content storage, and no code review capability. All three P0 blockers from v0 are resolved:

1. ✅ `--help` works without server connection
2. ✅ File content is stored and readable via `jj file show` / `jj diff` / `jj show`
3. ✅ `TANDEM_SERVER` env var works as fallback

The tool now embeds full jj — every jj command works transparently. An agent can write files, commit, read other agents' files, see diffs, manage bookmarks, and view operation history. **This is a usable multi-agent collaboration tool.**

**Verdict: Tandem v1 is agent-ready for core workflows. Two minor UX issues remain.**

---

## v0 → v1 P0 Issue Resolution

| v0 Issue | v0 Status | v1 Status | Evidence |
|----------|-----------|-----------|----------|
| `--help` works without server | 🔴 RED | ✅ GREEN | Prints full usage with commands, env vars, examples |
| File content storage + readback | 🔴 RED | ✅ GREEN | `jj file show`, `jj diff`, `jj show` all work |
| `TANDEM_SERVER` env var | 🔴 RED | ✅ GREEN | `TANDEM_SERVER=host:port tandem init .` works |
| Command suggestions on error | 🔴 RED | ✅ GREEN | jj provides "tip: a similar subcommand exists" |
| Code review capability | 🔴 RED | ✅ GREEN | Full diffs, file listing, show command all work |
| Bookmark management | 🔴 RED | ✅ GREEN | `tandem bookmark create/list` work transparently |
| Commit stores only descriptions | 🔴 RED | ✅ GREEN | Real jj commits with file trees |

**All 7 P0 issues from v0 are resolved.**

---

## Test Results by Area

### 1. DISCOVERY — `tandem --help`

**Score: ✅ GREEN**

| What I tried | Output | Agent-friendly? |
|---|---|---|
| `tandem --help` | Full usage: tandem commands, jj commands, env vars, setup examples | Yes — excellent |
| `tandem` (no args) | Same as `--help` | Yes — prints usage, not error |
| `tandem serve --help` | Shows `--listen` and `--repo` flags with examples | Yes |
| `tandem init --help` | Shows `--tandem-server`, `--workspace`, env vars, examples | Yes |

**Key improvement over v0:** Help text works *without* a server connection. An agent's first instinct (`tool --help`) immediately works. The output includes environment variables, all commands, and working examples.

**Actual output of `tandem --help`:**
```
tandem — jj workspaces over the network

USAGE:
    tandem [OPTIONS] <COMMAND> [ARGS...]

TANDEM COMMANDS:
    serve       Start the tandem server
    init        Initialize a tandem-backed workspace
    watch       Stream head change notifications (requires server)

JJ COMMANDS:
    All standard jj commands work transparently:
      tandem log            Show commit history
      tandem new            Create a new change
      tandem diff           Show changes in a revision
      tandem cat            Print file contents at a revision
      tandem bookmark       Manage bookmarks
      tandem describe       Update change description
      ... and every other jj command

OPTIONS:
    --help, -h              Print this help message

ENVIRONMENT:
    TANDEM_SERVER           Server address (host:port)
    TANDEM_WORKSPACE        Workspace name (default: "default")

SETUP:
    # Start a server
    tandem serve --listen 0.0.0.0:13013 --repo /path/to/repo

    # Initialize a workspace backed by the server
    tandem init --tandem-server server:13013 my-workspace

    # Use jj normally
    cd my-workspace
    echo 'hello' > hello.txt
    tandem new -m 'add hello'
    tandem log
```

---

### 2. INIT — Workspace Setup

**Score: ✅ GREEN**

| What I tried | Output | Works? |
|---|---|---|
| `tandem init --tandem-server=host:port /path` | `Initialized tandem workspace 'default' at /path (server: host:port)` | ✅ |
| `tandem init --tandem-server=host:port --workspace agent-b /path` | `Initialized tandem workspace 'agent-b' at /path` | ✅ |
| `TANDEM_SERVER=host:port tandem init /path` | Works via env var | ✅ |
| `tandem init` (no server) | Falls through to jj error (see issues) | ⚠️ |
| `tandem init /path` (already exists) | `error: workspace init failed: The destination repo already exists` | ✅ |
| `tandem init --tandem-server=bad:99999 /path` | `error: workspace init failed: failed to connect to tandem server at bad:99999` | ✅ |

**What init creates:** A `.jj/` directory with `repo/` and `working_copy/` subdirectories. The workspace is immediately functional — `tandem log` shows root commit.

---

### 3. FILE ROUND-TRIP — Write, Commit, Read Back

**Score: ✅ GREEN**

**Test sequence:**
```bash
cd /tmp/workspace-a
echo 'hello world from agent A' > test.txt
tandem new -m 'add test file'
tandem file show -r @- test.txt   # → "hello world from agent A"
tandem diff -r @-                 # → shows test.txt added
tandem show @-                    # → full commit with diff
```

| What I tried | Result |
|---|---|
| Write text file, commit, read back | ✅ Exact content match |
| Binary file (7 bytes, includes `\x00\xff`) | ✅ Exact byte match via `cmp` |
| Large file (1MB random) | ✅ SHA match after round-trip |
| `tandem diff -r @-` | ✅ Shows file additions with content |
| `tandem diff --stat` | ✅ Shows file stats |
| `tandem show @-` | ✅ Full commit metadata + diff |
| `tandem file list -r <rev>` | ✅ Lists all files in commit tree |
| `tandem status` | ✅ Shows working copy state |

**Key improvement over v0:** v0 stored only descriptions — no files, no diffs, no content. v1 stores real jj commits with full file trees. Every jj command that reads content works.

---

### 4. MULTI-AGENT — Cross-Workspace Visibility

**Score: ✅ GREEN**

**Setup:** Two workspaces (default + agent-b) connected to same server.

| What I tried | Result |
|---|---|
| B runs `tandem log` — sees A's commits | ✅ Both branches visible |
| B reads A's file: `tandem file show -r <A's rev> test.txt` | ✅ Returns "hello world from agent A" |
| A reads B's file: `tandem file show -r <B's rev> agent_b.txt` | ✅ Returns "hello from agent B" |
| Third workspace (agent-c) sees both A and B | ✅ Full graph visible |
| `tandem workspace list` | ✅ Shows all workspaces + their heads |
| A creates bookmark, B sees it via `tandem bookmark list` | ✅ Bookmarks shared |

**Actual workspace list output:**
```
agent-b: xr ace144d9 (empty) parallel write B
agent-c: os de16beaf (empty) parallel write C
default: xy 19bbdd1d (empty) parallel write A
```

---

### 5. ERROR STATES

**Score: ✅ GREEN**

| Error condition | Output | Agent-friendly? |
|---|---|---|
| `tandem serve` (no flags) | `error: serve requires --listen <addr>` + hint | ✅ Progressive |
| `tandem serve --listen x` (no repo) | `error: serve requires --repo <path>` + hint | ✅ Progressive |
| `tandem serve --listen bad --repo .` | `error: failed to bind bad: invalid socket address` | ✅ Clear |
| `tandem foobar` | `error: unrecognized subcommand 'foobar'` + `tip: a similar subcommand exists: 'bookmark'` | ✅ Suggests alternatives |
| `tandem init --tandem-server=bad:99999 /path` | `error: workspace init failed: failed to connect to tandem server at bad:99999` | ✅ Includes address |
| `tandem init /existing/.jj` | `error: workspace init failed: The destination repo already exists` | ✅ Clear |
| `tandem log` in non-repo dir | `Error: There is no jj repo in "."` | ✅ Standard jj error |
| Unreachable server (192.0.2.1:13099) | Hangs (>30s timeout) | ⚠️ No timeout |

---

### 6. CONCURRENT — Parallel Writes

**Score: ✅ GREEN**

**Test 1: Sequential rapid writes from two agents**
- Agent A: 3 rapid commits, Agent B: 3 rapid commits
- Result: All 6 commits present, correct parent chains
- "Concurrent modification detected, resolving automatically" — handled transparently

**Test 2: Truly parallel writes (3 agents simultaneously)**
```bash
# A, B, C write in parallel background processes
(cd ws-a && echo "parallel from A" > parallel_a.txt && tandem new -m "parallel write A") &
(cd ws-b && echo "parallel from B" > parallel_b.txt && tandem new -m "parallel write B") &
(cd ws-c && echo "parallel from C" > parallel_c.txt && tandem new -m "parallel write C") &
wait
```
- Result: ✅ All 3 commits present, all 3 files readable from any workspace
- File content verified: exact matches across all workspaces

---

### 7. PERSISTENCE — Kill Server, Restart, Verify

**Score: ✅ GREEN**

**Procedure:**
1. Created ~15 commits across 3 workspaces
2. `kill $SERVER_PID` — server stopped
3. Restarted: `tandem serve --listen 127.0.0.1:13099 --repo /same/path`
4. Verified from workspace A:
   - `tandem log` — all commits present with correct graph
   - `tandem file show -r <rev> test.txt` — "hello world from agent A" ✅
   - `tandem file show -r <rev> concurrent_b_1.txt` — "concurrent file B-1" ✅

All data, commit metadata, file trees, and workspace assignments survived the restart.

---

### 8. INTEGRATION TESTS

**Score: ✅ GREEN**

```
slice1_single_agent_file_round_trip .................. ok (1.61s)
v1_slice2_two_agent_file_visibility .................. ok (1.45s)
v1_slice3_two_agents_concurrent_file_writes_converge . ok
v1_slice3_five_agents_concurrent_file_writes_all_survive ok (5.28s)
```

All 4 integration tests pass. Tests assert on **file bytes** (not just descriptions), which was the critical v0 gap.

---

## Issues Found

### 🟡 YELLOW — `tandem init` without `--tandem-server` shows confusing jj error

**What happens:**
```
$ tandem init /tmp/workspace
error: unrecognized subcommand 'init'
Hint: You probably want `jj git init`. See also `jj help git`.
```

**Expected:** Should show `tandem init --help` or say "init requires --tandem-server <addr>".

**Why it matters:** When `--tandem-server` is missing, the `init` command falls through to jj's CLI which doesn't have an `init` subcommand. The jj error message ("You probably want `jj git init`") is misleading — the agent wants tandem init, not jj git init.

**Fix:** Detect `init` as a tandem command even without `--tandem-server` and show the tandem init help text.

---

### 🟡 YELLOW — `tandem cat` listed in help but doesn't work

**What happens:**
```
$ tandem cat -r @- test.txt
error: unrecognized subcommand 'cat'
```

**The help text says:** `tandem cat    Print file contents at a revision`

**What works instead:** `tandem file show -r @- test.txt`

**Why it matters:** The help text advertises `tandem cat` as a command, but jj renamed `cat` to `file show` in recent versions. An agent following the help text will hit this error.

**Fix:** Either update the help text to say `tandem file show` instead of `tandem cat`, or add a jj alias `cat = ["file", "show"]` in the workspace config.

---

### 🟡 YELLOW — Connection to unreachable server hangs indefinitely

**What happens:** `tandem init --tandem-server=192.0.2.1:13099 /tmp/ws` hangs for >30 seconds with no output.

**Expected:** Should timeout after ~5s with an error like "connection timed out to 192.0.2.1:13099".

**Impact:** Low — agents rarely connect to unreachable hosts. Bad ports (99999) fail fast.

---

### ⚠️ NOTE — Leaked server processes from integration tests

During testing, I found **30 orphaned `tandem serve` processes** from previous integration test runs, each listening on different ports in `/var/folders/*/T/`. These are from `cargo test` and suggest the test harness doesn't always clean up server processes on completion.

**Impact:** Resource leak on CI/dev machines. Not a user-facing issue.

---

### ⚠️ NOTE — fsmonitor.backend = "watchman" conflict

The tandem binary is not compiled with the `watchman` feature, but the user's jj config sets `fsmonitor.backend = "watchman"`. This causes:
```
Internal error: Failed to snapshot the working copy
Cannot query Watchman because jj was not compiled with the `watchman` feature
```

**Workaround:** Add `[fsmonitor]\nbackend = "none"` to workspace config.

**Suggestion:** `tandem init` should set this automatically, or the tandem binary should override the fsmonitor config.

---

## Scorecard

| Area | Score | Notes |
|------|-------|-------|
| Discovery (`--help`) | ✅ GREEN | Works locally, comprehensive, includes examples |
| Init (workspace setup) | ✅ GREEN | Clean init, good messages, env var support |
| File round-trip | ✅ GREEN | Text, binary, large files all round-trip perfectly |
| Multi-agent visibility | ✅ GREEN | Full cross-workspace file read, workspace list |
| Error states | ✅ GREEN | Progressive errors, suggestions, clear messages |
| Concurrent writes | ✅ GREEN | 3 parallel agents, all data preserved |
| Persistence | ✅ GREEN | Kill + restart, all data survives |
| Integration tests | ✅ GREEN | 4/4 pass, assert on file bytes |
| `init` without `--tandem-server` | 🟡 YELLOW | Falls through to confusing jj error |
| `cat` command in help text | 🟡 YELLOW | Help says `cat`, but jj uses `file show` |
| Connection timeout | 🟡 YELLOW | Unreachable server hangs, no timeout |

---

## v0 vs v1 Comparison

| Metric | v0 | v1 | Change |
|--------|----|----|--------|
| Agent discoverability | 🔴 5/10 | ✅ 9/10 | +4 |
| File content storage | ❌ None | ✅ Full jj trees | Fixed |
| Code review capability | ❌ Blocked | ✅ Full diffs + file read | Fixed |
| Help text | ❌ None | ✅ Comprehensive | Fixed |
| Error messages | 🟡 Partial | ✅ Progressive + suggestions | Improved |
| Bookmark management | ❌ None | ✅ Full jj bookmark | Fixed |
| Command suggestions | ❌ None | ✅ jj provides "did you mean" | Fixed |
| `TANDEM_SERVER` env var | ❌ None | ✅ Works | Fixed |
| Concurrent writes | ✅ CAS works | ✅ CAS + file trees | Maintained |
| Persistence | ✅ Works | ✅ Works | Maintained |

---

## Recommendations

### P1 — Minor fixes

1. **Fix `tandem init` without `--tandem-server`** — Detect `init` as tandem command, show help instead of falling through to jj
2. **Update help text** — Replace `tandem cat` with `tandem file show` (or alias `cat → file show`)
3. **Add connection timeout** — 5–10s timeout for unreachable servers
4. **Auto-set `fsmonitor.backend = "none"`** in `tandem init` to avoid watchman conflicts
5. **Clean up server processes** in integration test teardown

### P2 — Nice to have

6. **Auto-alias `cat`** in workspace jj config for agents that expect `jj cat`
7. **Print workspace name** in `tandem log` header for orientation

---

## Conclusion

**Tandem v1 is ready for agent use.** The core workflow — init workspace, write files, commit, read other agents' files, manage bookmarks — works end-to-end with clear help text and good error messages. Every P0 blocker from v0 is resolved.

The remaining issues (init without server flag, stale `cat` reference in help) are minor UX papercuts that can be fixed in a single slice. An agent encountering tandem for the first time can discover commands via `--help`, set up a workspace, and collaborate with other agents without reading any documentation.

**Overall score: GREEN** — ready for multi-agent deployment.
