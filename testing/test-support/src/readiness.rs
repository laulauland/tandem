//! The readiness handshake, shared by the test suite and the benches.
//!
//! Both suites find a port by binding `:0` and letting go, and both then have
//! to wait for a server to take it. That wait is the same question in both
//! places, and the answer encodes a contract — the endpoint, the bearer, the
//! status — so it is written once here and included from `benches/bench_support.rs`
//! with `#[path]`. The two `wait_for_server` wrappers stay where they are: they
//! differ on purpose, one panicking and one returning a `Result`.
//!
//! Nothing outside `std` is used, so the file compiles in either target.

#![allow(dead_code)]

use std::time::Duration;

/// How long a readiness probe waits for the other end to say something.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether a socket already connected to `addr` is talking to the server this
/// caller is waiting for.
///
/// Readiness has to be a handshake, and `token` is what makes it one about the
/// right server. A bare `TcpStream::connect` answers a question nobody asked —
/// "is this port refusing connections" — and there are two ways for it to say
/// yes while the server the caller wants is not there:
///
/// - *Somebody else's listener.* `free_addr` finds a port by binding `:0` and
///   letting go, so between the finding and the server's own `bind` the port
///   belongs to nobody. A second `cargo test`, a bench, or any other process on
///   the machine can take it in that window. This fixture's server then dies at
///   `bind`, and a connect-only probe cheerfully reports the *other* process's
///   server as ready — after which the test runs its first command against a
///   repository directory that is still empty, and fails with "there is no jj
///   repo here", nowhere near the actual fault.
/// - *A socket connected to itself.* Loopback `connect` to an unbound port can
///   succeed by TCP's simultaneous open, when the kernel picks the destination
///   port as the source port. Current kernels avoid choosing that port, so this
///   is the smaller hazard of the two — but it costs nothing to exclude, and a
///   readiness check should not depend on which kernel is underneath it.
///
/// So the probe asks `/api/info` and, when a token is given, requires a `200`.
/// A socket connected to itself can only read back what was written into it and
/// never says `HTTP/`; another fixture's server has a different admin token and
/// says `401`. Only this server says `200` — and to say it, its process must be
/// past `Server::new`, which is where the repository is created, before the
/// bind.
///
/// `None` is for the handful of callers whose server generates its own token
/// and never prints it where the caller can read it. Those get the weaker
/// check: something at that address speaks HTTP.
pub fn answers_the_tandem_protocol(
    mut stream: std::net::TcpStream,
    addr: &str,
    token: Option<&str>,
) -> bool {
    use std::io::{Read, Write};

    if stream.set_write_timeout(Some(PROBE_TIMEOUT)).is_err()
        || stream.set_read_timeout(Some(PROBE_TIMEOUT)).is_err()
    {
        return false;
    }
    let authorization = match token {
        Some(token) => format!("Authorization: Bearer {token}\r\n"),
        None => String::new(),
    };
    let request = format!("GET /api/info HTTP/1.0\r\nHost: {addr}\r\n{authorization}\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    // `HTTP/1.1 200` — the version and the code, and nothing after them.
    let mut head = [0u8; 12];
    let mut filled = 0;
    while filled < head.len() {
        match stream.read(&mut head[filled..]) {
            Ok(0) | Err(_) => return false,
            Ok(read) => filled += read,
        }
    }
    if &head[..5] != b"HTTP/" {
        return false;
    }
    match token {
        Some(_) => &head[9..12] == b"200",
        None => true,
    }
}

/// Whether the server this caller is waiting for is answering at `addr`.
pub fn server_is_answering(addr: &str, token: Option<&str>) -> bool {
    match std::net::TcpStream::connect(addr) {
        Ok(stream) => answers_the_tandem_protocol(stream, addr, token),
        Err(_) => false,
    }
}
