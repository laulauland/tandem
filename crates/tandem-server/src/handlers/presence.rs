use axum::{extract::{Path, State}, http::StatusCode, Json};
use serde::Serialize;
use crate::{AppState, auth::AuthenticatedUser, authz};

#[derive(Serialize)]
pub struct PresenceResponse {
    pub user_id: String,
    pub change_id: String,
    pub device: String,
    pub timestamp: String,
}

pub async fn get_presence(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<PresenceResponse>>, StatusCode> {
    authz::require_read(&state, &user, &repo_id).await?;

    let doc = state.docs.get_or_load(&repo_id).await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let doc = doc.read().await;
    let presence_list = doc.get_all_presence();

    Ok(Json(presence_list.into_iter().map(|p| PresenceResponse {
        user_id: p.user_id,
        change_id: p.change_id.to_string(),
        device: p.device,
        timestamp: p.timestamp.to_rfc3339(),
    }).collect()))
}
