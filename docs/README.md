# Docs

Minimal docs structure for the project:

- `../AGENTS.md` — execution/testing/debugging conventions
- `../ARCHITECTURE.md` — system shape and boundaries
- `design-docs/` — durable technical decisions
- `exec-plans/` — active/completed implementation plans
- `product-specs/` — concise product intent and scope

## Build and schema-binding notes

- End users do **not** need a system `capnp` binary to install/build tandem.
- `build.rs` attempts to compile `schema/tandem.capnp` when `capnp` is available.
- If `capnp` is missing, `build.rs` falls back to checked-in generated bindings at
  `src/tandem_capnp.rs`.

Maintainers changing the schema should regenerate checked-in bindings via:

```bash
TANDEM_REGENERATE_BINDINGS=1 cargo build
```

This docs set is the canonical source of project direction and architecture.
