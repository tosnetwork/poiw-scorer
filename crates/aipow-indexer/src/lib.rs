//! Chain-ingestion boundary.
//!
//! The scorer never talks to the chain directly; it consumes a
//! [`ChainSource`]. Two sources exist:
//!
//! - [`FixtureSource`] — a complete, deterministic source backed by a
//!   JSON fixture, used for methodology test vectors,
//!   cross-implementation comparison, and the CLI;
//! - [`rpc::RpcChainSource`] — walks finalized blocks over a
//!   [`rpc::ChainRpc`] with reorg-safe checkpoints, following the
//!   block-walking pattern of the node's contract indexer, with a
//!   JSON-RPC adapter in [`rpc::JsonRpcChainRpc`];
//! - [`tosctld::TosctldSource`] — consumes the phase-A
//!   `GET /aipow/settled-work` data plane served by `tosctld`, applying
//!   the published interim mapping.

pub mod rpc;
pub mod tosctld;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use aipow_types::{EpochId, IdentityId, ReliabilityInputs, SettledWorkUnit};

/// A reorg-safe scan checkpoint: the last block fully ingested and its
/// hash, so a source can detect a fork and rewind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub block_seqno: u64,
    pub block_hash: [u8; 32],
}

/// One settlement carrying a receipt-borne AIPoW work attribution, not
/// yet valued: a rate card turns it into a [`SettledWorkUnit`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributedSettlement {
    pub earner: IdentityId,
    pub payer: IdentityId,
    pub settled_price: aipow_types::Nanotos,
    pub attribution: aipow_types::WorkAttribution,
}

/// Everything the scorer needs for one epoch.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EpochData {
    pub units: Vec<SettledWorkUnit>,
    /// Settlements whose receipts carry AIPoW work attributions; valued
    /// against a rate card by [`EpochData::valued_units`]. Sources that
    /// serve attributions populate this instead of pre-valuing `units`.
    #[serde(default)]
    pub attributed: Vec<AttributedSettlement>,
    /// Trailing-window reliability inputs per identity, where history
    /// exists. Identities without history score at the neutral factor.
    pub reliability: Vec<(IdentityId, ReliabilityInputs)>,
}

impl EpochData {
    pub fn reliability_map(&self) -> BTreeMap<IdentityId, ReliabilityInputs> {
        self.reliability.iter().copied().collect()
    }

    /// All scoreable units: the pre-valued `units` plus every attributed
    /// settlement valued under `rate_card`. A malformed or unpriceable
    /// attribution is a hard error, never a silent skip.
    pub fn valued_units(
        &self,
        rate_card: &aipow_types::RateCard,
    ) -> Result<Vec<SettledWorkUnit>, aipow_types::AttributionError> {
        let mut units = self.units.clone();
        for settlement in &self.attributed {
            units.push(settlement.attribution.to_settled_work_unit(
                settlement.earner,
                settlement.payer,
                settlement.settled_price,
                rate_card,
            )?);
        }
        Ok(units)
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

/// The JSON fixture format: epochs keyed by number, plus the disclosed
/// control-domain assignments in force.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Fixture {
    pub epochs: BTreeMap<u64, EpochData>,
    #[serde(default)]
    pub domains: Vec<(IdentityId, aipow_types::DomainId)>,
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

    /// The disclosed control-domain assignments carried by the fixture.
    pub fn domain_map(&self) -> aipow_types::DomainMap {
        aipow_types::DomainMap::from_pairs(self.fixture.domains.iter().cloned())
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

    use aipow_types::{CapabilityClass, EvidenceLevel};

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
                attributed: vec![],
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

    #[test]
    fn attributed_settlements_value_under_a_rate_card() {
        let attribution = aipow_types::WorkAttribution {
            capability_class: "text-generation".into(),
            unit: "kilo-output-tokens".into(),
            work_units: 4,
            rate_card_version: "v0".into(),
            evidence_level: EvidenceLevel::Observed,
            earner_identity_commitment: None,
            payer_identity_commitment: None,
            challenge_task: false,
        };
        let data = EpochData {
            units: vec![],
            attributed: vec![AttributedSettlement {
                earner: IdentityId([5; 32]),
                payer: IdentityId([6; 32]),
                settled_price: 1_000,
                attribution: attribution.clone(),
            }],
            reliability: vec![],
        };
        let card = aipow_types::RateCard {
            version: "v0".into(),
            prices: [("text-generation".to_owned(), 250u64)]
                .into_iter()
                .collect(),
        };
        let units = data.valued_units(&card).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].rate_card_value, 1_000); // 4 x 250
        assert_eq!(units[0].settled_price, 1_000);

        // The same data under a card without the class is a hard error.
        let empty_card = aipow_types::RateCard {
            version: "v0".into(),
            prices: Default::default(),
        };
        assert!(data.valued_units(&empty_card).is_err());

        // Attributed rows survive a fixture JSON round trip.
        let mut fixture = Fixture::default();
        fixture.epochs.insert(3, data.clone());
        let json = serde_json::to_string(&fixture).unwrap();
        let source = FixtureSource::from_json(&json).unwrap();
        assert_eq!(source.epoch_data(EpochId(3)).unwrap(), data);
    }
}
