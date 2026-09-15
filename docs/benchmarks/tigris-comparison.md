# Tigris versus R2 on the same Frankfurt host

Tigris multi-region EUR was faster than R2 WEUR for this warm single-writer workload on the same disposable exe.dev Frankfurt VM. The application, adapter, retry policy, durability order, instrumentation, workload and client placement were unchanged.

| Provider | Snapshot mean / p50 / p95 / p99, ms | WAL PUT mean / p50 / p95 / p99, ms | Index PUT mean / p50 / p95 / p99, ms |
|---|---|---|---|
| Tigris multi-region EUR | 481.11 / 473.11 / 540.38 / 553.24 | 71.20 / 62.73 / 126.40 / 144.06 | 41.08 / 39.34 / 47.14 / 82.58 |
| R2 WEUR, same VM control | 772.57 / 750.62 / 893.43 / 1247.70 | 182.98 / 163.70 / 234.27 / 673.27 | 175.03 / 164.60 / 223.28 / 348.04 |

Each run used three warmups and 40 measured snapshots, eight deterministic edited files, disk cache enabled, a separately initialized server cache, and 2 CPU/8 GiB on td-profile-4bd1c0836a (Frankfurt). Tigris ran first, followed by R2; this is one sequential pair, not a randomized repeated study. Nearest-rank p99 is the maximum at this sample size. CLI SHA256: 7865377a3c7cb2c5cb9725613e22c1e2068fe48e5899245c9b108d8998a29b70. Benchmark SHA256: 0a88c6a7ac30e8528b061027c6fd6123f5e35b8b2c2a73162219560e97c50b4f.

Both runs made eight client requests and three bucket operations per snapshot. Mean request-body bytes 1485.6, response-body bytes 1637.525, and attempted bucket-write bytes 2011.575 are identical, as are their distributions. Headers and TLS overhead are excluded. IDs/timestamps differ between independently initialized repositories but file content is identical.

All 80 write PUTs per provider returned 200 with ETags, with zero retry, zero ETag follow-up HEAD, and previously observed local/remote socket pairs. The separate WAL existence HEAD ran once per publish. No bucket object-body reads occurred. Lock and publish-admission waits were zero at timer resolution.

| Mean additive component | Tigris, ms | R2, ms |
|---|---:|---:|
| Bucket operations | 124.026 | 415.928 |
| Host outside bucket | 10.736 | 10.637 |
| HTTP outside host | 344.277 | 344.126 |
| Local residual | 2.069 | 1.881 |
| Total | 481.107 | 772.572 |

Bucket-operation means were 11.076/71.532/41.418 ms for Tigris and57.147/183.389/175.392 ms for R2 (WAL existence/WAL write/index write). The ~292 ms bucket difference explains the end-to-end improvement; client/proxy cost stayed essentially unchanged. DNS/TCP/TLS durations, pool-hit counters, lower-level transparent replays, and provider internal execution time remain unobserved. Repeated socket pairs suggest reuse. Network transit versus provider processing remains unresolved. No cache behavior was changed.

## Storage contract and configuration

The default Tigris Global bucket would not satisfy globally current conditional writes: its cross-region consistency is eventual. We deliberately chose private STANDARD **multi-region EUR**, which documents globally strong GET/LIST/conditional-write semantics and region redundancy. The actual CLI bucket details report Multi-region(eur), and S3 GetBucketLocation returns eur. Snapshotting, soft deletion, lifecycle expiry, notifications and paid-tier options were not enabled. Tigris selects Amsterdam/Frankfurt candidate regions for EUR.

Before benchmarking, the existing `s3_backend_honours_the_bucket_contract` test passed against this real bucket using Tandem's existing adapter: first immutable write succeeds, second preserves first bytes, index create-if-absent conflicts, stale ETag conflicts, fresh ETag succeeds, and read bytes/ETag match. A separate AWS CLI probe verified typed PreconditionFailed for immutable overwrite and stale CAS, then successful fresh CAS, immediate exact-byte GET and LIST-after-write. Those CLI durations include process startup and are contract evidence, not benchmark timings.

The selected multi-region mode documents synchronous metadata replication before write completion and persistent storage, with regional redundancy. The service's long-term numeric durability SLA was not established by this experiment; short contract/recovery tests cannot prove it. The documented Standard 99.99% figure is **availability**, not durability. No equivalence of numerical R2/Tigris durability SLA is claimed. The application retains WAL-before-index-before-acknowledgment ordering unchanged.

Official sources:
- [Conditional operations](https://www.tigrisdata.com/docs/objects/conditionals/)
- [Consistency including LIST-after-write](https://www.tigrisdata.com/docs/concepts/consistency/)
- [Bucket locations, replication and cross-region guarantees](https://www.tigrisdata.com/docs/buckets/locations/)
- [Storage architecture](https://www.tigrisdata.com/docs/concepts/architecture/)
- [Storage tiers](https://www.tigrisdata.com/docs/objects/tiers/)
- [Pricing and free tier](https://www.tigrisdata.com/pricing/)

Official source snapshots are retained beside this report. Tigris CLI OAuth was already configured; no MCP tool was callable. A new bucket-scoped ReadWrite key was created solely for the experiment. The organization had zero buckets at the starting credential probe. The workload is far below the published 5 GB Standard storage/10,000 Class A/100,000 Class B allowance; no billing upgrade was made.

Evidence: comparison.json contains complete attempt and snapshot attributions, source hashes and trace paths. contract-test.json/log records the real adapter test. wire-probe.json records independent CAS/read/list checks. bucket.json and infrastructure.json identify exact configuration and owned resources; protected credentials live outside the evidence and are removed during cleanup. Raw SDK-attempt and host/client traces are retained separately for Tigris and R2 control.

An additional recovery drill passed: restart against a new empty host cache, clone into a new client cache, and read all eight final published files with exact bytes `round 42, file N` plus newline. recovery.json records each digest. Initial controller probes used the wrong host-level credential and then the wrong catalog JSON field name; those were controller errors corrected before the successful fresh-cache run, not storage failures.

Cleanup passed: disposable VM removed; all 60 Tigris objects and51 R2 control objects deleted; both listings empty; Tigris bucket removed; experiment access key revoked and absent from key listing; all temporary protected key/config files removed. Production resources were untouched.


The [measurement artifact](tigris-comparison.json) retains the full tables and source hashes. Raw evidence and controller scripts remain under `/home/laurynas/.local/state/tandem-tigris-comparison`.
