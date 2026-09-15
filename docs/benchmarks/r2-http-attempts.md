# R2 HTTP attempt profile — 2026-09-15

A repeat of the eight-file warm benchmark used three warmups and 40 measured
snapshots on a disposable 2 CPU, 8 GiB exe.dev Dallas host. The client disk
cache remained enabled. Cloudflare's bucket API confirmed WEUR, Standard
storage and the default jurisdiction. This run changed tracing, not transport
options, retry policy or publish behavior.

| Measured write | PUT attempts | HTTP 200 with ETag | Follow-up HEAD | SDK retries | PUT header latency p50 / p95 / p99 (ms) |
|---|---:|---:|---:|---:|---:|
| WAL, 40 writes | 40 | 40 | 0 | 0 | 490.373 / 688.807 / 939.204 |
| Index, 40 writes | 40 | 40 | 0 | 0 | 507.434 / 809.059 / 1230.518 |

Mean WAL PUT duration was 521.988 ms; mean index PUT duration was 574.931 ms.
All 80 PUTs used local/remote socket pairs already seen in the retained trace.
That is evidence consistent with connection reuse, not a direct connection-pool
hit counter. The separate WAL existence HEAD remains before each WAL PUT;
it is not an ETag follow-up. Snapshot p50/p95/p99 was
2436.079/2899.916/3110.301 ms, mean 2496.831 ms (nearest-rank quantiles).

The [attempt artifact](r2-http-attempts.json) records method, start/end UTC and
monotonic elapsed duration, response status, ETag presence, socket addresses,
SDK call ID, attempt ordinal and inter-attempt gaps for every selected write.
With no retries in this measured set, retry-gap timing has zero samples; it is
not reported as zero-duration backoff.

## What the observations distinguish

The measured WAL and index costs are individual successful PUTs, rather than
PUT-plus-HEAD or a failed PUT followed by a retry. The pinned SDK is
`object_store` 0.14.1. A real HTTP regression sends 503 then 200 with an ETag
and verifies two connector attempts with a measured intervening gap. It then
sends 200 without an ETag: the SDK rejects that response before the adapter's
fallback branch, and no HEAD occurs. The adapter's existing explicit fallback
is traced if reached; it was not reached in either this regression or the
real-R2 measured writes.

Attempt end is response headers or transport error, not completion of arbitrary
response-body consumption. Inter-attempt gaps include SDK body handling,
signing, backoff and scheduling. The preceding response status/error kind is
recorded; the SDK's exact sleep decision is not exposed by this hook. Lower
transport transparent replays are also not exposed. DNS lookup, TCP connect and
TLS handshake durations are unavailable through the existing connector, so no
numbers are invented for them.

One slow PUT on an apparently reused socket still leaves network transit
versus R2 processing unresolved. No placement comparison was performed here.

## Cloudflare evidence

The Cloudflare plugin's R2 guidance was used with the authenticated REST and
GraphQL APIs. The API confirmed bucket configuration. GraphQL returned
operation/status/byte counters filtered to this experiment's exact prefix;
these adaptive aggregates include setup and warmups and are not treated as
packet counts or per-request latency. Introspection exposed dimensions and
request/response-size sums, but no service-latency field in this dataset.
See [R2 analytics](https://developers.cloudflare.com/r2/platform/metrics-analytics/)
and [data location](https://developers.cloudflare.com/r2/reference/data-location/).

[Data Access Logs](https://developers.cloudflare.com/r2/buckets/data-access-logs/)
are a different facility: documented fields do not provide the missing duration
split, and failed requests are excluded. This experiment did not enable or
change logging configuration on the shared bucket.

## Reproduction and scope

Raw traces, binary/build identity, Cloudflare query/schema/configuration results,
controller scripts and cleanup evidence are retained under
`/home/laurynas/.local/state/tandem-r2-combined-experiment`.

```sh
python3 scripts/profile_r2_attempts.py \
  /home/laurynas/.local/state/tandem-r2-combined-experiment \
  --output /tmp/r2-attempts.json
cargo test -p jj-tandem-storage --features s3
```

The analyzer requires matching attempt start/end records, complete SDK attempt
ordinals, two logical writes per selected publish and exactly 40 benchmark
samples. Socket observations include earlier captured setup/warmup requests.
Production was not modified. Cleanup passed: the profiling VM was removed, all 51 objects under its exact
ephemeral prefix were deleted, and the protected profile configuration was
removed. Provider and bucket reads verified removal.
