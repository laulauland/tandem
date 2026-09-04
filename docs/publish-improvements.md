# Faster, simpler durable publishes

Tandem remains pre-1.0. Improve the headless repository by removing an unsafe
mutation path, closing concrete durability gaps, and reducing serialized file
uploads. Preserve stock jj operations, concurrent heads, scoped authorization,
and exact file bytes after cold recovery.

## Ordered work

1. Remove the parked automatic integration workspace: its background worker
   publishes into the local repo without the WAL commit protocol. Remove its
   flags, status/configuration plumbing, and dedicated tests together. Explicit
   integration through ordinary jj commands remains available.
2. Keep uploaded blobs eligible for publication until the bucket index commits.
   A successful WAL write followed by a failed index update must not strand
   another workspace's bytes in an unreachable WAL. Prove the abandoned-publish
   case with deterministic fault injection and cold recovery.
3. Batch file uploads using the existing object batch endpoint. Use upstream
   Git content hashing for file IDs and flush before dependent tree/commit
   writes. Bound buffered bytes and batch size, support reads of buffered files,
   and preserve queued bytes across partial/transport failures. Keep tree and
   commit normalization on the server. Target eight file uploads in one request
   and roughly eight serialized requests per eight-file snapshot instead of
   fifteen. Confirm the actual request counts and timings before claiming gains.
4. Validate replay identity before adopting an index: the requested WAL key,
   decoded operation identity/parents, and stored operation/view content must
   agree. Corrupt-but-decodable history fails recovery before the new index
   version is adopted. Preserve incremental warm replay.
5. Compare the refactor baseline and final implementation on disposable exe.dev
   VMs, with one server and two independent clients. Capture physical locations,
   observed RTT and request latency, exact binary hashes, p50/p95 snapshot
   latency, payload digests, concurrent publish results, and warm/cold recovery.
   Use the same topology and workload for both binaries; distinguish filesystem
   durability from an actual S3 API. Delete only resources created for this run
   after successful evidence capture.

## Simplicity constraints

No new publish endpoint, background upload service, transaction/session
protocol, writer-lock enforcement, or extra production crate. Do not batch
operations and views in this iteration. Checkpoints, garbage collection,
multi-node serving, and broad transport retries remain future design work.
Use existing fault seams, batch framing, and test packages. Remove superseded
code rather than keeping parallel implementations or compatibility switches.

## Evidence and execution

Each stage is a jj revision with acceptance criteria, verification, and a
deviations log. Implementation is delegated in bounded pieces, reviewed against
the contract, and verified before moving to the next stage. The user requested
planning and implementation together, so execution follows the plan in this
run. Network qualification requires functioning exe.dev SSH authentication;
unavailable access is reported as missing evidence, never replaced by an
injected-delay claim. Baseline revision: `zywqrrxksoqt` (`21318c90`).
