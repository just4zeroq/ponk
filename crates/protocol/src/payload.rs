//! 各事件家族的负载结构（03 §5.3 权威定义的落地）。
//!
//! 本模块是**全平台协议的唯一结构定义处**（03 §5.2「职责划分」）：
//! 修改这里任一结构 = 修改全平台协议，所有节点须重编译（高变更门槛）；
//! 常变明细请放进开放字段（`payload` / `meta` / `counters` / `detail`）。

use crate::envelope::Envelope;
use crate::ids::{instrument_id, Channel, Market, NodeId, Topic, Venue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ─────────────────────────────────────────────────────────────────────────────
// dig3 类型复用（03 §5.2 行情行 / 01 E5：「不自定义」「不在此重定义」）
// ─────────────────────────────────────────────────────────────────────────────

pub use digdigdig3_core::core::types::{
    AccountType, FundingRate, Kline, Liquidation, MarkPrice, OpenInterest, OrderBook, PublicTrade,
    SymbolInfo, Ticker, TradeSide,
};

/// 行情负载（03 §5.3 `MarketBody::data`）。
///
/// 01 E5 / 03 §5.2 要求行情负载**直接复用 dig3 `core::types::market_data`，不另造行情结构**。
/// dig3 侧该路径是一个**模块**（导出 `Kline` / `Ticker` / `PublicTrade` / `OrderBook` /
/// `FundingRate` / …，没有名为 `MarketData` 的聚合结构），因此这里只做**按通道的分派枚举**，
/// 每个变体的载荷**原样引用 dig3 类型**，本 crate 不重定义任何行情字段。
///
/// 变体集合对应 02 §2 的通道表（含标为二期的 OI / 清算）+ `md.meta`（09 §2.3）。
///
/// **数值**：dig3 类型的价量字段是 `f64`，03 §5.5 据此接受入站精度损失（见 [`MarketBody`]）。
///
/// 例外只有一处：[`MarketData::Meta`] 里的 [`CurrencyInfo::extra`] 是所方原始 JSON 透传，
/// 与行情价量的字符串约定无关；同变体里 `SymbolInfo` 的 `min_quantity` / `tick_size` 等
/// 仍是 dig3 的 `f64`，按 03 §5.5 已接受。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketData {
    /// 1m K 线（02 §2，`md.candle.1m`）。
    Kline(Kline),
    /// ticker（含 L1 bid/ask，02 §2：L1 即 ticker 副产品）。
    Ticker(Ticker),
    /// 全市场成交（02 §2，`md.trade`）。
    Trade(PublicTrade),
    /// L1 以外的深度（02 §2：仅核心集，一期可不做）。
    OrderBook(OrderBook),
    /// 资金费（02 §2，`md.funding`）。
    Funding(FundingRate),
    /// mark price（02 §2「资金费 / mark / index」）。
    MarkPrice(MarkPrice),
    /// 未平仓量（02 §2 标为二期）。
    OpenInterest(OpenInterest),
    /// 清算（02 §2 标为二期）。
    Liquidation(Liquidation),
    /// 交易对 / 币参考信息（`md.meta`，09 §2.3 定时任务的产物；03 §9-8）。
    ///
    /// **唯一一个平台自建的行情负载**：`SymbolInfo` 仍是 dig3 原样，`CurrencyInfo`
    /// 由平台自建（dig3 没有 `Currency` 类型）。
    #[serde(rename = "meta")]
    Meta(MetaPayload),
}

// ─────────────────────────────────────────────────────────────────────────────
// 公共枚举
// ─────────────────────────────────────────────────────────────────────────────

/// 信号建议方向（03 §5.3 `SignalEvent::direction`；05 §4：**不触发任何执行**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Long,
    Short,
    Flat,
}

/// 告警级别（03 §5.3 `AlarmEvent::level`；06 §4：决定默认路由与升级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

/// 节点种类（03 §5.3 `OpsHeartbeat::node_kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Collector,
    Signal,
    Storage,
    Alert,
    Factor,
    DataSvc,
    Control,
}

/// 进程状态（03 §5.3 `OpsHeartbeat::status`；10 §5 register / heartbeat / shutdown）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Startup,
    Running,
    Shutdown,
}

/// 订阅计划下发模式（03 §5.3 `SubscribePlan::mode`；10 §4.2 snapshot / diff）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    Snapshot,
    Diff,
}

// ─────────────────────────────────────────────────────────────────────────────
// 行情 md.*
// ─────────────────────────────────────────────────────────────────────────────

/// `md.meta` 负载（03 §5.3「`md.meta` 变体载 dig3 `SymbolInfo` + 平台 `CurrencyInfo`」；
/// 09 §2.3 定时任务的产物）。
///
/// 一条 `md.meta` 事件 = 一个标的的交易对信息 + 该标的相关的币信息，发到总线 →
/// 存储层订阅落库 → 数据服务可查（09 §2.3 产物第 3 路）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaPayload {
    /// 交易对信息 —— dig3 `SymbolInfo` 原样，**本层不重定义字段**
    /// （状态 / 价格与数量精度 / `tick_size` / `step_size` / 最小名义额 /
    /// `instrument_type` / `extra` 所方原始透传，09 §2.3）。
    pub symbol: SymbolInfo,
    /// 本次附带的币信息。
    ///
    /// 一个标的基础币 + 报价币通常 2 条，部分场景 0/1 条（例如该所未接币信息端点），
    /// 故用 `Vec` 而非 `Option` —— 后者在接线第二个币时就得改协议。
    #[serde(default)]
    pub currencies: Vec<CurrencyInfo>,
}

/// 币信息（**平台自建**，09 §2.3：「dig3 无对应类型 → 控制面自建轻量 `CurrencyInfo`」）。
///
/// dig3 没有任何 `Currency` 类型，经所方公开端点拉取（**具体端点按所核对**）。
/// 字段取"最小必要"：能表达 09 §2.3 列的「币种、名称、网络与充值精度、`extra` 原始透传」，
/// 其余所方差异一律进开放字段 `meta` / `extra`，不为此改协议。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrencyInfo {
    /// 币种代码（如 `USDT`）。
    pub currency: String,
    /// 币种名称（如 `Tether USD`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 可用的充值 / 提现网络（链）名，如 `TRC20` / `ERC20`。
    ///
    /// 逐网络的充值精度差异进 `meta`（如 `deposit_precision_trc20`），
    /// 原始值在 `extra` —— 避免为"每个所的网络字段长什么样"反复改协议。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<String>,
    /// 精度（小数位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<u32>,
    /// 开放字段（09 §2.3 的"轻量"就是靠它兜住所方差异）。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
    /// 所方原始透传 —— 03 §5.5 的例外：这是**参考元数据**不是行情价量，
    /// 原样保留 JSON，不做任何类型化（与 `SymbolInfo::extra` 同一处理）。
    #[serde(default)]
    pub extra: serde_json::Value,
}

impl CurrencyInfo {
    /// 只带币种代码的最小构造（其余字段走 serde 缺省）。
    pub fn new(currency: impl Into<String>) -> Self {
        CurrencyInfo {
            currency: currency.into(),
            name: None,
            networks: Vec::new(),
            precision: None,
            meta: BTreeMap::new(),
            extra: serde_json::Value::Null,
        }
    }
}

/// 行情家族头（03 §5.3 `MarketBody`）。
///
/// 行情专属字段（标的、交易所时间）**下移在家族体内而非公共信封头**（03 §5.1 / 01 E5）：
/// 非行情事件没有这些语义，放头部只能填占位值，会污染按 venue 的过滤与索引。
///
/// **数值**：`data` 沿用 dig3 类型，其价量是 `f64` —— 03 §5.5 明确「为此**接受入站
/// 精度损失**，与既有结论『数值等价』一致」。平台自建负载的价量类字段仍走十进制字符串。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketBody {
    /// 交易所（03 §5.3：`binance` / `okx` / …）。
    pub venue: Venue,
    /// 市场类型（03 §5.3：`spot` / `swap` / …）。
    pub market: Market,
    /// 交易所原生代码（canonical 见 02 §3）。
    pub native_code: String,
    /// 交易所事件时间（ms）。
    pub exchange_ts: u64,
    /// dig3 负载；不在此重定义行情结构（01 E5）。
    pub data: MarketData,
}

impl MarketBody {
    /// `venue:market:native_code`（02 §3）——本家族的身份键与分片键取值。
    pub fn instrument_id(&self) -> String {
        instrument_id(&self.venue, &self.market, &self.native_code)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 信号 signal.*
// ─────────────────────────────────────────────────────────────────────────────

/// 信号事件（03 §5.3 `SignalEvent`；语义见 05 §4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalEvent {
    /// 稳定 ID，告警规则据此匹配。
    pub signal_id: String,
    /// 规则名。
    pub signal_name: String,
    /// 规则配置版本（新旧信号可区分、可审计，09 §3）。
    pub version: String,
    /// canonical（`venue:market:native_code`，02 §3）。
    pub instrument: String,
    /// 建议方向（Long / Short / Flat），不触发执行。
    pub direction: Direction,
    /// 当前计算值，阈值判定主判据。
    pub value: f64,
    /// 规则对齐周期（如 `4h`）。
    pub timeframe: String,
    /// 交易所时间（ms，05 §4）。
    pub timestamp: u64,
    /// 因子值明细（开放字段，告警条件引用字段，05 §4）。
    pub payload: BTreeMap<String, f64>,
}

// ─────────────────────────────────────────────────────────────────────────────
// 因子 factor.*
// ─────────────────────────────────────────────────────────────────────────────

/// 因子事件（03 §5.3 `FactorEvent`；语义见 07）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorEvent {
    /// 稳定 ID。
    pub factor_id: String,
    /// 因子名（如 `btc_funding_discount`）。
    pub factor_name: String,
    /// 因子规则配置版本。
    pub version: String,
    /// canonical 标的。
    pub instrument: String,
    /// 对齐周期。
    pub timeframe: String,
    /// 因子值。
    pub value: f64,
    /// 时间（ms）。
    pub timestamp: u64,
    /// 开放明细字段。
    pub meta: BTreeMap<String, f64>,
}

// ─────────────────────────────────────────────────────────────────────────────
// 告警 alarm.*
// ─────────────────────────────────────────────────────────────────────────────

/// 告警事件（03 §5.3 `AlarmEvent`；语义见 06 §4）。
///
/// **不携带 venue / market / native_code / exchange_ts**（03 §5.1 两级信封）：
/// 平台级告警本就没有交易所与标的语义，需要标的时由回指的信号事件提供。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmEvent {
    /// 告警实例 ID = 幂等键（同规则同触发源去重 + 节流，06 §4）。
    pub alarm_id: String,
    /// 触发它的告警规则。
    pub rule_id: String,
    /// 若由信号触发，回指该信号。
    pub signal_id: Option<String>,
    /// 级别，决定默认路由与升级。
    pub level: Severity,
    /// 人读标题。
    pub title: String,
    /// 结构化上下文（渠道模板渲染用，开放字段）。
    pub detail: BTreeMap<String, String>,
    /// 触发时间（ms）。
    pub triggered_ts: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// 心跳 / 指标 ops.*
// ─────────────────────────────────────────────────────────────────────────────

/// 心跳 / 指标事件（03 §5.3 `OpsHeartbeat`；时序见 10 §5）。
///
/// ## 多域收敛约定（09 §4.4）
///
/// `applied_version` 是**单值**，表达不了 09 §2 的八个配置域。约定：
///
/// * `applied_version` 存**主域**——采集器 = `SubscribePlan::assignment_version`（10 §4.3），
///   控制面为 `None`（它是版本来源）；
/// * **逐域收敛判据**放进已有的开放字段 `counters`，键为 [`ConfigDomain::counter_key`]，
///   即 `cfg.<domain>`（如 `cfg.signal_rules`、`cfg.serialization`）。
///
/// 这是**纯约定，不改结构**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpsHeartbeat {
    /// 实例 ID（= [`NodeId`] 口径，10 §5）。
    pub instance_id: String,
    /// 实例所在机器（register 上报；控制面据此按 host 分散分片，09 §2.3）。
    ///
    /// 采集器在 `Startup` 心跳里填；其余节点类型可留空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// 节点种类。
    pub node_kind: NodeKind,
    /// 进程状态。
    pub status: Status,
    /// 已应用**主域**配置版本（收敛判据，10 §4.3：隐式 ACK；09 §4.4）。
    pub applied_version: Option<u64>,
    /// 在线计数器（积压、降级计数；开放字段）。
    pub counters: BTreeMap<String, i64>,
    /// 启动时间（ms）。
    pub started_at: u64,
    /// 心跳时间（ms）。
    pub ts: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// 命令 / 配置 ctl.*
// ─────────────────────────────────────────────────────────────────────────────

/// 精度等标的元数据 + 通道集（03 §5.3 `InstrumentMeta`；下发方为控制面，09 §2）。
///
/// **黑名单不进这里**：黑名单是独立配置域 [`ConfigDomain::InstrumentBlocklist`]（09 §2），
/// 指派阶段就已把黑名单标的从标的集剔除，所以送达采集的 `instruments` 里根本不含它们。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentMeta {
    /// 交易所原生代码。
    pub native_code: String,
    /// 价格精度（小数位）。
    pub price_precision: u32,
    /// 数量精度（小数位）。
    pub qty_precision: u32,
    /// 该标的要采的通道。
    pub channels: Vec<Channel>,
    /// `false` = 采集侧**临时停用**（运行时开关）。
    ///
    /// **不承担黑名单语义**（03 §5.3 注：黑名单是独立域，指派时已剔除）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 其余元数据（开放字段，09 §2「精度等 meta 随配置下发」）。
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
}

fn default_true() -> bool {
    true
}

/// 配额份额（03 §5.3 `QuotaShare`；口径见 02 §6、10 §4.2）。
///
/// > 03 未给出 `QuotaShare` 的字段级定义。02 §6 定案：控制面按 IP 切固定份额，
/// > 采集 ~50% / 回填 ~35% / 元数据 ~15%，配额域 = `(venue, 出口 IP)`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaShare {
    /// 配额域之一：交易所。
    pub venue: Venue,
    /// 配额域之二：出口 IP。
    pub egress_ip: String,
    /// 采集角色份额（~0.50）。
    pub collect: f64,
    /// 回填角色份额（~0.35；02 §6：该份额**不属于采集进程**，留给检查修复节点与元数据任务）。
    pub backfill: f64,
    /// 元数据角色份额（~0.15）。
    pub metadata: f64,
    /// 版本（与 `SubscribePlan::assignment_version` 同步下发）。
    pub version: u64,
}

/// 一个逻辑分片指派（03 §5.3 `ShardPlan`；分片单位 `(venue, market, shard_idx)`，02 §4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardPlan {
    /// 交易所。
    pub venue: Venue,
    /// 市场类型。
    pub market: Market,
    /// 分片序号。
    pub shard_idx: u32,
    /// 指派给本进程的标的（原生代码 + 精度 meta + 通道集）。
    pub instruments: Vec<InstrumentMeta>,
}

/// 订阅计划（03 §5.3 `SubscribePlan`；信道语义见 10 §4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribePlan {
    /// 单调递增；**旧版丢弃，新版整包应用**（10 §4.3）。
    pub assignment_version: u64,
    /// snapshot | diff。
    pub mode: PlanMode,
    /// 指派给本进程的分片，**可含多所**（02 §4：进程是承载容器，不是分片单位）。
    pub shards: Vec<ShardPlan>,
    /// 配额份额（可选，02 §6）。
    pub quota_share: Option<QuotaShare>,
}

/// 配置域全集（09 §2，八个域；**域即版本单位**：一域一文档，发布即该域版本 +1）。
///
/// 03 §5.3 引用了 `ConfigDomain` 但未给出取值；此处以 `docs/09 §2` 的八域登记为准。
///
/// 三个派生名，三处用法互不相同：
///
/// | 派生名 | 用法 | 例 |
/// |---|---|---|
/// | [`as_str`](ConfigDomain::as_str) / serde 线格式 | 配置域标识 | `signal_rules` |
/// | [`topic`](ConfigDomain::topic) | `ConfigChanged` 广播主题（09 §4.3） | `ctl.signal_rules` |
/// | [`counter_key`](ConfigDomain::counter_key) | 心跳逐域收敛键（09 §4.4） | `cfg.signal_rules` |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDomain {
    /// 订阅计划：标的候选集（白名单）、通道集、分片参数、instrument meta、配额份额。
    SubscribePlan,
    /// 信号规则：RuleGraph DAG、算子参数、作用标的、输出映射（05 §2）。
    SignalRules,
    /// 告警规则：match / when / window / level / route / throttle（06 §2）。
    AlertRules,
    /// 因子规则：输入通道、窗口、计算算子、输出因子（07 §4）。
    FactorRules,
    /// 渠道配置：飞书 / 钉钉 backend、endpoint 与凭据、重试与限额（06 §3）。
    Channels,
    /// 进程承载上限与指派参数（09 §2：**无外部消费者**，不发通知）。
    ProcessCapacity,
    /// 序列化格式（按主题族）；`ctl.*` / `ops.*` **固定 JSON，不开放切换**（03 §6 / 09 §2）。
    Serialization,
    /// 交易对黑名单：标的级排除集（下架 / 暂停 / 风险标的）。
    ///
    /// 取值与 09 §2 八域登记、03 §5.3 一致（wire = `instrument_blocklist`）。
    /// 兼容早期出现过的 `blacklist` 拼法（03 §5.3 一度如此登记），仅作反序列化别名。
    #[serde(alias = "blacklist")]
    InstrumentBlocklist,
}

impl ConfigDomain {
    /// 八域全集（09 §2 表序）。
    pub const ALL: [ConfigDomain; 8] = [
        ConfigDomain::SubscribePlan,
        ConfigDomain::SignalRules,
        ConfigDomain::AlertRules,
        ConfigDomain::FactorRules,
        ConfigDomain::Channels,
        ConfigDomain::ProcessCapacity,
        ConfigDomain::Serialization,
        ConfigDomain::InstrumentBlocklist,
    ];

    /// 线格式名（= 09 §2 第一列取值的 snake_case）。
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigDomain::SubscribePlan => "subscribe_plan",
            ConfigDomain::SignalRules => "signal_rules",
            ConfigDomain::AlertRules => "alert_rules",
            ConfigDomain::FactorRules => "factor_rules",
            ConfigDomain::Channels => "channels",
            ConfigDomain::ProcessCapacity => "process_capacity",
            ConfigDomain::Serialization => "serialization",
            ConfigDomain::InstrumentBlocklist => "instrument_blocklist",
        }
    }

    /// `ConfigChanged` 的广播主题 `ctl.<domain>`（09 §4.3 步骤 ②）。
    pub fn topic(&self) -> String {
        format!("ctl.{}", self.as_str())
    }

    /// 心跳逐域收敛键 `cfg.<domain>`（09 §4.4）。
    pub fn counter_key(&self) -> String {
        format!("cfg.{}", self.as_str())
    }
}

impl std::fmt::Display for ConfigDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 配置变更通知（03 §5.3 `ConfigChanged`）。
///
/// **只带域与版本，不携带配置本体**（09 §3：配置体量大，引擎按需拉取更稳）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigChanged {
    /// 配置域。
    pub domain: ConfigDomain,
    /// 配置版本。
    pub version: u64,
    /// 发布者（控制面实例）。
    pub publisher: String,
    /// 时间（ms）。
    pub ts: u64,
}

/// 拉取应答的配置本体（03 §5.3 `ConfigSnapshot::payload`，类型名 `ConfigPayload`）。
///
/// **取 JSON 文档**，不是按域的枚举。理由：
///
/// * 09 §4.6 明确「`payload: <JSON 文档>` 域文档本体；`ctl.*` 固定 JSON，故直接为 JSON」；
/// * 八域的 schema 分散在 09 §3.2~§3.5 的各域 YAML 里，若按域做强类型枚举，
///   协议层就得跟着各域 schema 演进反复改动 —— 正是 03 §5.2「改结构 = 全平台重编译」
///   想要避免的高变更门槛；
/// * 03 §5.3 注释「订阅计划域可直接用 `SubscribePlan`」由
///   [`ConfigPayload::from_subscribe_plan`] 承接。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConfigPayload(pub serde_json::Value);

impl ConfigPayload {
    /// 直接包一份 JSON 文档。
    pub fn new(value: serde_json::Value) -> Self {
        ConfigPayload(value)
    }

    /// 由结构体类型（如 [`SubscribePlan`]）构造：走 serde，任何可序列化类型都行。
    pub fn from_serializable<T: Serialize>(value: &T) -> Result<Self, crate::error::ProtocolError> {
        let v = serde_json::to_value(value)
            .map_err(|e| crate::error::ProtocolError::Serialize(e.to_string()))?;
        Ok(ConfigPayload(v))
    }

    /// 订阅计划域的便捷构造（03 §5.3「订阅计划域可直接用 `SubscribePlan`」）。
    pub fn from_subscribe_plan(
        plan: &SubscribePlan,
    ) -> Result<Self, crate::error::ProtocolError> {
        Self::from_serializable(plan)
    }

    /// 取底层 JSON 文档。
    pub fn as_json(&self) -> &serde_json::Value {
        &self.0
    }

    /// 拆成底层 JSON 文档。
    pub fn into_json(self) -> serde_json::Value {
        self.0
    }
}

impl From<serde_json::Value> for ConfigPayload {
    fn from(v: serde_json::Value) -> Self {
        ConfigPayload(v)
    }
}

/// 命令 / 配置联合体（03 §5.3 `Body::Ctl`；三变体见 09 §4.6 与 10 §4）。
///
/// 线格式为 serde 默认外部标签：`{"subscribe_plan":{…}}` /
/// `{"config_changed":{…}}` / `{"config_snapshot":{…}}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CtlMessage {
    /// 订阅计划：订阅计划域的强类型特例（03 §5.3 注）。
    ///
    /// 它同时承担实例级命令序号 `assignment_version`（10 §4.3），
    /// 与只带域+版本的 `ConfigChanged` / `ConfigSnapshot` 语义不同。
    SubscribePlan(SubscribePlan),
    /// 配置变更通知：**只带域与版本，不带本体**（03 §5.3 / 09 §3）。
    ConfigChanged(ConfigChanged),
    /// `ctl.request.<instance>` 拉取的应答：带配置本体（03 §5.3 / 09 §4.6）。
    ConfigSnapshot {
        /// 应答的配置域。
        domain: ConfigDomain,
        /// 该域版本（09 §4.1：域是唯一版本单位）。
        version: u64,
        /// 域文档本体（`ctl.*` 固定 JSON，故直接为 JSON 文档）。
        payload: ConfigPayload,
    },
}

impl CtlMessage {
    /// 身份键（03 §5.4：命令用 `assignment_version` 或 `domain+version`）。
    pub fn identity_key(&self) -> String {
        match self {
            CtlMessage::SubscribePlan(p) => p.assignment_version.to_string(),
            CtlMessage::ConfigChanged(c) => format!("{}+{}", c.domain.as_str(), c.version),
            CtlMessage::ConfigSnapshot { domain, version, .. } => {
                format!("{}+{}", domain.as_str(), version)
            }
        }
    }

    /// 构造拉取应答（09 §4.6）。
    pub fn config_snapshot(domain: ConfigDomain, version: u64, payload: ConfigPayload) -> Self {
        CtlMessage::ConfigSnapshot {
            domain,
            version,
            payload,
        }
    }

    /// 若是拉取应答则取出 `(域, 版本, 本体)`。
    pub fn as_config_snapshot(&self) -> Option<(ConfigDomain, u64, &ConfigPayload)> {
        match self {
            CtlMessage::ConfigSnapshot {
                domain,
                version,
                payload,
            } => Some((*domain, *version, payload)),
            _ => None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 死信 dlq.*
// ─────────────────────────────────────────────────────────────────────────────

/// 死信（03 §5.3 `DeadLetterMeta`；03 §7.3：发布失败的事件写入 `dlq.<topic>`）。
///
/// > 03 §5.3 在结构体末尾以注释 `// body = 原 envelope` 标注了原负载的归属，
/// > 按「原负载 + `DeadLetterMeta`」（03 §5.2 死信行）落地为内嵌字段 `body`。
/// > 该结构**不经 `Session::next()` 读取**（`next` 只返回 `Envelope`），
/// > 由 [`crate::Deserializer::deserialize_dead_letter`] 读取，见 03 §7.4「运维 CLI 查看」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterMeta {
    /// 原主题。
    pub origin_topic: Topic,
    /// 失败原因。
    pub error: String,
    /// 已尝试次数。
    pub attempts: u32,
    /// 失败时间（ms）。
    pub failed_ts: u64,
    /// 原 envelope（原负载）。
    pub body: Box<Envelope>,
}

/// 死信记录的另一名称。
///
/// 03 §4.1 的 [`DeadLetterReader`](crate::DeadLetterReader) 签名用 `DeadLetter`，
/// 03 §5.3 的结构定义用 `DeadLetterMeta` —— 两者指同一结构，这里是别名以免出现两份定义。
pub type DeadLetter = DeadLetterMeta;

impl DeadLetterMeta {
    /// 构造一条死信记录。
    pub fn new(origin_topic: Topic, error: impl Into<String>, body: Envelope) -> Self {
        Self {
            origin_topic,
            error: error.into(),
            attempts: 1,
            failed_ts: 0,
            body: Box::new(body),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 小工具
// ─────────────────────────────────────────────────────────────────────────────

/// 事件源（[`NodeId`]）便捷别名，供 02 §5 采集侧调用形态使用。
pub type Source = NodeId;
