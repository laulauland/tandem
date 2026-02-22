# Execution Plan: Server Lifecycle — COMPLETED

**Design doc:** `docs/design-docs/server-lifecycle.md`

> Historical note (CLI rename): this completion note uses the original
> command names (`tandem status`, `tandem logs`). In the current CLI,
> daemon health/log streaming are `tandem server status` and
> `tandem server logs`. `tandem status` is jj working-copy status.

Implements `tandem up/down/status/logs` — daemon management without systemd.
(Current CLI: `tandem up/down` + `tandem server status/logs`.)

**Completed:** 2026-02-19. All 4 slices implemented, 19 tests passing.

## Slice 10 — Signal handling and graceful shutdown ✅

**Test:** `tests/slice10_graceful_shutdown.rs` (6 tests)

Implemented:
- Signal handler (tokio::signal) for SIGTERM/SIGINT in `tandem serve`.
- Clean shutdown: stop accepting connections, drain in-flight RPCs, exit 0.
- Double signal: immediate exit.
- `--log-level` and `--log-format` flags on `tandem serve`.

## Slice 11 — Control socket and tandem status (`tandem server status` now) ✅

**Test:** `tests/slice11_control_socket.rs` (5 tests)

Implemented:
- Control socket (Unix domain socket, JSON lines) in `tandem serve`.
- `tandem status` command with human-readable and `--json` output
  (renamed to `tandem server status`).
- Socket path defaults to `$TMPDIR/tandem/control.sock`, override with `--control-socket`.
- Exit code 0 = running, 1 = not running.
- Socket cleaned up on server exit.

Implementation note: used newline-delimited JSON over Unix stream socket
instead of HTTP-over-Unix-socket (simpler, no axum/hyper dependency needed).

## Slice 12 — tandem up and tandem down ✅

**Test:** `tests/slice12_up_down.rs` (4 tests)

Implemented:
- `tandem up` forks `tandem serve --daemon`, waits for control socket health, prints PID.
- `tandem down` sends shutdown via control socket, waits for process exit.
- `tandem up` when already running: exits 1 with "already running" message.
- `--daemon` is a hidden internal flag on `tandem serve`.
- Daemon stdout/stderr redirected to log file.

## Slice 13 — tandem logs (`tandem server logs` now, streaming) ✅

**Test:** `tests/slice13_log_streaming.rs` (4 tests)

Implemented:
- Broadcast channel in server fans out log events to control socket clients.
- `tandem logs` connects to control socket, streams JSON log events
  (renamed to `tandem server logs`).
- `--level` flag filters server-side (trace/debug/info/warn/error).
- `--json` flag outputs raw JSON lines; default is formatted text.
- Exits cleanly when daemon shuts down.
- "no tandem daemon running" message when no daemon found.
