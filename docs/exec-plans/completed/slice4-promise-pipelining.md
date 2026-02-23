# Slice 4 — Promise pipelining

- **Date completed:** 2026-02-15
- **Test file(s):** `tests/slice4_promise_pipelining.rs`

## What was implemented

Cap'n Proto transport migration groundwork for future pipelining/batching:

1. **Cap'n Proto RPC transport**
   - RPC protocol defined in `schema/tandem.capnp`
   - Schema defined in `schema/tandem.capnp`
   - Schema codegen integrated via `capnpc` in `build.rs` with checked-in fallback `src/tandem_capnp.rs`

2. **Cap'n Proto transport foundation**
   - End-to-end transport switched to Cap'n Proto for backend/op/op-head calls
   - Write path exercised: `putObject(file) → putObject(tree) → putObject(commit) → putOperation → putView → updateOpHeads`
   - This slice established transport correctness under rapid sequential writes

3. **RPC client abstraction**
   - `TandemClient` (`src/rpc.rs`) wraps the Cap'n Proto client
   - Exposes blocking wrappers used by Backend/OpStore/OpHeadsStore trait implementations
   - True client-side promise-pipelined call chaining remains a follow-up optimization

## Acceptance coverage

Integration tests in `tests/slice4_promise_pipelining.rs` validate:

- Rapid sequential write/commit correctness across multiple objects
- Round-trip byte integrity for small and larger file sets
- All slice 1-3 behavior remains intact after Cap'n Proto transport switch

What this does **not** currently prove:

- explicit RTT reduction attributable to client-side promise pipelining
- latency-under-artificial-delay gains with asserted thresholds

## Architecture notes

Cap'n Proto remains a good fit for future pipelining/batching, but this slice should be read as a transport migration + correctness milestone, not as final evidence of pipelining latency wins.
