//! The boolean switches tandem reads out of the environment.
//!
//! Several unrelated parts of the binary are turned on or off by an
//! environment variable, and each one has to decide what counts as "on". They
//! agree today — `1`, `true`, `yes` or `on`, in any case, with any surrounding
//! space — and one parser is what keeps them agreeing: a spelling added here
//! is added everywhere at once, rather than in whichever switch its author
//! happened to be looking at.
//!
//! `object_store.rs` keeps a parser of its own on purpose. That one reads a
//! value out of a store configuration and fails on a spelling it does not
//! know, which is the right answer for something a person typed and the wrong
//! one for a switch that is simply off until it is set.

/// Whether an environment value means "on". An unset variable is off.
pub fn flag_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// The same question, asked of the process environment.
pub fn env_flag_enabled(name: &str) -> bool {
    flag_enabled(std::env::var(name).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_accepted_spelling_reads_as_on() {
        for value in ["1", "true", "yes", "on", "TRUE", " On ", "Yes"] {
            assert!(flag_enabled(Some(value)), "{value:?} must read as on");
        }
    }

    #[test]
    fn anything_else_reads_as_off() {
        for value in ["", "0", "false", "no", "off", "maybe", "2"] {
            assert!(!flag_enabled(Some(value)), "{value:?} must read as off");
        }
        assert!(!flag_enabled(None), "an unset variable must read as off");
    }
}
