use axum::http::StatusCode;
use crate::{AppState, auth::AuthenticatedUser};

/// Role levels (ordered by permission level)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read = 0,
    Write = 1,
    Admin = 2,
}

impl Role {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Role::Read),
            "write" => Some(Role::Write),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }
}

/// Check if user has at least the required role for a repo
pub async fn check_role(
    state: &AppState,
    user: &AuthenticatedUser,
    repo_id: &str,
    required: Role,
) -> Result<(), StatusCode> {
    let role_str = state.db.get_user_role(&user.id, repo_id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let role = role_str
        .and_then(|s| Role::from_str(&s))
        .ok_or(StatusCode::FORBIDDEN)?;

    if role >= required {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

/// Helper to require read access
pub async fn require_read(
    state: &AppState,
    user: &AuthenticatedUser,
    repo_id: &str,
) -> Result<(), StatusCode> {
    check_role(state, user, repo_id, Role::Read).await
}

/// Helper to require write access
pub async fn require_write(
    state: &AppState,
    user: &AuthenticatedUser,
    repo_id: &str,
) -> Result<(), StatusCode> {
    check_role(state, user, repo_id, Role::Write).await
}

/// Helper to require admin access
pub async fn require_admin(
    state: &AppState,
    user: &AuthenticatedUser,
    repo_id: &str,
) -> Result<(), StatusCode> {
    check_role(state, user, repo_id, Role::Admin).await
}

/// Check if a bookmark is protected
pub async fn is_bookmark_protected(
    state: &AppState,
    repo_id: &str,
    bookmark_name: &str,
) -> Result<bool, StatusCode> {
    let doc = state.docs.get_or_load(repo_id).await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let doc = doc.read().await;
    let _bookmarks = doc.get_all_bookmarks();

    // For now, consider bookmarks named "main" or "master" as protected
    // TODO: Make this configurable per-repo
    Ok(matches!(bookmark_name, "main" | "master"))
}

/// Check if user can move a bookmark
pub async fn can_move_bookmark(
    state: &AppState,
    user: &AuthenticatedUser,
    repo_id: &str,
    bookmark_name: &str,
) -> Result<(), StatusCode> {
    let protected = is_bookmark_protected(state, repo_id, bookmark_name).await?;

    if protected {
        require_admin(state, user, repo_id).await
    } else {
        require_write(state, user, repo_id).await
    }
}
