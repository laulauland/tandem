# tandem

> ⚠️ **Experimental software.** tandem is a working prototype — the RPC
> protocol, on-disk format, and CLI surface may change. Don't use it for
> data you can't regenerate. Back up your repos.

jj workspaces over the network. One server, many agents on many machines, real files.

## What does tandem do?

- Runs a central server that hosts a real `jj`+`git` repo.
- Lets each agent/machine use its own local workspace backed by that server.
- Makes all work visible across agents via normal `jj` commands (`log`, `diff`, `file show`, `new`, etc.).
- Keeps shipping simple: push to GitHub from the server with `jj git push`.

## Install

Published on [crates.io](https://crates.io/crates/jj-tandem) as `jj-tandem`.
Requires a Rust toolchain. No system `capnp` binary is required for install/build.

```bash
cargo install jj-tandem
```

Or build from source:

```bash
git clone https://github.com/laulauland/tandem.git && cd tandem
cargo build --release
```

## Quickstart

```bash
# On your server (VPS, or localhost for testing)
tandem up --repo ~/project --listen 0.0.0.0:13013
tandem server status

# On each agent's machine
tandem init --server=your-server:13013 ~/work
cd ~/work
echo 'pub fn auth() {}' > auth.rs
tandem new -m "feat: add auth"
```

That's it. The agent is now using jj against the remote store — `tandem log`,
`tandem diff`, `tandem file show`, `tandem bookmark` all work because tandem
implements jj-lib's store traits as RPC stubs. The server holds a real jj+git
repo, so `jj git push` on the server ships to GitHub.

---

## Deployment

### On a VPS (recommended)

The default setup. Server on a VPS, agents connect from their machines.

```bash
# SSH to your VPS, install tandem
cargo install jj-tandem

# Start the server
tandem up --repo /srv/project --listen 0.0.0.0:13013

# Verify
tandem server status
```

On agent machines:

```bash
# Agent A
tandem init --server=your-vps:13013 ~/work
cd ~/work
echo 'pub fn auth(token: &str) -> bool { !token.is_empty() }' > auth.rs
tandem new -m "feat: add auth module"

# Agent B (different machine)
tandem init --server=your-vps:13013 --workspace=agent-b ~/work
cd ~/work
tandem log                                     # sees Agent A's commit
tandem file show -r <change-id> auth.rs        # reads Agent A's file
echo 'pub fn api() -> &str { "ok" }' > api.rs
tandem new -m "feat: add API handler"
```

Ship via git from the server:

```bash
# On the VPS
cd /srv/project
jj bookmark create main -r <tip>
jj git push --bookmark main
```

The server is a real jj+git repo. `jj git push` just works.

### Local testing

Server and agents on the same machine, different directories.

```bash
# Start server
tandem up --repo /tmp/project --listen 127.0.0.1:13013

# Agent A
tandem init --server=127.0.0.1:13013 /tmp/agent-a
cd /tmp/agent-a && echo 'hello' > file.txt && tandem new -m "agent A"

# Agent B
tandem init --server=127.0.0.1:13013 --workspace=agent-b /tmp/agent-b
cd /tmp/agent-b && tandem log   # sees agent A's commit

# Done
tandem down
```

### Docker

Containers connecting to a server. Use `tandem serve` (foreground mode) —
appropriate for container entrypoints.

```bash
docker network create tandem-net

# Server container
docker run -d --name tandem-server --network tandem-net \
  -v $(pwd)/target/release/tandem:/usr/local/bin/tandem \
  debian:trixie-slim \
  tandem serve --listen 0.0.0.0:13013 --repo /srv/project

# Agent container
docker run --rm --network tandem-net \
  -v $(pwd)/target/release/tandem:/usr/local/bin/tandem \
  debian:trixie-slim bash -c '
    tandem init --server=tandem-server:13013 /work
    cd /work
    echo "from agent A" > hello.txt
    tandem new -m "agent A commit"
    tandem log --no-graph
  '

docker stop tandem-server && docker rm tandem-server
docker network rm tandem-net
```

### With Claude Code / AI agents

Each agent gets its own tandem workspace. They see each other's work in real
time via the shared store.

```bash
# Server (your VPS)
tandem up --repo /srv/project --listen 0.0.0.0:13013

# Agent 1
tandem init --server=your-vps:13013 --workspace=backend ~/work-backend
cd ~/work-backend
claude --prompt "Implement auth module in src/auth.rs. Use tandem for version control."

# Agent 2
tandem init --server=your-vps:13013 --workspace=frontend ~/work-frontend
cd ~/work-frontend
claude --prompt "Implement UI. Run tandem log to see other agents' work."
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

---

## Commands

`tandem status` is the stock jj working-copy status command.
Use `tandem server status` for daemon health.

### Server lifecycle

Start, stop, and monitor the tandem server.

```
tandem up --repo <path> [--listen <addr>] [--enable-integration-workspace]
                                                Start background daemon
tandem down                                     Stop the daemon
tandem server status                            Check if daemon is running
tandem server logs                              Stream logs from daemon
tandem serve --listen <addr> --repo <path> [--enable-integration-workspace]
                                                Start server (foreground)
```

**tandem up** — starts a background daemon and returns immediately.

```
tandem up --repo <path> [--listen <addr>] [--log-level <level>] [--log-file <path>]
                         [--control-socket <path>]
                         [--enable-integration-workspace]
```

Forks `tandem serve --daemon` in the background. Waits for the control socket
to become healthy, prints the PID, exits. If a daemon is already running,
exits with an error.

If `--listen` is omitted, tandem chooses a listen address with this heuristic:
1) reuse the last successful listen address for this repo (if still free),
2) otherwise pick the first free port in `0.0.0.0:13013-13063`, with a
repo-path hash offset to reduce collisions across repos.

**tandem down** — stops the running daemon.

```
tandem down [--control-socket <path>]
```

Sends a shutdown request via the control socket, waits for the process to exit.

**tandem server status** — reports whether the daemon is running.

```
tandem server status [--json] [--control-socket <path>]
```

Exit code 0 = running, 1 = not running.

```
$ tandem server status
tandem is running
  PID:      1234
  Uptime:   2h 15m
  Repo:     /srv/project
  Listen:   0.0.0.0:13013
  Version:  0.3.0
  Integration workspace: disabled
```

```
$ tandem server status --json
{"running":true,"pid":1234,"uptime_secs":8100,"repo":"/srv/project","listen":"0.0.0.0:13013","version":"0.3.0","integration":{"enabled":false,"lastStatus":"disabled"}}
```

**tandem server logs** — streams log output from the daemon.

```
tandem server logs [--level <level>] [--json] [--control-socket <path>]
```

Connects to the control socket and streams log events. `--level` filters
server-side (trace, debug, info, warn, error). `--json` outputs raw JSON
lines instead of formatted text.

JSON log objects include structured fields:
`ts`, `level`, `target`, `msg`, and `fields`.

**tandem serve** — runs the server in the foreground. Use this for systemd,
Docker, or debugging. Logs to stderr.

Pass `--enable-integration-workspace` to keep an `integration` bookmark updated
from active workspace heads. This mode is off by default.

```
tandem serve --listen <addr> --repo <path> [--log-level <level>] [--log-format <fmt>]
             [--control-socket <path>] [--log-file <path>]
             [--enable-integration-workspace]
```

### Workspace setup

```
tandem init --server <addr> [--workspace <name>] [path]
```

Initializes a tandem-backed workspace. Creates the directory, registers the
tandem backend, and connects to the server. `--workspace` names the workspace.
If omitted, tandem auto-generates a unique workspace name to avoid cross-device
workspace collisions by default.

### Watch

```
tandem watch --server <addr>
```

Streams head change notifications from the server. Useful for triggering
rebuilds or CI when any agent commits.

### Everything else

Every jj command works through tandem:

```
tandem status                           Show working-copy status
tandem log                              Show commit history
tandem new -m "message"                 Create new change
tandem diff -r @-                       Show changes
tandem file show -r <rev> <path>        Read file at revision
tandem bookmark create <name> -r <rev>  Create bookmark
tandem describe -m "message"            Update description
```

The `tandem` binary embeds jj — these are stock jj commands running against
the remote store.

---

## Environment variables

| Variable | Purpose |
|----------|---------|
| `TANDEM_SERVER` | Server address — fallback for `--server` |
| `TANDEM_WORKSPACE` | Workspace name fallback for `tandem init` when `--workspace` is not provided. |
| `TANDEM_LISTEN` | Listen address fallback for `tandem up --listen`. |
| `TANDEM_ENABLE_INTEGRATION_WORKSPACE` | Set to `1`/`true` to enable integration workspace mode when `--enable-integration-workspace` is not passed. |

---

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

The `tandem` binary has two ways to run the server:

- **`tandem up`** — starts a background daemon. No systemd needed.
- **`tandem serve`** — runs in the foreground. For systemd, Docker, debugging.

And one way to run as a client:

- **`tandem <jj-command>`** — runs stock jj with tandem as the remote store.

The client registers three jj-lib trait implementations:

| Trait | What it stores | RPC calls |
|-------|---------------|-----------|
| `Backend` | Files, trees, commits, symlinks | `getObject`, `putObject` |
| `OpStore` | Operations, views | `getObject`, `putObject` |
| `OpHeadsStore` | Operation head pointers | `getHeads`, `updateOpHeads` (CAS) |

Concurrent writes use compare-and-swap on operation heads with automatic
retry. Two agents committing simultaneously both succeed — CAS contention
resolves transparently.

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

## Tests

```bash
cargo test
```

38 integration tests covering:

- Single-agent file round-trip (write → commit → read back exact bytes)
- Two-agent cross-workspace file visibility
- Concurrent writes from 2 and 5 agents (CAS convergence)
- Promise pipelining (rapid sequential writes)
- WatchHeads real-time notifications
- Git round-trip (tandem → jj git objects)
- End-to-end multi-agent with bookmarks
- Signal handling and graceful shutdown
- Control socket status reporting
- Daemon lifecycle (up/down)
- Log streaming

Cross-machine tested with Docker containers — see `qa/v1/cross-machine-report.md`.

## Known limitations

- **No TLS** — connections are plaintext. Use SSH tunnels or a VPN for untrusted networks.
- **No auth** — anyone who can reach the port can read/write the repo. Firewall the port and use SSH tunnels for access.
- **Unix only for daemon management** — `tandem up`, `tandem down`, `tandem server status`, and `tandem server logs` use Unix domain sockets. macOS and Linux only, not Windows. (`tandem serve` works everywhere.)
- **No static binary yet** — requires glibc 2.39+. Use matching distro or build locally.
- **fsmonitor conflict** — if your jj config has `fsmonitor.backend = "watchman"`,
  pass `--config=fsmonitor.backend=none` to tandem commands.

## Running in production

- **Back up the server repo directory** — it's the source of truth.
- **Git credentials on the server** — the server needs SSH keys or tokens for `jj git push` / `jj git fetch`.
- **Monitor disk space** — all agent objects land on the server.
- **Firewall the port** — no auth means network-level access control is your only defense.

## Maintainer note: schema regeneration

`tandem` checks in generated bindings at `src/tandem_capnp.rs`.
When you change `schema/tandem.capnp`, regenerate via:

```bash
TANDEM_REGENERATE_BINDINGS=1 cargo build
```

(`build.rs` compiles from schema when `capnp` is available, and falls back to
checked-in bindings otherwise.)

## Project structure

```
src/
  main.rs              CLI dispatch (clap) + jj CliRunner passthrough
  tandem_capnp.rs      Generated Cap'n Proto bindings (checked in)
  server.rs            Server — jj Git backend + Cap'n Proto RPC
  control.rs           Control socket — daemon management protocol (Unix socket, JSON lines)
  backend.rs           TandemBackend (jj-lib Backend trait over RPC)
  op_store.rs          TandemOpStore (jj-lib OpStore trait over RPC)
  op_heads_store.rs    TandemOpHeadsStore (CAS head management over RPC)
  rpc.rs               Cap'n Proto RPC client
  proto_convert.rs     jj protobuf ↔ Rust struct conversion
  watch.rs             tandem watch command
schema/
  tandem.capnp         Cap'n Proto schema (13 Store methods + HeadWatcher)
build.rs               Build-time schema generation with checked-in fallback
tests/
  common/mod.rs        Test harness (server spawn, HOME isolation)
  slice1-7 tests       Core integration tests (file round-trip, visibility, CAS, git)
  slice10-13 tests     Server lifecycle tests (shutdown, control socket, up/down, logs)
```

## License

MIT
