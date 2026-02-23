# RPC Error Model

This document defines Tandem’s error contract between client and server.

## Goals

- Keep failures mappable to `jj-lib` error types.
- Separate transport failures from domain/storage failures.
- Make retries safe and predictable.
- Keep wire semantics stable for integration tests.

## Error classes

### 1) Transport/session errors

Examples:

- connection refused/reset
- timeout
- stream canceled
- server unavailable

Behavior:

- surfaced by RPC runtime (not domain payload)
- usually retriable for reads
- writes may be retried only if idempotency is guaranteed

Transport-binding note:

- TCP/WSS/SSH-exec connectors should normalize disconnect/timeout/failure into
  this same transport error class so retry policy stays consistent across
  transports.

### 2) Domain/storage errors

Returned by server logic for request-specific failures.

Canonical codes:

- `not_found`
- `invalid_id_length`
- `invalid_data`
- `unsupported`
- `permission_denied` (reserved for future auth)
- `internal`

### 3) Concurrency outcomes (not errors)

- `updateOpHeads(...)->ok=false` is **not** an error.
- It represents normal CAS contention and triggers jj merge/retry flow.

## Error envelope (application-level)

When a method needs structured failures beyond generic exceptions, use:

```text
RpcError {
  code: <canonical code>,
  message: <human-readable>,
  retriable: <bool>,
  details: {
    object_type?: <string>,
    hash?: <hex>,
    op_id?: <hex>,
    expected_len?: <u32>,
    actual_len?: <u32>
  }
}
```

Notes:

- Do not put secrets/tokens in `message` or `details`.
- `message` is for operators; clients should branch on `code`.

## Mapping to `jj-lib`

### Backend mapping

- `not_found` + object context -> `BackendError::ObjectNotFound`
- `invalid_id_length` -> `BackendError::InvalidHashLength`
- invalid UTF-8 payloads -> `BackendError::InvalidUtf8`
- read failures -> `BackendError::ReadObject` / `ReadFile`
- write failures -> `BackendError::WriteObject`
- unsupported feature -> `BackendError::Unsupported`
- anything else -> `BackendError::Other`

### OpStore mapping

- `not_found` -> `OpStoreError::ObjectNotFound`
- read failures -> `OpStoreError::ReadObject`
- write failures -> `OpStoreError::WriteObject`
- other -> `OpStoreError::Other`

### OpHeadsStore mapping

- get/list failures -> `OpHeadsStoreError::Read`
- update failures (excluding CAS miss) -> `OpHeadsStoreError::Write`
- lock failures -> `OpHeadsStoreError::Lock`

CAS miss path:

- represented by `ok=false` in `updateOpHeads`
- should not be converted to `OpHeadsStoreError`

## Retry policy

### Safe to retry automatically

- pure reads (`getObject`, `getOperation`, `getView`, `getHeads`)
- `watchHeads` subscribe after disconnect

### Retry with care

- writes only if idempotent by content-addressed semantics
- if write acknowledgment is unknown, client may re-issue same write bytes

### Do not blind-retry

- `invalid_data`, `invalid_id_length`, `unsupported`

## Observability requirements

Log on both client and server:

- `rpc.method`
- `rpc.error.code`
- `retriable`
- `attempt`
- `latency_ms`
- object/op identifiers (short hash prefix)

This allows debugging retries/concurrency without ad-hoc logging.
