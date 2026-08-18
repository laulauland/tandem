# Design Docs Index

This folder holds stable technical decisions.

## Current docs

- [Target architecture (v2)](./target-architecture.md) — the current design;
  its invariants bind everything else here
- [Core beliefs](./core-beliefs.md)
- [Workflow](./workflow.md) — how work moves: clone, daemon, publish, ship
- [RPC error model](./rpc-error-model.md)
- [Server lifecycle](./server-lifecycle.md) — `tandem up/down` + `tandem server status/logs`, daemon management
- [The test suite](./test-suite.md) — the three homes (DST, properties, integration), where each old test went, and where a new one belongs

## Superseded

Kept for the reasoning in them, not for the facts. Each describes the Cap'n
Proto transport that stage 3 replaced with HTTP + SSE; where they disagree with
[the target architecture](./target-architecture.md), the target architecture
is right.

- [jj-lib integration](./jj-lib-integration.md) — the trait-by-trait analysis
  still holds; the calls it says go over Cap'n Proto go over HTTP
- [RPC protocol](./rpc-protocol.md) — replaced by the API surface in
  [ARCHITECTURE.md](../../ARCHITECTURE.md)
- [Transport matrix](./transport-matrix.md) — written when the transport was
  raw TCP, which is what made sandbox reachability a question at all

## Add a new design doc when

- a decision affects correctness or compatibility
- a decision changes protocol or storage format
- a decision introduces operational tradeoffs
