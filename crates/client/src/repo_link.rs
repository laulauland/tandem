//! What a workspace on disk knows about the server it belongs to.
//!
//! Three jj store traits — the backend, the op store and the op-heads store —
//! each get their own directory under `.jj/repo/`, each are constructed by jj
//! without reference to the other two, and each need the same three facts: the
//! address to talk to, the bearer to talk with, and the workspace name to
//! speak for. Every one of them wrote its own copy of the address-reading code
//! before this file existed; adding the token would have made that three
//! copies of two things.
//!
//! Each fact is a file next to the store, overridable by an environment
//! variable, because both halves have a use: the file is what `tandem init`
//! leaves behind so an ordinary `jj` command in the workspace just works, and
//! the variable is what a harness or a container sets when the workspace was
//! created somewhere else.

use std::path::Path;

use jj_lib::backend::{BackendInitError, BackendLoadError};

pub const SERVER_ADDRESS_FILE: &str = "server_address";
pub const TOKEN_FILE: &str = "token";
pub const WORKSPACE_ID_FILE: &str = "workspace_id";

pub const SERVER_ADDRESS_ENV: &str = "TANDEM_SERVER";
pub const TOKEN_ENV: &str = "TANDEM_TOKEN";
pub const WORKSPACE_ID_ENV: &str = "TANDEM_WORKSPACE";

/// Write everything a later `load` of this store will need.
pub fn write_link(
    store_path: &Path,
    server_addr: &str,
    token: &str,
) -> Result<(), BackendInitError> {
    std::fs::write(store_path.join(SERVER_ADDRESS_FILE), server_addr)
        .map_err(|e| BackendInitError(e.into()))?;
    write_token(store_path, token)
}

/// The token on its own, for the store that writes its address itself.
pub fn write_token(store_path: &Path, token: &str) -> Result<(), BackendInitError> {
    std::fs::write(store_path.join(TOKEN_FILE), token).map_err(|e| BackendInitError(e.into()))
}

pub fn read_server_address(store_path: &Path) -> Result<String, BackendLoadError> {
    if let Some(addr) = env_value(SERVER_ADDRESS_ENV) {
        return Ok(addr);
    }
    let path = store_path.join(SERVER_ADDRESS_FILE);
    std::fs::read_to_string(&path).map_err(|e| {
        BackendLoadError(
            anyhow::anyhow!(
                "cannot read tandem server address from {} or {SERVER_ADDRESS_ENV} env: {e}",
                path.display()
            )
            .into(),
        )
    })
}

/// The bearer this workspace presents on every request.
///
/// Absent is a failure rather than an empty string: a server refuses an
/// unauthenticated request, so a workspace with no token would fail later,
/// somewhere less obvious, with an HTTP status instead of a sentence about
/// where the token was supposed to be.
pub fn read_token(store_path: &Path) -> Result<String, BackendLoadError> {
    if let Some(token) = env_value(TOKEN_ENV) {
        return Ok(token);
    }
    let path = store_path.join(TOKEN_FILE);
    let token = std::fs::read_to_string(&path).map_err(|e| {
        BackendLoadError(
            anyhow::anyhow!(
                "cannot read the tandem token from {} or {TOKEN_ENV} env: {e}",
                path.display()
            )
            .into(),
        )
    })?;
    Ok(token.trim().to_string())
}

/// The workspace this checkout speaks for. Unnamed means `default`.
pub fn read_workspace_id(store_path: &Path) -> Result<String, BackendLoadError> {
    if let Some(id) = env_value(WORKSPACE_ID_ENV) {
        return Ok(id);
    }
    let path = store_path.join(WORKSPACE_ID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(id) => {
            let trimmed = id.trim();
            if trimmed.is_empty() {
                Ok("default".to_string())
            } else {
                Ok(trimmed.to_string())
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("default".to_string()),
        Err(e) => Err(BackendLoadError(
            anyhow::anyhow!(
                "cannot read tandem workspace identity from {}: {e}",
                path.display()
            )
            .into(),
        )),
    }
}

fn env_value(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_init_writes_is_what_load_reads() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_link(dir.path(), "127.0.0.1:13013", "tdmw_abc").expect("write the link");

        assert_eq!(
            read_server_address(dir.path()).expect("address"),
            "127.0.0.1:13013"
        );
        assert_eq!(read_token(dir.path()).expect("token"), "tdmw_abc");
    }

    #[test]
    fn a_workspace_without_a_name_is_the_default_one() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(read_workspace_id(dir.path()).expect("id"), "default");

        std::fs::write(dir.path().join(WORKSPACE_ID_FILE), "  \n").expect("write");
        assert_eq!(read_workspace_id(dir.path()).expect("id"), "default");

        std::fs::write(dir.path().join(WORKSPACE_ID_FILE), "agent-a\n").expect("write");
        assert_eq!(read_workspace_id(dir.path()).expect("id"), "agent-a");
    }

    #[test]
    fn a_missing_token_says_where_it_was_looked_for() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = read_token(dir.path()).expect_err("no token file");
        let message = format!("{error}");
        assert!(message.contains(TOKEN_ENV), "{message}");
        assert!(message.contains(TOKEN_FILE), "{message}");
    }
}
