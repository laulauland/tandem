# Combined publish experiment — 2026-09-15

The prototype accepts a locally prepared graph only if native ingestion preserves
its identities and normalized payloads. A mismatch returns a conflict before
head publication. This extends the [preparation gate](local-publish-parity.md)
with explicit rejection; it does not attempt identity correction.

## Behavior and correctness

The frame carries objects in dependency order, a view, an operation, and the
existing head compare-and-swap request. The prototype supports files, trees and
resolved unsigned commits, with at most 64 objects and the existing request and
metadata size bounds. It is an isolated experiment: the stock remote-store
client and daemon do not automatically use it.

Authentication and publish admission precede body processing. The server checks
the metadata identities and supported object shapes, then ingests each object
through the existing native jj backend and compares the returned identity and
normalized bytes. It stages the paired view/operation only after those checks
pass, and then invokes existing scope validation and head publication. WAL,
conditional index commit, local application and acknowledgment retain their
existing ordering. A rejected request can leave unreachable staged objects;
it does not publish heads or change the index on its own.

The native-jj collision reproducer now also sends a combined request to the
HTTP host. It receives a conflict, and exact index bytes, head set and head
version stay unchanged, including after cold reconstruction.

Other passing real-HTTP/native-jj checks cover:

- two simultaneous scoped writers, including stale-version retry and both
  exact operations reachable from current heads after cold recovery;
- all five existing publish crash windows, no successful response from the
  halted host, recovery from the bucket alone, and retry of the same graph;
- a TCP proxy that drops the response after the host emits success, followed
  by cold recovery and retry without preparing new identities;
- missing credentials, wrong workspace attribution, forged view scope,
  object and operation identity mismatches, and malformed input.

Wire properties exercise round trips, hostile frames and every truncation of
a representative frame. The full workspace suite, including existing storage
properties and deterministic simulation, passes. Independent specification
and standards reviews are clean after fixing the evidence checks.

## Matched measurement method

The experiment prepares one nested edited file using native jj transactions
and warm local preparation repositories. It creates a commit for each edit;
it does not time filesystem scanning or the daemon's complete snapshot command.
The existing lane sends file, child tree, root tree, commit, paired metadata,
and heads in six requests. The combined lane sends those pieces in one request.

Both lanes run from this controller against separate repositories on the same
disposable Dallas exe.dev host and R2 bucket, with identical file payloads,
writer count, host sizing and preparation-cache policy. Three warmups precede
40 measured edits per lane; lane order alternates each round. Authentication,
initial history import and initial head-version lookup occur before timing.
Later versions come from acknowledgments. The measured interval includes local
preparation plus all mutation requests through response-body completion.

Every accepted sample is then verified outside the timed interval by an
explicitly cache-disabled fresh client: current heads reach the exact locally
prepared operation, its view identifies the expected commit, and the nested
file reads byte-identically. Separate remote concurrent writers must share one
repository, and both are verified after all retries. Cold-recovery verification
retains both the original operation and commit identities.

Request counts and transferred bytes describe the mutation interval. Body-byte
counts exclude HTTP headers, TLS, setup and verification traffic. The result
must not be presented as reducing the existing eight-request full daemon
snapshot to one request. No caching changes are included.

## Measured results

Forty measured edits per lane on the same Dallas host and R2:

| Interval, ms | Existing mean / p50 / p95 / p99 | Combined mean / p50 / p95 / p99 |
|---|---|---|
| Preparation + publish | 2175.033 / 2125.226 / 2458.679 / 2769.343 | 1475.653 / 1367.035 / 1911.293 / 2114.482 |

| Per measured edit | Existing | Combined |
|---|---:|---:|
| Mutation HTTP requests | 6 | 1 |
| Mean request-body bytes | 905.825 | 1041.825 |
| Mean response-body bytes | 573.700 | 312.875 |
| Mean combined body bytes | 1479.525 | 1354.700 |

Mean local preparation was 0.593 ms for existing and 0.596 ms for combined;
publish time accounts for the remaining measured interval.
Mean total latency decreased by 32.15%; total HTTP body bytes decreased by
8.44%. The combined request sends extra expected identities and framing, while
its acknowledgment omits individual normalized object payloads. Nearest-rank
p99 is the maximum for 40 samples. This is one alternating paired run, not a
population-wide tail-latency estimate.

Every measured edit and the separate concurrent-writer check passed the exact
operation/commit/file oracle. Cold recovery passed for both benchmark lanes and both concurrent writers
from an empty host cache. Cleanup removed the disposable VM, all 100 objects
under its exact R2 prefix, and its protected configuration; listings verified
removal. The [data artifact](combined-publish.json) includes
individual samples, preparation/publish percentile tables and input hashes.

## Evidence

Measured binary identities, samples, raw host tracing, controller scripts,
verification output and cleanup evidence are retained under
`/home/laurynas/.local/state/tandem-combined-publish`.
