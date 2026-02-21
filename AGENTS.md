# AGENTS

Execution guide for working on the `tandem` codebase.

## How to read these docs

1. Read `ARCHITECTURE.md` for system boundaries and project structure.
2. Read `docs/design-docs/workflow.md` for the orchestrator→agents→git workflow.
3. Read `docs/design-docs/jj-lib-integration.md` for trait signatures and registration.
4. Read `docs/exec-plans/tech-debt-tracker.md` for known issues to work on.
5. Check `docs/exec-plans/completed/` for context on how each slice was built.
6. Implement changes via failing integration test first.

## What tandem is

Tandem applies a **server-client model to jj's store layer**. The server hosts
a normal jj+git colocated repo. Agents on remote machines use the `tandem`
binary (which embeds jj-cli with a custom tandem backend) to read and write
objects over Cap'n Proto RPC.

The server is the **point of origin** — it typically runs on a VM/VPS as a
long-running service. It's where git operations happen (`jj git push`,
`jj git fetch`, `gh pr create`). The orchestrator/teamlead runs these on the
server to ship code upstream. The tandem server is the source of truth, with
GitHub as a mirror.

## Installation

Published on [crates.io](https://crates.io/crates/jj-tandem) as `jj-tandem`:

```bash
cargo install jj-tandem
```

Requires a Rust toolchain. No system `capnp` binary is required for normal installs/builds.
Or build from source: `cargo build --release`.

`build.rs` compiles schema bindings when `capnp` is available and falls back to
checked-in `src/tandem_capnp.rs` when it is not.

Maintainers only: when changing `schema/tandem.capnp`, regenerate checked-in bindings:
`TANDEM_REGENERATE_BINDINGS=1 cargo build`.

## Single binary, three modes

```
tandem up --repo <path> --listen <addr>       # start background daemon
tandem serve --listen <addr> --repo <path>    # foreground server (systemd/docker)
tandem [jj args...]                           # client mode (stock jj via CliRunner)
```

Plus lifecycle commands that talk to a running server:

```
tandem down                                   # stop daemon
tandem status [--json]                        # health check
tandem logs [--level <level>] [--json]        # stream logs from daemon
```

The client mode is `CliRunner::init().add_store_factories(tandem_factories()).run()`.
All stock jj commands work transparently: `tandem new`, `tandem log`, `tandem diff`,
`tandem file show`, `tandem bookmark create` are all jj commands running through our binary.

Server mode embeds jj-lib and uses the Git backend internally. When a client
calls `putObject(file, bytes)`, the server stores the object. Objects are real
jj-compatible blobs — `jj git push` on the server just works.

`tandem up` is the easy way to start the server — it forks `tandem serve --daemon`
in the background, waits for the control socket to become healthy, prints the PID,
and exits. `tandem serve` is the foreground mode for systemd, Docker, or debugging.
Both create a control socket so `tandem down/status/logs` work against either.

## Source layout

```
src/
  main.rs              CLI dispatch (clap) + CliRunner passthrough
  tandem_capnp.rs      Generated Cap'n Proto bindings (checked in)
  server.rs            Server — jj Git backend + Cap'n Proto RPC
  control.rs           Control socket — daemon management (Unix socket, JSON lines)
  backend.rs           TandemBackend (jj-lib Backend trait)
  op_store.rs          TandemOpStore (jj-lib OpStore trait)
  op_heads_store.rs    TandemOpHeadsStore (jj-lib OpHeadsStore trait)
  rpc.rs               Cap'n Proto RPC client wrapper
  proto_convert.rs     jj protobuf ↔ Rust struct conversion
  watch.rs             tandem watch command
schema/
  tandem.capnp         Cap'n Proto schema (Store + HeadWatcher)
build.rs               Build-time schema generation with checked-in fallback
tests/
  common/mod.rs        Test harness (server spawn, HOME isolation)
  slice1-7 tests       Core integration tests (file round-trip, visibility, CAS, git)
  slice10-13 tests     Server lifecycle tests (shutdown, control socket, up/down, logs)
```

## Docs layout

```
docs/
  README.md                          Overview and pointers
  design-docs/
    workflow.md                      Orchestrator→agents→git workflow
    jj-lib-integration.md            Trait signatures and store registration
    rpc-protocol.md                  Cap'n Proto protocol details
    rpc-error-model.md               Error handling conventions
    server-lifecycle.md              tandem up/down/status/logs design
    core-beliefs.md                  Design principles
  exec-plans/
    completed/                       Completion notes for all 13 slices
    tech-debt-tracker.md             Known issues (P1/P2/P3)
  product-specs/
    core-product.md                  Product intent and scope
```

## Critical invariants

1. **The client is stock `jj`.** Tandem implements jj-lib's `Backend`, `OpStore`,
   and `OpHeadsStore` traits as Cap'n Proto RPC stubs. There is no custom
   `tandem new/log/describe/diff` CLI — those are all jj commands.

2. **Tests assert on file bytes, not descriptions.** Every integration test
   must verify file content round-trips correctly via `jj cat`. Description-only
   assertions are insufficient.

3. **Help text works without a server.** `tandem --help`, `tandem serve --help`,
   and `tandem` with no args must print usage locally. Error messages must
   suggest alternatives for unknown commands and include addresses for
   connection failures.

## Help text and error handling (P0)

These are required, not nice-to-haves. QA found agents spend 50% of their time
guessing commands when help is missing.

- `tandem --help` — prints usage without server connection
- `tandem serve --help` — explains `--listen` and `--repo` flags
- `tandem` with no args — prints usage, not a cryptic error
- Unknown commands — suggest alternatives ("did you mean `new`?")
- Connection errors — include the address that was tried
- Missing args — say what's needed ("serve requires `--listen <addr>`")
- `TANDEM_SERVER` env var — fallback for `--server` flag on client commands
- `TANDEM_WORKSPACE` env var — workspace name for client

## Workflow

See `docs/design-docs/workflow.md` for the full picture. Summary:

1. **Orchestrator** sets up server on a VM/VPS: `tandem up --repo /srv/project --listen 0.0.0.0:13013`
2. **Agents** init workspaces: `tandem init --tandem-server=host:13013 ~/work/project`
3. **Agents** use stock jj commands: write files, `tandem new -m "feat: add auth"`, etc.
4. **Agents** see each other's files: `tandem file show -r <other-commit> src/auth.rs`
5. **Orchestrator** ships from server: `jj bookmark create main -r <tip>`, `jj git push`

Git operations are server-only. Agents never touch git directly.

## What exists

All core functionality is implemented across 13 slices:

| Capability | Test coverage |
|------------|--------------|
| Single-agent file round-trip | `tests/slice1_single_agent_round_trip.rs` |
| Two-agent file visibility | `tests/slice2_two_agent_visibility.rs` |
| Concurrent file writes converge | `tests/slice3_concurrent_convergence.rs` |
| Promise pipelining for writes | `tests/slice4_promise_pipelining.rs` |
| WatchHeads real-time notifications | `tests/slice5_watch_heads.rs` |
| Git push/fetch round-trip | `tests/slice6_git_round_trip.rs` |
| End-to-end multi-agent + git | `tests/slice7_end_to_end.rs` |
| Bookmark management via RPC | Slice 8 (see `docs/exec-plans/completed/`) |
| CLI help and discoverability | Slice 9 (see `docs/exec-plans/completed/`) |
| Signal handling + graceful shutdown | `tests/slice10_graceful_shutdown.rs` |
| Control socket + tandem status | `tests/slice11_control_socket.rs` |
| tandem up + tandem down | `tests/slice12_up_down.rs` |
| tandem logs (streaming) | `tests/slice13_log_streaming.rs` |

See `docs/exec-plans/completed/` for detailed completion notes on each slice.
See `docs/exec-plans/tech-debt-tracker.md` for known issues and next work.

## Testing policy

- Integration tests are the primary source of truth.
- Tests use the `tandem` binary which runs jj commands — never a separate jj binary.
- Acceptance criteria assert on **file bytes** via `jj cat`, not just log descriptions.
- Local deterministic tests first; cross-machine tests second.
- Use `sprites.dev` / `exe.dev` for distributed smoke tests.
- Keep networked tests opt-in (ignored by default / env-gated).
- Run: `cargo test`

## QA policy

- After major milestones, run agent-based QA (see `qa/`).
- QA uses **subagent programs**, not shell scripts — agents evaluate usability.
- Naive agent (zero-docs trial-and-error) tests discoverability.
- Workflow agent tests realistic multi-agent file collaboration.
- Stress agent tests concurrent write correctness.
- Reports go to `qa/REPORT.md`.
- Use opus for all implementation and evaluation models.

## Debug policy

Structured tracing is built in. Do not add ad-hoc debug prints.

Flags:

- `--tandem-debug`
- `--tandem-debug-format pretty|json`
- `--tandem-debug-file <path>`
- `--tandem-debug-filter <filter>`

Minimum events emitted:

- command lifecycle
- RPC lifecycle
- object read/write (kind, id, size)
- CAS heads success/failure + retries
- watcher subscribe/notify/reconnect
