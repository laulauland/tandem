//! Shared HTTP metadata that must be encoded identically on both sides.

/// The default duration of a workspace writer claim.
pub const DEFAULT_WRITER_TTL_SECONDS: u64 = 30;

/// The longest writer claim the server accepts.
pub const MAX_WRITER_TTL_SECONDS: u64 = 10 * 60;

/// Encode the mutable head version as a quoted HTTP entity tag.
pub fn etag_for_version(version: u64) -> String {
    format!("\"{version}\"")
}

/// Decode the mutable head version from a quoted HTTP entity tag.
pub fn version_from_etag(etag: &str) -> Option<u64> {
    etag.trim().trim_matches('"').parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{etag_for_version, version_from_etag};

    #[test]
    fn etags_round_trip_a_version() {
        assert_eq!(etag_for_version(0), "\"0\"");
        assert_eq!(version_from_etag(&etag_for_version(42)), Some(42));
        assert_eq!(version_from_etag("not-a-version"), None);
    }
}
