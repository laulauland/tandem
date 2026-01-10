use axum::{extract::{Path, State}, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use crate::{AppState, auth::AuthenticatedUser, authz};

#[derive(Serialize)]
pub struct BookmarkResponse {
    pub name: String,
    pub target: String,
}

#[derive(Deserialize)]
pub struct MoveBookmarkRequest {
    pub name: String,
    pub target: String,
}

pub async fn list_bookmarks(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<BookmarkResponse>>, StatusCode> {
    authz::require_read(&state, &user, &repo_id).await?;

    let doc = state.docs.get_or_load(&repo_id).await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let doc = doc.read().await;
    let bookmarks = doc.get_all_bookmarks();

    Ok(Json(bookmarks.into_iter().map(|(name, target)| BookmarkResponse {
        name,
        target: target.to_string(),
    }).collect()))
}

pub async fn move_bookmark(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    user: AuthenticatedUser,
    Json(req): Json<MoveBookmarkRequest>,
) -> Result<StatusCode, StatusCode> {
    authz::can_move_bookmark(&state, &user, &repo_id, &req.name).await?;

    let doc = state.docs.get_or_load(&repo_id).await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // Parse target change_id
    let target: tandem_core::types::ChangeId = req.target.parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let doc = doc.read().await;
    doc.set_bookmark(&req.name, &target);

    Ok(StatusCode::OK)
}
