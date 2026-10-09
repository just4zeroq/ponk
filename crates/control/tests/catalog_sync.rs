//! 09 §2.3 定时任务：交易对与币信息同步。
//!
//! 全部**离线**：拉取器是注入的假实现（无外网、无 Redis）。
//!
//! 必测（交付要求）：**变更触发重算、无变更零动作、catalog 比对逻辑、节流**。

mod common;

use common::{temp_store, Cleanup};
use ponk_control::{
    changed_fields, diff_catalog, CatalogSync, ControlConfig, ControlPlane, MetaError,
    MetadataFetcher, MetadataQuota, NewExchange, NewMarket, SyncOutcome, TokenBucket,
    UnwiredFetcher, META_SYNC_INTERVAL_MS,
};
use ponk_eventcenter::{EventCenter, MemoryEventCenter, SessionOpts, StartPosition};
use ponk_protocol::{
    Body, ConfigDomain, CtlMessage, CurrencyInfo, Group, MarketData, NodeKind, OpsHeartbeat,
    Status, SymbolInfo, Topic,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const NOW: u64 = 1_770_000_000_000;

// ── 假拉取器 ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct FakeState {
    symbols: Vec<SymbolInfo>,
    currencies: Vec<CurrencyInfo>,
    fail_symbols: bool,
    fail_currencies: bool,
}

/// 注入的假拉取器（离线；可原地改数据以模拟所方变更）。
struct FakeFetcher {
    state: Mutex<FakeState>,
    symbol_calls: AtomicUsize,
    currency_calls: AtomicUsize,
}

impl FakeFetcher {
    fn new() -> Self {
        FakeFetcher {
            state: Mutex::new(FakeState::default()),
            symbol_calls: AtomicUsize::new(0),
            currency_calls: AtomicUsize::new(0),
        }
    }

    fn with(symbols: Vec<SymbolInfo>, currencies: Vec<CurrencyInfo>) -> Self {
        let f = FakeFetcher::new();
        *f.state.lock().unwrap() = FakeState {
            symbols,
            currencies,
            fail_symbols: false,
            fail_currencies: false,
        };
        f
    }
}

impl MetadataFetcher for FakeFetcher {
    fn fetch_symbols(&self, _venue: &str, _market: &str) -> Result<Vec<SymbolInfo>, MetaError> {
        self.symbol_calls.fetch_add(1, Ordering::SeqCst);
        let st = self.state.lock().unwrap();
        if st.fail_symbols {
            return Err(MetaError::Fetch("boom".into()));
        }
        Ok(st.symbols.clone())
    }

    fn fetch_currencies(&self, _venue: &str) -> Result<Vec<CurrencyInfo>, MetaError> {
        self.currency_calls.fetch_add(1, Ordering::SeqCst);
        let st = self.state.lock().unwrap();
        if st.fail_currencies {
            return Err(MetaError::Fetch("boom".into()));
        }
        Ok(st.currencies.clone())
    }
}

fn sym(code: &str, base: &str, status: &str, price_precision: u8) -> SymbolInfo {
    SymbolInfo {
        symbol: code.to_string(),
        base_asset: base.to_string(),
        quote_asset: "USDT".to_string(),
        status: status.to_string(),
        price_precision,
        quantity_precision: 3,
        ..Default::default()
    }
}

fn currency(code: &str) -> CurrencyInfo {
    let mut c = CurrencyInfo::new(code);
    c.networks = vec!["TRC20".to_string(), "ERC20".to_string()];
    c
}

fn targets() -> Vec<(String, String)> {
    vec![("binance".to_string(), "swap".to_string())]
}

fn quotas() -> Vec<ponk_control::QuotaSpec> {
    vec![ponk_control::QuotaSpec {
        venue: "binance".to_string(),
        egress_ip: "1.2.3.4".to_string(),
        collect: 0.5,
        backfill: 0.35,
        metadata: 0.15,
    }]
}

// ── catalog 比对逻辑（09 §2.3「逐字段比对」）────────────────────────────────

#[test]
fn changed_fields_reports_exactly_the_differing_fields() {
    let a = sym("BTCUSDT", "BTC", "TRADING", 1);
    let mut b = sym("BTCUSDT", "BTC", "TRADING", 1);
    assert!(changed_fields(&a, &b).is_empty(), "完全相同 → 无字段差异");

    b.price_precision = 2;
    assert_eq!(changed_fields(&a, &b), vec!["price_precision".to_string()]);

    let mut c = sym("BTCUSDT", "BTC", "BREAK", 1);
    c.instrument_type = Some("PERPETUAL".into());
    let mut fields = changed_fields(&a, &c);
    fields.sort();
    assert_eq!(fields, vec!["instrument_type".to_string(), "status".to_string()]);

    // `extra` 是所方原始透传：嵌套键序无关（03 §5.4 规范化视图）
    let mut x = sym("BTCUSDT", "BTC", "TRADING", 1);
    x.extra = serde_json::json!({"a": 1, "b": {"c": 2}});
    let mut y = sym("BTCUSDT", "BTC", "TRADING", 1);
    y.extra = serde_json::json!({"b": {"c": 2}, "a": 1});
    assert!(
        changed_fields(&x, &y).is_empty(),
        "键序不同但内容相同 → 视为无差异"
    );
    y.extra = serde_json::json!({"b": {"c": 3}, "a": 1});
    assert_eq!(changed_fields(&x, &y), vec!["extra".to_string()]);
}

#[test]
fn catalog_diff_detects_added_removed_and_changed() {
    let mut old = ponk_control::Catalog::default();
    old.symbols.insert(
        "binance|swap".into(),
        [
            ("BTCUSDT", sym("BTCUSDT", "BTC", "TRADING", 1)),
            ("ETHUSDT", sym("ETHUSDT", "ETH", "TRADING", 2)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    );
    old.currencies
        .insert("binance".into(), [("USDT", currency("USDT"))].into_iter().map(|(k, v)| (k.to_string(), v)).collect());

    let mut new = old.clone();
    // 上新 SOL、下架 ETH、BTC 改精度
    new.symbols.get_mut("binance|swap").unwrap().insert("SOLUSDT".into(), sym("SOLUSDT", "SOL", "TRADING", 2));
    new.symbols.get_mut("binance|swap").unwrap().remove("ETHUSDT");
    new.symbols.get_mut("binance|swap").unwrap().get_mut("BTCUSDT").unwrap().price_precision = 3;
    // 币信息：改名 + 新增
    new.currencies.get_mut("binance").unwrap().get_mut("USDT").unwrap().name = Some("Tether USD".into());
    new.currencies.get_mut("binance").unwrap().insert("SOL".into(), currency("SOL"));

    let d = diff_catalog(&old, &new);
    assert_eq!(d.symbols_added, vec!["binance|swap|SOLUSDT"]);
    assert_eq!(d.symbols_removed, vec!["binance|swap|ETHUSDT"]);
    assert_eq!(d.symbols_changed.len(), 1);
    assert_eq!(d.symbols_changed[0].0, "binance|swap|BTCUSDT");
    assert_eq!(d.symbols_changed[0].1, vec!["price_precision".to_string()]);
    assert_eq!(d.currencies_added, vec!["binance|SOL"]);
    assert_eq!(d.currencies_changed.len(), 1);
    assert!(d.currencies_removed.is_empty());

    assert!(!d.is_empty());
    assert!(d.triggers_reassignment(), "标的变更 → 触发指派重算（09 §5.4）");

    // 纯币信息变化不改标的集 → 不触发重算
    let mut only_currency = old.clone();
    only_currency
        .currencies
        .get_mut("binance")
        .unwrap()
        .insert("BTC".into(), currency("BTC"));
    let d2 = diff_catalog(&old, &only_currency);
    assert!(!d2.is_empty());
    assert!(
        !d2.triggers_reassignment(),
        "纯币信息变化不改分片归属"
    );
}

// ── 节流：02 §6 元数据份额 ──────────────────────────────────────────────────

#[test]
fn token_bucket_throttles_at_the_metadata_share_rate() {
    let mut b = TokenBucket::new(0.001, 1.0, 0); // 1 token / 1000ms
    assert!(b.try_acquire(0), "初始满桶应可取");
    assert!(!b.try_acquire(0), "同一时刻第二次 → 节流");
    assert!(!b.try_acquire(500), "半周期只补一半令牌");
    assert!(b.try_acquire(1000), "补足后可取");
    assert!(!b.try_acquire(1001));
}

#[test]
fn metadata_quota_uses_the_metadata_share_only() {
    let q = MetadataQuota {
        share: 0.15,
        base_requests_per_minute: 60.0,
    };
    // 02 §6：元数据 15% × 60 次/分钟 = 9 次/分钟 = 0.00015 次/ms
    assert!((q.rate_per_ms() - 0.00015).abs() < 1e-12);
    let zero = MetadataQuota { share: 0.0, ..MetadataQuota::default() };
    assert_eq!(zero.rate_per_ms(), 0.0, "份额 0 → 完全节流");
}

#[test]
fn sync_round_throttles_once_the_venue_bucket_is_empty() {
    let dir = temp_store("throttle");
    let _c = Cleanup::new(dir.clone());

    let fetcher = FakeFetcher::with(
        vec![sym("BTCUSDT", "BTC", "TRADING", 1)],
        vec![currency("USDT")],
    );
    let mut sync = CatalogSync::new(Some(dir.clone()));
    sync.quota = MetadataQuota { share: 0.0, ..MetadataQuota::default() }; // 速率 0：桶不补
    sync.burst = 1.0; // 每所每轮只放 1 个请求

    // 每轮要 2 个令牌（币信息 1 + 交易对 1），容量 1 → 第 2 次必被节流
    let out = sync
        .run(&targets(), &quotas(), &fetcher, NOW)
        .unwrap();
    assert!(out.ran);
    assert_eq!(fetcher.currency_calls.load(Ordering::SeqCst), 1, "币信息先取");
    assert_eq!(
        fetcher.symbol_calls.load(Ordering::SeqCst),
        0,
        "令牌桶耗尽 → 交易对被节流"
    );
    assert!(out
        .skipped
        .iter()
        .any(|s| s.reason.contains("throttled") && s.market.as_deref() == Some("swap")),
        "实际 {:?}", out.skipped);
    assert!(out.fetched_symbols == 0);
    assert!(out.fetched_currencies == 1);
}

#[test]
fn fetch_failure_is_recorded_but_does_not_abort_the_round() {
    let dir = temp_store("fetchfail");
    let _c = Cleanup::new(dir.clone());
    let fetcher = FakeFetcher::with(vec![], vec![]);
    {
        let mut st = fetcher.state.lock().unwrap();
        st.fail_currencies = true;
    }
    let mut sync = CatalogSync::new(Some(dir));
    let out = sync.run(&targets(), &quotas(), &fetcher, NOW).unwrap();
    assert!(out.ran);
    assert!(out.skipped.iter().any(|s| s.reason.contains("boom")));
    assert!(!out.changed, "拉不到数据 → catalog 不变 → 零动作");
}

// ── 无变更零动作 / 变更触发重算 ────────────────────────────────────────────

#[test]
fn not_due_is_a_zero_action_round() {
    let dir = temp_store("due");
    let _c = Cleanup::new(dir.clone());
    let fetcher = FakeFetcher::with(vec![sym("BTCUSDT", "BTC", "TRADING", 1)], vec![]);
    let mut sync = CatalogSync::new(Some(dir));
    assert_eq!(sync.interval_ms, META_SYNC_INTERVAL_MS, "09 §2.3：每 5 分钟一轮");
    assert!(sync.due(NOW), "首轮总是到点");

    let out = sync.run(&targets(), &quotas(), &fetcher, NOW).unwrap();
    assert!(out.ran && out.changed, "首轮建立 catalog");

    // 未到 5 分钟 → 连拉取都不做
    let calls_before = fetcher.symbol_calls.load(Ordering::SeqCst);
    let out2 = sync
        .run(&targets(), &quotas(), &fetcher, NOW + META_SYNC_INTERVAL_MS - 1)
        .unwrap();
    assert!(!out2.ran, "未到周期");
    assert!(!out2.changed);
    assert_eq!(
        fetcher.symbol_calls.load(Ordering::SeqCst),
        calls_before,
        "未到周期不拉取"
    );
}

#[test]
fn identical_catalog_is_zero_action() {
    let dir = temp_store("nochurn");
    let _c = Cleanup::new(dir.clone());
    let fetcher = FakeFetcher::with(
        vec![
            sym("BTCUSDT", "BTC", "TRADING", 1),
            sym("ETHUSDT", "ETH", "TRADING", 2),
        ],
        vec![currency("USDT"), currency("BTC")],
    );
    let mut sync = CatalogSync::new(Some(dir));

    let first = sync.run(&targets(), &quotas(), &fetcher, NOW).unwrap();
    assert!(first.changed, "空 → 有：首轮有差异");
    assert_eq!(first.md_meta.len(), 2);
    assert!(first.diff.triggers_reassignment());
    let symbols_before = sync.catalog().symbol_count();

    // 第二轮：同数据 → 零动作
    let second = sync
        .run(&targets(), &quotas(), &fetcher, NOW + META_SYNC_INTERVAL_MS)
        .unwrap();
    assert!(second.ran, "到点了，本轮真的执行了");
    assert!(
        !second.changed,
        "逐字段无差异 → changed = false（硬要求）"
    );
    assert!(second.md_meta.is_empty(), "无差异不发 md.meta");
    assert!(!second.recompute_ran);
    assert!(second.diff.is_empty());
    assert_eq!(sync.catalog().symbol_count(), symbols_before);
    assert_eq!(
        second.diff.summary(),
        "标的 +0 -0 ~0；币 +0 -0 ~0",
        "零动作的摘要"
    );

    // 第三轮：仍然同数据 → 依然零动作（不空转 bump）
    let third = sync
        .run(&targets(), &quotas(), &fetcher, NOW + 2 * META_SYNC_INTERVAL_MS)
        .unwrap();
    assert!(third.ran && !third.changed && third.md_meta.is_empty());
}

#[test]
fn symbol_status_change_produces_md_meta_for_that_symbol_only() {
    let dir = temp_store("changed");
    let _c = Cleanup::new(dir.clone());
    let fetcher = FakeFetcher::with(
        vec![
            sym("BTCUSDT", "BTC", "TRADING", 1),
            sym("ETHUSDT", "ETH", "TRADING", 2),
        ],
        vec![currency("USDT")],
    );
    let mut sync = CatalogSync::new(Some(dir));
    sync.run(&targets(), &quotas(), &fetcher, NOW).unwrap();

    // 所方把 BTC 停牌
    {
        let mut st = fetcher.state.lock().unwrap();
        st.symbols[0] = sym("BTCUSDT", "BTC", "BREAK", 1);
    }
    let out = sync
        .run(&targets(), &quotas(), &fetcher, NOW + META_SYNC_INTERVAL_MS)
        .unwrap();
    assert!(out.changed);
    assert!(out.diff.triggers_reassignment());
    assert_eq!(out.diff.symbols_changed.len(), 1);
    assert_eq!(out.diff.symbols_changed[0].1, vec!["status".to_string()]);
    assert_eq!(out.md_meta.len(), 1, "只有变更的标的发生 md.meta");

    let env = &out.md_meta[0];
    assert_eq!(env.event_type, ponk_protocol::EventType::Market);
    match &env.body {
        Body::Market(m) => {
            assert_eq!(m.venue.as_str(), "binance");
            assert_eq!(m.market.as_str(), "swap");
            assert_eq!(m.native_code, "BTCUSDT");
            match &m.data {
                MarketData::Meta(p) => {
                    assert_eq!(p.symbol.status, "BREAK");
                    assert!(!p.currencies.is_empty(), "币信息随标的一起发");
                    assert_eq!(p.currencies[0].currency, "USDT");
                }
                other => panic!("期望 MarketData::Meta，实际 {other:?}"),
            }
        }
        other => panic!("期望 Body::Market，实际 {other:?}"),
    }
}

#[test]
fn currency_only_change_emits_md_meta_without_reassignment() {
    let dir = temp_store("currency");
    let _c = Cleanup::new(dir.clone());
    let fetcher = FakeFetcher::with(vec![sym("BTCUSDT", "BTC", "TRADING", 1)], vec![currency("USDT")]);
    let mut sync = CatalogSync::new(Some(dir));
    sync.run(&targets(), &quotas(), &fetcher, NOW).unwrap();

    {
        let mut st = fetcher.state.lock().unwrap();
        st.currencies = vec![currency("USDT"), currency("BTC")]; // 新增 BTC 币信息
    }
    let out = sync
        .run(&targets(), &quotas(), &fetcher, NOW + META_SYNC_INTERVAL_MS)
        .unwrap();
    assert!(out.changed, "币信息变化仍是变化");
    assert!(
        !out.diff.triggers_reassignment(),
        "纯币信息变化不改标的集 → 不触发指派重算"
    );
    assert!(!out.md_meta.is_empty(), "仍要发 md.meta（存储层要落库）");
}

// ── 控制面集成：变更触发重算 / 无变更零动作 ─────────────────────────────────

fn cfg(dir: &std::path::Path) -> ControlConfig {
    ControlConfig {
        store_root: dir.to_path_buf(),
        ..ControlConfig::default()
    }
}

fn plan_doc() -> serde_json::Value {
    serde_json::json!({
        "shard_size": 8,
        "default_channels": ["md.candle.1m"],
        "instruments": [
            {"venue": "binance", "market": "swap", "native_code": "BTCUSDT",
             "price_precision": 1, "qty_precision": 0},
            {"venue": "binance", "market": "swap", "native_code": "ETHUSDT",
             "price_precision": 2, "qty_precision": 3}
        ],
        "quota": [{"venue": "binance", "egress_ip": "1.2.3.4",
                   "collect": 0.5, "backfill": 0.35, "metadata": 0.15}]
    })
}

/// `docs/14 §2`：同步目标 = `exchange_market` 中 `enabled=1` 的 `(venue, market)`，
/// **不再由 `SubscribePlan` 域派生**（那是采集意图，冷启动为空 → 先有鸡还是先有蛋）。
/// 测试照此语义先在接入台账里登记 `(venue, market)`。
fn seed_market(
    plane: &ControlPlane<MemoryEventCenter>,
    venue: &str,
    market: &str,
    account_type: &str,
) {
    let db = plane.store().sqlite().expect("sqlite 后端").clone();
    db.upsert_exchange(&NewExchange {
        venue: venue.to_string(),
        name: venue.to_uppercase(),
        ..NewExchange::default()
    })
    .expect("登记交易所");
    db.upsert_market(&NewMarket {
        venue: venue.to_string(),
        market: market.to_string(),
        dig3_account_type: account_type.to_string(),
        ..NewMarket::default()
    })
    .expect("登记市场");
}

fn register(plane: &mut ControlPlane<MemoryEventCenter>, id: &str) {
    let hb = OpsHeartbeat {
        host: None,
        instance_id: id.to_string(),
        node_kind: NodeKind::Collector,
        status: Status::Startup,
        applied_version: Some(0),
        counters: Default::default(),
        started_at: NOW,
        ts: NOW,
    };
    let env = ponk_control::ops_envelope("peer", NOW, hb).unwrap();
    plane.dispatch(&format!("ops.collector.{id}"), env).unwrap();
}

#[test]
fn metadata_sync_triggers_reassignment_on_change_and_is_zero_action_otherwise() {
    let dir = temp_store("plane-meta");
    let _c = Cleanup::new(dir.clone());
    let ec = MemoryEventCenter::new();
    let bus = ec.clone();
    let mut plane = ControlPlane::open(ec, cfg(&dir)).unwrap();

    plane
        .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1", "i")
        .unwrap();
    register(&mut plane, "col-1");
    seed_market(&plane, "binance", "swap", "FuturesCross");
    let v0 = plane.plan_of("col-1").unwrap().assignment_version;
    assert_eq!(v0, 1);

    let fetcher = FakeFetcher::with(vec![sym("BTCUSDT", "BTC", "TRADING", 1)], vec![currency("USDT")]);
    plane.set_fetcher(std::sync::Arc::new(fetcher));

    let mut meta_watcher = bus
        .open(
            &Topic::new("md.meta"),
            &Group::new("storage"),
            SessionOpts {
                timeout: Duration::from_millis(300),
                start: StartPosition::Earliest,
            },
        )
        .unwrap();

    // ① 首轮：空 catalog → 有数据 = 变更 → 触发指派重算
    let o1 = plane.run_metadata_sync(NOW).unwrap();
    assert!(o1.ran && o1.changed, "首轮建立 catalog");
    assert!(o1.recompute_ran, "标的变更触发指派重算（09 §5.4）");
    let v1 = plane.plan_of("col-1").unwrap().assignment_version;
    assert!(
        v1 > v0,
        "强制下发 → assignment_version 递增 {v0} → {v1}"
    );
    assert!(
        meta_watcher
            .next(Duration::from_millis(300))
            .unwrap()
            .is_some(),
        "md.meta 事件经总线发出（红线 4）"
    );

    // ② 同数据 → 零动作
    let v_before = plane.plan_of("col-1").unwrap().assignment_version;
    let o2 = plane.run_metadata_sync(NOW + META_SYNC_INTERVAL_MS).unwrap();
    assert!(o2.ran);
    assert!(!o2.changed, "无差异 → 零动作");
    assert!(!o2.recompute_ran, "不触发重算");
    assert!(o2.md_meta.is_empty());
    assert_eq!(
        plane.plan_of("col-1").unwrap().assignment_version,
        v_before,
        "不 bump assignment_version、不发 ConfigChanged"
    );
    assert!(
        meta_watcher
            .next(Duration::from_millis(100))
            .unwrap()
            .is_none(),
        "无差异不发 md.meta"
    );

    // ③ 变更 → 再次触发重算
    // （拉取器是桥接对象，通过重新 set_fetcher 换一份数据）
    let changed = FakeFetcher::with(vec![sym("BTCUSDT", "BTC", "BREAK", 1)], vec![currency("USDT")]);
    plane.set_fetcher(std::sync::Arc::new(changed));
    let o3 = plane
        .run_metadata_sync(NOW + 2 * META_SYNC_INTERVAL_MS)
        .unwrap();
    assert!(o3.changed && o3.recompute_ran, "实际 changed={} recompute={}", o3.changed, o3.recompute_ran);
    let v3 = plane.plan_of("col-1").unwrap().assignment_version;
    assert!(v3 > v_before, "{v_before} → {v3}");
}

#[test]
fn unwired_fetcher_keeps_catalog_unchanged() {
    let dir = temp_store("unwired");
    let _c = Cleanup::new(dir.clone());
    let mut sync = CatalogSync::new(Some(dir));
    let out = sync
        .run(&targets(), &quotas(), &UnwiredFetcher, NOW)
        .unwrap();
    assert!(out.ran);
    assert!(!out.changed, "未接线 → catalog 不变 → 零动作");
    assert!(out
        .skipped
        .iter()
        .all(|s| s.reason.contains("unsupported") || s.reason.contains("未")));
    assert!(out.md_meta.is_empty());
}

#[test]
fn catalog_is_persisted_across_control_plane_restarts() {
    let dir = temp_store("catalog-persist");
    let _c = Cleanup::new(dir.clone());
    {
        let mut sync = CatalogSync::new(Some(dir.clone()));
        let fetcher = FakeFetcher::with(vec![sym("BTCUSDT", "BTC", "TRADING", 1)], vec![currency("USDT")]);
        let o = sync.run(&targets(), &quotas(), &fetcher, NOW).unwrap();
        assert!(o.changed);
    }
    let sync = CatalogSync::new(Some(dir));
    assert_eq!(sync.catalog().symbol_count(), 1, "重启后 catalog 仍在");

    // 重启后首轮同数据 → 零动作（不产生「空 → 满」的假变更风暴）
    let fetcher = FakeFetcher::with(vec![sym("BTCUSDT", "BTC", "TRADING", 1)], vec![currency("USDT")]);
    let mut sync = sync;
    let o = sync
        .run(&targets(), &quotas(), &fetcher, NOW + META_SYNC_INTERVAL_MS)
        .unwrap();
    assert!(o.ran && !o.changed, "重启后同数据 → 零动作");
}

// ── 无变更零动作的总线侧证据：不发 ConfigChanged、不发 md.meta ──────────────

#[test]
fn no_change_round_publishes_no_config_changed() {
    let dir = temp_store("no-changed-event");
    let _c = Cleanup::new(dir.clone());
    let ec = MemoryEventCenter::new();
    let bus = ec.clone();
    let mut plane = ControlPlane::open(ec, cfg(&dir)).unwrap();

    // `docs/14 §2`：同步目标来自接入台账（exchange_market），先登记
    seed_market(&plane, "binance", "swap", "FuturesCross");

    // 先做一次正常的域发布，让 ctl.<domain> 上确实有一条 ConfigChanged 可对照
    let (_doc, out) = plane
        .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1", "init")
        .unwrap();
    plane.publish_all(&out).unwrap();

    let mut watcher = bus
        .open(
            &Topic::new(ConfigDomain::SubscribePlan.topic()),
            &Group::new("cfg-watcher"),
            SessionOpts {
                timeout: Duration::from_millis(300),
                start: StartPosition::Earliest,
            },
        )
        .unwrap();
    let first = watcher
        .next(Duration::from_millis(500))
        .unwrap()
        .expect("域发布应有一条 ConfigChanged");
    match first.body {
        Body::Ctl(CtlMessage::ConfigChanged(c)) => {
            assert_eq!(c.version, 1);
            assert_eq!(c.publisher, "cp-1");
        }
        other => panic!("期望 ConfigChanged，实际 {other:?}"),
    }

    let fetcher = FakeFetcher::with(
        vec![sym("BTCUSDT", "BTC", "TRADING", 1)],
        vec![currency("USDT")],
    );
    plane.set_fetcher(std::sync::Arc::new(fetcher));

    // ① 首轮：catalog 从空到有 → 有变更，但 catalog **不是配置域**（09 §2.3 产物第 1 路）
    let o1 = plane.run_metadata_sync(NOW).unwrap();
    assert!(o1.changed);
    assert!(
        watcher.next(Duration::from_millis(300)).unwrap().is_none(),
        "catalog 变更不得发 ConfigChanged（它不是配置域）"
    );

    // ② 第二轮：无变更 → 零动作
    let o2 = plane.run_metadata_sync(NOW + META_SYNC_INTERVAL_MS).unwrap();
    assert!(o2.ran && !o2.changed);
    assert!(o2.md_meta.is_empty(), "无变更不发 md.meta");
    assert!(
        watcher.next(Duration::from_millis(300)).unwrap().is_none(),
        "无变更不发 ConfigChanged"
    );
    assert!(
        !o2.recompute_ran,
        "无变更不触发指派重算、不 bump assignment_version"
    );
}

#[test]
fn an_incomplete_round_does_not_reenter_before_the_next_period() {
    let dir = temp_store("no-reenter");
    let _c = Cleanup::new(dir.clone());

    // 全部拉取失败：这一轮「未完成」
    let fetcher = FakeFetcher::with(vec![], vec![]);
    {
        let mut st = fetcher.state.lock().unwrap();
        st.fail_symbols = true;
        st.fail_currencies = true;
    }
    let mut sync = CatalogSync::new(Some(dir));
    let o = sync.run(&targets(), &quotas(), &fetcher, NOW).unwrap();
    assert!(o.ran, "轮次确实开始了");
    assert!(!o.changed);
    assert_eq!(o.skipped.len(), 2, "币信息 + 交易对都失败");
    assert!(!sync.is_running(), "轮次已收尾");

    // 轮次时刻在**轮首**就前移 → 未完成的轮同样占掉整个 5 分钟周期，不得立刻重入
    assert!(
        !sync.due(NOW + 1),
        "一轮未完成不重入：必须等满下一个周期"
    );
    assert!(!sync.due(NOW + META_SYNC_INTERVAL_MS - 1));
    let calls_before = fetcher.symbol_calls.load(Ordering::SeqCst);
    let o2 = sync.run(&targets(), &quotas(), &fetcher, NOW + 1).unwrap();
    assert!(!o2.ran, "周期内再次调用 → 零动作");
    assert_eq!(
        fetcher.symbol_calls.load(Ordering::SeqCst),
        calls_before,
        "周期内不重复打所方端点"
    );
    assert!(sync.due(NOW + META_SYNC_INTERVAL_MS), "到点后才允许下一轮");
}

// 保证用到 SyncOutcome（编译期可见性）
#[allow(dead_code)]
fn _type_check(o: SyncOutcome) -> bool {
    o.ran && o.changed
}
