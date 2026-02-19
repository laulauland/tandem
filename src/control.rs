//! Control socket — newline-delimited JSON over Unix stream socket.
//!
//! Protocol:
//!   Client sends one JSON line: {"type": "status"} / {"type": "shutdown"} / {"type": "logs", "level": "debug"}
//!   Server responds with one or more JSON lines.
//!   For status/shutdown: single response line, then close.
//!   For logs: streaming response lines until client disconnects or server shuts down.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Instant;
use tokio::sync::broadcast;

// ─── Protocol types ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub running: bool,
    pub pid: u32,
    pub uptime_secs: u64,
    pub repo: String,
    pub listen: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    pub ts: String,
    pub level: String,
    pub msg: String,
}

/// Shared server state for the control socket.
pub struct ControlState {
    pub pid: u32,
    pub start_time: Instant,
    pub repo: String,
    pub listen: String,
    pub shutdown_tx: tokio::sync::mpsc::Sender<()>,
    pub log_tx: broadcast::Sender<LogEvent>,
}

impl ControlState {
    #[allow(dead_code)]
    pub fn emit_log(&self, level: &str, msg: &str) {
        let event = LogEvent {
            ts: chrono_now(),
            level: level.to_string(),
            msg: msg.to_string(),
        };
        // Ignore send errors (no receivers)
        let _ = self.log_tx.send(event);
    }
}

#[allow(dead_code)]
fn chrono_now() -> String {
    // Simple ISO-ish timestamp without pulling in chrono
    use std::time::SystemTime;
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}Z", d.as_secs())
}

fn level_rank(level: &str) -> u8 {
    match level.to_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "info" => 2,
        "warn" | "warning" => 3,
        "error" => 4,
        _ => 2,
    }
}

// ─── Control socket server ───────────────────────────────────────────────────

#[cfg(unix)]
pub async fn run_control_socket(
    socket_path: String,
    state: std::sync::Arc<ControlState>,
) -> anyhow::Result<()> {
    // Remove stale socket if present
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = tokio::net::UnixListener::bind(&socket_path)?;

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_control_connection(stream, state).await {
                eprintln!("control connection error: {e:#}");
            }
        });
    }
}

#[cfg(unix)]
async fn handle_control_connection(
    stream: tokio::net::UnixStream,
    state: std::sync::Arc<ControlState>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let line = line.trim();

    if line.is_empty() {
        return Ok(());
    }

    let request: serde_json::Value = serde_json::from_str(line)
        .map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?;

    let req_type = request["type"].as_str().unwrap_or("");

    match req_type {
        "status" => {
            let uptime = state.start_time.elapsed().as_secs();
            let resp = StatusResponse {
                running: true,
                pid: state.pid,
                uptime_secs: uptime,
                repo: state.repo.clone(),
                listen: state.listen.clone(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            };
            let json = serde_json::to_string(&resp)?;
            writer.write_all(json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
        "shutdown" => {
            let resp = serde_json::json!({"type": "shutdown", "ok": true});
            writer.write_all(resp.to_string().as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            // Signal shutdown
            let _ = state.shutdown_tx.send(()).await;
        }
        "logs" => {
            let level_filter = request["level"]
                .as_str()
                .unwrap_or("info")
                .to_string();
            let min_rank = level_rank(&level_filter);

            let mut rx = state.log_tx.subscribe();

            // Stream log events until client disconnects or channel closes
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if level_rank(&event.level) >= min_rank {
                            let json = match serde_json::to_string(&event) {
                                Ok(j) => j,
                                Err(_) => continue,
                            };
                            if writer.write_all(json.as_bytes()).await.is_err() {
                                break;
                            }
                            if writer.write_all(b"\n").await.is_err() {
                                break;
                            }
                            if writer.flush().await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        }
        _ => {
            let resp = serde_json::json!({"type": "error", "msg": format!("unknown request type: {req_type}")});
            writer.write_all(resp.to_string().as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
    }

    Ok(())
}

// ─── Control socket client ───────────────────────────────────────────────────

#[cfg(unix)]
pub fn client_status(socket_path: &str) -> anyhow::Result<StatusResponse> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| anyhow::anyhow!("cannot connect to control socket: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let request = serde_json::json!({"type": "status"});
    writeln!(stream, "{}", request)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let status: StatusResponse = serde_json::from_str(line.trim())?;
    Ok(status)
}

#[cfg(unix)]
pub fn client_shutdown(socket_path: &str) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| anyhow::anyhow!("cannot connect to control socket: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let request = serde_json::json!({"type": "shutdown"});
    writeln!(stream, "{}", request)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(())
}

#[cfg(unix)]
pub fn client_logs(socket_path: &str, level: &str, json_output: bool) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| anyhow::anyhow!("cannot connect to control socket: {e}"))?;
    // No read timeout for streaming
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;

    let request = serde_json::json!({"type": "logs", "level": level});
    writeln!(stream, "{}", request)?;
    stream.flush()?;

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => {
                if json_output {
                    println!("{l}");
                } else {
                    // Parse and format as human-readable
                    if let Ok(event) = serde_json::from_str::<LogEvent>(&l) {
                        println!("[{}] {} {}", event.level, event.ts, event.msg);
                    } else {
                        println!("{l}");
                    }
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

// Non-unix stubs
#[cfg(not(unix))]
pub async fn run_control_socket(
    _socket_path: String,
    _state: std::sync::Arc<ControlState>,
) -> anyhow::Result<()> {
    anyhow::bail!("control socket not supported on this platform")
}

#[cfg(not(unix))]
pub fn client_status(_socket_path: &str) -> anyhow::Result<StatusResponse> {
    anyhow::bail!("control socket not supported on this platform")
}

#[cfg(not(unix))]
pub fn client_shutdown(_socket_path: &str) -> anyhow::Result<()> {
    anyhow::bail!("control socket not supported on this platform")
}

#[cfg(not(unix))]
pub fn client_logs(_socket_path: &str, _level: &str, _json: bool) -> anyhow::Result<()> {
    anyhow::bail!("control socket not supported on this platform")
}
