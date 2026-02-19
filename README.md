# tandem

> ⚠️ **Experimental software.** tandem is a working prototype — the RPC
> protocol, on-disk format, and CLI surface may change. Don't use it for
> data you can't regenerate. Back up your repos.

jj workspaces over the network. One server, many agents in many vms, real files.

```
tandem serve --listen 0.0.0.0:13013 --repo ~/project   # server
tandem init --tandem-server=host:13013 ~/work           # agent
tandem new -m "feat: add auth"                          # it's just jj
```

tandem is a single binary that embeds [jj](https://jj-vcs.com). The server
hosts a jj+git repo — typically on a VM or VPS as a long-running service.
Agents on remote machines get transparent read/write access over Cap'n Proto
RPC. Every stock jj command works — `log`, `new`, `diff`, `file show`,
`bookmark`, `describe` — because tandem implements jj-lib's `Backend`,
`OpStore`, and `OpHeadsStore` traits as RPC stubs.

## Why

Coding agents need to collaborate on the same codebase without stepping on
each other. The current approach — git worktrees on a single machine — breaks
down when agents run on different machines, fight over `.git` locks, or need
to read each other's work-in-progress.

tandem gives each agent an isolated workspace that shares a single store over
the network. Agents see each other's commits instantly. No push/pull, no merge
conflicts on the transport layer. The server ships to GitHub when you're ready.

## How it works

```
┌──────────────┐                           ┌──────────────────────────┐
│  Agent A      │    Cap'n Proto RPC        │                          │
│  (Machine B)  │◄─────────────────────────►│    tandem serve           │
│               │                           │    (Machine A)            │
│  ~/work-a/    │                           │                          │
│  src/auth.rs  │                           │  ┌────────────────────┐  │
│  src/lib.rs   │                           │  │ Content-Addressed  │  │
└──────────────┘                           │  │ Store              │  │
┌──────────────┐                           │  │                    │  │
│  Agent B      │    Cap'n Proto RPC        │  │  jj+git repo       │  │
│  (Machine C)  │◄─────────────────────────►│  │  operations        │  │──► git push
│               │                           │  │  views             │  │
│  ~/work-b/    │                           │  │  op heads (CAS)    │  │
│  src/api.rs   │                           │  └────────────────────┘  │
└──────────────┘                           │                          │
┌──────────────┐                           │                          │
│  Agent C      │    Cap'n Proto RPC        │                          │
│  (Machine D)  │◄─────────────────────────►│                          │
│               │                           │                          │
│  ~/work-c/    │                           └──────────────────────────┘
│  tests/*.rs   │
└──────────────┘
```

Each agent has a full working copy on its local disk (fast reads/writes).
The commit store lives on the server. When Agent A commits, Agent B sees it
instantly in `tandem log` — no fetch, no pull, no merge.

The `tandem` binary has two modes:

- **`tandem serve`** — hosts the jj+git repo, accepts RPC connections
- **`tandem <jj-command>`** — runs stock jj with tandem as the remote store

The client registers three jj-lib trait implementations:

| Trait | What it stores | RPC calls |
|-------|---------------|-----------|
| `Backend` | Files, trees, commits, symlinks | `getObject`, `putObject` |
| `OpStore` | Operations, views | `getObject`, `putObject` |
| `OpHeadsStore` | Operation head pointers | `getHeads`, `updateOpHeads` (CAS) |

Concurrent writes use compare-and-swap on operation heads with automatic
retry. Two agents committing simultaneously both succeed — CAS contention
resolves transparently.

## Install

Published on [crates.io](https://crates.io/crates/jj-tandem) as `jj-tandem`.

```bash
cargo install jj-tandem
```

This installs the `tandem` binary. Requires a Rust toolchain and
[Cap'n Proto compiler](https://capnproto.org/install.html) (`capnp`).

To build from source:

```bash
git clone https://github.com/laulauland/tandem.git
cd tandem
cargo build --release
# binary at target/release/tandem
```

## Quickstart

### Start a server

```bash
tandem serve --listen 0.0.0.0:13013 --repo ~/project
```

For production use, run this on a VM/VPS — see [Deployment setups](#deployment-setups) below.

### Connect agents

```bash
# Agent A
tandem init --tandem-server=server:13013 ~/work-a
cd ~/work-a
echo 'pub fn auth(token: &str) -> bool { !token.is_empty() }' > auth.rs
tandem new -m "feat: add auth module"

# Agent B (different machine, or different terminal)
tandem init --tandem-server=server:13013 --workspace=agent-b ~/work-b
cd ~/work-b
echo 'pub fn api() -> String { "ok".into() }' > api.rs
tandem new -m "feat: add API handler"
```

### What agents see

Agent B runs `tandem log` and sees everyone's work:

```
@  w agent-b  agent-b@  f3f18a89
│  (empty) feat: add API handler
○  o agent-b  a918ed0d
│  api.rs
│ ○  k agent-a  default@  7acb3ff6
│ │  (empty) feat: add auth module
│ ○  u agent-a  78f31413
├─╯  auth.rs
◆  z root()  00000000
```

Agent B reads Agent A's file directly:

```bash
$ tandem file show -r k auth.rs
pub fn auth(token: &str) -> bool { !token.is_empty() }
```

### Ship via git

On the server:

```bash
jj bookmark create main -r <tip>
jj git push --bookmark main
```

The server is a real jj+git repo. Standard git push just works.

---

## Deployment setups

### Local: multiple terminals (for testing)

The simplest setup for trying tandem out. Server and agents on the same
machine, different directories. Not how you'd run it for real work — see
Remote machines below.

```bash
# Terminal 1 — server
tandem serve --listen 127.0.0.1:13013 --repo /tmp/project

# Terminal 2 — agent A
tandem init --tandem-server=127.0.0.1:13013 /tmp/agent-a
cd /tmp/agent-a && echo 'hello' > file.txt && tandem new -m "agent A"

# Terminal 3 — agent B
tandem init --tandem-server=127.0.0.1:13013 --workspace=agent-b /tmp/agent-b
cd /tmp/agent-b && tandem log   # sees agent A's commit
```

### Remote machines: sprites.dev / exe.dev / SSH

The typical production setup. Server on a VPS/VM, agents on separate machines.

```bash
# VPS — server
tandem serve --listen 0.0.0.0:13013 --repo /srv/project

# Machine A — agent A (e.g. sprites.dev sandbox)
# Copy the binary over, or build on the remote machine
scp target/release/tandem agent-a-host:/usr/local/bin/
ssh agent-a-host
  export TANDEM_SERVER=server-host:13013
  tandem init ~/work
  cd ~/work
  # ... write code, commit with tandem new ...

# Machine B — agent B (e.g. exe.dev VM)
scp target/release/tandem agent-b-host:/usr/local/bin/
ssh agent-b-host
  export TANDEM_SERVER=server-host:13013
  tandem init --workspace=agent-b ~/work
  cd ~/work
  tandem log                  # sees agent A's commits
  tandem file show -r <change-id> src/auth.rs   # reads agent A's files
```

Requirements:
- Server port (default 13013) must be reachable from agent machines
- No TLS yet — use SSH tunnels or VPN for untrusted networks
- The `tandem` binary is ~30MB, statically linkable, no runtime deps

### Docker: 3 agents on a shared network

Each agent runs in its own container. They connect to the server container
by hostname over a Docker bridge network.

```bash
# Build Linux binary (if on macOS)
docker run --rm -v $(pwd):/src -v tandem-cargo:/usr/local/cargo/registry \
  -w /src rust:1.84-slim \
  bash -c 'apt-get update -qq && apt-get install -y -qq capnproto >/dev/null 2>&1 && cargo build --release'

# Create network
docker network create tandem-net

# Server
docker run -d --name tandem-server --network tandem-net \
  -v $(pwd)/target/release/tandem:/usr/local/bin/tandem \
  debian:trixie-slim \
  tandem serve --listen 0.0.0.0:13013 --repo /srv/project

# Agent A
docker run --rm --network tandem-net \
  -v $(pwd)/target/release/tandem:/usr/local/bin/tandem \
  debian:trixie-slim bash -c '
    tandem init --tandem-server=tandem-server:13013 /work
    cd /work
    echo "from agent A" > hello.txt
    tandem --config=fsmonitor.backend=none new -m "agent A commit"
    tandem --config=fsmonitor.backend=none log --no-graph
  '

# Agent B
docker run --rm --network tandem-net \
  -v $(pwd)/target/release/tandem:/usr/local/bin/tandem \
  debian:trixie-slim bash -c '
    tandem init --tandem-server=tandem-server:13013 --workspace=agent-b /work
    cd /work
    tandem --config=fsmonitor.backend=none log --no-graph
    tandem --config=fsmonitor.backend=none file show -r <agent-a-change> hello.txt
  '

# Cleanup
docker stop tandem-server && docker rm tandem-server
docker network rm tandem-net
```

This simulates cross-machine communication. Each container has its own
filesystem, its own network identity, and connects to the server by DNS name.
Tested — see `qa/v1/cross-machine-report.md`.

### Claude Code: multi-agent with tandem

Each Claude Code instance gets its own tandem workspace. They see each
other's work in real time via the shared store.

```bash
# Server (your machine)
tandem serve --listen 0.0.0.0:13013 --repo ~/project

# Agent 1 — in one terminal
tandem init --tandem-server=localhost:13013 --workspace=backend ~/work-backend
cd ~/work-backend
claude --prompt "Implement auth module in src/auth.rs. Use tandem for version control (not git). Run tandem log to see context."

# Agent 2 — in another terminal
tandem init --tandem-server=localhost:13013 --workspace=frontend ~/work-frontend
cd ~/work-frontend
claude --prompt "Implement UI in src/routes.rs. Run tandem log to see other agents' work. Read files with: tandem file show -r <change-id> <path>"

# Agent 3 — in another terminal
tandem init --tandem-server=localhost:13013 --workspace=tests ~/work-tests
cd ~/work-tests
claude --prompt "Write tests for the code other agents wrote. Run tandem log, then tandem file show to read their implementations."
```

Add this to each agent's system prompt or CLAUDE.md:

```
You're working in a tandem workspace (jj over the network).
Use tandem instead of git for all version control:

  tandem log                           # see all agents' commits
  tandem new -m "description"          # commit your changes
  tandem diff -r @-                    # see what you changed
  tandem file show -r <rev> <path>     # read any agent's file
  tandem bookmark create <name> -r @-  # mark for review

Before starting work, run tandem log to see what others have done.
Do NOT use git commands — this repo uses tandem.
```

### Orchestrator pattern

One orchestrator manages the server and ships code. Multiple agents work
independently.

```bash
# Orchestrator machine
tandem serve --listen 0.0.0.0:13013 --repo ~/project

# ... agents do their work on remote machines ...

# When ready to ship:
cd ~/project
jj log                                    # see all agents' work
jj new --no-edit -m "merge: auth + api"   # create merge point
jj bookmark create main -r <tip>
jj git push --bookmark main               # ship to GitHub
```

The orchestrator never writes code. They review with `jj log`, `jj diff`,
`jj show`, and ship with `jj git push`. The server repo IS the jj+git repo,
so standard git tooling works.

---

## vs git worktrees

Most multi-agent tools (Conductor, Claude Squad, Cursor) use git worktrees
for agent isolation. tandem takes a different approach:

| | Git worktrees | Tandem |
|---|---|---|
| Machine scope | Same machine only | Any machine |
| Agent visibility | Must checkout other branch | `tandem log` shows all instantly |
| Concurrent writes | Merge conflicts at integration | CAS convergence — both succeed |
| Store sharing | Shared `.git` dir (lock contention) | Network RPC (no locks) |
| Git push | From any worktree | Server-only (single source of truth) |
| Disk usage | Full working copy × N worktrees | Full working copy × N (same) |
| Setup | `git worktree add` | `tandem init --workspace=<name>` |

tandem trades latency (every read/write is an RPC) for cross-machine
collaboration and instant visibility. If all your agents are on one machine,
git worktrees are simpler. If they're on different machines, or you need
agents to see each other's work without merging, tandem is what you want.

---

## Commands

```
tandem serve --listen <addr> --repo <path>     Start server
tandem init --tandem-server <addr> [path]      Init workspace
tandem watch --server <addr>                   Stream head notifications
tandem log                                     Show commit history
tandem new -m "message"                        Create new change
tandem diff -r @-                              Show changes
tandem file show -r <rev> <path>               Read file at revision
tandem bookmark create <name> -r <rev>         Create bookmark
tandem describe -m "message"                   Update description
tandem ...                                     Any jj command
```

## Environment variables

| Variable | Purpose |
|----------|---------|
| `TANDEM_SERVER` | Server address — fallback for `--tandem-server` |
| `TANDEM_WORKSPACE` | Workspace name (default: `default`) |

## Tests

```bash
cargo test
```

16 integration tests covering:

- Single-agent file round-trip (write → commit → read back exact bytes)
- Two-agent cross-workspace file visibility
- Concurrent writes from 2 and 5 agents (CAS convergence)
- Promise pipelining (rapid sequential writes)
- WatchHeads real-time notifications
- Git round-trip (tandem → jj git objects)
- End-to-end multi-agent with bookmarks

Cross-machine tested with Docker containers — see `qa/v1/cross-machine-report.md`.

## Known limitations

- **No TLS** — connections are plaintext. For production on a VPS, use SSH tunnels (`ssh -L`) or a VPN.
- **No auth** — anyone who can reach the port can read/write the repo. On a VPS, firewall the port and use SSH tunnels for access.
- **No static binary yet** — requires glibc 2.39+. Use matching distro or build locally.
- **fsmonitor conflict** — if your jj config has `fsmonitor.backend = "watchman"`,
  pass `--config=fsmonitor.backend=none` to tandem commands.
- **Description-based revsets** — `description(exact:"...")` may not work for
  cross-workspace queries. Use change IDs from `tandem log` instead.

## Running in production

- **Back up the server repo directory** — it's the source of truth. If lost without backups, data is gone (unless mirrored to GitHub via `jj git push`).
- **Use a process manager** (systemd, supervisord, etc.) to keep `tandem serve` running.
- **Git credentials on the server** — the server needs SSH keys or tokens for `jj git push` / `jj git fetch` to GitHub.
- **Monitor disk space** — all agent objects (files, trees, commits) land on the server.
- **Firewall the port** — no auth means network-level access control is your only defense. SSH tunnels or VPN.

## Project structure

```
src/
  main.rs              CLI dispatch (clap) + jj CliRunner passthrough
  server.rs            Server — jj Git backend + Cap'n Proto RPC
  backend.rs           TandemBackend (jj-lib Backend trait over RPC)
  op_store.rs          TandemOpStore (jj-lib OpStore trait over RPC)
  op_heads_store.rs    TandemOpHeadsStore (CAS head management over RPC)
  rpc.rs               Cap'n Proto RPC client
  proto_convert.rs     jj protobuf ↔ Rust struct conversion
  watch.rs             tandem watch command
schema/
  tandem.capnp         Cap'n Proto schema (13 Store methods + HeadWatcher)
tests/
  common/mod.rs        Test harness (server spawn, HOME isolation)
  slice1-7 tests       Integration tests asserting on file bytes
```

## License

MIT
