//! Ingestion of a locally prepared graph. Heads remain unchanged
//! until every supplied identity and normalized payload has been checked.
use anyhow::{bail, Result};
use jj_tandem_protocol::wire::{self, PreparedPublish};
use prost::Message as _;

use super::{decode_operation_with_id, decode_view_with_id, Repository};

#[derive(Debug)]
pub enum PreparedPublishError {
    Invalid,
    IdentityMismatch,
}
impl std::fmt::Display for PreparedPublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid prepared publish",
            Self::IdentityMismatch => {
                "prepared identity or normalized payload differs; heads were not published"
            }
        })
    }
}
impl std::error::Error for PreparedPublishError {}

impl Repository {
    pub fn stage_prepared_publish_sync(&self, request: &PreparedPublish) -> Result<()> {
        if let Err(error) = validate(request) {
            return Err(if error.is::<PreparedPublishError>() {
                error
            } else {
                PreparedPublishError::Invalid.into()
            });
        }
        for object in &request.objects {
            let kind = wire::kind_name(object.kind).ok_or(PreparedPublishError::Invalid)?;
            let (id, normalized) = self.put_object_sync(kind, &object.data)?;
            if id != object.id || normalized != object.data {
                return Err(PreparedPublishError::IdentityMismatch.into());
            }
        }
        self.put_operation_with_view_sync(&request.view, &request.operation)?;
        Ok(())
    }
}

fn validate(request: &PreparedPublish) -> Result<()> {
    if request.objects.len() > wire::MAX_PREPARED_OBJECTS
        || request.view.len() > super::MAX_VIEW_BYTES
    {
        bail!("prepared graph exceeds bounds");
    }
    let (view_id, _) = decode_view_with_id(&request.view)?;
    let (operation_id, operation) = decode_operation_with_id(&request.operation)?;
    super::validate_operation_publish_size(&request.operation, &operation)?;
    if operation.view_id.as_bytes() != view_id
        || jj_tandem_protocol::hex::from_hex(&request.heads.new_id)? != operation_id
    {
        return Err(PreparedPublishError::IdentityMismatch.into());
    }
    for object in &request.objects {
        if object.id.len() != 20 {
            bail!("invalid native Git identity");
        }
        match object.kind {
            wire::KIND_FILE => {}
            wire::KIND_SYMLINK => {
                std::str::from_utf8(&object.data)?;
            }
            wire::KIND_TREE => {
                let tree = jj_lib::protos::simple_store::Tree::decode(object.data.as_slice())?;
                let mut previous = None;
                for entry in tree.entries {
                    jj_lib::repo_path::RepoPathComponentBuf::new(entry.name.clone())?;
                    if previous.as_ref().is_some_and(|name| name >= &entry.name) {
                        bail!("unsorted tree");
                    }
                    previous = Some(entry.name);
                    use jj_lib::protos::simple_store::tree_value::Value;
                    match entry.value.and_then(|value| value.value) {
                        Some(Value::TreeId(id) | Value::SymlinkId(id)) if id.len() == 20 => {}
                        Some(Value::File(file)) if file.id.len() == 20 => {}
                        _ => bail!("unsupported prepared tree entry"),
                    }
                }
            }
            wire::KIND_COMMIT => {
                let commit = jj_lib::protos::simple_store::Commit::decode(object.data.as_slice())?;
                if commit.parents.is_empty()
                    || commit.parents.iter().any(|id| id.len() != 20)
                    || commit.predecessors.iter().any(|id| id.len() != 20)
                    || commit.change_id.len() != 16
                    || commit.root_tree.is_empty()
                    || commit.root_tree.len() % 2 == 0
                    || commit.root_tree.iter().any(|id| id.len() != 20)
                    || (!commit.conflict_labels.is_empty() && commit.conflict_labels.len() % 2 == 0)
                    || commit
                        .conflict_labels
                        .iter()
                        .any(|label| label.contains('\n'))
                    || commit.secure_sig.is_some()
                {
                    bail!("unsupported prepared commit");
                }
            }
            _ => bail!("unsupported prepared object kind"),
        }
    }
    Ok(())
}

use jj_lib::object_id::ObjectId as _;
