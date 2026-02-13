# ARCHITECTURE

`Tandem` = jj workspaces over the network.

## Shape

Single binary, two modes:

- `tandem serve --listen <addr> --repo <path>`
- `tandem <jj command...>` (client mode)

## Core model

- Server hosts a **normal jj+git colocated repo**.
- Client keeps **working copy local**.
- Client store calls are remote via Cap'n Proto.
- Clients always read heads from server, so no `workspace update-stale` model.

## Responsibilities

### Server

1. Read/write jj backend + op-store objects (commit/tree/file/symlink/copy/operation/view)
2. Coordinate op heads with atomic compare-and-swap
3. Notify watchers on head changes (`watchHeads`)

### Client

Implements jj traits as RPC stubs:

- `Backend`
- `OpStore`
- `OpHeadsStore`

On CAS failure, client retries using jj’s existing merge flow.

## Protocol

Cap'n Proto `Store` service (see `docs/design-docs/rpc-protocol.md` for the canonical schema).

Core capabilities:

- object read/write for backend + op-store data
- op head reads + atomic updates
- operation-prefix resolution
- head watch subscriptions
- optional snapshot/copy-tracking capabilities

No `repoId` in protocol: one server = one repo.

## Git compatibility

No custom git layer in tandem.

Git interop happens on server-hosted repo with stock `jj` commands:

- `jj git fetch`
- `jj git push`

## Dependency graph

- Slice 1 (round-trip)
  - enables Slice 2 (multi-agent)
    - enables Slice 3 (concurrent merge)
  - enables Slice 4 (pipelining)
  - enables Slice 5 (watchHeads)
  - enables Slice 6 (git round-trip)
- Slice 7 integrates slices 1-6

Critical path: **1 → 2 → 6 → 7**.

## Technology choices

- Language: Rust
- Binary: single `tandem`
- RPC: Cap'n Proto (for promise pipelining)
- Server storage: normal jj+git colocated repo
- Serialization: jj-compatible object/op/view bytes

## Non-goals (v0.1)

- auth / ACL / multi-tenant isolation
- workflow automation engines
- web UI / IDE integrations
- client-side caching
