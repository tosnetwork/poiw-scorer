//! Chain-ingestion boundary.
//!
//! The scorer never talks to the chain directly; it consumes a
//! [`ChainSource`]. The production source will walk finalized blocks over
//! JSON-RPC with reorg-safe checkpoints, following the block-walking
//! pattern of the node's contract indexer. This crate currently provides
//! the trait, the checkpoint types, and [`FixtureSource`] — a complete,
//! deterministic source backed by a JSON fixture, used for methodology
//! test vectors, cross-implementation comparison, and the CLI.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use poiw_types::{EpochId, IdentityId, ReliabilityInputs, SettledWorkUnit};

/// A reorg-safe scan checkpoint: the last block fully ingested and its
/// hash, so a source can detect a fork and rewind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub block_seqno: u64,
    pub block_hash: [u8; 32],
}

/// Everything the scorer needs for one epoch.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EpochData {
    pub units: Vec<SettledWorkUnit>,
    /// Trailing-window reliability inputs per identity, where history
    /// exists. Identities without history score at the neutral factor.
    pub reliability: Vec<(IdentityId, ReliabilityInputs)>,
}

impl EpochData {
    pub fn reliability_map(&self) -> BTreeMap<IdentityId, ReliabilityInputs> {
        self.reliability.iter().copied().collect()
    }
}

/// A source of settled-work records per epoch.
pub trait ChainSource {
    type Error: std::error::Error + Send + Sync + 'static;

    /// All settled work units and reliability inputs for `epoch`.
    fn epoch_data(&self, epoch: EpochId) -> Result<EpochData, Self::Error>;
}

/// Errors from the fixture source.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FixtureError {
    #[error("fixture has no data for epoch {0}")]
    UnknownEpoch(u64),
    #[error("fixture parse error: {0}")]
    Parse(String),
}

/// The JSON fixture format: epochs keyed by number.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Fixture {
    pub epochs: BTreeMap<u64, EpochData>,
}

/// A deterministic, in-memory [`ChainSource`] loaded from a JSON
/// fixture. This is a real source (used for methodology vectors and the
/// CLI), not a mock of the future RPC source.
#[derive(Debug, Clone, Default)]
pub struct FixtureSource {
    fixture: Fixture,
}

impl FixtureSource {
    pub fn new(fixture: Fixture) -> Self {
        Self { fixture }
    }

    pub fn from_json(json: &str) -> Result<Self, FixtureError> {
        let fixture: Fixture =
            serde_json::from_str(json).map_err(|e| FixtureError::Parse(e.to_string()))?;
        Ok(Self::new(fixture))
    }

    pub fn epochs(&self) -> impl Iterator<Item = EpochId> + '_ {
        self.fixture.epochs.keys().map(|k| EpochId(*k))
    }
}

impl ChainSource for FixtureSource {
    type Error = FixtureError;

    fn epoch_data(&self, epoch: EpochId) -> Result<EpochData, Self::Error> {
        self.fixture
            .epochs
            .get(&epoch.0)
            .cloned()
            .ok_or(FixtureError::UnknownEpoch(epoch.0))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use poiw_types::{CapabilityClass, EvidenceLevel};

    use super::*;

    #[test]
    fn fixture_round_trips_and_serves_epochs() {
        let mut fixture = Fixture::default();
        fixture.epochs.insert(
            7,
            EpochData {
                units: vec![SettledWorkUnit {
                    identity: IdentityId([1; 32]),
                    payer: IdentityId([2; 32]),
                    capability: CapabilityClass("embedding".into()),
                    rate_card_value: 100,
                    settled_price: 90,
                    evidence: EvidenceLevel::Observed,
                    is_challenge_task: false,
                    payer_related: false,
                }],
                reliability: vec![(
                    IdentityId([1; 32]),
                    ReliabilityInputs {
                        settlement_success_bps: 10_000,
                        dispute_loss_bps: 0,
                        sla_breach_bps: 0,
                    },
                )],
            },
        );
        let json = serde_json::to_string(&fixture).unwrap();
        let source = FixtureSource::from_json(&json).unwrap();
        let data = source.epoch_data(EpochId(7)).unwrap();
        assert_eq!(data.units.len(), 1);
        assert_eq!(data.reliability_map().len(), 1);
        assert_eq!(
            source.epoch_data(EpochId(8)),
            Err(FixtureError::UnknownEpoch(8))
        );
    }

    #[test]
    fn malformed_fixture_reports_parse_error() {
        assert!(matches!(
            FixtureSource::from_json("not json"),
            Err(FixtureError::Parse(_))
        ));
    }
}
