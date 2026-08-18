//! tandem — jj workspaces over the network.
//!
//! The binary in `src/main.rs` is a thin CLI over this library. The library
//! exists so that a test can hold a `Server` and a client store in one address
//! space: the deterministic-simulation suite drives both directly, without a
//! subprocess in between, which is what makes a generated schedule replayable
//! from its seed.

pub mod auth;
pub mod backend;
pub mod cache;
pub mod control;
pub mod env;
pub mod hex;
pub mod http_client;
pub mod logging;
pub mod object_store;
pub mod op_heads_store;
pub mod op_store;
pub mod proto_convert;
pub mod repo_link;
pub mod server;
pub mod wal;
pub mod watch;
pub mod wire;
pub mod workspace_init;
