# Design Docs Index

This folder holds stable technical decisions.

## Current docs

- [Core beliefs](./core-beliefs.md)
- [Workflow](./workflow.md)
- [jj-lib integration](./jj-lib-integration.md)
- [RPC protocol](./rpc-protocol.md)
- [RPC error model](./rpc-error-model.md)
- [Server lifecycle](./server-lifecycle.md) — `tandem up/down` + `tandem server status/logs`, daemon management
- [Transport matrix](./transport-matrix.md) — transport compatibility (TCP/WSS/SSH-exec) and sandbox guidance
- [The test suite](./test-suite.md) — the three homes (DST, properties, integration), where each old test went, and where a new one belongs

## Add a new design doc when

- a decision affects correctness or compatibility
- a decision changes protocol or storage format
- a decision introduces operational tradeoffs
