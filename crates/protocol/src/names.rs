use std::{fmt, sync::OnceLock};

const SEGMENT_LENGTH_MIN: usize = 2;
const SEGMENT_LENGTH_MAX: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RepositoryName {
    namespace: String,
    repository: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidRepositoryName(pub String);

impl fmt::Display for InvalidRepositoryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for InvalidRepositoryName {}

/// A hosted repository name failed either the grammar or the hosted content
/// policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidHostedRepositoryName {
    Grammar(InvalidRepositoryName),
    Offensive,
}

impl fmt::Display for InvalidHostedRepositoryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grammar(error) => error.fmt(formatter),
            Self::Offensive => formatter.write_str("hosted repository name is not allowed"),
        }
    }
}

impl std::error::Error for InvalidHostedRepositoryName {}

impl RepositoryName {
    pub fn parse(namespace: &str, repository: &str) -> Result<Self, InvalidRepositoryName> {
        validate_segment("namespace", namespace)?;
        validate_segment("repository", repository)?;
        Ok(Self {
            namespace: namespace.to_string(),
            repository: repository.to_string(),
        })
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn repository(&self) -> &str {
        &self.repository
    }
    pub fn path(&self) -> String {
        format!("{}/{}", self.namespace, self.repository)
    }

    /// Check a grammatically valid name against the hosted content policy.
    pub fn check_hosted(&self) -> Result<(), InvalidHostedRepositoryName> {
        if is_offensive_segment(&self.namespace) || is_offensive_segment(&self.repository) {
            Err(InvalidHostedRepositoryName::Offensive)
        } else {
            Ok(())
        }
    }

    /// Parse a name using the hosted service's content policy.
    pub fn parse_hosted(
        namespace: &str,
        repository: &str,
    ) -> Result<Self, InvalidHostedRepositoryName> {
        parse_hosted_repository_name(namespace, repository)
    }
}

/// Parse and check a hosted `<namespace>/<repository>` name.
pub fn parse_hosted_repository_name(
    namespace: &str,
    repository: &str,
) -> Result<RepositoryName, InvalidHostedRepositoryName> {
    let name = RepositoryName::parse(namespace, repository)
        .map_err(InvalidHostedRepositoryName::Grammar)?;
    name.check_hosted()?;
    Ok(name)
}

/// Check a grammatically valid repository name against the hosted policy.
pub fn check_hosted_repository_name(
    name: &RepositoryName,
) -> Result<(), InvalidHostedRepositoryName> {
    name.check_hosted()
}

fn is_offensive_segment(segment: &str) -> bool {
    let normalized = normalize_policy_text(segment);
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    offensive_terms()
        .iter()
        .any(|term| policy_term_matches(&tokens, term))
}

struct PolicyTerm {
    tokens: Vec<String>,
    compact: String,
}

fn offensive_terms() -> &'static [PolicyTerm] {
    static TERMS: OnceLock<Vec<PolicyTerm>> = OnceLock::new();
    TERMS.get_or_init(|| {
        include_str!("../data/offensive-names/en")
            .lines()
            .filter(|term| {
                term.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'_' | b'-' | b' ' | b'&')
                })
            })
            .filter(|term| !POLICY_EXCLUSIONS.contains(term))
            .map(|term| {
                let normalized = normalize_policy_text(term);
                let tokens: Vec<String> =
                    normalized.split_whitespace().map(str::to_owned).collect();
                let compact = tokens.concat();
                PolicyTerm { tokens, compact }
            })
            .collect()
    })
}

fn policy_term_matches(candidate: &[&str], term: &PolicyTerm) -> bool {
    if candidate.windows(term.tokens.len()).any(|window| {
        window
            .iter()
            .zip(&term.tokens)
            .all(|(left, right)| left == right)
    }) {
        return true;
    }
    for start in 0..candidate.len() {
        let mut length = 0;
        for end in start..candidate.len() {
            length += candidate[end].len();
            if length == term.compact.len()
                && candidate[start..=end]
                    .iter()
                    .flat_map(|token| token.bytes())
                    .eq(term.compact.bytes())
            {
                return true;
            }
            if length >= term.compact.len() {
                break;
            }
        }
    }
    false
}

// Exact exclusions keep ordinary technical, identity, health, and proper-name
// uses available. They do not exempt a larger name containing another blocked
// phrase.
const POLICY_EXCLUSIONS: &[&str] = &[
    "cialis",
    "viagra",
    "dick",
    "domination",
    "escort",
    "intercourse",
    "playboy",
    "nsfw",
    "gay",
    "lesbian",
    "queer",
    "sexuality",
    "sexual",
    "xx",
    "xxx",
];

fn normalize_policy_text(input: &str) -> String {
    let mut normalized = String::with_capacity(input.len());
    let mut separator_pending = false;
    let bytes = input.as_bytes();
    for (index, byte) in bytes.iter().copied().enumerate() {
        if matches!(byte, b'.' | b'_' | b'-' | b' ') {
            separator_pending = !normalized.is_empty();
            continue;
        }
        if separator_pending {
            normalized.push(' ');
            separator_pending = false;
        }
        let inside_word = index > 0
            && index + 1 < bytes.len()
            && bytes[index - 1].is_ascii_lowercase()
            && bytes[index + 1].is_ascii_lowercase();
        normalized.push(if inside_word {
            match byte {
                b'0' => 'o',
                b'1' => 'i',
                b'3' => 'e',
                b'4' => 'a',
                b'5' => 's',
                b'7' => 't',
                _ => char::from(byte),
            }
        } else {
            char::from(byte)
        });
    }
    normalized
}

fn validate_segment(label: &str, value: &str) -> Result<(), InvalidRepositoryName> {
    if !(SEGMENT_LENGTH_MIN..=SEGMENT_LENGTH_MAX).contains(&value.len()) {
        return Err(InvalidRepositoryName(format!(
            "{label} must be {SEGMENT_LENGTH_MIN} to {SEGMENT_LENGTH_MAX} characters"
        )));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) || !value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(InvalidRepositoryName(format!("invalid {label} {value:?}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn valid_segment_strategy() -> impl Strategy<Value = String> {
        (
            "[a-z0-9]",
            proptest::collection::vec("[a-z0-9._-]", 0..63),
            "[a-z0-9]",
        )
            .prop_map(|(first, middle, last)| format!("{first}{}{last}", middle.concat()))
    }

    fn valid_name_strategy() -> impl Strategy<Value = (String, String)> {
        (valid_segment_strategy(), valid_segment_strategy())
    }

    proptest! {
        #[test]
        fn hosted_parser_preserves_valid_grammar(
            (namespace, repository) in valid_name_strategy(),
        ) {
            match parse_hosted_repository_name(&namespace, &repository) {
                Ok(parsed) => {
                    prop_assert_eq!(parsed.path(), format!("{namespace}/{repository}"));
                }
                Err(InvalidHostedRepositoryName::Offensive) => {}
                Err(InvalidHostedRepositoryName::Grammar(error)) => {
                    prop_assert!(false, "valid grammar was rejected: {error}");
                }
            }
        }
    }

    #[test]
    fn hosted_names_reject_offensive_owner_and_repository_components() {
        for (namespace, repository) in [
            ("fuck", "project"),
            ("team", "fuck"),
            ("f-u-c-k", "project"),
            ("team", "f_u_c_k"),
            ("team", "f.u.c.k"),
            ("team", "f4ggot"),
            ("team", "fuck-project"),
            ("shit-tools", "repo"),
            ("team", "my-f.u.c.k-project"),
            ("team", "dirty-sanchez"),
        ] {
            assert_eq!(
                parse_hosted_repository_name(namespace, repository),
                Err(InvalidHostedRepositoryName::Offensive),
                "{namespace}/{repository}"
            );
        }
    }

    #[test]
    fn hosted_names_keep_false_positive_components_available() {
        for (namespace, repository) in [
            ("scunthorpe", "project"),
            ("team", "analysis"),
            ("team", "analytics"),
            ("team", "classic"),
            ("team", "assets"),
            ("team", "cucumber"),
            ("team", "cumulative-data"),
            ("team", "sexton-project"),
            ("team", "cocktail-robin"),
            ("team", "dickens-novel"),
            ("team", "xxl-worker"),
            ("queer", "identity"),
            ("lesbian", "history"),
            ("gay", "community"),
            ("team", "xxx"),
            ("team", "asset5"),
            ("team", "classic3"),
            ("dick", "project"),
            ("team", "nsfw-detector"),
            ("team", "sexual-health"),
        ] {
            assert!(
                parse_hosted_repository_name(namespace, repository).is_ok(),
                "{namespace}/{repository}"
            );
        }
    }

    #[test]
    fn grammar_parser_is_independent_from_hosted_content_policy() {
        assert!(RepositoryName::parse("team", "fuck").is_ok());
        let allowed = RepositoryName::parse("team", "project").expect("valid name");
        assert!(check_hosted_repository_name(&allowed).is_ok());
        assert_eq!(
            check_hosted_repository_name(&RepositoryName::parse("team", "fuck").unwrap()),
            Err(InvalidHostedRepositoryName::Offensive)
        );
        assert_eq!(
            parse_hosted_repository_name("Team", "project"),
            Err(InvalidHostedRepositoryName::Grammar(InvalidRepositoryName(
                "invalid namespace \"Team\"".to_string()
            )))
        );
    }
}
