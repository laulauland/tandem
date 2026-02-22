# Workflow: Tandem as Server-Client jj

Tandem applies a server-client model to jj's store layer. The server hosts
a normal jj+git colocated repo. Agents on remote machines use the `tandem`
binary (which embeds jj-cli with tandem backend) to read and write objects
over Cap'n Proto RPC. All jj commands work transparently — the agent never
knows the store is remote.

## Roles

**Server (point of origin):**
- Runs on a VM/VPS as a persistent service
- Hosts the canonical jj+git repo
- Runs `tandem serve`
- Is where git operations happen (`jj git push`, `jj git fetch`, `gh pr create`)
- Operated by the orchestrator / teamlead / main agent

**Agents (remote clients):**
- Run `jj-tandem` (stock jj + tandem backend)
- Have local working copies (real files on disk)
- Read/write objects through RPC — files, trees, commits all stored on server
- Never touch git directly

## Concrete Workflow

### 1. Setup

```bash
# On the server (typically a VM/VPS)
mkdir /srv/project && cd /srv/project
jj git init
jj git remote add origin git@github.com:org/project.git
jj git fetch
tandem serve --listen 0.0.0.0:13013 --repo /srv/project
```

### 2. Agents work

```bash
# Agent A (any machine)
tandem init --server=server:13013 ~/work/project
# If --workspace is omitted, tandem auto-generates a unique workspace name.
cd ~/work/project
ls src/                                 # real files, fetched from server
echo 'pub fn auth() {}' > src/auth.rs
tandem new -m "feat: add auth"
# Objects (file bytes, tree, commit) stored on server via RPC
```

```bash
# Agent B (different machine)
tandem init --server=server:13013 --workspace=agent-b ~/work/project
cd ~/work/project
tandem log                              # sees Agent A's commit
tandem file show -r <commit> src/auth.rs  # Agent A's file, fetched from server
echo 'pub fn api() {}' > src/api.rs
tandem new -m "feat: add api"
```

### 3. Orchestrator reviews and ships

```bash
# On the server (SSH or local)
cd /srv/project
jj log                                  # sees all agents' work
jj diff -r <commit>                     # reviews actual code changes
jj bookmark create feature -r <tip>
jj git push --bookmark feature
gh pr create --base main --head feature
```

### 4. Upstream changes flow back

```bash
# On the server, after PR is merged
jj git fetch
# Agents automatically see the new commits on next jj command
# (or immediately via watchHeads notification)
```

## Git operations: server only

Git commands run exclusively on the server:
- `jj git push` — server pushes to GitHub
- `jj git fetch` — server pulls from GitHub
- `gh pr create` — server creates PRs

Agents don't need git access. They work through tandem RPC.

This is intentional: the server is the single point of contact with the
outside world. It's where the orchestrator makes decisions about what
ships and what doesn't.

## Tandem as source of truth

The tandem server is the canonical store. GitHub is a mirror.

- The server holds the complete history — all agent objects live there
- `jj git push` mirrors to GitHub for CI, code review, external visibility
- Other teams interact via GitHub as usual
- Agents and the orchestrator work entirely through tandem

Back up the server repo directory. If it's lost without backups, the data
is gone (unless you've been pushing to GitHub regularly).
