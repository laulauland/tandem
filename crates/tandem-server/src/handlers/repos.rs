use axum::{extract::{Path, State}, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use crate::{AppState, auth::AuthenticatedUser, authz};

#[derive(Serialize)]
pub struct RepoResponse {
    pub id: String,
    pub name: String,
    pub org: String,
    pub created_at: String,
}

#[derive(Deserialize)]
pub struct CreateRepoRequest {
    pub name: String,
    pub org: String,
}

pub async fn list_repos(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<RepoResponse>>, StatusCode> {
    // Filter by user access
    let repos = state.db.list_repos_for_user(&user.id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(repos.into_iter().map(|r| RepoResponse {
        id: r.id,
        name: r.name,
        org: r.org,
        created_at: r.created_at.to_rfc3339(),
    }).collect()))
}

pub async fn create_repo(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(req): Json<CreateRepoRequest>,
) -> Result<Json<RepoResponse>, StatusCode> {
    let id = uuid::Uuid::new_v4().to_string();

    let repo = state.db.create_repo(&id, &req.name, &req.org).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Grant admin access to creator
    state.db.set_user_role(&user.id, &id, "admin").await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Create empty doc for repo
    state.docs.create(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(RepoResponse {
        id: repo.id,
        name: repo.name,
        org: repo.org,
        created_at: repo.created_at.to_rfc3339(),
    }))
}

pub async fn get_repo(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<RepoResponse>, StatusCode> {
    authz::require_read(&state, &user, &id).await?;

    let repo = state.db.get_repo(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(RepoResponse {
        id: repo.id,
        name: repo.name,
        org: repo.org,
        created_at: repo.created_at.to_rfc3339(),
    }))
}
