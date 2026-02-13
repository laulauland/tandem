# AGENTS

Execution guide for building `tandem` from the docs in this repository.

## How to read these docs (quick)

1. Read `ARCHITECTURE.md` for system boundaries.
2. Read `docs/exec-plans/active/slice-roadmap.md` and pick the next slice.
3. Implement via failing integration test first.
4. Keep any deferred cleanup in `docs/exec-plans/tech-debt-tracker.md`.
5. When a slice is done, move a completion note into `docs/exec-plans/completed/`.

## Working style

- Implement **one vertical slice at a time**.
- Each slice starts with a **failing Rust integration test**.
- Make the test pass with the smallest correct change.
- Keep behavior aligned with stock `jj` semantics.

## Priority order

1. Slice 1: Single-agent round-trip
2. Slice 2: Two-agent visibility
3. Slice 3: Concurrent convergence
4. Slice 4: Promise pipelining
5. Slice 5: WatchHeads
6. Slice 6: Git round-trip via server-side `jj`
7. Slice 7: End-to-end multi-agent

## Testing policy

- Integration tests are the primary source of truth.
- Local deterministic tests first; cross-machine tests second.
- Use `sprites.dev` / `exe.dev` for distributed smoke tests.
- Keep networked tests opt-in (ignored by default / env-gated).

## Debug policy

Add structured tracing early so we do not sprinkle debug prints later.

Recommended flags:

- `--tandem-debug`
- `--tandem-debug-format pretty|json`
- `--tandem-debug-file <path>`
- `--tandem-debug-filter <filter>`

Minimum events to emit:

- command lifecycle
- RPC lifecycle
- object read/write
- CAS heads success/failure + retries
- watcher subscribe/notify/reconnect
