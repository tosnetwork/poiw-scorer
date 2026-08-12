//! Organic/wash classification.
//!
//! This crate decides what counts as *organic settled value* — the input
//! that drives demand-coupled emission — and computes the counterparty-
//! concentration discount that decays wash-heavy earning toward zero.
//!
//! It is consensus-critical in effect: a wrong classification changes how
//! much TOS the protocol creates. Changes here follow the same discipline
//! as consensus code: versioned methodology, advance publication, and the
//! bonded challenge window.

use std::collections::BTreeMap;

use poiw_types::{Bps, IdentityId, PoiwError, SettledWorkUnit, BPS_DENOMINATOR};

/// Classifier parameters (methodology v0 draft values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassifierParams {
    /// Top-payer share at or below which the discount is full (10_000 bps
    /// multiplier, i.e. no reduction).
    pub top_payer_full_bps: Bps,
    /// Top-payer share at or above which the multiplier reaches zero.
    pub top_payer_zero_bps: Bps,
}

impl Default for ClassifierParams {
    fn default() -> Self {
        Self {
            top_payer_full_bps: 5_000,
            top_payer_zero_bps: 10_000,
        }
    }
}

impl ClassifierParams {
    fn validate(&self) -> Result<(), PoiwError> {
        if self.top_payer_full_bps >= self.top_payer_zero_bps {
            return Err(PoiwError::InvalidParameter(
                "top_payer_full_bps must be below top_payer_zero_bps",
            ));
        }
        if self.top_payer_zero_bps > 10_000 {
            return Err(PoiwError::InvalidParameter(
                "top_payer_zero_bps must not exceed 10_000",
            ));
        }
        Ok(())
    }
}

/// A work unit is eligible for scoring unless its payer sits inside the
/// earner's disclosed control domain.
pub fn is_score_eligible(unit: &SettledWorkUnit) -> bool {
    !unit.payer_related
}

/// A work unit contributes to organic settled value only if it is
/// score-eligible and not a protocol-issued challenge task.
pub fn is_organic(unit: &SettledWorkUnit) -> bool {
    is_score_eligible(unit) && !unit.is_challenge_task
}

/// Counterparty-concentration multiplier for one earner, from that
/// earner's per-payer settled totals: 10_000 bps at or below the full
/// threshold, decaying linearly to 0 at the zero threshold.
///
/// An earner with no settled value has nothing to discount and gets the
/// full multiplier.
pub fn counterparty_discount_bps(
    per_payer_settled: &BTreeMap<IdentityId, u128>,
    params: &ClassifierParams,
) -> Result<Bps, PoiwError> {
    params.validate()?;
    let total: u128 = per_payer_settled
        .values()
        .try_fold(0u128, |acc, v| acc.checked_add(*v))
        .ok_or(PoiwError::Overflow)?;
    if total == 0 {
        return Ok(10_000);
    }
    let top = per_payer_settled.values().max().copied().unwrap_or(0);
    let share_bps_wide = top
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(PoiwError::Overflow)?
        .checked_div(total)
        .ok_or(PoiwError::Overflow)?;
    let share_bps = Bps::try_from(share_bps_wide).map_err(|_| PoiwError::Overflow)?;

    if share_bps <= params.top_payer_full_bps {
        return Ok(10_000);
    }
    if share_bps >= params.top_payer_zero_bps {
        return Ok(0);
    }
    let span = u128::from(
        params
            .top_payer_zero_bps
            .checked_sub(params.top_payer_full_bps)
            .ok_or(PoiwError::Overflow)?,
    );
    let above = u128::from(
        share_bps
            .checked_sub(params.top_payer_full_bps)
            .ok_or(PoiwError::Overflow)?,
    );
    let remaining = span.checked_sub(above).ok_or(PoiwError::Overflow)?;
    let scaled = remaining
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(PoiwError::Overflow)?
        .checked_div(span)
        .ok_or(PoiwError::Overflow)?;
    Bps::try_from(scaled).map_err(|_| PoiwError::Overflow)
}

/// Epoch-level organic settled value: for each earner, sum the settled
/// prices of organic units, apply that earner's counterparty discount,
/// and sum across earners. Deterministic by construction (`BTreeMap`
/// ordering, integer arithmetic only).
pub fn organic_settled_value(
    units: &[SettledWorkUnit],
    params: &ClassifierParams,
) -> Result<u128, PoiwError> {
    params.validate()?;
    let mut per_earner: BTreeMap<IdentityId, BTreeMap<IdentityId, u128>> = BTreeMap::new();
    for unit in units.iter().filter(|u| is_organic(u)) {
        let payers = per_earner.entry(unit.identity).or_default();
        let entry = payers.entry(unit.payer).or_insert(0);
        *entry = entry
            .checked_add(u128::from(unit.settled_price))
            .ok_or(PoiwError::Overflow)?;
    }

    let mut total = 0u128;
    for payers in per_earner.values() {
        let earner_total: u128 = payers
            .values()
            .try_fold(0u128, |acc, v| acc.checked_add(*v))
            .ok_or(PoiwError::Overflow)?;
        let discount = counterparty_discount_bps(payers, params)?;
        let discounted = earner_total
            .checked_mul(u128::from(discount))
            .ok_or(PoiwError::Overflow)?
            .checked_div(BPS_DENOMINATOR)
            .ok_or(PoiwError::Overflow)?;
        total = total.checked_add(discounted).ok_or(PoiwError::Overflow)?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use poiw_types::{CapabilityClass, EvidenceLevel};

    use super::*;

    fn unit(earner: u8, payer: u8, price: u64, challenge: bool, related: bool) -> SettledWorkUnit {
        SettledWorkUnit {
            identity: IdentityId([earner; 32]),
            payer: IdentityId([payer; 32]),
            capability: CapabilityClass("embedding".into()),
            rate_card_value: price,
            settled_price: price,
            evidence: EvidenceLevel::Observed,
            is_challenge_task: challenge,
            payer_related: related,
        }
    }

    #[test]
    fn empty_input_yields_zero_organic_value() {
        let value = organic_settled_value(&[], &ClassifierParams::default()).unwrap();
        assert_eq!(value, 0);
    }

    #[test]
    fn challenge_and_related_units_are_excluded() {
        let units = vec![
            unit(1, 2, 1_000, false, false),
            unit(1, 3, 1_000, true, false),
            unit(1, 4, 1_000, false, true),
        ];
        // Only the first unit is organic; its single payer means 100%
        // concentration, so the discount multiplies it to zero.
        let value = organic_settled_value(&units, &ClassifierParams::default()).unwrap();
        assert_eq!(value, 0);
    }

    #[test]
    fn balanced_payers_keep_full_value() {
        let units = vec![
            unit(1, 2, 1_000, false, false),
            unit(1, 3, 1_000, false, false),
        ];
        let value = organic_settled_value(&units, &ClassifierParams::default()).unwrap();
        assert_eq!(value, 2_000);
    }

    #[test]
    fn dominant_payer_decays_linearly() {
        // 3:1 split -> top share 7,500 bps, halfway between 5,000 and
        // 10,000 -> multiplier 5,000 bps -> half value.
        let units = vec![
            unit(1, 2, 3_000, false, false),
            unit(1, 3, 1_000, false, false),
        ];
        let value = organic_settled_value(&units, &ClassifierParams::default()).unwrap();
        assert_eq!(value, 2_000);
    }

    #[test]
    fn single_payer_earns_zero_organic_value() {
        let per_payer: BTreeMap<IdentityId, u128> =
            [(IdentityId([9; 32]), 500u128)].into_iter().collect();
        let bps = counterparty_discount_bps(&per_payer, &ClassifierParams::default()).unwrap();
        assert_eq!(bps, 0);
    }

    #[test]
    fn invalid_params_are_rejected() {
        let params = ClassifierParams {
            top_payer_full_bps: 8_000,
            top_payer_zero_bps: 8_000,
        };
        assert!(matches!(
            organic_settled_value(&[], &params),
            Err(PoiwError::InvalidParameter(_))
        ));
    }
}
