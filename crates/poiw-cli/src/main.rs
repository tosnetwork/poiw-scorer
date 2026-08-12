//! `poiw-scorer <fixture.json> <epoch> [options]`
//!
//! Runs the complete scoring pipeline over a JSON fixture and prints the
//! epoch result as JSON: per-identity scores and payouts in both buckets
//! (organic and challenge), the organic settled value, the demand-coupled
//! pool, the challenge budget, and the committed score root. With
//! `--commit-out` and `--sign-seed-hex`, publishes the signed commitment
//! envelope (shadow-scoring form) via the file submitter.
//!
//! Options:
//!   --schedule-cap <nanotos>   epoch ceiling (default ~1.17M TOS)
//!   --k-percent <percent>      demand multiplier (default 300)
//!   --commit-out <directory>   write a signed commitment JSON here
//!   --sign-seed-hex <64 hex>   ed25519 seed for the commitment signature
//!
//! The fixture source stands in for the RPC-backed chain source;
//! everything downstream of ingestion is the real pipeline.

use std::collections::BTreeMap;
use std::process::ExitCode;

use serde::Serialize;

use poiw_classifier::ClassifierParams;
use poiw_commitment::{score_root, CommitmentEnvelope, FileSubmitter, ScoreEntry, Submitter};
use poiw_indexer::{ChainSource, FixtureSource};
use poiw_score::{allocate_epoch, epoch_pool, score_epoch, ChallengeBudgetParams, ScoreParams};
use poiw_types::{hex, EpochId, IdentityId};

const METHODOLOGY_VERSION: &str = "v0";

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(
        "usage: poiw-scorer <fixture.json> <epoch> [--schedule-cap N] [--k-percent N] [--commit-out DIR] [--sign-seed-hex HEX]"
    )]
    Usage,
    #[error("cannot read fixture: {0}")]
    Io(#[from] std::io::Error),
    #[error("fixture error: {0}")]
    Fixture(#[from] poiw_indexer::FixtureError),
    #[error("scoring error: {0}")]
    Score(#[from] poiw_types::PoiwError),
    #[error("commitment error: {0}")]
    Commitment(#[from] poiw_commitment::CommitmentError),
    #[error("envelope error: {0}")]
    Envelope(#[from] poiw_commitment::EnvelopeError),
    #[error("output encoding error: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("invalid argument value: {0}")]
    BadValue(String),
    #[error("--commit-out requires --sign-seed-hex")]
    MissingSeed,
}

#[derive(Serialize)]
struct PayoutLine {
    identity_hex: String,
    score: u128,
    payout: u128,
}

#[derive(Serialize)]
struct Output {
    epoch: u64,
    methodology_version: &'static str,
    organic_settled_value: u128,
    schedule_cap: u128,
    k_percent: u32,
    pool: u128,
    organic_budget: u128,
    challenge_budget: u128,
    score_root_hex: String,
    organic: Vec<PayoutLine>,
    challenge: Vec<PayoutLine>,
}

struct Args {
    fixture: String,
    epoch: u64,
    schedule_cap: u128,
    k_percent: u32,
    commit_out: Option<String>,
    sign_seed_hex: Option<String>,
}

fn parse_args(raw: &[String]) -> Result<Args, CliError> {
    let (fixture, epoch_text) = match (raw.first(), raw.get(1)) {
        (Some(fixture), Some(epoch)) if !fixture.starts_with("--") => (fixture, epoch),
        _ => return Err(CliError::Usage),
    };
    let mut args = Args {
        fixture: fixture.clone(),
        epoch: epoch_text
            .parse()
            .map_err(|_| CliError::BadValue(epoch_text.clone()))?,
        schedule_cap: 1_170_000_000_000_000, // ~1.17M TOS: draft per-epoch ceiling
        k_percent: 300,                      // bootstrap-phase k = 3.0
        commit_out: None,
        sign_seed_hex: None,
    };
    let mut index = 2;
    while let Some(flag) = raw.get(index) {
        let value = raw
            .get(index.checked_add(1).ok_or(CliError::Usage)?)
            .ok_or(CliError::Usage)?;
        match flag.as_str() {
            "--schedule-cap" => {
                args.schedule_cap = value
                    .parse()
                    .map_err(|_| CliError::BadValue(value.clone()))?;
            }
            "--k-percent" => {
                args.k_percent = value
                    .parse()
                    .map_err(|_| CliError::BadValue(value.clone()))?;
            }
            "--commit-out" => args.commit_out = Some(value.clone()),
            "--sign-seed-hex" => args.sign_seed_hex = Some(value.clone()),
            _ => return Err(CliError::Usage),
        }
        index = index.checked_add(2).ok_or(CliError::Usage)?;
    }
    Ok(args)
}

fn payout_lines(
    scores: &[poiw_score::IdentityScore],
    payouts: &[poiw_score::IdentityPayout],
) -> Vec<PayoutLine> {
    scores
        .iter()
        .zip(payouts.iter())
        .map(|(score, payout)| PayoutLine {
            identity_hex: hex::encode(&score.identity.0),
            score: score.score,
            payout: payout.amount,
        })
        .collect()
}

fn run() -> Result<(), CliError> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&raw)?;

    let json = std::fs::read_to_string(&args.fixture)?;
    let source = FixtureSource::from_json(&json)?;
    let domains = source.domain_map();
    let data = source.epoch_data(EpochId(args.epoch))?;

    let scores = score_epoch(
        &data.units,
        &data.reliability_map(),
        &domains,
        &ClassifierParams::default(),
    )?;
    let pool = epoch_pool(
        args.schedule_cap,
        args.k_percent,
        scores.organic_settled_value,
    )?;
    let allocation = allocate_epoch(
        pool,
        &scores,
        &domains,
        &ScoreParams::default(),
        &ChallengeBudgetParams::default(),
    )?;

    // The committed entry set is the merged per-identity total score
    // (organic + challenge).
    let mut merged: BTreeMap<IdentityId, u128> = BTreeMap::new();
    for score in scores.organic.iter().chain(scores.challenge.iter()) {
        let entry = merged.entry(score.identity).or_insert(0);
        *entry = entry
            .checked_add(score.score)
            .ok_or(poiw_types::PoiwError::Overflow)?;
    }
    let entries: Vec<ScoreEntry> = merged
        .iter()
        .map(|(identity, score)| ScoreEntry {
            identity: *identity,
            score: *score,
        })
        .collect();
    let root = score_root(&entries)?;
    let total_score: u128 = merged
        .values()
        .try_fold(0u128, |acc, v| acc.checked_add(*v))
        .ok_or(poiw_types::PoiwError::Overflow)?;

    if let Some(directory) = &args.commit_out {
        let seed_hex = args.sign_seed_hex.as_ref().ok_or(CliError::MissingSeed)?;
        let seed = hex::decode_array::<32>(seed_hex)
            .ok_or_else(|| CliError::BadValue(seed_hex.clone()))?;
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
        let envelope = CommitmentEnvelope {
            epoch: args.epoch,
            methodology_version: METHODOLOGY_VERSION.to_owned(),
            score_root_hex: hex::encode(&root),
            entry_count: u64::try_from(entries.len())
                .map_err(|_| CliError::BadValue("entry count".into()))?,
            total_score,
            organic_settled_value: scores.organic_settled_value,
        };
        let signed = envelope.sign(&signing_key)?;
        let submitter = FileSubmitter {
            directory: std::path::PathBuf::from(directory),
        };
        submitter.submit(&signed)?;
    }

    let output = Output {
        epoch: args.epoch,
        methodology_version: METHODOLOGY_VERSION,
        organic_settled_value: scores.organic_settled_value,
        schedule_cap: args.schedule_cap,
        k_percent: args.k_percent,
        pool,
        organic_budget: allocation.organic_budget,
        challenge_budget: allocation.challenge_budget,
        score_root_hex: hex::encode(&root),
        organic: payout_lines(&scores.organic, &allocation.organic),
        challenge: payout_lines(&scores.challenge, &allocation.challenge),
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
