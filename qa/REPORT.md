# Tandem QA Report — Agent Usability Evaluation

**Date:** 2026-02-15
**Method:** Three independent AI agents evaluated tandem from different angles:
- **Naive agent** (sonnet) — zero docs, trial and error only
- **Workflow agent** (sonnet) — realistic multi-agent workflow with docs
- **Stress agent** (haiku) — concurrent write correctness under load

Source reports: `naive-agent-report.md`, `workflow-eval-report.md`, `stress-report.md`

---

## Executive Summary

Tandem's core mechanism works: multiple agents can create commits concurrently via Cap'n Proto RPC, CAS-based head coordination prevents lost writes, data persists across server restarts, and agents see each other's work in real time via watchHeads. **The protocol and transport are solid.**

However, **agents struggle to use tandem effectively** because:
1. There is no `--help` — agents cannot discover commands without reading source code
2. Error messages for unknown commands don't suggest alternatives
3. Commits store only descriptions (no file trees) — agents can't review code
4. No bookmark management — git push requires manual server-side intervention
5. The workspace model is implicit and undiscoverable

**Verdict: Tandem is a working commit coordination layer. It is not yet a tool agents can self-serve with.**

---

## Functional Correctness

| Area | Status | Evidence |
|------|--------|----------|
| Single-agent round-trip | ✅ GREEN | 15/15 integration tests pass |
| Cross-workspace visibility | ✅ GREEN | Agent A sees Agent B's commits immediately |
| Concurrent CAS convergence | ✅ GREEN | 5 agents × 3 commits = 15/15 preserved |
| Persistence across restart | ✅ GREEN | 50 commits survived kill + restart |
| WatchHeads notifications | ✅ GREEN | <1s latency, reconnect works |
| Git push from server repo | ✅ GREEN | jj git push works after manual bookmark |
| Git fetch into server repo | ✅ GREEN | External commits visible in jj log |
| High concurrency (20+ agents) | ⚠️ YELLOW | Server drops connections at 20+ simultaneous agents |

**Throughput:** ~4.5 commits/sec steady state, independent of agent count (5-10 range).

---

## Agent Usability Assessment

### 🔴 RED — Discovery (can agents figure out commands?)

The naive agent spent **50% of its exploration time** (20+ of 41 attempts) guessing commands. Key findings:

- `tandem --help` tries to connect to server instead of showing usage
- `tandem help` returns "unsupported client command: help"
- No command listing, no usage text, no man page
- Agent had to guess `new` (not `commit`), `workspaces` (not `workspace`)
- `--workspace` flag is completely undiscoverable

**Evidence:** "Had to guess every single command... --help requires server connection — this is extremely unusual behavior." — naive agent report

### 🟡 YELLOW — Error Messages (can agents self-correct?)

Split verdict:

**Good (flag errors):** Progressive error messages for missing flags are excellent:
```
serve                    → "serve requires --listen <addr>"
serve --listen <addr>    → "serve requires --repo <path>"
describe                 → "describe requires -m <message>"
```
Each error tells the agent exactly what to add next.

**Bad (command errors):** Unknown commands give no guidance:
```
tandem commit → "unsupported client command: commit"
```
No "did you mean `new`?" suggestion. Agent must guess.

### 🟡 YELLOW — Workflow (can agents complete a feature?)

Agents can create commits, see each other's work, and describe commits. The basic collaboration loop works. But:

- Agents **cannot inspect commit contents** (no `show` command)
- Agents **cannot see file diffs** (`diff` only shows description changes)
- Agents **cannot review each other's code** — only descriptions
- Agents **cannot push to git** without manual server intervention

**Evidence:** "Agent B can see that Agent A created commit 7b04a8e with description 'Add auth layer', but cannot see which files changed, read the file content, or verify the changes match the description. → BLOCKED" — workflow report

### ✅ GREEN — Concurrency (does multi-agent work intuitively?)

Once agents know the `--workspace` flag, concurrent work just works:
- CAS retries are transparent to the agent
- No lost writes in any test scenario
- Workspace heads tracked correctly
- 5-10 concurrent agents operate without issues

### 🔴 RED — Information Completeness (does agent get what it needs?)

Tandem commits store **only description metadata**, not file trees. This means:

| Agent needs to... | Can they? | Why not |
|-------------------|-----------|---------|
| Create a commit with a message | ✅ Yes | |
| See commit history | ✅ Yes | |
| See which files changed | ❌ No | No file tree in commit objects |
| Read file content | ❌ No | No `cat` or `show` command |
| Review another agent's code | ❌ No | Only descriptions visible |
| Push to GitHub | ❌ No | No bookmark management |
| Check repo status | ❌ No | No `status` command |

---

## Where Agents Get Stuck

### Stuck Point 1: "How do I use this tool?"
**When:** Agent first encounters tandem binary
**What happens:** `--help` fails, `help` fails, no usage text
**Time lost:** 10-15 minutes of guessing (naive agent: 41 attempts)
**Fix:** Add `--help` that works without server connection (~20 lines)

### Stuck Point 2: "What command creates a commit?"
**When:** Agent wants to record work
**What happens:** Tries `commit`, `save`, `record` — all fail. No suggestion.
**Time lost:** 3-5 attempts
**Fix:** Error message should suggest `new`. Or alias `commit` → `new`.

### Stuck Point 3: "How do I create a workspace?"
**When:** Agent needs to work in isolation
**What happens:** Tries `workspace create`, `workspace add`, `new-workspace` — all fail
**Time lost:** 10+ attempts (naive agent: 16 attempts)
**Fix:** Document that `--workspace <name>` auto-creates on first write. Or add explicit `workspace create`.

### Stuck Point 4: "What did the other agent actually change?"
**When:** Agent A wants to review Agent B's commit
**What happens:** Can see description "Fix auth bug" but nothing else. No files, no diff, no content.
**Impact:** **Blocks all code review workflows**
**Fix:** Either add file tree storage + read commands, or document that tandem is metadata-only.

### Stuck Point 5: "How do I push to GitHub?"
**When:** Agents finished collaborating, need to ship
**What happens:** No `bookmark` command. Must SSH to server, manually create bookmark, run jj git push.
**Impact:** **Blocks shipping workflow**
**Fix:** Add `tandem bookmark create <name>` or auto-create bookmarks on commit.

---

## What Information Agents Need

### Information tandem provides:
- ✅ Commit descriptions and short IDs
- ✅ Parent-child relationships (via `log`)
- ✅ Current workspace head
- ✅ All workspace heads (via `workspaces`)
- ✅ Real-time head change notifications (via `watch`)

### Information tandem does NOT provide but agents need:

| Information | Importance | Recommendation |
|-------------|------------|----------------|
| Available commands and flags | P0 | Add `--help` |
| File tree contents | P0 | Add `files`, `cat` commands |
| File-level diffs | P0 | Enhance `diff` beyond descriptions |
| Bookmark/branch state | P0 | Add `bookmark` commands |
| Commit metadata (author, timestamp) | P1 | Add `show` command |
| Operation history (who did what) | P1 | Add `op log` command |
| Working copy status | P1 | Add `status` command |
| Server connection state | P2 | Add verbose/debug mode |

---

## Recommendations

### P0 — Blockers (agents cannot self-serve without these)

**1. Add `--help` that works without server** (~20 lines)
Every agent's first instinct is `tool --help`. This must work locally.
```
tandem --help
Usage: tandem [--server <addr>] [--workspace <name>] <command>

Commands:
  serve      Start tandem server
  new        Create new commit
  describe   Update commit description
  log        Show commit history
  diff       Show changes
  workspaces List workspaces
  watch      Watch for head changes

Server: tandem serve --listen <addr> --repo <path>
```

**2. Add command suggestions on unknown command** (~10 lines)
```
tandem commit → Error: unknown command 'commit'. Did you mean 'new'?
```

**3. Add `TANDEM_SERVER` env var** (~5 lines)
Agents shouldn't need `--server` on every call. Check env var as fallback.

### P1 — Significant Friction (agents can work around but shouldn't have to)

**4. Add `tandem bookmark create/list`** (~100 lines)
Unblocks git push workflow. Wraps `jj bookmark` via RPC.

**5. Add `tandem show <commit>`** (~50 lines)
Display full commit metadata: parent, description, timestamp.

**6. Document the mental model** (~1 page)
Agents need to know: "tandem stores commit descriptions, not file trees. Use it for coordinating who's working on what, not for code review."

### P2 — Nice to Have

**7. Auto-create bookmark on `new --bookmark <name>`**
**8. Add `tandem status` showing workspace state**
**9. Alias `commit` → `new` for git-native agents**
**10. Color output (green for current commit, etc.)**

---

## Raw Test Results

### Integration Tests (cargo test): 15/15 ✅
| Suite | Tests | Status |
|-------|-------|--------|
| slice1: single-agent round-trip | 2 | ✅ |
| slice2: two-agent visibility | 1 | ✅ |
| slice3: concurrent convergence | 2 | ✅ |
| slice5: watchHeads | 4 | ✅ |
| slice6: git round-trip | 3 | ✅ |
| slice7: end-to-end multi-agent | 3 | ✅ |

### Naive Agent Exploration: 8/8 goals achieved ✅
All goals achieved through 41 attempts. Agent-friendliness score: **5/10**.

### Workflow Evaluation: Core works, critical gaps identified
- Concurrent collaboration: ✅
- Cross-visibility: ✅
- Watch notifications: ✅
- Code review workflow: ❌ (no file content)
- Git push workflow: ❌ (no bookmarks)

### Stress Test: Production-ready for 5-10 agents ✅
| Scenario | Expected | Result | Status |
|----------|----------|--------|--------|
| 5 agents × 3 commits | 15 | 15 | ✅ |
| + 10 single agents | 10 | 10 | ✅ |
| 10 agents × 5 commits | 50 | 50 | ✅ |
| + 20 single agents | 20 | 9 | ⚠️ |
| Persistence (restart) | 100% | 100% | ✅ |
| Data loss | 0 | 0 | ✅ |

---

## Conclusion

**Tandem's transport and coordination work.** Cap'n Proto RPC, CAS heads, workspace isolation, and persistence are all correct.

**Tandem's agent UX does not.** The three highest-impact fixes are:
1. `--help` (5 minutes to implement, saves every agent 15 minutes)
2. Command suggestions on error (10 minutes, prevents guessing loops)
3. `TANDEM_SERVER` env var (5 minutes, eliminates `--server` on every call)

These three changes would move the agent-friendliness score from **5/10 to 8/10** with minimal code.

The deeper question — whether tandem should store file trees or remain metadata-only — is an architecture decision that determines whether agents can do code review through tandem or need a separate channel.
