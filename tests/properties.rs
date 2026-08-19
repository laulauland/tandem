//! Property tests: statements that must hold for every input, checked against
//! generated ones.
//!
//! Three layers, one idea. The framing layer must survive anything a disk or a
//! network can hand it. The serialization layer must round-trip whatever it
//! claims to carry. And the layer above both — write files, publish, read them
//! back — must return the bytes that went in, from a client and from the
//! server alike, whatever the files were and however the commits were cut.

mod support;

#[path = "properties/roundtrip.rs"]
mod roundtrip;
#[path = "properties/wal.rs"]
mod wal;
#[path = "properties/wire.rs"]
mod wire;
