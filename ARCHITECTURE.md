# ARCHITECTURE

`Tandem` = jj workspaces over the network.

## Implementation Status

**v1 complete as of 2026-02-15.** All slices 1-9 implemented and tested.
See `docs/exec-plans/completed/` for details.

## Shape

Single binary, two modes:

- `tandem serve --listen <addr> --repo <path>` — server mode
- `tandem <jj-command>` — client mode (stock jj via CliRunner)

## Core model

- Server hosts a **normal jj+git colocated repo** (uses jj's Git backend)
- Client keeps **working copy local** (real files on disk)
- Client store calls are remote via Cap'n Proto RPC
- Backend/OpStore/OpHeadsStore trait implementations route to server
- No `workspace update-stale` — clients always read current heads from server

## Responsibilities

### Server

The server embeds jj-lib and uses the Git backend internally.
When a client calls `putObject(file, bytes)`, the server writes the file
into the jj+git store. Objects are real git objects — `jj git push` on
the server just works.

1. Read/write jj backend + op-store objects (commit/tree/file/symlink/copy/operation/view)
2. Coordinate op heads with atomic compare-and-swap
3. Notify watchers on head changes (`watchHeads`)
4. Host the jj+git colocated repo for git interop

### Client

The `tandem` binary is `CliRunner::init().add_store_factories(tandem_factories()).run()`.

Tandem-provided trait implementations:

- **`TandemBackend`** (`src/backend.rs`) — implements jj-lib's `Backend` trait
  - `read_file/write_file`, `read_tree/write_tree`, `read_commit/write_commit` → `getObject/putObject` RPC
- **`TandemOpStore`** (`src/op_store.rs`) — implements jj-lib's `OpStore` trait
  - `read_operation/write_operation`, `read_view/write_view` → RPC calls
- **`TandemOpHeadsStore`** (`src/op_heads_store.rs`) — implements jj-lib's `OpHeadsStore` trait
  - `get_op_heads/update_op_heads` → `getHeads/updateOpHeads` RPC with CAS

On CAS failure, jj's existing transaction retry flow handles convergence automatically.

The agent runs **normal `jj` commands** (`tandem new`, `tandem log`, `tandem diff`,
`tandem file show`, `tandem bookmark create`, etc.) — tandem is invisible.

## Protocol

Cap'n Proto `Store` service defined in `schema/tandem.capnp`.

Core capabilities:

- **Object I/O:** `getObject(kind, id)`, `putObject(kind, data)`
  - Kinds: commit, tree, file, symlink
- **Operation I/O:** `getOperation(id)`, `putOperation(data)`, `getView(id)`, `putView(data)`
- **Op head coordination:** `getHeads()`, `updateOpHeads(old_ids, new_id)` (CAS)
- **Operation resolution:** `resolveOperationIdPrefix(prefix)`
- **Watch subscriptions:** `watchHeads(watcher)` — streaming notifications
- **Optional capabilities:** `snapshot()`, copy tracking (reserved for future)

No `repoId` in protocol: one server = one repo.

See `src/server.rs` for server implementation, `src/rpc.rs` for client wrapper.

## Git compatibility

No custom git layer in tandem. The server hosts a normal jj+git colocated repo.

Git operations run **on the server only** (v1):

- `jj git fetch` — pull upstream changes into the server's repo
- `jj git push` — push agents' work to GitHub
- `gh pr create` — create PRs from the server

Agents never touch git. The server is the single point of contact with
the outside world. The orchestrator SSHes to the server (or runs commands
locally) to manage git interop.

See `docs/design-docs/workflow.md` for the full workflow.

## Test Coverage

16 integration tests across slices 1-7:

| Slice | Test File | Coverage |
|-------|-----------|----------|
| 1 | `tests/slice1_single_agent_round_trip.rs` | Single agent file round-trip |
| 2 | `tests/v1_slice2_two_agent_visibility.rs` | Two-agent file visibility |
| 3 | `tests/v1_slice3_concurrent_convergence.rs` | 2-agent and 5-agent concurrent writes |
| 4 | `tests/slice4_promise_pipelining.rs` | Cap'n Proto pipelining efficiency |
| 5 | `tests/slice5_watch_heads.rs` | Real-time head notifications |
| 6 | `tests/slice6_git_round_trip.rs` | Git push/fetch round-trip |
| 7 | `tests/slice7_end_to_end.rs` | Multi-agent + git + external contributor |

All tests assert on **file byte content**, not just commit descriptions.

Run: `cargo test`

## Technology choices

- **Language:** Rust
- **Binary:** Single `tandem` (server + client modes)
- **RPC:** Cap'n Proto (promise pipelining for efficiency)
- **Server storage:** Normal jj+git colocated repo (Git backend)
- **Serialization:** jj-native protobuf object/op/view bytes (passed through as blobs)
- **Client CLI:** Stock `jj` via `CliRunner` (not a custom tandem CLI)
- **Dependencies:** `jj-lib`, `jj-cli`, `capnp`, `capnp-rpc`, `tokio`, `prost`

## Project Structure

```
src/
  main.rs              CLI dispatch (clap) + CliRunner passthrough
  server.rs            Server — jj Git backend + Cap'n Proto RPC
  backend.rs           TandemBackend (jj-lib Backend trait)
  op_store.rs          TandemOpStore (jj-lib OpStore trait)
  op_heads_store.rs    TandemOpHeadsStore (jj-lib OpHeadsStore trait)
  rpc.rs               Cap'n Proto RPC client wrapper
  proto_convert.rs     jj protobuf ↔ Rust struct conversion
  watch.rs             tandem watch command
schema/
  tandem.capnp         Cap'n Proto schema (Store + HeadWatcher)
tests/
  common/mod.rs        Test harness (server spawn, HOME isolation)
  slice1-7 tests       Integration tests asserting on file bytes
```

## Non-goals (v1.0)

- Auth / ACL / multi-tenant isolation (single-repo, single-trust-domain model)
- Workflow automation engines (out of scope)
- Web UI / IDE integrations (future)
- Client-side object caching (performance optimization for later)
