# Snapshot latency profile — 2026-09-15

This measures the existing native Rust host on a disposable exe.dev VM backed
by real R2. The revision adds timing traces and an offline analyzer. It changes
no publish, storage, cache, retry, admission, or workload behavior. Production
`tandem.land` was not modified.

## Measurement and comparison

Both runs used the same instrumented release artifacts and the same 2 CPU,
8 GiB exe.dev VM in Dallas, reached through its HTTPS proxy from `gondor`.
The controller's physical placement was not independently verified. R2's WEUR
location hint was recorded in the earlier qualification; it does not establish
an individual request's physical execution location.

The warm run rewrote eight small text files under `src`, with three warmups and
40 measured snapshots, one active writer, and the client disk cache enabled.
The mixed run used the existing [frozen workload](stage6-workload.md): ten
resident repositories, three small writers in separate repositories rewriting
eight 64-byte files each, and a repeated 32 MiB publisher in a fourth repository.
Its client disk cache is explicitly disabled. Its reported small-writer profile
uses the first 40 attempts per writer after three warmups, 120 samples per
profile. Burst input lasts 240 seconds; steady input has 40 edits five seconds
apart. The raw report retains drain attempts, the full active windows, and the
separate idle-connection matrix.

These are different payloads and cache policies, not a controlled test of
contention alone. Neither run includes clone/setup, file-edit generation, or
debounce in snapshot latency. The timer starts inside `snapshot_once`, includes
its writer check, and ends after the acknowledged transaction and working-copy
finish. This is a library-call measurement, not CLI startup latency.

Unless otherwise stated, timing tables use **p50 / p95 / p99, in milliseconds**, computed with
nearest-rank quantiles from raw samples. For 40 samples, p99 is the maximum.
The warm harness uses a different median convention; its originally printed
p50 is retained in the raw report. Percentiles of nested intervals must not be
added together.

## End-to-end and nested phases

| Interval (ms) | Warm | Mixed burst | Mixed steady |
|---|---:|---:|---:|
| Snapshot to acknowledgment and working-copy finish | 2463.121 / 3037.925 / 3130.513 | 2817.018 / 3399.928 / 3552.776 | 2783.415 / 3460.791 / 4105.484 |
| Writer check | 155.226 / 156.693 / 178.802 | 154.921 / 158.146 / 164.018 | 155.432 / 158.839 / 160.079 |
| Prepare repository | 155.175 / 156.134 / 159.040 | 465.894 / 476.118 / 484.486 | 465.474 / 491.884 / 637.542 |
| Scan and create tree, including synchronous HTTP | 467.386 / 477.172 / 625.301 | 468.224 / 910.568 / 1091.941 | 468.055 / 479.496 / 490.355 |
| Create commit, including HTTP | 157.978 / 159.029 / 159.926 | 157.774 / 191.005 / 475.146 | 159.544 / 172.934 / 178.173 |
| Publish transaction, including HTTP and R2 | 1513.185 / 2098.414 / 2194.087 | 1503.229 / 1901.054 / 2261.462 | 1526.184 / 2216.130 / 2857.082 |
| Finish client working copy | 0.084 / 0.093 / 0.106 | 0.083 / 0.106 / 0.148 | 0.082 / 0.094 / 0.129 |
| All client HTTP, through response headers | 2446.804 / 3035.951 / 3128.524 | 2815.529 / 3398.490 / 3550.898 | 2780.730 / 3459.447 / 4104.385 |
| All host request handlers | 1209.016 / 1796.507 / 1891.041 | 1266.292 / 1854.370 / 2008.801 | 1228.895 / 1918.412 / 2557.055 |
| Publish validation | 0.158 / 0.284 / 0.322 | 0.147 / 0.245 / 0.329 | 0.130 / 0.231 / 0.770 |
| WAL phase, including existence check and write | 669.445 / 1060.129 / 1281.315 | 653.752 / 1029.677 / 1245.885 | 651.624 / 1049.647 / 1220.858 |
| WAL encoding only, nested in WAL phase | 0.004 / 0.008 / 0.025 | 0.004 / 0.007 / 0.023 | 0.004 / 0.009 / 0.011 |
| Index commit phase | 508.725 / 831.077 / 1178.768 | 509.909 / 626.650 / 911.761 | 534.963 / 876.423 / 1710.370 |
| Apply committed heads locally on host | 0.132 / 0.252 / 0.328 | 0.116 / 0.384 / 0.517 | 0.115 / 0.290 / 0.405 |
| Post-apply preparation for response | 0.148 / 0.424 / 2.737 | 0.170 / 0.332 / 0.408 | 0.169 / 0.332 / 0.741 |
| Index call completion to host response ready | 0.612 / 1.294 / 3.116 | 0.590 / 1.130 / 1.568 | 0.610 / 1.291 / 2.456 |
| Client scan/tree time outside HTTP-header timers | 0.839 / 0.945 / 25.063 | 0.455 / 0.614 / 14.377 | 0.433 / 9.925 / 24.323 |

“Scan and create tree” is not a pure filesystem or CPU timer: it contains object
requests issued by jj, including requests on worker threads. Its residual also
contains response-body transfer/decoding, encoding, filesystem work and tracing.
“Publish validation” covers the head-publish validation checkpoint; object
hash validation and decoding in other endpoints remain within their host
handler totals.

The index-to-response interval uses timestamps on the same host. It ends before
response-body delivery. The final request's client-minus-host duration combines
outbound and return travel plus unmeasured client/proxy work; it cannot isolate
one-way acknowledgment delivery.

## An additive latency budget

The following **means**, in milliseconds, partition each snapshot before
aggregation. Unlike the nested table, these columns can be added.

| Profile | R2 calls | Host outside R2 | HTTP outside host | Client outside HTTP timers | Total |
|---|---:|---:|---:|---:|---:|
| Warm, one writer | 1297.028 | 9.231 | 1242.021 | 3.884 | 2552.164 |
| Mixed burst, small writers | 1235.401 | 130.741 | 1548.568 | 2.623 | 2917.333 |
| Mixed steady, small writers | 1278.633 | 65.361 | 1550.421 | 3.554 | 2897.968 |

The arithmetic has no unassigned remainder: its final terms are explicit
residuals, not identified mechanisms. The HTTP-outside-host and client residuals
are not further localized:

- Warm, one writer: **1245.905 ms**, **48.82%** of mean total latency.

- Mixed burst, small writers: **1551.191 ms**, **53.17%** of mean total latency.

- Mixed steady, small writers: **1553.975 ms**, **53.62%** of mean total latency.

The host-outside-R2 column is located on the host but also only partly explained.
It includes hosted lookup/authentication, body admission and receipt, decoding,
blocking-task scheduling, local repository work, response construction and
tracing. These observations do not split CPU time from waiting. R2 durations
likewise include transport and any SDK-internal retries, not just R2 service
execution time. A residual is therefore not a measurement of network RTT or
local scan cost.

## R2 calls, request counts and bytes

Each selected small snapshot made three ObjectStore boundary calls: one WAL
existence check, one immutable WAL write, then one conditional index write.
No bucket payload reads were recorded on these paths. The existence check has
no response payload; zero payload bytes does not mean zero network traffic.

| ObjectStore call (ms) | Warm | Mixed burst | Mixed steady |
|---|---:|---:|---:|
| exists wal | 167.700 / 283.292 / 816.397 | 164.417 / 206.789 / 407.938 | 160.041 / 204.536 / 685.890 |
| put_immutable wal | 481.364 / 876.157 / 991.341 | 486.002 / 849.296 / 1032.592 | 478.968 / 737.661 / 1040.277 |
| compare_and_put index | 508.538 / 830.928 / 1178.627 | 509.739 / 626.255 / 911.647 | 534.829 / 876.288 / 1710.154 |

The next table reports per-snapshot counts and bytes as **p50 / p95 / p99**.
HTTP bytes are body lengths, excluding headers and TLS. Bucket write bytes are
attempted input payload lengths; the instrumentation does not independently
measure confirmed stored bytes or physical retransmissions.

| Per-snapshot measure | Warm | Mixed burst | Mixed steady |
|---|---:|---:|---:|
| Client HTTP requests | 8 / 8 / 8 | 10 / 10 / 10 | 10 / 10 / 10 |
| HTTP request body bytes | 1,487 / 1,487 / 1,487 | 1,885 / 1,885 / 1,885 | 1,885 / 1,885 / 1,885 |
| HTTP response body bytes | 1,639 / 1,639 / 1,639 | 2,418 / 2,418 / 2,418 | 2,418 / 2,418 / 2,418 |
| ObjectStore calls | 3 / 3 / 3 | 3 / 3 / 3 | 3 / 3 / 3 |
| Bucket read payload bytes | 0 / 0 / 0 | 0 / 0 / 0 | 0 / 0 / 0 |
| Bucket attempted write payload bytes | 2,013 / 2,013 / 2,013 | 2,407 / 2,407 / 2,407 | 2,407 / 2,407 / 2,407 |

The [machine-readable profile](snapshot-latency-profile.json) includes every
observed route's count, body bytes and client/host timings, WAL versus index
bytes, and per-writer statistics. Counts refer to completed client requests and
ObjectStore calls, not backend-internal HTTP attempts.

## Order, overlap, waits and retries

Within a small warm snapshot, the eight observed HTTP calls run sequentially:
writer claim → head read → file batch → child tree → root tree → commit →
operation upload → head publish. The mixed small writers make ten: operation
and view reads also occur during repository preparation. The observed cache
policy difference accompanies those reads; this experiment does not isolate
its causal contribution from all other differences.

Inside head publish, validation → WAL existence check → immutable WAL write →
conditional index commit → local head application → response preparation are
sequential. The mutation lock remains held across the WAL and index work; its
hold time is nested in host duration, not extra latency. History reconstruction
is outside these warmed measured snapshots.

| Interval (ms) | Warm | Mixed burst | Mixed steady |
|---|---:|---:|---:|
| Repository mutation-lock wait, summed per snapshot | 0.000 / 0.000 / 0.000 | 0.000 / 0.000 / 0.000 | 0.000 / 0.000 / 0.000 |
| Head-publish mutation-lock hold | 1202.097 / 1786.173 / 1882.470 | 1184.368 / 1559.882 / 1785.800 | 1190.577 / 1872.417 / 2368.344 |
| Publish-permit admission wait | 0.000 / 0.000 / 0.000 | 0.000 / 0.000 / 0.000 | 0.000 / 0.000 / 0.000 |

Lock waits are integer microseconds; publish-admission waits are integer
milliseconds. A recorded zero is below that timer's resolution. The reported
queue depth is the per-repository publish queue, not the host's decoded-body
queue. Body-admission wait and host-wide queue depth were not independently
instrumented. Thus these zeros do not establish that all queues were empty.

- Warm, one writer: maximum repository publish queue depth 0; observed client CAS retries 0 maximum per snapshot; observed bucket CAS conflicts 0 maximum.

- Mixed burst, small writers: maximum repository publish queue depth 0; observed client CAS retries 0 maximum per snapshot; observed bucket CAS conflicts 0 maximum.

- Mixed steady, small writers: maximum repository publish queue depth 0; observed client CAS retries 0 maximum per snapshot; observed bucket CAS conflicts 0 maximum.

R2 SDK-internal retry counts are **unknown**. Startup health polling briefly
received HTTP 503 after restarting the disposable service; that happened
before the benchmark interval and is not a publish retry.

Across repositories, work overlaps. A sweep of host timestamps and request/call
durations gives the following observations for all successful active-profile
snapshots, including the large writer. Setup, warmups, unchanged attempts and
recovery are excluded. Each attributed
repository's own request intervals were checked for overlap before using
additive per-snapshot attribution.

| Profile | Maximum overlapping host requests | Host-request overlap (s) | Maximum overlapping R2 calls | R2 overlap (s) |
|---|---:|---:|---:|---:|
| burst | 4 | 180.454 | 3 | 133.491 |
| steady | 4 | 60.197 | 3 | 51.432 |

## The large publisher

The large publisher is retained separately rather than mixed into the small
snapshot percentiles. Its first snapshot is not prewarmed like the small
writers. The following rows use all its successful snapshots:

| Profile | Samples | Total p50 / p95 / p99 (ms) | Mean R2 (ms) | Mean host outside R2 (ms) | Mean HTTP outside host (ms) | Mean client residual (ms) |
|---|---:|---:|---:|---:|---:|---:|
| burst | 25 | 9977.760 / 10965.984 / 11453.188 | 3595.406 | 2950.697 | 1403.959 | 2151.469 |
| steady | 20 | 9961.199 / 10391.925 / 11417.958 | 3446.720 | 3062.362 | 1407.958 | 2135.057 |

The file-upload endpoint's response body is 32 MiB as well as its request body.
This is observed response size and matches the existing client consuming the
normalized response bytes. A substantial client residual occurs in scan/tree
creation after header timing; body transfer/decoding is a plausible contributor,
but its share was not separately measured. The host upload duration also
includes receiving the request body and cannot be called validation CPU time.
The machine-readable profile contains its complete phase and route percentiles.

## What the evidence establishes

- Warm publish validation, WAL encoding, local head application and working-copy
  finish are short relative to the seconds spent in HTTP and R2 boundaries.
- The warm path contains eight serial HTTP requests and three serial durable
  storage calls. Small mixed-load snapshots contain ten HTTP requests.
- R2 WAL and conditional index calls account for a substantial measured part of
  snapshot latency. Backend transport versus service execution remains unknown.
- Mixed-load host work outside R2 increases, but zero measured publish waits do
  not identify its cause. Body admission, scheduling and CPU work are hypotheses
  within that interval, not separately measured explanations.
- Instrumentation overhead was not isolated with a matched uninstrumented run.
  One burst sample also has a 23.646 ms gap between its last phase checkpoint
  and the final timer; the analyzer retains it in the client residual. Logging
  or scheduling is a hypothesis for that gap.

The previous [production-envelope run](stage8-production-envelope.json) reported
small-writer burst p50/p95/p99 of 2709.0/3500.1/3819.2 ms and steady
2711.9/3900.1/4215.8 ms. It used the same workload but a different runtime build,
VM/run window and logging configuration. It is historical context, not a matched
control for the new tracing overhead or a demonstrated performance change.

## Evidence and reproduction

The measured runtime source is `ff71dc62ce56f6f3bdbf0ff1828b5da9b4d05d44`,
based on `8b802601989d7d8a538bc1fe68056e36751196f7`. Client and server CLI
SHA-256 is `9c0f544b623eee32cb893e9548f96b4101df7520bb6b09259aee45e5b7fdd3f9`.
The artifact records both embedded benchmark hashes, the pinned build image,
raw report and trace hashes, placement, and the retained analysis output.

Raw reports, complete traces, binaries, build logs, instrumentation diff and
controller scripts remain under
`/home/laurynas/.local/state/tandem-latency-profile`.
The analyzer reads retained evidence only:

```sh
python3 scripts/profile_snapshot_latency.py   /home/laurynas/.local/state/tandem-latency-profile   --output /tmp/snapshot-latency-attribution.json
python3 scripts/test_profile_snapshot_latency.py
```

Pairing uses repository identity, successful operation IDs and ordered RPC
sequences, including worker-thread requests without the snapshot span. Missing
phase/count/wait coverage and overlapping attributed requests are rejected.
The final small-writer sample selection requires the first 40 attempts to be
published; full-run unchanged attempts remain separately counted. No errors
are silently selected away.

Full mixed-run outcomes: burst: passed=True, attempts={'published': 276, 'Unchanged': 1}, steady: passed=True, attempts={'published': 140}.

Both profiles completed the existing restart, acknowledged-operation
reachability and exact-byte checks. This qualifies those benchmark fixtures,
not additional workloads. The owned VM, R2 prefix and protected profiling
configuration were removed after retaining evidence; `cleanup.json` records
verification. Production resources were retained.

