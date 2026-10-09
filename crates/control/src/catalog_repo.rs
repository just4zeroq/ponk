//! 目录七表的读写（`docs/12-控制面数据库.md` §3.1–§3.4 + §3.7 全局审计）。
//!
//! 覆盖 `exchange` / `exchange_market` / `asset` / `venue_asset` / `instrument` /
//! `instrument_change` / `sync_run`，外加跨域全表 `audit_log` 查询——
//! 即 `docs/13 §6.2` 里标 **「新增」** 的那七行（§7.2 R1 的风险清单）。
//!
//! ## 三条硬约束
//!
//! 1. **主键即 canonical**（12 §3.3）：`(venue, market, native_code)` 不另造 id；
//! 2. **`row_hash` 变更检测**（12 §4.1 / §4.2）：规范化与 `event_id` 同源
//!    （`canonical_view` + `hash_text`），**不含任何时间戳**——
//!    哈希相同 ⇒ **只更新 `last_synced_at`（无变更零动作）**，
//!    哈希不同 ⇒ 写 `instrument_change`（列名 `before_v` / `after_v`，见 §7.2 R7）；
//! 3. **`dig3_account_type` 硬映射**（02 §6）：`swap → FuturesCross`（不是
//!    `FuturesIsolated`——写错连不上）。
//!
//! ## 与 `catalog.rs` 的分工
//!
//! 比对 / 触发语义（`diff_catalog`、`triggers_reassignment`、零动作门槛）仍在
//! [`crate::catalog`]；本模块只负责**把事实落进 12 §3 的表**。
//! 落库入口是 [`SqliteStore::persist_catalog_round`]，由 `CatalogSync` 每轮调用。

use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::catalog::{changed_fields, Catalog, CatalogDiff};
use crate::error::ControlError;
use crate::sqlite::SqliteStore;

// ─────────────────────────────────────────────────────────────────────────────
// §4.1 分页
// ─────────────────────────────────────────────────────────────────────────────

/// §4.1：`?limit=` 默认 50、最大 500。
pub const DEFAULT_LIMIT: usize = 50;
/// §4.1：上限。
pub const MAX_LIMIT: usize = 500;

/// 分页结果（组装 `{items,total,limit,offset}` 用）。
#[derive(Debug, Clone, PartialEq)]
pub struct PageOut<T> {
    /// 本页数据。
    pub items: Vec<T>,
    /// 筛选后的总数。
    pub total: usize,
    /// 生效的 limit。
    pub limit: usize,
    /// 生效的 offset。
    pub offset: usize,
}

/// 归一化 `limit` / `offset` 后切片。
pub fn paginate<T>(mut all: Vec<T>, limit: Option<usize>, offset: Option<usize>) -> PageOut<T> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = offset.unwrap_or(0);
    let total = all.len();
    let items = if offset >= total {
        Vec::new()
    } else {
        let end = (offset + limit).min(total);
        all.drain(offset..end).collect()
    };
    PageOut {
        items,
        total,
        limit,
        offset,
    }
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    needle.is_empty() || hay.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())
}

fn matches_q(fields: &[&str], q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    fields.iter().any(|f| contains_ci(f, q))
}

/// `?sort=` 允许字段白名单 + 升降序；不在白名单内 → 取 `default`。
fn order_of(sort: Option<&str>, order: Option<&str>, allowed: &[&str], default: &str) -> (String, bool) {
    let field = sort
        .map(str::to_ascii_lowercase)
        .filter(|s| allowed.contains(&s.as_str()))
        .unwrap_or_else(|| default.to_string());
    (field, order.map(|o| o.eq_ignore_ascii_case("desc")).unwrap_or(default_sort_desc(default)))
}

fn default_sort_desc(default: &str) -> bool {
    // 时间类字段默认倒序（最新在前），标识类字段默认正序。
    matches!(default, "ts" | "last_synced_at" | "detected_at" | "started_at" | "updated_at")
}

// ─────────────────────────────────────────────────────────────────────────────
// 记录类型
// ─────────────────────────────────────────────────────────────────────────────

/// 交易所行（`docs/12 §3.1`，`docs/13 §4.6`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeRecord {
    /// 主键。
    pub venue: String,
    /// 展示名。
    pub name: String,
    /// dig3 `ExchangeId`（必须能映射，否则采集起不来）。
    pub dig3_exchange_id: String,
    /// 启停。
    pub enabled: bool,
    /// 资产类（`docs/12 §3.1`）：`crypto` / `stock` / `forex` / `prediction`；
    /// **`stock` → 该所交易对跳过链上币信息**（`docs/14 §6`）。
    pub asset_class: String,
    /// 所方开放字段。
    pub extra: Value,
    pub created_at: u64,
    pub updated_at: u64,
    /// 关联市场数（§4.6「交易所行 + 市场数」）。
    pub market_count: i64,
}

/// 交易所市场行（`docs/12 §3.1`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeMarketRecord {
    /// 所。
    pub venue: String,
    /// 市场（spot / swap / future，02 §3）。
    pub market: String,
    /// dig3 `AccountType`（02 §6 硬映射；**无默认值**，运维显式填）。
    pub dig3_account_type: String,
    pub enabled: bool,
    pub quota_collect: f64,
    pub quota_backfill: f64,
    pub quota_metadata: f64,
    pub egress_ip: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 新建 / 更新交易所的入参。
#[derive(Debug, Clone, PartialEq)]
pub struct NewExchange {
    /// 主键。
    pub venue: String,
    /// 展示名。
    pub name: String,
    /// dig3 `ExchangeId`。
    pub dig3_exchange_id: String,
    /// 启停（缺省开）。
    pub enabled: bool,
    /// 资产类（缺省 `crypto`）。
    pub asset_class: String,
    /// 所方开放字段。
    pub extra: Value,
}

impl Default for NewExchange {
    fn default() -> Self {
        NewExchange {
            venue: String::new(),
            name: String::new(),
            dig3_exchange_id: String::new(),
            enabled: true,
            asset_class: "crypto".to_string(),
            extra: json!({}),
        }
    }
}

/// 新建 / 更新市场的入参。
#[derive(Debug, Clone, PartialEq)]
pub struct NewMarket {
    /// 所。
    pub venue: String,
    /// 市场。
    pub market: String,
    /// dig3 `AccountType`。
    pub dig3_account_type: String,
    /// 启停（缺省开）。
    pub enabled: bool,
    /// 采集份额（02 §6 ~50%）。
    pub quota_collect: f64,
    /// 回填份额（~35%）。
    pub quota_backfill: f64,
    /// 元数据份额（~15%）。
    pub quota_metadata: f64,
    /// 出口 IP（配额按 (venue, 出口 IP) 切）。
    pub egress_ip: Option<String>,
}

impl Default for NewMarket {
    fn default() -> Self {
        NewMarket {
            venue: String::new(),
            market: String::new(),
            dig3_account_type: String::new(),
            enabled: true,
            quota_collect: 0.50,
            quota_backfill: 0.35,
            quota_metadata: 0.15,
            egress_ip: None,
        }
    }
}

/// 全局资产行（`docs/12 §3.2` 的 `asset`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetRecord {
    /// 自增 id。
    pub id: i64,
    /// 代码（BTC / USDT，跨所对齐的锚）。
    pub code: String,
    /// 名称。
    pub name: Option<String>,
    /// coin / token / stock / index。
    pub kind: String,
    /// 外部源 coin id（如 `bitcoin`）；未匹配为 `None`（`docs/12 §3.2`）。
    pub external_id: Option<String>,
    /// 三级匹配结果：`matched` / `ambiguous` / `not_found`（`docs/14 §5.2`）。
    pub match_status: String,
    /// Twitter（`https://twitter.com/<screen_name>`）。
    pub twitter_url: Option<String>,
    /// 发行总量（十进制字符串，`docs/03 §5.5` 保真）。
    pub total_supply: Option<String>,
    /// 流通量（十进制字符串）。
    pub circulating_supply: Option<String>,
    /// 外部源名（`coingecko` / `coinmarketcap`）。
    pub detail_source: Option<String>,
    /// 详情字段最近一次拉取时刻（ms）；`None` = 尚未拉过详情。
    pub detail_synced_at: Option<u64>,
    /// 外部源原始响应（`docs/14 §4` 判断 2：只承接原始/次要字段）。
    pub extra: Value,
    pub created_at: u64,
    pub updated_at: u64,
    /// 各所覆盖数（`docs/13 §3.3 P5`）。
    pub venue_count: i64,
}

/// 合约地址子表行（`docs/12 §3.2.1` 的 `asset_contract`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetContractRecord {
    /// 自增 id。
    pub id: i64,
    /// 指向 `asset.id`。
    pub asset_id: i64,
    /// 资产代码（经 `asset` 关联给出）。
    pub code: String,
    /// 链（`TRON` / `Ethereum` / `Arbitrum` …）。
    pub chain: String,
    /// 合约地址。
    pub address: String,
    /// 标准（`TRC20` / `ERC20` / `BEP20` …）；外部源未给则 `None`。
    pub standard: Option<String>,
    /// 是否主链地址（0/1，**按 DDL 的 INTEGER 原样返回**——接口契约见 `docs/14 §9`）。
    pub is_primary: i64,
    /// 来源（`coingecko` / `coinmarketcap`）。
    pub source: String,
    pub updated_at: u64,
}

/// 各所层资产行（`docs/12 §3.2` 的 `venue_asset`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VenueAssetRecord {
    /// 指向 `asset.id`。
    pub asset_id: i64,
    /// 资产代码（经 `asset` 关联给出，界面直接可读）。
    pub code: String,
    /// 所。
    pub venue: String,
    /// 所方精度。
    pub precision: Option<i64>,
    /// 可用网络。
    pub networks: Vec<String>,
    pub deposit_enabled: Option<bool>,
    pub withdraw_enabled: Option<bool>,
    /// 所方状态字符串。
    pub status: Option<String>,
    pub extra: Value,
    /// 变更检测哈希（12 §4.2）。
    pub row_hash: String,
    pub last_synced_at: u64,
}

/// 交易对行（`docs/12 §3.3`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentRecord {
    /// 所（canonical 第一段）。
    pub venue: String,
    /// 市场（第二段）。
    pub market: String,
    /// 交易所原生代码（第三段）。
    pub native_code: String,
    pub base_asset_id: Option<i64>,
    pub quote_asset_id: Option<i64>,
    /// `asset.code`（`docs/13 §4.7`：`base` / `quote` 返回 `asset.code`）。
    pub base: Option<String>,
    pub quote: Option<String>,
    /// 所方口径：trading / break / delisted。
    pub status: String,
    /// 所方原样（spot / perpetual / future）。
    pub instrument_type: Option<String>,
    pub price_precision: Option<i64>,
    pub qty_precision: Option<i64>,
    /// 十进制字符串（03 §5.5 保真）。
    pub tick_size: Option<String>,
    pub step_size: Option<String>,
    pub min_notional: Option<String>,
    /// 首次见到（= 上新时刻的证据）。
    pub listed_at: Option<u64>,
    pub delisted_at: Option<u64>,
    pub first_seen_at: u64,
    pub last_synced_at: u64,
    pub row_hash: String,
    pub extra: Value,
}

impl InstrumentRecord {
    /// 参与 `row_hash` 的字段（12 §4.2：**不含任何时间戳**，也不含 `row_hash` 自身）。
    ///
    /// `base` / `quote` 取**代码**而非 `asset_id`，使哈希与 `asset` 表的自增 id 无关。
    pub fn hash_view(&self) -> Value {
        json!({
            "venue": self.venue,
            "market": self.market,
            "native_code": self.native_code,
            "base": self.base,
            "quote": self.quote,
            "status": self.status,
            "instrument_type": self.instrument_type,
            "price_precision": self.price_precision,
            "qty_precision": self.qty_precision,
            "tick_size": self.tick_size,
            "step_size": self.step_size,
            "min_notional": self.min_notional,
            "extra": self.extra,
        })
    }

    /// 按 [`hash_view`](InstrumentRecord::hash_view) 规范化取 SHA-256。
    pub fn compute_row_hash(&self) -> String {
        row_hash_of(&self.hash_view())
    }

    /// 状态是否是「已下架」（所方口径原样比较，大小写不敏感）。
    pub fn is_delisted(&self) -> bool {
        self.status.eq_ignore_ascii_case("delisted")
    }
}

/// `row_hash`：规范化与 `event_id` 内容哈希同源（03 §5.4 / 12 §4.2）。
pub fn row_hash_of(v: &Value) -> String {
    ponk_protocol::hash_text(&ponk_protocol::canonical_view(v))
}

/// 变更历史行（`docs/12 §3.3` 的 `instrument_change`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub id: i64,
    pub venue: String,
    pub market: String,
    pub native_code: String,
    /// new / delisted / status / precision / type / relisted。
    pub change_kind: String,
    /// 变更前（新上时为 `null`）。
    pub before: Option<Value>,
    /// 变更后。
    pub after: Option<Value>,
    /// DDL 实际列名（§7.2 R7；接口同时给出 `before` / `after` 别名）。
    pub before_v: Option<Value>,
    pub after_v: Option<Value>,
    pub detected_at: u64,
    pub sync_run_id: Option<i64>,
}

/// 同步轮次行（`docs/12 §3.4` 的 `sync_run`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRunRecord {
    pub id: i64,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    /// 逐所结果（JSON）。
    pub outcome: Value,
    pub error: Option<String>,
}

/// 外部（币信息）同步轮次行（`docs/12 §3.4b` 的 `offchain_sync_run`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OffchainSyncRunRecord {
    pub id: i64,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    /// `fetched` / `matched` / `ambiguous` / `not_found` / `detail_pulled` / `rate_limited`。
    pub outcome: Value,
    pub error: Option<String>,
}

/// 全局审计行（`docs/12 §3.7` 的 `audit_log`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditRow {
    pub id: i64,
    pub actor: String,
    /// publish / rollback / node_create / server_update / …
    pub action: String,
    pub target: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    /// 全文（域级行内含 `AuditEntry` 的 `diff[]`）。
    pub detail: Value,
    pub ts: u64,
}

/// 一次 upsert 的结果（落库层的可观测面）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpsertOutcome {
    /// `row_hash` 是否发生变化。
    pub changed: bool,
    /// 变更类别（`new` / `delisted` / `status` / `precision` / `type` / `relisted`）。
    pub change_kind: Option<String>,
    /// 是否是新行。
    pub is_new: bool,
    /// 写入的 `instrument_change.id`。
    pub change_id: Option<i64>,
}

// ─────────────────────────────────────────────────────────────────────────────
// dig3 映射与校验（docs/02 §6）
// ─────────────────────────────────────────────────────────────────────────────

/// `Market` → dig3 `AccountType`（02 §6 已定映射）。
///
/// **只钉死了 `spot` / `swap` 两条**（02 §6 原文），`future` 按同一注册键取
/// `FuturesCross`——见交付报告的规格缺口清单。
pub fn dig3_account_type_for(market: &str) -> Option<&'static str> {
    match market.trim().to_ascii_lowercase().as_str() {
        "spot" => Some("Spot"),
        "swap" | "future" | "futures" => Some("FuturesCross"),
        _ => None,
    }
}

/// 校验 `(market, dig3_account_type)`：必填 + **`swap → FuturesCross` 硬映射**。
///
/// 02 §6：`FuturesIsolated` 匹配不到 `TopicRegistry` 的注册键，**写错连不上**。
pub fn validate_account_type(market: &str, account_type: &str) -> Result<(), ControlError> {
    if market.trim().is_empty() {
        return Err(ControlError::validation("market 不能为空"));
    }
    if account_type.trim().is_empty() {
        return Err(ControlError::validation(
            "dig3_account_type 必填（docs/12 §3.1：不设默认值，写错连不上）",
        ));
    }
    if market.trim().eq_ignore_ascii_case("swap") && account_type.trim() != "FuturesCross" {
        return Err(ControlError::validation(format!(
            "market=swap 的 dig3_account_type 必须是 FuturesCross（docs/02 §6 硬映射：\
             不是 FuturesIsolated，写错连不上），得到 `{account_type}`"
        )));
    }
    Ok(())
}

/// `exchange.asset_class` 合法取值（`docs/12 §3.1` 注释 / `docs/14 §4`）。
pub const ASSET_CLASSES: [&str; 4] = ["crypto", "stock", "forex", "prediction"];

/// `asset.kind` 合法取值（`docs/12 §3.2` 注释）。
pub const ASSET_KINDS: [&str; 4] = ["coin", "token", "stock", "index"];

/// 校验并归一化交易所资产类（大小写不敏感；缺省 `crypto`）。
///
/// `stock` 是这里唯一有行为后果的取值：**该所交易对跳过链上币信息**（`docs/14 §6`）。
/// 人工台账填错的兜底 = 页面改单个 `asset.kind`（`docs/14 §4` 判断 1）。
pub fn validate_asset_class(v: &str) -> Result<String, ControlError> {
    let s = v.trim().to_lowercase();
    let s = if s.is_empty() { "crypto".to_string() } else { s };
    if ASSET_CLASSES.contains(&s.as_str()) {
        Ok(s)
    } else {
        Err(ControlError::validation(format!(
            "asset_class 非法：`{v}`（合法取值：{}）",
            ASSET_CLASSES.join(" / ")
        )))
    }
}

/// 校验并归一化全局资产的 `kind`（大小写不敏感；缺省 `coin`）。
pub fn validate_asset_kind(v: &str) -> Result<String, ControlError> {
    let s = v.trim().to_lowercase();
    let s = if s.is_empty() { "coin".to_string() } else { s };
    if ASSET_KINDS.contains(&s.as_str()) {
        Ok(s)
    } else {
        Err(ControlError::validation(format!(
            "asset.kind 非法：`{v}`（合法取值：{}）",
            ASSET_KINDS.join(" / ")
        )))
    }
}

/// 三项配额校验（`docs/13 §4.6`：∈ [0,1]；02 §6 给出 ~0.50 / ~0.35 / ~0.15）。
fn check_quota(name: &str, v: f64) -> Result<(), ControlError> {
    if !v.is_finite() || !(0.0..=1.0).contains(&v) {
        return Err(ControlError::validation(format!(
            "{name} 必须 ∈ [0,1]（docs/13 §4.6 / 02 §6），得到 {v}"
        )));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 交易所与市场（docs/13 §4.6）
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 新建 / 整体更新交易所（`venue` 主键）。
    pub fn upsert_exchange(&self, req: &NewExchange) -> Result<ExchangeRecord, ControlError> {
        let venue = req.venue.trim();
        if venue.is_empty() {
            return Err(ControlError::validation("venue 不能为空"));
        }
        let name = if req.name.trim().is_empty() {
            venue.to_string()
        } else {
            req.name.trim().to_string()
        };
        let dig3 = if req.dig3_exchange_id.trim().is_empty() {
            venue.to_string()
        } else {
            req.dig3_exchange_id.trim().to_string()
        };
        let extra = serde_json::to_string(&req.extra)?;
        let asset_class = validate_asset_class(&req.asset_class)?;
        let now = crate::now_ms();
        self.with_write(|c| {
            c.execute(
                "INSERT INTO exchange (venue, name, dig3_exchange_id, enabled, asset_class, extra,
                                       created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?7)
                 ON CONFLICT(venue) DO UPDATE SET
                   name = excluded.name, dig3_exchange_id = excluded.dig3_exchange_id,
                   enabled = excluded.enabled, asset_class = excluded.asset_class,
                   extra = excluded.extra,
                   updated_at = excluded.updated_at",
                params![venue, name, dig3, req.enabled, asset_class, extra, now as i64],
            )?;
            Ok(())
        })?;
        self.exchange(venue)?
            .ok_or_else(|| ControlError::Illegal("exchange write lost".into()))
    }

    /// 按 `venue` 取交易所（含市场数）。
    pub fn exchange(&self, venue: &str) -> Result<Option<ExchangeRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT e.venue, e.name, e.dig3_exchange_id, e.enabled, e.asset_class, e.extra,
                        e.created_at, e.updated_at,
                        (SELECT COUNT(*) FROM exchange_market m WHERE m.venue = e.venue)
                 FROM exchange e WHERE e.venue = ?1",
            )?;
            let mut rows = stmt.query_map(params![venue], decode_exchange)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 全部交易所（按 `venue` 升序；**不含**分页——分页在 [`ExchangeFilter`]）。
    pub fn list_exchanges_all(&self) -> Result<Vec<ExchangeRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT e.venue, e.name, e.dig3_exchange_id, e.enabled, e.asset_class, e.extra,
                        e.created_at, e.updated_at,
                        (SELECT COUNT(*) FROM exchange_market m WHERE m.venue = e.venue)
                 FROM exchange e ORDER BY e.venue",
            )?;
            let rows = stmt.query_map([], decode_exchange)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 只改 `enabled`（`PATCH /exchanges/{venue}/enabled`）。
    pub fn set_exchange_enabled(&self, venue: &str, enabled: bool) -> Result<(), ControlError> {
        self.require_exchange(venue)?;
        let now = crate::now_ms();
        self.with_write(|c| {
            c.execute(
                "UPDATE exchange SET enabled = ?1, updated_at = ?2 WHERE venue = ?3",
                params![enabled, now as i64, venue],
            )?;
            Ok(())
        })
    }

    /// 取交易所；不存在 → 404（§4.2 `not_found`）。
    pub fn require_exchange(&self, venue: &str) -> Result<ExchangeRecord, ControlError> {
        self.exchange(venue)?.ok_or_else(|| {
            ControlError::not_found(format!("交易所 `{venue}` 不存在"))
        })
    }

    /// 取市场；不存在 → 404。
    pub fn require_market(&self, venue: &str, market: &str) -> Result<ExchangeMarketRecord, ControlError> {
        self.market(venue, market)?.ok_or_else(|| {
            ControlError::not_found(format!("市场 `{venue}/{market}` 不存在"))
        })
    }

    /// 删除交易所（**级联**删市场与交易对：12 §3.1 `ON DELETE CASCADE`）。
    ///
    /// 返回被删市场数（调用方用于二次确认判断）。
    pub fn delete_exchange(&self, venue: &str) -> Result<i64, ControlError> {
        self.require_exchange(venue)?;
        let markets: i64 = self.with_read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM exchange_market WHERE venue = ?1",
                params![venue],
                |r| r.get(0),
            )
        })?;
        self.with_write(|c| {
            c.execute("DELETE FROM exchange WHERE venue = ?1", params![venue])?;
            Ok(markets)
        })
    }

    /// 新建 / 更新某所的市场。
    pub fn upsert_market(&self, req: &NewMarket) -> Result<ExchangeMarketRecord, ControlError> {
        validate_account_type(&req.market, &req.dig3_account_type)?;
        check_quota("quota_collect", req.quota_collect)?;
        check_quota("quota_backfill", req.quota_backfill)?;
        check_quota("quota_metadata", req.quota_metadata)?;
        if self.exchange(&req.venue)?.is_none() {
            return Err(ControlError::validation(format!(
                "交易所 `{}` 不存在（先建 exchange 再建市场）",
                req.venue
            )));
        }
        let now = crate::now_ms();
        let market = req.market.trim().to_string();
        let account = req.dig3_account_type.trim().to_string();
        let egress = req.egress_ip.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self.with_write(|c| {
            c.execute(
                "INSERT INTO exchange_market
                   (venue, market, dig3_account_type, enabled, quota_collect, quota_backfill,
                    quota_metadata, egress_ip, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)
                 ON CONFLICT(venue, market) DO UPDATE SET
                   dig3_account_type = excluded.dig3_account_type,
                   enabled = excluded.enabled, quota_collect = excluded.quota_collect,
                   quota_backfill = excluded.quota_backfill,
                   quota_metadata = excluded.quota_metadata,
                   egress_ip = excluded.egress_ip, updated_at = excluded.updated_at",
                params![
                    req.venue,
                    market,
                    account,
                    req.enabled,
                    req.quota_collect,
                    req.quota_backfill,
                    req.quota_metadata,
                    egress,
                    now as i64
                ],
            )?;
            Ok(())
        })?;
        self.market(&req.venue, &market)?
            .ok_or_else(|| ControlError::Illegal("exchange_market write lost".into()))
    }

    /// 取单个市场行。
    pub fn market(&self, venue: &str, market: &str) -> Result<Option<ExchangeMarketRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT venue, market, dig3_account_type, enabled, quota_collect,
                        quota_backfill, quota_metadata, egress_ip, created_at, updated_at
                 FROM exchange_market WHERE venue = ?1 AND market = ?2",
            )?;
            let mut rows = stmt.query_map(params![venue, market], decode_market)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 某所的全部市场（按 `market` 升序）。
    pub fn list_markets(&self, venue: &str) -> Result<Vec<ExchangeMarketRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT venue, market, dig3_account_type, enabled, quota_collect,
                        quota_backfill, quota_metadata, egress_ip, created_at, updated_at
                 FROM exchange_market WHERE venue = ?1 ORDER BY market",
            )?;
            let rows = stmt.query_map(params![venue], decode_market)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 只改市场的 `enabled`。
    pub fn set_market_enabled(&self, venue: &str, market: &str, enabled: bool) -> Result<(), ControlError> {
        self.require_market(venue, market)?;
        let now = crate::now_ms();
        self.with_write(|c| {
            c.execute(
                "UPDATE exchange_market SET enabled = ?1, updated_at = ?2
                 WHERE venue = ?3 AND market = ?4",
                params![enabled, now as i64, venue, market],
            )?;
            Ok(())
        })
    }

    /// 删除市场（连带删该市场的交易对）。
    pub fn delete_market(&self, venue: &str, market: &str) -> Result<bool, ControlError> {
        self.with_write(|c| {
            let n = c.execute(
                "DELETE FROM exchange_market WHERE venue = ?1 AND market = ?2",
                params![venue, market],
            )?;
            Ok(n > 0)
        })
    }
}

fn decode_exchange(row: &Row) -> rusqlite::Result<ExchangeRecord> {
    let extra: String = row.get(5)?;
    Ok(ExchangeRecord {
        venue: row.get(0)?,
        name: row.get(1)?,
        dig3_exchange_id: row.get(2)?,
        enabled: row.get::<_, i64>(3)? != 0,
        asset_class: row.get(4)?,
        extra: serde_json::from_str(&extra).unwrap_or_else(|_| json!({})),
        created_at: row.get::<_, i64>(6)? as u64,
        updated_at: row.get::<_, i64>(7)? as u64,
        market_count: row.get(8)?,
    })
}

fn decode_market(row: &Row) -> rusqlite::Result<ExchangeMarketRecord> {
    Ok(ExchangeMarketRecord {
        venue: row.get(0)?,
        market: row.get(1)?,
        dig3_account_type: row.get(2)?,
        enabled: row.get::<_, i64>(3)? != 0,
        quota_collect: row.get(4)?,
        quota_backfill: row.get(5)?,
        quota_metadata: row.get(6)?,
        egress_ip: row.get(7)?,
        created_at: row.get::<_, i64>(8)? as u64,
        updated_at: row.get::<_, i64>(9)? as u64,
    })
}

/// `GET /exchanges` 的筛选（§4.6：`enabled` / `q` + 分页）。
#[derive(Debug, Clone, Default)]
pub struct ExchangeFilter {
    /// 按启停过滤。
    pub enabled: Option<bool>,
    /// 关键字（venue / name / dig3_exchange_id 模糊）。
    pub q: Option<String>,
    /// 排序字段（`venue` / `name` / `updated_at` / `market_count`）。
    pub sort: Option<String>,
    /// `asc` / `desc`。
    pub order: Option<String>,
    /// 分页。
    pub limit: Option<usize>,
    /// 分页。
    pub offset: Option<usize>,
}

impl ExchangeFilter {
    /// 筛选 + 排序（不分页）。
    pub fn select(&self, rows: Vec<ExchangeRecord>) -> Vec<ExchangeRecord> {
        let q = self.q.clone().unwrap_or_default();
        let mut out: Vec<ExchangeRecord> = rows
            .into_iter()
            .filter(|e| {
                if let Some(w) = self.enabled {
                    if e.enabled != w {
                        return false;
                    }
                }
                matches_q(&[&e.venue, &e.name, &e.dig3_exchange_id], &q)
            })
            .collect();
        let (field, desc) = order_of(self.sort.as_deref(), self.order.as_deref(),
            &["venue", "name", "updated_at", "market_count", "enabled"], "venue");
        out.sort_by(|a, b| {
            let ord = match field.as_str() {
                "name" => a.name.cmp(&b.name),
                "updated_at" => a.updated_at.cmp(&b.updated_at),
                "market_count" => a.market_count.cmp(&b.market_count),
                "enabled" => a.enabled.cmp(&b.enabled),
                _ => a.venue.cmp(&b.venue),
            };
            if desc { ord.reverse() } else { ord }
        });
        out
    }

    /// 筛选 + 排序 + 分页。
    pub fn apply(&self, rows: Vec<ExchangeRecord>) -> PageOut<ExchangeRecord> {
        paginate(self.select(rows), self.limit, self.offset)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 资产（docs/13 §4.8）
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 取（或创建）全局资产行，返回 `asset.id`。
    ///
    /// * `name` 只在**非空时**覆盖：全局层是跨所对齐的锚，先到的所方事实
    ///   不应被后到的空值抹掉。
    /// * `kind` **只在建行时写一次**，此后同步不再覆盖 —— 它是页面的人工兜底
    ///   （`docs/14 §4` 判断 1 / §9 `PATCH /assets/{code}`），且**不参与** `instrument.row_hash`。
    ///   若这里每轮用 `"coin"` 冲掉人工值，页面"改了 5 分钟自己变回去"（`docs/14 §1`）。
    /// * `external_id` / `match_status` 等外部源字段由**外部同步任务**写（`offchain.rs`）。
    pub fn ensure_asset(
        &self,
        code: &str,
        name: Option<&str>,
        kind: &str,
    ) -> Result<i64, ControlError> {
        let code = code.trim();
        if code.is_empty() {
            return Err(ControlError::validation("asset.code 不能为空"));
        }
        let now = crate::now_ms();
        self.with_write(|c| {
            c.execute(
                "INSERT INTO asset (code, name, kind, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?4) ON CONFLICT(code) DO NOTHING",
                params![code, name, kind, now as i64],
            )?;
            c.execute(
                "UPDATE asset SET
                   name = CASE WHEN ?2 IS NOT NULL THEN ?2 ELSE name END,
                   updated_at = ?3
                 WHERE code = ?1",
                params![code, name, now as i64],
            )?;
            Ok(())
        })?;
        self.asset_id(code)?
            .ok_or_else(|| ControlError::Illegal("asset write lost".into()))
    }

    /// 按代码取 id。
    pub fn asset_id(&self, code: &str) -> Result<Option<i64>, ControlError> {
        self.with_read(|c| {
            c.query_row("SELECT id FROM asset WHERE code = ?1", params![code], |r| r.get(0))
                .optional()
        })
    }

    /// 单个资产（含各所覆盖数）。
    pub fn asset(&self, code: &str) -> Result<Option<AssetRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT a.id, a.code, a.name, a.kind, a.external_id, a.match_status, a.twitter_url,
                        a.total_supply, a.circulating_supply, a.detail_source,
                        a.detail_synced_at, a.extra, a.created_at, a.updated_at,
                        (SELECT COUNT(*) FROM venue_asset v WHERE v.asset_id = a.id)
                 FROM asset a WHERE a.code = ?1",
            )?;
            let mut rows = stmt.query_map(params![code], decode_asset)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 按 id 取资产。
    pub fn asset_by_id(&self, id: i64) -> Result<Option<AssetRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT a.id, a.code, a.name, a.kind, a.external_id, a.match_status, a.twitter_url,
                        a.total_supply, a.circulating_supply, a.detail_source,
                        a.detail_synced_at, a.extra, a.created_at, a.updated_at,
                        (SELECT COUNT(*) FROM venue_asset v WHERE v.asset_id = a.id)
                 FROM asset a WHERE a.id = ?1",
            )?;
            let mut rows = stmt.query_map(params![id], decode_asset)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 全部资产（按 `code` 升序）。
    pub fn list_assets_all(&self) -> Result<Vec<AssetRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT a.id, a.code, a.name, a.kind, a.external_id, a.match_status, a.twitter_url,
                        a.total_supply, a.circulating_supply, a.detail_source,
                        a.detail_synced_at, a.extra, a.created_at, a.updated_at,
                        (SELECT COUNT(*) FROM venue_asset v WHERE v.asset_id = a.id)
                 FROM asset a ORDER BY a.code",
            )?;
            let rows = stmt.query_map([], decode_asset)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 某所的全部 `venue_asset`（按 `code` 升序）。
    pub fn list_venue_assets_all(&self, venue: &str) -> Result<Vec<VenueAssetRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT va.asset_id, a.code, va.venue, va.precision, va.networks,
                        va.deposit_enabled, va.withdraw_enabled, va.status, va.extra,
                        va.row_hash, va.last_synced_at
                 FROM venue_asset va JOIN asset a ON a.id = va.asset_id
                 WHERE va.venue = ?1 ORDER BY a.code",
            )?;
            let rows = stmt.query_map(params![venue], decode_venue_asset)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 某资产在各所的口径（`GET /assets/{code}` 的 `venue_asset[]`）。
    pub fn venue_assets_of(&self, code: &str) -> Result<Vec<VenueAssetRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT va.asset_id, a.code, va.venue, va.precision, va.networks,
                        va.deposit_enabled, va.withdraw_enabled, va.status, va.extra,
                        va.row_hash, va.last_synced_at
                 FROM venue_asset va JOIN asset a ON a.id = va.asset_id
                 WHERE a.code = ?1 ORDER BY va.venue",
            )?;
            let rows = stmt.query_map(params![code], decode_venue_asset)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 全部 `venue_asset`（筛选用）。
    pub fn list_venue_assets(&self) -> Result<Vec<VenueAssetRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT va.asset_id, a.code, va.venue, va.precision, va.networks,
                        va.deposit_enabled, va.withdraw_enabled, va.status, va.extra,
                        va.row_hash, va.last_synced_at
                 FROM venue_asset va JOIN asset a ON a.id = va.asset_id
                 ORDER BY a.code, va.venue",
            )?;
            let rows = stmt.query_map([], decode_venue_asset)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 逐所层 upsert（12 §4.1：**先比 `row_hash`**）。
    ///
    /// 哈希相同 ⇒ 只推进 `last_synced_at`（无变更零动作）；不同 ⇒ 覆盖整行。
    /// 变更历史归 `instrument_change`（只管标的），币信息不另设变更表。
    pub fn upsert_venue_asset(&self, rec: &mut VenueAssetRecord) -> Result<UpsertOutcome, ControlError> {
        rec.row_hash = rec.compute_row_hash();
        let asset_id = rec.asset_id;
        let venue = rec.venue.clone();
        let existing: Option<String> = self.with_read(|c| {
            c.query_row(
                "SELECT row_hash FROM venue_asset WHERE asset_id = ?1 AND venue = ?2",
                params![asset_id, venue],
                |r| r.get(0),
            )
            .optional()
        })?;
        let now = rec.last_synced_at;
        let networks = serde_json::to_string(&rec.networks)?;
        let extra = serde_json::to_string(&rec.extra)?;
        // `venue_asset` 无变更历史表：哈希相同 ⇒ 仅推进 `last_synced_at`（零动作）
        let changed = existing.as_deref() != Some(rec.row_hash.as_str());
        self.with_write(|c| {
            c.execute(
                "INSERT INTO venue_asset
                   (asset_id, venue, precision, networks, deposit_enabled, withdraw_enabled,
                    status, extra, row_hash, last_synced_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(asset_id, venue) DO UPDATE SET
                   precision = excluded.precision, networks = excluded.networks,
                   deposit_enabled = excluded.deposit_enabled,
                   withdraw_enabled = excluded.withdraw_enabled,
                   status = excluded.status, extra = excluded.extra,
                   row_hash = excluded.row_hash, last_synced_at = excluded.last_synced_at",
                params![
                    asset_id,
                    venue,
                    rec.precision,
                    networks,
                    rec.deposit_enabled,
                    rec.withdraw_enabled,
                    rec.status,
                    extra,
                    rec.row_hash,
                    now as i64
                ],
            )?;
            Ok(())
        })?;
        Ok(UpsertOutcome {
            changed,
            change_kind: None,
            is_new: existing.is_none(),
            change_id: None,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// docs/14 §2 同步目标 / §4 外部源字段 / §5.2 三级匹配 / §9 写路径
// ─────────────────────────────────────────────────────────────────────────────

/// 外部同步的轻量字段补丁（`/coins/markets`，**每轮**）。
#[derive(Debug, Clone, PartialEq)]
pub struct LightAssetPatch {
    /// 外部源给出的名称：**只在当前 `name` 为空时填充**——`asset.name` 的主写入方
    /// 仍是交易所同步（`docs/14 §2`），外部源只做补位，不做第二写入方。
    pub name: Option<String>,
    /// 发行总量（十进制字符串）。
    pub total_supply: Option<String>,
    /// 流通量（十进制字符串）。
    pub circulating_supply: Option<String>,
    /// 外部源名（`coingecko` / `coinmarketcap`）。
    pub source: String,
    /// `/coins/markets` 中该币的原始条目（存 `extra.light`，`docs/14 §4` 判断 2）。
    pub raw: Value,
    /// 本轮时刻（ms）。
    pub at: u64,
}

/// 外部同步的详情字段补丁（`/coins/{id}`，**首次 + 轮转** `coin_detail_batch`）。
#[derive(Debug, Clone, PartialEq)]
pub struct DetailAssetPatch {
    /// Twitter 主页（`links.twitter_screen_name` → `https://twitter.com/<name>`）。
    pub twitter_url: Option<String>,
    /// 外部源名。
    pub source: String,
    /// `/coins/{id}` 的原始响应（存 `extra.detail`）。
    pub raw: Value,
    /// 拉取时刻（ms），同时写 `asset.detail_synced_at`。
    pub at: u64,
}

/// 待写入的合约地址（`docs/12 §3.2.1` 的 `asset_contract`）。
#[derive(Debug, Clone, PartialEq)]
pub struct NewContract {
    pub asset_id: i64,
    /// 链（`TRON` / `Ethereum` …）。
    pub chain: String,
    /// 合约地址。
    pub address: String,
    /// 标准（`TRC20` / `ERC20` …）；外部源未给则 `None`。
    pub standard: Option<String>,
    /// 主链位（同币多链时的展示主位）。
    pub is_primary: bool,
    /// 来源。
    pub source: String,
    pub updated_at: u64,
}

/// `PATCH /assets/{code}` 的入参（`docs/14 §9`）。
///
/// `kind` 与 `external_id` 都**不参与** `instrument.row_hash`（`docs/14 §4` 判断 3）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssetPatch {
    /// 人工兜底的资产类。
    pub kind: Option<String>,
    /// `None` = 本字段未提交；`Some(None)` = 清空；`Some(Some(id))` = 手填外部 id。
    pub external_id: Option<Option<String>>,
}

/// 外部同步目标行（`docs/14 §3`「catalog 中全部去重 base」，逐币一条）。
#[derive(Debug, Clone, PartialEq)]
pub struct OffchainAssetRow {
    pub id: i64,
    pub code: String,
    pub kind: String,
    pub name: Option<String>,
    /// 手填或自动匹配到的外部 id（`None` = 待匹配）。
    pub external_id: Option<String>,
    pub match_status: String,
    /// 详情字段最近拉取时刻（`None` = 从未拉过）。
    pub detail_synced_at: Option<u64>,
}

/// 把外部源原始响应的某个槽位并进 `asset.extra`（对象整体保留）。
fn upsert_asset_extra(
    c: &mut rusqlite::Connection,
    asset_id: i64,
    slot: &str,
    value: Value,
    now: i64,
) -> rusqlite::Result<()> {
    let raw: Option<String> = c
        .query_row(
            "SELECT extra FROM asset WHERE id = ?1",
            params![asset_id],
            |r| r.get(0),
        )
        .optional()?;
    let mut obj = raw
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    if let Value::Object(m) = &mut obj {
        m.insert(slot.to_string(), value);
    }
    let text = serde_json::to_string(&obj).unwrap_or_else(|_| "{}".to_string());
    c.execute(
        "UPDATE asset SET extra = ?1, updated_at = ?2 WHERE id = ?3",
        params![text, now, asset_id],
    )?;
    Ok(())
}

impl SqliteStore {
    // ── docs/14 §2 同步目标 ────────────────────────────────────────────────

    /// 交易所同步目标：`exchange_market` 中 **`enabled = 1` 的 `(venue, market)`**。
    ///
    /// `docs/14 §2`：**不是 `SubscribePlan` 域**——那是「采集意图」（派生自节点 scope，
    /// 冷启动为空 → 先有鸡还是先有蛋），而 `exchange_market` 是「接入台账」。
    /// 目录同步要回答的是「这个所有什么标的」，不是「我要采什么」。
    /// `exchange.enabled = 0` 或市场 `enabled = 0` 都停止同步（省配额与存储）。
    pub fn enabled_market_targets(&self) -> Result<Vec<(String, String)>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT m.venue, m.market
                   FROM exchange_market m
                   JOIN exchange e ON e.venue = m.venue
                  WHERE m.enabled = 1 AND e.enabled = 1
                  ORDER BY m.venue, m.market",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 外部同步目标资产行（含匹配与详情状态）。
    ///
    /// **股票类跳过**（`docs/14 §6`）：`exchange.asset_class = 'stock'` 的所，
    /// 其交易对**只同步交易对信息**，派生出的币不进外部同步目标——
    /// 判据是「该资产是否**只**出现在 stock 类交易所的交易对上」，
    /// 从不出现在任何交易对上（纯 `venue_asset` 派生）则保留。
    /// 另有 `asset.kind = 'stock'`（页面人工兜底，`docs/14 §4` 判断 1）直接跳过。
    pub fn offchain_targets(&self) -> Result<Vec<OffchainAssetRow>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT a.id, a.code, a.kind, a.name, a.external_id, a.match_status,
                        a.detail_synced_at
                   FROM asset a
                  WHERE a.kind != 'stock'
                    AND (NOT EXISTS (
                          SELECT 1 FROM instrument i
                           WHERE i.base_asset_id = a.id OR i.quote_asset_id = a.id)
                         OR EXISTS (
                          SELECT 1 FROM instrument i
                            JOIN exchange e ON e.venue = i.venue
                           WHERE (i.base_asset_id = a.id OR i.quote_asset_id = a.id)
                             AND e.asset_class != 'stock'))
                  ORDER BY a.code",
            )?;
            let rows = stmt.query_map([], decode_offchain_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 详情轮转候选：**已匹配**但详情最旧的 `limit` 个 `(asset_id, code, external_id)`。
    ///
    /// `detail_synced_at IS NULL`（从未拉过 = `docs/14 §3` 的「首次出现时拉一次」）
    /// 按 `0` 参与排序，天然排在最前；其后按 `detail_synced_at` 升序轮转，
    /// 即「首次 + 轮转 `coin_detail_batch`」由**同一条规则**覆盖，无需额外状态。
    /// 未匹配（`external_id IS NULL`）的资产不进候选——没有外部 id 就拉不到详情。
    pub fn offchain_detail_candidates(
        &self,
        limit: usize,
    ) -> Result<Vec<(i64, String, String)>, ControlError> {
        let limit = limit.clamp(1, 5_000) as i64;
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT a.id, a.code, a.external_id
                   FROM asset a
                  WHERE a.external_id IS NOT NULL AND a.external_id != ''
                  ORDER BY COALESCE(a.detail_synced_at, 0) ASC, a.code ASC
                  LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    // ── docs/14 §5.2 三级匹配 ──────────────────────────────────────────────

    /// 写三级匹配结果（**精确唯一 / 多命中 `ambiguous` / 零命中 `not_found`**）。
    ///
    /// `external_id = Some` 表示自动匹配命中；`None` 表示本轮无唯一命中，
    /// **不覆盖**已有的手填值（手填别名必须可审计、可保留，`docs/14 §5.2`）。
    pub fn set_asset_match(
        &self,
        asset_id: i64,
        external_id: Option<&str>,
        match_status: &str,
    ) -> Result<(), ControlError> {
        let now = crate::now_ms();
        self.with_write(|c| {
            c.execute(
                "UPDATE asset SET
                   external_id = CASE WHEN ?2 IS NOT NULL THEN ?2 ELSE external_id END,
                   match_status = ?3,
                   updated_at = ?4
                 WHERE id = ?1",
                params![asset_id, external_id, match_status, now as i64],
            )?;
            Ok(())
        })
    }

    // ── docs/14 §3 字段分层写入 ────────────────────────────────────────────

    /// 写**轻量字段**（每轮 `/coins/markets`：发行量、流通量、排名进 `extra.light`）。
    ///
    /// 传 `None` 的字段**不覆盖**已有值——外部源的部分响应不该把旧数据抹成空。
    pub fn set_asset_light(
        &self,
        asset_id: i64,
        patch: &LightAssetPatch,
    ) -> Result<(), ControlError> {
        let now = patch.at as i64;
        self.with_write(|c| {
            c.execute(
                "UPDATE asset SET
                   name = CASE WHEN ?2 IS NOT NULL AND (name IS NULL OR name = '')
                               THEN ?2 ELSE name END,
                   total_supply = COALESCE(?3, total_supply),
                   circulating_supply = COALESCE(?4, circulating_supply),
                   detail_source = ?5,
                   updated_at = ?6
                 WHERE id = ?1",
                params![
                    asset_id,
                    patch.name,
                    patch.total_supply,
                    patch.circulating_supply,
                    patch.source,
                    now
                ],
            )?;
            upsert_asset_extra(c, asset_id, "light", patch.raw.clone(), now)?;
            Ok(())
        })
    }

    /// 写**详情字段**（首次 + 轮转的 `/coins/{id}`：Twitter、原始响应、`detail_synced_at`）。
    pub fn set_asset_detail(
        &self,
        asset_id: i64,
        patch: &DetailAssetPatch,
    ) -> Result<(), ControlError> {
        let now = patch.at as i64;
        self.with_write(|c| {
            c.execute(
                "UPDATE asset SET
                   twitter_url = COALESCE(?2, twitter_url),
                   detail_source = ?3,
                   detail_synced_at = ?4,
                   updated_at = ?4
                 WHERE id = ?1",
                params![asset_id, patch.twitter_url, patch.source, now],
            )?;
            upsert_asset_extra(c, asset_id, "detail", patch.raw.clone(), now)?;
            Ok(())
        })
    }

    /// 只推进 `detail_synced_at`（**详情拉取失败时**用）。
    ///
    /// 失败也要推进，否则一个永久失败的币会永远排在轮转队首，
    /// 把整条「首次 + 轮转」队列堵死（头阻塞）。失败事实记在
    /// `offchain_sync_run.outcome.detail_failed` 与 `error` 里，页面照常可见。
    pub fn touch_asset_detail(&self, asset_id: i64, at: u64) -> Result<(), ControlError> {
        self.with_write(|c| {
            c.execute(
                "UPDATE asset SET detail_synced_at = ?1, updated_at = ?1 WHERE id = ?2",
                params![at as i64, asset_id],
            )?;
            Ok(())
        })
    }

    /// 同步某币的合约地址子表（`docs/12 §3.2.1`）：**逐条 upsert + 清理消失的行**。
    ///
    /// 幂等：同一份 `rows` 跑两次结果相同（`UNIQUE(asset_id, chain, address)`）。
    /// `rows` 为空时**整体跳过**——`platforms` 缺失既可能是「确实没有链」
    /// （BTC/DOGE），也可能是上游部分响应，两者无法区分，宁可保留旧数据。
    pub fn sync_asset_contracts(
        &self,
        asset_id: i64,
        rows: &[NewContract],
    ) -> Result<usize, ControlError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let now = rows[0].updated_at as i64;
        self.with_write(|c| {
            let mut keep: Vec<(String, String)> = Vec::with_capacity(rows.len());
            for r in rows {
                c.execute(
                    "INSERT INTO asset_contract
                       (asset_id, chain, address, standard, is_primary, source, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(asset_id, chain, address) DO UPDATE SET
                       standard = excluded.standard, is_primary = excluded.is_primary,
                       source = excluded.source, updated_at = excluded.updated_at",
                    params![
                        r.asset_id,
                        r.chain,
                        r.address,
                        r.standard,
                        r.is_primary as i64,
                        r.source,
                        r.updated_at as i64
                    ],
                )?;
                keep.push((r.chain.clone(), r.address.clone()));
            }
            let stale: Vec<(i64, String, String)> = {
                let mut stmt =
                    c.prepare("SELECT id, chain, address FROM asset_contract WHERE asset_id = ?1")?;
                let rows = stmt.query_map(params![asset_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
                let mut v = Vec::new();
                for r in rows {
                    v.push(r?);
                }
                v
            };
            for (id, chain, addr) in stale {
                if !keep.iter().any(|(ch, ad)| *ch == chain && *ad == addr) {
                    c.execute("DELETE FROM asset_contract WHERE id = ?1", params![id])?;
                }
            }
            // 清理后统一刷新主链位：按 (chain, address) 字典序第一个为主
            c.execute(
                "UPDATE asset_contract SET is_primary = 0 WHERE asset_id = ?1",
                params![asset_id],
            )?;
            if let Some((chain, addr)) = keep.iter().min() {
                c.execute(
                    "UPDATE asset_contract SET is_primary = 1
                      WHERE asset_id = ?1 AND chain = ?2 AND address = ?3",
                    params![asset_id, chain, addr],
                )?;
            }
            let _ = now;
            Ok(rows.len())
        })
    }

    /// 某币的合约地址列表（`GET /assets/{code}/contracts`，主链位在前）。
    pub fn asset_contracts(&self, code: &str) -> Result<Vec<AssetContractRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT c.id, c.asset_id, a.code, c.chain, c.address, c.standard,
                        c.is_primary, c.source, c.updated_at
                   FROM asset_contract c
                   JOIN asset a ON a.id = c.asset_id
                  WHERE a.code = ?1
                  ORDER BY c.is_primary DESC, c.chain ASC, c.address ASC",
            )?;
            let rows = stmt.query_map(params![code], decode_contract)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 全库最新的 `asset.detail_synced_at`（`GET /sync/offchain/status` 用）。
    pub fn max_asset_detail_synced_at(&self) -> Result<Option<u64>, ControlError> {
        self.with_read(|c| {
            c.query_row(
                "SELECT MAX(detail_synced_at) FROM asset",
                [],
                |r| r.get::<_, Option<i64>>(0),
            )
            .map(|v| v.map(|x| x as u64))
        })
    }

    // ── docs/14 §9 币信息写路径 ────────────────────────────────────────────

    /// `PATCH /assets/{code}`：改 `kind` / `external_id`（页面唯一可写的两个字段）。
    ///
    /// 手填 `external_id` 后服务端置 `match_status = 'matched'`（`docs/14 §9`）；
    /// 清空则回落 `not_found`。二者**不参与** `instrument.row_hash`（`docs/14 §4` 判断 3）。
    pub fn patch_asset(&self, code: &str, patch: &AssetPatch) -> Result<AssetRecord, ControlError> {
        self.require_asset(code)?;
        let kind = match &patch.kind {
            None => None,
            Some(k) => Some(validate_asset_kind(k)?),
        };
        if let Some(Some(v)) = &patch.external_id {
            let t = v.trim();
            if t.is_empty() {
                return Err(ControlError::validation(
                    "external_id 不能为空字符串（要清空请传 null）",
                ));
            }
            // `docs/14 §5.2`：一期不提供候选查询，一律由操作者手输 CG id，
            // 页面按 `[A-Za-z0-9._-]` 校验——服务端同规则再挡一次（防绕过与手滑）。
            if t.len() > 128
                || !t
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                return Err(ControlError::validation(format!(
                    "external_id 只允许 [A-Za-z0-9._-]（docs/14 §5.2），得到 `{t}`"
                )));
            }
        }
        if kind.is_none() && patch.external_id.is_none() {
            return Err(ControlError::validation(
                "请求体至少要包含 kind 或 external_id",
            ));
        }
        let now = crate::now_ms();
        self.with_write(|c| {
            if let Some(k) = &kind {
                c.execute(
                    "UPDATE asset SET kind = ?1, updated_at = ?2 WHERE code = ?3",
                    params![k, now as i64, code],
                )?;
            }
            match &patch.external_id {
                None => {}
                Some(Some(v)) => {
                    c.execute(
                        "UPDATE asset SET external_id = ?1, match_status = 'matched',
                                          updated_at = ?2
                          WHERE code = ?3",
                        params![v.trim(), now as i64, code],
                    )?;
                }
                Some(None) => {
                    c.execute(
                        "UPDATE asset SET external_id = NULL, match_status = 'not_found',
                                          updated_at = ?1
                          WHERE code = ?2",
                        params![now as i64, code],
                    )?;
                }
            }
            Ok(())
        })?;
        self.asset(code)?
            .ok_or_else(|| ControlError::Illegal("asset write lost".into()))
    }

    /// 取资产；不存在 → 404。
    pub fn require_asset(&self, code: &str) -> Result<AssetRecord, ControlError> {
        self.asset(code)?
            .ok_or_else(|| ControlError::not_found(format!("币 `{code}` 不存在")))
    }

    // ── docs/12 §3.4b 外部同步轮次 ─────────────────────────────────────────

    /// 落一条外部同步轮次（**每轮一条**，与 `sync_run` 分离）。
    pub fn record_offchain_sync_run(
        &self,
        started_at: u64,
        finished_at: u64,
        outcome: &Value,
        error: Option<&str>,
    ) -> Result<i64, ControlError> {
        let text = serde_json::to_string(outcome)?;
        self.with_write(|c| {
            c.execute(
                "INSERT INTO offchain_sync_run (started_at, finished_at, outcome, error)
                 VALUES (?1,?2,?3,?4)",
                params![started_at as i64, finished_at as i64, text, error],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// 单条外部同步轮次。
    pub fn offchain_sync_run(
        &self,
        id: i64,
    ) -> Result<Option<OffchainSyncRunRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, started_at, finished_at, outcome, error
                   FROM offchain_sync_run WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map(params![id], decode_offchain_run)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 最近一条外部同步轮次（`GET /sync/offchain/status` 的 `last`）。
    pub fn last_offchain_sync_run(&self) -> Result<Option<OffchainSyncRunRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, started_at, finished_at, outcome, error
                   FROM offchain_sync_run ORDER BY id DESC LIMIT 1",
            )?;
            let mut rows = stmt.query_map([], decode_offchain_run)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 外部同步轮次历史（新在前）。
    pub fn list_offchain_sync_runs_all(&self) -> Result<Vec<OffchainSyncRunRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, started_at, finished_at, outcome, error
                   FROM offchain_sync_run ORDER BY id DESC",
            )?;
            let rows = stmt.query_map([], decode_offchain_run)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }
}

fn decode_offchain_row(row: &Row) -> rusqlite::Result<OffchainAssetRow> {
    Ok(OffchainAssetRow {
        id: row.get(0)?,
        code: row.get(1)?,
        kind: row.get(2)?,
        name: row.get(3)?,
        external_id: row.get(4)?,
        match_status: row.get(5)?,
        detail_synced_at: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
    })
}

fn decode_contract(row: &Row) -> rusqlite::Result<AssetContractRecord> {
    Ok(AssetContractRecord {
        id: row.get(0)?,
        asset_id: row.get(1)?,
        code: row.get(2)?,
        chain: row.get(3)?,
        address: row.get(4)?,
        standard: row.get(5)?,
        is_primary: row.get(6)?,
        source: row.get(7)?,
        updated_at: row.get::<_, i64>(8)? as u64,
    })
}

fn decode_offchain_run(row: &Row) -> rusqlite::Result<OffchainSyncRunRecord> {
    let outcome: String = row.get(3)?;
    Ok(OffchainSyncRunRecord {
        id: row.get(0)?,
        started_at: row.get::<_, i64>(1)? as u64,
        finished_at: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
        outcome: serde_json::from_str(&outcome).unwrap_or_else(|_| json!({})),
        error: row.get(4)?,
    })
}

fn decode_asset(row: &Row) -> rusqlite::Result<AssetRecord> {
    let extra: String = row.get(11)?;
    Ok(AssetRecord {
        id: row.get(0)?,
        code: row.get(1)?,
        name: row.get(2)?,
        kind: row.get(3)?,
        external_id: row.get(4)?,
        match_status: row.get(5)?,
        twitter_url: row.get(6)?,
        total_supply: row.get(7)?,
        circulating_supply: row.get(8)?,
        detail_source: row.get(9)?,
        detail_synced_at: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        extra: serde_json::from_str(&extra).unwrap_or_else(|_| json!({})),
        created_at: row.get::<_, i64>(12)? as u64,
        updated_at: row.get::<_, i64>(13)? as u64,
        venue_count: row.get(14)?,
    })
}

fn decode_venue_asset(row: &Row) -> rusqlite::Result<VenueAssetRecord> {
    let networks: String = row.get(4)?;
    let extra: String = row.get(8)?;
    Ok(VenueAssetRecord {
        asset_id: row.get(0)?,
        code: row.get(1)?,
        venue: row.get(2)?,
        precision: row.get(3)?,
        networks: serde_json::from_str(&networks).unwrap_or_default(),
        deposit_enabled: row.get::<_, Option<i64>>(5)?.map(|v| v != 0),
        withdraw_enabled: row.get::<_, Option<i64>>(6)?.map(|v| v != 0),
        status: row.get(7)?,
        extra: serde_json::from_str(&extra).unwrap_or_else(|_| json!({})),
        row_hash: row.get(9)?,
        last_synced_at: row.get::<_, i64>(10)? as u64,
    })
}

impl VenueAssetRecord {
    /// 参与 `row_hash` 的字段（12 §4.2：不含时间戳）。
    pub fn hash_view(&self) -> Value {
        json!({
            "code": self.code,
            "venue": self.venue,
            "precision": self.precision,
            "networks": self.networks,
            "deposit_enabled": self.deposit_enabled,
            "withdraw_enabled": self.withdraw_enabled,
            "status": self.status,
            "extra": self.extra,
        })
    }

    /// 规范化 + SHA-256。
    pub fn compute_row_hash(&self) -> String {
        row_hash_of(&self.hash_view())
    }
}

/// `GET /assets` 的筛选（§4.8）。
#[derive(Debug, Clone, Default)]
pub struct AssetFilter {
    /// coin / token / stock / index。
    pub kind: Option<String>,
    /// 关键字（code / name 模糊）。
    pub q: Option<String>,
    /// 只保留该所有口径的币。
    pub venue: Option<String>,
    /// 按可充值状态过滤。
    pub deposit_enabled: Option<bool>,
    /// 按可提币状态过滤。
    pub withdraw_enabled: Option<bool>,
    /// 排序字段（`code` / `id` / `updated_at` / `venue_count`）。
    pub sort: Option<String>,
    /// 升降序。
    pub order: Option<String>,
    /// 分页。
    pub limit: Option<usize>,
    /// 分页。
    pub offset: Option<usize>,
}

impl AssetFilter {
    /// 筛选 + 排序（不分页）。
    pub fn select(
        &self,
        assets: Vec<AssetRecord>,
        venue_assets: &[VenueAssetRecord],
    ) -> Vec<AssetRecord> {
        let q = self.q.clone().unwrap_or_default();
        let want_venue = self.venue.clone();
        let mut out: Vec<AssetRecord> = assets
            .into_iter()
            .filter(|a| {
                if let Some(k) = self.kind.as_deref() {
                    if !a.kind.eq_ignore_ascii_case(k) {
                        return false;
                    }
                }
                if !matches_q(&[&a.code, a.name.as_deref().unwrap_or("")], &q) {
                    return false;
                }
                let scoped: Vec<&VenueAssetRecord> = venue_assets
                    .iter()
                    .filter(|v| v.asset_id == a.id)
                    .filter(|v| want_venue.as_deref().map(|w| v.venue == w).unwrap_or(true))
                    .collect();
                if want_venue.is_some() && scoped.is_empty() {
                    return false;
                }
                if let Some(w) = self.deposit_enabled {
                    if !scoped.iter().any(|v| v.deposit_enabled == Some(w)) {
                        return false;
                    }
                }
                if let Some(w) = self.withdraw_enabled {
                    if !scoped.iter().any(|v| v.withdraw_enabled == Some(w)) {
                        return false;
                    }
                }
                true
            })
            .collect();
        let (field, desc) = order_of(self.sort.as_deref(), self.order.as_deref(),
            &["code", "id", "updated_at", "venue_count", "kind",
              "match_status", "detail_synced_at"], "code");
        out.sort_by(|a, b| {
            let ord = match field.as_str() {
                "id" => a.id.cmp(&b.id),
                "updated_at" => a.updated_at.cmp(&b.updated_at),
                "venue_count" => a.venue_count.cmp(&b.venue_count),
                "kind" => a.kind.cmp(&b.kind),
                "match_status" => a.match_status.cmp(&b.match_status),
                "detail_synced_at" => a.detail_synced_at.cmp(&b.detail_synced_at),
                _ => a.code.cmp(&b.code),
            };
            if desc { ord.reverse() } else { ord }
        });
        out
    }

    /// 筛选 + 排序 + 分页。
    pub fn apply(
        &self,
        assets: Vec<AssetRecord>,
        venue_assets: &[VenueAssetRecord],
    ) -> PageOut<AssetRecord> {
        paginate(self.select(assets, venue_assets), self.limit, self.offset)
    }
}

/// `GET /venues/{venue}/assets` 的筛选（§4.8）。
#[derive(Debug, Clone, Default)]
pub struct VenueAssetFilter {
    /// 关键字（code / status / network 模糊）。
    pub q: Option<String>,
    /// 分页。
    pub limit: Option<usize>,
    /// 分页。
    pub offset: Option<usize>,
}

impl VenueAssetFilter {
    /// 筛选（不分页）。
    pub fn select(&self, rows: Vec<VenueAssetRecord>) -> Vec<VenueAssetRecord> {
        let q = self.q.clone().unwrap_or_default();
        rows.into_iter()
            .filter(|v| {
                matches_q(
                    &[&v.code, v.status.as_deref().unwrap_or(""), &v.networks.join(" ")],
                    &q,
                )
            })
            .collect()
    }

    /// 筛选 + 分页。
    pub fn apply(&self, rows: Vec<VenueAssetRecord>) -> PageOut<VenueAssetRecord> {
        paginate(self.select(rows), self.limit, self.offset)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 交易对（docs/13 §4.7）
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 按 canonical 三段主键取单条。
    pub fn instrument(
        &self,
        venue: &str,
        market: &str,
        native_code: &str,
    ) -> Result<Option<InstrumentRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT i.venue, i.market, i.native_code, i.base_asset_id, i.quote_asset_id,
                        ab.code, aq.code, i.status, i.instrument_type, i.price_precision,
                        i.qty_precision, i.tick_size, i.step_size, i.min_notional,
                        i.listed_at, i.delisted_at, i.first_seen_at, i.last_synced_at,
                        i.row_hash, i.extra
                 FROM instrument i
                 LEFT JOIN asset ab ON ab.id = i.base_asset_id
                 LEFT JOIN asset aq ON aq.id = i.quote_asset_id
                 WHERE i.venue = ?1 AND i.market = ?2 AND i.native_code = ?3",
            )?;
            let mut rows = stmt.query_map(params![venue, market, native_code], decode_instrument)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 全部交易对（canonical 序；筛选在 [`InstrumentFilter`]）。
    pub fn list_instruments_all(&self) -> Result<Vec<InstrumentRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT i.venue, i.market, i.native_code, i.base_asset_id, i.quote_asset_id,
                        ab.code, aq.code, i.status, i.instrument_type, i.price_precision,
                        i.qty_precision, i.tick_size, i.step_size, i.min_notional,
                        i.listed_at, i.delisted_at, i.first_seen_at, i.last_synced_at,
                        i.row_hash, i.extra
                 FROM instrument i
                 LEFT JOIN asset ab ON ab.id = i.base_asset_id
                 LEFT JOIN asset aq ON aq.id = i.quote_asset_id
                 ORDER BY i.venue, i.market, i.native_code",
            )?;
            let rows = stmt.query_map([], decode_instrument)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 逐条 upsert + `row_hash` 变更检测（`docs/12 §4.1` 的核心一步）。
    ///
    /// * 哈希相同 → **只更新 `last_synced_at`**，`changed = false`；
    /// * 哈希不同 → 更新整行 + 写 `instrument_change`（`before_v` / `after_v`）。
    pub fn upsert_instrument(
        &self,
        rec: &mut InstrumentRecord,
        sync_run_id: Option<i64>,
    ) -> Result<UpsertOutcome, ControlError> {
        let now = rec.last_synced_at;
        rec.row_hash = rec.compute_row_hash();
        let existing = self.instrument(&rec.venue, &rec.market, &rec.native_code)?;

        let Some(old) = existing else {
            if rec.listed_at.is_none() {
                rec.listed_at = Some(now);
            }
            rec.first_seen_at = now;
            // 12 §3.3：新上时 `before_v` 为 NULL
            let after = serde_json::to_value(&rec.hash_view())?;
            let change_id = self.write_change(
                &rec.venue,
                &rec.market,
                &rec.native_code,
                "new",
                None,
                Some(&after),
                now,
                sync_run_id,
            )?;
            self.write_instrument_row(rec)?;
            return Ok(UpsertOutcome {
                changed: true,
                change_kind: Some("new".into()),
                is_new: true,
                change_id: Some(change_id),
            });
        };

        if old.row_hash == rec.row_hash {
            // 12 §4.2：内容相同 ⇒ 哈希相同 ⇒ **只推进 last_synced_at（零动作）**
            self.touch_instrument(&rec.venue, &rec.market, &rec.native_code, now)?;
            rec.first_seen_at = old.first_seen_at;
            rec.listed_at = old.listed_at;
            rec.delisted_at = old.delisted_at;
            return Ok(UpsertOutcome::default());
        }

        // 状态翻转的两个特例（change_kind 判定）
        let was_delisted = old.is_delisted();
        let now_delisted = rec.is_delisted();
        rec.first_seen_at = old.first_seen_at;
        if now_delisted && !was_delisted {
            rec.delisted_at = Some(now);
        } else if was_delisted && !now_delisted {
            rec.listed_at = Some(now);
            rec.delisted_at = None;
        } else {
            rec.listed_at = old.listed_at;
            rec.delisted_at = old.delisted_at;
        }

        let kind = classify_change(&old, rec);
        let before = serde_json::to_value(&old.hash_view())?;
        let after = serde_json::to_value(&rec.hash_view())?;
        let change_id = self.write_change(
            &rec.venue,
            &rec.market,
            &rec.native_code,
            &kind,
            Some(&before),
            Some(&after),
            now,
            sync_run_id,
        )?;
        self.write_instrument_row(rec)?;
        Ok(UpsertOutcome {
            changed: true,
            change_kind: Some(kind),
            is_new: false,
            change_id: Some(change_id),
        })
    }

    /// 把标的标记为下架（12 §3.3 的 `delisted_at` + `instrument_change`）。
    ///
    /// 已经是 `delisted` ⇒ 零动作（不重复写变更、不推进 `last_synced_at`——
    /// 它已经不在本轮拉取结果里了）。
    pub fn mark_instrument_delisted(
        &self,
        venue: &str,
        market: &str,
        native_code: &str,
        at: u64,
        sync_run_id: Option<i64>,
    ) -> Result<UpsertOutcome, ControlError> {
        let Some(mut old) = self.instrument(venue, market, native_code)? else {
            return Ok(UpsertOutcome::default()); // 库里本来就没有 → 无事可做
        };
        if old.is_delisted() {
            return Ok(UpsertOutcome::default());
        }
        let before = serde_json::to_value(&old.hash_view())?;
        old.status = "delisted".to_string();
        old.delisted_at = Some(at);
        old.last_synced_at = at;
        old.row_hash = old.compute_row_hash();
        let after = serde_json::to_value(&old.hash_view())?;
        let change_id = self.write_change(
            venue,
            market,
            native_code,
            "delisted",
            Some(&before),
            Some(&after),
            at,
            sync_run_id,
        )?;
        self.write_instrument_row(&old)?;
        Ok(UpsertOutcome {
            changed: true,
            change_kind: Some("delisted".into()),
            is_new: false,
            change_id: Some(change_id),
        })
    }

    /// 写 / 覆盖一行 `instrument`（`asset_id` 由调用方先 `ensure_asset` 回填）。
    fn write_instrument_row(&self, rec: &InstrumentRecord) -> Result<(), ControlError> {
        let extra = serde_json::to_string(&rec.extra)?;
        let (v, m, n) = (rec.venue.clone(), rec.market.clone(), rec.native_code.clone());
        let kind = if rec.first_seen_at == 0 { crate::now_ms() } else { rec.first_seen_at };
        let last = if rec.last_synced_at == 0 { crate::now_ms() } else { rec.last_synced_at };
        let hash = rec.row_hash.clone();
        let status = rec.status.clone();
        let itype = rec.instrument_type.clone();
        let (pp, qp) = (rec.price_precision, rec.qty_precision);
        let (tick, step, notional) = (rec.tick_size.clone(), rec.step_size.clone(), rec.min_notional.clone());
        let (listed, delisted) = (rec.listed_at, rec.delisted_at);
        let (base_id, quote_id) = (rec.base_asset_id, rec.quote_asset_id);
        self.with_write(|c| {
            c.execute(
                "INSERT INTO instrument
                   (venue, market, native_code, base_asset_id, quote_asset_id, status,
                    instrument_type, price_precision, qty_precision, tick_size, step_size,
                    min_notional, listed_at, delisted_at, first_seen_at, last_synced_at,
                    row_hash, extra)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
                 ON CONFLICT(venue, market, native_code) DO UPDATE SET
                   base_asset_id = excluded.base_asset_id,
                   quote_asset_id = excluded.quote_asset_id,
                   status = excluded.status,
                   instrument_type = excluded.instrument_type,
                   price_precision = excluded.price_precision,
                   qty_precision = excluded.qty_precision,
                   tick_size = excluded.tick_size, step_size = excluded.step_size,
                   min_notional = excluded.min_notional,
                   listed_at = excluded.listed_at, delisted_at = excluded.delisted_at,
                   first_seen_at = excluded.first_seen_at,
                   last_synced_at = excluded.last_synced_at,
                   row_hash = excluded.row_hash, extra = excluded.extra",
                params![
                    v,
                    m,
                    n,
                    base_id,
                    quote_id,
                    status,
                    itype,
                    pp,
                    qp,
                    tick,
                    step,
                    notional,
                    listed.map(|t| t as i64),
                    delisted.map(|t| t as i64),
                    kind as i64,
                    last as i64,
                    hash,
                    extra
                ],
            )?;
            Ok(())
        })
    }

    /// 只推进 `last_synced_at`（**无变更零动作**的实现落点）。
    fn touch_instrument(
        &self,
        venue: &str,
        market: &str,
        native_code: &str,
        at: u64,
    ) -> Result<(), ControlError> {
        self.with_write(|c| {
            c.execute(
                "UPDATE instrument SET last_synced_at = ?1
                 WHERE venue = ?2 AND market = ?3 AND native_code = ?4",
                params![at as i64, venue, market, native_code],
            )?;
            Ok(())
        })
    }

    /// 写一条 `instrument_change`（列名 `before_v` / `after_v`，12 §3.3 / §7.2 R7）。
    fn write_change(
        &self,
        venue: &str,
        market: &str,
        native_code: &str,
        kind: &str,
        before: Option<&Value>,
        after: Option<&Value>,
        at: u64,
        sync_run_id: Option<i64>,
    ) -> Result<i64, ControlError> {
        let before_s = before.map(serde_json::to_string).transpose()?;
        let after_s = after.map(serde_json::to_string).transpose()?;
        self.with_write(|c| {
            c.execute(
                "INSERT INTO instrument_change
                   (venue, market, native_code, change_kind, before_v, after_v,
                    detected_at, sync_run_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![venue, market, native_code, kind, before_s, after_s, at as i64, sync_run_id],
            )?;
            Ok(c.last_insert_rowid())
        })
    }
}

/// 由「变过的字段」判定 `change_kind`（12 §3.3 的取值集合）。
fn classify_change(old: &InstrumentRecord, new: &InstrumentRecord) -> String {
    if old.is_delisted() && !new.is_delisted() {
        return "relisted".into();
    }
    if new.is_delisted() && !old.is_delisted() {
        return "delisted".into();
    }
    let fields = changed_fields(&old.hash_view(), &new.hash_view());
    if fields.iter().any(|f| f == "status") {
        return "status".into();
    }
    const PRECISION: [&str; 5] = [
        "price_precision",
        "qty_precision",
        "tick_size",
        "step_size",
        "min_notional",
    ];
    if fields.iter().any(|f| PRECISION.contains(&f.as_str())) {
        return "precision".into();
    }
    if fields.iter().any(|f| f == "instrument_type") {
        return "type".into();
    }
    "status".into()
}

fn decode_instrument(row: &Row) -> rusqlite::Result<InstrumentRecord> {
    let extra: String = row.get(19)?;
    Ok(InstrumentRecord {
        venue: row.get(0)?,
        market: row.get(1)?,
        native_code: row.get(2)?,
        base_asset_id: row.get(3)?,
        quote_asset_id: row.get(4)?,
        base: row.get(5)?,
        quote: row.get(6)?,
        status: row.get(7)?,
        instrument_type: row.get(8)?,
        price_precision: row.get(9)?,
        qty_precision: row.get(10)?,
        tick_size: row.get(11)?,
        step_size: row.get(12)?,
        min_notional: row.get(13)?,
        listed_at: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
        delisted_at: row.get::<_, Option<i64>>(15)?.map(|v| v as u64),
        first_seen_at: row.get::<_, i64>(16)? as u64,
        last_synced_at: row.get::<_, i64>(17)? as u64,
        row_hash: row.get(18)?,
        extra: serde_json::from_str(&extra).unwrap_or_else(|_| json!({})),
    })
}

/// `GET /instruments` 的筛选（§4.7）。
#[derive(Debug, Clone, Default)]
pub struct InstrumentFilter {
    pub venue: Option<String>,
    pub market: Option<String>,
    pub status: Option<String>,
    /// 模糊搜 `native_code`（前缀 / 包含都命中）。
    pub native_code: Option<String>,
    /// 模糊搜 `base`。
    pub base: Option<String>,
    /// 模糊搜 `quote`。
    pub quote: Option<String>,
    pub instrument_type: Option<String>,
    /// `last_synced_at >=`
    pub synced_after: Option<u64>,
    /// `last_synced_at <=`
    pub synced_before: Option<u64>,
    pub sort: Option<String>,
    pub order: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

impl InstrumentFilter {
    /// 筛选 + 排序（不分页；导出走这里）。
    pub fn select(&self, rows: Vec<InstrumentRecord>) -> Vec<InstrumentRecord> {
        let mut out: Vec<InstrumentRecord> = rows
            .into_iter()
            .filter(|i| {
                if let Some(w) = self.venue.as_deref() {
                    if i.venue != w {
                        return false;
                    }
                }
                if let Some(w) = self.market.as_deref() {
                    if i.market != w {
                        return false;
                    }
                }
                if let Some(w) = self.status.as_deref() {
                    if !i.status.eq_ignore_ascii_case(w) {
                        return false;
                    }
                }
                if let Some(w) = self.native_code.as_deref().filter(|s| !s.is_empty()) {
                    if !contains_ci(&i.native_code, w) {
                        return false;
                    }
                }
                if let Some(w) = self.base.as_deref().filter(|s| !s.is_empty()) {
                    if !contains_ci(i.base.as_deref().unwrap_or(""), w) {
                        return false;
                    }
                }
                if let Some(w) = self.quote.as_deref().filter(|s| !s.is_empty()) {
                    if !contains_ci(i.quote.as_deref().unwrap_or(""), w) {
                        return false;
                    }
                }
                if let Some(w) = self.instrument_type.as_deref().filter(|s| !s.is_empty()) {
                    if !i
                        .instrument_type
                        .as_deref()
                        .map(|t| contains_ci(t, w))
                        .unwrap_or(false)
                    {
                        return false;
                    }
                }
                if let Some(t) = self.synced_after {
                    if i.last_synced_at < t {
                        return false;
                    }
                }
                if let Some(t) = self.synced_before {
                    if i.last_synced_at > t {
                        return false;
                    }
                }
                true
            })
            .collect();
        // §4.7：sort 允许 native_code / status / last_synced_at / listed_at
        let (field, desc) = order_of(
            self.sort.as_deref(),
            self.order.as_deref(),
            &["native_code", "status", "last_synced_at", "listed_at", "venue", "market"],
            "native_code",
        );
        out.sort_by(|a, b| {
            let ord = match field.as_str() {
                "status" => a.status.cmp(&b.status),
                "last_synced_at" => a.last_synced_at.cmp(&b.last_synced_at),
                "listed_at" => a.listed_at.cmp(&b.listed_at),
                "venue" => a.venue.cmp(&b.venue),
                "market" => a.market.cmp(&b.market),
                _ => a.native_code.cmp(&b.native_code),
            };
            let ord = if desc { ord.reverse() } else { ord };
            ord.then_with(|| a.venue.cmp(&b.venue))
                .then_with(|| a.market.cmp(&b.market))
                .then_with(|| a.native_code.cmp(&b.native_code))
        });
        out
    }

    /// 筛选 + 排序 + 分页。
    pub fn apply(&self, rows: Vec<InstrumentRecord>) -> PageOut<InstrumentRecord> {
        paginate(self.select(rows), self.limit, self.offset)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 变更历史（docs/13 §4.7）
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 全部变更（按 `detected_at` 倒序、同刻按 id 倒序）。
    pub fn list_changes_all(&self) -> Result<Vec<ChangeRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, venue, market, native_code, change_kind, before_v, after_v,
                        detected_at, sync_run_id
                 FROM instrument_change
                 ORDER BY detected_at DESC, id DESC",
            )?;
            let rows = stmt.query_map([], decode_change)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 单条变更（`docs/13 §4.7 /instruments/changes/{id}`）。
    pub fn change(&self, id: i64) -> Result<Option<ChangeRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, venue, market, native_code, change_kind, before_v, after_v,
                        detected_at, sync_run_id
                 FROM instrument_change WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map(params![id], decode_change)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }
}

fn decode_change(row: &Row) -> rusqlite::Result<ChangeRecord> {
    let before: Option<String> = row.get(5)?;
    let after: Option<String> = row.get(6)?;
    let parse = |s: Option<String>| -> Option<Value> {
        s.and_then(|t| serde_json::from_str(&t).ok())
    };
    let b = parse(before);
    let a = parse(after);
    Ok(ChangeRecord {
        id: row.get(0)?,
        venue: row.get(1)?,
        market: row.get(2)?,
        native_code: row.get(3)?,
        change_kind: row.get(4)?,
        before: b.clone(),
        after: a.clone(),
        before_v: b,
        after_v: a,
        detected_at: row.get::<_, i64>(7)? as u64,
        sync_run_id: row.get(8)?,
    })
}

/// `GET /instruments/changes` 的筛选（§4.7）。
#[derive(Debug, Clone, Default)]
pub struct ChangeFilter {
    pub venue: Option<String>,
    pub market: Option<String>,
    pub native_code: Option<String>,
    pub change_kind: Option<String>,
    /// `detected_at >=`
    pub since: Option<u64>,
    /// `detected_at <=`
    pub until: Option<u64>,
    pub sync_run_id: Option<i64>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

impl ChangeFilter {
    /// 筛选（不分页）。
    pub fn select(&self, rows: Vec<ChangeRecord>) -> Vec<ChangeRecord> {
        rows.into_iter()
            .filter(|c| {
                if let Some(w) = self.venue.as_deref() {
                    if c.venue != w {
                        return false;
                    }
                }
                if let Some(w) = self.market.as_deref() {
                    if c.market != w {
                        return false;
                    }
                }
                if let Some(w) = self.native_code.as_deref().filter(|s| !s.is_empty()) {
                    if !contains_ci(&c.native_code, w) {
                        return false;
                    }
                }
                if let Some(w) = self.change_kind.as_deref() {
                    if !c.change_kind.eq_ignore_ascii_case(w) {
                        return false;
                    }
                }
                if let Some(t) = self.since {
                    if c.detected_at < t {
                        return false;
                    }
                }
                if let Some(t) = self.until {
                    if c.detected_at > t {
                        return false;
                    }
                }
                if let Some(id) = self.sync_run_id {
                    if c.sync_run_id != Some(id) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    /// 筛选 + 分页（顺序已在 SQL 里按时间倒序给定）。
    pub fn apply(&self, rows: Vec<ChangeRecord>) -> PageOut<ChangeRecord> {
        paginate(self.select(rows), self.limit, self.offset)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 同步轮次（docs/13 §4.12）
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 落一条 `sync_run`（12 §4.1：每轮一条），返回 `id`。
    pub fn record_sync_run(
        &self,
        started_at: u64,
        finished_at: u64,
        outcome: &Value,
        error: Option<&str>,
    ) -> Result<i64, ControlError> {
        let json = serde_json::to_string(outcome)?;
        let err = error.map(str::to_string);
        self.with_write(|c| {
            c.execute(
                "INSERT INTO sync_run (started_at, finished_at, outcome, error)
                 VALUES (?1,?2,?3,?4)",
                params![started_at as i64, finished_at as i64, json, err],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// 轮次历史（`started_at` 倒序）。
    pub fn list_sync_runs_all(&self) -> Result<Vec<SyncRunRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, started_at, finished_at, outcome, error
                 FROM sync_run ORDER BY started_at DESC, id DESC",
            )?;
            let rows = stmt.query_map([], decode_sync_run)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 单轮详情。
    pub fn sync_run(&self, id: i64) -> Result<Option<SyncRunRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, started_at, finished_at, outcome, error
                 FROM sync_run WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map(params![id], decode_sync_run)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }
}

fn decode_sync_run(row: &Row) -> rusqlite::Result<SyncRunRecord> {
    let outcome: String = row.get(3)?;
    Ok(SyncRunRecord {
        id: row.get(0)?,
        started_at: row.get::<_, i64>(1)? as u64,
        finished_at: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
        outcome: serde_json::from_str(&outcome).unwrap_or_else(|_| json!({})),
        error: row.get(4)?,
    })
}

/// `GET /sync/runs` 的分页（§4.12：只给 `limit` / `offset`）。
#[derive(Debug, Clone, Default)]
pub struct SyncRunFilter {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

impl SyncRunFilter {
    /// 分页。
    pub fn apply(&self, rows: Vec<SyncRunRecord>) -> PageOut<SyncRunRecord> {
        paginate(rows, self.limit, self.offset)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 全局审计（docs/13 §4.13）
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 全表审计（**跨域**；`ConfigStore::audit(domain)` 只覆盖 publish/rollback）。
    pub fn list_audit_all(&self) -> Result<Vec<AuditRow>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, actor, action, target, before_hash, after_hash, detail, ts
                 FROM audit_log ORDER BY ts DESC, id DESC",
            )?;
            let rows = stmt.query_map([], decode_audit)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 单条审计（含 `detail` 全文）。
    pub fn audit_row(&self, id: i64) -> Result<Option<AuditRow>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, actor, action, target, before_hash, after_hash, detail, ts
                 FROM audit_log WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map(params![id], decode_audit)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }
}

fn decode_audit(row: &Row) -> rusqlite::Result<AuditRow> {
    let detail: String = row.get(6)?;
    Ok(AuditRow {
        id: row.get(0)?,
        actor: row.get(1)?,
        action: row.get(2)?,
        target: row.get(3)?,
        before_hash: row.get(4)?,
        after_hash: row.get(5)?,
        detail: serde_json::from_str(&detail).unwrap_or_else(|_| json!({})),
        ts: row.get::<_, i64>(7)? as u64,
    })
}

/// `GET /audit` 的筛选（§4.13：`action` / `target` / `actor` / 时间区间 / `q`）。
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub action: Option<String>,
    pub target: Option<String>,
    pub actor: Option<String>,
    /// `ts >=`
    pub since: Option<u64>,
    /// `ts <=`
    pub until: Option<u64>,
    /// 关键字（actor / action / target / detail 模糊）。
    pub q: Option<String>,
    pub sort: Option<String>,
    pub order: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

impl AuditFilter {
    /// 筛选 + 排序（不分页；导出走这里）。
    pub fn select(&self, rows: Vec<AuditRow>) -> Vec<AuditRow> {
        let q = self.q.clone().unwrap_or_default();
        let mut out: Vec<AuditRow> = rows
            .into_iter()
            .filter(|a| {
                if let Some(w) = self.action.as_deref() {
                    if !a.action.eq_ignore_ascii_case(w) {
                        return false;
                    }
                }
                if let Some(w) = self.target.as_deref() {
                    if a.target != w {
                        return false;
                    }
                }
                if let Some(w) = self.actor.as_deref() {
                    if !contains_ci(&a.actor, w) {
                        return false;
                    }
                }
                if let Some(t) = self.since {
                    if a.ts < t {
                        return false;
                    }
                }
                if let Some(t) = self.until {
                    if a.ts > t {
                        return false;
                    }
                }
                if !q.is_empty() {
                    let hay = format!("{} {} {} {}", a.actor, a.action, a.target, a.detail);
                    if !contains_ci(&hay, &q) {
                        return false;
                    }
                }
                true
            })
            .collect();
        let (field, desc) = order_of(
            self.sort.as_deref(),
            self.order.as_deref(),
            &["ts", "id", "action", "actor", "target"],
            "ts",
        );
        out.sort_by(|a, b| {
            let ord = match field.as_str() {
                "id" => a.id.cmp(&b.id),
                "action" => a.action.cmp(&b.action),
                "actor" => a.actor.cmp(&b.actor),
                "target" => a.target.cmp(&b.target),
                _ => a.ts.cmp(&b.ts),
            };
            let ord = if desc { ord.reverse() } else { ord };
            ord.then_with(|| b.id.cmp(&a.id))
        });
        out
    }

    /// 筛选 + 排序 + 分页。
    pub fn apply(&self, rows: Vec<AuditRow>) -> PageOut<AuditRow> {
        paginate(self.select(rows), self.limit, self.offset)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 落库：catalog → instrument / venue_asset / sync_run（docs/12 §4.1）
// ─────────────────────────────────────────────────────────────────────────────

/// `Catalog` → `instrument` / `venue_asset` 事实落库，一轮一条 `sync_run`。
///
/// 由 [`crate::catalog::CatalogSync`] 每轮调用。**不参与比对与触发语义**——
/// 那些仍在 `catalog.rs`；这里只做 12 §4.1 的「逐条 upsert + 比对 `row_hash`」。
///
/// 返回 `sync_run.id`。
pub fn persist_catalog_round(
    store: &SqliteStore,
    catalog: &Catalog,
    diff: &CatalogDiff,
    started_at: u64,
    finished_at: u64,
    outcome: &Value,
    error: Option<&str>,
) -> Result<i64, ControlError> {
    let run_id = store.record_sync_run(started_at, finished_at, outcome, error)?;
    // 有错的轮次只留 `sync_run`（catalog 未推进，逐条比对无意义）
    if error.is_some() {
        return Ok(run_id);
    }

    // ① 交易所 / 市场：落事实前先补齐父行（12 §3.1 外键 `ON DELETE CASCADE`）
    for vm in catalog.symbols.keys() {
        if let Some((venue, market)) = crate::catalog::split_vm_key(vm) {
            ensure_exchange_and_market(store, &venue, Some(&market))?;
        }
    }
    for venue in catalog.currencies.keys() {
        ensure_exchange_and_market(store, venue, None)?;
    }

    // ② 交易对 → instrument
    for (vm, map) in &catalog.symbols {
        let Some((venue, market)) = crate::catalog::split_vm_key(vm) else {
            continue;
        };
        for (code, sym) in map {
            let base_id = store.ensure_asset(&sym.base_asset, None, "coin")?;
            let quote_id = store.ensure_asset(&sym.quote_asset, None, "coin")?;
            let mut rec = symbol_to_instrument(&venue, &market, code, sym, base_id, quote_id, finished_at);
            store.upsert_instrument(&mut rec, Some(run_id))?;
        }
    }

    // ③ 本轮从目录里消失的标的 → 标记下架（`change_kind = delisted`）
    for key in &diff.symbols_removed {
        let Some((vm, code)) = key.rsplit_once('|') else {
            continue;
        };
        let Some((venue, market)) = crate::catalog::split_vm_key(vm) else {
            continue;
        };
        store.mark_instrument_delisted(&venue, &market, code, finished_at, Some(run_id))?;
    }

    // ④ 币信息 → asset + venue_asset
    for (venue, map) in &catalog.currencies {
        for (code, cur) in map {
            let asset_id = store.ensure_asset(code, cur.name.as_deref(), "coin")?;
            let mut rec = currency_to_venue_asset(asset_id, code, venue, cur, finished_at);
            store.upsert_venue_asset(&mut rec)?;
        }
    }

    Ok(run_id)
}

/// 补齐 `exchange` / `exchange_market` 父行（幂等）。
///
/// `dig3_account_type` 由 02 §6 映射给出；**未知市场**没有映射，直接用市场名占位
/// （`TEXT NOT NULL` 必须有值）——运维可在 P3 页面改，见交付报告的规格缺口。
fn ensure_exchange_and_market(
    store: &SqliteStore,
    venue: &str,
    market: Option<&str>,
) -> Result<(), ControlError> {
    if store.exchange(venue)?.is_none() {
        store.upsert_exchange(&NewExchange {
            venue: venue.to_string(),
            name: venue.to_string(),
            dig3_exchange_id: venue.to_string(),
            enabled: true,
            asset_class: "crypto".to_string(),
            extra: json!({ "auto": "metadata-sync" }),
        })?;
    }
    if let Some(m) = market {
        if store.market(venue, m)?.is_none() {
            let account = dig3_account_type_for(m).unwrap_or(m);
            store.upsert_market(&NewMarket {
                venue: venue.to_string(),
                market: m.to_string(),
                dig3_account_type: account.to_string(),
                enabled: true,
                ..NewMarket::default()
            })?;
        }
    }
    Ok(())
}

/// `SymbolInfo` → 待落库的 [`InstrumentRecord`]。
fn symbol_to_instrument(
    venue: &str,
    market: &str,
    native_code: &str,
    sym: &ponk_protocol::SymbolInfo,
    base_asset_id: i64,
    quote_asset_id: i64,
    now: u64,
) -> InstrumentRecord {
    // 十进制字符串（03 §5.5 保真）：**不能走 serde_json** —— 它对 1e-8 输出
    // `"1e-8"`，页面上就显示成科学计数法。Rust 的 `Display` for f64 恒不使用
    // 指数记法，`0.00000001` 才是这个平台该看到的样子。
    let dec = |v: Option<f64>| -> Option<String> {
        v.filter(|f| f.is_finite()).map(|f| f.to_string())
    };
    InstrumentRecord {
        venue: venue.to_string(),
        market: market.to_string(),
        native_code: native_code.to_string(),
        base_asset_id: Some(base_asset_id),
        quote_asset_id: Some(quote_asset_id),
        base: Some(sym.base_asset.clone()),
        quote: Some(sym.quote_asset.clone()),
        status: sym.status.clone(),
        instrument_type: sym.instrument_type.clone(),
        price_precision: Some(sym.price_precision as i64),
        qty_precision: Some(sym.quantity_precision as i64),
        tick_size: dec(sym.tick_size),
        step_size: dec(sym.step_size),
        min_notional: dec(sym.min_notional),
        listed_at: None,
        delisted_at: None,
        first_seen_at: now,
        last_synced_at: now,
        row_hash: String::new(),
        extra: sym.extra.clone(),
    }
}

/// `CurrencyInfo` → 待落库的 [`VenueAssetRecord`]。
fn currency_to_venue_asset(
    asset_id: i64,
    code: &str,
    venue: &str,
    cur: &ponk_protocol::CurrencyInfo,
    now: u64,
) -> VenueAssetRecord {
    // `extra` 必须是 JSON 对象（DDL `TEXT`），`meta` 并进来；非对象时包一层。
    let mut extra = if cur.extra.is_object() {
        cur.extra.clone()
    } else if cur.extra.is_null() {
        json!({})
    } else {
        json!({ "raw": cur.extra })
    };
    if !cur.meta.is_empty() {
        if let Value::Object(map) = &mut extra {
            map.insert("meta".into(), json!(cur.meta));
        }
    }
    VenueAssetRecord {
        asset_id,
        code: code.to_string(),
        venue: venue.to_string(),
        precision: cur.precision.map(|p| p as i64),
        networks: cur.networks.clone(),
        // CurrencyInfo 无充提开关字段（12 §3.2 的两列留 NULL = 未知）
        deposit_enabled: None,
        withdraw_enabled: None,
        status: None,
        extra,
        row_hash: String::new(),
        last_synced_at: now,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 测试（离线：只构造结构，不联网、不建库）
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `03 §5.5` 保真：tick/step/notional 落库**不能出现指数记法**。
    ///
    /// 回归背景：原先用 `serde_json::json!(f).to_string()`，`1e-8` 被写成 `"1e-8"`，
    /// 页面上直接显示科学计数法；改用 `Display` for `f64` 后应是 `0.00000001`。
    #[test]
    fn instrument_decimals_are_plain_decimal_strings() {
        let sym = ponk_protocol::SymbolInfo {
            symbol: "BTCUSDT".into(),
            base_asset: "BTC".into(),
            quote_asset: "USDT".into(),
            status: "trading".into(),
            price_precision: 8,
            quantity_precision: 2,
            min_quantity: Some(1e-8),
            max_quantity: None,
            tick_size: Some(1e-8),
            step_size: Some(0.01),
            min_notional: Some(5.0),
            account_type: Default::default(),
            instrument_type: Some("spot".into()),
            extra: serde_json::json!({"status": "TRADING"}),
        };
        let rec = symbol_to_instrument("binance", "spot", "BTCUSDT", &sym, 1, 2, 42);
        assert_eq!(rec.tick_size.as_deref(), Some("0.00000001"), "禁止 1e-8");
        assert_eq!(rec.step_size.as_deref(), Some("0.01"));
        assert_eq!(rec.min_notional.as_deref(), Some("5"));
        assert!(rec.tick_size.as_deref().unwrap().contains('e') == false);
    }
}
