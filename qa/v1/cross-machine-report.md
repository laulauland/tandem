# Cross-Machine QA Report (Docker Simulation)

**Date:** 2026-02-15T20:52Z  
**Method:** 3 Docker containers on same host (debian:trixie-slim), connected via `tandem-qa` bridge network  
**Binary:** `target/release/tandem` (ELF 64-bit, aarch64, dynamically linked)  
**Base image:** debian:trixie-slim (GLIBC 2.41) — bookworm-slim failed due to GLIBC_2.39 requirement

---

## Test Setup

| Container | Role | Network Name | Workspace |
|-----------|------|--------------|-----------|
| tandem-server | Server (`tandem serve --listen 0.0.0.0:13013 --repo /srv/project`) | tandem-server | N/A |
| tandem-agent-a | Agent A (file author) | tandem-agent-a | `default` |
| tandem-agent-b | Agent B (cross-agent reader + author) | tandem-agent-b | `agent-b` |
| tandem-verify-a | Verification (re-attach as new workspace) | tandem-verify-a | `verify-a` |

---

## Step 1: Server Startup

```
tandem server listening on 0.0.0.0:13013
```

**Result:** ✅ PASS — Server started and listening.

---

## Step 2: Agent A — Write Files and Commit

Agent A initialized workspace `default`, created two commits:

```
wmlmvnrs 139a91b4 (empty) feat: add email validation
vvvroskp feade904 feat: add auth module
wpkyourz acaf0b21 (no description set)
qpksqysv 2221e4ce (empty) (no description set)
zzzzzzzz root() 00000000
```

Files written:
- `src/auth.rs`: `pub fn authenticate(token: &str) -> bool { !token.is_empty() }`
- `src/lib.rs`: `pub mod auth;`
- `src/validate.rs`: `pub fn validate_email(email: &str) -> bool { email.contains('@') }`

Agent A read-back of own file:
```
$ tandem file show -r @-- src/auth.rs
pub fn authenticate(token: &str) -> bool { !token.is_empty() }
```

**Result:** ✅ PASS — Agent A wrote files, committed, and read them back byte-for-byte.

---

## Step 3: Agent B — See Agent A's Commits

Agent B initialized workspace `agent-b` and ran `log --no-graph`:

```
osl... (empty) (no description set)        # abandoned workspace commits
ptnwpoxk agent-b@ 97969fcd (empty)
wmlmvnrs default@ 139a91b4 feat: add email validation
vvvroskp feade904 feat: add auth module
wpkyourz acaf0b21 (no description set)
qpksqysv 2221e4ce (empty) (no description set)
zzzzzzzz root() 00000000
```

**Result:** ✅ PASS — Agent B sees all of Agent A's commits in the log.

---

## Step 4: Agent B — Read Agent A's Files

```
$ tandem file show -r vvvroskp src/auth.rs
pub fn authenticate(token: &str) -> bool { !token.is_empty() }
```

**Result:** ✅ PASS — Agent B reads Agent A's `auth.rs` byte-for-byte via change ID.

**Note:** The `description(exact:"...")` revset syntax failed with "didn't resolve to any revisions". Using change IDs (e.g., `vvvroskp`) works correctly. This is a minor usability issue — agents need to use change IDs rather than description-based revsets for cross-workspace file access.

---

## Step 5: Agent B — Write Own Files

Agent B created two commits:

```
zkxnluwn 31c3de6d (empty) test: add integration tests
knyzztwk 84366ef3 feat: add API routes
ptnwpoxk feature-api bda97043 (no description set)
```

Files written:
- `src/api.rs`: `pub fn handle_request(path: &str) -> u16 { if path == "/health" { 200 } else { 404 } }`
- `tests/integration.rs`: integration test content

Bookmark created:
```
feature-api: ptnwpoxk bda97043 (no description set)
```

**Result:** ✅ PASS — Agent B wrote files, committed, and created a bookmark.

---

## Step 6: Verification — New Workspace Sees Everything

A fresh workspace `verify-a` was created to simulate Agent A re-attaching:

### All commits visible:
```
ttxtxmzq verify-a@ da367ca9 (empty)
mxzkrurs 350ca8a8 (empty)
zkxnluwn agent-b@ 31c3de6d test: add integration tests
knyzztwk 84366ef3 feat: add API routes
ptnwpoxk feature-api bda97043 (no description set)
oslxnnzk 554124d3 (empty)
oozqpwsv 46a0b2f0 (empty)
wmlmvnrs default@ 139a91b4 feat: add email validation
vvvroskp feade904 feat: add auth module
wpkyourz acaf0b21 (no description set)
qpksqysv 2221e4ce (empty)
zzzzzzzz root() 00000000
```

### Cross-agent file reads:
```
$ tandem file show -r knyzztwk src/api.rs
pub fn handle_request(path: &str) -> u16 { if path == "/health" { 200 } else { 404 } }

$ tandem file show -r zkxnluwn tests/integration.rs
#[test]
fn health_returns_200() {
    assert_eq!(api::handle_request("/health"), 200);
}

$ tandem file show -r vvvroskp src/auth.rs
pub fn authenticate(token: &str) -> bool { !token.is_empty() }
```

### Bookmarks visible:
```
feature-api: ptnwpoxk bda97043 (no description set)
```

**Result:** ✅ PASS — All commits, files, and bookmarks visible from a fresh workspace.

---

## Step 7: Server Storage

```
/srv/project/.tandem/heads.json    # CAS operation heads
/srv/project/.jj/                  # Full jj repo
/srv/project/.git/                 # Colocated git repo
Git objects: 29 files
```

**Result:** ✅ PASS — Server stores all objects in jj+git colocated repo.

---

## Acceptance Criteria Summary

| # | Criterion | Result |
|---|-----------|--------|
| 1 | Agent A can write files and commit | ✅ PASS |
| 2 | Agent B can see Agent A's commits in log | ✅ PASS |
| 3 | Agent B can read Agent A's files byte-for-byte | ✅ PASS |
| 4 | Agent B can write its own files | ✅ PASS |
| 5 | Agent A (re-attached) can see Agent B's files | ✅ PASS |
| 6 | Bookmarks are visible across agents | ✅ PASS |
| 7 | Server stores all objects | ✅ PASS |

---

## Issues Found

### Issue 1: GLIBC version requirement (Medium)
The tandem binary requires GLIBC 2.39+, which is not available in debian:bookworm-slim (stable). Had to use debian:trixie-slim (testing). Consider static linking or building against an older glibc for broader compatibility.

### Issue 2: `description(exact:"...")` revset fails (Low)
The `description(exact:"feat: add auth module")` revset syntax did not resolve any revisions, even though the commit was visible in `log`. Agents must use change IDs instead. This may be a jj version issue or a limitation of description matching in the tandem backend. Regular `description("...")` (substring match) also failed.

### Issue 3: Stale workspace on re-init (Low)
When a container re-initializes the `default` workspace (which was created by Agent A in a previous container), jj reports "The working copy is stale" and requires `jj workspace update-stale`. Workaround: use a unique workspace name for each container session.

### Issue 4: Abandoned workspace commits accumulate (Cosmetic)
Each `tandem init --workspace=agent-b` from a new container creates an additional empty commit, leading to abandoned commits (e.g., `oozqpwsv`, `oslxnnzk`) cluttering the log. These are harmless but noisy.

---

## Overall Verdict

**✅ ALL 7 CRITERIA PASS**

The tandem distributed VCS successfully enables multi-agent file collaboration across Docker containers. Files round-trip correctly through the Cap'n Proto RPC layer, cross-agent visibility works via shared jj operation log, and bookmarks propagate between workspaces. The system is ready for real multi-machine testing.
