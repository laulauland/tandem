# RPC Protocol (Cap'n Proto)

This document defines Tandem’s wire protocol and storage data model for `jj-lib` compatibility.

Error semantics are defined in `rpc-error-model.md`.

## Goals

- Map cleanly to `jj_lib::backend::Backend`, `OpStore`, and `OpHeadsStore`.
- Preserve jj’s operation/view model and multi-workspace visibility.
- Keep the server authoritative for shared state while clients keep local working copies.
- Support low-latency reads and push-based head updates.

## Repository scope

- One Tandem server serves one repo.
- No `repoId` is sent in requests.
- Run multiple servers for multiple repos.

## Compatibility contract

Clients must call `getRepoInfo()` on connect and verify:

- protocol version compatibility
- jj object/op/view format compatibility
- expected ID lengths and root IDs

If incompatible, client should fail fast with a clear error.

## Data model

### Backend object kinds

- `commit`
- `tree`
- `file`
- `symlink`
- `copy`

### Op-store objects

- `operation`
- `view`

### Head state

- Current op-head set is stored server-side.
- Head updates are linearizable via compare-and-swap semantics.
- A monotonic `headsVersion` is incremented on successful head updates.

## Cap'n Proto interface (shape)

```capnp
interface Store {
  getRepoInfo @0 () -> (info :RepoInfo);

  getObject @1 (kind :ObjectKind, id :Data) -> (data :Data);
  putObject @2 (kind :ObjectKind, data :Data) -> (id :Data, normalizedData :Data);

  getOperation @3 (id :Data) -> (data :Data);
  putOperation @4 (data :Data) -> (id :Data);

  getView @5 (id :Data) -> (data :Data);
  putView @6 (data :Data) -> (id :Data);

  resolveOperationIdPrefix @7 (hexPrefix :Text)
    -> (resolution :PrefixResolution, match :Data);

  getHeads @8 () -> (heads :List(Data), version :UInt64);
  updateOpHeads @9 (
    oldIds :List(Data),
    newId :Data,
    expectedVersion :UInt64
  ) -> (ok :Bool, heads :List(Data), version :UInt64);

  watchHeads @10 (watcher :HeadWatcher, afterVersion :UInt64)
    -> (cancel :Cancel);

  getHeadsSnapshot @11 () -> (
    heads :List(Data),
    version :UInt64,
    operations :List(IdBytes),
    views :List(IdBytes)
  );

  # Optional copy-tracking support (capability-gated)
  getRelatedCopies @12 (copyId :Data) -> (copies :List(Data));
}

interface HeadWatcher {
  notify @0 (version :UInt64, heads :List(Data)) -> ();
}

interface Cancel {
  cancel @0 () -> ();
}

struct IdBytes {
  id @0 :Data;
  data @1 :Data;
}

enum ObjectKind {
  commit @0;
  tree @1;
  file @2;
  symlink @3;
  copy @4;
}

enum PrefixResolution {
  noMatch @0;
  singleMatch @1;
  ambiguous @2;
}

struct RepoInfo {
  protocolMajor @0 :UInt16;
  protocolMinor @1 :UInt16;
  jjVersion @2 :Text;

  backendName @3 :Text;
  opStoreName @4 :Text;

  commitIdLength @5 :UInt16;
  changeIdLength @6 :UInt16;

  rootCommitId @7 :Data;
  rootChangeId @8 :Data;
  emptyTreeId @9 :Data;
  rootOperationId @10 :Data;

  capabilities @11 :List(Capability);
}

enum Capability {
  watchHeads @0;
  headsSnapshot @1;
  copyTracking @2;
}
```

## Method semantics

### `putObject`

- Server computes canonical object ID from bytes.
- Response returns canonical ID.
- Writes are idempotent (same object bytes => same ID).
- `normalizedData` allows commit write normalization; for non-commit objects it may equal input bytes.

### `putOperation` / `putView`

- Server computes IDs using jj-compatible content hashing.
- IDs and bytes must remain byte-compatible with jj expectations.

### `updateOpHeads`

- Logical behavior: remove `oldIds`, add `newId`.
- `ok=false` means caller must read current heads and retry merge/update flow.
- This operation is the concurrency correctness boundary.

### `watchHeads`

- Notifications are monotonic by `version`.
- Delivery is at-least-once and may coalesce rapid updates.
- On reconnect, client resubscribes with `afterVersion` and/or calls `getHeads()` to catch up.

### `getHeadsSnapshot`

- Fast path for dependent read chains (`heads -> operations -> views`).
- Returns a consistent snapshot tied to one `version`.

## Mapping to `jj-lib`

### Backend

- `read_*` -> `getObject(kind, id)`
- `write_*` -> `putObject(kind, data)`
- `get_related_copies` -> `getRelatedCopies` (when `copyTracking` capability exists)

### OpStore

- `read_operation` -> `getOperation`
- `write_operation` -> `putOperation`
- `read_view` -> `getView`
- `write_view` -> `putView`
- `resolve_operation_id_prefix` -> `resolveOperationIdPrefix`

### OpHeadsStore

- `get_op_heads` -> `getHeads`
- `update_op_heads` -> `updateOpHeads`
- `lock` -> client-local no-op lock (correctness comes from server-side CAS)

## Operational invariants

- `wc_commit_ids` in views is preserved exactly (workspace visibility model).
- Non-root operations must keep valid parent links.
- Head updates are durable before success responses.
- Object reads/writes must not require any client-side global cache for correctness.
