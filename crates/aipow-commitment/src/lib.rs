//! Deterministic score-root construction and commitment submission.
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
//! - the empty epoch root is `sha256(0x02 || "aipow-empty-v0")`.
//!
//! The [`CommitmentEnvelope`] is the canonical signing payload for a
//! committed epoch: its byte encoding and digest are fixed here so that
//! shadow-scoring publication (phase A) and on-chain submission (phase
//! C, once the distributor contract exists) sign the very same bytes.
//! Submission itself is behind the [`Submitter`] trait; the
//! [`FileSubmitter`] publishes signed envelopes as JSON files for the
//! shadow-scoring phase.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use aipow_types::{hex, IdentityId};

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
const EMPTY_DOMAIN: &[u8] = b"aipow-empty-v0";

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

const ENVELOPE_DOMAIN: &[u8] = b"aipow-commit-v0";

/// The canonical epoch-commitment payload. Field order and byte
/// encoding are normative: every implementation signs and verifies the
/// same digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentEnvelope {
    pub epoch: u64,
    pub methodology_version: String,
    /// Epoch score root over the merged per-identity totals (organic
    /// plus challenge score per identity), as produced by
    /// [`score_root`].
    pub score_root_hex: String,
    pub entry_count: u64,
    pub total_score: u128,
    pub organic_settled_value: u128,
}

/// Envelope handling errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("score_root_hex must be 64 hex characters")]
    BadRoot,
    #[error("bad key or signature encoding")]
    BadKeyMaterial,
    #[error("signature verification failed")]
    BadSignature,
    #[error("submission failed: {0}")]
    Submit(String),
}

impl CommitmentEnvelope {
    pub fn root(&self) -> Result<[u8; 32], EnvelopeError> {
        hex::decode_array::<32>(&self.score_root_hex).ok_or(EnvelopeError::BadRoot)
    }

    /// The canonical byte encoding:
    /// `domain || epoch_be(8) || len_be(4) || methodology_version_utf8 ||
    ///  root(32) || entry_count_be(8) || total_score_be(16) ||
    ///  organic_settled_value_be(16)`.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        let version = self.methodology_version.as_bytes();
        let version_len =
            u32::try_from(version.len()).map_err(|_| EnvelopeError::BadKeyMaterial)?;
        let mut out = Vec::new();
        out.extend_from_slice(ENVELOPE_DOMAIN);
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&version_len.to_be_bytes());
        out.extend_from_slice(version);
        out.extend_from_slice(&self.root()?);
        out.extend_from_slice(&self.entry_count.to_be_bytes());
        out.extend_from_slice(&self.total_score.to_be_bytes());
        out.extend_from_slice(&self.organic_settled_value.to_be_bytes());
        Ok(out)
    }

    /// The digest that gets signed.
    pub fn digest(&self) -> Result<[u8; 32], EnvelopeError> {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical_bytes()?);
        Ok(hasher.finalize().into())
    }

    /// Sign the envelope with an ed25519 key.
    pub fn sign(&self, signing_key: &SigningKey) -> Result<SignedCommitment, EnvelopeError> {
        let digest = self.digest()?;
        let signature = signing_key.sign(&digest);
        Ok(SignedCommitment {
            envelope: self.clone(),
            public_key_hex: hex::encode(signing_key.verifying_key().as_bytes()),
            signature_hex: hex::encode(&signature.to_bytes()),
        })
    }
}

/// A signed epoch commitment, ready for publication or submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedCommitment {
    pub envelope: CommitmentEnvelope,
    pub public_key_hex: String,
    pub signature_hex: String,
}

impl SignedCommitment {
    /// Verify the signature against the envelope's canonical digest.
    pub fn verify(&self) -> Result<(), EnvelopeError> {
        let key_bytes =
            hex::decode_array::<32>(&self.public_key_hex).ok_or(EnvelopeError::BadKeyMaterial)?;
        let signature_bytes =
            hex::decode_array::<64>(&self.signature_hex).ok_or(EnvelopeError::BadKeyMaterial)?;
        let key =
            VerifyingKey::from_bytes(&key_bytes).map_err(|_| EnvelopeError::BadKeyMaterial)?;
        let signature = Signature::from_bytes(&signature_bytes);
        let digest = self.envelope.digest()?;
        key.verify(&digest, &signature)
            .map_err(|_| EnvelopeError::BadSignature)
    }
}

/// Where signed commitments go. On-chain submission through the AIPoW
/// distributor contract implements this trait once that contract exists;
/// the shadow-scoring phase publishes files.
pub trait Submitter {
    fn submit(&self, commitment: &SignedCommitment) -> Result<(), EnvelopeError>;
}

/// Publishes signed commitments as pretty-printed JSON files named
/// `aipow-commitment-epoch-<n>.json` in a directory.
#[derive(Debug, Clone)]
pub struct FileSubmitter {
    pub directory: std::path::PathBuf,
}

impl Submitter for FileSubmitter {
    fn submit(&self, commitment: &SignedCommitment) -> Result<(), EnvelopeError> {
        commitment.verify()?;
        let name = format!("aipow-commitment-epoch-{}.json", commitment.envelope.epoch);
        let path = self.directory.join(name);
        let json = serde_json::to_string_pretty(commitment)
            .map_err(|e| EnvelopeError::Submit(e.to_string()))?;
        std::fs::write(&path, json).map_err(|e| EnvelopeError::Submit(e.to_string()))?;
        Ok(())
    }
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

    fn sample_envelope() -> CommitmentEnvelope {
        let root = score_root(&[entry(1, 10), entry(2, 20)]).unwrap();
        CommitmentEnvelope {
            epoch: 42,
            methodology_version: "v0".into(),
            score_root_hex: aipow_types::hex::encode(&root),
            entry_count: 2,
            total_score: 30,
            organic_settled_value: 1_000,
        }
    }

    #[test]
    fn envelope_digest_is_deterministic_and_field_sensitive() {
        let envelope = sample_envelope();
        assert_eq!(envelope.digest().unwrap(), envelope.digest().unwrap());
        let mut changed = envelope.clone();
        changed.epoch = 43;
        assert_ne!(envelope.digest().unwrap(), changed.digest().unwrap());
        let mut bad = envelope;
        bad.score_root_hex = "zz".into();
        assert_eq!(bad.digest(), Err(EnvelopeError::BadRoot));
    }

    #[test]
    fn sign_verify_round_trip_and_tamper_detection() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let signed = sample_envelope().sign(&signing_key).unwrap();
        signed.verify().unwrap();

        let mut tampered = signed.clone();
        tampered.envelope.total_score = 31;
        assert_eq!(tampered.verify(), Err(EnvelopeError::BadSignature));

        let mut bad_key = signed.clone();
        bad_key.public_key_hex = "00".into();
        assert_eq!(bad_key.verify(), Err(EnvelopeError::BadKeyMaterial));

        let json = serde_json::to_string(&signed).unwrap();
        let back: SignedCommitment = serde_json::from_str(&json).unwrap();
        back.verify().unwrap();
    }

    #[test]
    fn file_submitter_writes_verified_commitments() {
        let dir = std::env::temp_dir().join(format!(
            "aipow-commit-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let submitter = FileSubmitter {
            directory: dir.clone(),
        };
        let signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let signed = sample_envelope().sign(&signing_key).unwrap();
        submitter.submit(&signed).unwrap();
        let path = dir.join("aipow-commitment-epoch-42.json");
        let loaded: SignedCommitment =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        loaded.verify().unwrap();

        // A tampered commitment is refused before it is written.
        let mut tampered = signed;
        tampered.envelope.epoch = 43;
        assert!(submitter.submit(&tampered).is_err());
        assert!(!dir.join("aipow-commitment-epoch-43.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
