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

/// A disclosed control domain from the common-control registry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DomainId(pub String);

/// The effective control domain of an identity: either its disclosed
/// domain, or a singleton domain of itself when nothing is disclosed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DomainKey {
    Declared(DomainId),
    Solo(IdentityId),
}

/// Identity-to-domain assignments from the disclosed common-control
/// registry. Identities without an entry form singleton domains.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainMap(pub std::collections::BTreeMap<IdentityId, DomainId>);

impl DomainMap {
    pub fn from_pairs(pairs: impl IntoIterator<Item = (IdentityId, DomainId)>) -> Self {
        Self(pairs.into_iter().collect())
    }

    /// The effective domain of `identity`.
    pub fn domain_of(&self, identity: IdentityId) -> DomainKey {
        match self.0.get(&identity) {
            Some(domain) => DomainKey::Declared(domain.clone()),
            None => DomainKey::Solo(identity),
        }
    }

    /// Whether two identities share a control domain.
    pub fn same_domain(&self, a: IdentityId, b: IdentityId) -> bool {
        if a == b {
            return true;
        }
        self.domain_of(a) == self.domain_of(b)
    }
}

/// Lowercase hex helpers used for JSON-facing encodings of fixed-size
/// binary values (identities, hashes, keys, signatures).
pub mod hex {
    /// Encode bytes as lowercase hex.
    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().saturating_mul(2));
        for byte in bytes {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// Decode lowercase or uppercase hex into bytes. Returns `None` on
    /// any malformed input.
    pub fn decode(text: &str) -> Option<Vec<u8>> {
        if !text.len().is_multiple_of(2) {
            return None;
        }
        let digits: Vec<u32> = text
            .chars()
            .map(|c| c.to_digit(16))
            .collect::<Option<_>>()?;
        let mut out = Vec::with_capacity(digits.len() / 2);
        for pair in digits.chunks_exact(2) {
            if let [high, low] = pair {
                let value = high.checked_mul(16)?.checked_add(*low)?;
                out.push(u8::try_from(value).ok()?);
            }
        }
        Some(out)
    }

    /// Decode hex into an exact-size array.
    pub fn decode_array<const N: usize>(text: &str) -> Option<[u8; N]> {
        let bytes = decode(text)?;
        bytes.try_into().ok()
    }
}

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

/// The v0 capability-class vocabulary: the closed class/unit registry
/// published by the protocol (normative source: the PoIW work-attribution
/// specification). A measurement that cannot be normalized under a
/// specific class is reported under `default`, never approximated.
pub mod vocabulary {
    /// The vocabulary revision this module implements.
    pub const RATE_CARD_VERSION: &str = "v0";

    /// The interim fallback class carrying the settled amount in
    /// nanotos as its work units.
    pub const DEFAULT_CLASS: &str = "default";

    /// The closed v0 registry, ordered by class name.
    pub const CLASS_UNITS: &[(&str, &str)] = &[
        ("default", "settled-nanotos"),
        ("embedding", "call"),
        ("image-generation", "image"),
        ("speech-recognition", "audio-second"),
        ("speech-synthesis", "audio-second"),
        ("storage-byte-hour", "byte-hour"),
        ("text-generation", "kilo-output-tokens"),
        ("verification-replication", "replicated-call"),
    ];

    /// The normalized billing unit of a v0 class.
    pub fn unit_for(class: &str) -> Option<&'static str> {
        CLASS_UNITS
            .iter()
            .find(|(name, _)| *name == class)
            .map(|(_, unit)| *unit)
    }
}

/// A protocol-published rate card: nanotos per normalized unit, per
/// capability class. The `default` class needs no entry — its work units
/// already are the settled amount (an implicit price of 1).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RateCard {
    pub version: String,
    pub prices: std::collections::BTreeMap<String, Nanotos>,
}

/// PoIW work attribution as carried inside a signed settlement receipt
/// (the wire mirror of the protocol's `PoiwWorkAttribution` message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkAttribution {
    pub capability_class: String,
    pub unit: String,
    pub work_units: u64,
    pub rate_card_version: String,
    pub evidence_level: EvidenceLevel,
    /// Optional sha256 commitments (64 hex chars) to the earner/payer
    /// chain accounts. Not consumed by valuation; validated for shape
    /// when present.
    #[serde(default)]
    pub earner_identity_commitment: Option<String>,
    #[serde(default)]
    pub payer_identity_commitment: Option<String>,
    #[serde(default)]
    pub challenge_task: bool,
}

/// Attribution consumption errors. Every variant is a hard scoring
/// error: a malformed or unpriceable attribution is rejected, never
/// repaired or silently skipped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttributionError {
    #[error("unknown rate card version {0:?} (this scorer implements {1:?})")]
    UnknownVersion(String, &'static str),
    #[error("unknown capability class {0:?}")]
    UnknownClass(String),
    #[error("class {class:?} requires unit {expected:?}, got {got:?}")]
    UnitMismatch {
        class: String,
        expected: &'static str,
        got: String,
    },
    #[error("rate card {version:?} has no price for class {class:?}")]
    Unpriced { version: String, class: String },
    #[error("valuation overflow for class {0:?}")]
    Overflow(String),
    #[error("malformed {0} identity commitment")]
    BadCommitment(&'static str),
}

fn validate_commitment(
    label: &'static str,
    commitment: &Option<String>,
) -> Result<(), AttributionError> {
    match commitment {
        None => Ok(()),
        Some(text) => match hex::decode_array::<32>(text) {
            Some(_) => Ok(()),
            None => Err(AttributionError::BadCommitment(label)),
        },
    }
}

impl WorkAttribution {
    /// Convert a receipt attribution into a scoreable settled work unit
    /// under a rate card: `rate_card_value = work_units x price per
    /// unit` for priced classes, and the settled amount itself for the
    /// `default` class. The attribution's rate-card version must match
    /// both this scorer's vocabulary and the supplied card.
    pub fn to_settled_work_unit(
        &self,
        earner: IdentityId,
        payer: IdentityId,
        settled_price: Nanotos,
        rate_card: &RateCard,
    ) -> Result<SettledWorkUnit, AttributionError> {
        if self.rate_card_version != vocabulary::RATE_CARD_VERSION
            || rate_card.version != vocabulary::RATE_CARD_VERSION
        {
            return Err(AttributionError::UnknownVersion(
                self.rate_card_version.clone(),
                vocabulary::RATE_CARD_VERSION,
            ));
        }
        let expected_unit = vocabulary::unit_for(&self.capability_class)
            .ok_or_else(|| AttributionError::UnknownClass(self.capability_class.clone()))?;
        if self.unit != expected_unit {
            return Err(AttributionError::UnitMismatch {
                class: self.capability_class.clone(),
                expected: expected_unit,
                got: self.unit.clone(),
            });
        }
        validate_commitment("earner", &self.earner_identity_commitment)?;
        validate_commitment("payer", &self.payer_identity_commitment)?;
        let rate_card_value = if self.capability_class == vocabulary::DEFAULT_CLASS {
            settled_price
        } else {
            let price = rate_card
                .prices
                .get(&self.capability_class)
                .ok_or_else(|| AttributionError::Unpriced {
                    version: rate_card.version.clone(),
                    class: self.capability_class.clone(),
                })?;
            self.work_units
                .checked_mul(*price)
                .ok_or_else(|| AttributionError::Overflow(self.capability_class.clone()))?
        };
        Ok(SettledWorkUnit {
            identity: earner,
            payer,
            capability: CapabilityClass(self.capability_class.clone()),
            rate_card_value,
            settled_price,
            evidence: self.evidence_level,
            is_challenge_task: self.challenge_task,
            payer_related: false,
        })
    }
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
    fn domain_map_defaults_to_singleton_domains() {
        let map = DomainMap::from_pairs([
            (IdentityId([1; 32]), DomainId("acme".into())),
            (IdentityId([2; 32]), DomainId("acme".into())),
        ]);
        assert!(map.same_domain(IdentityId([1; 32]), IdentityId([2; 32])));
        assert!(!map.same_domain(IdentityId([1; 32]), IdentityId([3; 32])));
        assert!(!map.same_domain(IdentityId([3; 32]), IdentityId([4; 32])));
        assert!(map.same_domain(IdentityId([3; 32]), IdentityId([3; 32])));
    }

    #[test]
    fn hex_round_trips_and_rejects_malformed() {
        let bytes = [0u8, 15, 16, 255];
        let text = hex::encode(&bytes);
        assert_eq!(text, "000f10ff");
        assert_eq!(hex::decode(&text).unwrap(), bytes.to_vec());
        assert_eq!(hex::decode_array::<4>(&text).unwrap(), bytes);
        assert!(hex::decode("abc").is_none());
        assert!(hex::decode("zz").is_none());
        assert!(hex::decode_array::<3>(&text).is_none());
    }

    fn attribution(class: &str, unit: &str, work_units: u64) -> WorkAttribution {
        WorkAttribution {
            capability_class: class.into(),
            unit: unit.into(),
            work_units,
            rate_card_version: vocabulary::RATE_CARD_VERSION.into(),
            evidence_level: EvidenceLevel::Observed,
            earner_identity_commitment: None,
            payer_identity_commitment: None,
            challenge_task: false,
        }
    }

    fn card() -> RateCard {
        RateCard {
            version: vocabulary::RATE_CARD_VERSION.into(),
            prices: [("text-generation".to_owned(), 2_000_000u64)]
                .into_iter()
                .collect(),
        }
    }

    #[test]
    fn attribution_values_measured_work_by_the_rate_card() {
        let unit = attribution("text-generation", "kilo-output-tokens", 3)
            .to_settled_work_unit(IdentityId([1; 32]), IdentityId([2; 32]), 9_000_000, &card())
            .unwrap();
        // 3 kilo-tokens x 2_000_000 nanotos = 6_000_000, independent of
        // the settled price (which still caps the score downstream).
        assert_eq!(unit.rate_card_value, 6_000_000);
        assert_eq!(unit.settled_price, 9_000_000);
        assert_eq!(unit.capability.0, "text-generation");
        assert_eq!(unit.evidence, EvidenceLevel::Observed);
        assert!(!unit.is_challenge_task);
    }

    #[test]
    fn default_class_carries_the_settled_amount_unpriced() {
        let unit = attribution("default", "settled-nanotos", 7_000)
            .to_settled_work_unit(IdentityId([1; 32]), IdentityId([2; 32]), 7_000, &card())
            .unwrap();
        assert_eq!(unit.rate_card_value, 7_000);
        assert_eq!(unit.settled_price, 7_000);
    }

    #[test]
    fn attribution_conversion_rejects_malformed_and_unpriceable() {
        let earner = IdentityId([1; 32]);
        let payer = IdentityId([2; 32]);
        let mut wrong_version = attribution("text-generation", "kilo-output-tokens", 3);
        wrong_version.rate_card_version = "v99".into();
        assert!(matches!(
            wrong_version.to_settled_work_unit(earner, payer, 1, &card()),
            Err(AttributionError::UnknownVersion(_, _))
        ));
        assert!(matches!(
            attribution("rumor-mill", "call", 1).to_settled_work_unit(earner, payer, 1, &card()),
            Err(AttributionError::UnknownClass(_))
        ));
        assert!(matches!(
            attribution("embedding", "token", 1).to_settled_work_unit(earner, payer, 1, &card()),
            Err(AttributionError::UnitMismatch { .. })
        ));
        // A specific class absent from the rate card is unpriceable, not
        // silently zero.
        assert!(matches!(
            attribution("embedding", "call", 1).to_settled_work_unit(earner, payer, 1, &card()),
            Err(AttributionError::Unpriced { .. })
        ));
        assert!(matches!(
            attribution("text-generation", "kilo-output-tokens", u64::MAX).to_settled_work_unit(
                earner,
                payer,
                1,
                &card()
            ),
            Err(AttributionError::Overflow(_))
        ));
        let mut bad_commitment = attribution("text-generation", "kilo-output-tokens", 1);
        bad_commitment.earner_identity_commitment = Some("zz".into());
        assert!(matches!(
            bad_commitment.to_settled_work_unit(earner, payer, 1, &card()),
            Err(AttributionError::BadCommitment("earner"))
        ));
        let mut good_commitment = attribution("text-generation", "kilo-output-tokens", 1);
        good_commitment.payer_identity_commitment = Some(hex::encode(&[7u8; 32]));
        assert!(good_commitment
            .to_settled_work_unit(earner, payer, 1, &card())
            .is_ok());
    }

    #[test]
    fn vocabulary_lookup_matches_the_registry() {
        assert_eq!(
            vocabulary::unit_for("text-generation"),
            Some("kilo-output-tokens")
        );
        assert_eq!(vocabulary::unit_for("default"), Some("settled-nanotos"));
        assert_eq!(vocabulary::unit_for("rumor-mill"), None);
        for (class, unit) in vocabulary::CLASS_UNITS {
            assert_eq!(vocabulary::unit_for(class), Some(*unit));
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
