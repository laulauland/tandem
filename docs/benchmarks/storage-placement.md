# Matched host-placement measurement

The same warm workload was materially faster with the Tandem host in Frankfurt than Dallas while retaining the same Cloudflare R2 bucket. A fresh Dallas control reproduced the previous Dallas result closely. This establishes a placement-sensitive cost; it does not identify how much of an individual R2 request is network transit versus internal R2 execution.

| Run (40 snapshots each) | Snapshot mean / p50 / p95 / p99, ms | WAL PUT mean / p50 / p95 / p99, ms | Index PUT mean / p50 / p95 / p99, ms |
|---|---|---|---|
| Prior Dallas | 2496.83 / 2436.08 / 2899.92 / 3110.30 | 521.99 / 490.37 / 688.81 / 939.20 | 574.93 / 507.43 / 809.06 / 1230.52 |
| Frankfurt | 815.54 / 742.09 / 1082.84 / 1803.35 | 178.55 / 154.22 / 240.32 / 701.66 | 196.65 / 170.87 / 376.47 / 492.49 |
| Fresh Dallas control | 2524.27 / 2412.64 / 2993.59 / 3437.80 | 566.55 / 482.76 / 1055.77 / 1163.84 | 537.55 / 503.70 / 593.78 / 1523.38 |

Compared with the fresh Dallas control, Frankfurt reduces mean snapshot time by 67.69%, WAL PUT header latency by 68.49%, and index PUT header latency by 63.42%. These are sequential runs with one VM per new placement, not randomized repeated placement trials. Nearest-rank p99 equals the maximum for 40 samples.

Controls: unchanged frozen CLI SHA256 7865377a3c7cb2c5cb9725613e22c1e2068fe48e5899245c9b108d8998a29b70 and benchmark SHA256 0a88c6a7ac30e8528b061027c6fd6123f5e35b8b2c2a73162219560e97c50b4f; same controller machine; same three warmups and 40 measured eight-file snapshots; disk cache enabled; same 2 CPU/8 GiB exe.dev host sizing; unchanged R2 bucket tandem-native, actual location WEUR, unique ephemeral prefix per run. File content is identical and deterministic across runs; jj identities/timestamps necessarily differ. HTTP request and response payload byte distributions and bucket write payload byte distributions are exactly equal across all three runs (means 1485.6, 1637.525, and 2011.575 bytes per snapshot). Headers and TLS bytes are excluded.

Each measured snapshot makes eight client HTTP requests and three bucket calls: a WAL existence HEAD, immutable WAL PUT, and conditional index PUT. In every run all 80 write PUTs return HTTP200 plus ETag on their first attempt, with zero SDK retry and zero ETag follow-up HEAD. All show previously observed local/remote socket pairs. This is evidence consistent with reuse, not a pool-hit counter. Every WAL existence HEAD in the Frankfurt run returns 404 on its first attempt; those are separate required existence checks, not ETag fallback. No bucket object payload reads occur and caching is unchanged.

Mean additive budget, Frankfurt versus fresh Dallas:

| Component | Frankfurt, ms | Dallas, ms |
|---|---:|---:|
| Bucket operations | 461.163 | 1308.723 |
| Host work outside bucket calls | 10.434 | 8.968 |
| Client HTTP time outside host handler | 341.839 | 1204.052 |
| Local residual | 2.103 | 2.531 |
| Total | 815.539 | 2524.274 |

The end-to-end improvement includes both host-to-storage placement and the controller's path to the host/proxy. The server-side connector PUT timers independently establish the storage-facing change. Frankfurt's mean bucket time consists of WAL existence 85.285 ms, WAL write178.903 ms, and index write196.975 ms. One Frankfurt existence HEAD takes 1037.7 ms; this is a successful first-attempt 404 rather than a retry. Host validation/local application remain small and locks/admission record zero wait at timer resolution in these single-writer runs.

Cloudflare read-only adaptive operation queries are retained in each run's cloudflare-operations.json. Their coverage includes setup and warmup and may lag; these are not substitutes for exact SDK attempt logs. Frankfurt queries report eyeballRegion EEUR while actual bucket metadata remains WEUR; this does not expose internal processing location. Bucket metadata and schema snapshots are copied from the immediately preceding instrumentation experiment; no bucket configuration was changed.

DNS/TCP/TLS durations, direct connection-pool-hit counters, lower-transport transparent replays, and R2 service-execution durations remain unavailable from the connector. Network versus R2 processing remains unresolved. No behavior, caching, production deployment, or durability rule changed.

The subsequent [Tigris comparison](tigris-comparison.md) measures a second provider on one Frankfurt VM with a fresh R2 control. It records the conditional-write contract, selected consistency mode, cold recovery and remaining long-term durability uncertainty.

Evidence: comparison.json carries full summaries, exact infrastructure/build identities and per-run source directories. Each run retains raw client/server structured events, benchmark output, per-attempt attribution, snapshot attribution, Cloudflare operation aggregates and cleanup manifest. Ephemeral VMs td-profile-43f8278363 (Frankfurt) and td-profile-73a6e8c470 (Dallas) and their exact profiles/43f8278363/ and profiles/73a6e8c470/ prefixes are owned by this experiment. Account region preference was captured as dal and restored immediately after provisioning each host; existing production VMs were not moved.

Cleanup completed successfully for both runs: both VMs absent, 51 objects deleted from each exact prefix, zero remaining objects, and both protected temporary environment files removed. Source hashes for both analyzers’ retained inputs verify.


The [measurement artifact](storage-placement.json) retains the full tables and source hashes. Raw evidence and controller scripts remain under `/home/laurynas/.local/state/tandem-storage-placement`.
