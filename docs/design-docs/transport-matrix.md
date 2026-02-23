# Transport Matrix & Sandbox Compatibility

This doc tracks tandem transport compatibility for VM sandbox runtimes that
restrict network egress (HTTP-only, WebSocket-only, or SSH-exec-only).

## Why this matters

Several sandboxed agent runtimes and hosted VM systems do not allow arbitrary
outbound TCP sockets. Tandem's current transport is Cap'n Proto over raw TCP,
which can fail in those environments even when normal web access works.

## Current state (v0.3.2)

- **Protocol semantics:** Cap'n Proto `Store` service (`schema/tandem.capnp`)
- **Implemented transport:** raw TCP (`host:port`)
- **Operational workaround:** SSH tunneling / bastion forwarding

## Target transport set

1. **`tcp://host:port`** (keep)
   - Best latency and simplest deployment in trusted networks.

2. **`wss://...`** (add)
   - Priority for HTTP(S)/WebSocket-only egress environments.
   - Should carry same RPC semantics and error model.

3. **`ssh-exec://user@host`** (add)
   - Priority for environments that allow SSH exec but block arbitrary TCP.
   - Suggested shape: stdio-based RPC tunnel (`ssh host tandem rpc-stdio ...`).

## Correctness invariants (must not change across transports)

- `updateOpHeads(expectedVersion, ...)` is the serial order boundary.
- object/op/view writes are idempotent and content-addressed.
- no success responses before durable writes.
- watch reconnect is safe with `afterVersion` + `getHeads()` catch-up.

## Phased plan

### Phase 1 — transport abstraction

- Refactor client connection setup behind a stream connector interface.
- Keep existing TCP behavior as default.
- Add transport-specific error categories without changing RPC semantics.

### Phase 2 — WSS binding

- Add WSS endpoint (native server path or sidecar/proxy mode).
- Run existing concurrency/CAS/watch tests over WSS path.
- Add reconnect/backoff defaults for unstable links.

### Phase 3 — SSH-exec binding

- Add `rpc-stdio` server mode suitable for SSH exec.
- Add client URL/flag support for SSH-exec transport.
- Validate `watchHeads` long-lived subscriptions over SSH-exec sessions.

## Operator guidance (today)

Until WSS/SSH-exec support lands:

- Prefer direct TCP in trusted networks.
- For restricted networks, use SSH tunnel/bastion forwarding.
- Assume no auth/TLS on native tandem port; rely on network isolation.

## Doc touchpoints

When transport support changes, update:

- `README.md` (user-facing deployment guidance)
- `ARCHITECTURE.md` (high-level transport model)
- `docs/design-docs/rpc-protocol.md` (binding + invariants)
- `docs/design-docs/rpc-error-model.md` (retry/error semantics)
- `docs/exec-plans/tech-debt-tracker.md` (status)
