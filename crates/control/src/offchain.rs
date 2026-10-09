//! 外部（币信息）同步任务 —— `docs/14-目录类页面与币信息.md` §3 / §5 / §8。
//!
//! 与 [`crate::catalog`] 的**交易所同步**是两个节拍、两种失败语义：
//!
//! | | 交易所同步（`catalog.rs`） | 外部（币）同步（本模块） |
//! |---|---|---|
//! | 节拍 | **5 分钟**（`META_SYNC_INTERVAL_MS`） | **1 小时**（[`OFFCHAIN_SYNC_INTERVAL_MS`]） |
//! | 目标 | `exchange_market` 中 `enabled=1` 的 `(venue, market)` | catalog 中**全部去重**的币 |
//! | 记录 | `sync_run` | **`offchain_sync_run`**（独立表） |
//! | 失败隔离 | 交易所目录必须新鲜 | CG 限速/失败**不得**伪装成交易所同步失败 |
//!
//! **为什么分离**（`docs/14 §3`）：交易所目录是"事实必须新鲜"（影响采集与黑名单判定），
//! 币的供应量/社交信息是"参考数据"，时效性差两个数量级；绑在同一轮里，
//! 要么 CG 拖慢目录同步，要么 CG 失败污染 `sync_run` 的语义。
//!
//! ## 字段分层（决定请求量）
//!
//! | 字段 | 策略 |
//! |---|---|
//! | 排名 / 发行量 / 流通量 | **每轮**拉 `/coins/markets`（分页 `coin_market_page_size`/页 → 4~6 请求） |
//! | 链 / 合约地址 / Twitter / 主页 | **首次出现时拉一次** + **低频轮转**（每轮 `coin_detail_batch` 个） |
//! | 股票类交易对 | **完全跳过**（`docs/14 §6`） |
//!
//! ## 匹配三级（`docs/14 §5.2`）：宁可"未收录"不静默错配
//!
//! | 情况 | 处理 |
//! |---|---|
//! | `symbol` 大小写不敏感**精确唯一**命中 | 自动 `external_id = <id>`，`match_status = 'matched'` |
//! | **多命中**（同 symbol 多个项目） | 不自动选，`match_status = 'ambiguous'`，页面列候选由人点选 |
//! | **零命中** | `external_id = NULL`，`match_status = 'not_found'`，允许手填 |
//!
//! **不做**模糊匹配与"剥数字前缀"的归一化——`1INCH` 会被误伤，且错配会**静默污染**
//! （把 A 币的供应量贴到 B 币上，页面看起来"有数据"却无从察觉）。

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::catalog_repo::{DetailAssetPatch, LightAssetPatch, NewContract};
use crate::sqlite::SqliteStore;

/// 外部同步周期（`docs/14 §3`：每 1 小时一轮），可被 `[providers].offchain_interval` 覆盖。
pub const OFFCHAIN_SYNC_INTERVAL_MS: u64 = 60 * 60 * 1000;

/// 缺省 `/coins/markets` 分页大小（CoinGecko 上限 250）。
pub const DEFAULT_COIN_MARKET_PAGE_SIZE: u32 = 250;
/// 缺省每轮轮转拉取的详情币数（≈900/20 ≈ 一周扫完一轮）。
pub const DEFAULT_COIN_DETAIL_BATCH: usize = 20;
/// 缺省详情拉取重试次数（**只对可重试的传输错误**；429 不重试）。
pub const DEFAULT_COIN_DETAIL_MAX_RETRIES: u32 = 2;
/// 缺省单次请求超时（ms）。
pub const DEFAULT_COIN_TIMEOUT_MS: u64 = 10_000;

/// 一次市场索引拉取的页数上限（40 × 250 = 10000 条），防上游异常导致失控翻页。
const MAX_MARKET_PAGES: u32 = 40;

// ─────────────────────────────────────────────────────────────────────────────
// 配置（`docs/14 §8` 的 `[providers]` 段）
// ─────────────────────────────────────────────────────────────────────────────

/// 外部数据源配置（`control.toml [providers]`，`docs/14 §8`）。
///
/// **不进八域**：与密钥同一处理（`docs/03` / `docs/13 §5.1`）——
/// 它们是连接凭据与调优参数，不是平台配置。
#[derive(Debug, Clone, PartialEq)]
pub struct OffchainConfig {
    /// `coingecko`（主源）| `coinmarketcap`（备选，骨架）。
    pub coin: String,
    /// 可选 API key（CG Demo 档无需；CMC 必填）。**只进配置文件，不进八域、不进环境变量。**
    pub coin_api_key: Option<String>,
    /// 外部同步节拍（ms）。
    pub offchain_interval_ms: u64,
    /// `/coins/markets` 分页大小。
    pub coin_market_page_size: u32,
    /// 每轮轮转拉取的详情币数。
    pub coin_detail_batch: usize,
    /// 详情拉取重试次数。
    pub coin_detail_max_retries: u32,
    /// 单次请求超时（ms）。
    pub coin_timeout_ms: u64,
}

impl Default for OffchainConfig {
    fn default() -> Self {
        OffchainConfig {
            coin: "coingecko".to_string(),
            coin_api_key: None,
            offchain_interval_ms: OFFCHAIN_SYNC_INTERVAL_MS,
            coin_market_page_size: DEFAULT_COIN_MARKET_PAGE_SIZE,
            coin_detail_batch: DEFAULT_COIN_DETAIL_BATCH,
            coin_detail_max_retries: DEFAULT_COIN_DETAIL_MAX_RETRIES,
            coin_timeout_ms: DEFAULT_COIN_TIMEOUT_MS,
        }
    }
}

/// 解析时长字面量 → 毫秒（口径与 `--jwt-ttl` 一致：`8h` / `30m` / `45s` / 裸数字 = 秒）。
pub fn parse_duration_ms(raw: &str) -> Result<u64, String> {
    let t = raw.trim();
    let (num, mult) = match t.strip_suffix('h') {
        Some(v) => (v, 3_600_000u64),
        None => match t.strip_suffix('m') {
            Some(v) => (v, 60_000),
            None => match t.strip_suffix('s') {
                Some(v) => (v, 1_000),
                None => (t, 1_000),
            },
        },
    };
    let n: u64 = num.trim().parse().map_err(|_| {
        format!("取值不合法：`{raw}`（可用 1h / 30m / 45s / 3600，与 jwt_ttl 同口径）")
    })?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("时长过大：`{raw}`"))
}

// ─────────────────────────────────────────────────────────────────────────────
// 错误
// ─────────────────────────────────────────────────────────────────────────────

/// 外部数据源拉取错误。
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// 上游限速（CG 免费档 30 req/min）——记进 `offchain_sync_run.rate_limited`。
    #[error("rate limited: {0}")]
    RateLimited(String),

    /// 该源未实现 / 未接线。
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// 拉取失败（网络 / 停服 / 响应畸形）。
    #[error("fetch failed: {0}")]
    Fetch(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// 数据形状
// ─────────────────────────────────────────────────────────────────────────────

/// `/coins/markets` 的一条（轻量字段层）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MarketEntry {
    /// 外部源 coin id（如 `bitcoin`）。
    pub id: String,
    /// 交易代码（如 `BTC`）——匹配用的键。
    pub symbol: String,
    pub name: Option<String>,
    /// 排名（`market_cap_rank`），进 `extra.light`。
    pub market_cap_rank: Option<i64>,
    /// 发行总量（十进制字符串）。
    pub total_supply: Option<String>,
    /// 流通量（十进制字符串）。
    pub circulating_supply: Option<String>,
    /// 原始条目（存 `asset.extra.light`，`docs/14 §4` 判断 2）。
    pub raw: Value,
}

/// `/coins/{id}` 的一条（详情层：链 / 合约 / Twitter）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CoinDetail {
    /// 外部源 coin id。
    pub id: String,
    /// 链 → 合约地址（CG `platforms`）。
    pub platforms: BTreeMap<String, String>,
    /// Twitter 账号名（CG `links.twitter_screen_name`）。
    pub twitter_screen_name: Option<String>,
    /// 原始响应（存 `asset.extra.detail`）。
    pub raw: Value,
}

impl CoinDetail {
    /// `twitter_screen_name` → 完整主页 URL；无账号则 `None`。
    pub fn twitter_url(&self) -> Option<String> {
        self.twitter_screen_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("https://twitter.com/{s}"))
    }
}

/// 三级匹配结果（`docs/14 §5.2`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    /// 精确唯一命中 → 自动 `external_id`。
    Unique(String),
    /// 多命中（同 symbol 多个项目）→ `ambiguous`，**不自动选**。
    Ambiguous(Vec<String>),
    /// 零命中（含股票、缩放前缀如 `1000PEPE`）→ `not_found`，页面可手填。
    NotFound,
}

/// 市场索引：`symbol → ids` 与 `id → 条目`（`docs/14 §5.2`「索引来源」）。
#[derive(Debug, Clone, Default)]
pub struct MarketIndex {
    by_symbol: BTreeMap<String, Vec<String>>,
    by_id: BTreeMap<String, MarketEntry>,
}

impl MarketIndex {
    /// 由 `/coins/markets` 全量条目建索引（`symbol` **大小写不敏感**）。
    pub fn build(entries: &[MarketEntry]) -> Self {
        let mut by_symbol: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut by_id: BTreeMap<String, MarketEntry> = BTreeMap::new();
        for e in entries {
            let key = e.symbol.trim().to_uppercase();
            if key.is_empty() || e.id.is_empty() {
                continue;
            }
            let ids = by_symbol.entry(key).or_default();
            if !ids.contains(&e.id) {
                ids.push(e.id.clone());
            }
            by_id.insert(e.id.clone(), e.clone());
        }
        for ids in by_symbol.values_mut() {
            ids.sort();
        }
        MarketIndex { by_symbol, by_id }
    }

    /// 三级匹配：**只做精确匹配**（不模糊、不剥前缀）。
    pub fn lookup(&self, code: &str) -> MatchOutcome {
        let key = code.trim().to_uppercase();
        match self.by_symbol.get(&key).map(|v| v.as_slice()) {
            None | Some([]) => MatchOutcome::NotFound,
            Some(ids) if ids.len() == 1 => MatchOutcome::Unique(ids[0].clone()),
            Some(ids) => MatchOutcome::Ambiguous(ids.to_vec()),
        }
    }

    /// 按外部 id 取条目（轻量字段更新用）。
    pub fn entry(&self, id: &str) -> Option<&MarketEntry> {
        self.by_id.get(id)
    }

    /// 索引条数（诊断 / 测试）。
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 拉取器（可注入假实现，测试全离线）
// ─────────────────────────────────────────────────────────────────────────────

/// 外部币信息拉取器。
///
/// **抽象原因**：真现实现要打外网（`docs/14 §5.1`），而测试必须**全离线**；
/// 另一个源（CoinMarketCap）只留骨架。控制面只依赖本 trait，
/// 测试注入假实现覆盖精确唯一 / 多命中 / 零命中 / 限速 / 轮转等分支。
pub trait OffchainProvider: Send + Sync {
    /// 外部源名（写 `asset.detail_source`：`coingecko` / `coinmarketcap`）。
    fn source(&self) -> &'static str;

    /// `/coins/markets` 第 `page` 页（**1 起**），每页 `per_page` 条。
    ///
    /// 返回条数 `< per_page`（或空页）表示已到末页。
    fn fetch_markets_page(&self, page: u32, per_page: u32) -> Result<Vec<MarketEntry>, ProviderError>;

    /// `/coins/{id}` 详情（平台/合约、Twitter、原始响应）。
    fn fetch_coin_detail(&self, id: &str) -> Result<CoinDetail, ProviderError>;
}

/// 缺省拉取器：**未接线**——全部返回 `Unsupported`，本轮记进 `offchain_sync_run` 后结束。
#[derive(Debug, Default, Clone, Copy)]
pub struct UnwiredProvider;

impl OffchainProvider for UnwiredProvider {
    fn source(&self) -> &'static str {
        "none"
    }

    fn fetch_markets_page(&self, _page: u32, _per_page: u32) -> Result<Vec<MarketEntry>, ProviderError> {
        Err(ProviderError::Unsupported(
            "外部币源未接线（见 control.toml [providers].coin）".to_string(),
        ))
    }

    fn fetch_coin_detail(&self, _id: &str) -> Result<CoinDetail, ProviderError> {
        Err(ProviderError::Unsupported(
            "外部币源未接线（见 control.toml [providers].coin）".to_string(),
        ))
    }
}

/// 分页拉全量市场索引（`docs/14 §5.2`「4~6 请求一次」）。
///
/// 两种停止条件：短页/空页 = 正常到尾；**新增 id 不再增长** = 上游翻页位移
/// （或假实现不认 page 参数）时立即止步，避免空转 `MAX_MARKET_PAGES` 次。
pub fn fetch_market_index(
    provider: &dyn OffchainProvider,
    per_page: u32,
) -> Result<Vec<MarketEntry>, ProviderError> {
    let per_page = per_page.clamp(1, 250);
    let mut all: Vec<MarketEntry> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut page: u32 = 1;
    while page <= MAX_MARKET_PAGES {
        let batch = provider.fetch_markets_page(page, per_page)?;
        let batch_len = batch.len();
        let mut fresh = 0usize;
        for e in batch {
            if seen.insert(e.id.clone()) {
                fresh += 1;
            }
            all.push(e);
        }
        if fresh == 0 {
            break; // 上游不翻页 / 位移重页 → 已经拿到全部去重后的条目
        }
        if batch_len < per_page as usize {
            break;
        }
        page += 1;
    }
    // 按 id 保留最后一条（同 id 取最新页的条目）
    let mut by_id: BTreeMap<String, MarketEntry> = BTreeMap::new();
    for e in all {
        by_id.insert(e.id.clone(), e);
    }
    Ok(by_id.into_values().collect())
}

/// 带重试的详情拉取：`max_retries` 是**首次尝试之后**的重试次数。
///
/// **429 不重试**——立刻重打只会让限速更严重；由调用方中断本轮详情轮转。
fn fetch_detail_with_retry(
    provider: &dyn OffchainProvider,
    id: &str,
    max_retries: u32,
) -> Result<CoinDetail, ProviderError> {
    let mut attempt = 0u32;
    loop {
        match provider.fetch_coin_detail(id) {
            Err(ProviderError::Fetch(_)) if attempt < max_retries => attempt += 1,
            other => return other,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CoinGecko 适配器
// ─────────────────────────────────────────────────────────────────────────────

/// CoinGecko 适配器（`docs/14 §5.1` 主源；Demo 档无需 API key，30 req/min）。
///
/// HTTP 客户端选 `reqwest::blocking`：见交付报告的选型说明——本 workspace 已因
/// `ponk-collect → digdigdig3` 编译了同一版本的 `reqwest`，复用它**不新增依赖树**。
pub struct CoinGecko {
    client: reqwest::blocking::Client,
    base: String,
    api_key: Option<String>,
}

impl CoinGecko {
    /// 构造（**不联网**，只建客户端）。
    pub fn new(timeout_ms: u64, api_key: Option<String>, base: Option<String>) -> Result<Self, ProviderError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(timeout_ms.max(1_000)))
            .user_agent(concat!("ponk-control/", env!("CARGO_PKG_VERSION"), " (docs/14)"))
            .build()
            .map_err(|e| ProviderError::Fetch(format!("构建 HTTP 客户端失败：{e}")))?;
        Ok(CoinGecko {
            client,
            base: base
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://api.coingecko.com/api/v3".to_string()),
            api_key: api_key.filter(|s| !s.trim().is_empty()),
        })
    }

    fn get_json(&self, path_and_query: &str) -> Result<Value, ProviderError> {
        let url = format!("{}{}", self.base, path_and_query);
        let mut req = self.client.get(&url);
        if let Some(k) = &self.api_key {
            req = req.header("x-cg-demo-api-key", k);
        }
        let resp = req.send().map_err(|e| ProviderError::Fetch(format!("{url}：{e}")))?;
        let status = resp.status();
        if status.as_u16() == 429 {
            return Err(ProviderError::RateLimited(format!(
                "CoinGecko 429（免费档 30 req/min）：{url}"
            )));
        }
        if !status.is_success() {
            return Err(ProviderError::Fetch(format!(
                "HTTP {status}：{url}"
            )));
        }
        resp.json::<Value>()
            .map_err(|e| ProviderError::Fetch(format!("{url}：响应不是合法 JSON：{e}")))
    }
}

/// 十进制字符串化（`docs/03 §5.5` 保真；CG 给的是 JSON 数字）。
///
/// 整数值走整数文本（`19000000000000` 而不是 `1.9e13`）；非整数保留小数。
fn supply_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() { None } else { Some(t.to_string()) }
        }
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                return Some(u.to_string());
            }
            if let Some(i) = n.as_i64() {
                return Some(i.to_string());
            }
            let f = n.as_f64()?;
            if !f.is_finite() {
                return None;
            }
            if f.fract() == 0.0 && f.abs() < 1e18 {
                Some(format!("{f:.0}"))
            } else {
                Some(f.to_string())
            }
        }
        _ => None,
    }
}

fn parse_market_entry(v: &Value) -> Option<MarketEntry> {
    let id = v.get("id")?.as_str()?.to_string();
    let symbol = v.get("symbol")?.as_str()?.to_string();
    Some(MarketEntry {
        id,
        symbol,
        name: v.get("name").and_then(Value::as_str).map(str::to_string),
        market_cap_rank: v.get("market_cap_rank").and_then(Value::as_i64),
        total_supply: supply_str(v.get("total_supply")),
        circulating_supply: supply_str(v.get("circulating_supply")),
        raw: v.clone(),
    })
}

impl OffchainProvider for CoinGecko {
    fn source(&self) -> &'static str {
        "coingecko"
    }

    fn fetch_markets_page(
        &self,
        page: u32,
        per_page: u32,
    ) -> Result<Vec<MarketEntry>, ProviderError> {
        let path = format!(
            "/coins/markets?vs_currency=usd&order=market_cap_desc&per_page={per_page}&page={page}&sparkline=false"
        );
        let v = self.get_json(&path)?;
        let arr = v
            .as_array()
            .ok_or_else(|| ProviderError::Fetch("`/coins/markets` 返回的不是数组".to_string()))?;
        Ok(arr.iter().filter_map(parse_market_entry).collect())
    }

    fn fetch_coin_detail(&self, id: &str) -> Result<CoinDetail, ProviderError> {
        // 只要我们要的字段：`platforms`（链+合约）与 `links.twitter_screen_name`
        let path = format!(
            "/coins/{}?localization=false&tickers=false&market_data=false\
             &community_data=false&developer_data=false&sparkline=false&desc=false",
            urlencode(id)
        );
        let v = self.get_json(&path)?;
        let mut platforms = BTreeMap::new();
        if let Some(Value::Object(m)) = v.get("platforms") {
            for (chain, addr) in m {
                if let Some(a) = addr.as_str() {
                    let a = a.trim();
                    if !a.is_empty() {
                        platforms.insert(chain.clone(), a.to_string());
                    }
                }
            }
        }
        let twitter = v
            .pointer("/links/twitter_screen_name")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(CoinDetail {
            id: id.to_string(),
            platforms,
            twitter_screen_name: twitter,
            raw: v,
        })
    }
}

/// 最小 URL 转义（coin id 只含 `[a-z0-9-]`，但路径段仍要防注入）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// CoinMarketCap 骨架（`docs/14 §5.1` 备选）
// ─────────────────────────────────────────────────────────────────────────────

/// CoinMarketCap 骨架：**留位不实现**——需要注册 key 且免费档有月度额度，
/// 覆盖面也不如 CG（小币、部分 RWA）。`[providers] coin = "coinmarketcap"` 时
/// 本轮会以 `Unsupported` 记进 `offchain_sync_run`，**不影响**交易所同步。
#[derive(Debug, Default, Clone)]
pub struct CoinMarketCap {
    api_key: Option<String>,
}

impl CoinMarketCap {
    /// 构造。
    pub fn new(api_key: Option<String>) -> Self {
        CoinMarketCap { api_key }
    }
}

impl OffchainProvider for CoinMarketCap {
    fn source(&self) -> &'static str {
        "coinmarketcap"
    }

    fn fetch_markets_page(&self, _page: u32, _per_page: u32) -> Result<Vec<MarketEntry>, ProviderError> {
        let _ = &self.api_key;
        Err(ProviderError::Unsupported(
            "coinmarketcap 适配器未实现（docs/14 §5.1 备选源；先用 coingecko）".to_string(),
        ))
    }

    fn fetch_coin_detail(&self, _id: &str) -> Result<CoinDetail, ProviderError> {
        Err(ProviderError::Unsupported(
            "coinmarketcap 适配器未实现（docs/14 §5.1 备选源；先用 coingecko）".to_string(),
        ))
    }
}

/// 按 `[providers].coin` 构造适配器（`coingecko` | `coinmarketcap`）。
pub fn build_provider(cfg: &OffchainConfig) -> Result<std::sync::Arc<dyn OffchainProvider>, ProviderError> {
    match cfg.coin.trim().to_ascii_lowercase().as_str() {
        "coingecko" | "cg" | "" => Ok(std::sync::Arc::new(CoinGecko::new(
            cfg.coin_timeout_ms,
            cfg.coin_api_key.clone(),
            None,
        )?)),
        "coinmarketcap" | "cmc" => Ok(std::sync::Arc::new(CoinMarketCap::new(
            cfg.coin_api_key.clone(),
        ))),
        other => Err(ProviderError::Unsupported(format!(
            "未知外部币源 `{other}`（可用：coingecko / coinmarketcap）"
        ))),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 同步一轮
// ─────────────────────────────────────────────────────────────────────────────

/// 一轮外部同步的产出（落 `offchain_sync_run.outcome`）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct OffchainOutcome {
    /// 本轮是否真的执行（`false` = 未到周期 / 上一轮未收尾 → 零动作）。
    pub ran: bool,
    /// 本轮纳入外部同步目标的币数。
    pub targets: usize,
    /// 被排除的币数（`asset.kind = 'stock'` 或只出现在 stock 类交易所的交易对上）。
    pub excluded: usize,
    /// 市场索引条数（`/coins/markets` 全量）。
    pub fetched: usize,
    /// 精确唯一命中（含已手填 `external_id` 的）。
    pub matched: usize,
    /// 多命中 → `ambiguous`（不自动选，页面列候选）。
    pub ambiguous: usize,
    /// 零命中 → `not_found`。
    pub not_found: usize,
    /// 本轮写过轻量字段的币数。
    pub light_updated: usize,
    /// 本轮成功拉到详情的币数。
    pub detail_pulled: usize,
    /// 本轮详情拉取失败的币数（**不中断整轮**）。
    pub detail_failed: usize,
    /// 本轮写入的合约地址条数。
    pub contracts: usize,
    /// 上游限速（记进 `offchain_sync_run`，页面显示"上轮被限速"）。
    pub rate_limited: bool,
    /// 本轮结束原因（限速 / 拉取失败）；成功轮为 `None`。
    pub error: Option<String>,
    /// 本轮落库的 `offchain_sync_run.id`。
    pub offchain_run_id: Option<i64>,
    /// 落库失败原因（不影响同步语义，只如实汇报）。
    pub persist_error: Option<String>,
}

impl OffchainOutcome {
    /// 未到周期（零动作）。
    pub fn not_due() -> Self {
        OffchainOutcome::default()
    }

    /// 本轮结果对象：`offchain_sync_run.outcome` 与
    /// `POST /sync/offchain/runs` 的 `outcome` **必须是同一份**
    /// （`docs/14 §9` 前端按同构解析）。
    ///
    /// `rate_limited` 是**计数**（0 = 本轮未被限速），不是布尔——接口契约如此。
    pub fn outcome_value(&self, ts: u64) -> Value {
        json!({
            "targets": self.targets,
            "excluded": self.excluded,
            "fetched": self.fetched,
            "matched": self.matched,
            "ambiguous": self.ambiguous,
            "not_found": self.not_found,
            "light_updated": self.light_updated,
            "detail_pulled": self.detail_pulled,
            "detail_failed": self.detail_failed,
            "contracts": self.contracts,
            "rate_limited": if self.rate_limited { 1 } else { 0 },
            "summary": self.summary(),
            "ts": ts,
        })
    }

    /// 一行摘要（日志用）。
    pub fn summary(&self) -> String {
        format!(
            "目标 {}（排除 {}）；索引 {}；匹配 {}/模糊 {}/未收录 {}；轻量 {}；详情 {}（失败 {}）；合约 {}{}",
            self.targets,
            self.excluded,
            self.fetched,
            self.matched,
            self.ambiguous,
            self.not_found,
            self.light_updated,
            self.detail_pulled,
            self.detail_failed,
            self.contracts,
            if self.rate_limited { "；被限速" } else { "" }
        )
    }
}

/// 定时任务：每 `interval_ms` 同步一轮外部币信息。
///
/// 防重入与 `CatalogSync` 同口径（`docs/14 §3`）：`running` 单飞 + `due()` 节流 +
/// 轮次时刻在**轮首**前移（失败轮同样占掉整个周期，不被立刻重试刷爆上游）。
pub struct OffchainSync {
    /// 同步周期（ms，默认 1 小时）。
    pub interval_ms: u64,
    last_round_ms: u64,
    /// 一轮尚未收尾（**一轮未完成不重入**）。
    running: bool,
}

impl Default for OffchainSync {
    fn default() -> Self {
        Self::new()
    }
}

impl OffchainSync {
    /// 新建（`interval_ms` 可被 `[providers].offchain_interval` 覆盖）。
    pub fn new() -> Self {
        OffchainSync {
            interval_ms: OFFCHAIN_SYNC_INTERVAL_MS,
            last_round_ms: 0,
            running: false,
        }
    }

    /// 最近一轮的时刻（ms）。
    pub fn last_round_ms(&self) -> u64 {
        self.last_round_ms
    }

    /// 一轮是否尚未收尾（**一轮未完成不重入**的显式状态）。
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// 是否到了同步周期（`docs/14 §3`：每 1 小时一轮）。
    pub fn due(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_round_ms) >= self.interval_ms
    }

    /// 执行一轮（**一轮未完成不重入**；**每轮落一条 `offchain_sync_run`**）。
    ///
    /// 三道闸与 [`crate::catalog::CatalogSync::run`] 同口径：`running` / `due` /
    /// 轮首前移。所有错误都被折进 [`OffchainOutcome`]（**不向外抛**）——
    /// 外部源失败是"参考数据没更新"，不是"控制面坏了"。
    pub fn run(
        &mut self,
        store: &SqliteStore,
        provider: &dyn OffchainProvider,
        cfg: &OffchainConfig,
        now: u64,
    ) -> OffchainOutcome {
        if self.running || !self.due(now) {
            return OffchainOutcome::not_due();
        }
        self.running = true;
        self.last_round_ms = now;
        let mut out = self.run_inner(store, provider, cfg, now);
        self.running = false;
        self.persist(store, now, &mut out);
        out
    }

    /// **强制**触发（`POST /sync/offchain/runs`）：临时把周期置 0，跑完恢复。
    ///
    /// `last_round_ms` 已在轮首前移，因此之后的定时任务仍按原周期节流；
    /// `running` 单飞语义完全不动（与 `run_metadata_sync_forced` 同一口径）。
    pub fn run_forced(
        &mut self,
        store: &SqliteStore,
        provider: &dyn OffchainProvider,
        cfg: &OffchainConfig,
        now: u64,
    ) -> OffchainOutcome {
        let saved = self.interval_ms;
        self.interval_ms = 0;
        let out = self.run(store, provider, cfg, now);
        self.interval_ms = saved;
        out
    }

    fn run_inner(
        &mut self,
        store: &SqliteStore,
        provider: &dyn OffchainProvider,
        cfg: &OffchainConfig,
        now: u64,
    ) -> OffchainOutcome {
        let mut out = OffchainOutcome {
            ran: true,
            ..OffchainOutcome::default()
        };

        // ① 目标：catalog 中全部去重的币（`docs/14 §3`），stock 类已排除（`docs/14 §6`）
        let targets = match store.offchain_targets() {
            Ok(t) => t,
            Err(e) => {
                out.error = Some(format!("读取同步目标失败：{e}"));
                return out;
            }
        };
        out.targets = targets.len();
        out.excluded = store
            .list_assets_all()
            .map(|all| all.len().saturating_sub(targets.len()))
            .unwrap_or(0);
        if targets.is_empty() {
            return out; // 还没有任何币 → 零动作（仍落一条 run，页面看得见"跑了，0 条"）
        }

        // ② 市场索引（每轮 4~6 请求）——限速/失败 → **只记本轮，不碰交易所同步**
        let entries = match fetch_market_index(provider, cfg.coin_market_page_size) {
            Ok(e) => e,
            Err(ProviderError::RateLimited(m)) => {
                out.rate_limited = true;
                out.error = Some(m);
                return out;
            }
            Err(e) => {
                out.error = Some(e.to_string());
                return out;
            }
        };
        out.fetched = entries.len();
        let index = MarketIndex::build(&entries);

        // ③ 三级匹配 + 轻量字段（每轮）
        for t in &targets {
            let resolved: Option<String> = match &t.external_id {
                // 已有（手填别名或上轮自动命中）→ 保留，不再重新匹配
                Some(id) => {
                    out.matched += 1;
                    Some(id.clone())
                }
                None => match index.lookup(&t.code) {
                    MatchOutcome::Unique(id) => {
                        if let Err(e) = store.set_asset_match(t.id, Some(&id), "matched") {
                            out.error = Some(format!("写匹配结果失败（{}）：{e}", t.code));
                            continue;
                        }
                        out.matched += 1;
                        Some(id)
                    }
                    MatchOutcome::Ambiguous(_) => {
                        if let Err(e) = store.set_asset_match(t.id, None, "ambiguous") {
                            out.error = Some(format!("写匹配结果失败（{}）：{e}", t.code));
                        }
                        out.ambiguous += 1;
                        None
                    }
                    MatchOutcome::NotFound => {
                        if let Err(e) = store.set_asset_match(t.id, None, "not_found") {
                            out.error = Some(format!("写匹配结果失败（{}）：{e}", t.code));
                        }
                        out.not_found += 1;
                        None
                    }
                },
            };
            let Some(eid) = resolved else { continue };
            let Some(entry) = index.entry(&eid) else {
                continue; // 索引里没有该 id（下架/改名）→ 轻量字段不动
            };
            let patch = LightAssetPatch {
                name: entry.name.clone(),
                total_supply: entry.total_supply.clone(),
                circulating_supply: entry.circulating_supply.clone(),
                source: provider.source().to_string(),
                raw: entry.raw.clone(),
                at: now,
            };
            match store.set_asset_light(t.id, &patch) {
                Ok(()) => out.light_updated += 1,
                Err(e) => out.error = Some(format!("写轻量字段失败（{}）：{e}", t.code)),
            }
        }

        // ④ 详情层：**首次 + 轮转** `coin_detail_batch`（`docs/14 §3`）
        let candidates = if cfg.coin_detail_batch == 0 {
            Vec::new() // 配 0 = 本轮只跑轻量层（省配额时的退路）
        } else {
            match store.offchain_detail_candidates(cfg.coin_detail_batch) {
                Ok(c) => c,
                Err(e) => {
                    out.error = Some(format!("读取详情候选失败：{e}"));
                    return out;
                }
            }
        };
        for (asset_id, code, external_id) in candidates {
            match fetch_detail_with_retry(provider, &external_id, cfg.coin_detail_max_retries) {
                Ok(detail) => {
                    let patch = DetailAssetPatch {
                        twitter_url: detail.twitter_url(),
                        source: provider.source().to_string(),
                        raw: detail.raw.clone(),
                        at: now,
                    };
                    match store.set_asset_detail(asset_id, &patch) {
                        Ok(()) => out.detail_pulled += 1,
                        Err(e) => {
                            out.detail_failed += 1;
                            out.error = Some(format!("写详情字段失败（{code}）：{e}"));
                            continue;
                        }
                    }
                    // 一币多链 → 子表（`docs/12 §3.2.1`，**不是 JSON 列**）
                    if !detail.platforms.is_empty() {
                        let rows: Vec<NewContract> = detail
                            .platforms
                            .iter()
                            .map(|(chain, address)| NewContract {
                                asset_id,
                                chain: chain.clone(),
                                address: address.clone(),
                                standard: None, // CG 的 platforms 不给标准，留 NULL
                                is_primary: false,
                                source: provider.source().to_string(),
                                updated_at: now,
                            })
                            .collect();
                        match store.sync_asset_contracts(asset_id, &rows) {
                            Ok(n) => out.contracts += n,
                            Err(e) => {
                                out.error =
                                    Some(format!("写合约地址失败（{code}）：{e}"));
                            }
                        }
                    }
                }
                Err(ProviderError::RateLimited(m)) => {
                    out.rate_limited = true;
                    out.error = Some(m);
                    // 限速：**中断本轮详情轮转**（继续打只会更糟），轻量字段已写完
                    break;
                }
                Err(e) => {
                    out.detail_failed += 1;
                    out.error = Some(format!("详情拉取失败（{code}）：{e}"));
                    // 推进 `detail_synced_at` 让该币退到轮转队尾——
                    // 否则一个永久失败的币会卡死整条轮转队列（头阻塞）。
                    let _ = store.touch_asset_detail(asset_id, now);
                }
            }
        }
        out
    }

    /// 落一条 `offchain_sync_run`（**每轮一条**，与 `sync_run` 分离）。
    fn persist(&self, store: &SqliteStore, started_at: u64, out: &mut OffchainOutcome) {
        let finished_at = crate::now_ms().max(started_at);
        let value = out.outcome_value(finished_at);
        match store.record_offchain_sync_run(
            started_at,
            finished_at,
            &value,
            out.error.as_deref(),
        ) {
            Ok(id) => out.offchain_run_id = Some(id),
            Err(e) => {
                let msg = format!("外部同步轮次落库失败（12 §3.4b）：{e}");
                eprintln!("[control] {msg}");
                out.persist_error = Some(msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_literals_follow_jwt_ttl_semantics() {
        assert_eq!(parse_duration_ms("1h").unwrap(), 3_600_000);
        assert_eq!(parse_duration_ms("30m").unwrap(), 1_800_000);
        assert_eq!(parse_duration_ms("45s").unwrap(), 45_000);
        assert_eq!(parse_duration_ms("3600").unwrap(), 3_600_000, "裸数字 = 秒");
        assert!(parse_duration_ms("abc").is_err());
    }

    #[test]
    fn matching_is_exact_only_no_fuzzy_no_prefix_strip() {
        let idx = MarketIndex::build(&[
            entry("bitcoin", "BTC"),
            entry("ethereum", "ETH"),
            entry("inch", "1INCH"),
            entry("pepe", "PEPE"),
            entry("shiba-inu", "SHIB"),
        ]);
        assert_eq!(idx.lookup("btc"), MatchOutcome::Unique("bitcoin".into()));
        assert_eq!(idx.lookup("Btc"), MatchOutcome::Unique("bitcoin".into()));
        // 剥前缀的诱惑：`1000PEPE` / `1000SHIB` 必须是零命中，不许归一化到 PEPE
        assert_eq!(idx.lookup("1000PEPE"), MatchOutcome::NotFound);
        assert_eq!(idx.lookup("1000SHIB"), MatchOutcome::NotFound);
        // 同理：`BTC` 剥成 `B` 之类的想法一律没有
        assert_eq!(idx.lookup("B"), MatchOutcome::NotFound);
        assert_eq!(idx.lookup("NOPE"), MatchOutcome::NotFound);
    }

    #[test]
    fn same_symbol_from_two_projects_is_ambiguous_not_first_wins() {
        let idx = MarketIndex::build(&[entry("foo", "FOO"), entry("bar", "FOO")]);
        match idx.lookup("foo") {
            MatchOutcome::Ambiguous(ids) => {
                assert_eq!(ids, vec!["bar".to_string(), "foo".to_string()]);
            }
            other => panic!("多命中必须 ambiguous，得到 {other:?}"),
        }
    }

    #[test]
    fn market_index_paging_stops_on_short_and_stalled_pages() {
        struct Pages {
            data: Vec<MarketEntry>,
        }
        impl OffchainProvider for Pages {
            fn source(&self) -> &'static str {
                "test"
            }
            fn fetch_markets_page(&self, page: u32, per_page: u32) -> Result<Vec<MarketEntry>, ProviderError> {
                let s = ((page - 1) * per_page) as usize;
                Ok(self.data.iter().skip(s).take(per_page as usize).cloned().collect())
            }
            fn fetch_coin_detail(&self, _: &str) -> Result<CoinDetail, ProviderError> {
                Err(ProviderError::Unsupported("x".into()))
            }
        }
        // 5 条 / 每页 2 → 3 页（最后一页短页）
        let data: Vec<MarketEntry> = (0..5).map(|i| entry(&format!("c{i}"), "S")).collect();
        let all = fetch_market_index(&Pages { data: data.clone() }, 2).unwrap();
        assert_eq!(all.len(), 5);

        // 假实现不认 page 参数（每页都回同一批）→ 靠"新增 id 不增长"止步，不死循环
        struct Stuck {
            data: Vec<MarketEntry>,
        }
        impl OffchainProvider for Stuck {
            fn source(&self) -> &'static str {
                "test"
            }
            fn fetch_markets_page(&self, _: u32, _: u32) -> Result<Vec<MarketEntry>, ProviderError> {
                Ok(self.data.clone())
            }
            fn fetch_coin_detail(&self, _: &str) -> Result<CoinDetail, ProviderError> {
                Err(ProviderError::Unsupported("x".into()))
            }
        }
        let all = fetch_market_index(&Stuck { data }, 2).unwrap();
        assert_eq!(all.len(), 5, "去重后仍是 5 条，且没有翻满 40 页");
    }

    fn entry(id: &str, symbol: &str) -> MarketEntry {
        MarketEntry {
            id: id.to_string(),
            symbol: symbol.to_string(),
            name: Some(id.to_string()),
            market_cap_rank: None,
            total_supply: None,
            circulating_supply: None,
            raw: json!({"id": id, "symbol": symbol}),
        }
    }
}
