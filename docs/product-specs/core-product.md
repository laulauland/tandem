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

## Out of scope (v0.1)

- authentication and tenant isolation
- UI layer
- policy/workflow automation
