//! What the fixture is allowed to call a started server.
//!
//! The whole suite starts servers the same way — spawn `tandem serve`, wait,
//! then use it — so the definition of "waited long enough" is load-bearing for
//! every test in this binary. It used to be `TcpStream::connect(addr).is_ok()`,
//! and that is not a definition of anything. It answers "is this port refusing
//! connections", and there are two ways for it to say yes with the server the
//! test wants nowhere in sight.
//!
//! The one that has actually been observed is another process's server. A port
//! comes from `free_addr`, which binds `:0` and lets go, so between the finding
//! and this server's own `bind` the port belongs to nobody and anything on the
//! machine — a second `cargo test`, a bench — can take it. This fixture's
//! server then dies at `bind`, and a connect-only probe reports the other
//! process's server as ready. The test then runs its first command against a
//! repository directory that is still empty and fails with "there is no jj repo
//! here", nowhere near the actual fault, because `Server::new` creates the
//! repository before it binds.
//!
//! The other is a socket connected to itself: loopback `connect` to an unbound
//! port can succeed by TCP's simultaneous open when the kernel picks the
//! destination port as the source port. Measured on this machine it does not
//! happen by itself — 300,000 connects to an unbound loopback port produced
//! zero, because the kernel declines to choose that source port — but it costs
//! nothing to exclude, and a readiness check should not rest on which kernel is
//! underneath it.
//!
//! One test per way, and each holds the readiness predicate to it.

use std::net::TcpStream;

use crate::common;

/// A socket bound to `port` on loopback and connected to itself.
///
/// This is the same pair of syscalls `connect` makes when it picks a source
/// port equal to the destination: bind, then connect to the address just
/// bound. `std::net::TcpStream` has no way to spell it, because it has no way
/// to choose a source port, which is precisely why the collision is the
/// kernel's to make and not the caller's to avoid.
#[cfg(unix)]
fn self_connected(port: u16) -> Option<TcpStream> {
    use std::os::fd::FromRawFd;

    // SAFETY: every pointer handed to libc points at a live, correctly sized
    // `sockaddr_in`, and the descriptor is either closed here or given to a
    // `TcpStream` that owns it from then on.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let addr = libc::sockaddr_in {
            #[cfg(target_os = "macos")]
            sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            // Already in network order: the bytes of 127.0.0.1, in the order
            // they go on the wire.
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes([127, 0, 0, 1]),
            },
            sin_zero: [0; 8],
        };
        let sockaddr = std::ptr::addr_of!(addr) as *const libc::sockaddr;
        let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        if libc::bind(fd, sockaddr, len) != 0 || libc::connect(fd, sockaddr, len) != 0 {
            libc::close(fd);
            return None;
        }
        Some(TcpStream::from_raw_fd(fd))
    }
}

#[cfg(unix)]
#[test]
fn a_connect_that_succeeded_is_not_a_server_that_started() {
    let addr = common::free_addr();
    let port: u16 = addr
        .rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .expect("a port in the address free_addr handed out");

    let socket = self_connected(port).expect(
        "the kernel refused a loopback self-connect, which is the hazard this test exists for; \
         if that is now genuinely impossible the readiness handshake is merely redundant, but \
         find out before deleting it",
    );

    // Nothing is listening on this port, and the socket is connected anyway —
    // to itself. This is the state the old readiness check reported as "the
    // server is up".
    assert_eq!(
        socket
            .peer_addr()
            .expect("the socket has a peer")
            .to_string(),
        addr,
        "the socket's peer is the address nobody is serving"
    );
    assert_eq!(
        socket
            .local_addr()
            .expect("the socket has a local address")
            .to_string(),
        addr,
        "and the peer is the socket itself"
    );

    assert!(
        !common::answers_the_tandem_protocol(socket, &addr, None),
        "a socket connected to itself was accepted as a started tandem server"
    );
}

#[test]
fn somebody_elses_server_on_the_port_is_not_this_server() {
    // The observed failure, reduced: a real tandem server, answering HTTP at
    // the address this caller is waiting on, that this caller's server is not.
    // A connect succeeds against it. So does a tokenless handshake. Only the
    // token tells the two apart — every fixture generates its own, and a server
    // that will not answer 200 to it is somebody else's.
    let somebody_else = common::ServerFixture::start();

    assert!(
        common::server_is_answering(&somebody_else.addr, Some(somebody_else.token())),
        "the server does not answer its own token, so this test proves nothing"
    );

    let mine = jj_tandem_server::generate_admin_token();
    assert!(
        !common::server_is_answering(&somebody_else.addr, Some(&mine)),
        "a server that refuses this caller's token was accepted as this caller's server"
    );
}

#[test]
fn a_started_fixture_has_already_created_its_repository() {
    // The other half of the claim, and the one the suite depends on: when the
    // fixture hands a server back, the server is past `Server::new` — so the
    // repository directory the tests write into exists.
    let fx = common::ServerFixture::start();

    assert!(
        common::server_is_answering(&fx.addr, Some(fx.token())),
        "the fixture returned a server that does not answer at {}",
        fx.addr
    );
    assert!(
        fx.repo.join(".jj").is_dir(),
        "the fixture returned before the server created {}",
        fx.repo.display()
    );
}
