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

Implemented (methodology v0 draft, end to end, unit-tested per crate):

- the deterministic two-bucket scoring pipeline (organic + challenge)
  with evidence multipliers, reliability factors, and control-domain
  grouping;
- demand-coupled epoch pool, triple-bounded challenge budget with the
  additive cold-start floor, per-domain payout caps with proportional
  scale-down, and per-identity challenge caps;
- the reorg-safe block walker (`rpc::RpcChainSource`) over the
  `rpc::ChainRpc` boundary, with the JSON-RPC adapter and HTTP
  transport;
- signed commitment envelopes (`poiw-commit-v0` digest, ed25519) with
  file-based publication for the shadow-scoring phase.

Remaining node-side integration (owned by the `tos` repository, tracked
in the methodology's open items):

- the `poiwGetSettledWork` JSON-RPC method serving settled work units
  per block (the wire adapter here defines the consuming side);
- wire-mapping verification of `getMasterchainInfo` / `lookupBlock` /
  `getBlockHeader` against a localnet before phase A sign-off;
- the PoIW distributor contract, at which point an on-chain `Submitter`
  joins the file submitter.

## License

GPL-3.0, aligned with the TOS node repository. See [LICENSE](LICENSE).
