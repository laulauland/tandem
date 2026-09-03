# Deployment and recovery

This document owns operator procedures. Use `tandem --help` for current flags,
[ARCHITECTURE.md](../ARCHITECTURE.md) for boundaries, and
[reliability.md](reliability.md) for why the recovery order is safe.

## Production shape

Run one Tandem server per repository on a persistent VM or container host. The
server directory is a colocated jj/Git materialization used for Git interop;
the bucket is the durable record of published work.

Before admitting clients:

1. Create and access-test a dedicated S3-compatible bucket or prefix. Do not
   apply object-expiration rules.
2. Initialize or restore the server repository and configure its Git remote and
   credentials locally.
3. Supply a stable admin token through protected environment or service-secret
   storage. Do not put it in shell history, process arguments, images, or logs.
4. Bind to a private interface, use a VPN/tunnel, or terminate TLS at a reverse
   proxy. Tandem does not encrypt HTTP itself.
5. Run `tandem serve` under a service manager for supervised production use;
   `tandem up` is suitable for a user-managed background process.
6. Verify status, inspect startup replay metrics, then perform a byte-level
   publish/read smoke test before distributing workspace credentials.

The bucket must already exist. A filesystem bucket is useful for development,
but placing it inside the server repo means one disk failure loses both the
materialization and its supposed backup.

## Workspace access

Give each active agent a unique workspace identity. `tandem clone` accepts the
admin token and exchanges it for a scoped token, or accepts an already-scoped
token. For explicit minting:

```bash
printf 'header = "Authorization: Bearer %s"\n' "$TANDEM_ADMIN_TOKEN" | \
  curl --config - --fail --silent --show-error https://tandem.example/api/tokens \
  -H 'content-type: application/json' \
  --data '{"workspaceId":"agent-a","ttlSeconds":3600}'
```

Reading the authorization header from standard input keeps the expanded token
out of the curl process arguments. Do not wrap this command in evidence capture;
its response contains the new workspace token.

Workspace tokens expire and are not refreshable in place. There is no
individual revocation; rotate the admin token to invalidate all tokens minted
from it. Treat the admin token as repository-wide authority.

One workspace identity has one writer lease. Starting two daemons for the same
identity does not create parallelism; the second cannot publish until the lease
expires. Use distinct identities for distinct agents.

## Observability

- `tandem server status --json` is the machine-readable liveness and startup
  replay surface.
- `tandem server logs --json` streams structured daemon events. Persist service
  logs and apply retention outside Tandem.
- Run the server at normal verbosity and raise the streaming filter during an
  incident. Never enable ad-hoc token or request-body logging.
- Alert on repeated publish failures, index conflicts that do not settle,
  writer-lease churn, restart loops, storage errors, disk pressure, and an
  unexpectedly large cold replay.

The local control socket is unauthenticated by design and depends on filesystem
permissions. Do not expose it through a network proxy.

## Backups

Back up or replicate the bucket with versioning where available. A consistent
recovery point needs both the head index and every WAL object reachable from
it. Because publishes write WAL before the index, copying an index before its
referenced entries is unsafe; prefer object-store versioning/snapshots or stop
publishes while taking a manual copy.

Also preserve the server's operational state separately: Git remote
configuration, deploy credentials, service definition, admin-token secret,
and TLS/proxy configuration. The server repository itself accelerates restart
but is not the durable backup.

Periodically restore into an isolated empty directory and verify operation-log
walks plus exact file bytes. A backup that has not been replayed is untested.

## Recover a lost server disk

1. Stop or fence the old server so only one process can accept publishes.
2. Preserve logs and identify the exact bucket/prefix and admin-token secret.
3. Start Tandem against an empty directory and the existing bucket. Do not
   initialize a different history into that directory first.
4. Wait for startup replay to finish; do not admit clients during repeated
   startup failures.
5. Verify status and logs, walk the jj operation log, and read known files by
   revision to confirm byte identity.
6. Reconfigure the Git remote and credentials if they were lost with the disk,
   then fetch/push only after comparing the materialized state with upstream.
7. Admit a disposable workspace, publish a uniquely named file, restart once,
   and read those bytes back before reopening normal traffic.

Replay is incremental on a warm disk and reconstructive on an empty one. It is
idempotent, so retrying startup after correcting an external storage or
credential problem is safe.

## Respond to suspected bucket damage

Stop writers first. Do not delete, rename, compact, or overwrite objects while
diagnosing. Snapshot/version the affected prefix and retain server logs and the
local materialization.

- A missing index loses the authoritative current head set even if WAL entries
  remain.
- An index that names a missing or corrupt WAL entry cannot be safely replayed.
- A local repo ahead of an empty bucket is not a supported full-history
  backfill path.

Restore a mutually consistent index and WAL set into a new prefix, then perform
the lost-disk procedure against that prefix. Keep the original evidence until
the recovered repository passes byte-level verification and upstream Git
comparison.

## Routine upgrades and shutdown

Drain or pause workspace daemons, stop the server gracefully, retain the
current binary and configuration for rollback, deploy the new binary, and
watch startup replay before resuming writers. `tandem down` and service-manager
termination both request graceful shutdown; an unclean death must still
preserve every acknowledged publish.

For cross-machine acceptance and disaster-recovery drills, use the repo-local
`distributed-smoke` skill. For releases, use the `release` skill. Both require
an explicit review before externally mutating production or upstream state.
