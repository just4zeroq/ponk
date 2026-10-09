//! 标的选择规则：**catalog 事实 ⋈ 节点声明式意图 → 逐条候选**（`docs/12 §5`）。
//!
//! # 为什么是规则式
//!
//! 全市场是常态（一个所能有上千标的），逐条维护既写不出也改不动，且每次上新都要动配置。
//! 因此**标的事实由 catalog 自动维护**（5 分钟同步，`docs/09 §2.4`），节点只声明"我要什么"：
//!
//! ```text
//! 候选 = catalog(venue/market/quote/status 过滤)
//!      − base_exclude − scope_exclude（例外排除）
//!      + scope_include（例外强制纳入）
//!      − InstrumentBlocklist 域（黑名单，优先级最高，09 §2）
//!      → 切分片 → 指派 → SubscribePlan
//! ```
//!
//! **黑名单在指派阶段统一剔除**（`assign::index_blocklist`），本模块不重复处理——
//! 这样 `scope_include` 也拉不回黑名单标的（`docs/12 §5` 的硬约束）。
//!
//! **逐条 `instruments` 不废弃**：它退化为"手工池"（A 方案里无法用规则表达的精确清单），
//! 与规则解析结果合并后进入指派。

use std::collections::BTreeMap;

use ponk_protocol::SymbolInfo;

use crate::catalog::Catalog;
use crate::error::ControlError;
use crate::schema::PlanInstrument;

/// 节点声明式标的范围（`docs/12 §5`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ScopeRules {
    /// 交易所（空 = 全部）。
    pub venues: Vec<String>,
    /// 市场类型（空 = 全部；`spot` / `swap` …）。
    pub markets: Vec<String>,
    /// 该节点要采的通道（`candle_1m` / `trade` / `ticker` / `l1` / `funding`，`docs/02 §2`）。
    pub channels: Vec<String>,
    /// 计价币过滤，如 `["USDT"]`；空 = 不限。
    pub quote: Vec<String>,
    /// 基础币白名单（可选；空 = 不限）。
    pub base: Vec<String>,
    /// 基础币排除。
    pub base_exclude: Vec<String>,
    /// 交易对状态过滤；空 = 不限，`["trading"]` 为默认推荐。
    pub status: Vec<String>,
    /// 分片大小（标的数/分片，`docs/09 §5.1`）。
    pub shard_size: u64,
}

impl Default for ScopeRules {
    fn default() -> Self {
        Self {
            venues: Vec::new(),
            markets: Vec::new(),
            channels: vec![
                "candle_1m".into(),
                "trade".into(),
                "ticker".into(),
                "l1".into(),
                "funding".into(),
            ],
            quote: Vec::new(),
            base: Vec::new(),
            base_exclude: Vec::new(),
            status: vec!["trading".into()],
            shard_size: 200,
        }
    }
}

impl ScopeRules {
    /// 校验（节点创建 / 修改时调用）。
    pub fn validate(&self) -> Result<(), ControlError> {
        if self.shard_size == 0 {
            return Err(ControlError::validation("scope.shard_size 必须 >= 1（09 §5.1）"));
        }
        for ch in &self.channels {
            if ch.is_empty() {
                return Err(ControlError::validation("scope.channels 存在空通道名"));
            }
        }
        for v in &self.venues {
            if v.trim().is_empty() {
                return Err(ControlError::validation("scope.venues 存在空交易所名"));
            }
        }
        Ok(())
    }
}

/// 规则解析结果：入选标的 + 被跳过的条目及原因（供 CLI/日志如实汇报）。
#[derive(Debug, Default, Clone)]
pub struct ScopeOutcome {
    /// 入选的逐条候选（可直接进指派）。
    pub instruments: Vec<PlanInstrument>,
    /// 例外清单里**catalog 查不到**的条目（无法取得精度等事实，不敢臆造）。
    pub unresolved: Vec<(String, String)>,
}

impl ScopeOutcome {
    /// 跳过数（未解析的例外条目）。
    pub fn unresolved_count(&self) -> usize {
        self.unresolved.len()
    }
}

/// 例外清单条目：支持**canonical**（`venue:market:native_code`，精确到所）与
/// **裸码**（`BTCUSDT`，跨所生效）两种写法。
fn matches_entry(entry: &str, venue: &str, market: &str, native: &str) -> bool {
    if let Some((v, m, n)) = entry.split_once(':').and_then(|(a, rest)| {
        let (m, n) = rest.split_once(':')?;
        Some((a, m, n))
    }) {
        return v.eq_ignore_ascii_case(venue)
            && m.eq_ignore_ascii_case(market)
            && n.eq_ignore_ascii_case(native);
    }
    entry.eq_ignore_ascii_case(native)
}

/// 把 dig3 `SymbolInfo` 转成指派用的逐条候选（精度直取，`meta` 承接原样字段）。
fn to_plan_instrument(
    venue: &str,
    market: &str,
    code: &str,
    info: &SymbolInfo,
    channels: &[String],
) -> PlanInstrument {
    let mut meta: BTreeMap<String, String> = BTreeMap::new();
    if let Some(tick) = &info.tick_size {
        meta.insert("tick_size".into(), tick.to_string());
    }
    if let Some(step) = &info.step_size {
        meta.insert("lot_size".into(), step.to_string());
    }
    if let Some(min) = &info.min_notional {
        meta.insert("min_notional".into(), min.to_string());
    }
    if let Some(t) = &info.instrument_type {
        meta.insert("instrument_type".into(), t.clone());
    }
    meta.insert("base".into(), info.base_asset.clone());
    meta.insert("quote".into(), info.quote_asset.clone());

    PlanInstrument {
        venue: venue.to_string(),
        market: market.to_string(),
        native_code: code.to_string(),
        channels: channels.to_vec(),
        price_precision: u32::from(info.price_precision),
        qty_precision: u32::from(info.quantity_precision),
        meta,
    }
}

/// 用 `scope` 从 catalog 解析出逐条候选（`docs/12 §5`）。
///
/// * `include` / `exclude` 为例外清单（可为 canonical 或裸码）；
/// * **黑名单不在这里处理**（`docs/12 §5`：统一在指派阶段剔除，优先级最高）；
/// * catalog 里没有的例外条目**不臆造**，进 [`ScopeOutcome::unresolved`]。
pub fn resolve(
    scope: &ScopeRules,
    include: &[String],
    exclude: &[String],
    catalog: &Catalog,
) -> ScopeOutcome {
    let mut out = ScopeOutcome::default();
    let mut picked: BTreeMap<(String, String, String), PlanInstrument> = BTreeMap::new();

    let in_list = |list: &[String], code: &str| {
        list.is_empty() || list.iter().any(|p| matches_entry(p, "", "", code))
    };

    for vm in catalog.symbols.keys() {
        let Some((venue, market)) = vm.split_once('|') else {
            continue;
        };
        if !scope.venues.is_empty()
            && !scope
                .venues
                .iter()
                .any(|v| v.eq_ignore_ascii_case(venue))
        {
            continue;
        }
        if !scope.markets.is_empty()
            && !scope
                .markets
                .iter()
                .any(|m| m.eq_ignore_ascii_case(market))
        {
            continue;
        }
        let Some(map) = catalog.symbols.get(vm) else {
            continue;
        };
        for (code, info) in map {
            if !scope.quote.is_empty() && !in_list(&scope.quote, &info.quote_asset) {
                continue;
            }
            if !scope.base.is_empty() && !in_list(&scope.base, &info.base_asset) {
                continue;
            }
            // 排除语义与"空=不限"相反：空列表 = 什么都不排除
            if scope
                .base_exclude
                .iter()
                .any(|b| b.eq_ignore_ascii_case(&info.base_asset))
            {
                continue;
            }
            if !scope.status.is_empty()
                && !scope.status.iter().any(|s| s.eq_ignore_ascii_case(&info.status))
            {
                continue;
            }
            if exclude.iter().any(|e| matches_entry(e, venue, market, code)) {
                continue;
            }
            picked.insert(
                (venue.to_string(), market.to_string(), code.clone()),
                to_plan_instrument(venue, market, code, info, &scope.channels),
            );
        }
    }

    // 例外强制纳入：catalog 有事实才收（没有事实不臆造精度）
    for inc in include {
        let parts: Vec<&str> = inc.split(':').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
            out.unresolved.push((
                inc.clone(),
                "必须写 canonical（venue:market:native_code，02 §3）".into(),
            ));
            continue;
        }
        let key = (
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        );
        if picked.contains_key(&key) {
            continue;
        }
        let Some(info) = catalog
            .symbols
            .get(&format!("{}|{}", key.0, key.1))
            .and_then(|m| m.get(&key.2))
        else {
            out.unresolved
                .push((inc.clone(), "catalog 中不存在（未上架或未同步）".into()));
            continue;
        };
        picked.insert(
            key.clone(),
            to_plan_instrument(&key.0, &key.1, &key.2, info, &scope.channels),
        );
    }

    out.instruments = picked.into_values().collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(base: &str, quote: &str, status: &str) -> SymbolInfo {
        SymbolInfo {
            symbol: format!("{base}{quote}"),
            base_asset: base.into(),
            quote_asset: quote.into(),
            status: status.into(),
            price_precision: 2,
            quantity_precision: 6,
            min_quantity: None,
            max_quantity: None,
            tick_size: Some(0.01),
            step_size: None,
            min_notional: None,
            account_type: Default::default(),
            instrument_type: Some("spot".into()),
            extra: serde_json::json!({"lotSizeFilter": {"minQty": "0.001"}}),
        }
    }

    fn catalog() -> Catalog {
        let mut symbols = BTreeMap::new();
        let mut spot = BTreeMap::new();
        spot.insert("BTCUSDT".to_string(), info("BTC", "USDT", "trading"));
        spot.insert("ETHUSDT".to_string(), info("ETH", "USDT", "trading"));
        spot.insert("XRPUSDT".to_string(), info("XRP", "USDT", "BREAK"));
        spot.insert("BTCFDUSD".to_string(), info("BTC", "FDUSD", "trading"));
        symbols.insert("binance|spot".to_string(), spot);
        let mut swap = BTreeMap::new();
        swap.insert("BTCUSDT".to_string(), info("BTC", "USDT", "trading"));
        symbols.insert("binance|swap".to_string(), swap);
        Catalog {
            symbols,
            ..Default::default()
        }
    }

    #[test]
    fn filters_by_quote_status_and_market() {
        let mut scope = ScopeRules::default();
        scope.quote = vec!["USDT".into()];
        scope.markets = vec!["spot".into()];
        let out = resolve(&scope, &[], &[], &catalog());
        let codes: Vec<&str> = out.instruments.iter().map(|i| i.native_code.as_str()).collect();
        // BTCUSDT ✓ ETHUSDT ✓ XRPUSDT(状态非 trading) ✗ BTCFDUSD(quote) ✗ swap 整组 ✗
        assert_eq!(codes, vec!["BTCUSDT", "ETHUSDT"], "{codes:?}");
        assert!(out.instruments[0].channels.contains(&"candle_1m".to_string()));
        assert_eq!(out.instruments[0].price_precision, 2, "精度取自 catalog 事实");
        assert_eq!(out.instruments[0].meta.get("base").unwrap(), "BTC");
    }

    #[test]
    fn scoped_venues_exclude_everything_else() {
        let mut scope = ScopeRules::default();
        scope.venues = vec!["okx".into()];
        let out = resolve(&scope, &[], &[], &catalog());
        assert!(out.instruments.is_empty(), "okx 未在 catalog 里 → 空");
    }

    #[test]
    fn exclude_supports_bare_and_canonical_forms() {
        let mut scope = ScopeRules::default();
        scope.quote = vec!["USDT".into()];
        scope.markets = vec!["spot".into()];
        // 裸码 = 跨所生效；canonical = 精确到所
        let out = resolve(&scope, &[], &["ETHUSDT".into()], &catalog());
        assert_eq!(
            out.instruments.iter().map(|i| i.native_code.as_str()).collect::<Vec<_>>(),
            vec!["BTCUSDT"],
            "裸码排除 ETHUSDT"
        );
        // 不限 markets：canonical 排除只应命中 binance:swap:BTCUSDT，
        // binance:spot:BTCUSDT 与另一些标的必须仍在。
        let mut wide = ScopeRules::default();
        wide.quote = vec!["USDT".into()];
        let out2 = resolve(&wide, &[], &["binance:swap:BTCUSDT".into()], &catalog());
        let key = |m: &str, c: &str| (m.to_string(), c.to_string());
        assert!(
            out2.instruments
                .iter()
                .any(|i| key(&i.market, &i.native_code) == key("spot", "BTCUSDT")),
            "spot 的 BTCUSDT 不应被 canonical 排除：{:?}",
            out2.instruments
        );
        assert!(
            !out2
                .instruments
                .iter()
                .any(|i| key(&i.market, &i.native_code) == key("swap", "BTCUSDT")),
            "canonical 应精确剔除 binance:swap:BTCUSDT"
        );
    }

    #[test]
    fn include_requires_catalog_facts_and_reports_unresolved() {
        let mut scope = ScopeRules::default();
        scope.markets = vec!["spot".into()];
        scope.quote = vec!["USDT".into()];
        let base_count = resolve(&scope, &[], &[], &catalog()).instruments.len();

        // 规则未覆盖 swap，例外仍纳入
        let out = resolve(&scope, &["binance:swap:BTCUSDT".into()], &[], &catalog());
        assert_eq!(out.instruments.len(), base_count + 1, "例外应在规则之上叠加");
        assert!(out
            .instruments
            .iter()
            .any(|i| i.market == "swap" && i.native_code == "BTCUSDT"));
        assert_eq!(out.unresolved_count(), 0);

        // catalog 无事实 → 不臆造精度
        let out2 = resolve(&scope, &["binance:spot:NEWUSDT".into()], &[], &catalog());
        assert_eq!(out2.instruments.len(), base_count, "未解析的例外不得凭空出现");
        assert_eq!(out2.unresolved_count(), 1, "catalog 无事实 → 不臆造");
        assert!(out2.unresolved[0].1.contains("不存在"));
    }

    #[test]
    fn defaults_are_usable_and_validated() {
        let s = ScopeRules::default();
        assert_eq!(s.shard_size, 200);
        assert!(s.status.contains(&"trading".to_string()));
        s.validate().unwrap();
        let mut bad = ScopeRules::default();
        bad.shard_size = 0;
        assert!(bad.validate().is_err());
    }
}
