# ARCHITECTURE

`tandem` = jj workspaces over the network.

One binary, two roles. As a server it holds a repository and answers HTTP. As
a client it is stock jj, with the store traits pointed at that server. What
edits the files — an agent, a compiler, a person — is outside the model:
a daemon watches the filesystem and publishes what changes.

The design this implements is
[docs/design-docs/target-architecture.md](docs/design-docs/target-architecture.md);
its invariants are binding on everything below. The workflow it produces is
[docs/design-docs/workflow.md](docs/design-docs/workflow.md).

## Shape

```
tandem up --repo <path> [--listen <addr>] [--bucket <url>]   background server
tandem serve --listen <addr> --repo <path> [--bucket <url>]  foreground server
tandem down | tandem server status | tandem server logs      manage it
tandem clone <server> <dir> --workspace <name> --token <t>   make a workspace
tandem daemon <dir> [--debounce-ms N]                        watch and publish
tandem watch --server <addr> --token <t>                     read the wake-ups
tandem <jj-command>                                          stock jj
```

`tandem up` forks `tandem serve --daemon`, waits for the control socket to
answer, prints the PID and exits. `tandem serve` is the same server in the
foreground, for systemd, Docker, or a debugger. Both open the control socket,
so `down`, `server status` and `server logs` work against either. `tandem init`
is the older spelling of `clone` and remains.

## Core model

The durable truth is a **write-ahead log in a bucket**, not a disk. One publish
is one immutable WAL entry; the set of op heads is one index object, updated by
compare-and-swap. `POST /api/heads` acknowledges only after both are durable.

The server's jj+git colocated repo is therefore a **materialization**, not the
original: kill the server, run `tandem up --bucket <same url>` anywhere, and it
replays. The repo exists so that git interop is real git and jj-lib does the
jj-shaped work, not because it holds anything that cannot be rebuilt.

A client keeps a real working copy on real disk. Its store reads go over HTTP
through a content-addressed disk cache, so an object fetched once is never
fetched again — by that client, or by anything that inherits its cache
directory, which is what image baking is.

`.jj/repo/tandem/heads.json` is a metadata sidecar (`version`,
`workspace_heads`), never the authority; op-head authority is jj-lib's own
op-heads store, on the server.

## Responsibilities

### Server (`src/server/`)

1. Read and write jj backend and op-store objects: commit, tree, file, symlink,
   copy, operation, view (`mod.rs`, `http.rs`).
2. Order publishes: compare-and-swap on op heads, with jj-lib's op-heads APIs
   doing the mutation, in one fixed order — WAL entry, then index CAS, then any
   local state (`bucket.rs`).
3. Write the WAL and keep the index (`bucket.rs`, `../wal.rs`), and replay them
   at start-up whenever a crash left the bucket ahead of the repo, up to and
   including materializing the repo from nothing.
4. Answer who is asking and whether they may, and enforce what a token may
   publish as a diff of the operation's view against views the server itself
   vouches for (`authority.rs`, `scope.rs`, `../auth.rs`).
5. Track the writer role — one workspace, one writer, claims that expire unless
   renewed (`writer.rs`).
6. Fan out head-change wake-ups over server-sent events.
7. Mint workspace-scoped bearers from the admin token.
8. Host the colocated repo, so `jj git push` / `jj git fetch` are ordinary git.
9. Put back, best effort and after the fact, a workspace pointer that a merge
   settled on an interrupted clone's placeholder (`repair.rs`).
10. Inject faults on demand, for the deterministic simulation (`faults.rs`).

The integration worker (`integration.rs`, `--enable-integration-workspace`) is
present and **parked**: continuous recompute does not survive continuous
snapshotting. See the design doc's "Explicitly parked".

### Client

The binary is `CliRunner::init().add_store_factories(tandem_factories()).run()`
for anything that is not one of tandem's own verbs.

- **`TandemBackend`** (`src/backend.rs`) — jj-lib's `Backend`:
  `read_file`/`write_file`, `read_tree`/`write_tree`, `read_commit`/
  `write_commit` → `/api/objects`.
- **`TandemOpStore`** (`src/op_store.rs`) — jj-lib's `OpStore`:
  operations and views → `/api/ops`, `/api/views`.
- **`TandemOpHeadsStore`** (`src/op_heads_store.rs`) — jj-lib's `OpHeadsStore`:
  `get_op_heads`/`update_op_heads` → `GET`/`POST /api/heads` with `ETag` and
  `If-Match`. A lost race is a `412`, and jj's transaction retry converges.
- **`DiskCache`** (`src/cache.rs`) — content-addressed, in front of every
  immutable read. Location is `TANDEM_CACHE_DIR`, else `$XDG_CACHE_HOME/tandem`,
  else `$HOME/.cache/tandem`; `TANDEM_DISABLE_CACHE=1` is the kill switch.
  There is no CLI surface, on purpose.
- **`Daemon`** (`src/daemon.rs`) — filesystem watch, debounce, snapshot,
  publish; writer-role claim and renewal; SSE subscription that marks the
  workspace stale and goes no further.
- **`repo_link`** (`src/repo_link.rs`) — the server address, workspace name and
  token that `clone` writes next to the store. `TANDEM_SERVER`,
  `TANDEM_WORKSPACE` and `TANDEM_TOKEN` override them, which is what lets one
  baked image serve several agents.

There is no checkpoint verb, and `jj workspace update-stale` is never run for
the user: moving files under whoever is editing them is a decision.

## Protocol

HTTP. `--server host:port` means `http://host:port`; an explicit `http://` or
`https://` URL works too. Every route wants `Authorization: Bearer …`, the
handshake included: a server that answered even one question to an
unauthenticated caller would be telling a stranger which repository it is
holding. A tokenless `GET /api/info` is a `401`, not a greeting.

| Endpoint | Purpose |
|----------|---------|
| `GET /api/info` | Handshake: protocol version, id lengths, root ids, capabilities |
| `GET /api/objects/{kind}/{id}` | Read one object; `kind` is `commit`, `tree`, `file`, `symlink` or `copy` |
| `POST /api/objects/{kind}` | Write one object; answers the id in `tandem-object-id` |
| `POST /api/objects:batch` | Write many in one round trip (`application/vnd.tandem.batch`) |
| `GET /api/ops/{id}`, `POST /api/ops` | Read/write operations |
| `GET /api/ops?prefix=<hex>` | Resolve an operation id prefix |
| `GET /api/views/{id}`, `POST /api/views` | Read/write views |
| `GET /api/heads` | Current heads; the CAS version is the `ETag` |
| `POST /api/heads` | Publish; requires `If-Match`, answers `412` on a lost race, acks only once the bucket has it |
| `GET /api/events` | Server-sent events; each event names a version |
| `POST /api/tokens` | Admin token mints a workspace-scoped, short-lived bearer |
| `POST /api/workspaces/{id}/writer` | Claim or renew the writer role |

Objects, operations and views are content-addressed, so their reads carry
`Cache-Control: public, max-age=31536000, immutable` — the client cache trusts
that and nothing else. Heads are the one mutable resource and carry
`Cache-Control: no-store`.

`/api/events` is a wake-up channel, never a data channel: an event names a
version and the watcher reads `/api/heads` to learn what changed. Wake-ups may
coalesce, and a watcher must survive versions it never saw.

Optional capabilities `headsSnapshot` and `getRelatedCopies` have no endpoint
yet; the server does not advertise them. There is no `repoId`: one server, one
repo.

See `src/server/http.rs` for the surface, `src/wire.rs` for the serialization,
`src/http_client.rs` for the client side.

## Storage

The bucket is anything that answers put and get: a directory, `file://…`, or
`s3://<bucket>[/<prefix>][?endpoint=…&region=…&anonymous=true]`
(`src/object_store.rs`). `--bucket` defaults to a directory inside the repo,
which is right for a laptop and wrong for anything that must outlive its
machine.

Layout (`src/wal.rs`):

- `wal/<operation id>` — one immutable entry per publish: the operation, its
  view, and every blob written since the previous publish. The blob list is
  "what arrived since", not "what this operation introduced" — the three client
  store traits hold separate connections, so there is no session to attribute
  blobs to. What is guaranteed is that every blob reachable from a published
  head is durable before that head is acknowledged.
- `index/heads.json` — the op-head set, one object, compare-and-swapped.

Conditional put is needed only for the serverless degenerate mode, where
clients CAS the index themselves. With a server in front, plain put and get
are enough.

## Git compatibility

There is no custom git layer. The server hosts a normal jj+git colocated repo,
and objects clients write are real git objects, so `jj git push` on the server
just works.

Git runs **on the server only** — `jj git fetch`, `jj git push`,
`gh pr create`. The server holds the credentials; agents never touch git and
never need to. It is the single point of contact with the outside world, which
is where decisions about what ships belong.

## Test coverage

Three homes, described at length in
[docs/design-docs/test-suite.md](docs/design-docs/test-suite.md).

| Home | Path | What lives there |
|------|------|------------------|
| Properties | `tests/properties.rs` + `tests/properties/` | Wire and WAL round-trips, and no panic or over-allocation on arbitrary bytes |
| Deterministic simulation | `tests/dst.rs` + `tests/support/` | Server and clients in one process on a seeded schedule, against an oracle, with injected faults |
| Integration | `tests/integration.rs` + `tests/integration/` | Real processes: clone, daemon, auth, cache, durability, replay, git round trip, control socket, HTTP surface, baked images |

`tests/common/` is the shared harness — server fixture, `HOME` isolation,
workspace helpers, bucket helpers. Assertions are on **file byte content**, not
on commit descriptions.

Benchmarks live in `benches/` and record under `docs/benchmarks/`; the gate
metric is snapshot→publish latency including the durability hop. See
[docs/benchmarks/README.md](docs/benchmarks/README.md) for the tiers and the
one command each of them takes.

Run: `cargo test`. For the S3 tier, point `TANDEM_TEST_S3_BUCKET` at a
SeaweedFS container — the design doc's "Validation infra" has the recipe.

## Technology choices

- **Language:** Rust
- **Binary:** one `tandem`, server and client
- **Transport:** HTTP, with server-sent events for wake-ups
- **Durable store:** an S3-compatible bucket holding a WAL and one index object
- **Server storage:** a jj+git colocated repo, rebuildable from the bucket
- **Serialization:** jj-native protobuf object/op/view bytes passed through as
  blobs; JSON for metadata; a small hand-rolled frame for batches and for the
  WAL, which outlives the wire format
- **Client CLI:** stock `jj` via `CliRunner`, not a tandem CLI
- **Dependencies:** `jj-lib`, `jj-cli`, `axum`, `reqwest`, `tokio`, `prost`,
  `notify`, `object_store`

## Project structure

```
src/
  main.rs              CLI dispatch (clap) + CliRunner passthrough
  lib.rs               store factories, shared entry points
  server/
    mod.rs             Server — jj Git backend, heads authority, lifecycle
    http.rs            HTTP surface + SSE
    authority.rs       who is asking, and whether they may
    bucket.rs          WAL, index, publish ordering, and replay at start-up
    repair.rs          put a workspace back after an interrupted clone
    scope.rs           what a workspace token may publish
    writer.rs          the writer role: claims, renewal, expiry
    integration.rs     integration workspace worker (parked)
    faults.rs          fault injection for the simulation
  wal.rs               WAL entry and index formats
  object_store.rs      bucket URLs → an object store
  auth.rs              tokens, namespaces
  wire.rs              wire types: object kinds, JSON bodies, batch codec
  backend.rs           TandemBackend (jj-lib Backend)
  op_store.rs          TandemOpStore (jj-lib OpStore)
  op_heads_store.rs    TandemOpHeadsStore (jj-lib OpHeadsStore)
  http_client.rs       the HTTP client behind the three store traits
  cache.rs             content-addressed client disk cache
  daemon.rs            fs-watch → debounce → snapshot → publish
  watch.rs             tandem watch (SSE reader)
  workspace_init.rs    tandem clone / tandem init
  repo_link.rs         server address, workspace name and token on disk
  control.rs           control socket (Unix socket, JSON lines)
  proto_convert.rs     jj protobuf ↔ Rust structs
  logging.rs, env.rs, hex.rs, placeholder.rs
docs/
  design-docs/         durable decisions; target-architecture.md is the current one
  benchmarks/          recorded numbers and the commands that make them
  images/              the sandbox image template and its recipe
```

## Non-goals

- Workflow automation engines
- Web UI / IDE integrations
- Multi-tenant isolation beyond per-workspace token scope: one server is one
  repo and one trust domain, and the token limit is a publish-scope check, not
  a tenancy boundary
- A CLI surface for the cache. Baking an image is running `tandem clone` where
  the output is kept
