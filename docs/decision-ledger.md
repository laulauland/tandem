# Decision and evidence ledger

This is a compact historical ledger. It preserves why the current system looks
the way it does and which risks remain; it is not a command reference or a
source for current implementation detail. Current behavior is owned by
[ARCHITECTURE.md](../ARCHITECTURE.md), [reliability.md](reliability.md), and
[operations.md](operations.md).

Status meanings: **active** still governs the design; **superseded** records a
rejected or replaced direction; **historical evidence** records an observation
that may no longer describe current performance.

## Active decisions

| Decision | Why it remains | Evidence / consequence |
| --- | --- | --- |
| Stock jj client over remote store traits | Preserves jj semantics and avoids a second VCS command language | Product tests exercise ordinary jj commands through `tandem` |
| jj op-heads are the live authority | A parallel Tandem head database drifted from jj under retries | Head API and jj authority are compared by the DST oracle |
| Bucket durability inversion | A long-running server machine must be replaceable | Cold replay tests destroy the repo and recover exact bytes from WAL/index |
| WAL before index before local apply | Makes every acknowledged head replayable and lets crashes leave durable state ahead of cache | All five publish gaps are deterministic fault points |
| Head reads are read-only | Reconciliation during reads moved versions inside transactions and could diverge change IDs | Reconciliation is confined to publish; integration coverage pins passive reads |
| Loose blob attribution | jj store writes arrive on independent connections, so per-operation ownership cannot be inferred safely | Content addressing makes cross-publish carriage and duplicate storage harmless |
| One writer lease per workspace | A workspace identity represents one mutable filesystem history | Parallel actors use distinct names; leases expire if not renewed |
| Staleness is signaled, not auto-applied | Updating a stale workspace moves files under an editor | Daemons stop at marking stale; image boot may update before editing starts |
| Workspace scope is a server-side view diff | Client conventions cannot be an authorization boundary | Scoped tokens can advance only their workspace and namespace |
| Git is server-only | Centralizes credentials and the decision to ship | The materialized repo remains a normal colocated jj/Git repository |
| Cache has no management CLI | Clone and catch-up already create the reusable artifact | Image baking preserves the cache and operation index |

## Superseded directions

- Early prototypes used line-oriented messages and later a schema-driven RPC
  transport. HTTP plus server-sent events replaced both. The old transport
  protocol, connector matrix, generated-schema workflow, and compatibility
  plan are intentionally removed; only the transport-independent error and
  durability lessons survived in current owners.
- A separate Tandem head authority was rejected in favor of jj's op-heads
  store. The metadata file remains only for version and workspace-pointer
  materialization.
- Automatic server-side integration was removed after being parked. Its worker
  published local operations outside the WAL commit protocol, and recomputing
  on each file-save snapshot merged mid-edit states. Explicit integration uses
  ordinary jj commands. A future conflict query should be on-demand and
  read-only over work that actors have explicitly marked ready.
- Large suites of one-scenario subprocess tests were consolidated. Pure codecs
  moved to properties; schedule-dependent correctness moved to the DST; only
  behavior that needs a real process, socket, filesystem, S3 API, or Git remains
  integration coverage.
- A custom checkpoint command was rejected. Filesystem events trigger
  publishing, and the debounce interval is the only unacknowledged window.

## Historical evidence retained

- Initial agent QA found that missing local help caused roughly half of a
  naive session to be spent guessing commands. This established offline help,
  useful unknown-command suggestions, explicit missing arguments, and
  address-bearing connection errors as correctness requirements.
- Early workflow QA proved concurrent visibility and CAS convergence but
  exposed that descriptions alone could not support code review. That led to
  real file/tree storage and the byte-level acceptance invariant.
- Early stress runs preserved data at five and ten agents but observed resource
  failures around twenty agents. Treat this as historical evidence, not a
  current capacity promise; repeat the distributed smoke procedure on the
  intended deployment before assigning a limit.
- Container-based cross-machine QA confirmed independent workspaces, shared
  history, byte reads, restart persistence, and server-side Git compatibility.
  It also exposed runtime-library portability and stale-workspace ergonomics,
  motivating image baking and boot-time catch-up.
- Recorded latency measurements show that remote publish cost is dominated by
  serialized round trips rather than the local bucket write. The retained
  artifacts and reproducible methods live in
  [benchmarks/README.md](benchmarks/README.md).
- The Stage 7 replacement drill first exposed a real interrupted-recovery bug:
  a killed cold materialization forgot its disposable jj initializer head and
  later reconciled that local-only commit into durable history. The failed run
  and exported bucket/cache forensic set are retained as
  `stage7-failed-89389ca372` and `stage7-forensics`. After persisting the
  initializer identity, run `539ebd095c` replaced an exe.dev VM, killed
  recovery after one applied WAL entry, resumed from the partial cache, rotated
  from an active K1 signing key to active K2 with K1 retained, rolled back, and
  promoted the replacement again. After the supervised restart and two further
  publishes, all 11 acknowledged operations were reachable and their exact
  file bytes were verified. The reviewed source was
  `ddc772196466d11f136fbbc0552ad1bbdda35a13`, and the GNU/Linux binary SHA-256
  was `0e9165cdc5ab6a7b470d4a401f80e1b5905e4d66158abdebd5daa404b4ed64ab`.
  The sanitized records are `stage7-replacement.json`,
  `stage7-infrastructure.json`, and `stage7-local-review.json` in the retained
  qualification state. Replacement process health took 3.638 seconds; the
  first authenticated known-repository request took 18.898 seconds, while the
  complete private qualification took 138.940 seconds. Those measurements are
  separate from total traffic-transition downtime and come from one drill.

## Unresolved risks and follow-up

- No built-in TLS; bearer secrecy depends on private networking, a tunnel, or a
  TLS proxy.
- Workspace tokens have expiry but no individual revocation or automatic
  refresh. Admin-token rotation invalidates the whole trust domain.
- An established repository cannot yet be fully backfilled into an empty
  bucket. Attaching durable storage after history exists needs a separate,
  verifiable migration procedure.
- WAL retention is unbounded. Compaction and garbage collection require an
  explicit reachability and restore-horizon design.
- The pre-publish blob buffer is process memory. Its cap supplies backpressure,
  but objects not attached to an acknowledged operation are not recoverable
  after process loss.
- Conflict inspection has no on-demand query yet.
- Publish latency over real distance remains sensitive to request depth.
- Capacity and recovery must be requalified against the actual object store,
  proxy, latency, and agent count before production use.

Resolved slice checklists, raw QA transcripts, and obsolete transport plans
were removed after their durable lessons were assigned to the owners above.
History remains available through jj when forensic detail is needed.
