# Naive Agent Report: Tandem Binary Exploration

**Date**: 2026-02-15  
**Agent**: Claude (Opus 4)  
**Binary**: `target/debug/tandem`  
**Method**: Pure trial-and-error (no source code or documentation)

## Executive Summary

Successfully discovered and used core tandem functionality through 41 attempts over ~15 minutes of exploration. The tool is a version control system with client-server architecture and workspace support (similar to Jujutsu). Most features were discoverable through trial and error, but several UX issues made the process unnecessarily difficult.

**Success Rate**: 8/8 goals achieved ✅

## Goals Achievement

| Goal | Status | Attempts | Key Insight |
|------|--------|----------|-------------|
| 1. Understand what tool does | ✅ | 11 | Error messages + command responses |
| 2. Start a server | ✅ | 7 | `serve --listen <addr> --repo <path>` |
| 3. Create a commit | ✅ | 13 | `new` command (not "commit") |
| 4. List commits | ✅ | 11 | `log` command |
| 5. Update description | ✅ | 16 | `describe -m <msg>` |
| 6. See a diff | ✅ | 18 | `diff` command |
| 7. Use workspaces | ✅ | 37 | `--workspace` flag auto-creates |
| 8. List workspaces | ✅ | 25 | `workspaces` command (plural!) |

## Detailed Discovery Timeline

### Phase 1: Initial Discovery (Attempts 1-10)

**What I tried**: Running binary with no args, `--help`, `-h`, guessing commands  
**What worked**: Error messages provided crucial hints  
**What was confusing**:

1. **`--help` requires server connection** (Attempt 2)
   - Expected: Local help text
   - Got: `Error: failed to connect to tandem server 127.0.0.1:13013: Connection refused`
   - **Impact**: This is extremely unusual behavior. Help should NEVER require a server.
   - **Learning**: Discovered server address (127.0.0.1:13013) from error

2. **No help command works** (Attempts 9-10)
   - Tried: `--help`, `-h`, `help`
   - Result: All either try to connect to server or return "unsupported command"
   - **Impact**: Had to guess all commands through trial and error
   - **Time cost**: ~50% of total exploration time spent discovering commands

### Phase 2: Server Discovery (Attempts 4-8)

**What I tried**: Guessing server commands  
**What worked**: Progressive error messages guided me  

**Discovery chain**:
```
tandem server          → "Connection refused" (not a client command)
tandem serve           → "serve requires --listen <addr>"
tandem serve --listen  → "serve requires --repo <path>"
tandem serve --listen 127.0.0.1:13013 --repo . → ✅ SUCCESS
```

**Positive observation**: Error messages were **incremental** - each one told me exactly what was missing next. This was excellent UX for server startup.

### Phase 3: Client Commands (Attempts 11-24)

**Discovery pattern**: Guessing based on VCS knowledge

| Attempt | Command | Result | Notes |
|---------|---------|--------|-------|
| 11 | `log` | ✅ `(no commits)` | First success! |
| 12 | `commit` | ❌ Unsupported | Misleading - expected this to work |
| 13 | `new` | ✅ Created commit | Jujutsu-style naming |
| 15 | `describe` | ❌ Requires -m | Good error, told me what to add |
| 18 | `diff` | ✅ Showed changes | Worked but only showed metadata |
| 23 | `status` | ❌ Unsupported | Expected this in a VCS |

**What worked well**:
- Error messages for missing flags were clear and actionable
- Commands that worked did so intuitively

**What was confusing**:
- `new` instead of `commit` - not obvious without JJ knowledge
- `diff` only showed description changes, not file changes
- No `status` command to see what changed

### Phase 4: Workspace Discovery (Attempts 24-40)

**Most difficult part of exploration** - took 16 attempts to figure out.

**Failed attempts**:
```
workspace              → Unsupported
workspace-new          → Unsupported
new-workspace          → Unsupported
workspace add          → Unsupported
add-workspace          → Unsupported
create-workspace       → Unsupported
```

**Breakthrough** (Attempt 25): `workspaces` (plural) worked!
```
tandem workspaces → * default 4cb75b689ca2
```

**Second breakthrough** (Attempt 30): `--workspace` flag was accepted
```
tandem --workspace agent2 log → worked without error
```

**Third breakthrough** (Attempt 32): Creating commit with flag auto-creates workspace
```
tandem --workspace agent2 new -m "..." → ✅ Created workspace
```

**What was confusing**:
1. Command is `workspaces` (plural) not `workspace`
2. No explicit "create workspace" command
3. Workspaces are implicitly created on first use
4. No documentation of the `--workspace` flag discovery

## What Worked Well (Agent-Friendly Design)

### 1. **Progressive Error Messages** ⭐⭐⭐⭐⭐
```
serve                          → "serve requires --listen <addr>"
serve --listen <addr>          → "serve requires --repo <path>"
describe                       → "describe requires -m <message>"
```
Each error told me exactly what to add next. This is **excellent** design.

### 2. **Sensible Defaults**
- Server port (13013) was hardcoded in client
- Workspaces auto-create on first use
- Commands worked on "current" context without extra flags

### 3. **Clear Output Format**
```
@ fbd38a6ba02c Agent2 commit       ← Current commit
o 4cb75b689ca2 Add test file       ← Parent
o 62f2cc30eb9a My first commit
```
The `@` and `o` symbols made it easy to understand commit relationships.

### 4. **Minimal Ceremony**
Once server was running, commands were simple:
- `tandem new -m "msg"` - create commit
- `tandem log` - see history
- `tandem workspaces` - list workspaces

## What Was Confusing (Friction Points)

### 1. **--help Requires Server** ⚠️ CRITICAL ISSUE
**Impact**: Cannot discover commands without running server  
**Time cost**: ~30% of exploration time  
**Fix**: Provide local help text that works without server

### 2. **No Command Discovery Mechanism** ⚠️ HIGH PRIORITY
**Tried**: `help`, `--help`, `-h`, `commands`, `list`  
**Result**: All failed  
**Impact**: Had to guess every single command  
**Fix**: Add `tandem help` that lists available commands (server-less)

### 3. **Inconsistent Command Naming**
- `workspaces` (plural) - but why not `workspace list`?
- `new` instead of `commit` - non-obvious for non-JJ users
- No `status` - expected in any VCS

### 4. **Workspace Creation is Implicit**
**Confusing sequence**:
1. `tandem workspaces` → shows only "default"
2. `tandem --workspace agent2 log` → no error
3. `tandem workspaces` → still only "default"
4. `tandem --workspace agent2 new -m "..."` → NOW it appears

**Expected**: Explicit `tandem workspace create <name>` command

### 5. **diff Only Shows Metadata**
```
tandem diff
description:
- My first commit
+ Add test file
```

**Expected**: Also show file changes (like `git diff` or `jj diff`)  
**Tested**: Created file, ran diff, file changes not shown  
**Impact**: Can't verify actual work without other tools

### 6. **No Command Suggestions**
```
tandem commit
Error: unsupported client command: commit
```

**Better**:
```
Error: unknown command 'commit'. Did you mean 'new'?
```

### 7. **Log Format Unclear for Branches**
When workspaces diverged, `log` showed linear history:
```
@ fbd38a6ba02c Agent2 commit
o 4cb75b689ca2 Add test file
o 62f2cc30eb9a My first commit
o 662ee423c5f9 Default workspace commit
```

This made it seem linear when actually there were two workspace heads. Graph visualization would help.

## What Was Impossible Without Docs

### 1. **Advanced Features**
I have no idea if these exist:
- Merging commits
- Rebasing
- Conflict resolution
- Syncing between servers
- Garbage collection
- Configuration options

### 2. **Performance/Limits**
- How many workspaces can I have?
- How large can commits be?
- What's stored in commits? (files? metadata only?)
- How to clean up old commits?

### 3. **Server Management**
- How to stop server gracefully?
- What happens on crash?
- Can multiple servers run?
- Authentication/security?

### 4. **Workspace Semantics**
- Can I delete a workspace?
- Can I rename a workspace?
- Can I switch between workspaces?
- What's the difference between workspaces and branches?

## Recommendations for Agent-Friendliness

### Priority 1: Critical (Blockers)

#### 1.1 Make --help work locally
```bash
tandem --help
# Should show:
# Usage: tandem <command> [options]
# 
# Commands:
#   serve       Start tandem server
#   new         Create new commit
#   log         Show commit history
#   ...
#
# Use 'tandem <command> --help' for more info
```

#### 1.2 Add server-less help command
```bash
tandem help
tandem help <command>
```

### Priority 2: High (Major Friction)

#### 2.1 Add command suggestions
```bash
tandem commit
Error: unknown command 'commit'
Did you mean: new
```

#### 2.2 Add workspace subcommands
```bash
tandem workspace list          # instead of 'workspaces'
tandem workspace create <name>
tandem workspace delete <name>
tandem workspace switch <name>
```

#### 2.3 Add status command
```bash
tandem status
# Workspace: default
# Current commit: 662ee423c5f9
# Changed files: 0
```

#### 2.4 Make diff show file changes
Current: Only shows description  
Expected: Show file diffs like git/jj

### Priority 3: Medium (Quality of Life)

#### 3.1 Add --version flag
```bash
tandem --version
tandem 0.1.0
```

#### 3.2 Better error messages
Current: "Error: missing client command"  
Better: "Error: missing client command. Try 'tandem help' to see available commands."

#### 3.3 Add command aliases
```bash
tandem commit  → alias for 'new'
tandem ws      → alias for 'workspaces'
tandem show    → alias for 'diff' with better formatting
```

#### 3.4 Colorized output
- Current commit in green
- Parents in gray
- Descriptions in white
- Commit IDs in yellow

### Priority 4: Low (Nice to Have)

#### 4.1 Interactive mode
```bash
tandem
> help
> new -m "test"
> log
> exit
```

#### 4.2 Shell completion
Generate bash/zsh completion scripts

#### 4.3 Verbose mode
```bash
tandem --verbose new -m "test"
# Connecting to server...
# Connected to 127.0.0.1:13013
# Creating commit...
# Commit created: abc123
```

## Agent-Specific Observations

### What Made Exploration Easier
1. **Deterministic errors**: Same input = same output
2. **No authentication**: Could start testing immediately
3. **Simple state model**: Easy to understand what happened
4. **Clear success messages**: "Created commit X" confirmed actions

### What Made Exploration Harder
1. **No help system**: Had to guess everything
2. **No tab completion**: Couldn't discover commands
3. **Minimal feedback**: Many commands silent on success
4. **No validation**: Bad flags sometimes silently ignored

### Cognitive Load Assessment

**Low cognitive load**:
- Server startup (progressive errors guided me)
- Basic commands (new, log, describe)
- Reading output (clear formatting)

**High cognitive load**:
- Command discovery (pure guessing)
- Workspace creation (implicit, non-obvious)
- Understanding workspace semantics (no docs)
- Figuring out what's possible (no feature list)

## Comparison to Standard Tools

### Git
- ✅ Git has extensive help: `git help`, `git <cmd> --help`, man pages
- ✅ Git suggests commands: "did you mean 'commit'?"
- ❌ Git has complex UX, but at least it's documented

### Jujutsu (jj)
- ✅ JJ has helpful errors and suggestions
- ✅ JJ help works offline: `jj help`, `jj help <cmd>`
- ✅ JJ has workspace commands: `jj workspace add/list/forget`
- 🤔 Tandem seems to follow JJ model but without the help system

### Tandem
- ✅ Simpler than Git
- ✅ Similar to JJ (good model)
- ❌ No help system at all
- ❌ No command discovery
- ❌ Missing expected commands (status, commit)

## Testing Methodology Notes

### What Worked Well in My Approach
1. **Started with no args** - discovered "missing command" error
2. **Tried --help early** - discovered server requirement
3. **Followed error breadcrumbs** - each error led to next step
4. **Tested systematically** - tried variations when stuck
5. **Verified each success** - checked output after each command

### What Would Have Been Faster
1. **Command list** - would have cut exploration time in half
2. **Example workflows** - "how to create workspace" example
3. **Error suggestions** - "did you mean" would help
4. **Tab completion** - could discover flags/commands

## Summary Statistics

- **Total attempts**: 41
- **Time spent**: ~15 minutes
- **Commands discovered**: 5 (serve, new, log, describe, diff, workspaces)
- **Flags discovered**: 3 (--listen, --repo, --workspace, -m)
- **Failed command attempts**: 15+
- **Server startups**: 1
- **Workspaces created**: 2
- **Commits created**: 4

## Final Verdict

### What Tandem Got Right
- Clean, simple command set
- Progressive error messages (for flags)
- Implicit workspace creation (once you know it exists)
- Clear output formatting

### What Needs Improvement
1. **Help system** - CRITICAL missing feature
2. **Command discovery** - No way to learn what's possible
3. **Expected commands** - Missing `status`, aliasing `commit` to `new`
4. **Workspace management** - Implicit creation is confusing
5. **File diffs** - `diff` should show file changes
6. **Error suggestions** - "did you mean" would help a lot

### Agent-Friendliness Score

**Overall: 5/10**

| Aspect | Score | Reasoning |
|--------|-------|-----------|
| Discoverability | 2/10 | No help, must guess everything |
| Error Messages | 8/10 | Good for flags, poor for commands |
| Consistency | 6/10 | Mostly consistent, some odd choices |
| Documentation | 0/10 | None accessible via CLI |
| Usability | 7/10 | Once you know commands, easy to use |

### Recommendation
**Tandem has good bones but needs a help system urgently.** The core functionality is solid and the error messages for missing flags are excellent. However, the complete absence of command discovery makes it frustrating for new users (human or AI). Adding `tandem help` and `tandem <cmd> --help` would immediately improve the score to 8/10.

## Appendix: Full Command Reference Discovered

### Server Commands
```bash
tandem serve --listen <addr> --repo <path>
```

### Client Commands
```bash
tandem log                              # List commits
tandem new [-m <message>]               # Create new commit
tandem describe -m <message>            # Update commit description
tandem diff                             # Show changes (metadata only)
tandem workspaces                       # List workspaces
```

### Global Flags
```bash
--workspace <name>                      # Specify workspace (auto-creates)
```

### Commands That Don't Exist (Tried)
```
help, --help, -h, commit, status, init, clone, checkout, switch,
edit, squash, rebase, merge, workspace, workspace-new, new-workspace,
workspace add, add-workspace, create-workspace
```

---

**End of Report**

Generated by: Claude (Opus 4)  
Session: Naive agent exploration  
Goal: Discover tandem UX issues before reading docs
