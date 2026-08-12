//! `poiw-scorer <fixture.json> <epoch> [schedule_cap] [k_percent]`
//!
//! Runs the complete scoring pipeline over a JSON fixture and prints the
//! epoch result as JSON: per-identity scores and payouts, the organic
//! settled value, the demand-coupled pool, and the committed score root.
//!
//! The fixture source stands in for the future RPC-backed chain source;
//! everything downstream of ingestion is the real pipeline.

use std::process::ExitCode;

use serde::Serialize;

use poiw_classifier::ClassifierParams;
use poiw_commitment::{score_root, ScoreEntry};
use poiw_indexer::{ChainSource, FixtureSource};
use poiw_score::{allocate_pool, epoch_pool, score_epoch, ScoreParams};
use poiw_types::EpochId;

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("usage: poiw-scorer <fixture.json> <epoch> [schedule_cap] [k_percent]")]
    Usage,
    #[error("cannot read fixture: {0}")]
    Io(#[from] std::io::Error),
    #[error("fixture error: {0}")]
    Fixture(#[from] poiw_indexer::FixtureError),
    #[error("scoring error: {0}")]
    Score(#[from] poiw_types::PoiwError),
    #[error("commitment error: {0}")]
    Commitment(#[from] poiw_commitment::CommitmentError),
    #[error("output encoding error: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("invalid numeric argument: {0}")]
    BadNumber(String),
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
    organic_settled_value: u128,
    schedule_cap: u128,
    k_percent: u32,
    pool: u128,
    score_root_hex: String,
    entries: Vec<PayoutLine>,
}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn parse_u128(text: &str) -> Result<u128, CliError> {
    text.parse()
        .map_err(|_| CliError::BadNumber(text.to_owned()))
}

fn parse_u32(text: &str) -> Result<u32, CliError> {
    text.parse()
        .map_err(|_| CliError::BadNumber(text.to_owned()))
}

fn run() -> Result<(), CliError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (fixture_path, epoch_text) = match args.as_slice() {
        [fixture, epoch, ..] => (fixture, epoch),
        _ => return Err(CliError::Usage),
    };
    let epoch = EpochId(
        epoch_text
            .parse()
            .map_err(|_| CliError::BadNumber(epoch_text.clone()))?,
    );
    let schedule_cap = match args.get(2) {
        Some(text) => parse_u128(text)?,
        None => 1_170_000_000_000_000, // ~1.17M TOS in nanotos: draft per-epoch ceiling
    };
    let k_percent = match args.get(3) {
        Some(text) => parse_u32(text)?,
        None => 300, // bootstrap-phase k = 3.0
    };

    let json = std::fs::read_to_string(fixture_path)?;
    let source = FixtureSource::from_json(&json)?;
    let data = source.epoch_data(epoch)?;

    let scores = score_epoch(
        &data.units,
        &data.reliability_map(),
        &ClassifierParams::default(),
    )?;
    let pool = epoch_pool(schedule_cap, k_percent, scores.organic_settled_value)?;
    let payouts = allocate_pool(pool, &scores.scores, &ScoreParams::default())?;

    let entries: Vec<ScoreEntry> = scores
        .scores
        .iter()
        .map(|s| ScoreEntry {
            identity: s.identity,
            score: s.score,
        })
        .collect();
    let root = score_root(&entries)?;

    let lines = scores
        .scores
        .iter()
        .zip(payouts.iter())
        .map(|(score, payout)| PayoutLine {
            identity_hex: hex32(&score.identity.0),
            score: score.score,
            payout: payout.amount,
        })
        .collect();

    let output = Output {
        epoch: epoch.0,
        organic_settled_value: scores.organic_settled_value,
        schedule_cap,
        k_percent,
        pool,
        score_root_hex: hex32(&root),
        entries: lines,
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
