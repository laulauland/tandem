# Target architecture (v2)

Decided 2026-08-18. Interactive recap with diagrams and call chains:
https://claude.ai/code/artifact/79bbe86a-6e74-4def-8f37-26b4752abdf2

This document records the target design that the staged plan (see the jj
revisions descending from the revision that introduces this file) implements.
It supersedes parts of ARCHITECTURE.md; that file is updated as stages land.

## Summary

One binary, two roles. `tandem up` runs a bearer-authenticated HTTP server.
A client is the same binary anywhere (sandbox, VM, laptop): stock jj via
`CliRunner` whose store traits call the server over HTTP through a
content-addressed disk cache. What edits the working directory — an agent, a
build, a human — is outside tandem's model: a daemon watches the filesystem
and turns changes into published jj operations.

The durable source of truth is an S3 write-ahead log, not the server's disk
(the "durability inversion", after Cursor's Continuity design,
https://cursor.com/blog/git-at-any-scale). jj's op log already is a WAL:
one publish = one immutable WAL object; the op-heads set = one CAS-updated
index object. The server's jj+git colocated repo is a disposable
materialization — kill the server, `tandem up --bucket` anywhere, it replays.

## Components

- **Server** (`tandem up --repo <path> --listen <addr> [--bucket <url>]`):
  embeds jj-lib over a colocated jj+git repo (a rebuildable cache of the
  bucket). Owns: HTTP API, op-head CAS ordering, SSE fan-out, token minting,
  writer-role tracking, git interop (`jj git push`/`fetch` run here only).
- **Bucket** (any S3-compatible store; SeaweedFS or a filesystem backend for
  dev):
  WAL entries (immutable) + one index object (conditional PUT). Plain put/get
  suffices when a server fronts it; conditional-put is required only for the
  serverless degenerate mode (clients CAS the index directly).
- **Client**: `tandem clone <server> <dir> --workspace <name>`, a per-workspace
  daemon, and stock jj commands. Daemon: fs-watch debounced auto-snapshot →
  publish, SSE subscription (staleness marking only; `update-stale` is policy),
  writer-role renewal.

## API surface

- `GET /api/objects/{kind}/{id}` — immutable, `Cache-Control: immutable`
- `POST /api/objects/{kind}`, `POST /api/objects:batch`
- `GET`/`POST /api/ops`, `/api/views`
- `GET /api/heads` (ETag) / `POST /api/heads` (If-Match CAS; ack only after
  WAL entry + index are durable in the bucket; 412 → jj transaction retry)
- `GET /api/events` — SSE; best-effort wake-up only, never the data channel
- `POST /api/workspaces`, `POST /api/workspaces/{id}/writer` — claim/renew the
  writer role; unrenewed claims expire
- `POST /api/tokens` — admin token mints workspace-scoped short-lived bearers

## Invariants

1. One workspace = one writer (server-enforced writer role, TTL + renewal).
2. Parallel actors get parallel workspaces; merge in the repo, never on disk.
3. Agents write to namespace bookmarks (`agent-a/task-42`); `main` advances
   only by an integrator rebasing a ready stack (fast-forward, linear, no
   merge commits), then the server mirrors to GitHub.
4. Token scope is enforced as a view-diff check at publish: add commits, move
   own workspace pointer, move own-namespace bookmarks — nothing else.
5. Snapshotting is filesystem-event-driven and debounced; durability window =
   debounce interval; no checkpoint verb exists.
6. Concurrent op heads are kept and merged, never last-writer-wins.
7. The op log is the audit trail (`op log` / `evolog`); any point restorable.

## Explicitly parked

- Integration worker (slices 16–17): continuous recompute does not survive
  continuous snapshotting (recompute per file-save over mid-edit states).
  Rework direction: on-demand conflict query over ready bookmarks, writable
  nowhere, runnable by any actor.
- Cache CLI surface: none. `tandem clone` warms the cache; image baking is
  running `tandem clone` at image build time. The location is environment
  only: `TANDEM_CACHE_DIR` (default `$XDG_CACHE_HOME/tandem`, else
  `$HOME/.cache/tandem`), with `TANDEM_DISABLE_CACHE=1` as the kill switch.

## Validation infra

Three tiers; each stage names the tier that gates it.

1. **`cargo test` only** (stages 1–4): the simulation runs server and clients
   in-process; the object-store trait's filesystem backend (a directory as a
   bucket) covers WAL/index semantics. No external services. See
   [The test suite](./test-suite.md).
2. **SeaweedFS container** (stages 1, 5): a real S3 API locally via
   `docker run -d -p 8333:8333 chrislusf/seaweedfs server -s3` (pin the tag;
   poll ListBuckets for readiness, ~3 s cold start; anonymous access is open
   by default). Verified 2026-08-18: put/get round trip and conditional PUT
   (`If-None-Match: *` and stale `If-Match` → 412) both work, so it also
   covers the serverless degenerate mode's index CAS. MinIO is not used
   (archived).
3. **Real bucket + real distance** (stage 6, the gate metric): server on an
   exe.dev VM (`ssh exe.dev new --name tandem-server`; clients reach it at
   `https://tandem-server.exe.xyz/`, TLS proxied), bucket on any real
   S3-compatible provider. Note: all exe.dev VMs in one account share one
   region (DAL for this account), so VM↔VM is same-datacenter; the honest
   long-haul number is laptop↔DAL. Non-interactive shells must set
   `SSH_AUTH_SOCK` to the forwarded agent socket.

## Gate metric

Snapshot→publish latency at tool-call frequency from a real sandbox provider,
including the S3 durability hop. Measured at the clone+daemon stage; the
checkpoint story depends on it.
