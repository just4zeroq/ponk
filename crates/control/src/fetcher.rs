//! 交易所取数 —— **统一经 dig3 连接器层**（`docs/09 §2.4`）。
//!
//! ## 为什么在这里
//!
//! `docs/09 §2.4` 的口径是「交易对信息**经 dig3 连接器层**
//! `get_exchange_info(AccountType) -> Vec<SymbolInfo>`；币信息 dig3 原本没有，
//! 各所端点按所接线」。本模块即该口径的落地：transport（端点发现、限速、
//! 重连、超时、镜像路由）全部由 [`ExchangeHub`] 承担，控制面只做**平台语义归一化**。
//!
//! dig3 侧为币信息新增了 `CurrencyInfo` 与 `MarketData::get_currencies`
//! （带默认 `NotImplemented`），故本模块能对交易对与币信息走同一条路。
//!
//! ## 三条硬约束
//!
//! **C1 — 归一化必须与迁移前逐字节一致**
//! `InstrumentRecord::hash_view`（`catalog_repo.rs:322`）由 13 个字段定 `row_hash`，
//! 漏归一化一个字段 = 3724 行全变 = 一次全量指派重算。[`normalize`] 是唯一入口，
//! 每个改写字段都标注了「dig3 现状 / 本平台要求 / 依据」。
//!
//! **C2 — Runtime 只能局部建、当场 drop**
//! tokio 禁止在 async 上下文 drop Runtime（`blocking/shutdown.rs:51`）。本 trait 的
//! 两个调用点——`main.rs` 的 `control_loop`（`std::thread::spawn` 出来的独立 OS 线程，
//! 全仓没有 `#[tokio::main]`）与 `http.rs` 的 `spawn_blocking`——**都不在**该上下文，
//! 所以 [`with_runtime`] 在函数体内建完立刻丢是安全的。**绝不能把这个 Runtime
//! 放进结构体**：`tests/http.rs` 的 fixture 在 `#[tokio::test]` 的 `block_on` 内拆除，
//! 那里一 drop 就 panic，而且编译期看不出来。
//!
//! **C3 — 每个 `(venue, market)` 独立 [`ExchangeHub`]**
//! dig3 的 `rest_override` 是 prefix 模式（`assemble_rest_url` 丢弃 `real_base`）
//! 且按 `ExchangeId` 一锁一值 —— 一个 override 会同时改写 spot 与 futures 的主机，
//! 而 Binance 这两台本来就不同（`api.` vs `fapi.`）。每个 `(venue, market)` 各建一个
//! hub 实例，粒度问题在本仓消失，**dig3 的 plumbing 一行不用动**。

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use digdigdig3::connector_manager::ExchangeHub;
// 注：`get_exchange_info` / `get_currencies` 定义在 `MarketData` 上，但
// `hub.rest()` 返回的是 `Arc<dyn CoreConnector>`（`: MarketData` 的 supertrait），
// 调用其上的 supertrait 方法**不需要**把 `MarketData` 引入作用域 —— 引入反而 unused。
use digdigdig3::core::types::{
    AccountType, CurrencyInfo as DigCurrencyInfo, ExchangeError as DigError, ExchangeId,
    SymbolInfo,
};
use serde_json::Value;

use crate::catalog::{MetaError, MetadataFetcher};
use ponk_protocol::CurrencyInfo;

/// 缺省单次请求超时（ms）。
///
/// dig3 的 `HttpClient::new(30_000)` 硬编码 30s，本仓配置的 8s 必须靠
/// [`with_runtime`] 内的 `tokio::time::timeout` 自己兜住，否则一轮会打满 150s。
pub const DEFAULT_EXCHANGE_TIMEOUT_MS: u64 = 10_000;
/// 缺省连接失败冷却时长（ms）：期间跳过该 base，避免每轮都被同一台不可达主机吃满。
pub const DEFAULT_FAILURE_COOLDOWN_MS: u64 = 60_000;

/// 内置 base 候选表：`"<venue>.<market>"` → **origin 列表**（按序 failover）。
///
/// 键还有一个约定后缀 `"<venue>.currencies"`（币信息端点）。
/// **值只到 origin，不含 path/query** —— 路径由 dig3 的端点定义自己拼。
///
/// 未配置的键 = 该目标**没有可用端点** → [`MetaError::Unsupported`]，
/// 与旧实现「不猜端点」的只读保守语义一致。
pub fn default_rest_bases() -> BTreeMap<String, Vec<String>> {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Binance 现货：官方公开数据镜像排在前面 —— 实测 `api.binance.com` 在部分
    // 网络下超时，而 `data-api.binance.vision` 是同一份 exchangeInfo 且可达。
    m.insert(
        "binance.spot".into(),
        vec![
            "https://data-api.binance.vision".to_string(),
            "https://api.binance.com".to_string(),
        ],
    );
    // Binance 永续：fapi 是独立主机（.vision 不镜像 fapi），只有一个候选。
    // 注意本仓对 spot 与 swap 各建独立 hub，故两者不会互相污染（C3）。
    m.insert("binance.swap".into(), vec!["https://fapi.binance.com".into()]);
    // OKX 单主机，spot/swap 各自建 hub（C3 的统一处理，不为省事共享）。
    m.insert("okx.spot".into(), vec!["https://www.okx.com".into()]);
    m.insert("okx.swap".into(), vec!["https://www.okx.com".into()]);
    // 币信息：**只有 OKX 有公开端点**（`/api/v5/asset/currencies`）。
    // Binance 不给键 → 走 `Unsupported`（其 coin config 需签名）。
    m.insert("okx.currencies".into(), vec!["https://www.okx.com".into()]);
    m
}

/// 拉取器配置（`control.toml [providers]`，`docs/14 §8`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ExchangeFetcherConfig {
    /// 单次请求超时（ms）——覆盖 dig3 内置的 30s。
    pub timeout_ms: u64,
    /// 连接失败冷却（ms）：期间跳过该 base。
    pub cooldown_ms: u64,
    /// base 候选表；**按键覆盖**内置默认（未覆盖的键沿用默认）。
    pub rest_bases: BTreeMap<String, Vec<String>>,
}

impl Default for ExchangeFetcherConfig {
    fn default() -> Self {
        ExchangeFetcherConfig {
            timeout_ms: DEFAULT_EXCHANGE_TIMEOUT_MS,
            cooldown_ms: DEFAULT_FAILURE_COOLDOWN_MS,
            rest_bases: default_rest_bases(),
        }
    }
}

impl ExchangeFetcherConfig {
    /// 合并用户配置：出现的键**整体替换**内置候选，未出现的键保留默认。
    ///
    /// base 一律去掉尾斜杠 —— dig3 的 override 走 `format!("{base}{path}")`
    /// 而 `path` 以 `/` 开头，带尾斜杠会拼出 `//`。
    pub fn merged(rest_bases: BTreeMap<String, Vec<String>>) -> Self {
        let mut c = Self::default();
        for (k, v) in rest_bases {
            let cleaned: Vec<String> = v
                .iter()
                .map(|s| s.trim().trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !cleaned.is_empty() {
                c.rest_bases.insert(k, cleaned);
            }
        }
        c
    }

    /// 已配置的键数（启动日志用）。
    pub fn endpoint_count(&self) -> usize {
        self.rest_bases.len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 拉取器
// ─────────────────────────────────────────────────────────────────────────────

/// 交易所取数：dig3 连接器 + 平台语义归一化。
///
/// 不持有任何连接器或 Runtime —— 每次取数现建现用（见模块文档 C2）。
pub struct Dig3Fetcher {
    rest_bases: BTreeMap<String, Vec<String>>,
    timeout_ms: u64,
    cooldown_ms: u64,
    /// base → 上次连接失败时刻（冷却不重试）。
    failed: Mutex<BTreeMap<String, Instant>>,
}

impl std::fmt::Debug for Dig3Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dig3Fetcher")
            .field("keys", &self.rest_bases.len())
            .field("timeout_ms", &self.timeout_ms)
            .field("cooldown_ms", &self.cooldown_ms)
            .finish()
    }
}

impl Dig3Fetcher {
    /// 构造（**不联网**，也不建 Runtime）。
    pub fn new(cfg: ExchangeFetcherConfig) -> Self {
        Dig3Fetcher {
            rest_bases: cfg.rest_bases,
            timeout_ms: cfg.timeout_ms.max(1_000),
            cooldown_ms: cfg.cooldown_ms.max(1_000),
            failed: Mutex::new(BTreeMap::new()),
        }
    }

    fn endpoints(&self, key: &str) -> &[String] {
        self.rest_bases.get(key).map(|v| v.as_slice()).unwrap_or(&[])
    }

    fn cooling(&self, base: &str) -> bool {
        let map = match self.failed.lock() {
            Ok(m) => m,
            Err(_) => return false, // 锁中毒：不冷却，宁可多试一次
        };
        map.get(base)
            .map(|t| t.elapsed() < Duration::from_millis(self.cooldown_ms))
            .unwrap_or(false)
    }

    fn mark_failed(&self, base: &str) {
        if let Ok(mut m) = self.failed.lock() {
            m.insert(base.to_string(), Instant::now());
        }
    }

    fn mark_ok(&self, base: &str) {
        if let Ok(mut m) = self.failed.lock() {
            m.remove(base);
        }
    }

    /// 候选 base 按序尝试，**首个成功的胜出**。
    ///
    /// `fetch` 收 base 与一个 async future 工厂；每个候选各建一个 hub（C3）。
    async fn try_bases<T, Fut>(
        &self,
        key: &str,
        what: &str,
        id: ExchangeId,
        mk: impl Fn(ExchangeHub, ExchangeId) -> Fut,
    ) -> Result<T, MetaError>
    where
        Fut: std::future::Future<Output = Result<T, MetaError>>,
    {
        let bases = self.endpoints(key);
        if bases.is_empty() {
            return Err(MetaError::Unsupported(format!(
                "{what}: 未配置端点（[providers].dig3_rest_base[\"{key}\"] 为空）"
            )));
        }

        let mut errors: Vec<String> = Vec::new();
        for base in bases {
            if self.cooling(base) {
                eprintln!(
                    "[sync]   {what} ← {base} 连接失败冷却中（{}ms 内不再重试），跳过",
                    self.cooldown_ms
                );
                errors.push(format!("{base}：连接失败冷却中"));
                continue;
            }

            let hub = ExchangeHub::new();
            hub.set_rest_base_override(id, base.clone());
            if let Err(e) = hub.connect_public(id, false).await {
                self.mark_failed(base);
                eprintln!("[sync]   {what} ← {base} 建连失败：{e}");
                errors.push(format!("{base}：建连失败 {e}"));
                continue;
            }

            let t0 = Instant::now();
            match mk(hub, id).await {
                Ok(v) => {
                    self.mark_ok(base);
                    eprintln!(
                        "[sync]   {what} ← {base} → ok，{} ms",
                        t0.elapsed().as_millis()
                    );
                    return Ok(v);
                }
                Err(e) => {
                    // 冷却只针对**传输层失败**（连不上/超时）；
                    // 4xx/业务错误重连也一样错，冷却同样有意义。
                    self.mark_failed(base);
                    eprintln!(
                        "[sync]   {what} ← {base} 失败（{} ms）：{e}",
                        t0.elapsed().as_millis()
                    );
                    errors.push(format!("{base}：{e}"));
                }
            }
        }

        Err(MetaError::Fetch(format!(
            "{what}: {} 个候选 base 全部不可用 —— {}",
            bases.len(),
            errors.join(" ；")
        )))
    }

    /// 驱动 dig3 的 future：局部 Runtime + 可配超时（见模块文档 C2）。
    ///
    /// 调用点必须**不在 async 上下文** —— 当前只有 `control_loop` 线程与
    /// `spawn_blocking` 线程会进来，两者都满足。
    fn with_runtime<T>(
        &self,
        what: &str,
        f: impl std::future::Future<Output = Result<T, MetaError>>,
    ) -> Result<T, MetaError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| MetaError::Fetch(format!("{what}: 构建 tokio 运行时失败：{e}")))?;

        let label = what.to_string();
        let timeout = Duration::from_millis(self.timeout_ms);
        let out = rt.block_on(async move {
            match tokio::time::timeout(timeout, f).await {
                Ok(inner) => inner,
                Err(_) => Err(MetaError::Fetch(format!(
                    "{label}: 超过 {}ms（dig3 内置 30s 更宽，这里由本仓配置收紧）",
                    timeout.as_millis()
                ))),
            }
        });
        // 显式 drop：当前线程是 control_loop / spawn_blocking，**不是** async 上下文。
        drop(rt);
        out
    }

    fn venues_key(&self, venue: &str, market: &str) -> String {
        format!("{venue}.{market}")
    }
}

impl MetadataFetcher for Dig3Fetcher {
    fn fetch_symbols(&self, venue: &str, market: &str) -> Result<Vec<SymbolInfo>, MetaError> {
        let key = self.venues_key(venue, market);
        let what = format!("{venue}/{market}");
        let id = venue_to_exchange_id(venue).ok_or_else(|| {
            MetaError::Unsupported(format!(
                "{what}: dig3 无此交易所（`ExchangeId` 未登记，见 dig3 common.rs）"
            ))
        })?;
        let acct = account_type_for(market).ok_or_else(|| {
            MetaError::Unsupported(format!("{what}: 未知 market 类型（02 §6 只钉死 spot/swap）"))
        })?;

        eprintln!("[sync]   {what} → dig3 {}（account_type={acct:?}）", id.as_str());

        let timeout = Duration::from_millis(self.timeout_ms);
        let raw: Vec<SymbolInfo> = self.with_runtime(&what, async {
            // 闭包是 `Fn`（循环里多次调用）：`what` 只能借，进 future 前先克隆一份
            self.try_bases(&key, &what, id, |hub, id| {
                let what = what.clone();
                async move {
                    let conn = hub.rest(id).ok_or_else(|| {
                        MetaError::Fetch(format!("{what}: hub 未持有 {id:?} 的连接器"))
                    })?;
                    match tokio::time::timeout(timeout, conn.get_exchange_info(acct)).await {
                        Ok(Ok(v)) => Ok(v),
                        Ok(Err(e)) => Err(dig3_error(&e)),
                        Err(_) => Err(MetaError::Fetch(format!(
                            "{what}: dig3 get_exchange_info 超过 {}ms",
                            timeout.as_millis()
                        ))),
                    }
                }
            })
            .await
        })?;

        // dig3 保持所方原值；本平台要的是 `hash_view` 那 13 个字段的稳定口径（C1）
        let mut out = raw;
        for s in &mut out {
            normalize(venue, market, s);
        }
        Ok(out)
    }

    fn fetch_currencies(&self, venue: &str) -> Result<Vec<CurrencyInfo>, MetaError> {
        let key = format!("{venue}.currencies");
        let what = format!("{venue} 币信息");
        let id = venue_to_exchange_id(venue).ok_or_else(|| {
            MetaError::Unsupported(format!("{what}: dig3 无此交易所"))
        })?;

        eprintln!("[sync]   {what} → dig3 {}", id.as_str());

        let timeout = Duration::from_millis(self.timeout_ms);
        let raw: Vec<DigCurrencyInfo> = self.with_runtime(&what, async {
            self.try_bases(&key, &what, id, |hub, id| {
                let what = what.clone();
                async move {
                    let conn = hub.rest(id).ok_or_else(|| {
                        MetaError::Fetch(format!("{what}: hub 未持有 {id:?} 的连接器"))
                    })?;
                    match tokio::time::timeout(timeout, conn.get_currencies()).await {
                        Ok(Ok(v)) => Ok(v),
                        Ok(Err(e)) => Err(dig3_error(&e)),
                        Err(_) => Err(MetaError::Fetch(format!(
                            "{what}: dig3 get_currencies 超过 {}ms",
                            timeout.as_millis()
                        ))),
                    }
                }
            })
            .await
        })?;

        Ok(raw.into_iter().map(to_protocol_currency).collect())
    }
}

/// dig3 错误 → 平台错误。
///
/// 关键分野：`WireAbsent`（**交易所根本不提供**，永不到期）归 [`MetaError::Unsupported`]
/// —— 与「未配置端点不猜端点」同一语义；`NotImplemented`（我们还没写）与传输/解析
/// 失败归 [`MetaError::Fetch`]，要进 `skipped` 让页面看得见。
fn dig3_error(e: &DigError) -> MetaError {
    match e {
        DigError::WireAbsent(m) | DigError::NotImplemented(m) => MetaError::Unsupported(m.clone()),
        DigError::RateLimit | DigError::RateLimitExceeded { .. } => {
            MetaError::Fetch(format!("所方限流：{e}"))
        }
        DigError::Auth(_) | DigError::InvalidCredentials(_) | DigError::PermissionDenied(_) => {
            MetaError::Fetch(format!("鉴权失败（公开端点不该走到这里）：{e}"))
        }
        other => MetaError::Fetch(other.to_string()),
    }
}

/// dig3 `CurrencyInfo` → 平台 `CurrencyInfo`（`md.meta` 负载）。
///
/// 平台协议不随 dig3 类型演进：充提开关没有协议字段，折进 `meta`（落库时
/// `currency_to_venue_asset` 会把它并进 `extra`，数据不丢）。
fn to_protocol_currency(c: DigCurrencyInfo) -> CurrencyInfo {
    let mut meta = BTreeMap::new();
    if let Some(v) = c.deposit_enabled {
        meta.insert("deposit_enabled".to_string(), v.to_string());
    }
    if let Some(v) = c.withdraw_enabled {
        meta.insert("withdraw_enabled".to_string(), v.to_string());
    }
    CurrencyInfo {
        currency: c.currency,
        name: c.name,
        networks: c.networks,
        precision: c.precision,
        meta,
        extra: c.extra,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 映射
// ─────────────────────────────────────────────────────────────────────────────

/// `venue` → dig3 `ExchangeId`。
///
/// 标准名直接走 dig3 自带的 `ExchangeId::from_str`（小写归一，覆盖 binance/okx/
/// bybit/... 几十个），这里只补**本仓用到的别名**。别名表与
/// `collect/src/config.rs::venue_to_exchange_id` 保持同口径 —— 但控制面**不能**
/// 依赖 `ponk-collect`（那会把整个采集器拖进控制面）。
fn venue_to_exchange_id(venue: &str) -> Option<ExchangeId> {
    // 先绑定：`.to_ascii_lowercase()` 是临时值，直接在其上 `.as_str()` 会 E0716
    let lowered = venue.trim().to_ascii_lowercase();
    let key = match lowered.as_str() {
        "gate" | "gateio" => "gateio",
        "htx" | "huobi" => "htx",
        "hl" | "hyperliquid" => "hyperliquid",
        "crypto_com" | "cryptocom" => "crypto_com",
        other => other,
    };
    ExchangeId::from_str(key)
}

/// `market` → dig3 `AccountType`（复用 `02 §6` 已定映射）。
fn account_type_for(market: &str) -> Option<AccountType> {
    match crate::catalog_repo::dig3_account_type_for(market)? {
        "Spot" => Some(AccountType::Spot),
        "FuturesCross" => Some(AccountType::FuturesCross),
        "FuturesIsolated" => Some(AccountType::FuturesIsolated),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 归一化（C1：这一段直接决定 row_hash 稳不稳定）
// ─────────────────────────────────────────────────────────────────────────────

/// dig3 的类型化字段 → 本平台 `hash_view` 口径。
///
/// **只改写以下 7 项**；其余字段（`tick_size` / `step_size` / `min_notional` /
/// 现货的 `base`/`quote` / `extra`）经逐字段核对与原实现一致，直接沿用 dig3。
/// 改动任何一条都要同步更新 `docs/09 §2.4` 与回归测试。
fn normalize(venue: &str, market: &str, s: &mut SymbolInfo) {
    let raw = &s.extra;

    // ① status —— dig3 刻意保留所方原值（`binance/parser.rs`:「keep the native
    //    status verbatim, never filter」），本平台要三态（`docs/12 §3.1`）。
    s.status = match venue {
        "binance" => binance_status(&s.status),
        "okx" => okx_status(&s.status),
        _ => s.status.clone(),
    };

    // ② native_code / ③ base / quote —— 只在**所方不给这两个字段**的组合上重算：
    //    · binance 永续：记录没有 `baseAsset`，`symbol` 还是 `BTCUSDT_PERP`
    //    · okx 永续：记录没有 `baseCcy`/`quoteCcy`，计价对在 `uly`（`BTC-USDT`）
    //    现货两所都直接给了，不动。
    match (venue, market) {
        ("binance", "swap") => {
            let pair = raw_str(raw, "pair").or_else(|| raw_str(raw, "symbol"));
            if let Some(p) = pair {
                s.symbol = p.to_string();
            }
            if let Some(quote) = raw_str(raw, "quoteAsset") {
                let base = pair
                    .and_then(|p| p.strip_suffix(quote))
                    .filter(|b| !b.is_empty())
                    .or_else(|| raw_str(raw, "underlying"));
                if let Some(b) = base {
                    s.base_asset = b.to_string();
                }
            }
        }
        ("okx", "swap") => {
            if let Some((b, q)) = raw_str(raw, "uly").and_then(|u| u.split_once('-')) {
                s.base_asset = b.to_string();
                s.quote_asset = q.to_string();
            }
        }
        _ => {}
    }

    // ④ ⑤ 精度 —— Binance **现货**记录没有 `pricePrecision`/`quantityPrecision`
    //    （那是 fapi 的字段），dig3 兜底成 8；本平台从 `filters` 算小数位。
    //    永续两边都优先取所方给的显式字段，但**兜底不同**（dig3 兜 8，本平台兜 2/0）。
    //    OKX 不用动：dig3 的 `count_decimals` 与本平台 `decimals` 同算法。
    if venue == "binance" {
        let filters = raw.get("filters").and_then(Value::as_array);
        let tick = filter(filters, "PRICE_FILTER", "tickSize");
        let step = filter(filters, "LOT_SIZE", "stepSize");
        let (p, q) = if market == "spot" {
            (
                decimals(tick)
                    .or_else(|| u8_of(raw, "quotePrecision"))
                    .unwrap_or(8),
                decimals(step)
                    .or_else(|| u8_of(raw, "baseAssetPrecision"))
                    .unwrap_or(8),
            )
        } else {
            (
                u8_of(raw, "pricePrecision").or_else(|| decimals(tick)).unwrap_or(2),
                u8_of(raw, "quantityPrecision")
                    .or_else(|| decimals(step))
                    .unwrap_or(0),
            )
        };
        s.price_precision = p;
        s.quantity_precision = q;
    }

    // ⑥ instrument_type —— 参与 `hash_view`。现货两所本平台都写 `spot` 字面量
    //    （dig3 在现货上给 `contractType` 缺失 → `None`，okx 则给大写 `instType`）；
    //    binance 永续取所方 `contractType`（`PERPETUAL`），okx 永续写 `swap`。
    s.instrument_type = match (venue, market) {
        ("binance", "spot") => Some("spot".to_string()),
        ("binance", _) => raw_str(raw, "contractType").map(str::to_string),
        (_, "spot") => Some("spot".to_string()),
        (_, _) => Some("swap".to_string()),
    };
}

// ─────────────────────────────────────────────────────────────────────────────
// 所方原值的小工具（复用旧实现的口径，避免两处算出不同的小数位）
// ─────────────────────────────────────────────────────────────────────────────

/// `f64` 的十进制小数位（`"0.01000000"` → `2`），尾零不计。
fn decimals(raw: Option<&str>) -> Option<u8> {
    let t = raw?.trim();
    let (_, frac) = t.split_once('.')?;
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        Some(0)
    } else {
        Some(frac.len() as u8)
    }
}

/// 取字符串字段。
fn raw_str<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// 取数字字段转 `u8`（字符串或数字都接受）。
fn u8_of(v: &Value, k: &str) -> Option<u8> {
    v.get(k)
        .and_then(|x| match x {
            Value::Number(n) => n.as_u64(),
            Value::String(t) => t.trim().parse::<u64>().ok(),
            _ => None,
        })
        .map(|n| n.min(u8::MAX as u64) as u8)
}

/// 从 Binance `filters[]` 里按 `filterType` 取字段。
fn filter<'a>(filters: Option<&'a Vec<Value>>, ty: &str, field: &str) -> Option<&'a str> {
    filters?
        .iter()
        .find(|x| raw_str(x, "filterType") == Some(ty))
        .and_then(|x| raw_str(x, field))
}

/// Binance `status` → 平台三态（`docs/12 §3.1`）。
fn binance_status(raw: &str) -> String {
    match raw {
        "TRADING" => "trading",
        "BREAK" | "HALT" | "PRE_TRADING" | "POST_TRADING" | "END_OF_DAY" => "break",
        _ => "delisted", // DELISTED 及其它未核对状态：宁可报下架也不谎报可交易
    }
    .to_string()
}

/// OKX `state` → 平台三态。
fn okx_status(raw: &str) -> String {
    match raw {
        "live" => "trading",
        "suspend" | "preopen" | "freeze" | "test" | "pre-suspend" => "break",
        _ => "delisted", // delisted / clearing / terminate
    }
    .to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_venue_is_unsupported_not_fetch() {
        let f = Dig3Fetcher::new(ExchangeFetcherConfig::default());
        // 未登记的交易所：在打网络之前就该是 Unsupported
        let e = f.fetch_symbols("nosuchexchange", "spot").expect_err("必须报错");
        assert!(
            matches!(e, MetaError::Unsupported(_)),
            "未核对的所 = Unsupported，实为 {e:?}"
        );
        // Binance 没配币信息端点（其 coin config 需签名）→ 同样是 Unsupported
        let e = f.fetch_currencies("binance").expect_err("必须报错");
        assert!(
            matches!(e, MetaError::Unsupported(_)),
            "无端点 = Unsupported，实为 {e:?}"
        );
        assert!(e.to_string().contains("binance"), "{e}");
    }

    #[test]
    fn unknown_market_is_unsupported() {
        let f = Dig3Fetcher::new(ExchangeFetcherConfig::default());
        let e = f.fetch_symbols("binance", "options").expect_err("未知 market");
        assert!(matches!(e, MetaError::Unsupported(_)), "{e:?}");
    }

    #[test]
    fn merged_rest_bases_replace_per_key_only_and_strip_trailing_slash() {
        let mut custom = BTreeMap::new();
        custom.insert(
            "binance.spot".to_string(),
            vec!["https://mirror.example/".to_string(), "  ".to_string()],
        );
        let c = ExchangeFetcherConfig::merged(custom);
        assert_eq!(
            c.rest_bases["binance.spot"],
            vec!["https://mirror.example".to_string()],
            "尾斜杠被去掉（dig3 override 走 format!{{base}}{{path}}），空白项被丢弃"
        );
        assert!(
            c.rest_bases.contains_key("okx.swap"),
            "未覆盖的键沿用内置默认"
        );
    }

    #[test]
    fn default_table_covers_the_four_live_targets_and_currencies_gate() {
        let t = default_rest_bases();
        for k in ["binance.spot", "binance.swap", "okx.spot", "okx.swap"] {
            assert!(t.contains_key(k), "缺 {k}");
            assert!(
                t[k].iter().all(|u| u.starts_with("https://") && !u.ends_with('/')),
                "{k} 必须是不带尾斜杠的 https origin"
            );
        }
        // 语义 gate：Binance 无公开币信息端点 → 不给键
        assert!(!t.contains_key("binance.currencies"));
        assert!(t.contains_key("okx.currencies"));
    }

    #[test]
    fn venue_mapping_supports_aliases_and_rejects_unknown() {
        assert_eq!(venue_to_exchange_id("binance"), Some(ExchangeId::Binance));
        assert_eq!(venue_to_exchange_id("OKX"), Some(ExchangeId::OKX));
        assert_eq!(venue_to_exchange_id("gate"), Some(ExchangeId::GateIO));
        assert_eq!(venue_to_exchange_id("huobi"), Some(ExchangeId::HTX));
        assert_eq!(venue_to_exchange_id("hl"), Some(ExchangeId::HyperLiquid));
        assert_eq!(venue_to_exchange_id("nosuch"), None);
    }

    #[test]
    fn market_maps_to_account_type_per_02_6() {
        assert_eq!(account_type_for("spot"), Some(AccountType::Spot));
        // 02 §6 硬映射：swap 一律 FuturesCross，FuturesIsolated 永不用
        assert_eq!(
            account_type_for("swap"),
            Some(AccountType::FuturesCross)
        );
        assert_eq!(account_type_for("future"), Some(AccountType::FuturesCross));
        assert_eq!(account_type_for("options"), None);
    }

    /// `status` 归一化：dig3 保持所方原值，本平台要三态。
    #[test]
    fn normalize_maps_native_status_to_three_states() {
        let mut s = mk_sym("TRADING");
        normalize("binance", "spot", &mut s);
        assert_eq!(s.status, "trading");

        let mut s = mk_sym("BREAK");
        normalize("binance", "spot", &mut s);
        assert_eq!(s.status, "break");

        let mut s = mk_sym("DELISTED");
        normalize("binance", "spot", &mut s);
        assert_eq!(s.status, "delisted");

        let mut s = mk_sym("live");
        normalize("okx", "spot", &mut s);
        assert_eq!(s.status, "trading");

        let mut s = mk_sym("suspend");
        normalize("okx", "swap", &mut s);
        assert_eq!(s.status, "break");

        let mut s = mk_sym("terminate");
        normalize("okx", "spot", &mut s);
        assert_eq!(s.status, "delisted");
    }

    /// Binance 永续：native_code 取 `pair`、base 剥 quote 后缀（fapi 无 `baseAsset`）。
    #[test]
    fn normalize_binance_swap_takes_pair_and_strips_base() {
        let mut s = mk_sym("TRADING");
        s.symbol = "BTCUSDT_PERP".to_string(); // dig3 从 `symbol` 取
        s.base_asset = String::new(); // fapi 记录没有 baseAsset → 空串
        s.quote_asset = "USDT".to_string();
        s.extra = serde_json::json!({
            "symbol": "BTCUSDT_PERP", "pair": "BTCUSDT",
            "status": "TRADING", "underlying": "BTC", "quoteAsset": "USDT",
            "contractType": "PERPETUAL", "pricePrecision": 1, "quantityPrecision": 2
        });
        normalize("binance", "swap", &mut s);
        assert_eq!(s.symbol, "BTCUSDT", "native_code 用 pair，采集侧靠它订阅");
        assert_eq!(s.base_asset, "BTC", "从 pair 剥掉 USDT 后缀");
        assert_eq!(s.quote_asset, "USDT");
        assert_eq!(s.instrument_type.as_deref(), Some("PERPETUAL"));
        assert_eq!(s.price_precision, 1, "所方给了 pricePrecision 就用它");
        assert_eq!(s.quantity_precision, 2);
    }

    /// OKX 永续：base/quote 从 `uly` 切（记录里没有 baseCcy/quoteCcy）。
    #[test]
    fn normalize_okx_swap_splits_uly() {
        let mut s = mk_sym("live");
        s.base_asset = String::new();
        s.quote_asset = String::new();
        s.extra = serde_json::json!({"instId": "BTC-USDT-SWAP", "uly": "BTC-USDT", "state": "live"});
        normalize("okx", "swap", &mut s);
        assert_eq!(s.base_asset, "BTC");
        assert_eq!(s.quote_asset, "USDT");
        assert_eq!(s.instrument_type.as_deref(), Some("swap"));
    }

    /// Binance 现货：记录没有 `pricePrecision`（fapi 字段），dig3 会兜成 8；
    /// 本平台必须从 `tickSize`/`stepSize` 算小数位。
    #[test]
    fn normalize_binance_spot_precision_comes_from_filters() {
        let mut s = mk_sym("TRADING");
        s.price_precision = 8; // dig3 的兜底值
        s.quantity_precision = 8;
        s.instrument_type = None; // dig3：现货无 contractType
        s.extra = serde_json::json!({
            "symbol": "0GTRY", "status": "TRADING",
            "baseAsset": "0G", "quoteAsset": "TRY",
            "filters": [
                {"filterType": "PRICE_FILTER", "tickSize": "0.00100000"},
                {"filterType": "LOT_SIZE", "stepSize": "0.01000000"}
            ]
        });
        normalize("binance", "spot", &mut s);
        assert_eq!(s.price_precision, 3, "0.001 → 3 位，不是 dig3 的兜底 8");
        assert_eq!(s.quantity_precision, 2, "0.01 → 2 位");
        assert_eq!(s.instrument_type.as_deref(), Some("spot"));
        assert_eq!(s.status, "trading");
    }

    /// 现货两所：dig3 给的 base/quote 与原实现一致 → 归一化不得改动它们。
    #[test]
    fn normalize_keeps_spot_base_quote_untouched() {
        let mut s = mk_sym("TRADING");
        s.base_asset = "BTC".to_string();
        s.quote_asset = "USDT".to_string();
        s.extra = serde_json::json!({"symbol": "BTCUSDT", "status": "TRADING",
            "baseAsset": "BTC", "quoteAsset": "USDT"});
        normalize("binance", "spot", &mut s);
        assert_eq!(s.base_asset, "BTC");
        assert_eq!(s.quote_asset, "USDT");
    }

    fn mk_sym(status: &str) -> SymbolInfo {
        SymbolInfo {
            status: status.to_string(),
            ..Default::default()
        }
    }

    // ── V3 等价性：真 dig3 parser + 本仓 normalize ≟ 迁移前的库中行 ──────────
    //
    // 这是防「归一化漏字段 → 3724 行 hash 全变 → 全量指派重算」的回归闸。
    // 期望值直接取自 `control-store/control.db` 的 `instrument` 行（迁移前快照）。
    //
    // fixture 只保留**归一化与解析真正读取**的字段（其余如 `permissionSets` /
    // `orderTypes` 不参与，精简后结论不变）；`extra` 本身不比对 —— 它由生产路径
    // 原样透传（dig3 `parse_exchange_info` 是 `extra: s.clone()`），已单独核实。

    /// 按 Binance 现货真实响应的字段形状造一条 fixture。
    fn binance_spot_raw(
        symbol: &str,
        status: &str,
        base: &str,
        quote: &str,
        tick: &str,
        step: &str,
        min_qty: &str,
        min_notional: &str,
    ) -> Value {
        serde_json::json!({
            "symbol": symbol, "status": status,
            "baseAsset": base, "quoteAsset": quote,
            "filters": [
                {"filterType": "PRICE_FILTER", "tickSize": tick},
                {"filterType": "LOT_SIZE", "stepSize": step, "minQty": min_qty},
                {"filterType": "NOTIONAL", "minNotional": min_notional}
            ]
        })
    }

    /// `BinanceParser::parse_exchange_info`（**dig3 的真实实现**）→ `normalize`。
    fn via_dig3(raw: Value) -> SymbolInfo {
        let resp = serde_json::json!({"symbols": [raw]});
        let mut v = digdigdig3::l3::open::crypto::cex::binance::BinanceParser::parse_exchange_info(
            &resp,
            AccountType::Spot,
        )
        .expect("dig3 parse_exchange_info");
        assert_eq!(v.len(), 1);
        let mut s = v.pop().expect("one symbol");
        normalize("binance", "spot", &mut s);
        s
    }

    /// 行断言：`(status, price_precision, qty_precision, tick, step, min_notional, base, quote, instrument_type)`
    fn assert_matches_db(s: &SymbolInfo, want: (&str, u8, u8, f64, f64, f64, &str, &str), ctx: &str) {
        let (status, pp, qp, tick, step, mn, base, quote) = want;
        assert_eq!(s.status, status, "{ctx}: status");
        assert_eq!(s.price_precision, pp, "{ctx}: price_precision");
        assert_eq!(s.quantity_precision, qp, "{ctx}: qty_precision");
        assert_eq!(s.tick_size, Some(tick), "{ctx}: tick_size");
        assert_eq!(s.step_size, Some(step), "{ctx}: step_size");
        assert_eq!(s.min_notional, Some(mn), "{ctx}: min_notional");
        assert_eq!(s.base_asset, base, "{ctx}: base");
        assert_eq!(s.quote_asset, quote, "{ctx}: quote");
        assert_eq!(s.instrument_type.as_deref(), Some("spot"), "{ctx}: instrument_type");
    }

    /// 迁移前库中真实行的期望值（`instrument` 表快照）。
    ///
    /// 注意 dig3 自己的 `price_precision` 对这四行**全是 8**（现货记录没有
    /// `pricePrecision` 字段，它兜底成 8）—— 归一化后必须落回平台口径。
    #[test]
    fn equivalence_with_pre_migration_rows() {
        let cases: [(&str, Value, (&str, u8, u8, f64, f64, f64, &str, &str)); 4] = [
            (
                "BTCUSDT",
                binance_spot_raw("BTCUSDT", "TRADING", "BTC", "USDT",
                    "0.01000000", "0.00001000", "0.00001000", "5.00000000"),
                ("trading", 2, 5, 0.01, 0.00001, 5.0, "BTC", "USDT"),
            ),
            (
                "ETHUSDT",
                binance_spot_raw("ETHUSDT", "TRADING", "ETH", "USDT",
                    "0.01000000", "0.00010000", "0.00010000", "5.00000000"),
                ("trading", 2, 4, 0.01, 0.0001, 5.0, "ETH", "USDT"),
            ),
            // tick=0.001 → price_precision 3（dig3 会是 8）
            (
                "0GTRY",
                binance_spot_raw("0GTRY", "TRADING", "0G", "TRY",
                    "0.00100000", "0.01000000", "0.01000000", "10.00000000"),
                ("trading", 3, 2, 0.001, 0.01, 10.0, "0G", "TRY"),
            ),
            // 所方 status=BREAK → 平台 `break`；tick=1e-8 → price_precision 8
            (
                "0GBNB",
                binance_spot_raw("0GBNB", "BREAK", "0G", "BNB",
                    "0.00000001", "0.01000000", "0.01000000", "0.01000000"),
                ("break", 8, 2, 1e-8, 0.01, 0.01, "0G", "BNB"),
            ),
        ];

        for (code, raw, want) in cases {
            let s = via_dig3(raw);
            assert_eq!(s.symbol, code, "{code}: native_code");
            assert_matches_db(&s, want, code);
        }
    }

    /// dig3 在现货上给的 `price_precision` **确实**是兜底 8 —— 若哪天归一化被
    /// 绕过，这条会先红，比 3724 行 hash 全变早得多。
    #[test]
    fn dig3_spot_precision_needs_normalization() {
        let resp = serde_json::json!({"symbols": [binance_spot_raw(
            "0GTRY", "TRADING", "0G", "TRY",
            "0.00100000", "0.01000000", "0.01000000", "10.00000000")]});
        let raw = digdigdig3::l3::open::crypto::cex::binance::BinanceParser::parse_exchange_info(
            &resp,
            AccountType::Spot,
        )
        .expect("parse");
        assert_eq!(raw[0].price_precision, 8, "dig3 对现货兜底成 8（无 pricePrecision 字段）");
        assert_eq!(raw[0].instrument_type, None, "dig3 对现货给 None（无 contractType）");
        assert_eq!(raw[0].status, "TRADING", "dig3 保留所方原值，不做过滤");
    }
}
