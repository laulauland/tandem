//! Property tests: statements that must hold for every input, checked against
//! generated ones.
//!
//! This package owns the end-to-end property: write files, publish, and read
//! them back byte-identically from both client and server. Framing properties
//! live with the protocol and WAL packages that own those formats.

use jj_tandem_test_support as support;

#[path = "properties/roundtrip.rs"]
mod roundtrip;
