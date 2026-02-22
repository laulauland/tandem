//! Protobuf encoding/decoding for jj objects.
//!
//! These are reimplementations of jj-lib's private encoding functions,
//! using the public proto types from jj_lib::protos.

use std::collections::BTreeMap;

use jj_lib::backend::*;
use jj_lib::merge::Merge;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::*;
use jj_lib::ref_name::{GitRefNameBuf, RefNameBuf, RemoteNameBuf, WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo_path::RepoPathComponentBuf;
use prost::Message as _;

// ─── Backend: Tree ────────────────────────────────────────────────────────────

pub fn tree_to_proto(tree: &Tree) -> jj_lib::protos::simple_store::Tree {
    let mut proto = jj_lib::protos::simple_store::Tree::default();
    for entry in tree.entries() {
        proto
            .entries
            .push(jj_lib::protos::simple_store::tree::Entry {
                name: entry.name().as_internal_str().to_owned(),
                value: Some(tree_value_to_proto(entry.value())),
            });
    }
    proto
}

pub fn tree_from_proto(proto: jj_lib::protos::simple_store::Tree) -> Tree {
    let entries = proto
        .entries
        .into_iter()
        .map(|proto_entry| {
            let value = tree_value_from_proto(proto_entry.value.unwrap());
            (RepoPathComponentBuf::new(proto_entry.name).unwrap(), value)
        })
        .collect();
    Tree::from_sorted_entries(entries)
}

fn tree_value_to_proto(value: &TreeValue) -> jj_lib::protos::simple_store::TreeValue {
    let mut proto = jj_lib::protos::simple_store::TreeValue::default();
    match value {
        TreeValue::File {
            id,
            executable,
            copy_id,
        } => {
            proto.value = Some(jj_lib::protos::simple_store::tree_value::Value::File(
                jj_lib::protos::simple_store::tree_value::File {
                    id: id.to_bytes(),
                    executable: *executable,
                    copy_id: copy_id.to_bytes(),
                },
            ));
        }
        TreeValue::Symlink(id) => {
            proto.value = Some(jj_lib::protos::simple_store::tree_value::Value::SymlinkId(
                id.to_bytes(),
            ));
        }
        TreeValue::Tree(id) => {
            proto.value = Some(jj_lib::protos::simple_store::tree_value::Value::TreeId(
                id.to_bytes(),
            ));
        }
        TreeValue::GitSubmodule(_) => {
            panic!("cannot store git submodules in tandem backend");
        }
    }
    proto
}

fn tree_value_from_proto(proto: jj_lib::protos::simple_store::TreeValue) -> TreeValue {
    match proto.value.unwrap() {
        jj_lib::protos::simple_store::tree_value::Value::TreeId(id) => {
            TreeValue::Tree(TreeId::new(id))
        }
        jj_lib::protos::simple_store::tree_value::Value::File(
            jj_lib::protos::simple_store::tree_value::File {
                id,
                executable,
                copy_id,
            },
        ) => TreeValue::File {
            id: FileId::new(id),
            executable,
            copy_id: CopyId::new(copy_id),
        },
        jj_lib::protos::simple_store::tree_value::Value::SymlinkId(id) => {
            TreeValue::Symlink(SymlinkId::new(id))
        }
    }
}

// ─── Backend: Commit ──────────────────────────────────────────────────────────

// commit_to_proto is public from jj_lib::simple_backend::commit_to_proto

pub fn commit_from_proto(mut proto: jj_lib::protos::simple_store::Commit) -> Commit {
    // Extract secure_sig before partial moves of proto fields
    let secure_sig = proto.secure_sig.take().map(|sig| SecureSig {
        data: proto.encode_to_vec(),
        sig,
    });

    let parents = proto.parents.into_iter().map(CommitId::new).collect();
    let predecessors = proto.predecessors.into_iter().map(CommitId::new).collect();
    let merge_builder: jj_lib::merge::MergeBuilder<_> =
        proto.root_tree.into_iter().map(TreeId::new).collect();
    let root_tree = merge_builder.build();
    let conflict_labels = jj_lib::conflict_labels::ConflictLabels::from_vec(proto.conflict_labels);
    let change_id = ChangeId::new(proto.change_id);

    Commit {
        parents,
        predecessors,
        root_tree,
        conflict_labels: conflict_labels.into_merge(),
        change_id,
        description: proto.description,
        author: signature_from_proto(proto.author.unwrap_or_default()),
        committer: signature_from_proto(proto.committer.unwrap_or_default()),
        secure_sig,
    }
}

#[allow(dead_code)]
fn signature_to_proto(signature: &Signature) -> jj_lib::protos::simple_store::commit::Signature {
    jj_lib::protos::simple_store::commit::Signature {
        name: signature.name.clone(),
        email: signature.email.clone(),
        timestamp: Some(jj_lib::protos::simple_store::commit::Timestamp {
            millis_since_epoch: signature.timestamp.timestamp.0,
            tz_offset: signature.timestamp.tz_offset,
        }),
    }
}

fn signature_from_proto(proto: jj_lib::protos::simple_store::commit::Signature) -> Signature {
    let timestamp = proto.timestamp.unwrap_or_default();
    Signature {
        name: proto.name,
        email: proto.email,
        timestamp: Timestamp {
            timestamp: MillisSinceEpoch(timestamp.millis_since_epoch),
            tz_offset: timestamp.tz_offset,
        },
    }
}

// ─── OpStore: Timestamp helpers ───────────────────────────────────────────────

fn op_timestamp_to_proto(timestamp: &Timestamp) -> jj_lib::protos::simple_op_store::Timestamp {
    jj_lib::protos::simple_op_store::Timestamp {
        millis_since_epoch: timestamp.timestamp.0,
        tz_offset: timestamp.tz_offset,
    }
}

fn op_timestamp_from_proto(proto: jj_lib::protos::simple_op_store::Timestamp) -> Timestamp {
    Timestamp {
        timestamp: MillisSinceEpoch(proto.millis_since_epoch),
        tz_offset: proto.tz_offset,
    }
}

// ─── OpStore: Operation ───────────────────────────────────────────────────────

pub fn operation_to_proto(operation: &Operation) -> jj_lib::protos::simple_op_store::Operation {
    let (commit_predecessors, stores_commit_predecessors) = match &operation.commit_predecessors {
        Some(map) => (commit_predecessors_map_to_proto(map), true),
        None => (vec![], false),
    };
    let parents = operation.parents.iter().map(|id| id.to_bytes()).collect();
    jj_lib::protos::simple_op_store::Operation {
        view_id: operation.view_id.as_bytes().to_vec(),
        parents,
        metadata: Some(operation_metadata_to_proto(&operation.metadata)),
        commit_predecessors,
        stores_commit_predecessors,
    }
}

pub fn operation_from_proto(
    proto: jj_lib::protos::simple_op_store::Operation,
) -> anyhow::Result<Operation> {
    let parents: Vec<OperationId> = proto
        .parents
        .into_iter()
        .map(|bytes| {
            anyhow::ensure!(
                bytes.len() == 64,
                "invalid operation id length: {}",
                bytes.len()
            );
            Ok(OperationId::new(bytes))
        })
        .collect::<anyhow::Result<_>>()?;
    let view_id = {
        anyhow::ensure!(
            proto.view_id.len() == 64,
            "invalid view id length: {}",
            proto.view_id.len()
        );
        ViewId::new(proto.view_id)
    };
    let metadata = operation_metadata_from_proto(proto.metadata.unwrap_or_default());
    let commit_predecessors = proto
        .stores_commit_predecessors
        .then(|| commit_predecessors_map_from_proto(proto.commit_predecessors));
    Ok(Operation {
        view_id,
        parents,
        metadata,
        commit_predecessors,
    })
}

fn operation_metadata_to_proto(
    metadata: &OperationMetadata,
) -> jj_lib::protos::simple_op_store::OperationMetadata {
    jj_lib::protos::simple_op_store::OperationMetadata {
        start_time: Some(op_timestamp_to_proto(&metadata.time.start)),
        end_time: Some(op_timestamp_to_proto(&metadata.time.end)),
        description: metadata.description.clone(),
        hostname: metadata.hostname.clone(),
        username: metadata.username.clone(),
        is_snapshot: metadata.is_snapshot,
        tags: metadata.tags.clone(),
    }
}

fn operation_metadata_from_proto(
    proto: jj_lib::protos::simple_op_store::OperationMetadata,
) -> OperationMetadata {
    let time = TimestampRange {
        start: op_timestamp_from_proto(proto.start_time.unwrap_or_default()),
        end: op_timestamp_from_proto(proto.end_time.unwrap_or_default()),
    };
    OperationMetadata {
        time,
        description: proto.description,
        hostname: proto.hostname,
        username: proto.username,
        is_snapshot: proto.is_snapshot,
        tags: proto.tags,
    }
}

fn commit_predecessors_map_to_proto(
    map: &BTreeMap<CommitId, Vec<CommitId>>,
) -> Vec<jj_lib::protos::simple_op_store::CommitPredecessors> {
    map.iter()
        .map(
            |(commit_id, predecessor_ids)| jj_lib::protos::simple_op_store::CommitPredecessors {
                commit_id: commit_id.to_bytes(),
                predecessor_ids: predecessor_ids.iter().map(|id| id.to_bytes()).collect(),
            },
        )
        .collect()
}

fn commit_predecessors_map_from_proto(
    proto: Vec<jj_lib::protos::simple_op_store::CommitPredecessors>,
) -> BTreeMap<CommitId, Vec<CommitId>> {
    proto
        .into_iter()
        .map(|entry| {
            let commit_id = CommitId::new(entry.commit_id);
            let predecessor_ids = entry
                .predecessor_ids
                .into_iter()
                .map(CommitId::new)
                .collect();
            (commit_id, predecessor_ids)
        })
        .collect()
}

// ─── OpStore: View ────────────────────────────────────────────────────────────

pub fn view_to_proto(view: &View) -> jj_lib::protos::simple_op_store::View {
    let wc_commit_ids = view
        .wc_commit_ids
        .iter()
        .map(|(name, id): (&WorkspaceNameBuf, &CommitId)| {
            (AsRef::<str>::as_ref(name).to_owned(), id.to_bytes())
        })
        .collect();
    let head_ids = view.head_ids.iter().map(|id| id.to_bytes()).collect();

    let bookmarks = bookmark_views_to_proto_legacy(&view.local_bookmarks, &view.remote_views);

    let local_tags = view
        .local_tags
        .iter()
        .map(|(name, target)| jj_lib::protos::simple_op_store::Tag {
            name: AsRef::<str>::as_ref(name).to_owned(),
            target: ref_target_to_proto(target),
        })
        .collect();

    let remote_views = remote_views_to_proto(&view.remote_views);

    let git_refs = view
        .git_refs
        .iter()
        .map(|(name, target)| {
            #[allow(deprecated)]
            jj_lib::protos::simple_op_store::GitRef {
                name: AsRef::<str>::as_ref(name).to_owned(),
                commit_id: Default::default(),
                target: ref_target_to_proto(target),
            }
        })
        .collect();

    let git_head = ref_target_to_proto(&view.git_head);

    #[allow(deprecated)]
    jj_lib::protos::simple_op_store::View {
        head_ids,
        wc_commit_id: Default::default(),
        wc_commit_ids,
        bookmarks,
        local_tags,
        remote_views,
        git_refs,
        git_head_legacy: Default::default(),
        git_head,
        has_git_refs_migrated_to_remote_tags: true,
    }
}

pub fn view_from_proto(proto: jj_lib::protos::simple_op_store::View) -> anyhow::Result<View> {
    let mut wc_commit_ids = BTreeMap::new();
    #[allow(deprecated)]
    if !proto.wc_commit_id.is_empty() {
        wc_commit_ids.insert(
            WorkspaceName::DEFAULT.to_owned(),
            CommitId::new(proto.wc_commit_id),
        );
    }
    for (name, commit_id) in proto.wc_commit_ids {
        wc_commit_ids.insert(WorkspaceNameBuf::from(name), CommitId::new(commit_id));
    }
    let head_ids = proto.head_ids.into_iter().map(CommitId::new).collect();

    let (local_bookmarks, mut remote_views) = bookmark_views_from_proto_legacy(proto.bookmarks)?;

    let local_tags = proto
        .local_tags
        .into_iter()
        .map(|tag_proto| {
            let name: RefNameBuf = tag_proto.name.into();
            (name, ref_target_from_proto(tag_proto.target))
        })
        .collect();

    let git_refs: BTreeMap<_, _> = proto
        .git_refs
        .into_iter()
        .map(|git_ref| {
            let name: GitRefNameBuf = git_ref.name.into();
            let target = if git_ref.target.is_some() {
                ref_target_from_proto(git_ref.target)
            } else {
                #[allow(deprecated)]
                RefTarget::normal(CommitId::new(git_ref.commit_id))
            };
            (name, target)
        })
        .collect();

    // Use new remote_views format when available
    if !proto.remote_views.is_empty() {
        remote_views = remote_views_from_proto(proto.remote_views)?;
    }

    #[allow(deprecated)]
    let git_head = if proto.git_head.is_some() {
        ref_target_from_proto(proto.git_head)
    } else if !proto.git_head_legacy.is_empty() {
        RefTarget::normal(CommitId::new(proto.git_head_legacy))
    } else {
        RefTarget::absent()
    };

    Ok(View {
        head_ids,
        local_bookmarks,
        local_tags,
        remote_views,
        git_refs,
        git_head,
        wc_commit_ids,
    })
}

// ─── RefTarget helpers ────────────────────────────────────────────────────────

fn ref_target_to_proto(value: &RefTarget) -> Option<jj_lib::protos::simple_op_store::RefTarget> {
    let term_to_proto =
        |term: &Option<CommitId>| jj_lib::protos::simple_op_store::ref_conflict::Term {
            value: term.as_ref().map(|id| id.to_bytes()),
        };
    let merge = value.as_merge();
    let conflict_proto = jj_lib::protos::simple_op_store::RefConflict {
        removes: merge.removes().map(term_to_proto).collect(),
        adds: merge.adds().map(term_to_proto).collect(),
    };
    Some(jj_lib::protos::simple_op_store::RefTarget {
        value: Some(jj_lib::protos::simple_op_store::ref_target::Value::Conflict(conflict_proto)),
    })
}

fn ref_target_from_proto(
    maybe_proto: Option<jj_lib::protos::simple_op_store::RefTarget>,
) -> RefTarget {
    let Some(proto) = maybe_proto else {
        return RefTarget::absent();
    };
    match proto.value.unwrap() {
        #[allow(deprecated)]
        jj_lib::protos::simple_op_store::ref_target::Value::CommitId(id) => {
            RefTarget::normal(CommitId::new(id))
        }
        #[allow(deprecated)]
        jj_lib::protos::simple_op_store::ref_target::Value::ConflictLegacy(conflict) => {
            let removes = conflict.removes.into_iter().map(CommitId::new);
            let adds = conflict.adds.into_iter().map(CommitId::new);
            RefTarget::from_legacy_form(removes, adds)
        }
        jj_lib::protos::simple_op_store::ref_target::Value::Conflict(conflict) => {
            let term_from_proto = |term: jj_lib::protos::simple_op_store::ref_conflict::Term| {
                term.value.map(CommitId::new)
            };
            let removes = conflict.removes.into_iter().map(term_from_proto);
            let adds = conflict.adds.into_iter().map(term_from_proto);
            RefTarget::from_merge(Merge::from_removes_adds(removes, adds))
        }
    }
}

// ─── Bookmark/RemoteView helpers ──────────────────────────────────────────────

fn bookmark_views_to_proto_legacy(
    local_bookmarks: &BTreeMap<RefNameBuf, RefTarget>,
    remote_views: &BTreeMap<RemoteNameBuf, RemoteView>,
) -> Vec<jj_lib::protos::simple_op_store::Bookmark> {
    // Collect all bookmark names (local + remote)
    let mut all_names: std::collections::BTreeSet<RefNameBuf> = std::collections::BTreeSet::new();
    for name in local_bookmarks.keys() {
        all_names.insert(name.clone());
    }
    for remote_view in remote_views.values() {
        for name in remote_view.bookmarks.keys() {
            all_names.insert(name.clone());
        }
    }

    all_names
        .into_iter()
        .map(|name| {
            let local_target = local_bookmarks
                .get(&name)
                .map(ref_target_to_proto)
                .unwrap_or(ref_target_to_proto(&RefTarget::absent()));
            let remote_bookmarks: Vec<_> = remote_views
                .iter()
                .filter_map(|(remote_name, remote_view)| {
                    remote_view.bookmarks.get(&name).map(|remote_ref| {
                        #[allow(deprecated)]
                        jj_lib::protos::simple_op_store::RemoteBookmark {
                            remote_name: AsRef::<str>::as_ref(remote_name).to_owned(),
                            target: ref_target_to_proto(&remote_ref.target),
                            state: Some(remote_ref_state_to_proto(remote_ref.state)),
                        }
                    })
                })
                .collect();
            #[allow(deprecated)]
            jj_lib::protos::simple_op_store::Bookmark {
                name: AsRef::<str>::as_ref(&name).to_owned(),
                local_target,
                remote_bookmarks,
            }
        })
        .collect()
}

type BookmarkViews = (
    BTreeMap<RefNameBuf, RefTarget>,
    BTreeMap<RemoteNameBuf, RemoteView>,
);

fn bookmark_views_from_proto_legacy(
    bookmarks_legacy: Vec<jj_lib::protos::simple_op_store::Bookmark>,
) -> anyhow::Result<BookmarkViews> {
    let mut local_bookmarks: BTreeMap<RefNameBuf, RefTarget> = BTreeMap::new();
    let mut remote_views: BTreeMap<RemoteNameBuf, RemoteView> = BTreeMap::new();
    for bookmark_proto in bookmarks_legacy {
        let bookmark_name: RefNameBuf = bookmark_proto.name.into();
        let local_target = ref_target_from_proto(bookmark_proto.local_target);
        #[allow(deprecated)]
        let remote_bookmarks = bookmark_proto.remote_bookmarks;
        for remote_bookmark in remote_bookmarks {
            let remote_name: RemoteNameBuf = remote_bookmark.remote_name.into();
            let state = match remote_bookmark.state {
                Some(n) => remote_ref_state_from_proto(n)?,
                None => RemoteRefState::New,
            };
            let remote_view = remote_views.entry(remote_name).or_default();
            let remote_ref = RemoteRef {
                target: ref_target_from_proto(remote_bookmark.target),
                state,
            };
            remote_view
                .bookmarks
                .insert(bookmark_name.clone(), remote_ref);
        }
        if local_target.is_present() {
            local_bookmarks.insert(bookmark_name, local_target);
        }
    }
    Ok((local_bookmarks, remote_views))
}

fn remote_views_to_proto(
    remote_views: &BTreeMap<RemoteNameBuf, RemoteView>,
) -> Vec<jj_lib::protos::simple_op_store::RemoteView> {
    remote_views
        .iter()
        .map(|(name, view)| jj_lib::protos::simple_op_store::RemoteView {
            name: AsRef::<str>::as_ref(name).to_owned(),
            bookmarks: remote_refs_to_proto(&view.bookmarks),
            tags: remote_refs_to_proto(&view.tags),
        })
        .collect()
}

fn remote_views_from_proto(
    remote_views_proto: Vec<jj_lib::protos::simple_op_store::RemoteView>,
) -> anyhow::Result<BTreeMap<RemoteNameBuf, RemoteView>> {
    remote_views_proto
        .into_iter()
        .map(|proto| {
            let name: RemoteNameBuf = proto.name.into();
            let view = RemoteView {
                bookmarks: remote_refs_from_proto(proto.bookmarks)?,
                tags: remote_refs_from_proto(proto.tags)?,
            };
            Ok((name, view))
        })
        .collect()
}

fn remote_refs_to_proto(
    remote_refs: &BTreeMap<RefNameBuf, RemoteRef>,
) -> Vec<jj_lib::protos::simple_op_store::RemoteRef> {
    remote_refs
        .iter()
        .map(
            |(name, remote_ref)| jj_lib::protos::simple_op_store::RemoteRef {
                name: AsRef::<str>::as_ref(name).to_owned(),
                target_terms: ref_target_to_terms_proto(&remote_ref.target),
                state: remote_ref_state_to_proto(remote_ref.state),
            },
        )
        .collect()
}

fn remote_refs_from_proto(
    remote_refs_proto: Vec<jj_lib::protos::simple_op_store::RemoteRef>,
) -> anyhow::Result<BTreeMap<RefNameBuf, RemoteRef>> {
    remote_refs_proto
        .into_iter()
        .map(|proto| {
            let name: RefNameBuf = proto.name.into();
            let remote_ref = RemoteRef {
                target: ref_target_from_terms_proto(proto.target_terms)?,
                state: remote_ref_state_from_proto(proto.state)?,
            };
            Ok((name, remote_ref))
        })
        .collect()
}

fn ref_target_to_terms_proto(
    value: &RefTarget,
) -> Vec<jj_lib::protos::simple_op_store::RefTargetTerm> {
    value
        .as_merge()
        .iter()
        .map(|term| term.as_ref().map(|id| id.to_bytes()))
        .map(|value| jj_lib::protos::simple_op_store::RefTargetTerm { value })
        .collect()
}

fn ref_target_from_terms_proto(
    proto: Vec<jj_lib::protos::simple_op_store::RefTargetTerm>,
) -> anyhow::Result<RefTarget> {
    let terms: Vec<_> = proto
        .into_iter()
        .map(|jj_lib::protos::simple_op_store::RefTargetTerm { value }| value.map(CommitId::new))
        .collect();
    anyhow::ensure!(
        !terms.len().is_multiple_of(2),
        "even number of ref target terms: {}",
        terms.len()
    );
    let small: smallvec::SmallVec<[_; 1]> = terms.into();
    Ok(RefTarget::from_merge(Merge::from_vec(small)))
}

fn remote_ref_state_to_proto(state: RemoteRefState) -> i32 {
    let proto_state = match state {
        RemoteRefState::New => jj_lib::protos::simple_op_store::RemoteRefState::New,
        RemoteRefState::Tracked => jj_lib::protos::simple_op_store::RemoteRefState::Tracked,
    };
    proto_state as i32
}

fn remote_ref_state_from_proto(proto_value: i32) -> anyhow::Result<RemoteRefState> {
    let proto_state: jj_lib::protos::simple_op_store::RemoteRefState = proto_value
        .try_into()
        .map_err(|prost::UnknownEnumValue(n)| anyhow::anyhow!("invalid remote ref state: {n}"))?;
    Ok(match proto_state {
        jj_lib::protos::simple_op_store::RemoteRefState::New => RemoteRefState::New,
        jj_lib::protos::simple_op_store::RemoteRefState::Tracked => RemoteRefState::Tracked,
    })
}
