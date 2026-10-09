//! 目录七表读写（`docs/12 §3.1–§3.4` + §3.7）与 **catalog 落库**（`docs/12 §4.1`）。
//!
//! 全部离线：只开临时目录里的 SQLite，不联网、不连 Redis。
//!
//! 必测（交付要求）：
//! * 分页 / 筛选正确性（`{items,total,limit,offset}`，limit 默认 50 / 上限 500）；
//! * `row_hash` 相同 → **只更新 `last_synced_at`（无变更零动作）**；
//! * `row_hash` 不同 → 写 `instrument_change`（`before_v` / `after_v`）；
//! * 每轮同步落一条 `sync_run`。

use std::collections::BTreeMap;

use ponk_control::catalog::Catalog;
use ponk_control::{
    diff_catalog, persist_catalog_round, AssetFilter, AssetRecord, AuditFilter, CatalogDiff,
    ChangeFilter, ChangeRecord, ExchangeFilter, InstrumentFilter, InstrumentRecord, NewExchange,
    NewMarket, SqliteStore, SyncRunFilter, VenueAssetFilter, VenueAssetRecord,
};
use ponk_protocol::{ConfigDomain, CurrencyInfo, SymbolInfo};
use serde_json::{json, Value};

const T0: u64 = 1_770_000_000_000;
const T1: u64 = 1_770_000_300_000;
const T2: u64 = 1_770_000_600_000;
const T3: u64 = 1_770_000_900_000;
const T4: u64 = 1_770_001_200_000;

/// 临时库 + 保活的 `TempDir`（目录必须活到连接关闭之后，否则 WAL 文件清不掉）。
fn store() -> (SqliteStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let s = SqliteStore::open(dir.path()).expect("open");
    (s, dir)
}

fn sym(code: &str, base: &str, status: &str, pp: u8) -> SymbolInfo {
    SymbolInfo {
        symbol: code.to_string(),
        base_asset: base.to_string(),
        quote_asset: "USDT".to_string(),
        status: status.to_string(),
        price_precision: pp,
        quantity_precision: 2,
        ..Default::default()
    }
}

fn cat(symbols: Vec<SymbolInfo>) -> Catalog {
    let mut c = Catalog::default();
    let mut m: BTreeMap<String, SymbolInfo> = BTreeMap::new();
    for s in symbols {
        m.insert(s.symbol.clone(), s);
    }
    c.symbols.insert("binance|swap".to_string(), m);
    let mut cur: BTreeMap<String, CurrencyInfo> = BTreeMap::new();
    cur.insert("USDT".into(), CurrencyInfo::new("USDT"));
    c.currencies.insert("binance".to_string(), cur);
    c
}

fn outcome_json() -> Value {
    json!({ "ran": true })
}

// ── §4.6 交易所与市场 ────────────────────────────────────────────────────────

#[test]
fn exchange_and_market_crud_enforces_dig3_hard_mapping() {
    let (s, _keep) = store();

    let ex = s
        .upsert_exchange(&NewExchange {
            venue: "binance".into(),
            name: "Binance".into(),
            ..NewExchange::default()
        })
        .unwrap();
    assert_eq!(ex.name, "Binance");
    assert_eq!(ex.market_count, 0);

    // venue 主键：再次 upsert = 更新同一行
    let ex2 = s
        .upsert_exchange(&NewExchange {
            venue: "binance".into(),
            name: "Binance Spot".into(),
            ..NewExchange::default()
        })
        .unwrap();
    assert_eq!(ex2.name, "Binance Spot");
    assert_eq!(ex2.created_at, ex.created_at);
    assert_eq!(s.list_exchanges_all().unwrap().len(), 1);

    // `dig3_account_type` 必填（12 §3.1 无默认值）
    let err = s
        .upsert_market(&NewMarket {
            venue: "binance".into(),
            market: "swap".into(),
            ..NewMarket::default()
        })
        .unwrap_err();
    assert!(matches!(err, ponk_control::ControlError::Validation(_)), "{err}");

    // `swap → FuturesCross` 硬映射（02 §6：写错连不上）
    let err = s
        .upsert_market(&NewMarket {
            venue: "binance".into(),
            market: "swap".into(),
            dig3_account_type: "FuturesIsolated".into(),
            ..NewMarket::default()
        })
        .unwrap_err();
    assert!(err.to_string().contains("FuturesCross"), "{err}");

    let mk = s
        .upsert_market(&NewMarket {
            venue: "binance".into(),
            market: "swap".into(),
            dig3_account_type: "FuturesCross".into(),
            ..NewMarket::default()
        })
        .unwrap();
    assert!(mk.enabled);
    assert!((mk.quota_collect - 0.50).abs() < 1e-9);
    assert!((mk.quota_metadata - 0.15).abs() < 1e-9);
    assert_eq!(s.list_markets("binance").unwrap().len(), 1);
    assert_eq!(s.exchange("binance").unwrap().unwrap().market_count, 1);

    // 三项配额 ∈ [0,1]（docs/13 §4.6）
    let err = s
        .upsert_market(&NewMarket {
            venue: "binance".into(),
            market: "spot".into(),
            dig3_account_type: "Spot".into(),
            quota_collect: 1.5,
            ..NewMarket::default()
        })
        .unwrap_err();
    assert!(matches!(err, ponk_control::ControlError::Validation(_)), "{err}");

    // 交易所不存在 → 挂不上市场
    let err = s
        .upsert_market(&NewMarket {
            venue: "okx".into(),
            market: "spot".into(),
            dig3_account_type: "Spot".into(),
            ..NewMarket::default()
        })
        .unwrap_err();
    assert!(matches!(err, ponk_control::ControlError::Validation(_)), "{err}");

    // 启停 + 级联删除
    s.set_market_enabled("binance", "swap", false).unwrap();
    assert!(!s.market("binance", "swap").unwrap().unwrap().enabled);
    s.set_exchange_enabled("binance", false).unwrap();
    assert!(!s.require_exchange("binance").unwrap().enabled);

    assert_eq!(s.delete_exchange("binance").unwrap(), 1, "返回被删市场数");
    assert!(s.exchange("binance").unwrap().is_none());
    assert!(s.market("binance", "swap").unwrap().is_none());
    assert!(s.require_exchange("nope").is_err(), "不存在 → not_found");

    assert!(!s.delete_market("binance", "swap").unwrap(), "已随父级消失");
}

#[test]
fn exchange_filter_pages_and_filters_correctly() {
    let (s, _keep) = store();
    for v in ["binance", "okx", "bitget", "hyperliquid"] {
        s.upsert_exchange(&NewExchange {
            venue: v.into(),
            name: v.to_uppercase(),
            ..NewExchange::default()
        })
        .unwrap();
    }
    s.set_exchange_enabled("okx", false).unwrap();

    let all = ExchangeFilter::default().apply(s.list_exchanges_all().unwrap());
    assert_eq!(all.total, 4);
    assert_eq!(all.limit, 50, "§4.1 默认 50");
    assert_eq!(all.offset, 0);
    assert_eq!(all.items.len(), 4);

    let p = ExchangeFilter {
        limit: Some(2),
        offset: Some(1),
        ..ExchangeFilter::default()
    }
    .apply(s.list_exchanges_all().unwrap());
    assert_eq!((p.total, p.limit, p.offset), (4, 2, 1));
    assert_eq!(p.items.len(), 2, "offset=1 limit=2 → 第 2、3 条");
    assert_eq!(p.items[0].venue, "bitget", "venue 升序");

    let p = ExchangeFilter {
        limit: Some(10_000),
        ..ExchangeFilter::default()
    }
    .apply(s.list_exchanges_all().unwrap());
    assert_eq!(p.limit, 500, "§4.1 上限 500");

    let p = ExchangeFilter {
        offset: Some(99),
        ..ExchangeFilter::default()
    }
    .apply(s.list_exchanges_all().unwrap());
    assert!(p.items.is_empty() && p.total == 4, "越界 offset → 空页但 total 正确");

    let off = ExchangeFilter {
        enabled: Some(false),
        ..ExchangeFilter::default()
    }
    .apply(s.list_exchanges_all().unwrap());
    assert_eq!(off.total, 1);
    assert_eq!(off.items[0].venue, "okx");

    let q = ExchangeFilter {
        q: Some("BIT".into()),
        ..ExchangeFilter::default()
    }
    .apply(s.list_exchanges_all().unwrap());
    assert_eq!(q.total, 1, "q 模糊匹配 venue / name");
    assert_eq!(q.items[0].venue, "bitget");
}

// ── §4.1 落库：row_hash 零动作 / 变更写 instrument_change / sync_run 落行 ────

#[test]
fn catalog_round_persists_and_is_zero_action_when_row_hash_matches() {
    let (s, _keep) = store();
    let c1 = cat(vec![
        sym("BTCUSDT", "BTC", "TRADING", 1),
        sym("ETHUSDT", "ETH", "TRADING", 2),
    ]);

    let run1 = persist_catalog_round(
        &s,
        &c1,
        &diff_catalog(&Catalog::default(), &c1),
        T0,
        T0,
        &outcome_json(),
        None,
    )
    .unwrap();

    // ① 首轮：instrument / venue_asset / sync_run 全部落库
    let inst = s.list_instruments_all().unwrap();
    assert_eq!(inst.len(), 2, "instrument 落库");
    assert_eq!(s.list_sync_runs_all().unwrap().len(), 1, "每轮一条 sync_run");
    assert_eq!(inst[0].venue, "binance");
    assert_eq!(inst[0].market, "swap");
    assert_eq!(inst[0].base.as_deref(), Some("BTC"), "base 返回 asset.code");
    assert_eq!(inst[0].quote.as_deref(), Some("USDT"));
    assert!(!inst[0].row_hash.is_empty());

    // 父行自动补齐（12 §3.1 的外键）+ 02 §6 硬映射
    assert!(s.exchange("binance").unwrap().is_some());
    let mk = s.market("binance", "swap").unwrap().unwrap();
    assert_eq!(mk.dig3_account_type, "FuturesCross");
    assert_eq!(s.list_venue_assets_all("binance").unwrap().len(), 1, "venue_asset 落库");

    let changes = s.list_changes_all().unwrap();
    assert_eq!(changes.len(), 2, "新上标的各写一条 change");
    assert!(changes.iter().all(|c| c.change_kind == "new"));
    assert!(changes.iter().all(|c| c.before.is_none() && c.after.is_some()), "新上 before_v = NULL");
    assert!(changes.iter().all(|c| c.sync_run_id == Some(run1)), "变更挂到本轮 sync_run");

    // ② 同数据第二轮 → **row_hash 相同 ⇒ 零动作**
    let run2 = persist_catalog_round(
        &s,
        &c1,
        &diff_catalog(&c1, &c1),
        T1,
        T1,
        &outcome_json(),
        None,
    )
    .unwrap();
    assert_ne!(run1, run2, "每轮一条新的 sync_run");
    assert_eq!(s.list_sync_runs_all().unwrap().len(), 2);
    assert_eq!(s.list_changes_all().unwrap().len(), 2, "无变更零动作");
    let btc = s.instrument("binance", "swap", "BTCUSDT").unwrap().unwrap();
    assert_eq!(btc.last_synced_at, T1, "只更新 last_synced_at");
    assert_eq!(btc.first_seen_at, T0, "first_seen_at 不动");
    let va = s.venue_assets_of("USDT").unwrap();
    assert_eq!(va.len(), 1);
    assert_eq!(va[0].last_synced_at, T1, "venue_asset 同口径");

    // ③ 状态变化 → change_kind = status（before / after 都有）
    let c3 = cat(vec![
        sym("BTCUSDT", "BTC", "BREAK", 1),
        sym("ETHUSDT", "ETH", "TRADING", 2),
    ]);
    persist_catalog_round(&s, &c3, &diff_catalog(&c1, &c3), T2, T2, &outcome_json(), None).unwrap();
    let changes = s.list_changes_all().unwrap();
    assert_eq!(changes.len(), 3);
    let latest = &changes[0]; // detected_at DESC, id DESC
    assert_eq!(latest.change_kind, "status");
    assert_eq!(latest.native_code, "BTCUSDT");
    assert!(latest.before.is_some() && latest.after.is_some(), "before_v / after_v 都有值");
    assert_eq!(latest.detected_at, T2);
    assert_eq!(
        s.instrument("binance", "swap", "BTCUSDT").unwrap().unwrap().status,
        "BREAK"
    );

    // ④ 精度变化 → change_kind = precision
    let c4 = cat(vec![
        sym("BTCUSDT", "BTC", "BREAK", 4),
        sym("ETHUSDT", "ETH", "TRADING", 2),
    ]);
    persist_catalog_round(&s, &c4, &diff_catalog(&c3, &c4), T3, T3, &outcome_json(), None).unwrap();
    let changes = s.list_changes_all().unwrap();
    assert_eq!(changes.len(), 4);
    assert_eq!(changes[0].change_kind, "precision");

    // ⑤ 从目录里消失 → delisted（12 §3.3 的 delisted_at）
    let c5 = cat(vec![sym("BTCUSDT", "BTC", "BREAK", 4)]);
    persist_catalog_round(&s, &c5, &diff_catalog(&c4, &c5), T4, T4, &outcome_json(), None).unwrap();
    let changes = s.list_changes_all().unwrap();
    assert_eq!(changes.len(), 5);
    assert_eq!(changes[0].change_kind, "delisted");
    assert_eq!(changes[0].native_code, "ETHUSDT");
    let eth = s.instrument("binance", "swap", "ETHUSDT").unwrap().unwrap();
    assert!(eth.is_delisted());
    assert_eq!(eth.delisted_at, Some(T4));

    // 重复下架 → 零动作
    persist_catalog_round(&s, &c5, &diff_catalog(&c5, &c5), T4 + 1, T4 + 1, &outcome_json(), None)
        .unwrap();
    assert_eq!(s.list_changes_all().unwrap().len(), 5, "已下架不再重复写变更");
}

#[test]
fn sync_run_rows_are_paged_and_gettable() {
    let (s, _keep) = store();
    let id = persist_catalog_round(
        &s,
        &Catalog::default(),
        &CatalogDiff::default(),
        T0,
        T1,
        &json!({"ran": true}),
        Some("fetch failed: boom"),
    )
    .unwrap();

    let runs = s.list_sync_runs_all().unwrap();
    assert_eq!(runs.len(), 1, "每轮一条 sync_run");
    let r = s.sync_run(id).unwrap().unwrap();
    assert_eq!(r.started_at, T0);
    assert_eq!(r.finished_at, Some(T1));
    assert_eq!(r.error.as_deref(), Some("fetch failed: boom"));
    assert_eq!(r.outcome["ran"], json!(true));
    assert!(s.list_instruments_all().unwrap().is_empty(), "出错轮不动目录事实");

    let page = SyncRunFilter {
        limit: Some(10),
        offset: Some(0),
    }
    .apply(s.list_sync_runs_all().unwrap());
    assert_eq!((page.total, page.limit, page.offset), (1, 10, 0));
    assert!(s.sync_run(id + 99).unwrap().is_none(), "单条查询 → None");
}

#[test]
fn instrument_filters_are_faceted_and_sort_is_whitelisted() {
    let (s, _keep) = store();
    let c1 = cat(vec![
        sym("BTCUSDT", "BTC", "TRADING", 1),
        sym("ETHUSDT", "ETH", "BREAK", 2),
    ]);
    persist_catalog_round(
        &s,
        &c1,
        &diff_catalog(&Catalog::default(), &c1),
        T0,
        T0,
        &outcome_json(),
        None,
    )
    .unwrap();
    let all = s.list_instruments_all().unwrap();
    assert_eq!(all.len(), 2);

    let total = |f: InstrumentFilter| f.apply(all.clone()).total;
    assert_eq!(
        total(InstrumentFilter {
            venue: Some("binance".into()),
            ..Default::default()
        }),
        2
    );
    assert_eq!(
        total(InstrumentFilter {
            market: Some("spot".into()),
            ..Default::default()
        }),
        0,
        "market 精确筛选"
    );
    assert_eq!(
        total(InstrumentFilter {
            status: Some("break".into()),
            ..Default::default()
        }),
        1,
        "status 大小写不敏感"
    );
    assert_eq!(
        total(InstrumentFilter {
            native_code: Some("BTC".into()),
            ..Default::default()
        }),
        1,
        "native_code 模糊搜"
    );
    assert_eq!(
        total(InstrumentFilter {
            base: Some("ETH".into()),
            ..Default::default()
        }),
        1,
        "base 模糊搜（经 asset.code 关联）"
    );
    assert_eq!(
        total(InstrumentFilter {
            quote: Some("USDT".into()),
            ..Default::default()
        }),
        2
    );
    assert_eq!(
        total(InstrumentFilter {
            synced_after: Some(T0 + 1),
            ..Default::default()
        }),
        0,
        "last_synced_at 范围"
    );

    // §4.7 sort 白名单：非法字段回落默认，不 panic
    let page = InstrumentFilter {
        sort: Some("row_hash".into()),
        order: Some("desc".into()),
        ..Default::default()
    }
    .apply(all.clone());
    assert_eq!(page.total, 2);
    let page = InstrumentFilter {
        sort: Some("status".into()),
        order: Some("desc".into()),
        ..Default::default()
    }
    .apply(all);
    assert_eq!(page.items[0].status, "TRADING", "status desc");

    // 导出走 select（忽略分页）
    let f = InstrumentFilter {
        limit: Some(1),
        ..Default::default()
    };
    assert_eq!(f.select(s.list_instruments_all().unwrap()).len(), 2, "导出忽略分页");
    assert_eq!(f.apply(s.list_instruments_all().unwrap()).items.len(), 1);
}

#[test]
fn change_rows_are_time_desc_and_filterable() {
    let (s, _keep) = store();
    let c1 = cat(vec![sym("BTCUSDT", "BTC", "TRADING", 1)]);
    persist_catalog_round(
        &s,
        &c1,
        &diff_catalog(&Catalog::default(), &c1),
        T0,
        T0,
        &outcome_json(),
        None,
    )
    .unwrap();
    let c2 = cat(vec![sym("BTCUSDT", "BTC", "BREAK", 1)]);
    persist_catalog_round(&s, &c2, &diff_catalog(&c1, &c2), T1, T1, &outcome_json(), None).unwrap();

    let all: Vec<ChangeRecord> = s.list_changes_all().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].change_kind, "status", "detected_at DESC");
    assert_eq!(all[0].venue, "binance");

    let total = |f: ChangeFilter| f.apply(all.clone()).total;
    assert_eq!(total(ChangeFilter { change_kind: Some("new".into()), ..Default::default() }), 1);
    assert_eq!(total(ChangeFilter { since: Some(T1), until: Some(T1), ..Default::default() }), 1);
    assert_eq!(total(ChangeFilter { native_code: Some("BTCUSDT".into()), ..Default::default() }), 2);
    assert_eq!(total(ChangeFilter { venue: Some("okx".into()), ..Default::default() }), 0);
    let run_id = all[0].sync_run_id.unwrap();
    assert_eq!(
        total(ChangeFilter { sync_run_id: Some(run_id), ..Default::default() }),
        1,
        "按轮次跳转（P4b → P8）"
    );

    let page = ChangeFilter {
        limit: Some(1),
        offset: Some(1),
        ..ChangeFilter::default()
    }
    .apply(all);
    assert_eq!((page.total, page.items.len(), page.offset), (2, 1, 1));
    assert!(s.change(99_999).unwrap().is_none(), "单条变更查询");
}

#[test]
fn global_audit_query_is_cross_domain_and_filterable() {
    use ponk_control::ConfigStore;

    let (s, _keep) = store();
    // publish / rollback 会写 audit_log（action 两种，target = 域）
    s.publish(ConfigDomain::SignalRules, json!({"rules": []}), "cp-1/alice", "init").unwrap();
    s.publish(ConfigDomain::AlertRules, json!({"rules": []}), "cp-2/bob", "init").unwrap();
    s.publish(ConfigDomain::SignalRules, json!({"rules": [1]}), "cp-1/alice", "edit").unwrap();
    s.rollback(ConfigDomain::SignalRules, 1, "cp-1/alice").unwrap();

    let rows = s.list_audit_all().unwrap();
    assert_eq!(rows.len(), 4, "跨域全表（不止单域 publish/rollback 过滤）");
    assert!(rows[0].ts >= rows[3].ts, "ts DESC（idx_audit_ts）");
    assert_eq!(rows[0].action, "rollback");

    let total = |f: AuditFilter| f.apply(rows.clone()).total;
    assert_eq!(total(AuditFilter { action: Some("publish".into()), ..Default::default() }), 3);
    assert_eq!(total(AuditFilter { target: Some("alert_rules".into()), ..Default::default() }), 1);
    assert_eq!(total(AuditFilter { actor: Some("ALICE".into()), ..Default::default() }), 3, "actor 大小写不敏感");
    assert_eq!(total(AuditFilter { actor: Some("cp-2".into()), ..Default::default() }), 1);
    assert_eq!(total(AuditFilter { q: Some("alert".into()), ..Default::default() }), 1, "q 覆盖 detail");

    // 时间区间（边界含等值；发布可能落在同一毫秒，故与参考实现比对而非写死条数）
    let mid = rows[1].ts;
    let f = AuditFilter {
        until: Some(mid),
        ..AuditFilter::default()
    };
    let n = f.apply(rows.clone()).total;
    let expect = rows.iter().filter(|r| r.ts <= mid).count();
    assert_eq!(n, expect, "until 为闭区间上界");
    let f = AuditFilter {
        since: Some(mid),
        ..AuditFilter::default()
    };
    let n = f.apply(rows.clone()).total;
    let expect = rows.iter().filter(|r| r.ts >= mid).count();
    assert_eq!(n, expect, "since 为闭区间下界");

    // 排序白名单 + 分页
    let page = AuditFilter {
        sort: Some("action".into()),
        order: Some("asc".into()),
        limit: Some(2),
        ..AuditFilter::default()
    }
    .apply(rows.clone());
    assert_eq!(page.limit, 2);
    assert_eq!(page.total, 4);
    assert_eq!(page.items.len(), 2);

    let page = AuditFilter {
        sort: Some("bogus".into()),
        limit: Some(5000),
        offset: Some(10),
        ..AuditFilter::default()
    }
    .apply(rows.clone());
    assert_eq!(page.limit, 500, "limit 上限 500");
    assert!(page.items.is_empty() && page.total == 4);

    let id = rows[0].id;
    assert!(s.audit_row(id).unwrap().is_some());
    assert!(s.audit_row(99_999).unwrap().is_none());
}

// ── 资产两层（docs/13 §4.8）─────────────────────────────────────────────────

#[test]
fn asset_and_venue_asset_layers_filter_by_kind_and_venue() {
    let (s, _keep) = store();
    // `venue_asset.venue` 外键指向 `exchange`（12 §3.2）——先建所
    s.upsert_exchange(&NewExchange {
        venue: "binance".into(),
        name: "Binance".into(),
        ..NewExchange::default()
    })
    .unwrap();

    let btc = s.ensure_asset("BTC", Some("Bitcoin"), "coin").unwrap();
    assert_eq!(s.ensure_asset("BTC", None, "coin").unwrap(), btc, "同 code 同行");
    assert_eq!(
        s.asset("BTC").unwrap().unwrap().name.as_deref(),
        Some("Bitcoin"),
        "空 name 不覆盖全局层"
    );
    let usdt = s.ensure_asset("USDT", None, "coin").unwrap();

    let mut va = VenueAssetRecord {
        asset_id: usdt,
        code: "USDT".into(),
        venue: "binance".into(),
        precision: Some(8),
        networks: vec!["TRC20".into(), "ERC20".into()],
        deposit_enabled: Some(true),
        withdraw_enabled: Some(true),
        status: Some("live".into()),
        extra: json!({}),
        row_hash: String::new(),
        last_synced_at: T0,
    };
    assert!(s.upsert_venue_asset(&mut va).unwrap().is_new);

    // row_hash 相同 → 只推进 last_synced_at
    va.last_synced_at = T1;
    let o = s.upsert_venue_asset(&mut va).unwrap();
    assert!(!o.changed, "无变更零动作");
    assert_eq!(s.venue_assets_of("USDT").unwrap()[0].last_synced_at, T1);

    // row_hash 不同 → 覆盖
    va.precision = Some(2);
    va.last_synced_at = T2;
    assert!(s.upsert_venue_asset(&mut va).unwrap().changed);
    let got = s.venue_assets_of("USDT").unwrap();
    assert_eq!(got[0].precision, Some(2));
    assert_eq!(got[0].networks, vec!["TRC20".to_string(), "ERC20".to_string()]);
    assert_eq!(got[0].venue, "binance");
    assert_eq!(got[0].code, "USDT");
    assert_eq!(s.asset("USDT").unwrap().unwrap().venue_count, 1, "各所覆盖数");

    let assets: Vec<AssetRecord> = s.list_assets_all().unwrap();
    assert_eq!(assets.len(), 2);
    let total = |f: AssetFilter| f.apply(assets.clone(), &s.list_venue_assets().unwrap()).total;
    assert_eq!(total(AssetFilter { kind: Some("token".into()), ..Default::default() }), 0);
    assert_eq!(total(AssetFilter { kind: Some("COIN".into()), ..Default::default() }), 2, "kind 大小写不敏感");
    assert_eq!(total(AssetFilter { q: Some("bit".into()), ..Default::default() }), 1, "q 匹配 code / name");
    assert_eq!(total(AssetFilter { venue: Some("okx".into()), ..Default::default() }), 0, "按所过滤");
    assert_eq!(total(AssetFilter { venue: Some("binance".into()), ..Default::default() }), 1);
    assert_eq!(
        total(AssetFilter {
            deposit_enabled: Some(true),
            ..Default::default()
        }),
        1
    );
    assert!(s.asset("NOPE").unwrap().is_none());
    assert!(s.asset_by_id(99_999).unwrap().is_none());

    let va_page = VenueAssetFilter {
        q: Some("usdt".into()),
        limit: Some(5),
        offset: Some(0),
    }
    .apply(s.list_venue_assets_all("binance").unwrap());
    assert_eq!((va_page.total, va_page.limit), (1, 5));
    assert!(s.venue_assets_of("NOPE").unwrap().is_empty());
}

// ── row_hash 规范化（docs/12 §4.2）─────────────────────────────────────────

#[test]
fn row_hash_ignores_timestamps_and_object_key_order() {
    let mut a = sym("BTCUSDT", "BTC", "TRADING", 1);
    a.extra = json!({"a": 1, "b": {"c": 2}});
    let mut b = sym("BTCUSDT", "BTC", "TRADING", 1);
    b.extra = json!({"b": {"c": 2}, "a": 1}); // 键序不同，内容相同

    let mk = |s: &SymbolInfo, at: u64| InstrumentRecord {
        venue: "binance".into(),
        market: "swap".into(),
        native_code: s.symbol.clone(),
        base_asset_id: Some(1),
        quote_asset_id: Some(2),
        base: Some(s.base_asset.clone()),
        quote: Some(s.quote_asset.clone()),
        status: s.status.clone(),
        instrument_type: s.instrument_type.clone(),
        price_precision: Some(s.price_precision as i64),
        qty_precision: Some(s.quantity_precision as i64),
        tick_size: s.tick_size.map(|f| serde_json::json!(f).to_string()),
        step_size: s.step_size.map(|f| serde_json::json!(f).to_string()),
        min_notional: s.min_notional.map(|f| serde_json::json!(f).to_string()),
        listed_at: None,
        delisted_at: None,
        first_seen_at: at,
        last_synced_at: at,
        row_hash: String::new(),
        extra: s.extra.clone(),
    };

    let (ra, rb) = (mk(&a, T0), mk(&b, T9));
    assert_eq!(
        ra.compute_row_hash(),
        rb.compute_row_hash(),
        "12 §4.2：不含时间戳、键序无关 ⇒ 内容相同则哈希相同"
    );
    let rc = mk(&sym("BTCUSDT", "BTC", "BREAK", 1), T0);
    assert_ne!(ra.compute_row_hash(), rc.compute_row_hash(), "status 变了 → 哈希变");

    let mut rd = ra.clone();
    rd.price_precision = Some(4);
    assert_ne!(ra.compute_row_hash(), rd.compute_row_hash(), "精度变了 → 哈希变");

    let mut re = ra.clone();
    re.last_synced_at = T9;
    re.first_seen_at = T9;
    assert_eq!(
        ra.compute_row_hash(),
        re.compute_row_hash(),
        "时间戳不参与哈希（否则「无变更零动作」永远做不到）"
    );
}

const T9: u64 = 1_770_099_999_999;
