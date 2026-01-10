use axum::{extract::{Path, State}, http::StatusCode, Json};
use serde::Serialize;
use std::collections::HashMap;
use crate::{AppState, auth::AuthenticatedUser, authz};

#[derive(Serialize)]
pub struct ChangeResponse {
    pub change_id: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub description: String,
    pub author_email: String,
    pub author_name: Option<String>,
    pub timestamp: String,
    pub divergent: bool,
}

pub async fn list_changes(
    State(state): State<AppState>,
    Path(repo_id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<ChangeResponse>>, StatusCode> {
    authz::require_read(&state, &user, &repo_id).await?;

    let doc = state.docs.get_or_load(&repo_id).await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let doc = doc.read().await;
    let records = doc.get_all_change_records();

    // Group by change_id to detect divergence
    let mut by_id: HashMap<String, Vec<_>> = HashMap::new();
    for record in records {
        if record.visible {
            by_id.entry(record.change_id.to_string())
                .or_default()
                .push(record);
        }
    }

    let changes: Vec<ChangeResponse> = by_id.into_iter().map(|(id, records)| {
        let record = &records[0];
        ChangeResponse {
            change_id: id,
            tree: record.tree.to_string(),
            parents: record.parents.iter().map(|p| p.to_string()).collect(),
            description: record.description.clone(),
            author_email: record.author.email.clone(),
            author_name: record.author.name.clone(),
            timestamp: record.timestamp.to_rfc3339(),
            divergent: records.len() > 1,
        }
    }).collect();

    Ok(Json(changes))
}

pub async fn get_change(
    State(state): State<AppState>,
    Path((repo_id, change_id)): Path<(String, String)>,
    user: AuthenticatedUser,
) -> Result<Json<ChangeResponse>, StatusCode> {
    authz::require_read(&state, &user, &repo_id).await?;

    let doc = state.docs.get_or_load(&repo_id).await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let doc = doc.read().await;

    // Parse change_id
    let cid: tandem_core::types::ChangeId = change_id.parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let records = doc.get_change_records(&cid);
    if records.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }

    let record = &records[0];
    Ok(Json(ChangeResponse {
        change_id: record.change_id.to_string(),
        tree: record.tree.to_string(),
        parents: record.parents.iter().map(|p| p.to_string()).collect(),
        description: record.description.clone(),
        author_email: record.author.email.clone(),
        author_name: record.author.name.clone(),
        timestamp: record.timestamp.to_rfc3339(),
        divergent: records.len() > 1,
    }))
}
