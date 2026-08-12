//! Deterministic PoIW score computation.
//!
//! Everything here is pure integer arithmetic over the settled-work
//! records produced by the indexer and classified by `poiw-classifier`.
//! The functions implement the published methodology; the methodology
//! document, not this code, is the normative artifact.
//!
//! Scoring and payout are deliberately separate stages: `score_epoch`
//! produces evidence- and reliability-weighted scores, and
//! `allocate_pool` turns scores into payouts under the per-control-domain
//! cap. Pool value above the cap is simply never created, consistent with
//! the design rule that un-earned residue is not minted and not rolled
//! over.

use std::collections::BTreeMap;

use poiw_classifier::{is_score_eligible, ClassifierParams};
use poiw_types::{Bps, IdentityId, PoiwError, ReliabilityInputs, SettledWorkUnit, BPS_DENOMINATOR};

/// Scoring parameters (methodology v0 draft values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreParams {
    /// Hard cap on any single identity's share of one epoch pool.
    /// Pro-rata payout above this share is not created.
    pub payout_cap_share_bps: Bps,
}

impl Default for ScoreParams {
    fn default() -> Self {
        Self {
            payout_cap_share_bps: 500,
        }
    }
}

impl ScoreParams {
    fn validate(&self) -> Result<(), PoiwError> {
        if self.payout_cap_share_bps == 0 || self.payout_cap_share_bps > 10_000 {
            return Err(PoiwError::InvalidParameter(
                "payout_cap_share_bps must be within 1..=10_000",
            ));
        }
        Ok(())
    }
}

/// Multiply an amount by a basis-point factor, flooring.
pub fn apply_bps(value: u128, bps: Bps) -> Result<u128, PoiwError> {
    value
        .checked_mul(u128::from(bps))
        .ok_or(PoiwError::Overflow)?
        .checked_div(BPS_DENOMINATOR)
        .ok_or(PoiwError::Overflow)
}

/// Score of one settled work unit: the rate-card valuation capped by the
/// actually settled price, times the evidence multiplier. `Declared`
/// evidence yields exactly zero.
pub fn work_unit_score(unit: &SettledWorkUnit) -> Result<u128, PoiwError> {
    let base = u128::from(unit.rate_card_value.min(unit.settled_price));
    base.checked_mul(u128::from(unit.evidence.multiplier_percent()))
        .ok_or(PoiwError::Overflow)?
        .checked_div(100)
        .ok_or(PoiwError::Overflow)
}

/// Reliability factor in basis points, clamped to [5_000, 11_000].
///
/// No history means a neutral 10_000: a new identity starts at 1.0 and
/// seniority alone is never rewarded. A window with zero penalties earns
/// the 11_000 ceiling; penalties subtract from the 10_000 baseline down
/// to the 5_000 floor.
pub fn reliability_factor_bps(history: Option<ReliabilityInputs>) -> Result<Bps, PoiwError> {
    let Some(inputs) = history else {
        return Ok(10_000);
    };
    for value in [
        inputs.settlement_success_bps,
        inputs.dispute_loss_bps,
        inputs.sla_breach_bps,
    ] {
        if value > 10_000 {
            return Err(PoiwError::InvalidParameter(
                "reliability inputs must be within 10_000 bps",
            ));
        }
    }
    let failure = 10_000u32
        .checked_sub(inputs.settlement_success_bps)
        .ok_or(PoiwError::Overflow)?;
    let penalty = failure
        .checked_add(inputs.dispute_loss_bps)
        .ok_or(PoiwError::Overflow)?
        .checked_add(inputs.sla_breach_bps)
        .ok_or(PoiwError::Overflow)?;
    if penalty == 0 {
        return Ok(11_000);
    }
    Ok(10_000u32.saturating_sub(penalty).max(5_000))
}

/// Demand-coupled epoch pool: the smaller of the schedule ceiling and
/// `k` (integer percent) times the trailing organic settled value.
pub fn epoch_pool(
    schedule_cap: u128,
    k_percent: u32,
    organic_settled_value: u128,
) -> Result<u128, PoiwError> {
    let coupled = organic_settled_value
        .checked_mul(u128::from(k_percent))
        .ok_or(PoiwError::Overflow)?
        .checked_div(100)
        .ok_or(PoiwError::Overflow)?;
    Ok(schedule_cap.min(coupled))
}

/// A maturation schedule: the immediate part plus the per-epoch stream.
/// The parts always sum exactly to the input total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaturationSchedule {
    pub immediate: u128,
    pub stream: Vec<u128>,
}

/// Split a reward into the immediate part and a deterministic stream:
/// `immediate_bps` now, the remainder spread over `stream_epochs`, with
/// division remainders assigned to the earliest stream epochs.
pub fn maturation_schedule(
    total: u128,
    immediate_bps: Bps,
    stream_epochs: u32,
) -> Result<MaturationSchedule, PoiwError> {
    if immediate_bps > 10_000 {
        return Err(PoiwError::InvalidParameter(
            "immediate_bps must not exceed 10_000",
        ));
    }
    if stream_epochs == 0 {
        return Err(PoiwError::InvalidParameter(
            "stream_epochs must be at least 1",
        ));
    }
    let immediate = apply_bps(total, immediate_bps)?;
    let streamed = total.checked_sub(immediate).ok_or(PoiwError::Overflow)?;
    let per = streamed
        .checked_div(u128::from(stream_epochs))
        .ok_or(PoiwError::Overflow)?;
    let remainder = streamed
        .checked_rem(u128::from(stream_epochs))
        .ok_or(PoiwError::Overflow)?;
    let mut stream = Vec::with_capacity(stream_epochs as usize);
    for index in 0..u128::from(stream_epochs) {
        let extra = if index < remainder { 1 } else { 0 };
        stream.push(per.checked_add(extra).ok_or(PoiwError::Overflow)?);
    }
    Ok(MaturationSchedule { immediate, stream })
}

/// One identity's final epoch score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityScore {
    pub identity: IdentityId,
    pub score: u128,
}

/// The complete deterministic result of scoring one epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochScores {
    /// Final per-identity scores, ordered by identity.
    pub scores: Vec<IdentityScore>,
    /// Organic settled value for the emission formula.
    pub organic_settled_value: u128,
}

/// Score one epoch of settled work.
///
/// Pipeline: filter score-eligible units, sum evidence-weighted unit
/// scores per identity, apply each identity's reliability factor.
/// Deterministic by construction: `BTreeMap` ordering and integer
/// arithmetic only.
pub fn score_epoch(
    units: &[SettledWorkUnit],
    reliability: &BTreeMap<IdentityId, ReliabilityInputs>,
    classifier_params: &ClassifierParams,
) -> Result<EpochScores, PoiwError> {
    let mut raw: BTreeMap<IdentityId, u128> = BTreeMap::new();
    for unit in units.iter().filter(|u| is_score_eligible(u)) {
        let unit_score = work_unit_score(unit)?;
        let entry = raw.entry(unit.identity).or_insert(0);
        *entry = entry.checked_add(unit_score).ok_or(PoiwError::Overflow)?;
    }

    let mut scores = Vec::with_capacity(raw.len());
    for (identity, raw_score) in &raw {
        let factor = reliability_factor_bps(reliability.get(identity).copied())?;
        let adjusted = apply_bps(*raw_score, factor)?;
        scores.push(IdentityScore {
            identity: *identity,
            score: adjusted,
        });
    }

    let organic = poiw_classifier::organic_settled_value(units, classifier_params)?;
    Ok(EpochScores {
        scores,
        organic_settled_value: organic,
    })
}

/// One identity's payout from an epoch pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityPayout {
    pub identity: IdentityId,
    pub amount: u128,
}

/// Allocate an epoch pool pro rata by score, capping every identity at
/// `payout_cap_share_bps` of the pool. Capped-off value is **not
/// redistributed and not created** — the same no-residue rule as the
/// demand-coupled pool itself. A zero score total allocates nothing.
pub fn allocate_pool(
    pool: u128,
    scores: &[IdentityScore],
    params: &ScoreParams,
) -> Result<Vec<IdentityPayout>, PoiwError> {
    params.validate()?;
    let total: u128 = scores
        .iter()
        .try_fold(0u128, |acc, s| acc.checked_add(s.score))
        .ok_or(PoiwError::Overflow)?;
    if total == 0 || pool == 0 {
        return Ok(scores
            .iter()
            .map(|s| IdentityPayout {
                identity: s.identity,
                amount: 0,
            })
            .collect());
    }
    let cap_amount = apply_bps(pool, params.payout_cap_share_bps)?;
    let mut payouts = Vec::with_capacity(scores.len());
    for entry in scores {
        let pro_rata = pool
            .checked_mul(entry.score)
            .ok_or(PoiwError::Overflow)?
            .checked_div(total)
            .ok_or(PoiwError::Overflow)?;
        payouts.push(IdentityPayout {
            identity: entry.identity,
            amount: pro_rata.min(cap_amount),
        });
    }
    Ok(payouts)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use poiw_types::{CapabilityClass, EvidenceLevel};

    use super::*;

    fn unit(earner: u8, payer: u8, price: u64, evidence: EvidenceLevel) -> SettledWorkUnit {
        SettledWorkUnit {
            identity: IdentityId([earner; 32]),
            payer: IdentityId([payer; 32]),
            capability: CapabilityClass("embedding".into()),
            rate_card_value: price,
            settled_price: price,
            evidence,
            is_challenge_task: false,
            payer_related: false,
        }
    }

    #[test]
    fn declared_work_scores_zero() {
        let declared = unit(1, 2, 10_000, EvidenceLevel::Declared);
        assert_eq!(work_unit_score(&declared).unwrap(), 0);
    }

    #[test]
    fn settled_price_caps_the_rate_card_value() {
        let mut inflated = unit(1, 2, 0, EvidenceLevel::Observed);
        inflated.rate_card_value = 10_000;
        inflated.settled_price = 400;
        assert_eq!(work_unit_score(&inflated).unwrap(), 400);
    }

    #[test]
    fn evidence_multiplier_scales_the_score() {
        let replicated = unit(1, 2, 1_000, EvidenceLevel::Replicated);
        assert_eq!(work_unit_score(&replicated).unwrap(), 2_000);
    }

    #[test]
    fn apply_bps_reports_overflow() {
        assert!(apply_bps(u128::MAX, 10_000).is_err());
    }

    #[test]
    fn reliability_defaults_neutral_rewards_clean_punishes_bad() {
        assert_eq!(reliability_factor_bps(None).unwrap(), 10_000);
        let clean = ReliabilityInputs {
            settlement_success_bps: 10_000,
            dispute_loss_bps: 0,
            sla_breach_bps: 0,
        };
        assert_eq!(reliability_factor_bps(Some(clean)).unwrap(), 11_000);
        let bad = ReliabilityInputs {
            settlement_success_bps: 2_000,
            dispute_loss_bps: 3_000,
            sla_breach_bps: 3_000,
        };
        assert_eq!(reliability_factor_bps(Some(bad)).unwrap(), 5_000);
        let invalid = ReliabilityInputs {
            settlement_success_bps: 10_001,
            dispute_loss_bps: 0,
            sla_breach_bps: 0,
        };
        assert!(reliability_factor_bps(Some(invalid)).is_err());
    }

    #[test]
    fn epoch_pool_is_demand_bounded() {
        // Organic value 1_000 at k=300% -> 3_000, below the 10_000 cap.
        assert_eq!(epoch_pool(10_000, 300, 1_000).unwrap(), 3_000);
        // Large demand saturates at the schedule cap.
        assert_eq!(epoch_pool(10_000, 300, 1_000_000).unwrap(), 10_000);
        // An empty network emits nothing.
        assert_eq!(epoch_pool(10_000, 300, 0).unwrap(), 0);
    }

    #[test]
    fn maturation_conserves_total_and_orders_remainder() {
        let schedule = maturation_schedule(1_003, 2_500, 8).unwrap();
        let streamed: u128 = schedule.stream.iter().sum();
        assert_eq!(schedule.immediate + streamed, 1_003);
        assert_eq!(schedule.stream.len(), 8);
        for pair in schedule.stream.windows(2) {
            assert!(pair[0] >= pair[1]);
        }
        assert!(maturation_schedule(1, 10_001, 8).is_err());
        assert!(maturation_schedule(1, 2_500, 0).is_err());
        let zero = maturation_schedule(0, 2_500, 8).unwrap();
        assert_eq!(zero.immediate, 0);
        assert_eq!(zero.stream.iter().sum::<u128>(), 0);
    }

    #[test]
    fn score_epoch_is_deterministic_and_filters_related() {
        let mut related = unit(1, 1, 5_000, EvidenceLevel::Observed);
        related.payer_related = true;
        let units = vec![
            unit(1, 10, 1_000, EvidenceLevel::Observed),
            unit(1, 11, 1_000, EvidenceLevel::Observed),
            unit(3, 10, 1_000, EvidenceLevel::Benchmarked),
            unit(3, 11, 1_000, EvidenceLevel::Benchmarked),
            related,
        ];
        let reliability = BTreeMap::new();
        let once = score_epoch(&units, &reliability, &ClassifierParams::default()).unwrap();
        let twice = score_epoch(&units, &reliability, &ClassifierParams::default()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(once.scores.len(), 2);
        let earner_one = once
            .scores
            .iter()
            .find(|s| s.identity == IdentityId([1; 32]))
            .unwrap();
        let earner_three = once
            .scores
            .iter()
            .find(|s| s.identity == IdentityId([3; 32]))
            .unwrap();
        // Related-payer work contributed nothing; benchmarked evidence
        // outweighs observed at equal settled value.
        assert_eq!(earner_one.score, 2_000);
        assert_eq!(earner_three.score, 2_400);
        // Each earner's value is split evenly across two payers, so the
        // counterparty discount stays full and organic value is the sum
        // of the four organic settlements; the related unit is excluded.
        assert_eq!(once.organic_settled_value, 4_000);
    }

    #[test]
    fn empty_epoch_scores_empty() {
        let result = score_epoch(&[], &BTreeMap::new(), &ClassifierParams::default()).unwrap();
        assert!(result.scores.is_empty());
        assert_eq!(result.organic_settled_value, 0);
    }

    #[test]
    fn allocate_pool_caps_shares_and_burns_the_excess() {
        let scores = vec![
            IdentityScore {
                identity: IdentityId([1; 32]),
                score: 900,
            },
            IdentityScore {
                identity: IdentityId([2; 32]),
                score: 100,
            },
        ];
        let params = ScoreParams {
            payout_cap_share_bps: 2_000,
        };
        let payouts = allocate_pool(10_000, &scores, &params).unwrap();
        // Identity 1's pro-rata 9_000 is capped at 2_000; identity 2
        // keeps its uncapped 1_000. The 7_000 excess is never created.
        assert_eq!(payouts[0].amount, 2_000);
        assert_eq!(payouts[1].amount, 1_000);
        let created: u128 = payouts.iter().map(|p| p.amount).sum();
        assert!(created <= 10_000);
    }

    #[test]
    fn allocate_pool_handles_zero_totals() {
        let scores = vec![IdentityScore {
            identity: IdentityId([1; 32]),
            score: 0,
        }];
        let payouts = allocate_pool(10_000, &scores, &ScoreParams::default()).unwrap();
        assert_eq!(payouts[0].amount, 0);
        let none = allocate_pool(0, &scores, &ScoreParams::default()).unwrap();
        assert_eq!(none[0].amount, 0);
    }
}
