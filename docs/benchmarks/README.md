# Benchmarks

Recorded numbers, and the commands that produce them. Every file in this
directory is written by a bench run that was asked to record — the benches
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

### Running it

One command per tier, and the tier is chosen by environment alone.
The harness builds the named CLI package and discovers its release executable
from Cargo output. Set `TANDEM_BENCH_BIN` to an explicit existing binary only
when intentionally measuring that artifact (relative paths are checkout-root
relative); no stale debug fallback is used.

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
publish path's request count is the
next thing worth doing: at two round trips instead of fifteen, the same link
prices a publish at roughly a third of a second.

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
