//! Deterministic score-root construction.
//!
//! The epoch score root is the value committed on-chain and exposed to
//! the bonded challenge window. Every conforming scorer implementation
//! must produce this root byte for byte from the same scored epoch.
//!
//! Construction (methodology v0):
//! - entries are sorted by identity bytes; duplicate identities are an
//!   error, never silently merged;
//! - leaf hash: `sha256(0x00 || identity(32) || score_be(16))`;
//! - node hash: `sha256(0x01 || left(32) || right(32))`;
//! - an odd node at any level is promoted unchanged;
//! - the empty epoch root is `sha256(0x02 || "poiw-empty-v0")`.

use sha2::{Digest, Sha256};

use poiw_types::IdentityId;

/// One identity's final score, as committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreEntry {
    pub identity: IdentityId,
    pub score: u128,
}

/// Commitment construction errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommitmentError {
    #[error("duplicate identity in score entries")]
    DuplicateIdentity,
}

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;
const EMPTY_PREFIX: u8 = 0x02;
const EMPTY_DOMAIN: &[u8] = b"poiw-empty-v0";

fn leaf_hash(entry: &ScoreEntry) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([LEAF_PREFIX]);
    hasher.update(entry.identity.0);
    hasher.update(entry.score.to_be_bytes());
    hasher.finalize().into()
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([NODE_PREFIX]);
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

fn empty_root() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([EMPTY_PREFIX]);
    hasher.update(EMPTY_DOMAIN);
    hasher.finalize().into()
}

/// Compute the epoch score root. Input order does not matter; entries
/// are sorted by identity before hashing. Duplicate identities are
/// rejected.
pub fn score_root(entries: &[ScoreEntry]) -> Result<[u8; 32], CommitmentError> {
    if entries.is_empty() {
        return Ok(empty_root());
    }
    let mut sorted: Vec<ScoreEntry> = entries.to_vec();
    sorted.sort_by(|a, b| a.identity.cmp(&b.identity));
    for pair in sorted.windows(2) {
        if let [a, b] = pair {
            if a.identity == b.identity {
                return Err(CommitmentError::DuplicateIdentity);
            }
        }
    }

    let mut level: Vec<[u8; 32]> = sorted.iter().map(leaf_hash).collect();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        let mut chunks = level.chunks_exact(2);
        for pair in chunks.by_ref() {
            if let [left, right] = pair {
                next.push(node_hash(left, right));
            }
        }
        if let [odd] = chunks.remainder() {
            next.push(*odd);
        }
        level = next;
    }
    level
        .first()
        .copied()
        .ok_or(CommitmentError::DuplicateIdentity)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    fn entry(id: u8, score: u128) -> ScoreEntry {
        ScoreEntry {
            identity: IdentityId([id; 32]),
            score,
        }
    }

    #[test]
    fn empty_root_is_stable_and_distinct() {
        let empty = score_root(&[]).unwrap();
        assert_eq!(empty, score_root(&[]).unwrap());
        let one = score_root(&[entry(1, 10)]).unwrap();
        assert_ne!(empty, one);
    }

    #[test]
    fn root_is_order_independent() {
        let forward = score_root(&[entry(1, 10), entry(2, 20), entry(3, 30)]).unwrap();
        let shuffled = score_root(&[entry(3, 30), entry(1, 10), entry(2, 20)]).unwrap();
        assert_eq!(forward, shuffled);
    }

    #[test]
    fn root_depends_on_scores_and_membership() {
        let base = score_root(&[entry(1, 10), entry(2, 20)]).unwrap();
        let changed_score = score_root(&[entry(1, 10), entry(2, 21)]).unwrap();
        let extra_member = score_root(&[entry(1, 10), entry(2, 20), entry(3, 0)]).unwrap();
        assert_ne!(base, changed_score);
        assert_ne!(base, extra_member);
    }

    #[test]
    fn duplicate_identities_are_rejected() {
        let result = score_root(&[entry(1, 10), entry(1, 20)]);
        assert_eq!(result, Err(CommitmentError::DuplicateIdentity));
    }

    #[test]
    fn odd_leaf_counts_are_handled() {
        for count in 1u8..=9 {
            let entries: Vec<ScoreEntry> = (1..=count).map(|i| entry(i, u128::from(i))).collect();
            let root = score_root(&entries).unwrap();
            assert_eq!(root, score_root(&entries).unwrap());
        }
    }
}
