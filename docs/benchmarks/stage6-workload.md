# Stage 6 frozen workload

Frozen before implementation or tuning, from Stage 5 source
`1af39bbb7034ac366e8b3938e2d6470c9341b9ea` and its
[paired measurements](stage5-paired-publish.json).

## Supported mix

Measure idle connections at 1, 10 and 100 across 1 and 10 repositories.
These are long-lived notification connections, not active writers. At 100,
RSS growth from the same warmed host must stay within 128 MiB, with no
publishes or bucket writes caused by idle connections.

The active workload has 10 loaded repositories and four active writers in
four separate repositories: three small publishers and one uploader of a
valid 32 MiB payload. Also reject an explicitly oversized payload. Each small
publisher rewrites eight files, with three warmups and 40 measured publishes
per writer (120 measured samples total). Record each writer separately as well
as the aggregate; no writer may disappear from the result. Use deterministic
five-edit-per-second bursts (each edit rewrites the eight files) and a benchmark debounce policy permitting
at most one snapshot per second per workspace. This is not a product default.
Exercise 0.2 Hz edits separately from bursts, four writers contending in one
repository, and a slow or faulted bucket operation in another repository.

For the filesystem profile with 50 ms added per request, small-publisher
snapshot-to-ack p95 must be at most 2 seconds and p99 at most 5 seconds;
edit-to-durable p95 at most 3 seconds and p99 at most 6 seconds. For real R2,
small-publisher snapshot-to-ack p95 must be at most 5067.064042 ms (twice the
Stage 5 unloaded p95), and p99 at most 15 seconds. Record real edit-to-durable
percentiles too. Admission queue wait p99 must be at most 2 seconds, with no
starvation and at most eight retries per update. Report failures without
relaxing these limits.

## Resource envelope

Cap resident repository engines at 16, retaining live engines rather than
unsafe eviction. Keep at most four concurrent cold opens. Admit at most four
decoded request bodies of at most 64 MiB each, with permits acquired before
allocation. Idle notification streams must not consume these permits.
Limit staged data to 64 MiB per repository and 512 MiB across the host.
Limit encoded WAL to 64 MiB, rejecting before oversized allocation. Allow
one active publish per repository with at most eight queued requests, and
at most four active publishes across the host. Explicit rejection is part
of bounded admission; successful supported-workload publishes must not be
silently dropped. Record process RSS under active load, with an absolute
2 GiB ceiling on the 8 GiB validation host.

## Required observations and correctness

Record p50/p95/p99 edit-to-durable and snapshot-to-ack latency, repository
lock wait and hold, admission queue depth and wait, retries, process memory,
and bucket calls and bytes with their coverage stated. Freeze and run the
same workload before and after tuning. Retain executable identities, raw
samples, and placement. No capacity claim extends beyond the measured mix.

After drain and restart, verify exact payload bytes and all concurrent heads.
Rerun storage fault, property and deterministic simulation gates. Coalesce
notifications in bounded state while preserving eventual authoritative
refresh after missed notifications or reconnects, without moving user files.

Compare continuous edits on the same 10,000-file tree using pinned upstream
jj filesystem monitoring if its required service is available. Adopt it only
with exact-byte correctness and a measured scan-p95 gain; otherwise document
the dependency or measurement reason for deferral. Do not add a custom scanner.

## Harness semantics fixed before implementation

Edit-to-durable starts at the oldest unacknowledged edit in a coalesced batch.
The acknowledgement identifies the operation and captured generation. Versions
overwritten before capture need not be published; final generations must be.
Schedule input independently of acknowledgements: a 240-second burst contains
five edit batches per second per small writer, each rewriting eight files, then drain within 60 seconds. Retain
all attempts, rejections and timeouts; the 40 samples per writer are the first
40 measured attempts after warmup, not a selection of successful attempts.
Report the full burst separately if it produces more samples. For 0.2 Hz,
schedule 40 edits at five-second intervals after warmup, then drain.

The large uploader repeats distinct 32 MiB generations for the full small-writer
measurement window, waiting for each acknowledgement before uploading its next
generation. Record overlap and completed generations. Keep in-flight staged
bytes charged until publication succeeds or an explicitly safe rollback returns
them to staging; draining a buffer does not release its budget.

Eight queued requests excludes the active publisher. Queue wait starts when a
publish reaches admission and ends when it gets both repository and host
permits. Request bodies retain their accounting until freed, including while
queued. Idle SSE holds neither body nor publish admission permits.

The deterministic slow-bucket case blocks one repository until an unrelated
small publish completes, with a ten-second failure deadline and explicit release
in cleanup. It must prove progress before release, not merely after the slow
request completes. This injected case is separate from real-R2 latency numbers.
After drain and restart, compare exact final bytes at each workspace's explicit
revision and prove all acknowledged operations remain reachable; jj may converge
the head set, so four literal heads are not the oracle.
