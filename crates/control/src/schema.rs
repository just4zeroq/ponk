//! 八个配置域的文档结构（`docs/09-控制面.md` §3 的落地）。
//!
//! **线格式 = JSON**：09 §4.6 定明 `CtlMessage::ConfigSnapshot::payload` 是「该域文档本体」，
//! 且 03 §6 定明 `ctl.*` 固定 JSON、不开放切换；协议层 [`ponk_protocol::ConfigPayload`]
//! 因此取 `serde_json::Value`。故存储与下发一律 JSON，
//! §3 的 YAML 示例等价改写为 JSON（字段名与结构完全一致）。
//!
//! 文档里示例中的 `version: N` 字段（09 §3.2「域版本，由控制面在发布时赋值」）
//! **不由提交方提供**：[`crate::store::ConfigStore::publish`] 会在发布时把 `version`
//! 写入文档本体；提交方若带了该字段则以控制面赋值为准。
//!
//! 校验所需但 schema 层不表达的部分（跨域引用、DAG 无环）见 [`crate::validate`]。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

// ─────────────────────────────────────────────────────────────────────────────
// 共用：条件表达式（09 §3.3「条件 schema 两处共用」）
// ─────────────────────────────────────────────────────────────────────────────

/// 条件表达式（09 §3.3）：叶子 `{field, op, value}` / `all: [...]` / `any: [...]`。
///
/// 信号的 `emit.direction` 与告警的 `when` **共用同一份 schema**（09 §9-10）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Cond {
    /// AND（默认组合方式）。
    All {
        all: Vec<Cond>,
    },
    /// OR。
    Any {
        any: Vec<Cond>,
    },
    /// 叶子：对 `field` 施加 `op`（`>` `>=` `<` `<=` `==` `!=`）与 `value` 比较。
    Leaf {
        field: String,
        op: String,
        value: Value,
    },
}

impl Cond {
    /// 遍历所有叶子条件。
    pub fn leaves(&self, out: &mut Vec<(String, String)>) {
        match self {
            Cond::All { all } => all.iter().for_each(|c| c.leaves(out)),
            Cond::Any { any } => any.iter().for_each(|c| c.leaves(out)),
            Cond::Leaf { field, op, .. } => out.push((field.clone(), op.clone())),
        }
    }

    /// 操作符是否在 09 §3.3 允许集合内。
    pub fn ops_valid(&self) -> bool {
        let mut ops = Vec::new();
        self.leaves(&mut ops);
        ops.iter()
            .all(|(_, op)| matches!(op.as_str(), ">" | ">=" | "<" | "<=" | "==" | "!="))
    }

    /// 空的 `all` / `any` 无意义，判为非法。
    pub fn non_empty(&self) -> bool {
        match self {
            Cond::All { all } => !all.is_empty() && all.iter().all(Cond::non_empty),
            Cond::Any { any } => !any.is_empty() && any.iter().all(Cond::non_empty),
            Cond::Leaf { field, .. } => !field.is_empty(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ① SubscribePlan（09 §3.1；结构引用 03 §5.3）
// ─────────────────────────────────────────────────────────────────────────────

/// 订阅计划域文档：**输入**（标的候选集 / 分片参数 / 配额份额）。
///
/// 每实例的 `SubscribePlan` 是**派生物**（09 §3.1 两层结构），由本域 +
/// [`BlocklistDoc`] + [`ProcessCapacityDoc`] + 活实例清单确定性推导。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubscribePlanDoc {
    /// 每分片标的数上限 `shard_size`（09 §5.1：`shard_count = 2^ceil(log2(ceil(n/shard_size)))`）。
    #[serde(default = "default_shard_size")]
    pub shard_size: u64,
    /// 标的缺省通道集（`PlanInstrument::channels` 为空时取此）。
    #[serde(default)]
    pub default_channels: Vec<String>,
    /// 白名单标的候选集 + instrument meta（09 §2 / §4.2「instrument meta 随本域存放」）。
    #[serde(default)]
    pub instruments: Vec<PlanInstrument>,
    /// 配额份额（02 §6：按 `(venue, 出口 IP)` 切固定份额）。
    #[serde(default)]
    pub quota: Vec<QuotaSpec>,
}

fn default_shard_size() -> u64 {
    200
}

/// 白名单内的一个标的（09 §3.1 `ShardPlan.instruments` 的来源）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanInstrument {
    /// 交易所（`binance` / `okx` …）。
    pub venue: String,
    /// 市场类型（`spot` / `swap` …）。
    pub market: String,
    /// 交易所原生代码（canonical `venue:market:code` 的末段，02 §3）。
    pub native_code: String,
    /// 通道集；空 = 取 [`SubscribePlanDoc::default_channels`]。
    #[serde(default)]
    pub channels: Vec<String>,
    /// 价格精度（小数位）。
    #[serde(default)]
    pub price_precision: u32,
    /// 数量精度（小数位）。
    #[serde(default)]
    pub qty_precision: u32,
    /// 其余 instrument meta（开放字段）。
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
}

impl PlanInstrument {
    /// canonical 标的 id：`venue:market:native_code`（02 §3）。
    pub fn instrument_id(&self) -> String {
        format!("{}:{}:{}", self.venue, self.market, self.native_code)
    }
}

/// 一个 `(venue, 出口 IP)` 配额域的固定份额（02 §6）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSpec {
    /// 交易所。
    pub venue: String,
    /// 出口 IP。
    pub egress_ip: String,
    /// 采集角色份额（~0.50）。
    pub collect: f64,
    /// 回填角色份额（~0.35，不属于采集进程）。
    pub backfill: f64,
    /// 元数据角色份额（~0.15）。
    pub metadata: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// ② SignalRules（09 §3.2）
// ─────────────────────────────────────────────────────────────────────────────

/// 信号规则域。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalRulesDoc {
    /// 规则集合（一域一文档，09 §2）。
    #[serde(default)]
    pub rules: Vec<SignalRule>,
}

/// 一条信号规则 = 一个 DAG（09 §3.2）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalRule {
    /// → `SignalEvent.signal_id`，稳定 ID（告警按它匹配）。
    pub signal_id: String,
    /// → `SignalEvent.signal_name`。
    pub signal_name: String,
    /// → `SignalEvent.timeframe`（如 `4h`）。
    pub timeframe: String,
    /// 作用标的（canonical，02 §3）；空 = 全部（黑名单仍全局生效）。
    #[serde(default)]
    pub scope: Vec<String>,
    /// 输入主题：行情 + 因子并列（05 G5 / 07）。
    #[serde(default)]
    pub inputs: Vec<String>,
    /// 节点 = 算子；`in` = 上游节点 id（DAG 边）。
    #[serde(default)]
    pub graph: Vec<GraphArg>,
    /// 输出映射（→ `SignalEvent` 字段，09 §3.2）。
    pub emit: SignalEmit,
}

/// DAG 节点（算子调用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphArg {
    /// 节点 id（域内唯一，DAG 边的引用目标）。
    pub id: String,
    /// 算子名（05 §3 算子库；第一批清单待定，故控制面只校验结构不校验取值）。
    pub op: String,
    /// 算子参数（开放字段）。
    #[serde(default)]
    pub args: BTreeMap<String, Value>,
    /// 上游节点 id 列表（DAG 边）。
    #[serde(default, rename = "in")]
    pub incoming: Vec<String>,
}

/// 信号输出映射。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalEmit {
    /// → `SignalEvent.value`，阈值判定主判据（06 §2）。
    pub value: String,
    /// → `SignalEvent.payload`，告警可引用字段集（05 §4 / 06 §2）。
    #[serde(default)]
    pub payload: Vec<String>,
    /// → `SignalEvent.direction`（Long / Short / Flat）。
    #[serde(default)]
    pub direction: Option<DirectionEmit>,
}

/// `emit.direction`：三个分支条件 + 缺省方向（09 §3.2）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectionEmit {
    /// 多头分支条件。
    #[serde(default)]
    pub long: Option<Cond>,
    /// 空头分支条件。
    #[serde(default)]
    pub short: Option<Cond>,
    /// 缺省方向（`Long` / `Short` / `Flat`）。
    #[serde(default)]
    pub default: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// ③ AlertRules（09 §3.3）
// ─────────────────────────────────────────────────────────────────────────────

/// 告警规则域。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AlertRulesDoc {
    /// 告警规则集合。
    #[serde(default)]
    pub alerts: Vec<AlertRule>,
}

/// 一条告警规则（09 §3.3）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRule {
    /// → `AlarmEvent.rule_id`，稳定 ID。
    pub rule_id: String,
    /// 人读名。
    #[serde(default)]
    pub name: String,
    /// 匹配面（只对该信号 / 标的 / 方向 / 配置版本过滤）。
    #[serde(default, rename = "match")]
    pub match_spec: Option<AlertMatch>,
    /// 条件（引用信号事件 payload 字段；与信号 `emit.direction` 同 schema）。
    pub when: Cond,
    /// 时间窗内 N 次才判触发。
    #[serde(default)]
    pub window: Option<AlertWindow>,
    /// 级别（`Info` / `Warn` / `Critical` → `AlarmEvent.level`）。
    #[serde(default)]
    pub level: Option<String>,
    /// 去向；`channel` 引用 [`ChannelsDoc`] 的 id（09 §3.3）。
    #[serde(default)]
    pub route: Vec<Route>,
    /// 节流（如 `5m`）。
    #[serde(default)]
    pub throttle: Option<String>,
}

/// 告警匹配面。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AlertMatch {
    /// 只对该信号。
    #[serde(default)]
    pub signal_id: Option<String>,
    /// 按标的过滤（canonical）。
    #[serde(default)]
    pub instrument: Option<Vec<String>>,
    /// 按方向过滤。
    #[serde(default)]
    pub direction: Option<Vec<String>>,
}

/// 告警触发窗。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertWindow {
    /// 时间窗内命中 N 次才成立。
    pub occurrences: u32,
    /// 时间窗（如 `10m`）。
    pub within: String,
}

/// 告警去向。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// 渠道 id（→ [`Channel::id`]，09 §3.3）。
    pub channel: String,
    /// 渠道内目标（房间 / 群 / 号码）。
    #[serde(default)]
    pub target: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// ④ FactorRules（09 §3.5）
// ─────────────────────────────────────────────────────────────────────────────

/// 因子规则域。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FactorRulesDoc {
    /// 因子规则集合。
    #[serde(default)]
    pub factors: Vec<FactorRule>,
}

/// 一条因子规则（09 §3.5）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactorRule {
    /// 稳定 ID（→ `FactorEvent.factor_id`）。
    pub factor_id: String,
    /// 因子名。
    pub factor_name: String,
    /// 对齐周期（如 `1m`）。
    pub timeframe: String,
    /// 输入通道（07 §2）。
    #[serde(default)]
    pub inputs: Vec<String>,
    /// 计算窗口（如 `24h`）。
    pub window: String,
    /// 算子（与信号同一算子库，09 §3.5）。
    #[serde(default)]
    pub compute: Vec<GraphArg>,
    /// 输出映射。
    pub emit: FactorEmit,
}

/// 因子输出映射。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactorEmit {
    /// → `FactorEvent.value`。
    pub value: String,
    /// → `FactorEvent.meta`。
    #[serde(default)]
    pub meta: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// ⑤ Channels（09 §3.4）
// ─────────────────────────────────────────────────────────────────────────────

/// 渠道配置域。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelsDoc {
    /// 渠道集合。
    #[serde(default)]
    pub channels: Vec<Channel>,
}

/// 一个渠道后端（09 §3.4）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    /// 被 `AlertRules.route.channel` 引用。
    pub id: String,
    /// 适配器名：`feishu` / `dingtalk`（二期 `phone_sms` / `mail` / `webhook`）。
    pub backend: String,
    /// webhook / endpoint。
    pub endpoint: String,
    /// 重试。
    #[serde(default)]
    pub retry: Option<Retry>,
    /// 限额。
    #[serde(default)]
    pub limits: Option<Limits>,
}

/// 重试策略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Retry {
    /// 尝试次数。
    pub attempts: u32,
    /// 退避（如 `2s`）。
    pub backoff: String,
}

/// 渠道限额。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// 每分钟最大条数。
    pub rate_per_min: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// ⑥ ProcessCapacity（09 §3.5 / §5.2）
// ─────────────────────────────────────────────────────────────────────────────

/// 进程承载上限与指派参数域（**无外部消费者**，09 §2.2 C：不发通知）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessCapacityDoc {
    /// 承载上限。
    #[serde(default)]
    pub limits: CapacityLimits,
    /// 分布策略。
    #[serde(default)]
    pub distribution: Distribution,
}

/// 每进程承载上限（09 §5.2）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityLimits {
    /// 每进程最大分片数。
    #[serde(default = "default_max_shards")]
    pub max_shards_per_process: u64,
    /// 每进程最大标的数。
    #[serde(default = "default_max_instruments")]
    pub max_instruments_per_process: u64,
    /// 每进程最大通道数（连接数 ≈ 通道数，02 §4）。
    #[serde(default = "default_max_channels")]
    pub max_channels_per_process: u64,
}

fn default_max_shards() -> u64 {
    12
}
fn default_max_instruments() -> u64 {
    4000
}
fn default_max_channels() -> u64 {
    8
}

impl Default for CapacityLimits {
    fn default() -> Self {
        CapacityLimits {
            max_shards_per_process: default_max_shards(),
            max_instruments_per_process: default_max_instruments(),
            max_channels_per_process: default_max_channels(),
        }
    }
}

/// 分布策略（09 §5.3）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Distribution {
    /// `scatter` = 同所打散（默认）；`aggregate` = 同所聚合。
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// 打散顺序（`round_robin`）。
    #[serde(default = "default_order")]
    pub order: String,
}

fn default_strategy() -> String {
    "scatter".to_string()
}
fn default_order() -> String {
    "round_robin".to_string()
}

impl Default for Distribution {
    fn default() -> Self {
        Distribution {
            strategy: default_strategy(),
            order: default_order(),
        }
    }
}

/// 分布策略取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// 同所打散（默认，09 §5.3）。
    Scatter,
    /// 同所聚合。
    Aggregate,
}

impl Strategy {
    /// 解析（未知取值 → [`ControlError`](crate::error::ControlError)）。
    pub fn parse(s: &str) -> Result<Self, crate::error::ControlError> {
        match s {
            "scatter" => Ok(Strategy::Scatter),
            "aggregate" => Ok(Strategy::Aggregate),
            other => Err(crate::error::ControlError::validation(format!(
                "distribution.strategy 取值非法：`{other}`（允许 scatter / aggregate）"
            ))),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ⑦ Serialization（09 §3.5）
// ─────────────────────────────────────────────────────────────────────────────

/// 序列化格式域（粒度 = 主题族，03 §6 / 09 §2）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SerializationDoc {
    /// 主题族 → 格式（`json` / `messagepack` / `protobuf`）。
    #[serde(default)]
    pub families: BTreeMap<String, String>,
}

/// 可切换的格式取值（09 §2 / 03 §6）。
pub const SWITCHABLE_FORMATS: [&str; 3] = ["json", "messagepack", "protobuf"];

// ─────────────────────────────────────────────────────────────────────────────
// ⑧ InstrumentBlocklist（09 §3.5）
// ─────────────────────────────────────────────────────────────────────────────

/// 交易对黑名单域（09 §2.1：黑名单 ⊂ 白名单计算，独立成域以便独立审计与回滚）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BlocklistDoc {
    /// 黑名单条目。
    #[serde(default)]
    pub entries: Vec<BlockEntry>,
}

/// 一条黑名单条目（09 §3.5）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockEntry {
    /// canonical 标的（`venue:market:native_code`，02 §3）。
    pub instrument: String,
    /// 仅屏蔽的通道；**缺省 = 全通道**（09 §3.5）。
    #[serde(default)]
    pub channels: Vec<String>,
    /// 原因：`delisted` / `suspended` / `risk` / `manual`。
    #[serde(default)]
    pub reason: Option<String>,
    /// 生效时刻（RFC3339，如 `2026-10-01T00:00:00Z`）。
    #[serde(default)]
    pub since: Option<String>,
    /// 备注。
    #[serde(default)]
    pub note: Option<String>,
}

/// 黑名单原因取值（09 §3.5）。
pub const BLOCK_REASONS: [&str; 4] = ["delisted", "suspended", "risk", "manual"];

// ─────────────────────────────────────────────────────────────────────────────
// 时长解析（`10m` / `2s` / `24h` / `5m`，09 §3.3~§3.5）
// ─────────────────────────────────────────────────────────────────────────────

/// 解析 `Ns` / `Nm` / `Nh` / `Nd` 形式的时长为毫秒。
pub fn parse_duration_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num.parse().ok()?;
    let ms = match unit {
        "s" => n.checked_mul(1_000)?,
        "m" => n.checked_mul(60_000)?,
        "h" => n.checked_mul(3_600_000)?,
        "d" => n.checked_mul(86_400_000)?,
        "w" => n.checked_mul(604_800_000)?,
        _ => return None,
    };
    Some(ms)
}
