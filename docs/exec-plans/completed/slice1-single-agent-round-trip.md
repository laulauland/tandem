# Slice 1 — Single-agent round-trip

- **Date completed:** 2026-02-15
- **Test file(s):** `tests/slice1_single_agent_round_trip.rs`

## What was implemented

Implemented full jj-lib Backend integration with Cap'n Proto RPC:

1. **jj-lib trait implementations**
   - `TandemBackend` (src/backend.rs) — implements jj-lib's `Backend` trait
   - `TandemOpStore` (src/op_store.rs) — implements jj-lib's `OpStore` trait
   - `TandemOpHeadsStore` (src/op_heads_store.rs) — implements jj-lib's `OpHeadsStore` trait
   - All traits route to Cap'n Proto RPC calls to tandem server

2. **Stock jj integration**
   - `tandem` binary is `CliRunner::init().add_store_factories(tandem_factories()).run()`
   - All stock jj commands work: `tandem log`, `tandem new`, `tandem diff`, `tandem file show`, etc.
   - No custom tandem CLI commands beyond `serve`, `init`, and `watch`

3. **Cap'n Proto RPC**
   - Schema defined in `schema/tandem.capnp`
   - Server implements `Store` service (src/server.rs)
   - Client connects via TandemClient (src/rpc.rs)
   - Methods: getObject, putObject, getOperation, putOperation, getView, putView, getHeads, updateOpHeads

4. **Server storage via jj Git backend**
   - Server embeds jj-lib and uses Git backend for storage
   - Objects are real jj-compatible blobs (commit/tree/file protobuf)
   - No custom object encoding — jj protobuf passed through as bytes

5. **File round-trip with byte-level assertions**
   - Tests write files, commit via `tandem new`, read back via `tandem file show`
   - Assertions verify exact byte content, not just descriptions

## Acceptance coverage

Integration test `single_agent_round_trip` validates:

- Agent writes `hello.txt` with known content
- `tandem new -m "add hello"` commits file
- `tandem file show -r @- hello.txt` returns exact bytes
- Server restart: file still readable
- Server-side `jj file show` returns same bytes

## Architecture notes

This slice established the core architecture:
- Client is stock jj with remote Backend/OpStore/OpHeadsStore
- Server is a normal jj+git repo accessed via RPC
- No command proxying — all operations are store-level RPC calls
