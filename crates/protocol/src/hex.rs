//! Hex encoding for the ids that cross tandem's boundaries.
//!
//! jj ids are bytes, but every boundary tandem writes them to — bucket keys,
//! filesystem paths, log lines, etag strings — wants text. One pair of helpers
//! keeps that conversion in one place.

use anyhow::{anyhow, bail, Result};

/// Convert raw bytes to a lowercase hex string.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Convert a hex string back to raw bytes.
pub fn from_hex(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        bail!("odd-length hex string");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| anyhow!("bad hex: {e}")))
        .collect()
}
