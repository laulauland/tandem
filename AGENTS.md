# AGENTS

Tandem is jj workspaces over the network. Read only as far as the task needs:

- [README.md](README.md) — product and user entry point.
- [ARCHITECTURE.md](ARCHITECTURE.md) — canonical current boundaries and state
  ownership.
- [docs/reliability.md](docs/reliability.md) — publish ordering, WAL, crash
  recovery, and storage risks.
- [docs/testing.md](docs/testing.md) — unit, property, deterministic simulation,
  and integration test placement.
- [docs/operations.md](docs/operations.md) — deployment, monitoring, backup,
  restore, and incident recovery.
- [docs/self-hosting.md](docs/self-hosting.md) — host setup and workspace access.
- [docs/images/README.md](docs/images/README.md) — prepared agent containers.

Use code and generated help for implementation-derived detail. Run
`tandem --help` or inspect the relevant module; do not copy
CLI flags, routes, traits, source trees, or test lists into prose.

## Non-negotiable engineering rules

1. Use `jj`, never Git, for repository operations. Inspect `jj status` and
   `jj log` before editing; preserve existing work and create a child revision
   when work needs isolation. Use conventional commit descriptions.
2. The client is stock jj with remote store implementations. Do not invent
   custom equivalents of jj commands.
3. The bucket WAL and index are the durable authority. A publish is not
   acknowledged until its reachable data and head set are durable. Never move
   a local write ahead of the bucket commit; see the reliability contract.
4. jj's server-side op-heads store is the live head authority. The Tandem
   metadata sidecar is not a second authority, and head reads never reconcile
   or publish.
5. Preserve all concurrent heads and let jj converge them. Last-writer-wins is
   a correctness failure.
6. One workspace has one active writer. A daemon may mark a workspace stale,
   but must not move files with `workspace update-stale` on the user's behalf.
7. Authentication and publish-scope checks are server-enforced. Never log or
   persist bearer tokens in images, evidence, command lines, or fixtures.
8. Integration acceptance claims about repository content must assert exact
   file bytes, not descriptions alone.
9. Tandem-owned help and argument errors must work without a server and name
   the failed address or missing input where relevant.
10. Use structured tracing and existing fault seams; do not add ad-hoc debug
    prints, sleeps, or process-wide test controls.

## Change workflow

- Start with a failing test in the home selected by
  [docs/testing.md](docs/testing.md). Keep networked tests opt-in.
- Prefer the smallest change that makes the invariant true. Remove superseded
  paths and prose instead of maintaining compatibility scaffolding nobody
  requested.
- Keep each durable fact under one documentation owner. Historical documents
  do not become current references.
- Run `python3 scripts/check_docs.py` after documentation changes. It checks
  local links, repo-path references, and retired terminology in active docs.
- Run focused package tests while iterating and `cargo test --workspace` before handoff. For storage
  or concurrency changes, include the relevant property and DST targets.

Run `python3 scripts/check_workspace.py` after manifest changes; production
dependencies must remain acyclic and respect the client/server split.

User-facing crate: `jj-tandem`; binary: `tandem`. All production crates use the
root's lockstep pre-1.0 version. Release and distributed
verification procedures are executable repo-local skills under
`.agents/skills/`.
