//! Integration: tandem as a person meets it — a binary, a daemon, a socket.
//!
//! What lives here is what a subprocess is the coverage for. Anything that
//! only needs the library is faster, more deterministic and better asserted in
//! `dst.rs` or `properties.rs`, and belongs there instead. Everything in this
//! file runs in one test binary on purpose: twenty-three binaries ran one
//! after another, and one binary runs its tests at the same time.

mod common;

#[path = "integration/bucket_durability.rs"]
mod bucket_durability;
#[path = "integration/bucket_replay.rs"]
mod bucket_replay;
#[path = "integration/client_cache.rs"]
mod client_cache;
#[path = "integration/contention.rs"]
mod contention;
#[path = "integration/control_socket.rs"]
mod control_socket;
#[path = "integration/daemon_lifecycle.rs"]
mod daemon_lifecycle;
#[path = "integration/git_round_trip.rs"]
mod git_round_trip;
#[path = "integration/handshake.rs"]
mod handshake;
#[path = "integration/http_api.rs"]
mod http_api;
#[path = "integration/integration_workspace.rs"]
mod integration_workspace;
#[path = "integration/log_streaming.rs"]
mod log_streaming;
#[path = "integration/up_down.rs"]
mod up_down;
#[path = "integration/watch.rs"]
mod watch;
#[path = "integration/workspace_setup.rs"]
mod workspace_setup;
