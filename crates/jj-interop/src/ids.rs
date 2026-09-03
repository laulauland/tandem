//! Construction and recognition of jj identifiers at Tandem boundaries.

use jj_lib::backend::{ChangeId, CommitId, TreeId};
use jj_lib::op_store::{OperationId, ViewId};

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
