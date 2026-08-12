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

### 2.1 Receipt attributions and rate cards

Receipts carrying a PoIW work attribution (the protocol's
`PoiwWorkAttribution`; normative vocabulary: atos-spec's
`POIW_WORK_ATTRIBUTION.md`) are valued under a protocol-published rate
card before scoring:

```text
rate_card_value = work_units × price_per_unit(class)   (checked)
default class:    rate_card_value = settled amount (implicit price 1)
```

The attribution's `rate_card_version` must match both the scorer's
vocabulary revision and the supplied card; an unknown version, unknown
class, unit mismatch, unpriced specific class, valuation overflow, or
malformed identity commitment is a hard scoring error, never a repair
or a silent skip. Settlements without an attribution continue under the
interim mapping (settled amount as valuation, `default` class).

Reliability inputs per identity over the trailing window (8 epochs):
settlement-success, dispute-loss, and SLA-breach shares in bps.

## 2.1 Control domains

Identities carry disclosed control-domain assignments from the
common-control registry; an identity without an assignment forms a
singleton domain of itself. Domains, not bare identities, are the unit
of every anti-collusion rule: a payer inside the earner's domain is
treated as related, payers sharing one domain count as one counterparty
in the concentration discount, and payout caps apply to a domain's
combined take.

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

Units whose payer is flagged related or shares the earner's control
domain are excluded from scoring entirely. Challenge-task units score
normally but accumulate in a separate challenge bucket.

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
related-payer or same-domain units), group settled prices by payer
control domain:

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

## 7.1 Challenge budget

```text
challenge_budget = max( min( pool × 3,000 / 10,000,
                             organic_settled_value × 200 / 100 ),
                        cold_start_floor )
carve            = min( challenge_budget, pool × 3,000 / 10,000 )
organic_budget   = pool − carve
```

Draft values: 30% pool-share cap, 2× organic bound, cold-start floor
2,000 TOS per epoch, per-identity challenge cap 100 TOS per epoch. Only
the within-pool-share portion is carved out of the organic pool; the
floor's excess is additive emission, because the floor is deliberately
the only emission not gated by demand and must not cannibalize organic
payouts. In a zero-demand epoch, total emission is exactly the floor.

## 8. Payout allocation

Each bucket allocates independently:

```text
cap_amount = budget × 500 / 10,000        (5% per control domain)
payout(i)  = budget × score(i) / Σ bucket scores        (floor)
             then min(payout(i), per_identity_cap)      (challenge only)
```

If a control domain's combined payout exceeds `cap_amount`, every member
payout is scaled by `cap_amount / domain_total` (floor). Value above a
cap is **not redistributed and not created**.

## 9. Maturation

Each payout splits into 25% immediate plus a stream over the following
8 epochs. Division remainders go to the earliest stream epochs. The
unmatured remainder is forfeited only on registry-bond fraud slashing.

## 10. Score root

- Committed entries are the merged per-identity totals (organic plus
  challenge score per identity).
- Entries sorted by identity bytes; duplicate identities are an error.
- Leaf: `sha256(0x00 ‖ identity(32) ‖ score as 16-byte big-endian)`.
- Node: `sha256(0x01 ‖ left ‖ right)`; an odd node is promoted
  unchanged.
- Empty epoch: `sha256(0x02 ‖ "poiw-empty-v0")`.

## 10.1 Commitment envelope

The signing payload for an epoch commitment is
`sha256("poiw-commit-v0" ‖ epoch_be(8) ‖ version_len_be(4) ‖
methodology_version_utf8 ‖ root(32) ‖ entry_count_be(8) ‖
total_score_be(16) ‖ organic_settled_value_be(16))`, signed with
ed25519. The same digest is used for shadow-scoring file publication and
for on-chain submission once the distributor contract exists.

## 11. Chain ingestion

Finalized masterchain blocks are walked with a checkpoint (last scanned
seqno plus root hash). Before advancing, the stored hash is re-verified
against the live chain; on a mismatch the walker rewinds a fixed margin
(5 blocks) and re-ingests. The margin bounds, but does not eliminate,
stale data from an orphaned branch below the rewind point — the same
documented semantics as the node's contract indexer. Units bucket into
epochs by block unix time divided by the epoch length; an epoch is
scoreable once a finalized block's time passes the epoch end.

## 12. Open items for v1

- Trailing-window definition for organic value (today: current epoch in
  the reference pipeline; target: published multi-epoch window).
- Governance publication of the priced rate card per class (the
  vocabulary and units are normative in atos-spec's
  `POIW_WORK_ATTRIBUTION.md`; the prices themselves remain draft).
- Reliability-input sourcing from chain data (part of the pending
  node-side method surface).
- The node-side `poiwGetSettledWork` JSON-RPC method and the distributor
  contract submission path (owned by the `tos` repository).
