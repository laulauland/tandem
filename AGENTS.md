# AGENTS

Execution guide for building `tandem` from the docs in this repository.

## How to read these docs

1. Read `ARCHITECTURE.md` for system boundaries.
2. Read `docs/design-docs/workflow.md` for the concrete orchestrator→agents→git workflow.
3. Read `docs/design-docs/jj-lib-integration.md` for trait signatures and registration.
4. Read `docs/exec-plans/active/slice-roadmap.md` and pick the next slice.
5. Implement via failing integration test first.
6. Keep any deferred cleanup in `docs/exec-plans/tech-debt-tracker.md`.
7. When a slice is done, move a completion note into `docs/exec-plans/completed/`.

## What tandem is

Tandem applies a **server-client model to jj's store layer**. The server hosts
a normal jj+git colocated repo. Agents on remote machines use the `tandem`
binary (which embeds jj-cli with a custom tandem backend) to read and write
objects over Cap'n Proto RPC.

The server is the **point of origin** — it's where git operations happen
(`jj git push`, `jj git fetch`, `gh pr create`). The orchestrator/teamlead
runs these on the server to ship code upstream. Eventually the tandem server
becomes THE source of truth, with GitHub as a mirror.

## Single binary, two modes

```
tandem serve --listen <addr> --repo <path>    # server mode
tandem [jj args...]                           # client mode (stock jj via CliRunner)
```

The client mode is `CliRunner::init().add_store_factories(tandem_factories()).run()`.
All stock jj commands work transparently: `tandem new`, `tandem log`, `tandem diff`,
`tandem cat`, `tandem bookmark create` are all jj commands running through our binary.

Server mode embeds jj-lib and uses the Git backend internally. When a client
calls `putObject(file, bytes)`, the server stores the object. Objects are real
jj-compatible blobs — `jj git push` on the server just works.

## Critical invariants

1. **The client is stock `jj`.** Tandem implements jj-lib's `Backend`, `OpStore`,
   and `OpHeadsStore` traits as Cap'n Proto RPC stubs. There is no custom
   `tandem new/log/describe/diff` CLI — those are all jj commands.

2. **Tests assert on file bytes, not descriptions.** Every integration test
   must verify file content round-trips correctly via `jj cat`. Description-only
   assertions are insufficient (this is how v0 went wrong).

3. **Help text works without a server.** `tandem --help`, `tandem serve --help`,
   and `tandem` with no args must print usage locally. Error messages must
   suggest alternatives for unknown commands and include addresses for
   connection failures.

## Help text and error handling (P0)

These are required, not nice-to-haves. The v0 QA found agents spend 50% of
their time guessing commands when help is missing.

- `tandem --help` — prints usage without server connection
- `tandem serve --help` — explains `--listen` and `--repo` flags
- `tandem` with no args — prints usage, not a cryptic error
- Unknown commands — suggest alternatives ("did you mean `new`?")
- Connection errors — include the address that was tried
- Missing args — say what's needed ("serve requires `--listen <addr>`")
- `TANDEM_SERVER` env var — fallback for `--server` flag on client commands
- `TANDEM_WORKSPACE` env var — workspace name (already exists from v0)

## Workflow

See `docs/design-docs/workflow.md` for the full picture. Summary:

1. **Orchestrator** sets up server: `tandem serve --listen 0.0.0.0:13013 --repo /srv/project`
2. **Agents** init workspaces: `tandem init --tandem-server=host:13013 ~/work/project`
3. **Agents** use stock jj commands: write files, `tandem new -m "feat: add auth"`, etc.
4. **Agents** see each other's files: `tandem cat -r <other-commit> src/auth.rs`
5. **Orchestrator** ships from server: `jj bookmark create main -r <tip>`, `jj git push`

Git operations are server-only in v1. Agents never touch git directly.

## V0 → V1 migration

The v0 prototype built a custom CLI that stored description-only JSON blobs.
It proved the transport (Cap'n Proto), coordination (CAS heads), and notification
(watchHeads) layers work. See `docs/exec-plans/completed/v0-prototype-slices.md`.

V1 replaces the custom CLI with jj-lib trait implementations. What carries over:
- `schema/tandem.capnp` — unchanged
- `build.rs` — unchanged
- Server-side `store::Server` RPC handler — mostly unchanged
- CAS head coordination — unchanged
- WatchHeads callback system — unchanged

What gets replaced:
- Client: custom `tandem new/log/describe/diff` → jj-lib `Backend`/`OpStore`/`OpHeadsStore`
- Server: `CommitObject` JSON → real jj protobuf objects passed through as bytes
- Server: `apply_mirror_update` (jj CLI shelling) → direct content-addressed storage
- Tests: description assertions → file byte assertions via `jj cat`

## Priority order

1. Slice 1: Single-agent file round-trip (jj-lib Backend impl)
2. Slice 2: Two-agent file visibility
3. Slice 3: Concurrent file writes converge
4. Slice 4: Promise pipelining for object writes
5. Slice 5: WatchHeads with file awareness
6. Slice 6: Git round-trip with real files
7. Slice 7: End-to-end multi-agent with git shipping
8. Slice 8: Bookmark management via RPC
9. Slice 9: CLI help and agent discoverability

## Testing policy

- Integration tests are the primary source of truth.
- Tests use the `tandem` binary which runs jj commands — never a separate jj binary.
- Acceptance criteria assert on **file bytes** via `jj cat`, not just log descriptions.
- Local deterministic tests first; cross-machine tests second.
- Use `sprites.dev` / `exe.dev` for distributed smoke tests.
- Keep networked tests opt-in (ignored by default / env-gated).

## QA policy

- After each major milestone, run agent-based QA (see `qa/`).
- QA uses **subagent programs**, not shell scripts — agents evaluate usability.
- Naive agent (zero-docs trial-and-error) tests discoverability.
- Workflow agent tests realistic multi-agent file collaboration.
- Stress agent tests concurrent write correctness.
- Reports go to `qa/v1/REPORT.md` (compare against `qa/REPORT.md` for v0 baseline).
- Use opus for all implementation and evaluation models.

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
- object read/write (kind, id, size)
- CAS heads success/failure + retries
- watcher subscribe/notify/reconnect
