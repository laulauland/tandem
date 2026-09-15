# Local publish preparation gate — 2026-09-15

The ordinary one-file case passes, but commit identity is not independent of
the server's existing jj metadata. The combined-request experiment stopped at
this gate, as requested when local preparation produces different history.
No combined endpoint or production behavior was added, and there is no
six-to-one request or latency-reduction claim.

## Measured local results

The integration experiment uses two real native jj Git backends and the
existing repository ingestion API, with the same jj settings. It does not
substitute a hash function or a mock server.

- For a root-level edited file, native jj prepares file bytes, the tree, commit,
  view and operation. All five identities match the server's returned
  identities; normalized payloads match byte for byte. This is a preparation
  and ingestion check, not a published-head or fresh-client acceptance test.
- A second fixture uses two independent native repositories rewriting the same
  shared change. Their prior file contents differ; their edited file bytes,
  change ID, description, author and committer second agree. Their rewrite
  predecessor IDs differ. The timestamp and change ID are fixed test inputs;
  this deliberately forces a collision and does not measure its frequency.
- Both local engines produce Git commit ID
  `8f03b6cd689d09f704f7451c62f0747b67334050` for the final edit. The server
  accepts the first identity unchanged. On the second rewrite it returns
  `e44f9810b723634225bd41ba6142b41b0e4ad430` and reduces the committer timestamp
  by exactly 1,000 ms, preserving the second rewrite's predecessor metadata.
- Looking up the second client's original ID succeeds but returns the first
  rewrite's predecessor metadata. Retrying the second original commit payload
  returns the same normalized server ID and bytes as its first attempt.

The initial assertion that every locally prepared commit ID remained unchanged
failed on this fixture. The retained regression asserts the counterexample's
exact normalization and retry behavior, rather than leaving a failing test.

## Why this matters

jj 0.38's Git backend does not include rewrite predecessors in Git's commit
hash. It stores them as extra jj metadata. When a Git commit ID already has
different extra metadata, its existing write path decrements the committer
second until the ID is available. The default change-ID header is enabled in
both test repositories; the mismatch does not depend on disabling it or on
configuration drift.

A view prepared against the second client's original commit ID would identify
the first rewrite's metadata on this server. Changing that reference would
also change the view and dependent operation identity. This is the concrete
design issue with assuming that a small acknowledgment can unconditionally
confirm the entire locally prepared graph unchanged.

The experiment does not establish how often this collision occurs in normal
workloads, or that unrelated changes from distinct agents collide. It establishes
that matching settings and native local jj alone do not guarantee identity
parity when the server has additional rewrite metadata.

## Verification and remaining scope

```sh
cargo test -p jj-tandem --test local_publish_parity
```

No combined publish was sent. Fresh-client publication, concurrent combined
publishes, crash windows, lost combined responses, invalid/unauthorized bundles
and the matched six-versus-one exe.dev/R2 comparison therefore remain untested.
Those later gates were not represented as passed. The independent
[R2 HTTP-attempt measurement](r2-http-attempts.md) is complete and includes its
own disposable-host evidence and cleanup; it is not a combined-request result.
