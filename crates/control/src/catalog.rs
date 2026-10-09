//! 定时任务：交易对与币信息同步（`docs/09-控制面.md` §2.3）。
//!
//! | 项 | 约定 |
//! |---|---|
//! | 频率 | 每 **5 分钟**一轮，对 `exchange_market` 中 `enabled=1` 的每个 `(venue, market)` 各拉一次
//!       （`docs/14 §2`：**取接入台账，不取 `SubscribePlan` 域**——后者是采集意图，冷启动为空） |
//! | 交易对 | 经 dig3 连接器层 `get_exchange_info(AccountType) -> Vec<SymbolInfo>`（[`MetadataFetcher`] 抽象） |
//! | 币信息 | dig3 无 `Currency` 类型 → 用协议层自建的 [`CurrencyInfo`](ponk_protocol::CurrencyInfo)，经所方公开端点拉取（**端点按所核对，未核对的用 trait 抽象并标 TODO**） |
//! | 鉴权 | 全部公共端点，无密钥 |
//! | 配额 | 02 §6 的「元数据」份额（默认 15%），**与采集、回填份额互不串**；per-venue 令牌桶节流 |
//! | 变更检测 | 与上一轮 catalog **逐字段**比对；**无差异 = 零动作**（不 bump 域版本、不发事件、不重算指派） |
//!
//! ## 产物三路（09 §2.3）
//!
//! 1. **catalog = 控制面内部状态，不是配置域**——它是「所方事实」，与 `ProcessCapacity`
//!    同样不广播、无外部消费者；域里可变的部分仍是用户配置（白名单、`InstrumentBlocklist`）。
//!    **不新增第 9 个 `ConfigDomain`**（09 §2 是封闭八域）。
//! 2. **指派输入**：上新 / 下架 / 状态或精度变化 → 触发指派重算 → 以 `assignment_version`
//!    下发（09 §5.4 触发表新增行）；采集仍**不做发现**（02 §4），发现延迟 = 轮询周期 + 下发延迟。
//! 3. **`md.meta` 事件**：`Body::Market(MarketBody { data: MarketData::Meta(...) })`
//!    发到总线 → 存储层订阅落库（红线 4：经总线，**不直连存储**）。
//!
//! 币信息无 `native_code` 概念：它**随标的一起发**——`MetaPayload.currencies` 装该标的
//! `base_asset` / `quote_asset` 对应的币（03 §5.3 `MetaPayload` 就是这么定的）。
//! 只有币信息变化时，发给「base 或 quote 落在变更币集合里」的标的。

use ponk_protocol::{
    canonical_view, Body, CurrencyInfo, Envelope, EventType, MarketBody, MarketData, MetaPayload,
    SymbolInfo, Venue,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 同步周期（09 §2.3：每 5 分钟一轮）。
pub const META_SYNC_INTERVAL_MS: u64 = 5 * 60 * 1000;

// ─────────────────────────────────────────────────────────────────────────────
// 错误
// ─────────────────────────────────────────────────────────────────────────────

/// 元数据拉取错误。
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum MetaError {
    /// 该所的端点尚未核对（09 §2.3「具体端点按所核对」→ 未核对即标 TODO）。
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// 拉取失败（网络 / 所方限流 / 响应畸形）。
    #[error("fetch failed: {0}")]
    Fetch(String),

    /// 事件构造失败（协议层信封校验）。
    #[error("protocol: {0}")]
    Protocol(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// 拉取器（可注入假实现，测试离线）
// ─────────────────────────────────────────────────────────────────────────────

/// 交易对 / 币信息拉取器。
///
/// **为什么仍是 trait 而不直接用 dig3**：本 trait 是同步的，而 dig3 是 async ——
/// 桥接（局部 Runtime + 超时）与**平台语义归一化**都属于实现细节，不该渗进
/// `run_round` 的同步控制流。更实际的原因：三个集成测试靠注入假实现保持**全离线**。
///
/// **真实现**：[`crate::Dig3Fetcher`]，按 09 §2.4 统一经 dig3 连接器层
/// （`get_exchange_info` / `get_currencies`），transport 归 dig3、语义归本仓。
/// 未接线时由 [`UnwiredFetcher`] 兜底，全部 `Unsupported` → catalog 零动作。
pub trait MetadataFetcher: Send + Sync {
    /// 拉取某 `(venue, market)` 的交易对信息（dig3 `get_exchange_info(AccountType)`）。
    fn fetch_symbols(&self, venue: &str, market: &str) -> Result<Vec<SymbolInfo>, MetaError>;

    /// 拉取某所的币信息（dig3 `get_currencies`，`docs/09 §2.4`）。
    ///
    /// 该方法在 dig3 侧带默认 `NotImplemented`；**没有公开端点的所**（如 Binance，
    /// 其 coin config 需签名）由连接器返回 `WireAbsent` → 本 trait 的
    /// [`MetaError::Unsupported`]，本轮该所币信息不更新。
    fn fetch_currencies(&self, venue: &str) -> Result<Vec<CurrencyInfo>, MetaError>;
}

/// 缺省拉取器：**未接线**——全部返回 `Unsupported`，因此 catalog 不变化、零动作。
#[derive(Debug, Default, Clone, Copy)]
pub struct UnwiredFetcher;

impl MetadataFetcher for UnwiredFetcher {
    fn fetch_symbols(&self, venue: &str, _market: &str) -> Result<Vec<SymbolInfo>, MetaError> {
        Err(MetaError::Unsupported(format!(
            "{venue}: dig3 `get_exchange_info` 适配器未接线（见交付报告缺口清单）"
        )))
    }

    fn fetch_currencies(&self, _venue: &str) -> Result<Vec<CurrencyInfo>, MetaError> {
        Err(MetaError::Unsupported(
            "币信息拉取器未接线（09 §2.4：接 [`crate::Dig3Fetcher`]）".to_string(),
        ))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// catalog（控制面内部状态）
// ─────────────────────────────────────────────────────────────────────────────

/// `(venue, market)` 的字符串键（`venue|market`），便于落盘与日志。
pub fn vm_key(venue: &str, market: &str) -> String {
    format!("{venue}|{market}")
}

/// 解析 [`vm_key`]。
pub fn split_vm_key(key: &str) -> Option<(String, String)> {
    let (v, m) = key.split_once('|')?;
    Some((v.to_string(), m.to_string()))
}

/// 所方事实的快照（**不是配置域**，09 §2.3 产物第 1 路）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    /// `venue|market` → `native_code` → 交易对信息。
    pub symbols: BTreeMap<String, BTreeMap<String, SymbolInfo>>,
    /// `venue` → `currency code` → 币信息。
    pub currencies: BTreeMap<String, BTreeMap<String, CurrencyInfo>>,
    /// 最近一次成功写入的时刻（ms）。
    pub updated_at: u64,
}

impl Catalog {
    /// 该 `(venue, market)` 是否已有数据。
    pub fn has_vm(&self, venue: &str, market: &str) -> bool {
        self.symbols.contains_key(&vm_key(venue, market))
    }

    /// 标的数（诊断）。
    pub fn symbol_count(&self) -> usize {
        self.symbols.values().map(|m| m.len()).sum()
    }
}

/// 与上一轮 catalog 的**逐字段**比对结果（09 §2.3「无差异则零动作」的判据）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogDiff {
    /// 新增标的：`venue|market|native_code`。
    pub symbols_added: Vec<String>,
    /// 下架标的。
    pub symbols_removed: Vec<String>,
    /// 变更标的 → 变更的字段名（状态 / 精度 / tick_size …）。
    pub symbols_changed: Vec<(String, Vec<String>)>,
    /// 新增币。
    pub currencies_added: Vec<String>,
    /// 消失的币。
    pub currencies_removed: Vec<String>,
    /// 变更币 → 变更字段名。
    pub currencies_changed: Vec<(String, Vec<String>)>,
}

impl CatalogDiff {
    /// 是否**零差异**——零差异必须零动作（不 bump 版本、不发事件、不重算）。
    pub fn is_empty(&self) -> bool {
        self.symbols_added.is_empty()
            && self.symbols_removed.is_empty()
            && self.symbols_changed.is_empty()
            && self.currencies_added.is_empty()
            && self.currencies_removed.is_empty()
            && self.currencies_changed.is_empty()
    }

    /// 是否触发指派重算（09 §5.4：上新/下架/状态或精度变化）。
    ///
    /// 纯币信息变化**不改标的集**，不触发重算（但仍要发 `md.meta`）。
    pub fn triggers_reassignment(&self) -> bool {
        !(self.symbols_added.is_empty()
            && self.symbols_removed.is_empty()
            && self.symbols_changed.is_empty())
    }

    /// 汇总为一行摘要（日志用）。
    pub fn summary(&self) -> String {
        format!(
            "标的 +{} -{} ~{}；币 +{} -{} ~{}",
            self.symbols_added.len(),
            self.symbols_removed.len(),
            self.symbols_changed.len(),
            self.currencies_added.len(),
            self.currencies_removed.len(),
            self.currencies_changed.len()
        )
    }
}

/// 两个可序列化值的**逐字段**差异（按字段名给出；嵌套值用规范视图比较，
/// 与键序无关——03 §5.4 的规范化视图在此复用）。
pub fn changed_fields<T: Serialize>(a: &T, b: &T) -> Vec<String> {
    let va = serde_json::to_value(a).ok();
    let vb = serde_json::to_value(b).ok();
    match (va, vb) {
        (Some(Value::Object(am)), Some(Value::Object(bm))) => {
            let mut keys: BTreeSet<String> = BTreeSet::new();
            keys.extend(am.keys().cloned());
            keys.extend(bm.keys().cloned());
            keys.into_iter()
                .filter(|k| {
                    let x = am.get(k).cloned().unwrap_or(Value::Null);
                    let y = bm.get(k).cloned().unwrap_or(Value::Null);
                    view(&x) != view(&y)
                })
                .collect()
        }
        (Some(x), Some(y)) => {
            if view(&x) == view(&y) {
                Vec::new()
            } else {
                vec!["(root)".to_string()]
            }
        }
        _ => vec!["(serialize error)".to_string()],
    }
}

use serde_json::Value;

fn view(v: &Value) -> String {
    canonical_view(v)
}

/// 旧 → 新 的目录差异。
pub fn diff_catalog(old: &Catalog, new: &Catalog) -> CatalogDiff {
    let mut d = CatalogDiff::default();

    for (vk, new_map) in &new.symbols {
        match old.symbols.get(vk) {
            None => {
                for code in new_map.keys() {
                    d.symbols_added.push(format!("{vk}|{code}"));
                }
            }
            Some(old_map) => {
                for (code, ins) in new_map {
                    match old_map.get(code) {
                        None => d.symbols_added.push(format!("{vk}|{code}")),
                        Some(before) => {
                            let fields = changed_fields(before, ins);
                            if !fields.is_empty() {
                                d.symbols_changed.push((format!("{vk}|{code}"), fields));
                            }
                        }
                    }
                }
                for code in old_map.keys() {
                    if !new_map.contains_key(code) {
                        d.symbols_removed.push(format!("{vk}|{code}"));
                    }
                }
            }
        }
    }
    for vk in old.symbols.keys() {
        if !new.symbols.contains_key(vk) {
            for code in old.symbols[vk].keys() {
                d.symbols_removed.push(format!("{vk}|{code}"));
            }
        }
    }

    for (venue, new_map) in &new.currencies {
        match old.currencies.get(venue) {
            None => {
                for c in new_map.keys() {
                    d.currencies_added.push(format!("{venue}|{c}"));
                }
            }
            Some(old_map) => {
                for (code, cur) in new_map {
                    match old_map.get(code) {
                        None => d.currencies_added.push(format!("{venue}|{code}")),
                        Some(before) => {
                            let fields = changed_fields(before, cur);
                            if !fields.is_empty() {
                                d.currencies_changed.push((format!("{venue}|{code}"), fields));
                            }
                        }
                    }
                }
                for code in old_map.keys() {
                    if !new_map.contains_key(code) {
                        d.currencies_removed.push(format!("{venue}|{code}"));
                    }
                }
            }
        }
    }
    for venue in old.currencies.keys() {
        if !new.currencies.contains_key(venue) {
            for c in old.currencies[venue].keys() {
                d.currencies_removed.push(format!("{venue}|{c}"));
            }
        }
    }

    d.symbols_added.sort();
    d.symbols_removed.sort();
    d.symbols_changed.sort();
    d.currencies_added.sort();
    d.currencies_removed.sort();
    d.currencies_changed.sort();
    d
}

// ─────────────────────────────────────────────────────────────────────────────
// 节流：02 §6「元数据」份额（默认 15%），per-venue 令牌桶
// ─────────────────────────────────────────────────────────────────────────────

/// 元数据份额配额（02 §6：采集 ~50% / 回填 ~35% / **元数据 ~15%**，
/// 三者**互不串**——这里只用元数据那一份）。
#[derive(Debug, Clone, PartialEq)]
pub struct MetadataQuota {
    /// 元数据份额（来自 `SubscribePlan` 域 `quota[].metadata`，缺省 0.15）。
    pub share: f64,
    /// 基准速率（请求 / 分钟）。控制面自身运行参数，不构成配置域（09 §4.4）。
    pub base_requests_per_minute: f64,
}

impl Default for MetadataQuota {
    fn default() -> Self {
        MetadataQuota {
            share: 0.15,
            base_requests_per_minute: 60.0,
        }
    }
}

impl MetadataQuota {
    /// 元数据份额对应的有效速率（请求 / ms）。
    pub fn rate_per_ms(&self) -> f64 {
        (self.share.max(0.0)) * (self.base_requests_per_minute.max(0.0)) / 60_000.0
    }
}

/// 每所一个的令牌桶（09 §2.3「用一个 per-(venue) 令牌桶约束节流」）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBucket {
    /// 令牌补充速率（个 / ms）。
    rate_per_ms: f64,
    /// 桶容量（允许的突发）。
    capacity: f64,
    /// 当前令牌数。
    tokens: f64,
    /// 上次结算时刻（ms）。
    last_ms: u64,
}

impl TokenBucket {
    /// 新建：初始满桶。
    pub fn new(rate_per_ms: f64, capacity: f64, now_ms: u64) -> Self {
        TokenBucket {
            rate_per_ms: rate_per_ms.max(0.0),
            capacity: capacity.max(0.0),
            tokens: capacity.max(0.0),
            last_ms: now_ms,
        }
    }

    /// 按流逝时间补足令牌后尝试取一个；取到返回 `true`。
    pub fn try_acquire(&mut self, now_ms: u64) -> bool {
        let elapsed = now_ms.saturating_sub(self.last_ms) as f64;
        self.last_ms = now_ms;
        self.tokens = (self.tokens + elapsed * self.rate_per_ms).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// 当前令牌数（测试 / 诊断）。
    pub fn tokens(&self) -> f64 {
        self.tokens
    }

    /// 更新补充速率（份额或基准速率变更时同步）。
    pub fn set_rate(&mut self, rate_per_ms: f64) {
        self.rate_per_ms = rate_per_ms.max(0.0);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 同步一轮
// ─────────────────────────────────────────────────────────────────────────────

/// 被跳过的拉取。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skip {
    /// 交易所。
    pub venue: String,
    /// 市场（币信息拉取为 `None`）。
    pub market: Option<String>,
    /// 原因（`throttled` / `unsupported` / `fetch failed: …`）。
    pub reason: String,
}

/// 一轮同步的产出。
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    /// 本轮是否真的执行（`false` = 未到 5 分钟周期 → 零动作）。
    pub ran: bool,
    /// catalog 是否有差异（`false` = **零动作**）。
    pub changed: bool,
    /// 触发了指派重算（09 §5.4）。
    pub recompute_ran: bool,
    /// 逐字段差异。
    pub diff: CatalogDiff,
    /// 被跳过的拉取（节流 / 端点未核对 / 拉取失败）。
    pub skipped: Vec<Skip>,
    /// 待发到 `md.meta` 的信封（产物第 3 路）。
    pub md_meta: Vec<Envelope>,
    /// 本轮成功拉到的交易对条数。
    pub fetched_symbols: usize,
    /// 本轮成功拉到的币条数。
    pub fetched_currencies: usize,
    /// 本轮落库的 `sync_run.id`（`docs/12 §4.1`「每轮一条」）；`None` = 未落库。
    pub sync_run_id: Option<i64>,
    /// 落库失败原因（**不影响**比对与触发语义，只如实汇报）。
    pub persist_error: Option<String>,
}

impl SyncOutcome {
    /// 未到周期（零动作）。
    pub fn not_due() -> Self {
        SyncOutcome::default()
    }
}

/// catalog 持久化文件名（控制面内部状态，非配置域）。
const CATALOG_FILE: &str = "catalog.json";

/// 每所令牌桶的**突发容量**：一轮要打的请求数上限
/// = 1 次币信息 + 该所各 `(venue, market)` 的交易对拉取（09 §2.3）。
///
/// 取 64 是给「一个所挂几十个交易对市场」留的余量；超过时该所本轮会被
/// [`TokenBucket`] 节流并记入 `skipped`，下一轮补拉——不会丢状态，只延后发现。
pub const DEFAULT_META_BURST: u32 = 64;

/// 定时任务：每 5 分钟同步一轮交易对与币信息。
pub struct CatalogSync {
    /// 同步周期（ms，默认 300_000）。
    pub interval_ms: u64,
    /// 元数据配额（02 §6）。
    pub quota: MetadataQuota,
    /// 每所令牌桶的突发容量（默认 [`DEFAULT_META_BURST`]）。
    pub burst: f64,
    catalog: Catalog,
    buckets: BTreeMap<String, TokenBucket>,
    last_round_ms: u64,
    /// 一轮尚未收尾（**一轮未完成不重入**，09 §2.3）。
    running: bool,
    root: Option<PathBuf>,
    /// 目录表落库句柄（懒建；`docs/12 §4.1` 的 `instrument` / `venue_asset` / `sync_run`）。
    db: Option<std::sync::Arc<crate::sqlite::SqliteStore>>,
    /// 落库被跳过（未配置 `root`，或建库失败后不再重试）。
    db_disabled: bool,
}

impl CatalogSync {
    /// `root = Some(dir)` 时启用落盘（控制面重启后避免「空 → 满」的假变更风暴）。
    pub fn new(root: Option<PathBuf>) -> Self {
        let mut s = CatalogSync {
            interval_ms: META_SYNC_INTERVAL_MS,
            quota: MetadataQuota::default(),
            burst: DEFAULT_META_BURST as f64,
            catalog: Catalog::default(),
            buckets: BTreeMap::new(),
            last_round_ms: 0,
            running: false,
            root: root.clone(),
            db: None,
            db_disabled: root.is_none(),
        };
        if let Some(dir) = root {
            let path = dir.join("state").join(CATALOG_FILE);
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(c) = serde_json::from_slice::<Catalog>(&bytes) {
                    s.catalog = c;
                }
            }
        }
        s
    }

    /// 当前 catalog（只读）。
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// 最近一轮的时刻（ms）。
    pub fn last_round_ms(&self) -> u64 {
        self.last_round_ms
    }

    /// 一轮是否尚未收尾（**一轮未完成不重入**的显式状态）。
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// 是否到了同步周期（09 §2.3：每 5 分钟一轮）。
    pub fn due(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_round_ms) >= self.interval_ms
    }

    /// 某所的元数据份额（02 §6 `quota[].metadata`，缺省 0.15）。
    fn share_of(quotas: &[crate::schema::QuotaSpec], venue: &str, fallback: f64) -> f64 {
        quotas
            .iter()
            .find(|q| q.venue == venue)
            .map(|q| q.metadata)
            .unwrap_or(fallback)
    }

    /// 某所的令牌桶（懒建）；速率 = 该所元数据份额 × 基准速率（与采集/回填互不串）。
    fn bucket(&mut self, venue: &str, share: f64, now: u64) -> &mut TokenBucket {
        let rate = (share.max(0.0)) * self.quota.base_requests_per_minute.max(0.0) / 60_000.0;
        let burst = self.burst.max(1.0);
        let bucket = self
            .buckets
            .entry(venue.to_string())
            .or_insert_with(|| TokenBucket::new(rate, burst, now));
        bucket.set_rate(rate);
        bucket
    }

    /// 执行一轮同步（**一轮未完成不重入**）。
    ///
    /// 三道闸，任一未过即零动作返回：
    ///
    /// 1. `running` —— 上一轮尚未收尾（`&mut self` 使同步重入本来就不可行，
    ///    这里把它显式化，异步化改造时也不会漏）；
    /// 2. [`due`](CatalogSync::due) —— 未到 5 分钟周期；
    /// 3. 轮次时刻 `last_round_ms` 在**轮首**即前移 —— 拉取全部失败 / 被节流的
    ///    「未完成轮」同样占掉整个周期，不会被立刻重试刷爆所方端点。
    ///
    /// * 拉取失败 / 节流 → 记入 `skipped`，**不中断整轮**；
    /// * 逐字段无差异 → `changed = false`，调用方**不得** bump 版本 / 发事件 / 重算指派。
    pub fn run(
        &mut self,
        targets: &[(String, String)],
        quotas: &[crate::schema::QuotaSpec],
        fetcher: &dyn MetadataFetcher,
        now: u64,
    ) -> Result<SyncOutcome, MetaError> {
        if self.running || !self.due(now) {
            return Ok(SyncOutcome::not_due());
        }
        self.running = true;
        self.last_round_ms = now;
        let t0 = std::time::Instant::now();
        let out = self.run_round(targets, quotas, fetcher, now);
        self.running = false;
        // `docs/12 §4.1` 落库路径：每轮一条 `sync_run` + 逐条 upsert 比对 `row_hash`。
        // **只加落库**：比对 / 触发 / 零动作语义完全由上面的 `run_round` 决定，
        // 这里不改变 `SyncOutcome` 的 `ran` / `changed` / `recompute_ran` / `diff`。
        let mut out = out;
        self.persist_round(now, &mut out);

        // 关键步骤日志：轮次收尾一行摘要（成败、差异、耗时、轮次号），排查时先看这行。
        match &out {
            Ok(o) => {
                let mut line = format!(
                    "[sync] 轮次结束 ran={} changed={} recompute={}；{}；拉取 标的 {} 币 {}，跳过 {}；{} ms",
                    o.ran,
                    o.changed,
                    o.recompute_ran,
                    o.diff.summary(),
                    o.fetched_symbols,
                    o.fetched_currencies,
                    o.skipped.len(),
                    t0.elapsed().as_millis(),
                );
                if let Some(id) = o.sync_run_id {
                    line.push_str(&format!("；sync_run#{id}"));
                }
                if let Some(pe) = &o.persist_error {
                    line.push_str(&format!("；⚠ 落库失败：{pe}"));
                }
                eprintln!("{line}");
            }
            Err(e) => eprintln!(
                "[sync] 轮次失败（{} ms）：{e}",
                t0.elapsed().as_millis()
            ),
        }
        out
    }

    /// 取落库句柄（懒建；失败后不再重试，避免每轮刷错误）。
    fn open_db(&mut self) -> Option<std::sync::Arc<crate::sqlite::SqliteStore>> {
        if self.db_disabled {
            return None;
        }
        if self.db.is_none() {
            let dir = self.root.clone()?;
            match crate::sqlite::SqliteStore::open(&dir) {
                Ok(s) => self.db = Some(std::sync::Arc::new(s)),
                Err(e) => {
                    eprintln!("[control] 目录表落库建库失败（本轮不落库）：{e}");
                    self.db_disabled = true;
                    return None;
                }
            }
        }
        self.db.clone()
    }

    /// 把本轮结果写进 `sync_run` / `instrument` / `instrument_change` / `venue_asset`。
    ///
    /// 落库失败**不改变**同步语义（不回滚、不重试、不改 `SyncOutcome` 的比对结论），
    /// 只把原因记进 `SyncOutcome::persist_error`。
    fn persist_round(
        &mut self,
        started_at: u64,
        out: &mut Result<SyncOutcome, MetaError>,
    ) {
        let finished_at = crate::now_ms().max(started_at);
        let diff = match out {
            Ok(o) => o.diff.clone(),
            Err(_) => CatalogDiff::default(),
        };
        let error = match out {
            Err(e) => Some(e.to_string()),
            Ok(_) => None,
        };
        let outcome_json = match out {
            Ok(o) => self.round_summary(o, finished_at),
            Err(e) => serde_json::json!({ "error": e.to_string(), "ts": finished_at }),
        };
        let Some(store) = self.open_db() else { return };
        let catalog = self.catalog.clone();
        match crate::catalog_repo::persist_catalog_round(
            &store,
            &catalog,
            &diff,
            started_at,
            finished_at,
            &outcome_json,
            error.as_deref(),
        ) {
            Ok(id) => {
                if let Ok(o) = out {
                    o.sync_run_id = Some(id);
                }
            }
            Err(e) => {
                let msg = format!("目录表落库失败（12 §4.1）：{e}");
                eprintln!("[control] {msg}");
                if let Ok(o) = out {
                    o.persist_error = Some(msg);
                }
            }
        }
    }

    /// `sync_run.outcome`（12 §3.4：「逐所结果」）——外加轮次级摘要。
    fn round_summary(&self, o: &SyncOutcome, ts: u64) -> serde_json::Value {
        use serde_json::json;
        let mut venues: std::collections::BTreeMap<String, (usize, usize, usize)> =
            std::collections::BTreeMap::new();
        for (vm, map) in &self.catalog.symbols {
            if let Some((v, _)) = split_vm_key(vm) {
                venues.entry(v).or_default().0 += map.len();
            }
        }
        for (venue, map) in &self.catalog.currencies {
            venues.entry(venue.clone()).or_default().1 = map.len();
        }
        for key in o
            .diff
            .symbols_added
            .iter()
            .chain(o.diff.symbols_removed.iter())
            .chain(o.diff.symbols_changed.iter().map(|(k, _)| k))
        {
            if let Some((vm, _)) = key.rsplit_once('|') {
                if let Some((v, _)) = vm.split_once('|') {
                    if let Some(e) = venues.get_mut(v) {
                        e.2 += 1;
                    }
                }
            }
        }
        let venues_json: serde_json::Map<String, serde_json::Value> = venues
            .into_iter()
            .map(|(v, (instruments, currencies, changed))| {
                (
                    v,
                    json!({
                        "instruments": instruments,
                        "currencies": currencies,
                        "changed": changed,
                    }),
                )
            })
            .collect();
        json!({
            "ran": o.ran,
            "changed": o.changed,
            "recompute_ran": o.recompute_ran,
            "summary": o.diff.summary(),
            "diff": o.diff,
            "skipped": o.skipped,
            "fetched_symbols": o.fetched_symbols,
            "fetched_currencies": o.fetched_currencies,
            "venues": venues_json,
            "ts": ts,
        })
    }

    fn run_round(
        &mut self,
        targets: &[(String, String)],
        quotas: &[crate::schema::QuotaSpec],
        fetcher: &dyn MetadataFetcher,
        now: u64,
    ) -> Result<SyncOutcome, MetaError> {
        let mut outcome = SyncOutcome {
            ran: true,
            ..SyncOutcome::default()
        };
        if targets.is_empty() {
            // 这是**最容易被漏看的零动作原因**：接入台账里没有启用的市场。
            eprintln!("[sync] 轮次目标为空 —— `exchange`/`exchange_market` 没有 enabled=1 的 (venue, market)，本轮零动作");
            return Ok(outcome); // 接入台账没有启用市场 → 零动作（`docs/14 §2`）
        }

        let listed: Vec<String> = targets.iter().map(|(v, m)| format!("{v}/{m}")).collect();
        eprintln!(
            "[sync] 轮次开始：{} 个目标 —— {}",
            targets.len(),
            listed.join(", ")
        );

        let mut next = self.catalog.clone();

        // ① 币信息：每所每轮一次
        let mut venues: BTreeSet<String> = BTreeSet::new();
        for (v, _) in targets {
            venues.insert(v.clone());
        }
        for venue in &venues {
            let share = Self::share_of(quotas, venue, self.quota.share);
            if !self.bucket(venue, share, now).try_acquire(now) {
                outcome.skipped.push(Skip {
                    venue: venue.clone(),
                    market: None,
                    reason: "throttled: 元数据份额令牌桶耗尽（02 §6）".to_string(),
                });
                continue;
            }
            match fetcher.fetch_currencies(venue) {
                Ok(list) => {
                    let n = list.len();
                    outcome.fetched_currencies += n;
                    let map: BTreeMap<String, CurrencyInfo> = list
                        .into_iter()
                        .map(|c| (c.currency.clone(), c))
                        .collect();
                    next.currencies.insert(venue.clone(), map);
                    eprintln!("[sync]   {venue} 币信息 → {n} 条");
                }
                Err(e) => {
                    eprintln!("[sync]   {venue} 币信息 → 跳过：{e}");
                    outcome.skipped.push(Skip {
                        venue: venue.clone(),
                        market: None,
                        reason: e.to_string(),
                    });
                }
            }
        }

        // ② 交易对：每个 (venue, market) 各拉一次
        for (venue, market) in targets {
            let share = Self::share_of(quotas, venue, self.quota.share);
            if !self.bucket(venue, share, now).try_acquire(now) {
                outcome.skipped.push(Skip {
                    venue: venue.clone(),
                    market: Some(market.clone()),
                    reason: "throttled: 元数据份额令牌桶耗尽（02 §6）".to_string(),
                });
                continue;
            }
            match fetcher.fetch_symbols(venue, market) {
                Ok(list) => {
                    let n = list.len();
                    outcome.fetched_symbols += n;
                    let map: BTreeMap<String, SymbolInfo> = list
                        .into_iter()
                        .map(|s| (s.symbol.clone(), s))
                        .collect();
                    next.symbols.insert(vm_key(venue, market), map);
                    eprintln!("[sync]   {venue}/{market} → {n} 个标的");
                }
                Err(e) => {
                    eprintln!("[sync]   {venue}/{market} → 跳过：{e}");
                    outcome.skipped.push(Skip {
                        venue: venue.clone(),
                        market: Some(market.clone()),
                        reason: e.to_string(),
                    });
                }
            }
        }

        outcome.diff = diff_catalog(&self.catalog, &next);

        // 硬要求：**无差异 = 零动作**
        if outcome.diff.is_empty() {
            eprintln!("[sync] 与上一轮 catalog 零差异 → 零动作（不 bump 版本 / 不发事件 / 不重算指派）");
            return Ok(outcome);
        }

        eprintln!("[sync] 检出差异：{} → 进入写库与触发阶段", outcome.diff.summary());
        outcome.changed = true;
        outcome.md_meta = self.build_md_meta(&self.catalog, &next, &outcome.diff, now)?;
        self.catalog = next;
        self.catalog.updated_at = now;
        self.persist()?;
        Ok(outcome)
    }

    fn persist(&self) -> Result<(), MetaError> {
        let Some(dir) = self.root.as_ref().map(|r| r.join("state")) else {
            return Ok(());
        };
        std::fs::create_dir_all(&dir).map_err(|e| MetaError::Fetch(e.to_string()))?;
        let path = dir.join(CATALOG_FILE);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.catalog)
            .map_err(|e| MetaError::Fetch(e.to_string()))?;
        std::fs::write(&tmp, bytes).map_err(|e| MetaError::Fetch(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| MetaError::Fetch(e.to_string()))
    }

    /// 由差异构造 `md.meta` 信封（产物第 3 路：**经总线**，不直连存储，红线 4）。
    fn build_md_meta(
        &self,
        _old: &Catalog,
        new: &Catalog,
        diff: &CatalogDiff,
        now: u64,
    ) -> Result<Vec<Envelope>, MetaError> {
        // 需要发的标的：新增 + 变更
        let mut want: BTreeSet<(String, String, String)> = BTreeSet::new(); // (venue, market, code)
        for key in diff.symbols_added.iter().chain(
            diff.symbols_changed.iter().map(|(k, _)| k),
        ) {
            if let Some((vm, code)) = key.rsplit_once('|') {
                if let Some((v, m)) = split_vm_key(vm) {
                    want.insert((v, m, code.to_string()));
                }
            }
        }

        // 纯币信息变化 → 发给 base/quote 落在变更币集合里的标的
        if !diff.currencies_added.is_empty()
            || !diff.currencies_removed.is_empty()
            || !diff.currencies_changed.is_empty()
        {
            let changed: BTreeSet<String> = diff
                .currencies_added
                .iter()
                .chain(diff.currencies_removed.iter())
                .chain(diff.currencies_changed.iter().map(|(k, _)| k))
                .filter_map(|k| k.split_once('|').map(|(_, c)| c.to_string()))
                .collect();
            for (vm, map) in &new.symbols {
                let Some((venue, market)) = split_vm_key(vm) else {
                    continue;
                };
                for (code, sym) in map {
                    if changed.contains(&sym.base_asset) || changed.contains(&sym.quote_asset) {
                        want.insert((venue.clone(), market.clone(), code.clone()));
                    }
                }
            }
        }

        let mut out = Vec::new();
        for (venue, market, code) in want {
            let Some(map) = new.symbols.get(&vm_key(&venue, &market)) else {
                continue;
            };
            let Some(symbol) = map.get(&code) else {
                continue;
            };
            // 币信息无 native_code：随标的一起发，装该标的 base/quote 对应的币
            let currencies: Vec<CurrencyInfo> = new
                .currencies
                .get(&venue)
                .map(|m| {
                    [symbol.base_asset.clone(), symbol.quote_asset.clone()]
                        .into_iter()
                        .filter_map(|c| m.get(&c).cloned())
                        .collect()
                })
                .unwrap_or_default();

            let env = Envelope::new(
                EventType::Market,
                "control", // 09 §2.3：控制面是 md.meta 的生产者
                now,
                Body::Market(MarketBody {
                    venue: Venue::new(venue.clone()),
                    market: ponk_protocol::Market::new(market.clone()),
                    native_code: code.clone(),
                    exchange_ts: now,
                    data: MarketData::Meta(MetaPayload {
                        symbol: symbol.clone(),
                        currencies,
                    }),
                }),
            )
            .map_err(|e| MetaError::Protocol(e.to_string()))?;
            out.push(env);
        }
        Ok(out)
    }
}

/// 当前时刻（ms）。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
