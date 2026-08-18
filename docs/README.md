# Docs

Minimal docs structure for the project:

- `../AGENTS.md` — execution/testing/debugging conventions
- `../ARCHITECTURE.md` — system shape and boundaries
- `design-docs/` — durable technical decisions
- `exec-plans/` — active/completed implementation plans
- `product-specs/` — concise product intent and scope

## Build notes

`cargo build` is the whole story: a Rust toolchain, no schema compiler, and no
code-generation step. The wire types are plain Rust in `src/wire.rs`.

This docs set is the canonical source of project direction and architecture.
