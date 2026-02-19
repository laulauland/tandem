# Core Product Spec

## One-liner

`tandem` lets multiple agents/machines use jj workspaces against a shared remote jj store, as if they shared a filesystem.

## Primary users

- AI/code agents collaborating concurrently
- engineers using multiple machines

## Must-have outcomes

- same repo state visible across clients
- no stale workspace workflow
- safe concurrent writes (no lost updates)
- server remains plain jj+git compatible

## Out of scope

- multi-tenant isolation and user/role auth model
- UI layer
- policy/workflow automation

## Planned

- **Server lifecycle management** — `tandem up/down/status/logs` for daemon
  management without systemd. See `docs/design-docs/server-lifecycle.md`.
- **Token auth** — bearer token on Cap'n Proto port. Single shared secret,
  no user/role model. Required for servers on public networks.
