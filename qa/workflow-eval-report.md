# Tandem Multi-Agent Workflow Evaluation

**Date:** 2026-02-15  
**Evaluator:** AI Agent (Claude)  
**Version:** tandem v0.1.0 (commit as of evaluation)

## Executive Summary

Tandem successfully enables basic multi-agent collaboration through a network-accessible jj backend. The core infrastructure works: concurrent commits converge correctly, agents can see each other's workspaces, and watch notifications deliver real-time updates. However, **agents lack critical information and commands needed for real-world workflows**, particularly around commit inspection, file operations, and git interop.

**Priority Recommendation:** Focus on agent introspection commands (show, files, status) and bookmark management for git round-trip before expanding to advanced features.

---

## 1. What Works Well

### ✅ Core Multi-Agent Coordination
- **Concurrent commits converge correctly**: Two agents can create commits simultaneously without data loss. CAS retry logic (up to 64 attempts with backoff) handles contention gracefully.
- **Workspace isolation**: Each agent has a distinct workspace ID. The `workspaces` command clearly shows which workspace belongs to which agent.
- **Cross-agent visibility**: Agents immediately see commits from other agents via `log` command. The distributed op-log architecture works as designed.

Example from test:
```
$ tandem --workspace agent-a workspaces
* agent-a 7b04a8e48e93
  agent-b 1114de45cc45
```

### ✅ Watch Command (Real-time Updates)
The `watch` command successfully delivers head change notifications:

```
watch: connected (afterVersion=0)
v4 heads: [1ff489533efa]
v5 heads: [07f5825066b7]
```

Notifications arrive within ~1 second of commit creation. Reconnect logic exists but wasn't tested under network partitions.

### ✅ Server-Side Mirror
The server maintains a working jj repository that mirrors tandem commits:

```bash
# Server-side jj log matches tandem state
$ jj log
@  np laurynas.keturakis@gmail.com now 6b4213cf
│  Agent A: Concurrent commit 1
○  zo laurynas.keturakis@gmail.com 1 second ago 73f0a962
│  Agent B: Concurrent commit 2
```

This proves the `.tandem` storage is correctly synchronized with jj's op-store.

### ✅ Error Handling
Clear error messages for common failures:
- Connection refused: `Error: failed to connect to tandem server 127.0.0.1:9999`
- Unknown commands: `Error: unsupported client command: invalid-command`

---

## 2. What Information Agents Need But Don't Get

### 🔴 Critical Gaps

#### 2.1 No Commit Inspection
Agents cannot examine commit details beyond the description.

**Missing:**
- `tandem show <commit-id>`: View full commit metadata (parent, timestamp, author)
- `tandem diff <commit-id>`: Show changes in a commit (currently `diff` only shows description change)
- Commit hash resolution: No way to reference commits by short prefix

**Real-world impact:**  
An agent cannot answer "what changed in commit abc123?" or "who created this commit?". This breaks review workflows.

**Test result:**
```bash
$ tandem show 795e91462bfb
Error: unsupported client command: show
```

#### 2.2 No File Operations
Agents operate blind to actual file content. Tandem stores commit objects but has no commands for file trees.

**Missing:**
- `tandem files [<commit>]`: List files in working copy or commit
- `tandem cat <file> [--revision <commit>]`: Read file content
- `tandem diff <file>`: Show file-level diffs
- Working copy status: No equivalent to `jj status`

**Real-world impact:**  
Agents can create commits with descriptions like "Fix bug in auth.rs" but cannot verify the fix, read the current state, or even confirm auth.rs exists.

**Test result:**
```bash
$ tandem files
Error: unsupported client command: files
```

**Recommendation:**  
Add minimal read-only file operations first:
1. `tandem files` (list)
2. `tandem cat <path>` (read)
3. `tandem diff <path>` (compare working copy vs parent)

#### 2.3 No Introspection or Help
No way for agents to discover available commands or their parameters.

**Missing:**
- `--help` flag: No usage information
- `tandem help`: Command listing
- `--version`: Can't verify tandem version

**Real-world impact:**  
An agent exploring tandem for the first time has to guess commands. LLMs default to `--help` as their primary discovery mechanism.

**Test result:**
```bash
$ tandem --help
Error: failed to connect to tandem server 127.0.0.1:13013: Connection refused (os error 61)
# (tries to connect as if "help" were a command)
```

---

### 🟡 Moderate Gaps

#### 2.4 Limited Workspace Context
`workspaces` command shows workspace → commit mapping but lacks key details:

**What's shown:**
```
* agent-a 795e91462bfb
  bob d659dd77f41d
```

**What's missing:**
- Commit description (agents must run `log` and match IDs manually)
- Timestamp (when was this workspace last updated?)
- Parent relationships (is this workspace ahead/behind/diverged from others?)

**Recommendation:**  
Enhance `workspaces` output:
```
* agent-a 795e91462bfb "Feature X: Add validation" 2s ago
  bob     d659dd77f41d "Feature Y" 5s ago
  charlie 1ff489533efa "Feature Z" 3s ago
```

#### 2.5 No Operation History
Agents cannot see *who* made a commit or *what* operation created it.

The server stores operations in `.tandem/operations/` with metadata like:
```json
{
  "type": "new",
  "workspaceId": "agent-a",
  "newCommitId": "7b04a8e48e93...",
  "parentHeads": [...]
}
```

But agents have no command to query this.

**Recommendation:**  
Add `tandem op log` to show operation history with workspace attribution.

---

## 3. Where Agents Would Get Stuck

### Scenario 1: Code Review Workflow
**Goal:** Agent B reviews Agent A's changes.

**Blocker:** Agent B can see that Agent A created commit `7b04a8e48e93` with description "Add auth layer", but cannot:
1. See which files changed
2. Read the file content
3. Verify the changes match the description

**Workaround:** None within tandem. Agent B must access the server filesystem directly or rely on out-of-band communication.

---

### Scenario 2: Debugging a Bug
**Goal:** Agent A needs to find when a bug was introduced.

**Blocker:**
1. No file content access → can't reproduce the bug
2. No commit diffs → can't bisect through history
3. No timestamps on log output → can't correlate with external events

**Workaround:** None.

---

### Scenario 3: Merging Concurrent Work
**Goal:** Agents A and B made conflicting changes to the same file and need to resolve it.

**Blocker:**
1. Tandem commits don't track file-level changes (only description metadata)
2. No merge command or conflict detection
3. No way to see divergence between workspace heads

**Current behavior:** Both commits exist as separate heads. Agents can `describe` to amend their own head but cannot merge.

**Recommendation:**  
Either:
- Document that tandem is metadata-only (commits = markers, not file snapshots), OR
- Implement file tree storage and expose merge operations

---

### Scenario 4: Shipping to Git/GitHub
**Goal:** Agents collaborate via tandem, then push to GitHub.

**Blocker:** **No bookmark (branch) management.**

**Test findings:**
```bash
$ jj bookmark list
(no output)

$ jj git push --branch main
Warning: No matching bookmarks for names: main
Nothing changed.
```

The server-side jj repo has all commits but no bookmarks. Git cannot push commits without refs.

**Root cause:** Tandem's `new` and `describe` commands don't create or update bookmarks.

**Workaround:** Manual server-side intervention:
```bash
# On server:
$ jj bookmark create main -r <commit-id>
$ jj git push --branch main
```

**Recommendation:**  
Add bookmark commands to tandem:
- `tandem bookmark create <name> [-r <commit>]`
- `tandem bookmark set <name> <commit>`
- `tandem bookmark list`

OR auto-create bookmarks: `tandem new -m "Fix bug" --bookmark feature-x`

---

## 4. Git Round-Trip Friction Points

### Issue 1: No Bookmark Management (Critical)
**Status:** ❌ Blocks git push/pull workflows  
**Detail:** Covered in Scenario 4 above.

### Issue 2: Commit IDs Diverge
**Status:** ⚠️ Confusing but not blocking

Tandem uses SHA-256 hashes for commit objects:
```
tandem: 7b04a8e48e93c86a2477b0900d04c40a876176d5235bbb696bd1b5e46e993f26
jj:     08e26cdab7bafaf487085bdb218bb11d497b6c1c
```

Git will generate its own SHA-1 hashes when commits are pushed.

**Impact:** Agents reference commits by tandem IDs, which don't match git IDs. Mapping is implicit through jj's backend.

**Recommendation:** Document this clearly. Consider exposing jj's change ID as a stable identifier.

### Issue 3: Server Must Run `jj git fetch/push`
**Status:** ✅ Works as designed but requires server access

Tandem clients cannot directly interact with git. The workflow is:

```
Agent A → tandem server → jj repo → git repo → GitHub
```

This requires:
1. Server admin to run `jj git push`, OR
2. Automation to watch for tandem commits and auto-push

**Recommendation:**  
Add a `tandem git push` command that triggers server-side `jj git push` via RPC, or document the manual workflow clearly in a "Shipping Code" guide.

### Issue 4: Git Colocated Repo Disables Git Commands
**Status:** ℹ️ Informational

The server repo has `.git` but jj disables direct git commands:
```bash
$ git log
Git commands are disabled. Use jj instead.
```

This is jj's intended behavior but may surprise users expecting to run `git status` on the server.

---

## 5. Specific Missing Features for Agent Usability

Prioritized by impact on realistic workflows:

### P0 - Critical (blocks core workflows)
1. ✅ **`tandem show <commit>`** - Inspect commit details
   - Output: parent, author, timestamp, description
   - Enables: review, debugging, attribution

2. ✅ **`tandem files [<commit>]`** - List files in tree
   - Output: file paths (no content)
   - Enables: understanding what changed

3. ✅ **`tandem cat <path> [--revision <commit>]`** - Read file content
   - Output: file content as bytes/text
   - Enables: code review, bug verification

4. ✅ **Bookmark management** - Create/update git branches
   - Commands: `bookmark create/set/list`
   - Enables: git push workflow

5. ✅ **`--help` and `tandem help`** - Command discovery
   - Output: available commands and usage
   - Enables: self-service learning for AI agents

### P1 - High (improves UX significantly)
6. ✅ **`tandem diff <path>`** - File-level diffs
   - Output: unified diff format
   - Enables: precise change review

7. ✅ **`tandem status`** - Working copy state
   - Output: modified/added/deleted files
   - Enables: pre-commit review

8. ✅ **Enhanced `workspaces` output** - Show descriptions/timestamps
   - Improves: context when switching between agents' work

9. ✅ **Commit hash prefix resolution** - Use short hashes
   - Example: `tandem show 7b04a8e` instead of full 64-char hash
   - Improves: command-line ergonomics

10. ✅ **`tandem op log`** - Operation history
    - Output: who did what when
    - Enables: audit trail, debugging sync issues

### P2 - Nice to have
11. ✅ **`tandem log --workspace <id>`** - Filter log by workspace
12. ✅ **`tandem watch --format json`** - Machine-readable notifications
13. ✅ **`tandem merge <commit>`** - Explicit merge operation
14. ✅ **`tandem git push`** - Trigger server-side git push via RPC
15. ✅ **`tandem gc`** - Garbage collect old operations/commits

---

## 6. Architecture Observations

### What's Good
- **Separation of storage and commands**: The `.tandem/` directory is clean and inspectable. Easy to debug.
- **Cap'n Proto RPC**: Low overhead, promise pipelining support exists (not yet utilized).
- **CAS-based head updates**: Correct distributed coordination primitive.
- **Watch notifications**: Fast delivery (~1s latency), reconnect logic in place.

### What's Questionable
- **Commit objects store only description, not file trees**: This makes tandem more of a "collaborative op-log" than a true distributed VCS. If this is intentional, document it clearly. If not, add tree/blob storage.

- **Server-side mirroring duplicates commits**: Every tandem commit is mirrored into the server's jj repo via `jj new/describe`. This is clever but adds complexity. Consider whether the `.tandem` store could BE the jj store (i.e., tandem directly implements jj's backend traits against `.jj/store` instead of a parallel `.tandem/` directory).

- **No authentication or workspace ownership**: Any client can write to any workspace. Fine for v0.1, but agents will need workspace ACLs for multi-team scenarios.

### What's Missing (Foundational)
- **File content storage**: Either commit objects need tree/blob pointers, or tandem needs a separate file store backend.
- **Workspace state persistence**: Where is the working copy? Currently, agents are stateless (no local files). Real agents need a working directory to edit files.

---

## 7. Recommendations (Prioritized by Impact)

### Immediate (before any production use)
1. **Add `show` command** (commit inspection)
   - Unblocks review workflows
   - Implementation: ~50 lines (read commit JSON, format output)

2. **Add `--help` flag**
   - Critical for agent discoverability
   - Implementation: ~20 lines (match on `--help`, print usage)

3. **Add bookmark commands** (create/set/list)
   - Unblocks git round-trip
   - Implementation: ~100 lines (wrap jj bookmark CLI or store bookmarks in `.tandem/bookmarks.json`)

4. **Document "Agents are stateless"** in README
   - Clarify that tandem doesn't manage working copies (yet)
   - Set expectations: agents can coordinate commits but not edit files

### Short-term (next 2-4 weeks)
5. **Add file operations** (files, cat, diff)
   - Required for real code review
   - Implementation: Either:
     - Option A: Store file trees in commit objects (big change)
     - Option B: Proxy to server-side `jj cat/diff` (quick hack)

6. **Add `status` command**
   - Shows what would be committed
   - Requires working copy state (see #7)

7. **Define working copy model**
   - Decision needed: Does tandem own the working directory, or delegate to `jj workspace`?
   - Current: Unclear. Agents run tandem from any directory.

8. **Add operation log query** (`op log`)
   - Enables debugging "who committed this?"
   - Implementation: Read `.tandem/operations/`, format output

### Medium-term (next 1-2 months)
9. **Git round-trip automation**
   - `tandem git push` triggers server-side `jj git push`
   - Auto-bookmark on `new` (e.g., `--bookmark` flag)

10. **Workspace ownership and ACLs**
    - Prevent agent-a from writing to agent-b's workspace
    - Requires authentication layer (out of scope for v0.1)

11. **Promise pipelining for batch operations**
    - Currently not used (see Slice 4 tests)
    - Benefit: Reduce RTT for multi-step workflows (e.g., `new` → `describe` → `bookmark create`)

12. **Handle conflicts explicitly**
    - Current: Concurrent commits create divergent heads
    - Needed: Merge strategy (manual or auto)

### Long-term (3+ months)
13. **Client-side caching** (marked as non-goal in ARCHITECTURE.md but will become necessary at scale)
14. **Multi-repo support** (one tandem server, multiple repos)
15. **WebSocket-based watch** (replace TCP RPC for better firewall traversal)

---

## 8. Conclusion

**Tandem successfully proves the core concept:** jj workspaces over the network enable multi-agent collaboration. The CAS-based op-head coordination is solid, and the server-side mirroring works.

**However, tandem is currently a "commit coordination layer" more than a "distributed VCS"**. Agents can create commits with descriptions but cannot interact with file content, inspect changes, or manage git integration without manual server access.

**To make tandem practical for real agents:**
- **Add introspection commands** (`show`, `help`, `status`)
- **Add bookmark management** (for git round-trip)
- **Decide on file storage model** (metadata-only vs full file trees)

**The path forward is clear:**  
Prioritize P0 features (#1-5 above). These are small, high-leverage changes that unlock 80% of agent workflows. Then tackle file operations (#6-7) once the working copy model is defined.

**Estimated effort to "agent-ready" state:** 2-3 weeks for a single developer implementing P0 + minimal file operations.

---

## Appendix: Test Artifacts

### A. Successful Concurrent Workflow
```bash
# Agent A and B create commits simultaneously
$ tandem --workspace agent-a new -m "Concurrent commit 1" &
$ tandem --workspace agent-b new -m "Concurrent commit 2" &
# Both succeed after CAS retries

$ tandem log
@ 0a1b3cd0460f Agent B: Concurrent commit 2
o 2a48d492d4ad Agent A: Concurrent commit 1
# Both commits preserved
```

### B. Watch Notifications (captured output)
```
watch: connected (afterVersion=0)
v4 heads: [1ff489533efa]
v5 heads: [07f5825066b7]
```
Latency: ~1 second from commit creation to notification delivery.

### C. Server State Inspection
```bash
$ ls -la server/.tandem/
total 8
drwxr-xr-x@ 6 laurynas-fp  wheel  192 Feb 15 16:28 .
drwxr-xr-x@ 5 laurynas-fp  wheel  160 Feb 15 16:28 ..
-rw-r--r--@ 1 laurynas-fp  wheel  376 Feb 15 16:28 heads.json
drwxr-xr-x@ 3 laurynas-fp  wheel   96 Feb 15 16:28 objects
drwxr-xr-x@ 6 laurynas-fp  wheel  192 Feb 15 16:28 operations
drwxr-xr-x@ 6 laurynas-fp  wheel  192 Feb 15 16:28 views

$ cat server/.tandem/heads.json | jq .
{
  "version": 5,
  "heads": ["07f5825066b7..."],
  "workspaceHeads": {
    "alice": "795e91462bfb...",
    "bob": "d659dd77f41d...",
    "charlie": "1ff489533efa...",
    "dave": "07f5825066b7..."
  }
}
```

### D. Git Round-Trip Attempt
```bash
$ jj bookmark list
(no output)

$ jj git push --branch main
Warning: No matching bookmarks for names: main
Nothing changed.
```
**Conclusion:** Git push requires bookmark creation, which tandem doesn't support yet.

---

**End of Evaluation Report**
