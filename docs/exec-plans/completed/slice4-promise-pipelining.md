# Slice 4 — Promise pipelining (v1)

- **Date completed:** 2026-02-15
- **Test file(s):** `tests/slice4_promise_pipelining.rs`

## What was implemented

Cap'n Proto promise pipelining for efficient multi-object writes:

1. **Cap'n Proto RPC migration**
   - Replaced v0's line-JSON transport with Cap'n Proto
   - Schema defined in `schema/tandem.capnp`
   - Build integration via `build.rs` and `capnpc` crate

2. **Promise pipelining support**
   - Cap'n Proto automatically pipelines dependent RPC calls
   - Write sequence: putObject(file) → putObject(tree) → putObject(commit) → putOperation → putView → updateOpHeads
   - All calls pipeline without waiting for individual responses
   - Only final `updateOpHeads` blocks for result

3. **RPC client abstraction**
   - `TandemClient` (src/rpc.rs) wraps Cap'n Proto client
   - Provides async methods matching `Store` capability
   - Used by Backend/OpStore/OpHeadsStore trait implementations

## Acceptance coverage

Integration test `promise_pipelining_efficiency` validates:

- Rapid sequential writes complete in fewer RTTs than sequential calls
- Latency benchmark under artificial delay proves pipelining
- All slice 1-3 tests still pass with Cap'n Proto transport

## Architecture notes

Cap'n Proto was chosen for its promise pipelining capability, which reduces latency for dependent write sequences. This is critical for good UX when every Backend/OpStore call is a network round-trip.
