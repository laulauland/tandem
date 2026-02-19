# Server Lifecycle (up/down/status/logs)

## Motivation

Users shouldn't need to understand systemd, launchd, or process management to
run a tandem server. `tandem up` starts it, `tandem down` stops it,
`tandem status` tells you if it's running. Same model as Tailscale (`tailscale up`)
and Caddy (`caddy start`).

## API surface

```
tandem up --repo /srv/project --listen 0.0.0.0:13013   # start daemon, return
tandem down                                              # stop daemon
tandem status                                            # health check
tandem status --json                                     # machine-readable
tandem logs                                              # stream logs from daemon
tandem logs --level debug                                # stream at higher verbosity
```

`tandem serve` remains the foreground mode for systemd/docker/debugging:

```
tandem serve --repo /srv/project --listen 0.0.0.0:13013
tandem serve --log-level debug --log-file /var/log/tandem.log --log-format json
tandem serve --pidfile /var/run/tandem.pid
```

## Fork model

`tandem up` forks itself as a background process. No separate daemon binary.

1. `tandem up` validates flags (repo exists, port parseable).
2. Forks `tandem serve --daemon` with same flags. `--daemon` is internal/hidden.
3. Parent waits for child to signal readiness (control socket exists + health OK).
4. Parent prints "tandem running, PID <n>" and exits 0.
5. If child fails to start within timeout (5s default), parent exits 1 with error.

The `--daemon` flag tells `serve` to:
- Detach from terminal (setsid, close stdin/stdout/stderr).
- Write PID file to `$XDG_RUNTIME_DIR/tandem/daemon.pid`.
- Create control socket.
- Redirect logs to `$XDG_RUNTIME_DIR/tandem/daemon.log` (unless --log-file overrides).

Same pattern as Caddy's `caddy start` → `caddy run --environ`.

### Already running

`tandem up` when a daemon is already running: exit 1 with
"tandem is already running (PID <n>). Use `tandem down` first."

Detected via control socket liveness check, not just PID file existence.

## Control socket

Path: `$XDG_RUNTIME_DIR/tandem/control.sock` (Linux) or
`$TMPDIR/tandem/control.sock` (macOS). Override with `--control-socket <path>`.

Protocol: HTTP/1.1 over Unix domain socket. Reasons:

- Reuse hyper/axum (same stack as the HTTP API feature).
- Structured request/response with status codes.
- Easy to curl for debugging: `curl --unix-socket /path/to/control.sock http://localhost/status`
- No need to invent a framing protocol.

### Control endpoints

```
GET  /status              → { "pid": 1234, "uptime_secs": 3600, "repo": "/srv/project", ... }
POST /shutdown            → 200 OK, daemon begins graceful shutdown
GET  /logs?level=debug    → SSE stream of log events (text/event-stream)
```

The control socket is **local-only** (Unix socket permissions). No auth needed.

## Log streaming

`tandem logs` connects to the control socket's `/logs` SSE endpoint.

Key design: the daemon always logs at trace level internally (ring buffer or
tracing subscriber). `tandem logs --level info` filters server-side before
streaming. This means you can attach at debug level to a daemon that was
started with `--log-level info` — the Consul `consul monitor` pattern.

Implementation: tracing subscriber that fans out to:
1. File/stderr (at configured --log-level).
2. Zero or more SSE clients (each with independent level filter).

Log format over SSE:

```
data: {"ts":"2026-02-19T18:00:00Z","level":"info","target":"tandem::server","msg":"client connected","fields":{"addr":"10.0.0.5:44312"}}
```

`tandem logs` renders these as human-readable lines by default.
`tandem logs --json` passes the raw JSON through.

### No daemon running

`tandem logs` when no daemon is running: exit 1 with
"no tandem daemon running. Start one with `tandem up`."

## Status output

`tandem status` (human-readable):

```
tandem is running
  PID:      1234
  Uptime:   2h 15m
  Repo:     /srv/project
  Listen:   0.0.0.0:13013
  Version:  0.3.0
```

`tandem status --json`:

```json
{
  "running": true,
  "pid": 1234,
  "uptime_secs": 8100,
  "repo": "/srv/project",
  "listen": "0.0.0.0:13013",
  "version": "0.3.0",
  "workspaces": 3
}
```

Exit codes: 0 = running, 1 = not running / unreachable.

When not running:

```
tandem is not running
```

## Signal handling

- **SIGTERM**: graceful shutdown. Drain in-flight RPCs (5s timeout), close
  sockets, remove PID file and control socket, exit 0.
- **SIGINT** (Ctrl+C): same as SIGTERM. Already needed for foreground `tandem serve`.
- **SIGHUP**: reserved for future config reload. Currently ignored.
- **Second SIGTERM/SIGINT**: immediate exit.

## Relationship to tandem serve

| | `tandem serve` | `tandem up` |
|---|---|---|
| Foreground | yes | no |
| Logs to stderr | yes (default) | no (logs to file) |
| Control socket | yes | yes |
| PID file | opt-in (--pidfile) | auto-managed |
| systemd/docker | yes | not needed |
| Human operator | debugging | normal use |

Both modes create the control socket. `tandem down` / `tandem status` /
`tandem logs` work against either mode.

## Flags summary

### tandem serve (existing + new)

```
--listen <addr>           Cap'n Proto listen address (required)
--repo <path>             Repository path (required)
--log-level <level>       trace|debug|info|warn|error (default: info)
--log-file <path>         Log to file instead of stderr
--log-format <fmt>        text|json (default: text)
--pidfile <path>          Write PID file (opt-in)
--control-socket <path>   Override control socket path
--daemon                  Internal flag, set by `tandem up`
```

### tandem up

```
--listen <addr>           Cap'n Proto listen address (required)
--repo <path>             Repository path (required)
--log-level <level>       Daemon log level (default: info)
--log-file <path>         Daemon log file (default: $XDG_RUNTIME_DIR/tandem/daemon.log)
```

### tandem down

No flags. Finds daemon via control socket.

### tandem status

```
--json                    Machine-readable output
```

### tandem logs

```
--level <level>           Filter level (default: info)
--json                    Raw JSON output
```

## Open questions

1. **Multiple daemons.** Current design assumes one daemon per user (single
   control socket path). Should we support named instances for serving multiple
   repos? Could use `--name <n>` with per-name socket paths. Punt until needed.

2. **Log retention.** How large should the daemon log file grow? Rotation
   policy? Probably punt to logrotate / the OS for now.

3. **macOS launchd.** Should `tandem up` optionally install a launchd plist
   for auto-restart? Probably not — keep it simple, add later if needed.
