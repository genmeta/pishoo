//! Incremental BLAKE3 tree frontier; only the final leaf is reread on resume.
use blake3::hazmat::{HasherExt, Mode, merge_subtrees_non_root, merge_subtrees_root};
#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct Checkpoint {
    pub(super) offset: u64,
    pub(super) stack: Vec<[u8; 32]>,
}

impl Checkpoint {
    pub(super) fn tail_len(&self) -> usize {
        if self.offset == 0 {
            0
        } else {
            ((self.offset - 1) % 1024 + 1) as usize
        }
    }
    fn push(&mut self, mut cv: [u8; 32], start: u64, mut leaves: u64) {
        let count = start + leaves;
        while count & leaves == 0 {
            cv = merge_subtrees_non_root(&self.stack.pop().unwrap(), &cv, Mode::Hash);
            leaves *= 2;
        }
        self.stack.push(cv);
    }
    pub(super) fn append(&mut self, tail: &mut Vec<u8>, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if tail.len() == 1024 {
                let start = self.offset - 1024;
                let cv = blake3::Hasher::new()
                    .set_input_offset(start)
                    .update(tail)
                    .finalize_non_root();
                self.push(cv, start / 1024, 1);
                tail.clear();
            }
            if tail.is_empty() && bytes.len() > 1024 {
                // Bulk subtrees preserve the library's SIMD path. Keep the
                // last leaf separate so the final root flag is applied once.
                let available = ((bytes.len() - 1) / 1024 * 1024) as u64;
                let power = 1u64 << (63 - available.leading_zeros());
                let len = power.min(blake3::hazmat::max_subtree_len(self.offset).unwrap_or(power));
                let cv = blake3::Hasher::new()
                    .set_input_offset(self.offset)
                    .update(&bytes[..len as usize])
                    .finalize_non_root();
                self.push(cv, self.offset / 1024, len / 1024);
                self.offset += len;
                bytes = &bytes[len as usize..];
            } else {
                let len = (1024 - tail.len()).min(bytes.len());
                tail.extend_from_slice(&bytes[..len]);
                self.offset += len as u64;
                bytes = &bytes[len..];
            }
        }
    }
    pub(super) fn finish(&self, tail: &[u8]) -> blake3::Hash {
        if self.stack.is_empty() {
            return blake3::hash(tail);
        }
        let mut cv = blake3::Hasher::new()
            .set_input_offset(self.offset - tail.len() as u64)
            .update(tail)
            .finalize_non_root();
        for left in self.stack[1..].iter().rev() {
            cv = merge_subtrees_non_root(left, &cv, Mode::Hash);
        }
        merge_subtrees_root(&self.stack[0], &cv, Mode::Hash)
    }
}
