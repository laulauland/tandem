//! Construction and recognition of jj identifiers at Tandem boundaries.

use jj_lib::backend::{ChangeId, CommitId, FileId, TreeId};
use jj_lib::op_store::{OperationId, ViewId};

/// GitBackend writes files as unmodified Git blobs. Use the same upstream
/// object framing and collision-detecting hash implementation before upload.
pub fn git_file(data: &[u8]) -> anyhow::Result<FileId> {
    let id = gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, data)?;
    Ok(FileId::from_bytes(id.as_bytes()))
}

pub fn commit(bytes: Vec<u8>) -> CommitId {
    CommitId::new(bytes)
}

pub fn change(bytes: Vec<u8>) -> ChangeId {
    ChangeId::new(bytes)
}

pub fn tree(bytes: Vec<u8>) -> TreeId {
    TreeId::new(bytes)
}

pub fn operation(bytes: Vec<u8>) -> OperationId {
    OperationId::new(bytes)
}

pub fn root_view(length: usize) -> ViewId {
    ViewId::from_bytes(&vec![0; length])
}

/// jj's synthetic root operation is represented by an all-zero identifier.
pub fn is_root_operation_hex(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte == b'0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use jj_lib::object_id::ObjectId as _;

    #[test]
    fn predicted_file_ids_match_jjs_git_backend() {
        use jj_lib::backend::Backend as _;
        use jj_lib::config::StackedConfig;
        use jj_lib::git_backend::GitBackend;
        use jj_lib::repo_path::RepoPath;
        use jj_lib::settings::UserSettings;

        let settings = UserSettings::from_config(StackedConfig::with_defaults()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let backend =
            GitBackend::init_internal(&settings, dir.path(), gix_hash::Kind::Sha1).unwrap();
        let payloads = [
            Vec::new(),
            b"hello\n".to_vec(),
            (0..=255).collect(),
            vec![42; 65537],
        ];
        for bytes in payloads {
            let mut input = bytes.as_slice();
            let actual =
                pollster::block_on(backend.write_file(RepoPath::root(), &mut input)).unwrap();
            assert_eq!(git_file(&bytes).unwrap(), actual);
        }
    }

    #[test]
    fn root_ids_keep_their_exact_bytes() {
        assert_eq!(commit(vec![1, 2]).as_bytes(), &[1, 2]);
        assert_eq!(change(vec![3, 4]).as_bytes(), &[3, 4]);
        assert_eq!(tree(vec![5, 6]).as_bytes(), &[5, 6]);
        assert_eq!(operation(vec![7, 8]).as_bytes(), &[7, 8]);
        assert_eq!(root_view(4).as_bytes(), &[0; 4]);
    }

    #[test]
    fn only_nonempty_all_zero_hex_is_the_root_operation() {
        assert!(is_root_operation_hex("0000"));
        assert!(!is_root_operation_hex(""));
        assert!(!is_root_operation_hex("0001"));
    }
}
