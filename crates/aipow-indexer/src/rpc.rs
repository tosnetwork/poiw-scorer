//! RPC-backed chain ingestion.
//!
//! [`RpcChainSource`] walks finalized masterchain blocks through any
//! [`ChainRpc`] implementation with reorg-safe checkpoints, following
//! the block-walking discipline of the node's contract indexer: record
//! the hash of the last scanned block, re-verify it before advancing,
//! and rewind a fixed safety margin on a mismatch.
//!
//! [`JsonRpcChainRpc`] maps the trait onto the node's JSON-RPC 2.0
//! endpoint. Wire mapping status:
//!
//! - `getMasterchainInfo`, `lookupBlock`, `getBlockHeader` follow the
//!   node's existing public method surface and must be verified against
//!   a localnet before phase A sign-off;
//! - `aipowGetSettledWork` is the **pending node-side extension** that
//!   returns settled work units per block. Per the node's JSON-RPC
//!   policy, new capabilities are added as explicit new methods; this
//!   client defines the consuming side of that contract.

use std::collections::BTreeMap;

use serde::Deserialize;

use aipow_types::{EpochId, SettledWorkUnit};

use crate::{ChainSource, EpochData};

/// A block reference: seqno plus root hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRef {
    pub seqno: u64,
    pub hash: [u8; 32],
}

/// Block metadata needed for epoch bucketing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockMeta {
    pub block: BlockRef,
    pub unix_time: u64,
}

/// The minimal chain surface the walker needs.
pub trait ChainRpc {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Current masterchain head.
    fn head(&self) -> Result<BlockRef, Self::Error>;
    /// Metadata of the block at `seqno` on the current chain.
    fn block_meta(&self, seqno: u64) -> Result<BlockMeta, Self::Error>;
    /// Settled work units recorded in the block at `seqno`.
    fn settled_units(&self, seqno: u64) -> Result<Vec<SettledWorkUnit>, Self::Error>;
}

/// Walker configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkerConfig {
    /// Blocks below `head - confirmations` are treated as finalized.
    pub confirmations: u64,
    /// How far to rewind when the checkpoint hash no longer matches.
    pub reorg_margin: u64,
    /// Upper bound on blocks ingested per [`RpcChainSource::tick`].
    pub max_blocks_per_tick: u64,
    /// Epoch length in seconds (the 65,536-second validation epoch).
    pub epoch_seconds: u64,
    /// First seqno to scan when starting from an empty checkpoint.
    pub start_seqno: u64,
}

impl Default for WalkerConfig {
    fn default() -> Self {
        Self {
            confirmations: 16,
            reorg_margin: 5,
            max_blocks_per_tick: 256,
            epoch_seconds: 65_536,
            start_seqno: 1,
        }
    }
}

/// Walker errors.
#[derive(Debug, thiserror::Error)]
pub enum SourceError<E: std::error::Error + Send + Sync + 'static> {
    #[error("rpc error: {0}")]
    Rpc(#[source] E),
    #[error("invalid walker configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("epoch {0} is not fully covered by finalized scanned blocks yet")]
    IncompleteEpoch(u64),
}

/// Progress report from one walker tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickProgress {
    pub blocks_scanned: u64,
    pub reorg_detected: bool,
    pub finalized_head: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScannedBlock {
    hash: [u8; 32],
    unix_time: u64,
    units: Vec<SettledWorkUnit>,
}

/// A [`ChainSource`] that ingests settled work from finalized blocks
/// over RPC, bucketing units into epochs by block time.
#[derive(Debug)]
pub struct RpcChainSource<R: ChainRpc> {
    rpc: R,
    config: WalkerConfig,
    checkpoint: Option<BlockRef>,
    blocks: BTreeMap<u64, ScannedBlock>,
}

impl<R: ChainRpc> RpcChainSource<R> {
    pub fn new(rpc: R, config: WalkerConfig) -> Result<Self, SourceError<R::Error>> {
        if config.epoch_seconds == 0 {
            return Err(SourceError::InvalidConfig("epoch_seconds must be positive"));
        }
        if config.max_blocks_per_tick == 0 {
            return Err(SourceError::InvalidConfig(
                "max_blocks_per_tick must be positive",
            ));
        }
        Ok(Self {
            rpc,
            config,
            checkpoint: None,
            blocks: BTreeMap::new(),
        })
    }

    pub fn checkpoint(&self) -> Option<BlockRef> {
        self.checkpoint
    }

    fn epoch_of(&self, unix_time: u64) -> u64 {
        unix_time
            .checked_div(self.config.epoch_seconds)
            .unwrap_or(0)
    }

    /// Verify the checkpoint against the live chain; on a hash mismatch
    /// rewind `reorg_margin` blocks and drop everything scanned above
    /// the rewound checkpoint so it is re-ingested from the new branch.
    fn verify_checkpoint(&mut self) -> Result<bool, SourceError<R::Error>> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(false);
        };
        let live = self
            .rpc
            .block_meta(checkpoint.seqno)
            .map_err(SourceError::Rpc)?;
        if live.block.hash == checkpoint.hash {
            return Ok(false);
        }
        let rewound_seqno = checkpoint.seqno.saturating_sub(self.config.reorg_margin);
        self.blocks.retain(|seqno, _| *seqno < rewound_seqno);
        self.checkpoint = self
            .blocks
            .get(&rewound_seqno.saturating_sub(1))
            .map(|scanned| BlockRef {
                seqno: rewound_seqno.saturating_sub(1),
                hash: scanned.hash,
            });
        Ok(true)
    }

    /// Advance toward the finalized head, ingesting at most
    /// `max_blocks_per_tick` blocks. Safe to call repeatedly; each call
    /// resumes from the stored checkpoint.
    pub fn tick(&mut self) -> Result<TickProgress, SourceError<R::Error>> {
        let reorg_detected = self.verify_checkpoint()?;
        let head = self.rpc.head().map_err(SourceError::Rpc)?;
        let finalized = head.seqno.saturating_sub(self.config.confirmations);

        let mut scanned = 0u64;
        let mut next = match self.checkpoint {
            Some(checkpoint) => checkpoint.seqno.saturating_add(1),
            None => self.config.start_seqno,
        };
        while next <= finalized && scanned < self.config.max_blocks_per_tick {
            let meta = self.rpc.block_meta(next).map_err(SourceError::Rpc)?;
            let units = self.rpc.settled_units(next).map_err(SourceError::Rpc)?;
            self.blocks.insert(
                next,
                ScannedBlock {
                    hash: meta.block.hash,
                    unix_time: meta.unix_time,
                    units,
                },
            );
            self.checkpoint = Some(meta.block);
            scanned = scanned.saturating_add(1);
            next = next.saturating_add(1);
        }
        Ok(TickProgress {
            blocks_scanned: scanned,
            reorg_detected,
            finalized_head: finalized,
        })
    }

    /// An epoch is complete once a finalized scanned block's time has
    /// passed the epoch's end.
    fn epoch_complete(&self, epoch: u64) -> bool {
        let Some(end) = epoch
            .checked_add(1)
            .and_then(|e| e.checked_mul(self.config.epoch_seconds))
        else {
            return false;
        };
        self.blocks
            .values()
            .next_back()
            .is_some_and(|scanned| scanned.unix_time >= end)
    }
}

impl<R: ChainRpc> ChainSource for RpcChainSource<R> {
    type Error = SourceError<R::Error>;

    fn epoch_data(&self, epoch: EpochId) -> Result<EpochData, Self::Error> {
        if !self.epoch_complete(epoch.0) {
            return Err(SourceError::IncompleteEpoch(epoch.0));
        }
        let mut data = EpochData::default();
        for scanned in self.blocks.values() {
            if self.epoch_of(scanned.unix_time) == epoch.0 {
                data.units.extend(scanned.units.iter().cloned());
            }
        }
        // Reliability inputs are part of the pending node-side method
        // surface; until then identities score at the neutral factor.
        Ok(data)
    }
}

/// JSON-RPC 2.0 transport boundary.
pub trait JsonRpcTransport {
    type Error: std::error::Error + Send + Sync + 'static;

    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Self::Error>;
}

/// Errors from the HTTP JSON-RPC transport.
#[derive(Debug, thiserror::Error)]
pub enum HttpRpcError {
    #[error("http error: {0}")]
    Http(String),
    #[error("json-rpc error response: {0}")]
    Server(String),
    #[error("malformed json-rpc response")]
    Malformed,
}

/// Blocking HTTP JSON-RPC 2.0 client (`POST` with a single request
/// object), matching the node's JSON-RPC policy.
#[derive(Debug, Clone)]
pub struct HttpJsonRpc {
    url: String,
    agent: ureq::Agent,
}

impl HttpJsonRpc {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            agent: ureq::AgentBuilder::new().build(),
        }
    }
}

impl JsonRpcTransport for HttpJsonRpc {
    type Error = HttpRpcError;

    fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Self::Error> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let response = self
            .agent
            .post(&self.url)
            .send_json(body)
            .map_err(|e| HttpRpcError::Http(e.to_string()))?;
        let value: serde_json::Value = response
            .into_json()
            .map_err(|e| HttpRpcError::Http(e.to_string()))?;
        if let Some(error) = value.get("error") {
            return Err(HttpRpcError::Server(error.to_string()));
        }
        value.get("result").cloned().ok_or(HttpRpcError::Malformed)
    }
}

/// Errors from the JSON-RPC chain adapter.
#[derive(Debug, thiserror::Error)]
pub enum JsonRpcChainError<E: std::error::Error + Send + Sync + 'static> {
    #[error("transport error: {0}")]
    Transport(#[source] E),
    #[error("unexpected response shape for {0}")]
    Shape(&'static str),
    #[error("unparseable block hash")]
    BadHash,
}

/// Decode a 32-byte hash given as 64 hex characters or 44 base64
/// characters.
fn parse_hash(text: &str) -> Option<[u8; 32]> {
    if text.len() == 64 {
        return aipow_types::hex::decode_array::<32>(text);
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()
        .or_else(|| base64::engine::general_purpose::URL_SAFE.decode(text).ok())?;
    bytes.try_into().ok()
}

#[derive(Deserialize)]
struct WireBlockId {
    seqno: u64,
    root_hash: String,
}

#[derive(Deserialize)]
struct WireMasterchainInfo {
    last: WireBlockId,
}

#[derive(Deserialize)]
struct WireLookupBlock {
    id: WireBlockId,
}

#[derive(Deserialize)]
struct WireBlockHeader {
    gen_utime: u64,
}

#[derive(Deserialize)]
struct WireSettledWork {
    units: Vec<SettledWorkUnit>,
}

/// [`ChainRpc`] over a JSON-RPC transport.
#[derive(Debug, Clone)]
pub struct JsonRpcChainRpc<T> {
    transport: T,
    /// Masterchain workchain id used in lookups.
    pub workchain: i32,
    /// Masterchain shard id used in lookups.
    pub shard: i64,
}

impl<T> JsonRpcChainRpc<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            workchain: -1,
            shard: i64::MIN,
        }
    }
}

impl<T: JsonRpcTransport> JsonRpcChainRpc<T> {
    fn block_ref(id: &WireBlockId) -> Result<BlockRef, JsonRpcChainError<T::Error>> {
        let hash = parse_hash(&id.root_hash).ok_or(JsonRpcChainError::BadHash)?;
        Ok(BlockRef {
            seqno: id.seqno,
            hash,
        })
    }
}

impl<T: JsonRpcTransport> ChainRpc for JsonRpcChainRpc<T> {
    type Error = JsonRpcChainError<T::Error>;

    fn head(&self) -> Result<BlockRef, Self::Error> {
        let result = self
            .transport
            .call("getMasterchainInfo", serde_json::json!({}))
            .map_err(JsonRpcChainError::Transport)?;
        let info: WireMasterchainInfo = serde_json::from_value(result)
            .map_err(|_| JsonRpcChainError::Shape("getMasterchainInfo"))?;
        Self::block_ref(&info.last)
    }

    fn block_meta(&self, seqno: u64) -> Result<BlockMeta, Self::Error> {
        let lookup = self
            .transport
            .call(
                "lookupBlock",
                serde_json::json!({
                    "workchain": self.workchain,
                    "shard": self.shard,
                    "seqno": seqno,
                }),
            )
            .map_err(JsonRpcChainError::Transport)?;
        let looked: WireLookupBlock =
            serde_json::from_value(lookup).map_err(|_| JsonRpcChainError::Shape("lookupBlock"))?;
        let block = Self::block_ref(&looked.id)?;
        let header = self
            .transport
            .call(
                "getBlockHeader",
                serde_json::json!({
                    "workchain": self.workchain,
                    "shard": self.shard,
                    "seqno": seqno,
                    "root_hash": looked.id.root_hash,
                }),
            )
            .map_err(JsonRpcChainError::Transport)?;
        let parsed: WireBlockHeader = serde_json::from_value(header)
            .map_err(|_| JsonRpcChainError::Shape("getBlockHeader"))?;
        Ok(BlockMeta {
            block,
            unix_time: parsed.gen_utime,
        })
    }

    fn settled_units(&self, seqno: u64) -> Result<Vec<SettledWorkUnit>, Self::Error> {
        let result = self
            .transport
            .call(
                "aipowGetSettledWork",
                serde_json::json!({
                    "workchain": self.workchain,
                    "shard": self.shard,
                    "seqno": seqno,
                }),
            )
            .map_err(JsonRpcChainError::Transport)?;
        let parsed: WireSettledWork = serde_json::from_value(result)
            .map_err(|_| JsonRpcChainError::Shape("aipowGetSettledWork"))?;
        Ok(parsed.units)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use std::cell::RefCell;

    use aipow_types::{CapabilityClass, EvidenceLevel, IdentityId};

    use super::*;

    fn unit(earner: u8, seq: u8) -> SettledWorkUnit {
        SettledWorkUnit {
            identity: IdentityId([earner; 32]),
            payer: IdentityId([seq; 32]),
            capability: CapabilityClass("embedding".into()),
            rate_card_value: 100,
            settled_price: 100,
            evidence: EvidenceLevel::Observed,
            is_challenge_task: false,
            payer_related: false,
        }
    }

    /// Per-seqno scripted block: (hash tag, unix time, units).
    type ScriptedBlock = (u8, u64, Vec<SettledWorkUnit>);

    /// A scripted chain, mutable so tests can inject a reorg between
    /// ticks.
    struct FakeChain {
        blocks: RefCell<BTreeMap<u64, ScriptedBlock>>,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("missing block {0}")]
    struct MissingBlock(u64);

    impl ChainRpc for &FakeChain {
        type Error = MissingBlock;

        fn head(&self) -> Result<BlockRef, Self::Error> {
            let blocks = self.blocks.borrow();
            let (seqno, (tag, _, _)) = blocks.iter().next_back().ok_or(MissingBlock(0))?;
            Ok(BlockRef {
                seqno: *seqno,
                hash: [*tag; 32],
            })
        }

        fn block_meta(&self, seqno: u64) -> Result<BlockMeta, Self::Error> {
            let blocks = self.blocks.borrow();
            let (tag, time, _) = blocks.get(&seqno).ok_or(MissingBlock(seqno))?;
            Ok(BlockMeta {
                block: BlockRef {
                    seqno,
                    hash: [*tag; 32],
                },
                unix_time: *time,
            })
        }

        fn settled_units(&self, seqno: u64) -> Result<Vec<SettledWorkUnit>, Self::Error> {
            let blocks = self.blocks.borrow();
            let (_, _, units) = blocks.get(&seqno).ok_or(MissingBlock(seqno))?;
            Ok(units.clone())
        }
    }

    fn chain_with(range: std::ops::RangeInclusive<u64>, tag: u8) -> FakeChain {
        let mut blocks = BTreeMap::new();
        for seqno in range {
            let units = if seqno.is_multiple_of(3) {
                vec![unit(u8::try_from(seqno % 200).unwrap(), 9)]
            } else {
                Vec::new()
            };
            blocks.insert(seqno, (tag, seqno.checked_mul(100).unwrap(), units));
        }
        FakeChain {
            blocks: RefCell::new(blocks),
        }
    }

    fn config() -> WalkerConfig {
        WalkerConfig {
            confirmations: 2,
            reorg_margin: 5,
            max_blocks_per_tick: 1_000,
            epoch_seconds: 1_000,
            start_seqno: 1,
        }
    }

    #[test]
    fn walker_advances_to_finalized_head_and_buckets_epochs() {
        let chain = chain_with(1..=40, 1);
        let mut source = RpcChainSource::new(&chain, config()).unwrap();
        let progress = source.tick().unwrap();
        assert_eq!(progress.finalized_head, 38);
        assert_eq!(progress.blocks_scanned, 38);
        assert!(!progress.reorg_detected);
        assert_eq!(source.checkpoint().unwrap().seqno, 38);

        // Epoch 0 covers times 0..1000 -> seqnos 1..=9; epoch 1 covers
        // 1000..2000 -> seqnos 10..=19. Blocks at multiples of 3 carry
        // one unit each.
        let epoch0 = source.epoch_data(EpochId(0)).unwrap();
        assert_eq!(epoch0.units.len(), 3); // seqnos 3, 6, 9
        let epoch1 = source.epoch_data(EpochId(1)).unwrap();
        assert_eq!(epoch1.units.len(), 3); // seqnos 12, 15, 18

        // Epoch 3 ends at t=4000, beyond the last finalized block time.
        assert!(matches!(
            source.epoch_data(EpochId(3)),
            Err(SourceError::IncompleteEpoch(3))
        ));
    }

    #[test]
    fn walker_bounds_blocks_per_tick_and_resumes() {
        let chain = chain_with(1..=40, 1);
        let mut cfg = config();
        cfg.max_blocks_per_tick = 10;
        let mut source = RpcChainSource::new(&chain, cfg).unwrap();
        assert_eq!(source.tick().unwrap().blocks_scanned, 10);
        assert_eq!(source.checkpoint().unwrap().seqno, 10);
        assert_eq!(source.tick().unwrap().blocks_scanned, 10);
        assert_eq!(source.tick().unwrap().blocks_scanned, 10);
        assert_eq!(source.tick().unwrap().blocks_scanned, 8);
        assert_eq!(source.tick().unwrap().blocks_scanned, 0);
        assert_eq!(source.checkpoint().unwrap().seqno, 38);
    }

    #[test]
    fn walker_rewinds_on_reorg_and_reingests_the_new_branch() {
        let chain = chain_with(1..=40, 1);
        let mut source = RpcChainSource::new(&chain, config()).unwrap();
        source.tick().unwrap();
        // Epoch 2 covers times 2000..3000 -> seqnos 20..=29; unit-bearing
        // seqnos are 21, 24, 27.
        let epoch2_before = source.epoch_data(EpochId(2)).unwrap();
        assert_eq!(epoch2_before.units.len(), 3);

        // Reorg: replace blocks >= 30 with a new branch (tag 2) whose
        // unit-bearing blocks differ (even seqnos), and extend the head.
        {
            let mut blocks = chain.blocks.borrow_mut();
            for seqno in 30..=44u64 {
                let units = if seqno.is_multiple_of(2) {
                    vec![unit(u8::try_from(seqno % 200).unwrap(), 9)]
                } else {
                    Vec::new()
                };
                blocks.insert(seqno, (2, seqno.checked_mul(100).unwrap(), units));
            }
        }

        let progress = source.tick().unwrap();
        assert!(progress.reorg_detected);
        // Checkpoint was 38; rewound by the 5-block margin to 33 and
        // rescanned 33..=42 from the new branch.
        assert_eq!(source.checkpoint().unwrap().seqno, 42);

        // Epoch 2 is entirely below the rewind point and unchanged.
        let epoch2_after = source.epoch_data(EpochId(2)).unwrap();
        assert_eq!(epoch2_after, epoch2_before);

        // Epoch 3 covers seqnos 30..=39. Blocks 30..=32 predate the
        // rewind point, so the old branch's unit at seqno 30 remains —
        // the reorg margin bounds, but does not eliminate, stale data
        // (the same documented semantics as the node's contract
        // indexer). Rescanned 33..=39 carry new-branch units at 34, 36,
        // 38.
        let epoch3 = source.epoch_data(EpochId(3)).unwrap();
        assert_eq!(epoch3.units.len(), 4);
    }

    /// Canned JSON-RPC transport.
    struct FakeTransport;

    #[derive(Debug, thiserror::Error)]
    #[error("no such method")]
    struct NoMethod;

    impl JsonRpcTransport for FakeTransport {
        type Error = NoMethod;

        fn call(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, Self::Error> {
            let hash_hex = aipow_types::hex::encode(&[7u8; 32]);
            match method {
                "getMasterchainInfo" => Ok(serde_json::json!({
                    "last": {"seqno": 99, "root_hash": hash_hex}
                })),
                "lookupBlock" => Ok(serde_json::json!({
                    "id": {"seqno": 42, "root_hash": hash_hex}
                })),
                "getBlockHeader" => Ok(serde_json::json!({"gen_utime": 123456})),
                "aipowGetSettledWork" => {
                    let mut sample = unit(1, 2);
                    sample.settled_price = 90;
                    let value = serde_json::to_value(vec![sample]).map_err(|_| NoMethod)?;
                    Ok(serde_json::json!({ "units": value }))
                }
                _ => Err(NoMethod),
            }
        }
    }

    #[test]
    fn json_rpc_adapter_parses_the_wire_shapes() {
        let rpc = JsonRpcChainRpc::new(FakeTransport);
        let head = rpc.head().unwrap();
        assert_eq!(head.seqno, 99);
        assert_eq!(head.hash, [7u8; 32]);
        let meta = rpc.block_meta(42).unwrap();
        assert_eq!(meta.block.seqno, 42);
        assert_eq!(meta.unix_time, 123_456);
        let units = rpc.settled_units(42).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].settled_price, 90);
    }

    #[test]
    fn hash_parsing_accepts_hex_and_base64_rejects_garbage() {
        let hex64 = aipow_types::hex::encode(&[9u8; 32]);
        assert_eq!(parse_hash(&hex64).unwrap(), [9u8; 32]);
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        assert_eq!(parse_hash(&b64).unwrap(), [9u8; 32]);
        assert!(parse_hash("nope").is_none());
    }
}
