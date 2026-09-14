# Native hosting on exe.dev

Proposed target, recorded on 2026-09-14. This is a plan, not a claim about the
running service. The empty descendant jj revisions own stage acceptance and
verification. Current behavior remains owned by [architecture](../ARCHITECTURE.md),
[reliability](reliability.md), [testing](testing.md), and
[operations](operations.md); update those owners as stages land.

## Target

Daytona agents and Mac mini agents keep local td workspaces. They reach
`tandem.land` through the exe.dev HTTPS proxy. Cloudflare retains DNS and R2;
the Rust host replaces the Worker and Durable Objects.

One supervised Rust process serves the website, installer, hosting API, and
repository API. Its hosting layer owns namespace/credential decisions and
repository creation. A repository manager embeds the existing native jj engine
with independent caches, publish coordination, leases, and event streams per
repository. No subprocess per repository and no custom equivalents of jj commands.

R2 owns namespace ownership, repository catalog, immutable published content,
WAL, and the durable head index. Local jj repositories and metadata are disposable
caches. The server's jj op-heads store remains the live head authority; reading
heads does not reconcile or publish. Concurrent heads survive until jj converges
them. Client files never move in response to remote events.

Publish reuses cached history, validates new work, persists reachable content and
WAL ancestry, conditionally commits the R2 index, applies locally, then acknowledges.
A warm snapshot must not reconstruct all history. Full reconstruction belongs to
cold-cache startup and recovery. Preserve abandoned-publish object retention and
same-operation retries after uncertain outcomes.

Start with one active host. Replacement means quiesce/fence the old host, recover
secrets and configuration independently of its disk, rebuild from R2, and switch
traffic. Bucket CAS alone does not make two active hosts safe. Automatic failover,
active-active service, and a distributed lease system are outside this plan.

Signing keys, R2 credentials, and deployment configuration need a protected,
recoverable source outside the disposable VM. The deployment stage must name and
exercise that source, including key rotation and recovery of existing credentials.
Do not place plaintext keys in the repository, images, evidence, or command lines.

## Implementation stocktake and branch point

Inspection started at commit `eb1140be` (name filtering), above the website work
ending at `67955d02`. That branch is preserved.

The Cloudflare planning revision is `23ac394f`. Its parent, `11467633`, is the
last native baseline before that work and is the parent of this plan. The first
implementation is `e7058204` (td command journey). WASM rules extraction begins
at `81fe81a6`; the Worker/DO runtime starts at `7a0c185f`. Branching before the
whole project avoids importing its runtime, vendored jj-lib, and recovery model.
Useful product behavior must be carried forward deliberately.

At the baseline, the native system already implements:

- A layered Rust workspace with a headless native jj repository engine and HTTP
  host, remote jj stores, verified client cache, workspace daemon, leases, and SSE.
- Bucket WAL/index authority, conditional index writes, local apply after durable
  commit, warm and cold reconstruction, identity validation, and crash fault seams.
- Bounded file batching and retention of staged blobs through failed index commits.
  The automatic integration worker has already been removed.
- Property, deterministic simulation, process integration, and distance/recovery
  evidence. These are existing tests and historical results, not a fresh execution
  claim for this plan.

The later Cloudflare branch implements the hosted CLI journey, owner tokens and
name rules, portable object handling, Worker routing, namespace/repository Durable
Objects, R2-backed repository publication, event/lease behavior, conformance work,
production qualification machinery, and website assets. Namespace ownership and
catalog recovery still depend on the namespace DO; repository reconstruction does
not prove recovery of that hosting state. None of that supplies the native
multi-repository hosting layer requested here.

Carry forward the td/installer journey from `e7058204`, pure namespace and owner
policy from `79560e6b` and its prerequisites only as needed, website behavior from
`67955d02`, and name filtering plus its licensed data from `eb1140be`. Adapt these
against the native boundaries; do not wholesale transplant the CF stack.

## Recovered performance work

Sources are archived tasks “Analyze Tandem latest changes”
(`01a067e9-98c9-7591-a54d-f65ceb2d67c8`), “Analyze REST operation modeling”
(`01a06e45-a824-7633-9c6c-83dedb2622f2`), and “Compare RPC and HTTP for Tandem”
(`01a06e5e-5661-7cb1-9617-9a05fcbe83bf`), checked against jj history and source.

Already in the selected baseline: file batching and durability fixes reduced the
recorded distant snapshot median from 2.29 s to 1.23 s, with 15 to 8 requests in
that workload. See [retained benchmark evidence](benchmarks/README.md). This is a
historical paired run, not a latency promise for exe.dev plus R2.

Implemented separately, not ancestors of the baseline or CF branch:

- `ece258fa`: share one authenticated HTTP session across the three jj adapters;
  pair view/operation uploads, validate IDs before caching, retain size fallback.
- `45b65022`: fix false daemon staleness by proving operation ancestry and equal
  trees, without moving the working copy. Genuine siblings remain refused.
- `7dd7b7b1`: retained study and reproduction evidence on `rest-recommended`.

That study measured startup 4 to 2 requests, CLI publish 11 to 8, and catch-up
20 to 18. Warm daemon median at 50 ms injected request delay fell 407 to 357 ms.
Shared sessions help startup; the paired upload saves a warm-publish round trip.
Eager operation/view fetching added 81% response-body bytes for one saved request;
do not adopt it by default. These workloads differ from the older 15-to-8 result.

Remaining opportunities from the RPC discussion, to verify against the new host:

1. Repository publish locks span validation, bucket I/O, and local reconciliation;
   head reads share that lock. Measure wait and hold time, then reduce safe work
   inside coordination without inventing another head authority.
2. Independent workspaces still contend on the repository's shared index/version.
   Bound admission and retry amplification; retain all heads and the durable commit
   point. Do not promise 100 simultaneous writers from connection counts alone.
3. Per-repository staged blobs can couple unrelated uploaders. Bound HTTP bodies,
   decoded batches, WAL encoding, and admission as well as the staging buffer;
   exercise a large uploader beside small publishers. Avoid a new client-managed
   transfer lifecycle unless evidence requires it.
4. Head events can cause every subscriber to fetch heads, and client queues can
   grow. Coalesce wake-ups with bounded state, recover from missed events, and
   preserve eventual fresh reads without touching working files.
5. File events discard path details before jj snapshots. Investigate the pinned
   upstream jj filesystem-monitor support before building any custom scanner.
   Measure scan cost and maximum edit-to-durable delay under continuous edits;
   reducing debounce alone can amplify host contention.
6. Checkpoints could bound cold recovery eventually. Compaction and GC need their
   own reachability/retention design; keep them deferred until recovery measurements
   justify that work. No deletion of durable history in this plan.

Keep HTTP and stock jj store semantics. Transport replacement, eager ancestry
prefetch, automatic integration, custom merge engines, and horizontal scaling do
not address the immediate native-host deliverable.

## Delivery and qualification

Every revision delivers a runnable user scenario across the required layers.
Storage, routing, credentials and client changes land together when that scenario
needs them. Each revision extends the preceding revision's acceptance fixture;
there is no separate infrastructure-only or benchmark-only stage.

1. **Create, publish, destroy the cache, recover.** One owner creates a named
   repository through the native host, clones it with td, publishes exact file
   bytes, and recovers ownership, catalog and content after cache loss. Start with
   the existing bucket abstraction locally; include an opt-in R2 run. Credentials
   are protected and scope is enforced from this first slice. Bring only the
   minimal hosted addressing and command journey needed for this scenario.
2. **Two owners use isolated repositories on one host.** Add the manager behavior,
   namespace races, per-repository resource/state separation and negative access
   tests required to publish concurrently and recover both repositories.
3. **Install and use the service over exe.dev HTTPS.** Serve the website and
   installer from the supervised host, exercise bootstrap/clone/publish from a
   second machine against R2, and preserve secret/configuration recovery outside
   the VM. Use a test address; production DNS remains unchanged.
4. **Two agents collaborate in one repository.** Distinct workspace credentials,
   simultaneous clones, automatic publishing, reconnects and stale notifications
   work without moving either agent's files. Carry the proven ancestry fix here.
5. **Make repeated remote saves cheaper as history grows.** Carry shared sessions
   and paired metadata uploads into the working journey, prove incremental cache
   reuse, and measure before/after request depth and latency in this revision.
6. **Keep small writers responsive under load.** Exercise large uploaders,
   continuous edits, notification bursts and slow repositories; add measured
   admission bounds, coalescing and coordination improvements in the same slice.
   Investigate upstream filesystem monitoring against the continuous-edit case.
7. **Replace the VM while agents retain access and content.** Fence the old host,
   restore protected secrets/configuration and R2 state on a fresh VM, reconnect
   existing agents, and publish new bytes. Exercise key rotation and rollback.
8. **Deploy the fresh service on tandem.land.** Validate real agent environments
   and the supported load envelope, deploy a fresh native host with R2, switch
   DNS/proxy routing, and validate the live production journey. This is greenfield:
   no legacy ownership, credentials, repositories or service behavior need migration.

Stage 1's demo must work from an empty host cache without manually editing bucket
records or invoking internal Rust APIs. A fresh client must read the exact
published bytes after recovery, and the original owner must retain ownership.
Later slices reuse this scenario as their regression floor. Performance baselines
and correctness tests are captured within the slice they validate. Implementation
stops between stages when its criteria fail.

Measure one and many repositories with 1, 10, and 100 connected agents, and report
active writer count, edit rate, file sizes, history size, and locations separately.
Capture edit-to-durable and snapshot-to-ack p50/p95/p99, request depth, bucket calls
and bytes, queue depths, memory, lock wait, retries, and cold/warm recovery work.
Set explicit resource budgets and a latency envelope from the frozen baseline
before optimizing; do not invent a numerical service promise from old benchmarks.

Local correctness gates precede opt-in R2 and cross-machine drills. Exact file
bytes, all concurrent heads, namespace ownership, repository catalog, credential
continuity, and a successful new publish must survive total VM cache loss.
A fault or large uploader in one repository must not prevent another repository
from making progress within the declared workload envelope.

The user authorized implementation, production validation on exe.dev, and wiring
`tandem.land` to the new VM on 2026-09-14. They explicitly clarified that this is
all greenfield: no legacy preservation, export/import or migration is needed.
After qualification, deploy the fresh service and switch DNS/proxy routing within
that authorization. Retain recoverability of new acknowledged data and a safe
operational rollback for the new service; do not add legacy compatibility paths.
