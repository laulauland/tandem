# Normal prepared snapshots on Frankfurt and R2

The daemon now uses the prepared-publish endpoint through its ordinary stock
jj snapshot transaction. This follows the [isolated experiment](combined-publish.md).
The current behavior is owned by [the architecture](../../ARCHITECTURE.md);
production placement and recovery procedures belong to [operations](../operations.md).

## Local qualification

The full workspace suite passes 354 tests, including storage properties and
deterministic simulations. Independent implementation reviews are clean.
Normal snapshot tests verify one prepared mutation without individual object
or metadata uploads, fresh-client exact bytes, nested files, symlinks, existing jj tree conflicts, and the
existing repeated 10,000-file workload. Same-daemon retries cover lease loss,
failed WAL writes and failed index writes. A descendant-rebase test interrupts
lease renewal after scanning; disabling only the final lease check makes that
test fail. A successful retry survives bucket-only reconstruction with exact
ancestor and descendant bytes.

Concurrent stacked tests force both publication orders. Both acknowledged
operations remain reachable, and both versions of B's change remain in the
current served commit graph with exact bytes. A's descendant rebase contains
A's rewritten bytes and B's previous bytes; B's concurrent rewrite contains
B's edited bytes. These versions have the same jj change identity and are
reported as divergent. The daemon marks B stale without touching its edited
files. An explicit workspace update can select either divergent version and
reports the divergence. A separate schedule overlaps A's snapshot with B's
explicit jj rebase.

A graph head becoming an ancestor is now allowed only when the server proves
its history remains reachable. That permission does not authorize moving
another workspace pointer to an unrelated change.

## Measurement boundary

One prepared request replaces the individual mutation uploads. It does not
remove writer-lease requests, head reads, or jj's reconciliation of divergent
operation heads. A stacked snapshot can therefore send a normal head update
before its prepared publish. Request totals must include that prelude rather
than describing the entire snapshot as one HTTP call.

The frozen GNU/Linux candidate was built from
`452db17d0201d322d793e5ca7c11c51982c9fb16`; its SHA-256 is
`0a65a9854de082bde3f59bebfe5fdec98d94a5704e65b526b0687d7abd610eaa`.
Subsequent source changes refine deterministic test oracles and documentation;
they do not change the production source in that binary.

Remote qualification and measurement evidence is retained under
`/home/laurynas/.local/state/tandem-frankfurt-snapshots`.

## Frankfurt qualification and measured warm workload

Three distinct exe.dev Frankfurt VMs used the same frozen binary and an
isolated prefix in the Western Europe R2 bucket. Persistent daemons passed
stacked rewrites, explicit rebase, concurrent publication and two empty-cache
host reconstructions. Fresh clients verified every recorded acknowledged
operation and exact file bytes. The original daemon processes published new
files after the first reconstruction; those publishes survived the second.

The warm run used one Frankfurt client and a Frankfurt host, three warmups and
40 measured snapshots of eight deterministic files. It measures the current
normal daemon path, including scanning; it is not a matched before/after run.

| Interval, ms | Mean | p50 | p95 | p99 |
|---|---:|---:|---:|---:|
| Full snapshot | 542.34 | 512.45 | 662.54 | 1034.49 |
| WAL PUT through response headers | 186.18 | 178.75 | 247.08 | 294.43 |
| Index PUT through response headers | 226.34 | 196.58 | 362.29 | 732.26 |

Every measured snapshot made three client requests: writer claim, head read
and prepared publish. Mean request/response body bytes were 1779.60/722.93;
headers and TLS are excluded. Each snapshot made three bucket calls: the WAL
existence HEAD, WAL PUT and index PUT, with 2018.58 attempted write bytes and
zero object-body read bytes on average. All 80 PUTs succeeded on their first
attempt with status 200 and an ETag. No retries or ETag follow-up HEADs occurred.
All used previously observed socket pairs, consistent with connection reuse.

The mean additive budget is 482.37 ms in bucket operations, 7.56 ms in other
host work, 31.93 ms in client HTTP time outside host handling, and 20.48 ms of
local residual. These components account for the total by subtraction; the
residual is not a pure CPU measurement. It includes local jj preparation,
response handling, filesystem work and tracing. The durable sequence remains
WAL check/write, conditional index commit, local host application, then
acknowledgment. Raw nested timings are retained in the [data artifact](normal-prepared-snapshots.json).
DNS/TCP/TLS timings and the division between network transit and R2 execution
remain unexposed. Nearest-rank p99 is the maximum of these 40 samples.

Host checkpoint p50/p95/p99 timings were 0.150/0.205/0.219 ms for the
head-update validation phase, 0.141/0.178/0.872 ms for local application, and
0.229/0.271/0.313 ms for acknowledgment preparation. The validation checkpoint
excludes the earlier prepared-object identity checks and staging. The inclusive
WAL phase measured 250.619/313.564/364.102 ms; the separate existence HEAD
averaged 69.011 ms. Exposed admission and lock-wait counters were zero at their
recorded resolution. The update lock was held for 483.384 ms on average,
including storage. These nested phases must not be added to the additive
budget above. Index completion to response readiness averaged 0.750 ms.

## Production replacement

The qualified host became `tandem-frankfurt`. Dallas was fenced before the
replacement opened the production bucket. A qualification file published by
the previous binary was recovered through a private endpoint with its existing
scoped credential; the existing owner credential also remained valid. The
replacement accepted another normal snapshot before the domains moved.

Both public domains then passed HTTPS, website, installer and exact download
checksum checks. A fresh public normal snapshot survived a supervised restart
after SIGKILL and a subsequent empty-cache reconstruction. The active and
retained signing configuration remained identical. The old Dallas VM and the
old production cache were removed after these checks. Current deployment facts
and the preserved configuration/manifest locations are owned by
[operations](../operations.md).

Cleanup independently verified removal of both disposable client VMs, all 238
objects under the exact validation R2 prefix, three validation caches and five
temporary credential files. The promoted production host, production bucket
prefix, signing configuration and protected production proof credentials remain.
