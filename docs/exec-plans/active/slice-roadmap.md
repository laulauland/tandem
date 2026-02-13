# Active Execution Plan: Slice Roadmap

Canonical vertical-slice execution plan.

## Slice 1 — Single-agent round-trip

Goal: one client reads/writes via remote server and persists state.

Acceptance:
- `tandem log/new/describe/diff` work
- restarting client preserves state
- server-side `jj log` matches

## Slice 2 — Two-agent visibility

Goal: two workspaces on different machines see each other.

Acceptance:
- agent A and B both see each other's commits and workspaces

## Slice 3 — Concurrent convergence

Goal: concurrent writes do not lose data.

Acceptance:
- both (or all) concurrent commits survive after CAS contention

## Slice 4 — Promise pipelining

Goal: dependent reads avoid additive RTT cost.

Acceptance:
- latency benchmark proves pipelining behavior under artificial RPC delay

## Slice 5 — WatchHeads

Goal: clients receive head updates without polling.

Acceptance:
- callback receives updates quickly
- reconnect path catches up

## Slice 6 — Git round-trip

Goal: GitHub <-> server repo <-> clients round-trip via stock `jj git`.

Acceptance:
- fetch and push are successful with expected history/diff

## Slice 7 — End-to-end multi-agent

Goal: integrated real-repo workflow.

Acceptance:
- two agents collaborate concurrently and ship via server-side `jj git push`
