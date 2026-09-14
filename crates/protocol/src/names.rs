use std::fmt;

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
