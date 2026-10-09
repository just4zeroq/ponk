//! # ponk-control —— 控制面（PONK 配置事实来源 + 指派器 + 状态观测面）
//!
//! 落地 `docs/09-控制面.md`（权威设计）：
//!
//! * **§2 八个配置域**：`SubscribePlan` / `SignalRules` / `AlertRules` / `FactorRules` /
//!   `Channels` / `ProcessCapacity` / `Serialization` / `InstrumentBlocklist`；
//! * **§4 版本与发布**：域版本 / `assignment_version` / 收敛判据 / 审计与回滚
//!   （回滚 = 取历史内容**重新发布为新版本号**，`current` 绝不倒回旧号）；
//! * **§5 分片 → 进程指派**：`(venue, market, shard_idx)` 切分、承载约束
//!   `min(配置上限, register 自报)`、`scatter` 打散、`quota_share` 分摊；
//! * **§6 控制信道消费**：register / heartbeat / shutdown、兜底快照键、3×5s 失联判定；
//! * **§8 与各模块接口**：`ctl.<domain>` 通知 / `ctl.request.<instance>` 拉取 /
//!   `ctl.<node_kind>.<instance>` 应答 / `ops.<node_kind>.<instance>` 状态。
//!
//! 红线（`docs/01-architecture.md` §6）：
//! **一切配置归控制面、引擎无本地配置事实**；**星型拓扑**——一切交互经事件中心，
//! 控制面与其余节点不直连、不设第二条传输。
//!
//! ## 模块
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`store`] | §4.2 版本化存储 + 审计 + 回滚 |
//! | [`validate`] | §4.3 写入侧校验（schema + 跨域引用） |
//! | [`plane`] | §4.3 发布流程 / §4.6 拉取应答 / §6 控制信道消费 / §6.5 自身心跳 |
//! | [`assign`] | §5 切分片、承载约束、分布策略、配额分摊、版本强序 |
//! | [`state`] | §6.3 兜底快照键、§6.4 失联判定、§4.4 收敛比对 |
//! | [`schema`] | §3 八域文档结构 |
//! | [`topics`] | 03 §8 主题命名 |

pub mod assign;
pub mod auth;
pub mod catalog;
pub mod catalog_repo;
pub mod error;
pub mod fetcher;
pub mod http;
pub mod offchain;
pub mod plane;
pub mod schema;
pub mod repo;
pub mod scope;
pub mod sqlite;
pub mod state;
pub mod store;
pub mod topics;
pub mod validate;

pub use catalog::{
    changed_fields, diff_catalog, Catalog, CatalogDiff, CatalogSync, MetaError, MetadataFetcher,
    MetadataQuota, Skip, SyncOutcome, TokenBucket, UnwiredFetcher, DEFAULT_META_BURST,
    META_SYNC_INTERVAL_MS,
};
pub use assign::{
    assign, next_assignment_version, AssignRequest, AssignmentResult, Candidate, EffectiveCapacity,
    InstanceAssignment, OrderDecision, ReportedCapacity, VersionGate,
};
pub use auth::{
    ensure_bootstrap_admin, hash_password, verify_password, AuthError, BootstrapInfo,
    BootstrapMode, Claims, Jwt, NewUser, Role, UserPatch, UserRecord,
    DEFAULT_ADMIN_PASSWORD, DEFAULT_ADMIN_USERNAME, DEFAULT_JWT_TTL_SECS, JWT_ISSUER,
};
pub use error::ControlError;
pub use fetcher::{
    default_rest_bases, Dig3Fetcher, ExchangeFetcherConfig, DEFAULT_EXCHANGE_TIMEOUT_MS,
    DEFAULT_FAILURE_COOLDOWN_MS,
};
pub use offchain::{
    build_provider, parse_duration_ms, CoinDetail, CoinGecko, CoinMarketCap, MarketEntry,
    MarketIndex, MatchOutcome, OffchainConfig, OffchainOutcome, OffchainProvider, OffchainSync,
    ProviderError, UnwiredProvider, DEFAULT_COIN_DETAIL_BATCH, DEFAULT_COIN_DETAIL_MAX_RETRIES,
    DEFAULT_COIN_MARKET_PAGE_SIZE, DEFAULT_COIN_TIMEOUT_MS, OFFCHAIN_SYNC_INTERVAL_MS,
};
pub use plane::{ControlConfig, ControlPlane, Outbound};
pub use schema::*;
pub use state::{
    Convergence, InstStatus, LivenessConfig, NotConverged, OpsRegistry, SnapshotKey,
};
pub use repo::{NewNode, NewServer, NodeRecord, ServerRecord};
pub use scope::{resolve, ScopeOutcome, ScopeRules};
pub use sqlite::{PlanRow, SqliteStore};
pub use catalog_repo::{
    validate_asset_class, validate_asset_kind, AssetContractRecord, AssetFilter, AssetPatch,
    AssetRecord, AuditFilter, AuditRow, ChangeFilter, ChangeRecord, DetailAssetPatch,
    ExchangeFilter, ExchangeMarketRecord, ExchangeRecord, InstrumentFilter, InstrumentRecord,
    LightAssetPatch, NewContract, NewExchange, NewMarket, OffchainAssetRow,
    OffchainSyncRunRecord, PageOut, SyncRunFilter, SyncRunRecord, UpsertOutcome, VenueAssetFilter,
    VenueAssetRecord, ASSET_CLASSES, ASSET_KINDS, persist_catalog_round,
};
pub use offchain::fetch_market_index;
pub use store::{AuditEntry, AnyConfigStore, ConfigStore, FileConfigStore, StoredDoc};

use ponk_protocol::{Body, CtlMessage, Envelope, EventType, NodeId, ProtocolError};
use std::time::{SystemTime, UNIX_EPOCH};

/// 当前时刻（ms，03 §5.1 `received_ts` / 09 `ts` 口径）。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 构造一条 `ctl.*` 信封（命令 / 配置家族）。
pub fn ctl_envelope(
    source: impl Into<NodeId>,
    ts: u64,
    msg: CtlMessage,
) -> Result<Envelope, ProtocolError> {
    Envelope::new(EventType::Ctl, source, ts, Body::Ctl(msg))
}

/// 构造一条 `ops.*` 心跳信封（10 §5 控制信道）。
pub fn ops_envelope(
    source: impl Into<NodeId>,
    ts: u64,
    hb: ponk_protocol::OpsHeartbeat,
) -> Result<Envelope, ProtocolError> {
    Envelope::new(EventType::Ops, source, ts, Body::Ops(hb))
}
