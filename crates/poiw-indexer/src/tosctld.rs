//! Settled-work ingestion from a `tosctld` query API.
//!
//! [`TosctldSource`] consumes the interim phase-A data plane — the
//! authenticated `GET /poiw/settled-work` endpoint served by the node
//! repository's `tosctld` — and maps its rows onto [`SettledWorkUnit`]s
//! under the published interim mapping: the settled amount is both the
//! work valuation and its price cap, the evidence string is taken as
//! published (`Attested` for attestor-keyed settlements, `Observed`
//! otherwise), and all rows share a single default capability class
//! until the settlement-receipt schema lands.
//!
//! Epoch bucketing uses the row's `observed_at` timestamp. This is an
//! interim choice, documented in the methodology: the endpoint reports
//! the observing block's seqno but not its time, and shadow scoring
//! only ever scores past epochs. Rows that cannot be parsed are a hard
//! error, never silently skipped — two implementations must ingest
//! identical unit sets or fail identically.

use serde::Deserialize;

use poiw_types::{CapabilityClass, EvidenceLevel, IdentityId, SettledWorkUnit};

use crate::{ChainSource, EpochData};
use poiw_types::EpochId;

/// Transport boundary for the query API: fetch one URL, return parsed
/// JSON.
pub trait HttpGetJson {
    type Error: std::error::Error + Send + Sync + 'static;

    fn get_json(&self, url: &str) -> Result<serde_json::Value, Self::Error>;
}

/// Errors from the tosctld source.
#[derive(Debug, thiserror::Error)]
pub enum TosctldError<E: std::error::Error + Send + Sync + 'static> {
    #[error("transport error: {0}")]
    Transport(#[source] E),
    #[error("unexpected response shape: {0}")]
    Shape(&'static str),
    #[error("unparseable settled-work row: {0}")]
    BadRow(String),
    #[error("pagination did not converge")]
    Pagination,
}

/// Blocking HTTP getter with an optional bearer token.
#[derive(Debug, Clone)]
pub struct UreqGetter {
    agent: ureq::Agent,
    bearer: Option<String>,
}

impl UreqGetter {
    pub fn new(bearer: Option<String>) -> Self {
        Self {
            agent: ureq::AgentBuilder::new().build(),
            bearer,
        }
    }
}

/// Errors from the HTTP getter.
#[derive(Debug, thiserror::Error)]
pub enum UreqError {
    #[error("http error: {0}")]
    Http(String),
}

impl HttpGetJson for UreqGetter {
    type Error = UreqError;

    fn get_json(&self, url: &str) -> Result<serde_json::Value, Self::Error> {
        let mut request = self.agent.get(url);
        if let Some(token) = &self.bearer {
            request = request.set("Authorization", &format!("Bearer {token}"));
        }
        let response = request.call().map_err(|e| UreqError::Http(e.to_string()))?;
        response
            .into_json()
            .map_err(|e| UreqError::Http(e.to_string()))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct WireSettledWork {
    total: usize,
    result: Vec<WireEvent>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireEvent {
    address: String,
    request_id: String,
    earner: String,
    payer: String,
    amount: u64,
    evidence: String,
    observed_at: u64,
}

/// Parse a `workchain:hex64` TOS address into the 32-byte identity.
fn parse_identity(address: &str) -> Option<IdentityId> {
    let hex_part = address.rsplit(':').next()?;
    poiw_types::hex::decode_array::<32>(hex_part).map(IdentityId)
}

/// Parse an evidence-level string. All six ladder levels are accepted
/// for forward compatibility with the full receipt schema.
fn parse_evidence(text: &str) -> Option<EvidenceLevel> {
    match text {
        "Declared" => Some(EvidenceLevel::Declared),
        "Observed" => Some(EvidenceLevel::Observed),
        "Benchmarked" => Some(EvidenceLevel::Benchmarked),
        "Audited" => Some(EvidenceLevel::Audited),
        "Attested" => Some(EvidenceLevel::Attested),
        "Replicated" => Some(EvidenceLevel::Replicated),
        _ => None,
    }
}

const PAGE_LIMIT: usize = 1_000;
const MAX_PAGES: usize = 10_000;

/// A [`ChainSource`] over a `tosctld` `/poiw/settled-work` endpoint.
#[derive(Debug, Clone)]
pub struct TosctldSource<G> {
    getter: G,
    base_url: String,
    epoch_seconds: u64,
}

impl<G: HttpGetJson> TosctldSource<G> {
    /// `base_url` is the tosctld HTTP API root, without a trailing
    /// slash (for example `http://127.0.0.1:8080`).
    pub fn new(getter: G, base_url: impl Into<String>, epoch_seconds: u64) -> Self {
        Self {
            getter,
            base_url: base_url.into(),
            epoch_seconds,
        }
    }

    fn fetch_all(&self) -> Result<Vec<WireEvent>, TosctldError<G::Error>> {
        let mut events: Vec<WireEvent> = Vec::new();
        for _page in 0..MAX_PAGES {
            let url = format!(
                "{}/poiw/settled-work?offset={}&limit={PAGE_LIMIT}",
                self.base_url,
                events.len()
            );
            let value = self
                .getter
                .get_json(&url)
                .map_err(TosctldError::Transport)?;
            let page: WireSettledWork = serde_json::from_value(value)
                .map_err(|_| TosctldError::Shape("settled-work response"))?;
            let received = page.result.len();
            events.extend(page.result);
            if events.len() >= page.total || received == 0 {
                return Ok(events);
            }
        }
        Err(TosctldError::Pagination)
    }

    fn to_unit(event: &WireEvent) -> Result<SettledWorkUnit, TosctldError<G::Error>> {
        let identity = parse_identity(&event.earner)
            .ok_or_else(|| TosctldError::BadRow(format!("earner {}", event.earner)))?;
        let payer = parse_identity(&event.payer)
            .ok_or_else(|| TosctldError::BadRow(format!("payer {}", event.payer)))?;
        let evidence = parse_evidence(&event.evidence)
            .ok_or_else(|| TosctldError::BadRow(format!("evidence {}", event.evidence)))?;
        Ok(SettledWorkUnit {
            identity,
            payer,
            capability: CapabilityClass("default".to_owned()),
            rate_card_value: event.amount,
            settled_price: event.amount,
            evidence,
            is_challenge_task: false,
            payer_related: false,
        })
    }
}

impl<G: HttpGetJson> ChainSource for TosctldSource<G> {
    type Error = TosctldError<G::Error>;

    fn epoch_data(&self, epoch: EpochId) -> Result<EpochData, Self::Error> {
        let mut data = EpochData::default();
        for event in self.fetch_all()? {
            let bucket = event
                .observed_at
                .checked_div(self.epoch_seconds)
                .unwrap_or(0);
            if bucket == epoch.0 {
                // `address`/`request_id` identify the row on the wire but
                // are not part of the scoring unit itself.
                let _ = (&event.address, &event.request_id);
                data.units.push(Self::to_unit(&event)?);
            }
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use std::cell::RefCell;

    use super::*;

    fn addr(byte: u8) -> String {
        format!("0:{}", poiw_types::hex::encode(&[byte; 32]))
    }

    fn row(
        earner: u8,
        payer: u8,
        amount: u64,
        evidence: &str,
        observed_at: u64,
    ) -> serde_json::Value {
        serde_json::json!({
            "address": addr(200),
            "request_id": "",
            "kind": "task_escrow",
            "earner": addr(earner),
            "payer": addr(payer),
            "amount": amount,
            "evidence": evidence,
            "seqno": 5,
            "observed_at": observed_at,
        })
    }

    /// Serves scripted pages keyed by the requested offset.
    struct CannedGetter {
        rows: Vec<serde_json::Value>,
        page_limit: usize,
        calls: RefCell<usize>,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("no response")]
    struct NoResponse;

    impl HttpGetJson for &CannedGetter {
        type Error = NoResponse;

        fn get_json(&self, url: &str) -> Result<serde_json::Value, Self::Error> {
            *self.calls.borrow_mut() += 1;
            let offset: usize = url
                .split("offset=")
                .nth(1)
                .and_then(|rest| rest.split('&').next())
                .and_then(|text| text.parse().ok())
                .ok_or(NoResponse)?;
            let page: Vec<serde_json::Value> = self
                .rows
                .iter()
                .skip(offset)
                .take(self.page_limit)
                .cloned()
                .collect();
            Ok(serde_json::json!({
                "ok": true,
                "total": self.rows.len(),
                "offset": offset,
                "limit": self.page_limit,
                "result": page,
            }))
        }
    }

    #[test]
    fn maps_rows_to_units_and_buckets_by_observed_time() {
        let getter = CannedGetter {
            rows: vec![
                row(1, 10, 500, "Observed", 1_500),
                row(2, 10, 700, "Attested", 1_600),
                row(3, 10, 900, "Observed", 2_500),
            ],
            page_limit: 1_000,
            calls: RefCell::new(0),
        };
        let source = TosctldSource::new(&getter, "http://localhost:8080", 1_000);
        let epoch1 = source.epoch_data(EpochId(1)).unwrap();
        assert_eq!(epoch1.units.len(), 2);
        assert_eq!(epoch1.units[0].identity, IdentityId([1; 32]));
        assert_eq!(epoch1.units[0].rate_card_value, 500);
        assert_eq!(epoch1.units[0].settled_price, 500);
        assert_eq!(epoch1.units[0].evidence, EvidenceLevel::Observed);
        assert_eq!(epoch1.units[1].evidence, EvidenceLevel::Attested);
        let epoch2 = source.epoch_data(EpochId(2)).unwrap();
        assert_eq!(epoch2.units.len(), 1);
        assert_eq!(epoch2.units[0].identity, IdentityId([3; 32]));
    }

    #[test]
    fn paginates_until_the_reported_total() {
        let rows: Vec<serde_json::Value> =
            (0..5).map(|i| row(i, 10, 100, "Observed", 1_100)).collect();
        let getter = CannedGetter {
            rows,
            page_limit: 2,
            calls: RefCell::new(0),
        };
        let source = TosctldSource::new(&getter, "http://localhost:8080", 1_000);
        let data = source.epoch_data(EpochId(1)).unwrap();
        assert_eq!(data.units.len(), 5);
        assert_eq!(*getter.calls.borrow(), 3); // pages of 2, 2, 1
    }

    #[test]
    fn bad_rows_are_a_hard_error_not_a_skip() {
        let mut bad = row(1, 10, 100, "Observed", 1_100);
        bad["evidence"] = serde_json::json!("Rumored");
        let getter = CannedGetter {
            rows: vec![bad],
            page_limit: 1_000,
            calls: RefCell::new(0),
        };
        let source = TosctldSource::new(&getter, "http://localhost:8080", 1_000);
        assert!(matches!(
            source.epoch_data(EpochId(1)),
            Err(TosctldError::BadRow(_))
        ));

        let mut bad_addr = row(1, 10, 100, "Observed", 1_100);
        bad_addr["earner"] = serde_json::json!("0:nothex");
        let getter = CannedGetter {
            rows: vec![bad_addr],
            page_limit: 1_000,
            calls: RefCell::new(0),
        };
        let source = TosctldSource::new(&getter, "http://localhost:8080", 1_000);
        assert!(matches!(
            source.epoch_data(EpochId(1)),
            Err(TosctldError::BadRow(_))
        ));
    }
}
