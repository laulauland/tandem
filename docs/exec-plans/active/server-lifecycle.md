# Execution Plan: Server Lifecycle

**Design doc:** `docs/design-docs/server-lifecycle.md`

Implements `tandem up/down/status/logs` — daemon management without systemd.

## Slice 10 — Signal handling and graceful shutdown

**Goal:** `tandem serve` handles SIGTERM/SIGINT cleanly. Prerequisite for
everything else — daemon mode needs reliable shutdown.

**Work:**

- Install signal handler (tokio::signal) in `tandem serve`.
- On SIGTERM/SIGINT: stop accepting new connections, drain in-flight RPCs
  (5s timeout), close listeners, exit 0.
- Second signal: immediate exit.
- Add `--log-level` and `--log-format` flags to `tandem serve`.

**Acceptance:**

- `tandem serve` + SIGINT exits 0 (not 130).
- In-flight `getObject` call during shutdown completes (not dropped).
- `--log-level debug` produces debug output to stderr.
- Existing slice 1-7 tests still pass.

**Test:** `tests/slice10_graceful_shutdown.rs`
- Start server, connect client, send SIGTERM, verify clean exit.
- Start server, begin slow read, send SIGTERM, verify read completes.

## Slice 11 — Control socket and tandem status

**Goal:** `tandem serve` opens a control socket. `tandem status` queries it.

**Work:**

- Add HTTP-over-Unix-socket listener to `tandem serve` (axum + hyper-unix).
- Implement `GET /status` on control socket.
- Socket path: `$XDG_RUNTIME_DIR/tandem/control.sock` (Linux),
  `$TMPDIR/tandem/control.sock` (macOS). Override with `--control-socket`.
- Implement `tandem status` command: connect to control socket, print output.
- Implement `tandem status --json`.
- Exit code 0 = running, 1 = not running.

**Acceptance:**

- `tandem serve` creates control socket.
- `tandem status` prints human-readable output while server runs.
- `tandem status --json` returns valid JSON with pid, uptime, repo, listen fields.
- `tandem status` exits 1 when no server is running.
- Control socket is cleaned up on server exit (from slice 10).

**Test:** `tests/slice11_control_socket.rs`
- Start server, run `tandem status --json`, parse output, verify fields.
- No server running, run `tandem status`, verify exit code 1.

## Slice 12 — tandem up and tandem down

**Goal:** `tandem up` starts a background daemon. `tandem down` stops it.

**Work:**

- Implement `--daemon` internal flag on `tandem serve` (detach, redirect
  stdio, write PID file).
- Implement `tandem up`: validate flags, fork `tandem serve --daemon`, wait
  for control socket readiness, print PID, exit 0.
- Implement `tandem down`: connect to control socket, `POST /shutdown`,
  wait for process exit.
- `tandem up` when already running: exit 1 with message.
- PID file at `$XDG_RUNTIME_DIR/tandem/daemon.pid`.

**Acceptance:**

- `tandem up --repo ... --listen ...` returns immediately, daemon is running.
- `tandem status` shows running after `tandem up`.
- `tandem down` stops daemon, `tandem status` shows not running.
- `tandem up` twice: second invocation errors with "already running".
- PID file and control socket cleaned up after `tandem down`.

**Test:** `tests/slice12_up_down.rs`
- `tandem up`, verify status, connect client, read object, `tandem down`, verify stopped.
- `tandem up` twice, verify error.

## Slice 13 — tandem logs (streaming)

**Goal:** `tandem logs` streams log output from a running daemon.

**Work:**

- Add tracing subscriber that fans out to: file/stderr + SSE clients.
- Implement `GET /logs?level=<level>` on control socket (SSE stream).
- Implement `tandem logs` command: connect to SSE endpoint, print lines.
- `--level` flag on `tandem logs` (default: info).
- `--json` flag for raw JSON log lines.
- Client can request higher verbosity than daemon's file log level.

**Acceptance:**

- `tandem logs` prints log lines as events happen.
- `tandem logs --level debug` shows debug events even if daemon was started
  with `--log-level info`.
- `tandem logs --json` outputs one JSON object per line.
- `tandem logs` exits cleanly when daemon shuts down.
- `tandem logs` with no daemon: exit 1 with helpful message.

**Test:** `tests/slice13_log_streaming.rs`
- Start daemon, connect client, trigger activity, verify `tandem logs` output
  contains expected event.
- Verify `--level debug` produces more output than `--level warn`.
