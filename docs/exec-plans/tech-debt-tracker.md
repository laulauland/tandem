# Tech Debt Tracker

## Resolved (2026-02-15)

- [x] ~~Integrate real `jj-lib` store traits (`Backend`, `OpStore`, `OpHeadsStore`) on the client~~ → resolved
- [x] ~~Replace line-JSON RPC transport with Cap'n Proto and promise pipelining~~ → resolved
- [x] ~~Full byte-compatible object/op/view storage semantics~~ → resolved
- [x] ~~Remove test-only CAS delay knob (`TANDEM_TEST_DELAY_BEFORE_UPDATE_MS`)~~ → removed
- [x] ~~Clean up `opensrc/` directory leftover~~ → removed 2026-02-15

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
- Verify object write idempotency contract and error codes
- Clean shutdown for server (Ctrl+C signal handling)
- Add distributed smoke-test harness (`sprites.dev` / `exe.dev`) with env-gated CI step

### P3 (performance, not correctness)

- Client-side object cache for repeated reads (needed at scale)
- Index store optimization (currently rebuilds on every jj command)
- Batch RPC calls for `jj log` with many commits
