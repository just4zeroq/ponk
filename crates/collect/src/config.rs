//! 本地订阅计划配置（TOML/JSON）。
//!
//! # 与 03 §5.3 的关系
//!
//! 结构对齐 `SubscribePlan` / `ShardPlan`：
//!
//! ```text
//! SubscribePlan { assignment_version, mode, shards: Vec<ShardPlan>, quota_share }
//! ShardPlan     { venue, market, shard_idx, instruments: Vec<InstrumentMeta> }
//! InstrumentMeta{ native_code, 精度 meta, 通道集 }
//! ```
//!
//! # TODO(控制面 / docs/09)：改为经 `ctl.*` 拉取
//!
//! 控制面尚未实现（10 §4.1 的双路径「推 + 拉」都还没落地），本模块目前**只读本地文件**。
//! 将来接入时的改造点只有两处，均已在此标注：
//!
//! 1. [`CollectorConfig::load`] —— 改为「启动时从 `ctl.request.<instance>` 发拉取请求 +
//!    监听 `ctl.collector.<instance>` 的 push」，把收到的 `CtlMessage::SubscribePlan`
//!    反序列化成本结构（字段一一对应，见 [`ShardConfig`] / [`InstrumentConfig`]）。
//! 2. [`CollectorConfig::assignment_version`] / [`CollectorConfig::mode`] ——
//!    心跳里的 `applied_version` 与版本强序（旧版丢弃 / 新版整包应用，10 §4.3）
//!    现在取自本地文件，届时取自命令信道。
//!
//! `TODO(控制面)` 标记分布在 `config.rs`、`collector.rs`、`main.rs`。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use digdigdig3::core::types::{AccountType, ExchangeId};
use serde::{Deserialize, Serialize};

use crate::channel::Channel;
use crate::error::{Error, Result};

// ─────────────────────────────────────────────────────────────────────────────
// 顶层
// ─────────────────────────────────────────────────────────────────────────────

/// 一个采集进程承载的订阅计划（03 §5.3 `SubscribePlan` 的本地文件形态）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CollectorConfig {
    /// 采集实例 ID —— 心跳 `instance_id`、主题 `ops.collector.<instance>`、
    /// 命令信道 `ctl.collector.<instance>` 都用它（10 §5）。
    pub instance_id: String,

    /// 实例所在机器（register 心跳上报；控制面按 host 分散分片，09 §2.3）。
    /// 缺省取环境变量 `HOSTNAME` / `COMPUTERNAME`，仍取不到则不上报。
    #[serde(default)]
    pub host: Option<String>,

    /// 已应用的配置版本。TODO(控制面)：改为 `CtlMessage::SubscribePlan::assignment_version`。
    #[serde(default)]
    pub assignment_version: u64,

    /// 计划模式 `snapshot | diff`（03 §5.3 `PlanMode`）。
    /// TODO(控制面)：`diff` 的增量应用当前按 snapshot 处理（整包应用），控制面接上后实现真正的 diff。
    #[serde(default = "default_mode")]
    pub mode: String,

    /// 分片计划，可含多所（02 §4：一个进程承载 N 个分片，**可跨所**）。
    #[serde(default)]
    pub shards: Vec<ShardConfig>,

    /// 配额份额（03 §5.3 `QuotaShare`；02 §6「控制面按 IP 切固定份额，采集 ~50%」）。
    #[serde(default)]
    pub quota_share: Option<QuotaShareConfig>,

    /// 核心集原生代码（02 §6：candle 仅核心集可 REST 降级）。
    #[serde(default)]
    pub core_set: Vec<String>,

    #[serde(default)]
    pub quota: QuotaConfig,

    #[serde(default)]
    pub heartbeat: HeartbeatConfig,

    #[serde(default)]
    pub reconnect: ReconnectConfig,

    #[serde(default)]
    pub degrade: DegradeConfig,

    /// register 上报的承载能力（10 §5）。
    #[serde(default)]
    pub capacity: CapacityConfig,

    /// 命令信道（10 §4.1 / §7）：拉取与持续监听。
    #[serde(default)]
    pub control: ControlConfig,

    #[serde(default)]
    pub publish: PublishConfig,

    /// `market` → dig3 `AccountType` 的覆盖表。
    /// 默认见 [`market_to_account_type`]。
    #[serde(default)]
    pub account_type_overrides: HashMap<String, String>,
}

fn default_mode() -> String {
    "snapshot".to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// 分片 / 标的
// ─────────────────────────────────────────────────────────────────────────────

/// 03 §5.3 `ShardPlan`。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShardConfig {
    /// 交易所（`binance` / `okx` / …，03 §5.3 `Venue` 注释）。
    pub venue: String,
    /// 市场（`spot` / `swap`，03 §5.3 `Market` 注释）。
    pub market: String,
    /// 逻辑分片序号（02 §4：所有权 / 限频 / 配额单位）。
    #[serde(default)]
    pub shard_idx: u32,
    /// 该分片的默认通道集；标的级 `channels` 为空时继承这里。
    #[serde(default)]
    pub channels: Vec<String>,
    /// 标的白名单（02 §1「采集不做发现」：白名单之外一律不采）。
    #[serde(default)]
    pub instruments: Vec<InstrumentConfig>,
}

/// 03 §5.3 `InstrumentMeta`（原生代码 + 精度 meta + 通道集）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstrumentConfig {
    /// 交易所原生代码；canonical = `venue:market:native_code`（02 §3 定案 C）。
    pub native_code: String,
    /// 基础资产（跨所语义层，对应 dig3 `Symbol{base, quote}`）。
    #[serde(default)]
    pub base: String,
    /// 计价资产。
    #[serde(default)]
    pub quote: String,
    /// 价格精度（小数位）。
    #[serde(default)]
    pub price_precision: Option<u32>,
    /// 数量精度（小数位）。
    #[serde(default)]
    pub amount_precision: Option<u32>,
    /// 最小价格步长（十进制字符串）。
    #[serde(default)]
    pub tick_size: Option<String>,
    /// 最小数量步长（十进制字符串）。
    #[serde(default)]
    pub lot_size: Option<String>,
    /// 通道集；为空则继承分片的 `channels`。
    #[serde(default)]
    pub channels: Vec<String>,
    /// `false` = 采集侧**临时停用**该标的（03 §5.3 `InstrumentMeta::enabled`）。
    ///
    /// **不承担黑名单语义**：黑名单是独立配置域（09 §2 `InstrumentBlocklist`），
    /// 控制面在指派阶段就已把黑名单标的剔除，送达采集的计划里根本不含它们。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// 03 §5.3 `QuotaShare`（结构未在 03 中展开，此处按 02 §6 的语义落地）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuotaShareConfig {
    /// 按 (venue, 出口 IP) 的静态份额；02 §6：采集 ~50% / 回填 ~35% / 元数据 ~15%。
    #[serde(default)]
    pub share_bp: Option<u32>,
    /// 该份额对应的最大请求权重 / 分钟（控制面下发的具体数值）。
    #[serde(default)]
    pub weight_per_min: Option<u64>,
    /// 出口 IP（配额域 = (venue, 出口 IP)，01 / 定案 C 不变）。
    #[serde(default)]
    pub egress_ip: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// 子配置
// ─────────────────────────────────────────────────────────────────────────────

/// 令牌桶（02 §6「进程内按 venue 各自的令牌桶做配额仲裁，所间不串」）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QuotaConfig {
    /// 关闭后 `acquire` 恒成功（本地开发 / 已由上游代理限流时用）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 所有未单列 venue 的默认桶。
    #[serde(default)]
    pub default: BucketConfig,
    /// 按 venue 覆盖。
    #[serde(default)]
    pub venue: HashMap<String, BucketConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct BucketConfig {
    /// 桶容量（可突发的请求数 / 权重）。
    #[serde(default = "default_capacity")]
    pub capacity: f64,
    /// 每秒补充速率。
    #[serde(default = "default_refill")]
    pub refill_per_sec: f64,
    /// 单次 `acquire` 的成本，默认 1.0。
    #[serde(default = "default_cost")]
    pub cost: f64,
}

fn default_capacity() -> f64 {
    60.0
}
fn default_refill() -> f64 {
    10.0
}
fn default_cost() -> f64 {
    1.0
}

impl Default for BucketConfig {
    fn default() -> Self {
        Self {
            capacity: default_capacity(),
            refill_per_sec: default_refill(),
            cost: default_cost(),
        }
    }
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default: BucketConfig::default(),
            venue: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HeartbeatConfig {
    /// 心跳周期，10 §5 规定 5s。
    #[serde(default = "default_heartbeat_ms")]
    pub interval_ms: u64,
    /// 发布积压的观测上限（仅用于 06 告警阈值的参考，不做背压）。
    #[serde(default = "default_backlog_warn")]
    pub backlog_warn: i64,
}

fn default_heartbeat_ms() -> u64 {
    5_000
}
fn default_backlog_warn() -> i64 {
    1_024
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_heartbeat_ms(),
            backlog_warn: default_backlog_warn(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReconnectConfig {
    /// 连接级静默超时（ms）。超时视为连接级信号 → 重连（02 §6 检测项）。
    /// **不报静默**：静默的发现与告警归 06 观测 `md.*` 流（02 §7.3 / 10 §8）。
    #[serde(default = "default_silence_ms")]
    pub silence_timeout_ms: u64,
    /// 连续 `Disconnected` 状态持续多久（ms）才强制重连——给 dig3 自动重连留宽限。
    #[serde(default = "default_disconnected_grace_ms")]
    pub disconnected_grace_ms: u64,
    /// 退避初值（ms）。
    #[serde(default = "default_backoff_min_ms")]
    pub backoff_min_ms: u64,
    /// 退避上限（ms）。
    #[serde(default = "default_backoff_max_ms")]
    pub backoff_max_ms: u64,
    /// 最大重连次数；`0` = 无限（02 §6「只重连」默认不放弃）。
    #[serde(default)]
    pub max_attempts: u32,
    /// 状态轮询间隔（ms）。
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
}

fn default_silence_ms() -> u64 {
    60_000
}
fn default_disconnected_grace_ms() -> u64 {
    10_000
}
fn default_backoff_min_ms() -> u64 {
    1_000
}
fn default_backoff_max_ms() -> u64 {
    30_000
}
fn default_poll_ms() -> u64 {
    1_000
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            silence_timeout_ms: default_silence_ms(),
            disconnected_grace_ms: default_disconnected_grace_ms(),
            backoff_min_ms: default_backoff_min_ms(),
            backoff_max_ms: default_backoff_max_ms(),
            max_attempts: 0,
            poll_ms: default_poll_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DegradeConfig {
    /// 是否启用 REST 降级（02 §6）。关闭后断线只重连，不轮询。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 降级轮询间隔（ms）。
    #[serde(default = "default_degrade_poll_ms")]
    pub poll_interval_ms: u64,
    /// 单轮最多处理多少个标的（防止全市场 ticker 降级打爆配额）。
    #[serde(default = "default_degrade_batch")]
    pub max_instruments_per_round: usize,
}

fn default_degrade_poll_ms() -> u64 {
    1_000
}
fn default_degrade_batch() -> usize {
    200
}

impl Default for DegradeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_ms: default_degrade_poll_ms(),
            max_instruments_per_round: default_degrade_batch(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PublishConfig {
    /// 单条发布重试次数（10 §3「发送失败重试」）。
    #[serde(default = "default_publish_retries")]
    pub retries: u32,
    /// 重试间隔（ms）。
    #[serde(default = "default_publish_retry_ms")]
    pub retry_ms: u64,
    /// 事件发送队列容量。**满即丢并计数**——10 §3 明确不做背压，
    /// 积压与丢弃由 06 观测 ops 计数器。
    #[serde(default = "default_queue_cap")]
    pub queue_capacity: usize,
}

fn default_publish_retries() -> u32 {
    3
}
fn default_publish_retry_ms() -> u64 {
    50
}
fn default_queue_cap() -> usize {
    8192
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            retries: default_publish_retries(),
            retry_ms: default_publish_retry_ms(),
            queue_capacity: default_queue_cap(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 承载能力 / 控制信道
// ─────────────────────────────────────────────────────────────────────────────

/// 承载能力（10 §5 register：「可装哪些所 / 多少分片」；09 §2 `ProcessCapacity` 的自报面）。
///
/// 控制面用它做「承载约束 = min(配置上限, register 自报)」（09 §5.2）。
/// 数值默认值待 dig3 连接器层内存实测后定，但**是配置而非硬编码**（02 §6）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CapacityConfig {
    /// 每进程最大**逻辑分片**数。
    #[serde(default = "default_max_shards")]
    pub max_shards: u32,
    /// 每进程最大**标的**数（跨分片合计）。
    #[serde(default = "default_max_instruments")]
    pub max_instruments: u32,
    /// 每进程最大 `(venue, market)` **连接**数。
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    /// 可装的所（如 `["binance", "okx"]`）。
    /// **为空表示「按当前计划里实际出现的所自报」**。
    #[serde(default)]
    pub venues: Vec<String>,
}

fn default_max_shards() -> u32 {
    64
}
fn default_max_instruments() -> u32 {
    500
}
fn default_max_connections() -> u32 {
    16
}

impl Default for CapacityConfig {
    fn default() -> Self {
        Self {
            max_shards: default_max_shards(),
            max_instruments: default_max_instruments(),
            max_connections: default_max_connections(),
            venues: Vec::new(),
        }
    }
}

/// 命令信道（10 §4.1 / §7）：拉取节奏与持续监听。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ControlConfig {
    /// 是否在启动时发一次拉取请求（10 §7 第 2 步）。
    #[serde(default = "default_true")]
    pub pull_on_start: bool,
    /// 首个计划到达前的重拉间隔（ms）。
    ///
    /// **收敛后不再周期性拉取**：控制面 `repush_plan_to` 每次拉取都会
    /// `assignment_version + 1`，周期拉取会造成版本空转。
    /// 收敛后的变更由 ① `ctl.subscribe_plan` 上的 `ConfigChanged` 触发拉取，
    /// ② 控制面在心跳不收敛时主动重推（09 §4.4）来保证。
    #[serde(default = "default_pull_retry_ms")]
    pub pull_retry_ms: u64,
    /// 首个计划到达前的最大重拉次数（`0` = 无限）。
    #[serde(default = "default_max_pull_retries")]
    pub max_pull_retries: u32,
    /// 是否监听 `ctl.subscribe_plan` 上的 `ConfigChanged` 通知（09 §2.2 B 的「A 路径」）。
    #[serde(default = "default_true")]
    pub watch_changed: bool,
    /// 读取会话单次 `next` 超时（ms）——越小控制信道延迟越低，越大 CPU 越省。
    #[serde(default = "default_session_timeout_ms")]
    pub session_timeout_ms: u64,
}

fn default_pull_retry_ms() -> u64 {
    10_000
}
fn default_max_pull_retries() -> u32 {
    5
}
fn default_session_timeout_ms() -> u64 {
    200
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            pull_on_start: true,
            pull_retry_ms: default_pull_retry_ms(),
            max_pull_retries: default_max_pull_retries(),
            watch_changed: true,
            session_timeout_ms: default_session_timeout_ms(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// venue / market → dig3
// ─────────────────────────────────────────────────────────────────────────────

/// `venue` 字符串 → dig3 `ExchangeId`。首批 C3：Binance + OKX。
pub fn venue_to_exchange_id(venue: &str) -> Option<ExchangeId> {
    let id = match venue.to_ascii_lowercase().as_str() {
        "binance" => ExchangeId::Binance,
        "okx" => ExchangeId::OKX,
        "bybit" => ExchangeId::Bybit,
        "bitget" => ExchangeId::Bitget,
        "gate" | "gateio" => ExchangeId::GateIO,
        "kucoin" => ExchangeId::KuCoin,
        "kraken" => ExchangeId::Kraken,
        "htx" | "huobi" => ExchangeId::HTX,
        "mexc" => ExchangeId::MEXC,
        "bitmart" => return None,
        "coinbase" => ExchangeId::Coinbase,
        "bitfinex" => ExchangeId::Bitfinex,
        "bitstamp" => ExchangeId::Bitstamp,
        "gemini" => ExchangeId::Gemini,
        "cryptocom" => ExchangeId::CryptoCom,
        "bingx" => ExchangeId::BingX,
        "deribit" => ExchangeId::Deribit,
        "hyperliquid" | "hl" => ExchangeId::HyperLiquid,
        "bitmex" => ExchangeId::Bitmex,
        "dydx" => ExchangeId::Dydx,
        _ => return None,
    };
    Some(id)
}

/// `market` 字符串 → dig3 `AccountType`。
///
/// **`swap` → `FuturesCross`**：dig3 各所的 futures `TopicRegistry` 都以
/// `AccountType::FuturesCross` 为注册键（Binance `build_registry(FuturesCross)`、
/// OKX `build_futures_registry()` 内 `let at = FuturesCross`），传
/// `FuturesIsolated` 会匹配不到注册表。两者在**公共行情**语义上等价
/// （margin mode 是账户属性，不影响 public WS/REST 端点）。
pub fn market_to_account_type(market: &str) -> Option<AccountType> {
    match market.to_ascii_lowercase().as_str() {
        "spot" => Some(AccountType::Spot),
        "swap" | "perp" | "futures" | "usdm" => Some(AccountType::FuturesCross),
        "margin" => Some(AccountType::Margin),
        _ => None,
    }
}

/// 采集进程标识：`venue:market` 一个连接（02 §4「进程内按 shard × 通道开多个 dig3 通道」）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionKey {
    pub venue: String,
    pub market: String,
}

impl ConnectionKey {
    pub fn new(venue: impl Into<String>, market: impl Into<String>) -> Self {
        Self {
            venue: venue.into(),
            market: market.into(),
        }
    }
}

impl std::fmt::Display for ConnectionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.venue, self.market)
    }
}

/// 解析后的计划：按连接分组的订阅集合 + 每个标的的元数据。
#[derive(Debug, Clone)]
pub struct ResolvedPlan {
    pub assignment_version: u64,
    /// 连接键 → 订阅项。
    pub subscriptions: BTreeMap<ConnectionKey, Vec<Subscription>>,
    /// `instrument_id` → 标的元数据。
    pub instruments: BTreeMap<String, InstrumentMeta>,
    /// 连接键 → 该连接下的全部 `instrument_id`（降级轮询用）。
    pub connection_instruments: BTreeMap<ConnectionKey, Vec<String>>,
    pub core_set: Vec<String>,
}

/// 一条 WS 订阅（白名单 + 通道）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub connection: ConnectionKey,
    pub shard_idx: u32,
    pub channel: Channel,
    pub native_code: String,
    pub instrument_id: String,
}

/// 标的元数据（03 §5.3 `InstrumentMeta` 的采集侧视图）。
#[derive(Debug, Clone)]
pub struct InstrumentMeta {
    pub venue: String,
    pub market: String,
    pub native_code: String,
    pub base: String,
    pub quote: String,
    pub price_precision: Option<u32>,
    pub amount_precision: Option<u32>,
    pub tick_size: Option<String>,
    pub lot_size: Option<String>,
    pub channels: Vec<Channel>,
    /// `venue:market:native_code`（02 §3 定案 C），即 `Envelope::shard` 与
    /// `event_id` 的身份键。
    pub instrument_id: String,
}

impl InstrumentMeta {
    pub fn instrument_id(venue: &str, market: &str, native_code: &str) -> String {
        format!("{}:{}:{}", venue, market, native_code)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 加载 / 校验
// ─────────────────────────────────────────────────────────────────────────────

impl CollectorConfig {
    /// 按扩展名分派：`.json` 走 JSON，其余先 TOML 后 JSON。
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "json" {
            return Ok(serde_json::from_str(&raw)?);
        }
        match toml::from_str(&raw) {
            Ok(v) => Ok(v),
            Err(toml_err) => match serde_json::from_str(&raw) {
                Ok(v) => Ok(v),
                Err(json_err) => Err(Error::Config(format!(
                    "{}: TOML 解析失败（{}），JSON 解析失败（{}）",
                    path.display(),
                    toml_err,
                    json_err
                ))),
            },
        }
    }

    pub fn from_toml(raw: &str) -> Result<Self> {
        Ok(toml::from_str(raw)?)
    }

    pub fn from_json(raw: &str) -> Result<Self> {
        Ok(serde_json::from_str(raw)?)
    }

    /// 校验并展开为 [`ResolvedPlan`]。
    ///
    /// 校验项（均为 02/03 的硬约束）：
    /// - `venue` / `market` 可映射到 dig3；
    /// - 标的白名单非空（02 §1 采集不做发现）；
    /// - `funding` 只能出现在 `swap` 分片（02 §2 永续特有）；
    /// - **全市场通道 `shard_idx` 必须为 0**（02 §4 ticker/L1 不分片）；
    /// - 同一 `(venue, market)` 内 `native_code` 不重复；
    /// - 同一 `(venue, market, shard_idx)` 不出现两次。
    pub fn resolve(&self) -> Result<ResolvedPlan> {
        if self.shards.is_empty() {
            return Err(Error::Config(
                "shards 为空：控制面尚未实现前必须在本地配置至少一个分片（02 §1 白名单）"
                    .to_string(),
            ));
        }
        if self.mode != "snapshot" && self.mode != "diff" {
            return Err(Error::Config(format!(
                "mode 必须是 snapshot|diff，得到 {}",
                self.mode
            )));
        }

        let mut subscriptions: BTreeMap<ConnectionKey, Vec<Subscription>> = BTreeMap::new();
        let mut instruments: BTreeMap<String, InstrumentMeta> = BTreeMap::new();
        let mut connection_instruments: BTreeMap<ConnectionKey, Vec<String>> = BTreeMap::new();
        let mut seen_shard: std::collections::HashSet<(String, String, u32)> =
            std::collections::HashSet::new();

        for shard in &self.shards {
            let venue = shard.venue.to_ascii_lowercase();
            let market = shard.market.to_ascii_lowercase();

            if venue_to_exchange_id(&venue).is_none() {
                return Err(Error::Config(format!(
                    "未知 venue `{}`（首批 C3 = binance / okx）",
                    shard.venue
                )));
            }
            let account_type = self
                .account_type_overrides
                .get(&market)
                .and_then(|v| market_to_account_type(v))
                .or_else(|| market_to_account_type(&market));
            if account_type.is_none() {
                return Err(Error::Config(format!(
                    "未知 market `{}`（可用 spot|swap|margin，或用 account_type_overrides 覆盖）",
                    shard.market
                )));
            }

            if !seen_shard.insert((venue.clone(), market.clone(), shard.shard_idx)) {
                return Err(Error::Config(format!(
                    "重复的分片 ({}, {}, shard_idx={})",
                    venue, market, shard.shard_idx
                )));
            }

            let conn = ConnectionKey::new(venue.clone(), market.clone());
            if shard.instruments.is_empty() {
                return Err(Error::Config(format!(
                    "分片 ({}, {}, shard_idx={}) 的标的白名单为空 —— 采集不做发现，必须显式列出",
                    venue, market, shard.shard_idx
                )));
            }

            for inst in &shard.instruments {
                let channels_src: Vec<String> = if inst.channels.is_empty() {
                    shard.channels.clone()
                } else {
                    inst.channels.clone()
                };
                if channels_src.is_empty() {
                    return Err(Error::Config(format!(
                        "{} 未指定通道集（标的级与分片级皆空）",
                        inst.native_code
                    )));
                }

                let mut channels = Vec::with_capacity(channels_src.len());
                for c in &channels_src {
                    let ch = Channel::parse(c).ok_or_else(|| {
                        Error::Config(format!(
                            "未知通道 `{}`（可用 candle_1m|trade|ticker|l1|funding）",
                            c
                        ))
                    })?;
                    if ch.is_swap_only() && market != "swap" && market != "perp" && market != "futures" && market != "usdm" {
                        return Err(Error::Config(format!(
                            "通道 `{}` 只在永续市场存在（02 §2），但分片 market = {}（标的 {}）",
                            ch, market, inst.native_code
                        )));
                    }
                    if ch.is_full_market() && shard.shard_idx != 0 {
                        return Err(Error::Config(format!(
                            "全市场通道 `{}` 不分片（02 §4），但分片 ({}, {}, shard_idx={}) 的 shard_idx 必须为 0",
                            ch, venue, market, shard.shard_idx
                        )));
                    }
                    channels.push(ch);
                }
                channels.sort_unstable();
                channels.dedup();

                let instrument_id = InstrumentMeta::instrument_id(&venue, &market, &inst.native_code);
                if let Some(prev) = instruments.get(&instrument_id) {
                    return Err(Error::Config(format!(
                        "重复的标的 {}（shard_idx={} 与 shard_idx 已有条目冲突）",
                        instrument_id, prev.channels.len()
                    )));
                }
                instruments.insert(
                    instrument_id.clone(),
                    InstrumentMeta {
                        venue: venue.clone(),
                        market: market.clone(),
                        native_code: inst.native_code.clone(),
                        base: inst.base.clone(),
                        quote: inst.quote.clone(),
                        price_precision: inst.price_precision,
                        amount_precision: inst.amount_precision,
                        tick_size: inst.tick_size.clone(),
                        lot_size: inst.lot_size.clone(),
                        channels: channels.clone(),
                        instrument_id: instrument_id.clone(),
                    },
                );

                let slot = connection_instruments.entry(conn.clone()).or_default();
                if !slot.contains(&instrument_id) {
                    slot.push(instrument_id.clone());
                }

                let subs = subscriptions.entry(conn.clone()).or_default();
                for ch in channels {
                    let sub = Subscription {
                        connection: conn.clone(),
                        shard_idx: shard.shard_idx,
                        channel: ch,
                        native_code: inst.native_code.clone(),
                        instrument_id: instrument_id.clone(),
                    };
                    if !subs.contains(&sub) {
                        subs.push(sub);
                    }
                }
            }
        }

        let core_set: Vec<String> = self
            .core_set
            .iter()
            .map(|s| s.to_ascii_uppercase())
            .collect();

        Ok(ResolvedPlan {
            assignment_version: self.assignment_version,
            subscriptions,
            instruments,
            connection_instruments,
            core_set,
        })
    }

    pub fn account_type_for(&self, market: &str) -> Option<AccountType> {
        self.account_type_overrides
            .get(&market.to_ascii_lowercase())
            .and_then(|v| market_to_account_type(v))
            .or_else(|| market_to_account_type(market))
    }

    /// 上报用的机器名：配置优先，其次环境变量（拿不到就 `None`，控制面按"未上报"处理）。
    pub fn host_name(&self) -> Option<String> {
        if let Some(h) = self.host.as_ref().filter(|h| !h.trim().is_empty()) {
            return Some(h.clone());
        }
        for key in ["HOSTNAME", "COMPUTERNAME"] {
            if let Ok(v) = std::env::var(key) {
                if !v.trim().is_empty() {
                    return Some(v);
                }
            }
        }
        None
    }

    pub fn exchange_id_for(&self, venue: &str) -> Option<ExchangeId> {
        venue_to_exchange_id(venue)
    }

    /// 标的是否在核心集（02 §6：candle 仅核心集可降级）。大小写不敏感。
    pub fn is_core(&self, native_code: &str) -> bool {
        let up = native_code.to_ascii_uppercase();
        self.core_set.iter().any(|c| c == &up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
instance_id = "collector-01"
assignment_version = 7
mode = "snapshot"
core_set = ["BTCUSDT", "ETHUSDT"]

[heartbeat]
interval_ms = 5000

[quota]
enabled = true
[quota.default]
capacity = 60.0
refill_per_sec = 10.0

[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["ticker", "l1", "trade"]
[[shards.instruments]]
native_code = "BTCUSDT"
base = "BTC"
quote = "USDT"
price_precision = 2
amount_precision = 3
tick_size = "0.01"
lot_size = "0.001"
[[shards.instruments]]
native_code = "ETHUSDT"
base = "ETH"
quote = "USDT"
channels = ["ticker", "trade"]

[[shards]]
venue = "binance"
market = "swap"
shard_idx = 0
channels = ["candle_1m", "funding"]
[[shards.instruments]]
native_code = "BTCUSDT"
base = "BTC"
quote = "USDT"

[[shards]]
venue = "okx"
market = "spot"
shard_idx = 0
channels = ["trade"]
[[shards.instruments]]
native_code = "BTC-USDT"
"#;

    #[test]
    fn parses_toml() {
        let cfg = CollectorConfig::from_toml(SAMPLE).unwrap();
        assert_eq!(cfg.instance_id, "collector-01");
        assert_eq!(cfg.assignment_version, 7);
        assert_eq!(cfg.shards.len(), 3);
        assert_eq!(cfg.heartbeat.interval_ms, 5000);
    }

    #[test]
    fn resolves_multi_venue_one_process() {
        // 02 §4：一个进程承载多个 (venue, market) 分片，可跨所。
        let plan = CollectorConfig::from_toml(SAMPLE).unwrap().resolve().unwrap();
        assert_eq!(plan.subscriptions.len(), 3);
        assert!(plan.subscriptions.contains_key(&ConnectionKey::new("binance", "spot")));
        assert!(plan.subscriptions.contains_key(&ConnectionKey::new("binance", "swap")));
        assert!(plan.subscriptions.contains_key(&ConnectionKey::new("okx", "spot")));
        assert_eq!(plan.assignment_version, 7);
    }

    #[test]
    fn instrument_id_is_canonical_triple() {
        let plan = CollectorConfig::from_toml(SAMPLE).unwrap().resolve().unwrap();
        let id = InstrumentMeta::instrument_id("binance", "spot", "BTCUSDT");
        assert_eq!(id, "binance:spot:BTCUSDT");
        assert!(plan.instruments.contains_key(&id));
        // okx 的原生代码带连字符，canonical 原样拼接
        assert!(plan
            .instruments
            .contains_key("okx:spot:BTC-USDT"));
    }

    #[test]
    fn channel_inheritance_and_override() {
        let plan = CollectorConfig::from_toml(SAMPLE).unwrap().resolve().unwrap();
        // BTCUSDT 继承分片 channels = ticker,l1,trade
        let btc = plan.instruments.get("binance:spot:BTCUSDT").unwrap();
        // 排序按 `Channel` 枚举定义序（稳定、与 02 §2 通道表阅读序一致）
        assert_eq!(btc.channels, vec![Channel::Trade, Channel::Ticker, Channel::L1]);
        // ETHUSDT 自带 channels 覆盖分片默认
        let eth = plan.instruments.get("binance:spot:ETHUSDT").unwrap();
        assert_eq!(eth.channels, vec![Channel::Trade, Channel::Ticker]);
    }

    #[test]
    fn full_market_channel_rejects_non_zero_shard() {
        let bad = r#"
instance_id = "c"
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 3
channels = ["ticker"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;
        let err = CollectorConfig::from_toml(bad).unwrap().resolve().unwrap_err();
        assert!(err.to_string().contains("不分片"), "got: {}", err);
    }

    #[test]
    fn per_instrument_channel_allows_non_zero_shard() {
        let ok = r#"
instance_id = "c"
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 3
channels = ["trade"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;
        assert!(CollectorConfig::from_toml(ok).unwrap().resolve().is_ok());
    }

    #[test]
    fn funding_rejected_on_spot() {
        let bad = r#"
instance_id = "c"
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["funding"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;
        let err = CollectorConfig::from_toml(bad).unwrap().resolve().unwrap_err();
        assert!(err.to_string().contains("永续"), "got: {}", err);
    }

    #[test]
    fn empty_whitelist_rejected() {
        let bad = r#"
instance_id = "c"
[[shards]]
venue = "binance"
market = "spot"
channels = ["trade"]
instruments = []
"#;
        let err = CollectorConfig::from_toml(bad).unwrap().resolve().unwrap_err();
        assert!(err.to_string().contains("白名单"), "got: {}", err);
    }

    #[test]
    fn unknown_channel_rejected() {
        let bad = r#"
instance_id = "c"
[[shards]]
venue = "binance"
market = "spot"
channels = ["depth"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;
        let err = CollectorConfig::from_toml(bad).unwrap().resolve().unwrap_err();
        assert!(err.to_string().contains("未知通道"), "got: {}", err);
    }

    #[test]
    fn unknown_venue_rejected() {
        let bad = r#"
instance_id = "c"
[[shards]]
venue = "nosuch"
market = "spot"
channels = ["trade"]
[[shards.instruments]]
native_code = "X"
"#;
        let err = CollectorConfig::from_toml(bad).unwrap().resolve().unwrap_err();
        assert!(err.to_string().contains("未知 venue"), "got: {}", err);
    }

    #[test]
    fn duplicate_shard_rejected() {
        let bad = r#"
instance_id = "c"
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["trade"]
[[shards.instruments]]
native_code = "BTCUSDT"
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["trade"]
[[shards.instruments]]
native_code = "ETHUSDT"
"#;
        let err = CollectorConfig::from_toml(bad).unwrap().resolve().unwrap_err();
        assert!(err.to_string().contains("重复的分片"), "got: {}", err);
    }

    #[test]
    fn json_form_loads() {
        let json = serde_json::json!({
            "instance_id": "c",
            "shards": [{
                "venue": "okx",
                "market": "swap",
                "shard_idx": 0,
                "channels": ["funding"],
                "instruments": [{ "native_code": "BTC-USDT-SWAP" }]
            }]
        });
        let cfg = CollectorConfig::from_json(&json.to_string()).unwrap();
        let plan = cfg.resolve().unwrap();
        assert!(plan.instruments.contains_key("okx:swap:BTC-USDT-SWAP"));
    }

    #[test]
    fn market_to_account_type_maps_swap_to_futures_cross() {
        // dig3 的 futures TopicRegistry 以 FuturesCross 为注册键。
        assert_eq!(market_to_account_type("spot"), Some(AccountType::Spot));
        assert_eq!(market_to_account_type("swap"), Some(AccountType::FuturesCross));
        assert_eq!(market_to_account_type("nope"), None);
    }

    #[test]
    fn first_batch_venues_resolve() {
        assert_eq!(venue_to_exchange_id("binance"), Some(ExchangeId::Binance));
        assert_eq!(venue_to_exchange_id("OKX"), Some(ExchangeId::OKX));
        assert_eq!(venue_to_exchange_id("nope"), None);
    }

    #[test]
    fn core_set_matching_is_case_insensitive() {
        let cfg = CollectorConfig::from_toml(SAMPLE).unwrap();
        assert!(cfg.is_core("btcusdt"));
        assert!(cfg.is_core("BTCUSDT"));
        assert!(!cfg.is_core("SOLUSDT"));
    }
}
