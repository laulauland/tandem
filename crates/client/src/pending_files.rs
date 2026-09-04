//! A bounded set of file uploads that must finish before dependent objects.

use std::collections::BTreeMap;

use anyhow::Result;

// Leave ample room below the HTTP body ceiling, and bound bookkeeping even
// for tiny files. Sizes include the existing batch frame's eight-byte header
// and six-byte record headers.
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_BATCH_FILES: usize = 4096;
const FRAME_BYTES: usize = 8;
const RECORD_BYTES: usize = 6;

pub(crate) struct PendingFiles {
    files: BTreeMap<Vec<u8>, Vec<u8>>,
    encoded_bytes: usize,
}

impl Default for PendingFiles {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            encoded_bytes: FRAME_BYTES,
        }
    }
}

impl PendingFiles {
    pub(crate) fn get(&self, id: &[u8]) -> Option<&Vec<u8>> {
        self.files.get(id)
    }

    pub(crate) fn fits_empty(data_len: usize) -> bool {
        data_len <= MAX_BATCH_BYTES - FRAME_BYTES - RECORD_BYTES
    }

    pub(crate) fn fits(&self, data_len: usize) -> bool {
        self.files.len() < MAX_BATCH_FILES
            && data_len
                .checked_add(RECORD_BYTES)
                .and_then(|size| size.checked_add(self.encoded_bytes))
                .is_some_and(|size| size <= MAX_BATCH_BYTES)
    }

    pub(crate) fn insert(&mut self, id: Vec<u8>, data: Vec<u8>) {
        use std::collections::btree_map::Entry;
        if let Entry::Vacant(entry) = self.files.entry(id) {
            self.encoded_bytes += RECORD_BYTES + data.len();
            entry.insert(data);
        }
    }

    pub(crate) fn flush(
        &mut self,
        upload: impl FnOnce(&BTreeMap<Vec<u8>, Vec<u8>>) -> Result<()>,
    ) -> Result<()> {
        if self.files.is_empty() {
            return Ok(());
        }
        // Keep the bytes until the whole response has been validated. A lost
        // response or partial success can be retried using the same IDs.
        upload(&self.files)?;
        self.files.clear();
        self.encoded_bytes = FRAME_BYTES;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use jj_tandem_protocol::wire::{self, BatchItem, KIND_FILE};

    #[test]
    fn failed_uploads_keep_exact_bytes_for_the_next_attempt() {
        let mut pending = PendingFiles::default();
        pending.insert(vec![1], vec![0, 255]);
        pending.insert(vec![2], b"second".to_vec());
        let mut failed_attempt = None;
        assert!(pending
            .flush(|files| {
                failed_attempt = Some(files.clone());
                bail!("partial or transport failure")
            })
            .is_err());
        assert_eq!(pending.get(&[1]), Some(&vec![0, 255]));
        pending
            .flush(|files| {
                assert_eq!(Some(files), failed_attempt.as_ref());
                Ok(())
            })
            .unwrap();
        assert!(pending.get(&[1]).is_none());
        pending
            .flush(|_| panic!("an empty buffer sends nothing"))
            .unwrap();
    }

    #[test]
    fn the_limit_includes_framing_and_duplicates_use_no_extra_space() {
        let mut pending = PendingFiles::default();
        let data = vec![7; MAX_BATCH_BYTES - FRAME_BYTES - RECORD_BYTES];
        assert!(PendingFiles::fits_empty(data.len()));
        assert!(!PendingFiles::fits_empty(data.len() + 1));
        assert!(pending.fits(data.len()));
        pending.insert(vec![1], data.clone());
        pending.insert(vec![1], data);
        assert!(!pending.fits(0));
        pending
            .flush(|files| {
                let items = files
                    .values()
                    .map(|data| BatchItem {
                        kind: KIND_FILE,
                        data: data.clone(),
                    })
                    .collect::<Vec<_>>();
                assert_eq!(items.len(), 1);
                assert_eq!(wire::encode_batch_request(&items).len(), MAX_BATCH_BYTES);
                Ok(())
            })
            .unwrap();
        assert!(pending.fits(0));
    }

    #[test]
    fn tiny_files_are_bounded_by_item_count() {
        let mut pending = PendingFiles::default();
        for index in 0..MAX_BATCH_FILES {
            assert!(pending.fits(0));
            pending.insert(index.to_le_bytes().to_vec(), Vec::new());
        }
        assert!(!pending.fits(0));
    }
}
