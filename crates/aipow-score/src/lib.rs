//! Deterministic AIPoW score computation.
//!
//! Everything here is pure integer arithmetic over the settled-work
//! records produced by the indexer and classified by `aipow-classifier`.
//! The functions implement the published methodology; the methodology
//! document, not this code, is the normative artifact.
//!
//! Scoring and payout are deliberately separate stages: [`score_epoch`]
//! produces evidence- and reliability-weighted scores in two buckets
//! (organic and challenge), and [`allocate_epoch`] turns scores into
//! payouts under the challenge-budget bounds and the per-control-domain
//! cap. Pool value above a cap is simply never created, consistent with
//! the design rule that un-earned residue is not minted and not rolled
//! over.

use std::collections::BTreeMap;

use aipow_classifier::{is_score_eligible, ClassifierParams};
use aipow_types::{
    AipowError, Bps, DomainKey, DomainMap, IdentityId, ReliabilityInputs, SettledWorkUnit,
    BPS_DENOMINATOR,
};

/// Scoring parameters (methodology v0 draft values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreParams {
    /// Hard cap on any single control domain's share of one epoch
    /// budget. Pro-rata payout above this share is not created.
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
    fn validate(&self) -> Result<(), AipowError> {
        if self.payout_cap_share_bps == 0 || self.payout_cap_share_bps > 10_000 {
            return Err(AipowError::InvalidParameter(
                "payout_cap_share_bps must be within 1..=10_000",
            ));
        }
        Ok(())
    }
}

/// Challenge-task budget parameters (methodology v0 draft values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChallengeBudgetParams {
    /// Maximum share of the epoch pool the challenge bucket may take.
    pub max_pool_share_bps: Bps,
    /// The challenge budget may not exceed this multiple (integer
    /// percent) of the trailing organic settled value.
    pub organic_multiple_percent: u32,
    /// Small published floor so a brand-new network can bootstrap
    /// evidence before its first organic settlements. This is the only
    /// emission not gated by demand.
    pub cold_start_floor: u128,
    /// Per-identity cap on challenge payouts in one epoch.
    pub per_identity_cap: u128,
}

impl Default for ChallengeBudgetParams {
    fn default() -> Self {
        Self {
            max_pool_share_bps: 3_000,
            organic_multiple_percent: 200,
            // Draft: 2,000 TOS per epoch, deliberately too small to farm
            // profitably at scale.
            cold_start_floor: 2_000_000_000_000,
            // Draft: 100 TOS per identity per epoch from challenges.
            per_identity_cap: 100_000_000_000,
        }
    }
}

impl ChallengeBudgetParams {
    fn validate(&self) -> Result<(), AipowError> {
        if self.max_pool_share_bps > 10_000 {
            return Err(AipowError::InvalidParameter(
                "max_pool_share_bps must not exceed 10_000",
            ));
        }
        Ok(())
    }
}

/// Multiply an amount by a basis-point factor, flooring.
pub fn apply_bps(value: u128, bps: Bps) -> Result<u128, AipowError> {
    value
        .checked_mul(u128::from(bps))
        .ok_or(AipowError::Overflow)?
        .checked_div(BPS_DENOMINATOR)
        .ok_or(AipowError::Overflow)
}

/// Score of one settled work unit: the rate-card valuation capped by the
/// actually settled price, times the evidence multiplier. `Declared`
/// evidence yields exactly zero.
pub fn work_unit_score(unit: &SettledWorkUnit) -> Result<u128, AipowError> {
    let base = u128::from(unit.rate_card_value.min(unit.settled_price));
    base.checked_mul(u128::from(unit.evidence.multiplier_percent()))
        .ok_or(AipowError::Overflow)?
        .checked_div(100)
        .ok_or(AipowError::Overflow)
}

/// Reliability factor in basis points, clamped to [5_000, 11_000].
///
/// No history means a neutral 10_000: a new identity starts at 1.0 and
/// seniority alone is never rewarded. A window with zero penalties earns
/// the 11_000 ceiling; penalties subtract from the 10_000 baseline down
/// to the 5_000 floor.
pub fn reliability_factor_bps(history: Option<ReliabilityInputs>) -> Result<Bps, AipowError> {
    let Some(inputs) = history else {
        return Ok(10_000);
    };
    for value in [
        inputs.settlement_success_bps,
        inputs.dispute_loss_bps,
        inputs.sla_breach_bps,
    ] {
        if value > 10_000 {
            return Err(AipowError::InvalidParameter(
                "reliability inputs must be within 10_000 bps",
            ));
        }
    }
    let failure = 10_000u32
        .checked_sub(inputs.settlement_success_bps)
        .ok_or(AipowError::Overflow)?;
    let penalty = failure
        .checked_add(inputs.dispute_loss_bps)
        .ok_or(AipowError::Overflow)?
        .checked_add(inputs.sla_breach_bps)
        .ok_or(AipowError::Overflow)?;
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
) -> Result<u128, AipowError> {
    let coupled = organic_settled_value
        .checked_mul(u128::from(k_percent))
        .ok_or(AipowError::Overflow)?
        .checked_div(100)
        .ok_or(AipowError::Overflow)?;
    Ok(schedule_cap.min(coupled))
}

/// The challenge bucket for one epoch, bounded three ways: at most
/// `max_pool_share_bps` of the pool, at most `organic_multiple_percent`
/// of the organic settled value, but never below the cold-start floor —
/// the floor is the only emission not gated by demand, and it is sized
/// to be unprofitable to farm at scale.
pub fn challenge_budget(
    pool: u128,
    organic_settled_value: u128,
    params: &ChallengeBudgetParams,
) -> Result<u128, AipowError> {
    params.validate()?;
    let share_bound = apply_bps(pool, params.max_pool_share_bps)?;
    let organic_bound = organic_settled_value
        .checked_mul(u128::from(params.organic_multiple_percent))
        .ok_or(AipowError::Overflow)?
        .checked_div(100)
        .ok_or(AipowError::Overflow)?;
    Ok(share_bound.min(organic_bound).max(params.cold_start_floor))
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
) -> Result<MaturationSchedule, AipowError> {
    if immediate_bps > 10_000 {
        return Err(AipowError::InvalidParameter(
            "immediate_bps must not exceed 10_000",
        ));
    }
    if stream_epochs == 0 {
        return Err(AipowError::InvalidParameter(
            "stream_epochs must be at least 1",
        ));
    }
    let immediate = apply_bps(total, immediate_bps)?;
    let streamed = total.checked_sub(immediate).ok_or(AipowError::Overflow)?;
    let per = streamed
        .checked_div(u128::from(stream_epochs))
        .ok_or(AipowError::Overflow)?;
    let remainder = streamed
        .checked_rem(u128::from(stream_epochs))
        .ok_or(AipowError::Overflow)?;
    let mut stream = Vec::with_capacity(stream_epochs as usize);
    for index in 0..u128::from(stream_epochs) {
        let extra = if index < remainder { 1 } else { 0 };
        stream.push(per.checked_add(extra).ok_or(AipowError::Overflow)?);
    }
    Ok(MaturationSchedule { immediate, stream })
}

/// One identity's final epoch score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityScore {
    pub identity: IdentityId,
    pub score: u128,
}

/// The complete deterministic result of scoring one epoch, split into
/// the organic bucket and the challenge bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochScores {
    /// Scores from organic (consumer-paid) work, ordered by identity.
    pub organic: Vec<IdentityScore>,
    /// Scores from protocol-issued challenge tasks, ordered by identity.
    pub challenge: Vec<IdentityScore>,
    /// Organic settled value for the emission formula.
    pub organic_settled_value: u128,
}

/// Score one epoch of settled work.
///
/// Pipeline: filter score-eligible units (related payers and same-domain
/// payers excluded), sum evidence-weighted unit scores per identity into
/// the organic or challenge bucket, then apply each identity's
/// reliability factor. Deterministic by construction: `BTreeMap`
/// ordering and integer arithmetic only.
pub fn score_epoch(
    units: &[SettledWorkUnit],
    reliability: &BTreeMap<IdentityId, ReliabilityInputs>,
    domains: &DomainMap,
    classifier_params: &ClassifierParams,
) -> Result<EpochScores, AipowError> {
    let mut organic_raw: BTreeMap<IdentityId, u128> = BTreeMap::new();
    let mut challenge_raw: BTreeMap<IdentityId, u128> = BTreeMap::new();
    for unit in units.iter().filter(|u| is_score_eligible(u, domains)) {
        let unit_score = work_unit_score(unit)?;
        let bucket = if unit.is_challenge_task {
            &mut challenge_raw
        } else {
            &mut organic_raw
        };
        let entry = bucket.entry(unit.identity).or_insert(0);
        *entry = entry.checked_add(unit_score).ok_or(AipowError::Overflow)?;
    }

    let adjust = |raw: &BTreeMap<IdentityId, u128>| -> Result<Vec<IdentityScore>, AipowError> {
        let mut scores = Vec::with_capacity(raw.len());
        for (identity, raw_score) in raw {
            let factor = reliability_factor_bps(reliability.get(identity).copied())?;
            let adjusted = apply_bps(*raw_score, factor)?;
            scores.push(IdentityScore {
                identity: *identity,
                score: adjusted,
            });
        }
        Ok(scores)
    };

    let organic = adjust(&organic_raw)?;
    let challenge = adjust(&challenge_raw)?;
    let organic_value = aipow_classifier::organic_settled_value(units, domains, classifier_params)?;
    Ok(EpochScores {
        organic,
        challenge,
        organic_settled_value: organic_value,
    })
}

/// One identity's payout from an epoch budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityPayout {
    pub identity: IdentityId,
    pub amount: u128,
}

/// Allocate one budget pro rata by score, then enforce the optional
/// per-identity cap and the per-control-domain cap. When a domain's
/// combined pro-rata payout exceeds the domain cap, every member's
/// payout is scaled down proportionally; the capped-off value is **not
/// redistributed and not created**.
pub fn allocate_bucket(
    budget: u128,
    scores: &[IdentityScore],
    domains: &DomainMap,
    cap_share_bps: Bps,
    per_identity_cap: Option<u128>,
) -> Result<Vec<IdentityPayout>, AipowError> {
    let total: u128 = scores
        .iter()
        .try_fold(0u128, |acc, s| acc.checked_add(s.score))
        .ok_or(AipowError::Overflow)?;
    if total == 0 || budget == 0 {
        return Ok(scores
            .iter()
            .map(|s| IdentityPayout {
                identity: s.identity,
                amount: 0,
            })
            .collect());
    }

    let mut payouts = Vec::with_capacity(scores.len());
    for entry in scores {
        let pro_rata = budget
            .checked_mul(entry.score)
            .ok_or(AipowError::Overflow)?
            .checked_div(total)
            .ok_or(AipowError::Overflow)?;
        let capped = match per_identity_cap {
            Some(cap) => pro_rata.min(cap),
            None => pro_rata,
        };
        payouts.push(IdentityPayout {
            identity: entry.identity,
            amount: capped,
        });
    }

    let cap_amount = apply_bps(budget, cap_share_bps)?;
    let mut per_domain: BTreeMap<DomainKey, u128> = BTreeMap::new();
    for payout in &payouts {
        let key = domains.domain_of(payout.identity);
        let entry = per_domain.entry(key).or_insert(0);
        *entry = entry
            .checked_add(payout.amount)
            .ok_or(AipowError::Overflow)?;
    }
    for payout in &mut payouts {
        let key = domains.domain_of(payout.identity);
        let domain_total = per_domain.get(&key).copied().unwrap_or(0);
        if domain_total > cap_amount {
            payout.amount = payout
                .amount
                .checked_mul(cap_amount)
                .ok_or(AipowError::Overflow)?
                .checked_div(domain_total)
                .ok_or(AipowError::Overflow)?;
        }
    }
    Ok(payouts)
}

/// The complete allocation of one epoch's emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochAllocation {
    /// Budget allocated to organic work (`pool - challenge_budget`,
    /// never below zero).
    pub organic_budget: u128,
    /// Budget allocated to challenge work (see [`challenge_budget`]).
    pub challenge_budget: u128,
    pub organic: Vec<IdentityPayout>,
    pub challenge: Vec<IdentityPayout>,
}

/// Allocate one epoch: derive the challenge budget from the pool and
/// organic value, give the remainder of the pool to organic work, and
/// distribute both buckets under their caps.
///
/// Only the within-pool portion of the challenge budget (bounded by the
/// pool-share cap) is carved out of the organic pool; the cold-start
/// floor's excess above that is **additive** emission, because the floor
/// is deliberately the only emission not gated by demand and must not
/// cannibalize organic payouts. In a cold-start epoch (zero organic
/// value, zero pool) the challenge budget equals the floor and the
/// organic budget is zero — total emission is exactly the floor.
pub fn allocate_epoch(
    pool: u128,
    scores: &EpochScores,
    domains: &DomainMap,
    score_params: &ScoreParams,
    challenge_params: &ChallengeBudgetParams,
) -> Result<EpochAllocation, AipowError> {
    score_params.validate()?;
    let challenge = challenge_budget(pool, scores.organic_settled_value, challenge_params)?;
    let carve = challenge.min(apply_bps(pool, challenge_params.max_pool_share_bps)?);
    let organic_budget = pool.saturating_sub(carve);
    let organic_payouts = allocate_bucket(
        organic_budget,
        &scores.organic,
        domains,
        score_params.payout_cap_share_bps,
        None,
    )?;
    let challenge_payouts = allocate_bucket(
        challenge,
        &scores.challenge,
        domains,
        score_params.payout_cap_share_bps,
        Some(challenge_params.per_identity_cap),
    )?;
    Ok(EpochAllocation {
        organic_budget,
        challenge_budget: challenge,
        organic: organic_payouts,
        challenge: challenge_payouts,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use aipow_types::{CapabilityClass, DomainId, EvidenceLevel};

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
        assert_eq!(epoch_pool(10_000, 300, 1_000).unwrap(), 3_000);
        assert_eq!(epoch_pool(10_000, 300, 1_000_000).unwrap(), 10_000);
        assert_eq!(epoch_pool(10_000, 300, 0).unwrap(), 0);
    }

    #[test]
    fn challenge_budget_is_triple_bounded() {
        let params = ChallengeBudgetParams {
            max_pool_share_bps: 3_000,
            organic_multiple_percent: 200,
            cold_start_floor: 50,
            per_identity_cap: 1_000,
        };
        // Share bound binds: 30% of 10_000 = 3_000 < 2x organic 20_000.
        assert_eq!(challenge_budget(10_000, 10_000, &params).unwrap(), 3_000);
        // Organic bound binds: 2x organic 200 < 30% of 10_000.
        assert_eq!(challenge_budget(10_000, 100, &params).unwrap(), 200);
        // Cold start: zero pool, zero organic -> exactly the floor.
        assert_eq!(challenge_budget(0, 0, &params).unwrap(), 50);
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
    fn score_epoch_splits_buckets_and_filters_related() {
        let mut related = unit(1, 1, 5_000, EvidenceLevel::Observed);
        related.payer_related = true;
        let mut challenge = unit(4, 0, 1_000, EvidenceLevel::Benchmarked);
        challenge.is_challenge_task = true;
        let units = vec![
            unit(1, 10, 1_000, EvidenceLevel::Observed),
            unit(1, 11, 1_000, EvidenceLevel::Observed),
            unit(3, 10, 1_000, EvidenceLevel::Benchmarked),
            unit(3, 11, 1_000, EvidenceLevel::Benchmarked),
            related,
            challenge,
        ];
        let reliability = BTreeMap::new();
        let domains = DomainMap::default();
        let once =
            score_epoch(&units, &reliability, &domains, &ClassifierParams::default()).unwrap();
        let twice =
            score_epoch(&units, &reliability, &domains, &ClassifierParams::default()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(once.organic.len(), 2);
        assert_eq!(once.challenge.len(), 1);
        let earner_one = once
            .organic
            .iter()
            .find(|s| s.identity == IdentityId([1; 32]))
            .unwrap();
        let earner_three = once
            .organic
            .iter()
            .find(|s| s.identity == IdentityId([3; 32]))
            .unwrap();
        assert_eq!(earner_one.score, 2_000);
        assert_eq!(earner_three.score, 2_400);
        assert_eq!(once.challenge[0].score, 1_200);
        // Challenge work never counts toward organic value.
        assert_eq!(once.organic_settled_value, 4_000);
    }

    #[test]
    fn same_domain_work_is_excluded_from_scoring() {
        let domains = DomainMap::from_pairs([
            (IdentityId([1; 32]), DomainId("acme".into())),
            (IdentityId([2; 32]), DomainId("acme".into())),
        ]);
        let units = vec![unit(1, 2, 1_000, EvidenceLevel::Observed)];
        let result = score_epoch(
            &units,
            &BTreeMap::new(),
            &domains,
            &ClassifierParams::default(),
        )
        .unwrap();
        assert!(result.organic.is_empty());
        assert_eq!(result.organic_settled_value, 0);
    }

    #[test]
    fn empty_epoch_scores_empty() {
        let result = score_epoch(
            &[],
            &BTreeMap::new(),
            &DomainMap::default(),
            &ClassifierParams::default(),
        )
        .unwrap();
        assert!(result.organic.is_empty());
        assert!(result.challenge.is_empty());
        assert_eq!(result.organic_settled_value, 0);
    }

    #[test]
    fn allocate_bucket_caps_domains_and_burns_the_excess() {
        // Identities 1 and 2 share a control domain; identity 3 is solo.
        let domains = DomainMap::from_pairs([
            (IdentityId([1; 32]), DomainId("acme".into())),
            (IdentityId([2; 32]), DomainId("acme".into())),
        ]);
        let scores = vec![
            IdentityScore {
                identity: IdentityId([1; 32]),
                score: 400,
            },
            IdentityScore {
                identity: IdentityId([2; 32]),
                score: 400,
            },
            IdentityScore {
                identity: IdentityId([3; 32]),
                score: 200,
            },
        ];
        let payouts = allocate_bucket(10_000, &scores, &domains, 2_000, None).unwrap();
        // Pro rata: 4_000 + 4_000 + 2_000. The acme domain's 8_000
        // exceeds the 2_000 cap, so both members scale to 1_000 each;
        // identity 3 keeps its uncapped 2_000.
        assert_eq!(payouts[0].amount, 1_000);
        assert_eq!(payouts[1].amount, 1_000);
        assert_eq!(payouts[2].amount, 2_000);
        let created: u128 = payouts.iter().map(|p| p.amount).sum();
        assert!(created <= 10_000);
    }

    #[test]
    fn allocate_bucket_handles_zero_totals() {
        let scores = vec![IdentityScore {
            identity: IdentityId([1; 32]),
            score: 0,
        }];
        let payouts = allocate_bucket(10_000, &scores, &DomainMap::default(), 500, None).unwrap();
        assert_eq!(payouts[0].amount, 0);
        let none = allocate_bucket(0, &scores, &DomainMap::default(), 500, None).unwrap();
        assert_eq!(none[0].amount, 0);
    }

    #[test]
    fn allocate_epoch_cold_start_pays_only_the_floor() {
        let scores = EpochScores {
            organic: vec![],
            challenge: vec![IdentityScore {
                identity: IdentityId([7; 32]),
                score: 500,
            }],
            organic_settled_value: 0,
        };
        let params = ChallengeBudgetParams {
            max_pool_share_bps: 3_000,
            organic_multiple_percent: 200,
            cold_start_floor: 60,
            per_identity_cap: 1_000,
        };
        let allocation = allocate_epoch(
            0,
            &scores,
            &DomainMap::default(),
            &ScoreParams {
                payout_cap_share_bps: 10_000,
            },
            &params,
        )
        .unwrap();
        assert_eq!(allocation.organic_budget, 0);
        assert_eq!(allocation.challenge_budget, 60);
        assert_eq!(allocation.challenge[0].amount, 60);
    }

    #[test]
    fn cold_start_floor_does_not_cannibalize_organic_payouts() {
        let scores = EpochScores {
            organic: vec![IdentityScore {
                identity: IdentityId([1; 32]),
                score: 500,
            }],
            challenge: vec![IdentityScore {
                identity: IdentityId([7; 32]),
                score: 500,
            }],
            organic_settled_value: 40,
        };
        let params = ChallengeBudgetParams {
            max_pool_share_bps: 3_000,
            organic_multiple_percent: 200,
            cold_start_floor: 500,
            per_identity_cap: 1_000,
        };
        // Pool 100: challenge budget = max(min(30, 80), 500) = 500.
        // Only the 30 within the pool-share cap is carved out; the
        // remaining 470 of the floor is additive emission.
        let allocation = allocate_epoch(
            100,
            &scores,
            &DomainMap::default(),
            &ScoreParams {
                payout_cap_share_bps: 10_000,
            },
            &params,
        )
        .unwrap();
        assert_eq!(allocation.challenge_budget, 500);
        assert_eq!(allocation.organic_budget, 70);
        assert_eq!(allocation.organic[0].amount, 70);
        assert_eq!(allocation.challenge[0].amount, 500);
    }

    #[test]
    fn allocate_epoch_enforces_per_identity_challenge_cap() {
        let scores = EpochScores {
            organic: vec![],
            challenge: vec![IdentityScore {
                identity: IdentityId([7; 32]),
                score: 500,
            }],
            organic_settled_value: 100_000,
        };
        let params = ChallengeBudgetParams {
            max_pool_share_bps: 3_000,
            organic_multiple_percent: 200,
            cold_start_floor: 0,
            per_identity_cap: 40,
        };
        let allocation = allocate_epoch(
            10_000,
            &scores,
            &DomainMap::default(),
            &ScoreParams {
                payout_cap_share_bps: 10_000,
            },
            &params,
        )
        .unwrap();
        // Challenge budget is 3_000 (share bound), but the single
        // identity may take at most 40 of it.
        assert_eq!(allocation.challenge_budget, 3_000);
        assert_eq!(allocation.challenge[0].amount, 40);
        assert_eq!(allocation.organic_budget, 7_000);
    }
}
