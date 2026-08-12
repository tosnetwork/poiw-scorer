# PoIW Scoring Methodology — v0 draft

This document is the **normative artifact** for Proof of Intelligent Work
scoring. Conforming implementations — including the two independent
scorers required before mainnet activation — must reproduce identical
epoch score roots byte for byte from the same finalized chain data and
this document alone. The Rust code in this repository implements the
methodology; where code and document disagree, the document governs and
the code has a bug.

Status: v0 draft. Every constant below is a proposal pending governance
publication, adversarial red-team review, and the launch gates in the
PoIW distribution design.

## 1. Units and arithmetic

- Amounts: integer nanotos (1 TOS = 10^9 nanotos).
- Multipliers: integer basis points (10,000 bps = 1.0) or integer
  percent. No floating point anywhere.
- All arithmetic is checked; overflow aborts scoring for the epoch
  rather than wrapping.
- All aggregation orders are defined by ascending identity bytes.

## 2. Inputs

One settled work unit per finalized Task Escrow settlement or Service
Actor receipt, carrying: earner identity, payer identity, capability
class (published vocabulary), rate-card valuation, settled price,
evidence level, challenge-task flag, related-payer flag.

Reliability inputs per identity over the trailing window (8 epochs):
settlement-success, dispute-loss, and SLA-breach shares in bps.

## 3. Work-unit score

```text
base  = min(rate_card_value, settled_price)
score = base × evidence_multiplier_percent / 100      (floor)
```

| Evidence level | Multiplier (percent) |
|---|---:|
| Declared | 0 |
| Observed | 100 |
| Benchmarked | 120 |
| Audited | 150 |
| Attested | 170 |
| Replicated | 200 |

Units whose payer is inside the earner's disclosed control domain
(`payer_related`) are excluded from scoring entirely.

## 4. Identity score

```text
raw(identity)   = Σ work-unit scores of that identity's eligible units
factor(identity) = reliability factor in bps (Section 5)
score(identity) = raw × factor / 10,000               (floor)
```

## 5. Reliability factor

- No history in the window: 10,000 bps (neutral; new identities start at
  1.0 and seniority alone earns nothing).
- Otherwise: `penalty = (10,000 − settlement_success) + dispute_loss +
  sla_breach`.
- `penalty = 0` → 11,000. Otherwise `max(10,000 − penalty, 5,000)`.

## 6. Counterparty-concentration discount and organic value

Per earner, over organic units only (not challenge tasks, not
related-payer units), group settled prices by payer:

```text
top_share = top payer total × 10,000 / earner total   (bps, floor)
discount  = 10,000                        if top_share ≤ 5,000
          = 0                             if top_share ≥ 10,000
          = 10,000 × (10,000 − top_share) / 5,000     otherwise (floor)
```

```text
organic_settled_value(epoch) =
    Σ over earners ( earner organic total × discount / 10,000 )
```

## 7. Demand-coupled epoch pool

```text
pool = min( schedule_cap(epoch),
            organic_settled_value(trailing window) × k / 100 )
```

Draft values: `k` = 300 (bootstrap) → 150 (growth) → 80 (maturity)
percent; `schedule_cap` calibrated toward the 4.5B TOS allocation over
the target horizon. Un-emitted difference is never created and never
rolled over.

## 8. Payout allocation

```text
cap_amount = pool × 500 / 10,000          (5% per control domain)
payout(i)  = min( pool × score(i) / Σ scores, cap_amount )
```

Pro-rata value above the cap is **not redistributed and not created**.

## 9. Maturation

Each payout splits into 25% immediate plus a stream over the following
8 epochs. Division remainders go to the earliest stream epochs. The
unmatured remainder is forfeited only on registry-bond fraud slashing.

## 10. Score root

- Entries sorted by identity bytes; duplicate identities are an error.
- Leaf: `sha256(0x00 ‖ identity(32) ‖ score as 16-byte big-endian)`.
- Node: `sha256(0x01 ‖ left ‖ right)`; an odd node is promoted
  unchanged.
- Empty epoch: `sha256(0x02 ‖ "poiw-empty-v0")`.

## 11. Open items for v1

- Challenge-task sub-budget accounting (30% share cap, 2× organic cap,
  cold-start floor) as a separate scored bucket.
- Control-domain grouping (today: per identity; target: per disclosed
  control domain).
- Trailing-window definition for organic value (today: current epoch in
  the reference pipeline; target: published multi-epoch window).
- Rate-card vocabulary and per-class unit definitions (owned by
  `tos-protocol`).
