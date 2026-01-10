use axum::{extract::{Path, State}, http::StatusCode, body::Bytes};
use crate::{AppState, auth::AuthenticatedUser, authz};

pub async fn get_content(
    State(state): State<AppState>,
    Path((repo_id, hash)): Path<(String, String)>,
    user: AuthenticatedUser,
) -> Result<Bytes, StatusCode> {
    authz::require_read(&state, &user, &repo_id).await?;

    let doc = state.docs.get_or_load(&repo_id).await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let doc = doc.read().await;

    let content = doc.get_content(&hash)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Bytes::from(content))
}
