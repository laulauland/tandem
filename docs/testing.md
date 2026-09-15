# Testing strategy

Tests belong at the cheapest seam that can prove the behavior. One claim may
need more than one seam only when each adds a distinct kind of evidence.

| Home | Use it when | Do not use it for |
| --- | --- | --- |
| Unit tests beside each crate's source | Pure policy or a private boundary is the subject: parsing, authorization decisions, writer leases, cache bookkeeping | Cross-process behavior or broad product claims |
| Property tests | A codec, framing format, or value transformation must round-trip across a large input space; hostile bytes must never panic or over-allocate | Stateful schedules or OS behavior |
| Deterministic simulation (DST) | Correctness depends on interleavings, CAS loss, faults, restart, replay, convergence, or an invariant that should hold after every step | Signals, real Git, CLI text, or filesystem/process integration |
| Integration tests | The subprocess, socket, signal, real filesystem, genuine concurrent process, S3 API, Git round-trip, or user-facing CLI is itself the behavior | Repeating state-space coverage already owned by the DST or properties |

## Shared rules

- Start with a failing test at the owning seam.
- Assert exact file bytes for repository-content claims. Descriptions and logs
  may locate a revision but do not prove its tree survived.
- Do not use fixed sleeps. Await output with the shared line reader or await
  state through the shared workspace/event helpers.
- Keep environment-dependent tests opt-in and make local deterministic coverage
  the first gate.
- Put subprocess setup with the CLI integration tests in `crates/cli/tests/common/`;
  shared readiness/configuration and simulation actors belong in
  `testing/test-support/`. Stateful round-trip properties run with simulation;
  wire and WAL properties stay with their owning format crates.
- A generated failure must print a reproducer. Pin a DST seed only when it
  captured a real regression or otherwise guarantees a rare schedule.
- The in-process suites disable the client disk cache so a server read cannot
  accidentally be satisfied by another simulated client's local state.

## Reliability coverage

The property suite owns WAL and wire round-trips, corruption rejection, and
allocation safety. The DST owns the invariant oracle: client convergence,
non-divergent change IDs, real stored heads, equality between the API view and
jj head authority, index-to-WAL reachability, local state never claiming to be
ahead of durable state, and byte-identical reads from every actor.

Fixed DST schedules cover every publish crash window, objects arriving between
CAS attempts, and objects drained by a failed WAL write. Every schedule ends by
draining staged content, deleting the server materialization, replaying from
the bucket alone, and running the oracle again.

The abandoned-publish regressions deliberately skip the final drain: another
workspace uploads first, a publisher loses index CAS or encounters an index
write failure and never retries, then the first workspace publishes and
immediately cold-restarts. Its exact bytes must survive without reupload.

Recovery regressions corrupt a WAL ancestor, fail cold recovery, and retry on
the same partial materialization before restoring the valid entry. Both failed
boots must leave the index version unadopted; repaired recovery must return the
original file bytes. Repository unit tests cover WAL identity and record-order
validation, including protobuf encodings with unchanged semantic jj hashes.

Integration coverage remains where reality adds evidence: authentication and
scope at HTTP/CLI boundaries, clone and daemon lifecycle, cache and baked-image
behavior, process contention, control sockets, Git shipping, API cache/CAS
semantics, unclean process death, and filesystem/S3 replay.

Use `cargo test --workspace -- --list` to find current test names.

## Commands

```bash
# Documentation and workspace boundaries
python3 scripts/check_docs.py
python3 scripts/check_workspace.py
python3 -m unittest discover -s scripts -p 'test_*.py'

# Focused durable-format and storage checks
cargo test -p jj-tandem-wal
cargo test -p jj-tandem-storage

# Opt-in S3 contract (use an isolated non-production prefix)
TANDEM_TEST_S3_BUCKET=<isolated-s3-url> cargo test -p jj-tandem-storage --features s3

# Deterministic crash and concurrency schedules
cargo test -p jj-tandem-simulation

# Full local suite
cargo test --workspace
```

Set `TANDEM_TEST_S3_BUCKET` to an existing test bucket to run the S3-backed
integration tier. Never point tests at a production prefix.


## Measure performance

Run measurements against a disposable repository and storage prefix. Retain
raw reports outside the documentation tree. Record source revisions, binary
hashes, host placement, provider, file sizes, writer counts, and cache state
with each comparison. Use separate client benchmark and server binaries for
each revision: changing the server alone does not change the client code
embedded in a benchmark.

```bash
# Filesystem storage, with no external service.
cargo bench -p jj-tandem-benchmarks --bench snapshot_publish_latency

# A separately provisioned S3 test prefix; credentials stay in the environment.
TANDEM_TEST_S3_BUCKET='s3://your-test-bucket/snapshot-bench' \
  cargo bench -p jj-tandem-benchmarks --bench snapshot_publish_latency

# An existing disposable host. Set TANDEM_BENCH_TOKEN in a protected environment.
TANDEM_BENCH_SERVER=https://your-benchmark-host.example \
  cargo bench -p jj-tandem-benchmarks --bench snapshot_publish_latency

# Check which packages rebuild after client and server source changes.
python3 scripts/check_build_cache.py
```

The snapshot timer covers the daemon's scan through durable acknowledgement.
It excludes clone setup, debounce, writer acquisition, and later staleness
refresh. Do not present it as end-to-end edit latency without measuring those
other phases. Fixed request delay models added latency; it does not measure
network transit or object-store processing.

The default reports stay beneath the Cargo target directory. Set
`TANDEM_BENCH_OUTPUT_DIR` to an absolute evidence directory to retain a run.
`TANDEM_BENCH_BIN` selects an existing matching CLI binary; a copied standalone
benchmark needs both paths. Never capture bearer values in reports or command
arguments. Require exact-byte reads after restart when a benchmark also claims
recovery correctness.
