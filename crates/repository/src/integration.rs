//! The integration workspace: one commit that merges every workspace head.
//!
//! When `--integration-workspace` is on, the server keeps a bookmark called
//! `integration` pointing at a commit whose parents are the working-copy
//! commits of every workspace that has published. It is a read-only view of
//! "what everyone has right now", recomputed off the request path.
//!
//! The recompute is debounced and runs on a blocking thread, because a publish
//! must not wait on it: nothing about a client's own commit depends on the
//! integration commit existing. What the two share is `Repository::lock`, held only
//! while the new operation is published and the head version is bumped.
//!
//! `integration.json` next to `heads.json` records the last outcome so
//! `tandem status` can report it, and so a recompute whose inputs have not
//! moved can return without doing the merge again.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;
use serde::{Deserialize, Serialize};

use super::{HeadsMetadata, Repository};
use jj_tandem_protocol::hex::from_hex;

// ─── Recorded state ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IntegrationMetadata {
    enabled: bool,
    #[serde(default)]
    last_input_fingerprint: Option<String>,
    #[serde(default)]
    last_integration_commit: Option<String>,
    #[serde(default = "default_integration_status")]
    last_status: String,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    workspace_commit_count: Option<usize>,
}

fn default_integration_status() -> String {
    "idle".to_string()
}

fn now_epoch_secs_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn fingerprint_workspace_commits(workspace_commits: &BTreeMap<String, String>) -> String {
    workspace_commits
        .iter()
        .map(|(workspace, commit)| format!("{workspace}:{commit}"))
        .collect::<Vec<_>>()
        .join("|")
}

impl Repository {
    pub fn integration_metadata_path(&self) -> PathBuf {
        self.tandem_dir.join("integration.json")
    }

    pub(super) fn initialize_integration_metadata(&mut self) -> Result<()> {
        let mut metadata =
            self.read_integration_metadata()
                .unwrap_or_else(|_| IntegrationMetadata {
                    enabled: self.integration_enabled,
                    last_input_fingerprint: None,
                    last_integration_commit: None,
                    last_status: if self.integration_enabled {
                        "idle".to_string()
                    } else {
                        "disabled".to_string()
                    },
                    last_error: None,
                    updated_at: Some(now_epoch_secs_string()),
                    workspace_commit_count: Some(0),
                });
        metadata.enabled = self.integration_enabled;
        if !self.integration_enabled {
            metadata.last_status = "disabled".to_string();
        }
        self.write_integration_metadata(&metadata)
    }

    fn read_integration_metadata(&self) -> Result<IntegrationMetadata> {
        let bytes = fs::read(self.integration_metadata_path())?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn write_integration_metadata(&self, metadata: &IntegrationMetadata) -> Result<()> {
        fs::write(
            self.integration_metadata_path(),
            serde_json::to_vec_pretty(metadata)?,
        )?;
        Ok(())
    }

    fn record_integration_error(&self, err: &anyhow::Error) {
        let mut metadata = self
            .read_integration_metadata()
            .unwrap_or(IntegrationMetadata {
                enabled: true,
                last_input_fingerprint: None,
                last_integration_commit: None,
                last_status: "error".to_string(),
                last_error: None,
                updated_at: None,
                workspace_commit_count: None,
            });
        metadata.enabled = self.integration_enabled;
        metadata.last_status = "error".to_string();
        metadata.last_error = Some(format!("{err:#}"));
        metadata.updated_at = Some(now_epoch_secs_string());
        if let Err(write_err) = self.write_integration_metadata(&metadata) {
            tracing::error!(error = %write_err, "failed to persist integration error metadata");
        }
    }

    // ─── The worker ───────────────────────────────────────────────────

    pub fn start_integration_worker(self: &Arc<Self>) {
        if !self.integration_enabled {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        {
            let mut slot = self.integration_trigger.lock().unwrap();
            *slot = Some(tx);
        }
        let server = Arc::clone(self);
        tokio::spawn(async move {
            tracing::info!("integration worker started");
            while rx.recv().await.is_some() {
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                while rx.try_recv().is_ok() {}

                // The recompute loads a repo, merges trees and writes an
                // operation, all synchronously. It belongs on a blocking
                // thread, not on a reactor one.
                let worker = Arc::clone(&server);
                let outcome = tokio::task::spawn_blocking(move || {
                    if let Err(err) = worker.recompute_integration_bookmark() {
                        tracing::error!(error = %err, "integration recompute failed");
                        worker.record_integration_error(&err);
                    }
                })
                .await;
                if let Err(err) = outcome {
                    tracing::error!(error = %err, "integration worker task failed");
                }
            }
            tracing::info!("integration worker stopped");
        });
    }

    pub(super) fn enqueue_integration_recompute(&self) {
        let sender = self.integration_trigger.lock().unwrap().clone();
        if let Some(tx) = sender {
            let _ = tx.send(());
        }
    }

    fn recompute_integration_bookmark(&self) -> Result<()> {
        if !self.integration_enabled {
            return Ok(());
        }

        let workspace_heads = {
            let _guard = self.lock.lock().map_err(|e| anyhow!("lock: {e}"))?;
            self.read_heads_metadata()?.workspace_heads
        };
        let workspace_commits = self.resolve_workspace_commits(&workspace_heads)?;
        let input_fingerprint = fingerprint_workspace_commits(&workspace_commits);

        let mut metadata =
            self.read_integration_metadata()
                .unwrap_or_else(|_| IntegrationMetadata {
                    enabled: true,
                    last_input_fingerprint: None,
                    last_integration_commit: None,
                    last_status: "idle".to_string(),
                    last_error: None,
                    updated_at: None,
                    workspace_commit_count: Some(0),
                });
        metadata.enabled = true;

        if workspace_commits.is_empty() {
            metadata.last_input_fingerprint = Some(input_fingerprint);
            metadata.last_status = "idle".to_string();
            metadata.last_error = None;
            metadata.workspace_commit_count = Some(0);
            metadata.updated_at = Some(now_epoch_secs_string());
            self.write_integration_metadata(&metadata)?;
            tracing::debug!("integration recompute skipped: no workspace commits");
            return Ok(());
        }

        let already_current = metadata.last_input_fingerprint.as_deref()
            == Some(&input_fingerprint)
            && matches!(metadata.last_status.as_str(), "clean" | "conflicted");
        if already_current {
            return Ok(());
        }

        let mut parent_hexes: Vec<String> = workspace_commits.values().cloned().collect();
        parent_hexes.sort();
        parent_hexes.dedup();

        let readonly_repo = self
            .repo_loader
            .load_at_head()
            .context("load repo at head")?;
        let parent_ids: Vec<CommitId> = parent_hexes
            .iter()
            .map(|hex| from_hex(hex).map(CommitId::new))
            .collect::<Result<Vec<_>>>()?;
        let parent_commits: Vec<_> = parent_ids
            .iter()
            .map(|id| readonly_repo.store().get_commit(id))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("load parent commit: {e}"))?;

        let merged_tree =
            pollster::block_on(merge_commit_trees(readonly_repo.as_ref(), &parent_commits))
                .map_err(|e| anyhow!("merge workspace commits: {e}"))?;

        let mut tx = readonly_repo.start_transaction();
        let mut commit_builder = tx.repo_mut().new_commit(parent_ids, merged_tree).detach();
        commit_builder.set_description("integration workspace recompute");
        let integration_commit = commit_builder
            .write(tx.repo_mut())
            .map_err(|e| anyhow!("write integration commit: {e}"))?;
        tx.repo_mut().set_local_bookmark_target(
            "integration".as_ref(),
            RefTarget::normal(integration_commit.id().clone()),
        );

        let unpublished = tx
            .write("integration workspace recompute")
            .map_err(|e| anyhow!("write integration operation: {e}"))?;

        {
            let _guard = self.lock.lock().map_err(|e| anyhow!("lock: {e}"))?;
            unpublished
                .publish()
                .map_err(|e| anyhow!("publish integration operation: {e}"))?;

            let heads_metadata = self.read_heads_metadata()?;
            let next_heads = self.read_jj_op_heads()?;
            let next_metadata = HeadsMetadata {
                version: heads_metadata.version + 1,
                workspace_heads: heads_metadata.workspace_heads,
            };
            self.write_heads_metadata(&next_metadata)?;
            tracing::trace!(
                heads = next_heads.len(),
                version = next_metadata.version,
                "integration recompute moved the head set"
            );
            self.notify_watchers(next_metadata.version);
        }

        metadata.last_input_fingerprint = Some(input_fingerprint);
        metadata.last_integration_commit = Some(integration_commit.id().hex());
        metadata.last_status = if integration_commit.has_conflict() {
            "conflicted".to_string()
        } else {
            "clean".to_string()
        };
        metadata.last_error = None;
        metadata.workspace_commit_count = Some(workspace_commits.len());
        metadata.updated_at = Some(now_epoch_secs_string());
        self.write_integration_metadata(&metadata)?;

        tracing::info!(
            status = %metadata.last_status,
            integration_commit = %integration_commit.id().hex(),
            workspace_commits = workspace_commits.len(),
            "integration recompute completed"
        );
        Ok(())
    }

    /// The working-copy commit each workspace's last published operation named.
    ///
    /// A workspace whose operation or view has gone missing is skipped with a
    /// warning rather than failing the recompute: the integration commit is a
    /// convenience, and one unreadable workspace must not stop the others from
    /// being merged.
    fn resolve_workspace_commits(
        &self,
        workspace_heads: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let op_store = self.repo_loader.op_store();
        let mut workspace_commits = BTreeMap::new();

        for (workspace_id, op_hex) in workspace_heads {
            let op_bytes = match from_hex(op_hex) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(workspace_id = %workspace_id, op_id = %op_hex, error = %err, "bad workspace op id");
                    continue;
                }
            };
            let op_id = jj_lib::op_store::OperationId::new(op_bytes);
            let operation = match pollster::block_on(op_store.read_operation(&op_id)) {
                Ok(op) => op,
                Err(err) => {
                    tracing::warn!(workspace_id = %workspace_id, op_id = %op_hex, error = %err, "operation missing for workspace");
                    continue;
                }
            };
            let view = match pollster::block_on(op_store.read_view(&operation.view_id)) {
                Ok(view) => view,
                Err(err) => {
                    tracing::warn!(workspace_id = %workspace_id, op_id = %op_hex, error = %err, "view missing for workspace operation");
                    continue;
                }
            };
            let key = jj_lib::ref_name::WorkspaceNameBuf::from(workspace_id.clone());
            if let Some(commit_id) = view.wc_commit_ids.get(&key) {
                workspace_commits.insert(workspace_id.clone(), commit_id.hex());
            }
        }

        Ok(workspace_commits)
    }
}
