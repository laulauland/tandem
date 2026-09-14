# Benchmarks

Recorded numbers, and the commands that produce them. Benchmark reports and
reviewed comparisons are recorded deliberately — the benches
write under `target/benchmarks/` by default, and only put a number here when
`TANDEM_BENCH_RECORD=1` says so. Committing a measurement is meant to be a
decision rather than a side effect of having run one.

## Workspace build-cache isolation

Run `python3 scripts/check_build_cache.py` from the checkout. It warms the named
CLI build, touches one server source and one client source without modifying
their contents, then checks Cargo's artifact freshness and emits HTML timings.
Default evidence stays under the Cargo target directory; use `--record <path>`
only for a measurement intended for review.

The [recorded workspace probe](workspace-build-cache.json) rebuilt only the CLI
and server after a server touch, and only the CLI, client, and workspace after a
client touch. These are warm invalidation checks, not a cold-build speedup claim.

## Mixed-load qualification (2026-09-15)

The [Stage 6 comparison](stage6-mixed-load.json) records the frozen
[workload and budgets](stage6-workload.md), executable identities, observations,
and paths and hashes for the complete raw reports. It compares Stage 5 with the
reviewed native-host candidate. Both variants ran the same ten-repository mix:
three small publishers and a repeated 32 MiB uploader, with burst and steady
edit schedules. The table measures snapshot-to-ack latency using the first 40 attempts per small writer; the
artifact also includes full-run statistics and each writer separately.

| Profile | Baseline p95 (ms) | Candidate p50 / p95 / p99 (ms) |
|---|---:|---:|
| Real R2, burst | 3013.6 | 2842.2 / 3481.6 / 3879.2 |
| Real R2, steady | 2916.2 | 2796.7 / 3945.1 / 4329.2 |
| Filesystem + 50 ms/request, burst | 586.4 | 558.3 / 572.9 / 587.2 |
| Filesystem + 50 ms/request, steady | 558.9 | 568.4 / 593.5 / 607.6 |

All declared latency, memory, queue and recovery checks passed. The real-R2
candidate peaked at 198.2 MiB RSS on the 2 CPU, 8 GiB exe.dev host in Dallas;
R2 uses a WEUR location hint, and the controller's physical location is
unverified. The R2 p95 increased by 15.5% in bursts and 35.3% with steady edits.
These measurements establish bounded resource use under the declared mix.
The benchmark uses a 3,600-second writer lease; renewal across expiry and
latched ownership loss are verified by separate deterministic regressions.
These results do not establish a general speedup or capacity
beyond that workload. The earlier paired publish comparison records the
separately measured round-trip reduction.

All idle 1/10/100-connection cases passed independently of active writer count.
After each active profile, fresh clients verified acknowledged-operation
reachability and exact final bytes following restart. Every captured small
commit also received an exact-byte check; intermediate large commits received
reachability checks, with exact bytes checked at the final large commit.
Baseline reports explicitly mark unavailable coordination instrumentation.

The [supplementary distributed journey](stage6-distributed.json) passed on two
separate exe.dev client machines plus the controller. Installation, owner setup,
scoped clones and initial publishes overlapped 66.9 seconds of a separate
240-second background burst at the declared rates. Existing agents published
again after restart without moving local files; a fresh controller verified all
five expected files with caching disabled. Disposable client workspaces and
daemons were cleaned. This added-client drill does not expand the capacity claim.

The [10,000-file scan measurement](stage6-scan.json) retains 40 samples and
exact-byte checks. Upstream jj filesystem-monitor comparison is deferred because
no Watchman service was available on the controller or validation host.

## snapshot → publish latency

The gate metric: how long a file change takes to become durable.

What is timed is one call to the workspace daemon's own `snapshot_once` — the
filesystem scan, the tree write, the operation, and the head update the server
acknowledges only once the write has reached the bucket. It is the library call
the daemon makes and not a reimplementation of it, so a change that slows the
product down cannot leave this number alone.

The debounce window is deliberately outside the measurement. That window is a
policy number a person sets (`--debounce-ms`, `TANDEM_DEBOUNCE_MS`) and it
would swamp everything else; what this answers is the question the window is
set against — what the machinery itself costs, once it has been told to go.

Source: [`testing/benchmarks/benches/snapshot_publish_latency.rs`](../../testing/benchmarks/benches/snapshot_publish_latency.rs).

### File batching comparison (2026-09-04)

The [paired results and raw samples](publish-batching.json) compare the
workspace-refactor baseline with publish improvement stages 1–3. Each variant
uses its own compiled daemon/client benchmark and matching CLI/server binary.
Both use the same host, separate temporary filesystem buckets, 3 warmups,
and 40 measured snapshots of 8 files in a src directory inside the test workspace.

| Environment | Baseline p50 / p95 | With batching p50 / p95 |
|---|---|---|
| Loopback | 2.347 / 2.590 ms | 1.833 / 1.960 ms |
| Loopback + 50 ms added per request | 761.370 / 762.925 ms | 406.833 / 407.603 ms |

The integration test separately verifies that eight files use one upload
batch, with exact binary content read back through the server. Its files are
at the repository root; the benchmark's src directory adds a tree write.
The timer excludes cloning, debounce, writer acquisition, and the subsequent
staleness refresh. These are single local runs, not real-distance or S3
qualification. The separate exe.dev run below supplies real-network evidence.

### exe.dev qualification (2026-09-04)

The [remote results and raw samples](publish-distance.json) compare matching
baseline and final binaries over the same provider HTTPS route to Dallas,
with an isolated SeaweedFS 4.42 S3 bucket on server loopback. Each variant ran
3 warmups and 40 measured eight-file snapshots, with no injected delay.
The [qualification manifest](publish-qualification.json) records binary hashes
and the baseline's identical, harness-only portability changes.

| Metric | Baseline | Final |
|---|---:|---:|
| p50 | 2288.43 ms | 1225.09 ms |
| p95 | 2295.42 ms | 1233.35 ms |
| Requests per measured snapshot | 15 | 8 |

p50 fell 46.47%; p95 fell 46.27%. This is one paired run, not a broad latency
distribution. The controller's network egress reported Finland/HEL; its
physical location was not verified. ICMP to the resolved provider endpoint
averaged 33.65 ms and can terminate at ingress, not the Dallas VM.

Collaboration used two separate client VMs and one server VM, all
provider-verified in Dallas. Five random 64-KiB payloads matched on both
clients and the server with client caching disabled. Heads and workspace
pointers survived an unclean warm restart (zero entries replayed) and a fresh
materialization (90 entries replayed in 186 ms). A new publish after cold
recovery survived another restart. SeaweedFS stayed running: this proves
S3-backed materialization recovery, not loss of the entire host or bucket.

The initial concurrent clones were stale and required an explicit
`workspace update-stale` after preserving the test payload outside the
workspace. The subsequent byte and recovery checks passed; the clone setup
issue remains a separate finding, not a fix delivered by this work.

### Native-host publish comparison (2026-09-15)

The [Stage 5 paired report](stage5-paired-publish.json) compares the completed
Stage 4 source with Stage 5's combined shared sessions, paired view-operation
uploads, and reuse of validated hosted catalog ownership. Warm repository
requests no longer read the catalog from R2 each time. Both binaries and both embedded benchmark
executables were built in the same pinned `rust:1-bookworm` image. Each run
used 3 warmups followed by 40 measured `snapshot_once` calls that rewrote 8
files. The debounce window, clone/setup, writer acquisition, and later
staleness refresh remain outside the timer.

| Environment | Stage 4 p50 / p95 | Stage 5 p50 / p95 | Request depth |
|---|---:|---:|---:|
| Native host with R2 | 3876.619 / 4624.041 ms | 2238.556 / 2533.532 ms | 8 → 7 |
| Filesystem + 50 ms/request | 406.710 / 407.650 ms | 356.862 / 357.466 ms | one round trip removed |

On the native-host run, p50 fell 42.25% and p95 fell 45.21%. The injected
profile reproduces the earlier 407-to-357 ms estimate. These numbers measure
the combined Stage 5 change, so they do not assign separate latency shares to
session reuse, paired uploads, and hosted catalog caching. The local frozen CLI probe gives the command
shapes that explain the difference: startup uses 2 requests, publish 8, and
catch-up 18. A warm daemon keeps its authenticated session and operation state
alive; pairing its view and operation therefore takes the measured snapshot
from 8 requests to 7.

The native host was a provider-verified Dallas VM with 2 CPUs and 8 GiB RAM.
The R2 bucket reported a WEUR location hint. The two distributed-smoke client
VMs were also provider-verified Dallas, while the benchmark controller's
physical location was not verified. RPC counts were service-wide during an
exclusive validation-host window; final events also carried the named
repository. WAL record and byte samples cover WAL writes only, excluding
immutable objects, index traffic, recovery, and other bucket operations.
This is one paired run per profile and does not establish a general capacity
or latency promise.

### Running it

One command per tier, and the tier is chosen by environment alone.
The harness builds the named CLI package and discovers its release executable
from Cargo output. Set `TANDEM_BENCH_BIN` to an explicit existing binary only
when intentionally measuring that artifact (relative paths are checkout-root
relative); no stale debug fallback is used.

For a copied standalone benchmark, set both `TANDEM_BENCH_BIN` and
`TANDEM_BENCH_OUTPUT_DIR` to absolute paths. The latter receives the report's
filename and takes precedence over `TANDEM_BENCH_RECORD`; neither override
requires the original source checkout to exist. Supply the remote bearer
through `TANDEM_BENCH_TOKEN`; the harness passes it to the clone subprocess
through its environment.

The snapshot benchmark embeds the workspace daemon and client code. Comparing
revisions therefore requires separately built benchmark executables and
matching CLI/server binaries from each revision. Changing `TANDEM_BENCH_BIN`
alone does not change the embedded client being timed. Apply the same
harness-only portability changes to both builds and record their source
revisions, harness changes, and executable hashes alongside measurements.

```bash
# The filesystem bucket backend. Nothing external.
cargo bench -p jj-tandem-benchmarks --bench snapshot_publish_latency

# A real S3 API. Create the bucket once; SeaweedFS answers 403, not 404, for
# one that does not exist.
docker run -d --name seaweed-test -p 8333:8333 chrislusf/seaweedfs:4.42 server -s3
curl -X PUT http://127.0.0.1:8333/tandem-bench
TANDEM_TEST_S3_BUCKET='s3://tandem-bench?endpoint=http://127.0.0.1:8333&anonymous=true' \
  cargo bench -p jj-tandem-benchmarks --bench snapshot_publish_latency

# A stand-in for distance: a fixed delay added to every client request.
TANDEM_BENCH_INJECT_RTT_MS=50 \
TANDEM_TEST_S3_BUCKET='s3://tandem-bench?endpoint=http://127.0.0.1:8333&anonymous=true' \
  cargo bench -p jj-tandem-benchmarks --bench snapshot_publish_latency

# Real distance: a server that is already running somewhere else, with a
# bucket of its own. This is the only one of the four that measures rather
# than models.
TANDEM_BENCH_SERVER=https://tandem-bench.exe.xyz TANDEM_BENCH_TOKEN=tdma_… \
  cargo bench -p jj-tandem-benchmarks --bench snapshot_publish_latency
```

Add `TANDEM_BENCH_RECORD=1` to any of them to write the result here instead of
under `target/`. Each tier files under its own name, because these numbers are
only meaningful next to each other and a single `..._latest.json` would let one
silently replace another. The injected delay is part of the name wherever it is
part of the run — a remote server with `TANDEM_BENCH_INJECT_RTT_MS` set files
as `real_distance_injected_rtt<n>ms`, not as `real_distance`, so an emulation
cannot take a real measurement's place.

### Recorded

40 measured rounds after 3 warmup rounds, 8 files rewritten per round. The
first three rows are from 2026-08-21 with client and server on one machine.
The fourth is from 2026-08-24: `tandem serve` on an exe.dev VM in Dallas with
a SeaweedFS 4.42 bucket on that VM's own loopback, and the client on the
developer's laptop reaching it as `https://tandem-bench.exe.xyz` — bearer
auth, TLS terminated at the exe.xyz proxy edge, then forwarded to the VM.

| tier | p50 | p95 | artifact |
|---|---|---|---|
| filesystem bucket | 2.2 ms | 2.4 ms | [`…_filesystem_latest.json`](./snapshot_publish_latency_filesystem_latest.json) |
| S3 API (SeaweedFS 4.42, loopback) | 3.9 ms | 4.1 ms | [`…_s3_latest.json`](./snapshot_publish_latency_s3_latest.json) |
| S3 API + 50 ms injected per request | 765.0 ms | 766.4 ms | [`…_s3_injected_rtt50ms_latest.json`](./snapshot_publish_latency_s3_injected_rtt50ms_latest.json) |
| real distance (laptop ↔ exe.dev DAL) | 2518.3 ms | 2529.8 ms | [`…_real_distance_latest.json`](./snapshot_publish_latency_real_distance_latest.json) |

What the rows say together is that the durability hop is cheap and the round
trips are not. Going from a directory to a real S3 API costs 1.7 ms at the
p50; adding 50 ms to each request costs 761 ms, which is about fifteen
requests deep per publish; and the real link prices those same fifteen at
about 170 ms each, for a publish that costs two and a half seconds.

The third row is an upper bound only at its own delay: it charges full price
for requests a real client overlaps, and a constant has none of the jitter,
loss or bandwidth-delay product that a real link has. The real link here
simply cost more than 50 ms per request — a bare TCP connect from the laptop
reaches the proxy edge in ~34 ms, but a full request through the edge to the
VM ran ~170 ms — so the fourth row is 3.3× the third for the same reason the
third is 200× the second. The remarkable thing about the fourth row is how
tight it is: 40 samples between 2508 and 2535 ms, which says the cost is
structural — serialized round trips — and not network noise. That is the
number the durability window depends on (see
[the architecture](../../ARCHITECTURE.md)), and it is why collapsing the
publish path's request count was worth reducing. The current batching
comparison above measures that change locally; these older distant results
must not be presented as measurements of the new implementation.

## Commit-path latency and in-flight throughput

Older transport benchmarks, kept because the code they drive is
still live: `TANDEM_BENCH_INJECT_RTT_MS` and
`TANDEM_BENCH_DISABLE_OPTIMISTIC_OP_HEAD_VERSION_CACHE` are both read by the
client today.

```bash
cargo bench -p jj-tandem-benchmarks --bench tcp_commit_path
cargo bench -p jj-tandem-benchmarks --bench tcp_inflight_throughput
```

Artifacts: [`tcp_commit_path_latest.json`](./tcp_commit_path_latest.json),
[`tcp_inflight_throughput_latest.json`](./tcp_inflight_throughput_latest.json).
Both measure a whole `tandem describe` + `tandem new` cycle from outside the
process, which makes them a coarser instrument than the one above: they include
process start-up, and they cannot see inside a publish.
