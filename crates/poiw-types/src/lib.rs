//! Core domain types shared by every PoIW scorer component.
//!
//! All monetary amounts are integer nanotos (1 TOS = 1,000,000,000 nanotos).
//! All multipliers are integer basis points or integer percent. No floating
//! point appears anywhere in the scoring pipeline: every implementation of
//! the published methodology must reproduce score roots byte for byte, and
//! integer arithmetic is the only way to guarantee that across languages.

use serde::{Deserialize, Serialize};

/// Integer nanotos. 1 TOS = 1_000_000_000 nanotos.
pub type Nanotos = u64;

/// Basis points: 10_000 bps = 1.0.
pub type Bps = u32;

/// One basis-point unit as a divisor.
pub const BPS_DENOMINATOR: u128 = 10_000;

/// A scoring identity: the 32-byte account identifier of a bonded
/// Capability Registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IdentityId(pub [u8; 32]);

/// A PoIW epoch number (aligned with the 65,536-second validation epoch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EpochId(pub u64);

/// A capability class from the published rate-card vocabulary
/// (for example `text-generation`, `embedding`, `storage-byte-hour`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CapabilityClass(pub String);

/// The evidence ladder. The variant order is the trust order; `Declared`
/// deliberately multiplies every score to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EvidenceLevel {
    Declared,
    Observed,
    Benchmarked,
    Audited,
    Attested,
    Replicated,
}

impl EvidenceLevel {
    /// Score multiplier in integer percent (100 = 1.0x).
    pub fn multiplier_percent(self) -> u64 {
        match self {
            EvidenceLevel::Declared => 0,
            EvidenceLevel::Observed => 100,
            EvidenceLevel::Benchmarked => 120,
            EvidenceLevel::Audited => 150,
            EvidenceLevel::Attested => 170,
            EvidenceLevel::Replicated => 200,
        }
    }
}

/// One settled, on-chain-observable unit of work, as extracted from a Task
/// Escrow settlement or Service Actor receipt by the indexer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettledWorkUnit {
    /// The earning identity.
    pub identity: IdentityId,
    /// The paying counterparty identity.
    pub payer: IdentityId,
    /// Capability class under the published rate-card vocabulary.
    pub capability: CapabilityClass,
    /// Protocol rate-card valuation of the measured work, in nanotos.
    pub rate_card_value: Nanotos,
    /// The amount an arm's-length consumer actually escrowed and settled.
    pub settled_price: Nanotos,
    /// Evidence level attached to this settlement.
    pub evidence: EvidenceLevel,
    /// True when the work unit is a protocol-issued challenge task.
    /// Challenge work scores normally but never counts toward the
    /// organic settled value that drives emission.
    pub is_challenge_task: bool,
    /// True when the payer is inside the earner's disclosed control
    /// domain. Related-payer settlements are excluded from scoring and
    /// from organic settled value.
    pub payer_related: bool,
}

/// Trailing-window reliability inputs for one identity, all in basis
/// points of the relevant totals. Absence of history means a neutral
/// factor, never a bonus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReliabilityInputs {
    /// Share of accepted tasks that settled successfully.
    pub settlement_success_bps: Bps,
    /// Share of disputes lost.
    pub dispute_loss_bps: Bps,
    /// Share of accepted tasks with SLA breaches.
    pub sla_breach_bps: Bps,
}

/// Errors shared across scorer components.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoiwError {
    #[error("integer overflow in score arithmetic")]
    Overflow,
    #[error("invalid parameter: {0}")]
    InvalidParameter(&'static str),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn declared_evidence_multiplies_to_zero() {
        assert_eq!(EvidenceLevel::Declared.multiplier_percent(), 0);
    }

    #[test]
    fn evidence_ladder_is_strictly_increasing() {
        let ladder = [
            EvidenceLevel::Declared,
            EvidenceLevel::Observed,
            EvidenceLevel::Benchmarked,
            EvidenceLevel::Audited,
            EvidenceLevel::Attested,
            EvidenceLevel::Replicated,
        ];
        for pair in ladder.windows(2) {
            assert!(pair[0].multiplier_percent() < pair[1].multiplier_percent());
        }
    }

    #[test]
    fn work_unit_round_trips_through_json() {
        let unit = SettledWorkUnit {
            identity: IdentityId([1; 32]),
            payer: IdentityId([2; 32]),
            capability: CapabilityClass("embedding".into()),
            rate_card_value: 5_000,
            settled_price: 4_000,
            evidence: EvidenceLevel::Observed,
            is_challenge_task: false,
            payer_related: false,
        };
        let json = serde_json::to_string(&unit).unwrap();
        let back: SettledWorkUnit = serde_json::from_str(&json).unwrap();
        assert_eq!(unit, back);
    }
}
