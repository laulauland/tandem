# Tech Debt Tracker

## Resolved

- [x] ~~Integrate real `jj-lib` store traits (`Backend`, `OpStore`, `OpHeadsStore`) on the client~~ → resolved
- [x] ~~Replace line-JSON RPC transport with Cap'n Proto~~ → resolved
- [x] ~~Full byte-compatible object/op/view storage semantics~~ → resolved
- [x] ~~Remove test-only CAS delay knob (`TANDEM_TEST_DELAY_BEFORE_UPDATE_MS`)~~ → removed
- [x] ~~Clean up `opensrc/` directory leftover~~ → removed 2026-02-15
- [x] ~~Dual head authority in server state management~~ → resolved via Option C (`docs/exec-plans/completed/option-c-jj-lib-head-authority.md`) on 2026-02-22
- [x] ~~Tighten Cap'n Proto compatibility checks on connect (`RepoInfo` protocol/capability validation)~~ → resolved by slice 18 (`tests/slice18_repo_info_compat.rs`) in `docs/exec-plans/capnp-transport-tightening.md` on 2026-02-23
- [x] ~~Exploit client-side pipelining on hot write paths~~ → resolved for TCP-first scope via bounded RPC in-flight dispatch + optimistic op-head version caching (slice 20 + benchmark artifacts) on 2026-02-23

## Known issues

### P1 (blocks production use)

- **Flaky 5-agent concurrent test under full cargo test load**
  - `tests/slice3_concurrent_convergence.rs::five_agent_concurrent_convergence`
  - Intermittent failures when running full test suite (not in isolation)
  - Hypothesis: port contention or filesystem race during concurrent server cleanup
  - Workaround: test passes reliably in isolation

- **fsmonitor.backend=none not auto-configured**
  - Users with watchman installed must pass `--config-toml='core.fsmonitor="none"'` to jj commands
  - Without it, jj tries to use watchman and fails (tandem workspaces don't support fsmonitor)
  - Should be auto-configured in `.jj/repo/config.toml` during `tandem init`

### P2 (polish)

- Define stable tracing event schema (`command_id`, `rpc_id`, `workspace`, `latency_ms`)
- Add redaction rules for logs (paths, tokens, secrets)
- Decide reconnect/backoff defaults for `watchHeads`
- Add transport compatibility path for restricted VM sandboxes (WSS and SSH-exec transports) — deferred follow-on after TCP-first hardening completion (`docs/exec-plans/capnp-transport-tightening.md`)
- Verify object write idempotency contract and error codes
- Clean shutdown for server (Ctrl+C signal handling) — now part of server lifecycle feature, see `docs/exec-plans/active/server-lifecycle.md` slice 10
- Add distributed smoke-test harness (`sprites.dev` / `exe.dev`) with env-gated CI step
- Control socket protocol design — finalize HTTP-over-Unix-socket vs alternatives, see `docs/design-docs/server-lifecycle.md`
- Capnp token auth handshake design — how to validate bearer token during capnp connection setup

### P3 (performance, not correctness)

- Stretch TCP benchmark targets from early drafts remain unmet (`>=20%` commit-path p95 and `>=1.5x` contention throughput); current latest artifacts show modest gains (~3-5% p95 at injected RTT, ~1.00x throughput geometric mean)
- Client-side object cache for repeated reads (needed at scale)
- Index store optimization (currently rebuilds on every jj command)
- Batch RPC calls for `jj log` with many commits
