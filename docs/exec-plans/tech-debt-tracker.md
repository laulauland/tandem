# Tech Debt Tracker

## Initial items

- [ ] Define stable tracing event schema (`command_id`, `rpc_id`, `workspace`, `latency_ms`).
- [ ] Add redaction rules for logs (paths, tokens, secrets).
- [ ] Decide reconnect/backoff defaults for `watchHeads`.
- [ ] Verify object write idempotency contract and error codes.
- [ ] Add distributed smoke-test harness (`sprites.dev` / `exe.dev`) with env-gated CI step.
