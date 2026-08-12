# poiw-scorer

Reference implementation (#1) of the **Proof of Intelligent Work (PoIW)**
scorer for TOS Network.

PoIW distributes the community-agent allocation (~4.5B of the 5B TOS
supply policy) as proof-of-useful-work issuance: protocol-automatic,
issuer-less rewards for completed, evidence-graded, on-chain-settled
intelligent work. The scorer computes per-epoch scores from public chain
data and commits a score root on-chain, where it faces a bonded public
challenge window before any distribution occurs.

**The scorer computes; it never mints, holds, or moves funds.** Reward
creation happens in the TOS protocol itself, and a second, independently
written scorer implementation must reproduce this implementation's score
roots byte for byte before mainnet activation.

The normative artifact is [`docs/methodology.md`](docs/methodology.md) —
implementations, including this one, are replaceable.

## Workspace layout

| Crate | Responsibility |
|---|---|
| `poiw-types` | Shared domain types; integer-only units |
| `poiw-indexer` | Chain-ingestion boundary: `ChainSource` trait, checkpoints, JSON fixture source |
| `poiw-classifier` | Organic/wash classification and counterparty-concentration discount (consensus-critical discipline) |
| `poiw-score` | Work-unit and identity scoring, reliability factor, demand-coupled epoch pool, capped payout allocation, maturation schedule |
| `poiw-commitment` | Deterministic epoch score-root construction |
| `poiw-cli` | `poiw-scorer` binary: run the pipeline over a fixture, print scores, pool, payouts, and root |

## Quick start

```bash
cargo test --workspace
cargo run -p poiw-cli -- fixtures/example-epoch.json 1
```

## Development gates

All three must pass before pushing:

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --lib -- -D clippy::unwrap_used -D clippy::expect_used -D clippy::panic -D warnings
```

The workspace additionally denies `arithmetic_side_effects` and
`indexing_slicing`: every amount calculation is `checked_*`, and no raw
`+ - * /` appears in scoring code.

## Status and roadmap

Implemented: the deterministic scoring pipeline (methodology v0 draft)
end to end over fixture data, with unit tests per crate.

Not yet implemented (tracked, not stubbed):

- RPC-backed `ChainSource` walking finalized blocks with reorg-safe
  checkpoints (reference pattern: the node's contract indexer).
- Challenge-task sub-budget accounting and the cold-start floor.
- Control-domain grouping beyond single identities.
- Commitment submission and challenge transactions (blocked on the PoIW
  distributor contract in the `tos` repository).

## License

To be decided before external contributions are accepted.
