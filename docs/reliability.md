# WAL and reliability

This document owns Tandem's durability contract. [ARCHITECTURE.md](../ARCHITECTURE.md)
owns component boundaries; [operations.md](operations.md) owns operator
procedures.

## Durable state

The bucket is the source of truth for published work:

- Each non-root operation has one immutable WAL entry keyed by operation ID.
  The entry contains its parent IDs, operation, view, and a set of
  content-addressed records.
- One mutable index contains the versioned operation-head set and workspace
  pointers. Its update is compare-and-swap when the object store supports it.
- The server's colocated jj/Git repository and Tandem metadata sidecar are
  materializations. They may lag the bucket during a crash and must never be
  treated as more durable than it.

The WAL framing is independent of the HTTP representation because stored
history must outlive a transport implementation. Decoders reject bad magic,
truncation, trailing bytes, unknown record kinds, and hostile length claims.

## Publish ordering and acknowledgement

After the expected version and publish scope are validated, one head update
crosses these boundaries in order:

1. Ensure missing operation ancestry has WAL entries, parent before child.
2. Write the proposed operation's immutable WAL entry, including its view and
   the currently staged blobs.
3. Compute the prospective head set and commit it, with workspace pointers, to
   the bucket index using the observed index version.
4. Apply the operation-head update to jj's local op-heads store.
5. Reconcile concurrent heads through jj, repair interrupted-clone pointers,
   and write the local metadata sidecar.
6. If reconciliation derived a new operation, make that operation durable and
   best-effort mirror the settled head set into the index.
7. Notify watchers and acknowledge the request.

Steps 1–3 are the durable commit. Step 4 is the last operation allowed to fail
the request. Once the operation is durable and locally applied, later cleanup
must degrade to a state already known to be durable; returning an error then
would make the client repeat an operation that already landed and could create
divergent commits for one change ID.

An acknowledgement promises that every object reachable from the acknowledged
head set can be recovered from the bucket. It does not promise that every
object ever uploaded has been published: uploads staged in memory belong to no
successful operation until a head update carries them.

## Blob attribution

Backend objects, operations, and heads travel through separate jj store
connections, so the server has no trustworthy per-client session with which to
say which upload belongs to which operation. A WAL entry therefore carries
"objects received since the last publish," not "objects introduced by this
operation."

This loose attribution is intentional and safe because objects are
content-addressed. Under concurrency one agent's entry may carry another
agent's not-yet-published blob; replaying or storing it twice is harmless. The
required relationship is reachability before acknowledgement, not one-to-one
ownership.

The staging buffer deduplicates IDs, warns as it grows, and eventually applies
backpressure rather than risking an unbounded server process. When a WAL write
fails, drained objects are restored ahead of newer uploads. When a publish is
retried after its immutable entry already exists, newly staged objects remain
for the next publish instead of being discarded into an entry that cannot be
overwritten.

## CAS and concurrent writers

The client publishes against the version it read. A stale version or bucket
index conflict is a normal concurrency result, not storage failure: the server
adopts any newer durable index it can replay and reports the current state. The
client op-heads adapter then retries the same operation against the refreshed
version; it does not rerun the jj transaction. No writer may replace the head
set with only its own head.

Head reads never reconcile. Reconciliation during a read previously moved the
version inside a client command, causing a needless transaction retry and the
possibility of divergent change IDs. Mutation is confined to the write path.

## Crash windows

| Stop point | Durable state | Recovery consequence |
| --- | --- | --- |
| Before WAL write | Nothing from this attempt | Client may retry the whole transaction |
| After WAL write | Entry exists; index still names old heads | Unreferenced entry is harmless; retry reuses it |
| After index write | Bucket names the new heads; local repo may not | Startup replays the index before serving |
| After local apply | Local jj heads moved; sidecar may lag | Startup adopts the bucket version and repairs metadata |
| After metadata write | Publish landed; derived reconciliation may be absent | The indexed head set remains valid and the next publish can settle it |

The deterministic simulation exercises every boundary. Its closing phase
destroys the server materialization and verifies the same bytes from the bucket
alone, which prevents a locally readable but non-durable object from hiding.

## Replay and startup recovery

Startup reads the local metadata and bucket index before serving:

- On a cold materialization, it walks every indexed head's ancestry, applies
  parents before children, retires only the synthetic init heads created by
  that boot, then records the bucket version.
- On a warm restart with a newer bucket version, it replays only the missing
  ancestry.
- If local metadata is newer than the bucket index, it republishes local heads.
  This supports a pre-index crash or a newly attached bucket, but it is not a
  full historical backfill for an existing repository.
- Replay writes are idempotent: stored content is addressed by hash, operations
  have stable IDs, and making an existing operation a head is a no-op.

An index version is adopted only after every named head has landed locally.
Adopting a version without its heads would allow the next writer to pass CAS
while silently dropping history.

## Recovery limits and future retention

- The bucket must retain the WAL objects and index together. Copying only the
  index creates unreplayable heads; copying only WAL entries loses the current
  authority.
- Pointing an established repo at an empty bucket does not backfill its entire
  history. Migrate or seed durable storage with a separately designed and
  verified procedure.
- Workspace uploads that have not reached a successful publish live only in
  the server process and remain the client's responsibility to retry.
- Corrupt or missing WAL ancestry is a hard recovery failure. Preserve the
  bucket and logs before attempting repair.
- No compaction or garbage collection is implemented. A future design must
  define roots, workspace and bookmark retention, in-flight writers, object
  reachability, tombstones, concurrent index changes, restore horizons, and a
  verifiable mark-before-sweep protocol. Bucket lifecycle expiration is unsafe
  until that design exists.

The executable evidence for these claims is routed in
[testing.md](testing.md#reliability-coverage).
