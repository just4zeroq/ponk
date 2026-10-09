//! `docs/14-目录类页面与币信息.md` §3 / §5 外部（币）同步 —— **全离线**。
//!
//! `OffchainProvider` 用注入的假实现覆盖交付要求的全部分支：
//!
//! * 精确唯一命中 → 自动 `external_id` + `matched`
//! * 多命中 → `ambiguous`（**不自动选**）
//! * 零命中（含缩放前缀 `1000PEPE`）→ `not_found`（**不做模糊匹配与剥前缀**）
//! * 限速 → `rate_limited` 记进 `offchain_sync_run`，**不碰 `sync_run`**
//! * 详情轮转只拉未同步的（`coin_detail_batch`）
//! * `exchange.asset_class = 'stock'` → 该所标的不进外部同步目标
//! * `asset_contract` upsert 幂等
//! * 手改 `kind` / `external_id` 不参与 `instrument.row_hash`
//!
//! 无外网、无 Redis：只开临时目录里的 SQLite。

use std::collections::HashMap;
use std::sync::Mutex;

use ponk_control::catalog::Catalog;
use ponk_control::{
    persist_catalog_round, AssetPatch, CatalogDiff, CoinDetail, ControlError, MarketEntry,
    NewContract, NewExchange, NewMarket, OffchainConfig, OffchainProvider, OffchainSync,
    ProviderError, SqliteStore,
};
use ponk_protocol::SymbolInfo;

// ── 假外部源 ────────────────────────────────────────────────────────────────

/// 注入的假外部源：可逐用例配置市场索引、逐 id 配置详情与错误。
#[derive(Default)]
struct FakeProvider {
    markets: Vec<MarketEntry>,
    /// `coin id` → 详情（或该 id 的错误）。
    details: HashMap<String, Result<CoinDetail, ProviderError>>,
    /// 整轮市场索引的错误（限速 / 停服）。
    market_error: Option<ProviderError>,
    /// 详情拉取日志（按调用顺序）——轮转测试据此断言"只拉了谁"。
    detail_log: Mutex<Vec<String>>,
}

impl FakeProvider {
    fn new(markets: Vec<MarketEntry>) -> Self {
        FakeProvider {
            markets,
            ..FakeProvider::default()
        }
    }

    fn detail_calls(&self) -> Vec<String> {
        self.detail_log.lock().expect("detail_log").clone()
    }
}

impl OffchainProvider for FakeProvider {
    fn source(&self) -> &'static str {
        "coingecko"
    }

    fn fetch_markets_page(&self, page: u32, per_page: u32) -> Result<Vec<MarketEntry>, ProviderError> {
        if let Some(e) = &self.market_error {
            return Err(e.clone());
        }
        let start = ((page - 1) * per_page) as usize;
        Ok(self
            .markets
            .iter()
            .skip(start)
            .take(per_page as usize)
            .cloned()
            .collect())
    }

    fn fetch_coin_detail(&self, id: &str) -> Result<CoinDetail, ProviderError> {
        self.detail_log.lock().expect("detail_log").push(id.to_string());
        match self.details.get(id) {
            Some(r) => r.clone(),
            None => Err(ProviderError::Fetch(format!("假源没有 `{id}` 的详情"))),
        }
    }
}

fn market(id: &str, symbol: &str) -> MarketEntry {
    MarketEntry {
        id: id.to_string(),
        symbol: symbol.to_string(),
        name: Some(format!("{symbol} name")),
        market_cap_rank: Some(1),
        total_supply: Some("21000000".to_string()),
        circulating_supply: Some("19000000.5".to_string()),
        raw: serde_json::json!({ "id": id, "symbol": symbol }),
    }
}

fn detail(id: &str, platforms: &[(&str, &str)]) -> CoinDetail {
    let mut p = std::collections::BTreeMap::new();
    for (chain, addr) in platforms {
        p.insert(chain.to_string(), addr.to_string());
    }
    CoinDetail {
        id: id.to_string(),
        platforms: p,
        twitter_screen_name: Some("satoshi".to_string()),
        raw: serde_json::json!({ "id": id }),
    }
}

/// 临时库 + 保活的 `TempDir`（目录必须活到连接关闭之后，否则 WAL 文件清不掉）。
fn store() -> (SqliteStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let s = SqliteStore::open(dir.path()).expect("open");
    (s, dir)
}

/// 建一个所 + 一个市场（接入台账 = 同步目标来源，`docs/14 §2`）。
fn seed_venue(s: &SqliteStore, venue: &str, market_name: &str, asset_class: &str) {
    s.upsert_exchange(&NewExchange {
        venue: venue.to_string(),
        name: venue.to_uppercase(),
        asset_class: asset_class.to_string(),
        ..NewExchange::default()
    })
    .expect("登记交易所");
    let account = if market_name == "swap" { "FuturesCross" } else { "Spot" };
    s.upsert_market(&NewMarket {
        venue: venue.to_string(),
        market: market_name.to_string(),
        dig3_account_type: account.to_string(),
        ..NewMarket::default()
    })
    .expect("登记市场");
}

/// 详情关闭的一轮配置（只测匹配 / 轻量层时用）。
fn light_only_cfg() -> OffchainConfig {
    OffchainConfig {
        coin_detail_batch: 0,
        ..OffchainConfig::default()
    }
}

fn asset_of(s: &SqliteStore, code: &str) -> ponk_control::AssetRecord {
    s.asset(code).expect("read asset").expect("asset 存在")
}

// ── 1. 匹配三级 ─────────────────────────────────────────────────────────────

/// 精确唯一命中 → 自动 `external_id` + `matched`，并写入轻量字段。
#[test]
fn unique_symbol_match_is_automatic_and_writes_light_fields() {
    let (s, _keep) = store();
    s.ensure_asset("BTC", None, "coin").unwrap();
    s.ensure_asset("ETH", None, "coin").unwrap();

    let provider = FakeProvider::new(vec![market("bitcoin", "BTC"), market("ethereum", "ETH")]);
    let cfg = light_only_cfg();
    let mut sync = OffchainSync::new();
    let out = sync.run_forced(&s, &provider, &cfg, 1_770_000_000_000);

    assert!(out.ran);
    assert_eq!((out.matched, out.ambiguous, out.not_found), (2, 0, 0));
    assert_eq!(out.light_updated, 2, "两个币都写了轻量字段");

    let btc = asset_of(&s, "BTC");
    assert_eq!(btc.external_id.as_deref(), Some("bitcoin"));
    assert_eq!(btc.match_status, "matched");
    assert_eq!(btc.total_supply.as_deref(), Some("21000000"));
    assert_eq!(btc.circulating_supply.as_deref(), Some("19000000.5"));
    assert_eq!(btc.detail_source.as_deref(), Some("coingecko"));
    assert_eq!(btc.detail_synced_at, None, "batch=0 → 本轮不拉详情");
    assert!(
        btc.extra.get("light").is_some(),
        "CG 原始条目进 extra.light（12 §3.2 / 14 §4 判断 2）：{}",
        btc.extra
    );
    assert_eq!(asset_of(&s, "ETH").external_id.as_deref(), Some("ethereum"));
}

/// 多命中 → `ambiguous`，**不自动选**第一个。
#[test]
fn multi_hit_symbol_is_ambiguous_and_never_auto_picks() {
    let (s, _keep) = store();
    s.ensure_asset("FOO", None, "coin").unwrap();

    let provider = FakeProvider::new(vec![market("foo-project", "FOO"), market("bar-clone", "FOO")]);
    let mut sync = OffchainSync::new();
    let out = sync.run_forced(&s, &provider, &light_only_cfg(), 1_770_000_000_000);

    assert_eq!((out.matched, out.ambiguous, out.not_found), (0, 1, 0));
    let foo = asset_of(&s, "FOO");
    assert_eq!(foo.match_status, "ambiguous", "{}", foo.match_status);
    assert_eq!(foo.external_id, None, "多命中不许自动选");
}

/// 零命中（**含缩放前缀 `1000PEPE`**）→ `not_found`；
/// **不做模糊匹配、不做剥前缀归一化**（`docs/14 §5.2`）。
#[test]
fn zero_hit_and_scaled_prefix_are_not_found_without_fuzzy_matching() {
    let (s, _keep) = store();
    s.ensure_asset("1000PEPE", None, "coin").unwrap();
    s.ensure_asset("ZZZZ", None, "coin").unwrap();

    // 索引里只有裸 `PEPE` —— 归一化诱惑的典型场景
    let provider = FakeProvider::new(vec![market("pepe", "PEPE")]);
    let mut sync = OffchainSync::new();
    let out = sync.run_forced(&s, &provider, &light_only_cfg(), 1_770_000_000_000);

    assert_eq!(out.not_found, 2, "{out:#?}");
    for code in ["1000PEPE", "ZZZZ"] {
        let a = asset_of(&s, code);
        assert_eq!(a.match_status, "not_found", "{code}");
        assert_eq!(a.external_id, None, "{code} 不许错配到 pepe");
    }
}

/// `1INCH` 这类"看起来像有前缀"的代码必须照常精确命中（反向用例）。
#[test]
fn exact_match_with_digits_in_symbol_still_works() {
    let (s, _keep) = store();
    s.ensure_asset("1INCH", None, "coin").unwrap();

    let provider = FakeProvider::new(vec![market("inch", "1INCH")]);
    let mut sync = OffchainSync::new();
    let out = sync.run_forced(&s, &provider, &light_only_cfg(), 1_770_000_000_000);
    assert_eq!(out.matched, 1);
    assert_eq!(asset_of(&s, "1INCH").external_id.as_deref(), Some("inch"));
}

// ── 2. 限速与失败隔离 ───────────────────────────────────────────────────────

/// 限速 → `rate_limited` 记进 `offchain_sync_run`，**不写 `sync_run`**（14 §3 失败隔离）。
#[test]
fn rate_limited_is_recorded_offchain_and_never_touches_sync_run() {
    let (s, _keep) = store();
    s.ensure_asset("BTC", None, "coin").unwrap();

    let mut provider = FakeProvider::new(vec![market("bitcoin", "BTC")]);
    provider.market_error = Some(ProviderError::RateLimited("429 too many requests".into()));

    let mut sync = OffchainSync::new();
    let out = sync.run_forced(&s, &provider, &light_only_cfg(), 1_770_000_000_000);

    assert!(out.ran);
    assert!(out.rate_limited, "限速必须被识别：{out:#?}");
    assert!(out.error.as_deref().unwrap_or_default().contains("429"));

    // 独立表落行：outcome.rate_limited 是**计数**（接口契约，14 §9）
    let last = s
        .last_offchain_sync_run()
        .unwrap()
        .expect("每轮一条 offchain_sync_run");
    assert_eq!(
        last.outcome.get("rate_limited").and_then(|v| v.as_i64()),
        Some(1)
    );
    assert!(last.error.as_ref().unwrap().contains("429"), "{}", last.error.clone().unwrap_or_default());
    assert!(last.finished_at.is_some());

    // 关键：CG 限速**不得**伪装成交易所同步失败
    assert!(
        s.list_sync_runs_all().unwrap().is_empty(),
        "外部源限速不许写 sync_run"
    );
    // 也没写任何外部源字段
    assert_eq!(asset_of(&s, "BTC").match_status, "not_found");
}

/// 上游整体不可用 → 记错误、字段不动、轮次照常落行。
#[test]
fn provider_failure_is_folded_into_the_round_not_thrown() {
    let (s, _keep) = store();
    s.ensure_asset("BTC", None, "coin").unwrap();

    let mut provider = FakeProvider::new(vec![market("bitcoin", "BTC")]);
    provider.market_error = Some(ProviderError::Fetch("connection refused".into()));

    let mut sync = OffchainSync::new();
    let out = sync.run_forced(&s, &provider, &light_only_cfg(), 1_770_000_000_000);
    assert!(out.ran);
    assert!(!out.rate_limited);
    assert!(out.error.as_deref().unwrap_or_default().contains("connection refused"));
    assert!(s.last_offchain_sync_run().unwrap().is_some());
    assert_eq!(asset_of(&s, "BTC").external_id, None);
}

// ── 3. 详情层：首次 + 轮转 ──────────────────────────────────────────────────

/// 详情轮转**只拉未同步的**，且每轮不超过 `coin_detail_batch`。
#[test]
fn detail_rotation_pulls_only_unsynced_within_the_batch() {
    let (s, _keep) = store();
    let codes = ["AAA", "BBB", "CCC", "DDD", "EEE"];
    for c in codes {
        let id = c.to_lowercase();
        s.ensure_asset(c, None, "coin").unwrap();
        let asset_id = s.asset_id(c).unwrap().unwrap();
        s.set_asset_match(asset_id, Some(&id), "matched").unwrap();
    }
    // 第六个：未匹配 → 永远不该出现在详情日志里
    s.ensure_asset("ZZZ", None, "coin").unwrap();
    let zzz = s.asset_id("ZZZ").unwrap().unwrap();
    s.set_asset_match(zzz, None, "not_found").unwrap();

    let mut provider = FakeProvider::new(Vec::new());
    for c in codes {
        let id = c.to_lowercase();
        provider
            .details
            .insert(id.clone(), Ok(detail(&id, &[("Ethereum", "0xabc")])));
    }

    let cfg = OffchainConfig {
        coin_detail_batch: 2,
        ..OffchainConfig::default()
    };
    let mut sync = OffchainSync::new();

    let o1 = sync.run_forced(&s, &provider, &cfg, 1_770_000_000_000);
    assert_eq!(o1.detail_pulled, 2, "{o1:#?}");
    assert_eq!(
        provider.detail_calls(),
        vec!["aaa".to_string(), "bbb".to_string()],
        "首轮只拉批次内的两个（按 code 升序）"
    );
    assert_eq!(o1.contracts, 2, "每币一个平台 → 一条合约地址");

    let o2 = sync.run_forced(&s, &provider, &cfg, 1_770_000_000_000 + 3_600_000);
    assert_eq!(o2.detail_pulled, 2, "{o2:#?}");
    assert_eq!(
        provider.detail_calls(),
        vec![
            "aaa".to_string(),
            "bbb".to_string(),
            "ccc".to_string(),
            "ddd".to_string()
        ],
        "第二批只拉**尚未同步**的 ccc/ddd，不回头重拉 aaa/bbb"
    );

    // 第三轮：最后一个从未同步的 `eee` 排最前（COALESCE(detail_synced_at,0) = 0），
    // 其后开始轮转到最旧的 `aaa`
    let o3 = sync.run_forced(&s, &provider, &cfg, 1_770_000_000_000 + 7_200_000);
    assert_eq!(o3.detail_pulled, 2, "{o3:#?}");
    assert_eq!(provider.detail_calls()[4..], ["eee".to_string(), "aaa".to_string()]);
    assert!(!provider.detail_calls().contains(&"zzz".to_string()), "未匹配的币永不进详情队列");

    // 详情字段确实落到了行上
    let aaa = asset_of(&s, "AAA");
    assert_eq!(aaa.twitter_url.as_deref(), Some("https://twitter.com/satoshi"));
    assert!(aaa.detail_synced_at.is_some());
    assert!(aaa.extra.get("detail").is_some(), "{}", aaa.extra);

    // 五个都已同步 → 之后每轮仍只打 `coin_detail_batch` 次（轮转继续，不全量重拉）
    let before = provider.detail_calls().len();
    let o4 = sync.run_forced(&s, &provider, &cfg, 1_770_000_000_000 + 10_800_000);
    assert_eq!(o4.detail_pulled, 2, "每轮上限 = coin_detail_batch：{o4:#?}");
    assert_eq!(provider.detail_calls().len(), before + 2);
}

/// 详情拉取失败**不中断整轮**，且该币退到队尾（避免头阻塞）。
#[test]
fn detail_failure_does_not_block_the_rest_of_the_batch() {
    let (s, _keep) = store();
    for c in ["AAA", "BBB"] {
        s.ensure_asset(c, None, "coin").unwrap();
        let id = s.asset_id(c).unwrap().unwrap();
        s.set_asset_match(id, Some(&c.to_lowercase()), "matched").unwrap();
    }
    let mut provider = FakeProvider::new(Vec::new());
    // `aaa` 永久失败
    provider.details.insert(
        "aaa".into(),
        Err(ProviderError::Fetch("500 upstream".into())),
    );
    provider
        .details
        .insert("bbb".into(), Ok(detail("bbb", &[])));

    let cfg = OffchainConfig {
        coin_detail_batch: 2,
        coin_detail_max_retries: 0,
        ..OffchainConfig::default()
    };
    let mut sync = OffchainSync::new();
    let out = sync.run_forced(&s, &provider, &cfg, 1_770_000_000_000);

    assert_eq!(out.detail_pulled, 1, "{out:#?}");
    assert_eq!(out.detail_failed, 1);
    let err = out.error.as_ref().expect("有失败就得有原因");
    assert!(err.contains("AAA"), "错误要带上出问题的币：{err}");
    assert!(err.contains("500 upstream"), "并带上上游原因：{err}");
    // 两个都推进了 detail_synced_at → 下轮轮转会往前走
    assert!(asset_of(&s, "AAA").detail_synced_at.is_some());
    assert!(asset_of(&s, "BBB").detail_synced_at.is_some());
}

// ── 4. 股票类跳过（14 §6）───────────────────────────────────────────────────

/// `exchange.asset_class = 'stock'` → 该所交易对**不进外部同步目标**（只同步交易对信息）。
#[test]
fn stock_class_exchange_assets_are_excluded_from_offchain_targets() {
    let (s, _keep) = store();
    seed_venue(&s, "binance", "spot", "crypto");
    seed_venue(&s, "nasdaq", "spot", "stock");

    // 交易所同步的副产物：落两条交易对 → 派生出币
    let mut cat = Catalog::default();
    let mut spot = std::collections::BTreeMap::new();
    spot.insert(
        "BTCUSDT".to_string(),
        SymbolInfo {
            symbol: "BTCUSDT".into(),
            base_asset: "BTC".into(),
            quote_asset: "USDT".into(),
            status: "trading".into(),
            ..SymbolInfo::default()
        },
    );
    cat.symbols.insert("binance|spot".into(), spot);
    let mut nasdaq = std::collections::BTreeMap::new();
    nasdaq.insert(
        "AAPLUSD".to_string(),
        SymbolInfo {
            symbol: "AAPLUSD".into(),
            base_asset: "AAPL".into(),
            quote_asset: "USD".into(),
            status: "trading".into(),
            ..SymbolInfo::default()
        },
    );
    cat.symbols.insert("nasdaq|spot".into(), nasdaq);

    persist_catalog_round(&s, &cat, &CatalogDiff::default(), 1, 2, &serde_json::json!({}), None)
        .expect("落库");

    // 另有一个按页面兜底标成 stock 的币（14 §4 判断 1）
    s.ensure_asset("TSLA", None, "stock").unwrap();

    let targets = s.offchain_targets().unwrap();
    let codes: Vec<&str> = targets.iter().map(|t| t.code.as_str()).collect();
    assert!(codes.contains(&"BTC") && codes.contains(&"USDT"), "{codes:?}");
    assert!(!codes.contains(&"AAPL"), "stock 类交易所的标的要跳过：{codes:?}");
    assert!(!codes.contains(&"USD"), "同上（USD 只在 stock 所出现）：{codes:?}");
    assert!(!codes.contains(&"TSLA"), "`kind=stock` 的页面兜底也跳过：{codes:?}");

    // 端到端：跑一轮，stock 派生的币**不会**被写上外部源字段
    let provider = FakeProvider::new(vec![
        market("bitcoin", "BTC"),
        market("apple", "AAPL"),
        market("tesla", "TSLA"),
        market("usd", "USD"),
    ]);
    let mut sync = OffchainSync::new();
    let out = sync.run_forced(&s, &provider, &light_only_cfg(), 1_770_000_000_000);
    assert!(out.excluded >= 3, "{out:#?}");
    assert_eq!(asset_of(&s, "BTC").match_status, "matched");
    assert_eq!(asset_of(&s, "AAPL").match_status, "not_found", "stock 派生币不参与");
    assert_eq!(asset_of(&s, "TSLA").match_status, "not_found");
}

/// `enabled = 0` 的所/市场不进交易所同步目标（`docs/14 §2` 省配额与存储）。
#[test]
fn disabled_exchange_or_market_leaves_the_sync_target_list() {
    let (s, _keep) = store();
    seed_venue(&s, "binance", "spot", "crypto");
    seed_venue(&s, "okx", "swap", "crypto");
    assert_eq!(
        s.enabled_market_targets().unwrap(),
        vec![
            ("binance".to_string(), "spot".to_string()),
            ("okx".to_string(), "swap".to_string())
        ]
    );

    s.set_market_enabled("binance", "spot", false).unwrap();
    s.set_exchange_enabled("okx", false).unwrap();
    assert!(
        s.enabled_market_targets().unwrap().is_empty(),
        "启停两侧都要生效"
    );
}

// ── 5. asset_contract upsert 幂等 ───────────────────────────────────────────

/// 同一份平台集跑两次 → 同样的行（`UNIQUE(asset_id, chain, address)`）；
/// 平台集变了 → 只留新集。
#[test]
fn asset_contract_upsert_is_idempotent_and_cleans_stale_rows() {
    let (s, _keep) = store();
    let id = s.ensure_asset("USDT", None, "coin").unwrap();
    let rows = vec![
        NewContract {
            asset_id: id,
            chain: "TRON".into(),
            address: "TR7NHq".into(),
            standard: None,
            is_primary: false,
            source: "coingecko".into(),
            updated_at: 10,
        },
        NewContract {
            asset_id: id,
            chain: "Ethereum".into(),
            address: "0xdac17f".into(),
            standard: None,
            is_primary: false,
            source: "coingecko".into(),
            updated_at: 10,
        },
    ];

    let n1 = s.sync_asset_contracts(id, &rows).unwrap();
    let first: Vec<i64> = s.asset_contracts("USDT").unwrap().iter().map(|c| c.id).collect();
    let n2 = s.sync_asset_contracts(id, &rows).unwrap();
    let second = s.asset_contracts("USDT").unwrap();

    assert_eq!((n1, n2), (2, 2));
    assert_eq!(second.len(), 2, "幂等：不能变成 4 行");
    assert_eq!(
        second.iter().map(|c| c.id).collect::<Vec<_>>(),
        first,
        "幂等：行 id 也不变（upsert 而非 delete+insert）"
    );
    assert_eq!(
        second
            .iter()
            .map(|c| (c.chain.clone(), c.address.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("Ethereum".to_string(), "0xdac17f".to_string()),
            ("TRON".to_string(), "TR7NHq".to_string())
        ],
        "主链位在前，其余按 chain 升序"
    );
    // 主链位恰好一个
    assert_eq!(
        s.asset_contracts("USDT").unwrap().iter().filter(|c| c.is_primary == 1).count(),
        1
    );

    // 平台集变化 → 清掉消失的行
    let one = vec![NewContract {
        asset_id: id,
        chain: "TRON".into(),
        address: "TR7NHq".into(),
        standard: None,
        is_primary: false,
        source: "coingecko".into(),
        updated_at: 20,
    }];
    s.sync_asset_contracts(id, &one).unwrap();
    let left = s.asset_contracts("USDT").unwrap();
    assert_eq!(left.len(), 1, "{}", left.len());
    assert_eq!(left[0].chain, "TRON");

    // 空平台集 → **整体跳过**（无法区分"确实没链"与"上游部分响应"，保留旧数据）
    s.sync_asset_contracts(id, &[]).unwrap();
    assert_eq!(s.asset_contracts("USDT").unwrap().len(), 1);
}

// ── 6. offchain_sync_run 落行（12 §3.4b，与 sync_run 分离）──────────────────

/// 每轮一条 `offchain_sync_run`；`outcome` 与 `error` 都写进去。
#[test]
fn every_round_records_one_offchain_sync_run() {
    let (s, _keep) = store();
    s.ensure_asset("BTC", None, "coin").unwrap();
    let provider = FakeProvider::new(vec![market("bitcoin", "BTC")]);
    let mut sync = OffchainSync::new();

    let o1 = sync.run_forced(&s, &provider, &light_only_cfg(), 1_770_000_000_000);
    let id1 = o1.offchain_run_id.expect("落行");
    let o2 = sync.run_forced(&s, &provider, &light_only_cfg(), 1_770_000_000_000 + 3_600_000);
    let id2 = o2.offchain_run_id.expect("落行");
    assert!(id2 > id1, "id 单调递增");

    let runs = s.list_offchain_sync_runs_all().unwrap();
    assert_eq!(runs.len(), 2, "每轮一条");
    assert_eq!(runs[0].id, id2, "新在前");
    let last = s.last_offchain_sync_run().unwrap().unwrap();
    assert_eq!(last.outcome.get("matched").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(last.outcome.get("fetched").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(last.error, None);
    assert_eq!(s.offchain_sync_run(id1).unwrap().unwrap().id, id1);

    // 节拍独立：未到 1 小时 → 零动作且**不落行**
    let o3 = sync.run(&s, &provider, &light_only_cfg(), 1_770_000_000_000 + 60_000);
    assert!(!o3.ran, "1 小时节拍未到：{o3:#?}");
    assert_eq!(s.list_offchain_sync_runs_all().unwrap().len(), 2);
    assert_eq!(s.list_sync_runs_all().unwrap().len(), 0, "两条链路完全分离");
}

// ── 7. 手改 kind / external_id（14 §9 + §4 判断 3）─────────────────────────

/// 页面改 `kind` / 手填 `external_id`：
/// * 手填后服务端置 `matched`；
/// * 下一轮同步**不冲掉**手填的 `external_id`；
/// * `kind` 不被同步轮改回 `coin`；
/// * 二者**不参与** `instrument.row_hash`。
#[test]
fn manual_kind_and_external_id_survive_sync_and_do_not_touch_row_hash() {
    let (s, _keep) = store();
    seed_venue(&s, "binance", "spot", "crypto");

    let mut cat = Catalog::default();
    let mut spot = std::collections::BTreeMap::new();
    spot.insert(
        "BTCUSDT".to_string(),
        SymbolInfo {
            symbol: "BTCUSDT".into(),
            base_asset: "BTC".into(),
            quote_asset: "USDT".into(),
            status: "trading".into(),
            ..SymbolInfo::default()
        },
    );
    cat.symbols.insert("binance|spot".into(), spot);
    persist_catalog_round(&s, &cat, &CatalogDiff::default(), 1, 2, &serde_json::json!({}), None)
        .unwrap();
    let before = s.instrument("binance", "spot", "BTCUSDT").unwrap().unwrap();
    let changes_before = s.list_changes_all().unwrap().len();

    // ① 改 kind（人工兜底）
    let rec = s
        .patch_asset("BTC", &AssetPatch { kind: Some("TOKEN".into()), external_id: None })
        .unwrap();
    assert_eq!(rec.kind, "token", "归一化为小写");
    assert_eq!(rec.match_status, "not_found", "只改 kind 不动匹配状态");

    // ② 手填 external_id → 服务端置 matched
    let rec = s
        .patch_asset(
            "BTC",
            &AssetPatch { kind: None, external_id: Some(Some("wrapped-bitcoin".into())) },
        )
        .unwrap();
    assert_eq!(rec.external_id.as_deref(), Some("wrapped-bitcoin"));
    assert_eq!(rec.match_status, "matched");

    // ③ 再跑一轮外部同步：索引里 `BTC` 指向 `bitcoin`，但手填值必须保留
    let provider = FakeProvider::new(vec![market("bitcoin", "BTC")]);
    let mut sync = OffchainSync::new();
    let out = sync.run_forced(&s, &provider, &light_only_cfg(), 1_770_000_000_000);
    assert_eq!(out.matched, 1);
    let after = asset_of(&s, "BTC");
    assert_eq!(
        after.external_id.as_deref(),
        Some("wrapped-bitcoin"),
        "手填别名不许被自动匹配冲掉（14 §5.2 可审计）"
    );
    assert_eq!(after.kind, "token", "kind 不许被同步轮改回 coin（14 §1 单写入方）");

    // ④ 再跑交易所同步：`kind` / `external_id` 不参与 row_hash → 零变更
    persist_catalog_round(&s, &cat, &CatalogDiff::default(), 3, 4, &serde_json::json!({}), None)
        .unwrap();
    let after_ins = s.instrument("binance", "spot", "BTCUSDT").unwrap().unwrap();
    assert_eq!(before.row_hash, after_ins.row_hash, "手改字段不进 row_hash");
    assert_eq!(
        s.list_changes_all().unwrap().len(),
        changes_before,
        "零动作：不写 instrument_change"
    );

    // ⑤ 清空手填 → 回落 not_found
    let rec = s
        .patch_asset("BTC", &AssetPatch { kind: None, external_id: Some(None) })
        .unwrap();
    assert_eq!(rec.external_id, None);
    assert_eq!(rec.match_status, "not_found");
}

/// `patch_asset` 的校验：非法 `kind`、空 `external_id`、两个字段都没给。
#[test]
fn patch_asset_rejects_invalid_input() {
    let (s, _keep) = store();
    s.ensure_asset("BTC", None, "coin").unwrap();

    let err = s
        .patch_asset("BTC", &AssetPatch { kind: Some("money".into()), external_id: None })
        .unwrap_err();
    assert!(matches!(err, ControlError::Validation(_)), "{err:?}");

    let err = s
        .patch_asset("BTC", &AssetPatch { kind: None, external_id: Some(Some("  ".into())) })
        .unwrap_err();
    assert!(matches!(err, ControlError::Validation(_)), "{err:?}");

    let err = s
        .patch_asset("BTC", &AssetPatch::default())
        .unwrap_err();
    assert!(matches!(err, ControlError::Validation(_)), "{err:?}");

    // 手输 CG id 的字符集与页面同规则（docs/14 §5.2：[A-Za-z0-9._-]）
    let err = s
        .patch_asset(
            "BTC",
            &AssetPatch { kind: None, external_id: Some(Some("bad id!".into())) },
        )
        .unwrap_err();
    assert!(matches!(err, ControlError::Validation(_)), "{err:?}");

    let err = s.patch_asset("NOPE", &AssetPatch { kind: Some("coin".into()), external_id: None }).unwrap_err();
    assert!(matches!(err, ControlError::NotFound(_)), "{err:?}");

    // 合法的 asset_class 也一并钉住（stock → 跳过链上币信息）
    assert!(ponk_control::validate_asset_class("Stock").is_ok());
    assert!(ponk_control::validate_asset_class("option").is_err());
}

/// 旧库升级：缺 `docs/14` 新列/新表的库**打开时不炸**。
///
/// `CREATE TABLE IF NOT EXISTS` 不会给已存在的表补列，所以迁移后必须靠
/// `sqlite.rs::ensure_columns` 幂等补列；`asset_contract` / `offchain_sync_run`
/// 是新表，由迁移脚本的 `IF NOT EXISTS` 直接建出来。
#[test]
fn legacy_database_without_docs14_columns_still_opens() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("control.db");
    {
        let conn = rusqlite::Connection::open(&path).expect("建旧库");
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE exchange (
               venue TEXT PRIMARY KEY, name TEXT NOT NULL, dig3_exchange_id TEXT NOT NULL,
               enabled INTEGER NOT NULL DEFAULT 1, extra TEXT NOT NULL DEFAULT '{}',
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE asset (
               id INTEGER PRIMARY KEY, code TEXT NOT NULL UNIQUE, name TEXT,
               kind TEXT NOT NULL DEFAULT 'coin',
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             ) STRICT;",
        )
        .expect("旧 DDL");
        conn.execute(
            "INSERT INTO exchange (venue,name,dig3_exchange_id,created_at,updated_at)
             VALUES ('binance','Binance','binance',1,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset (code,name,kind,created_at,updated_at)
             VALUES ('BTC','Bitcoin','coin',1,1)",
            [],
        )
        .unwrap();
    }

    let s = SqliteStore::open(&path).expect("旧库必须能打开");
    let ex = s.exchange("binance").expect("读所").expect("存在");
    assert_eq!(ex.asset_class, "crypto", "幂等补列给了默认值");
    let a = s.asset("BTC").expect("读币").expect("存在");
    assert_eq!(a.kind, "coin");
    assert_eq!(a.match_status, "not_found", "新列默认值");
    assert_eq!(a.external_id, None);
    assert_eq!(a.extra, serde_json::json!({}), "extra 默认空 JSON 对象");
    assert_eq!(a.detail_synced_at, None);

    // 新表已建好且可用
    assert!(s.asset_contracts("BTC").unwrap().is_empty());
    s.record_offchain_sync_run(1, 2, &serde_json::json!({ "matched": 1 }), None)
        .expect("offchain_sync_run 可写");
    assert_eq!(s.list_offchain_sync_runs_all().unwrap().len(), 1);

    // 再开一次仍是幂等的（不会因为列已存在而报错）
    drop(s);
    SqliteStore::open(&path).expect("重复打开");
}
