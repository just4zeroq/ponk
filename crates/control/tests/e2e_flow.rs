//! 端到端串联：**管理流程 → 采集流程 → 数据信道**。
//!
//! 一条测试覆盖四段真实链路（全部离线：内存事件中心 + 临时 SQLite，无外网、无 Redis）：
//!
//! 1. **管理**：发布候选池与承载上限 → 元数据同步建 catalog → 建采集节点（scope 规则）
//!    → 导出采集器 TOML 配置；
//! 2. **控制**：实例 register → 指派（按 scope 归组切片）→ 计划下发 → 拉取 → 版本前进；
//! 3. **采集侧回执**：心跳带 `applied_version` → 控制面判定**收敛**（09 §4.4）；
//! 4. **数据信道**：采集发布 `md.ticker` → 独立消费组读回；元数据同步产出 `md.meta`。

mod common;

use common::{temp_store, Cleanup};
use ponk_control::{
    ConfigStore, ControlConfig, ControlPlane, MetaError, MetadataFetcher, NewExchange, NewMarket,
    NewNode, ScopeRules,
};
use ponk_eventcenter::{EventCenter, MemoryEventCenter, SessionOpts, StartPosition};
use ponk_protocol::{
    Body, ConfigDomain, CtlMessage, CurrencyInfo, Envelope, EventType, Group, Market, MarketBody,
    MarketData, NodeId, NodeKind, OpsHeartbeat, Status, SubscribePlan, SymbolInfo, Ticker, Topic,
    Venue,
};
use ponk_control::META_SYNC_INTERVAL_MS;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const NOW: u64 = 1_780_000_000_000;
const INSTANCE: &str = "col-binance";

fn cfg(dir: &std::path::Path) -> ControlConfig {
    ControlConfig {
        store_root: dir.to_path_buf(),
        ..ControlConfig::default()
    }
}

/// 候选池 + 元数据同步目标（`metadata_targets` 取自该域的 venue/market）。
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

fn capacity_doc() -> serde_json::Value {
    // max_channels_per_process 必须 >= scope 的通道数（默认 5），否则分片被判"装不下"
    serde_json::json!({
        "limits": {"max_shards_per_process": 8, "max_instruments_per_process": 500,
                   "max_channels_per_process": 8},
        "distribution": {"strategy": "scatter", "order": "round_robin"}
    })
}

// ── 离线元数据拉取器 ─────────────────────────────────────────────────────────

struct FakeFetcher {
    /// 所方事实集，可原地追加以模拟**上新**（09 §2.3）。
    symbols: Mutex<Vec<SymbolInfo>>,
}

impl FakeFetcher {
    fn with(symbols: Vec<SymbolInfo>) -> Self {
        Self {
            symbols: Mutex::new(symbols),
        }
    }

    /// 模拟交易所在两轮同步之间上了一个新标的。
    fn push(&self, s: SymbolInfo) {
        self.symbols.lock().expect("fetcher lock").push(s);
    }
}

impl MetadataFetcher for FakeFetcher {
    fn fetch_symbols(&self, _venue: &str, _market: &str) -> Result<Vec<SymbolInfo>, MetaError> {
        Ok(self.symbols.lock().expect("fetcher lock").clone())
    }
    fn fetch_currencies(&self, _venue: &str) -> Result<Vec<CurrencyInfo>, MetaError> {
        Ok(Vec::new())
    }
}

fn sym(code: &str, base: &str) -> SymbolInfo {
    SymbolInfo {
        symbol: code.to_string(),
        base_asset: base.to_string(),
        quote_asset: "USDT".to_string(),
        status: "trading".to_string(),
        price_precision: 1,
        quantity_precision: 3,
        ..Default::default()
    }
}

// ── 心跳与会话工具 ───────────────────────────────────────────────────────────

fn heartbeat(
    instance: &str,
    status: Status,
    applied: Option<u64>,
    counters: std::collections::BTreeMap<String, i64>,
    ts: u64,
) -> Envelope {
    let hb = OpsHeartbeat {
        host: Some("tokyo-1".to_string()),
        instance_id: instance.to_string(),
        node_kind: NodeKind::Collector,
        status,
        applied_version: applied,
        counters,
        started_at: NOW,
        ts,
    };
    ponk_control::ops_envelope("collector", ts, hb).expect("heartbeat envelope")
}

/// 控制面下发的读取：在**独立线程**里做阻塞读（内存 backend 的 `next` 是阻塞的，
/// 不能占住主测试线程）。
///
/// **读到超时为止并取最高版本**：register 与 pull 各会下发一次（09 §4.3 拉取应答
/// 版本 +1），只读第一条会把 `applied_version` 报成旧值，控制面判为未收敛。
fn read_delivered_plan(center: &MemoryEventCenter, instance: &str) -> Option<SubscribePlan> {
    let topic = Topic::new(format!("ctl.collector.{instance}"));
    let group = Group::new(instance.to_string());
    let opts = SessionOpts {
        timeout: Duration::from_millis(300),
        start: StartPosition::Earliest,
    };
    let (tx, rx) = mpsc::channel();
    let c = center.clone();
    std::thread::spawn(move || {
        let mut sess = match c.open(&topic, &group, opts) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut best: Option<SubscribePlan> = None;
        loop {
            match sess.next(Duration::from_millis(300)) {
                Ok(Some(env)) => {
                    if let Body::Ctl(CtlMessage::SubscribePlan(p)) = env.body {
                        let better = best
                            .as_ref()
                            .map(|b| p.assignment_version > b.assignment_version)
                            .unwrap_or(true);
                        if better {
                            best = Some(p);
                        }
                    }
                }
                Ok(None) => break, // 超时无消息 = 读完
                Err(_) => break,
            }
        }
        let _ = sess.close();
        if let Some(p) = best {
            let _ = tx.send(p);
        }
    });
    rx.recv_timeout(Duration::from_secs(3)).ok()
}

/// 消费一条 `md.*`（独立消费组，模拟存储/信号节点读数据信道）。
fn read_md(center: &MemoryEventCenter, topic: &str, group: &str) -> Option<Envelope> {
    let t = Topic::new(topic.to_string());
    let g = Group::new(group.to_string());
    let opts = SessionOpts {
        timeout: Duration::from_millis(300),
        start: StartPosition::Earliest,
    };
    let (tx, rx) = mpsc::channel();
    let c = center.clone();
    std::thread::spawn(move || {
        let mut sess = match c.open(&t, &g, opts) {
            Ok(s) => s,
            Err(_) => return,
        };
        let env = sess.next(Duration::from_millis(400)).ok().flatten();
        let _ = sess.close();
        if let Some(e) = env {
            let _ = tx.send(e);
        }
    });
    rx.recv_timeout(Duration::from_secs(3)).ok()
}

fn md_ticker_env(instance: &str, ts: u64) -> Envelope {
    let body = MarketBody {
        venue: Venue::new("binance"),
        market: Market::new("swap"),
        native_code: "BTCUSDT".to_string(),
        exchange_ts: ts,
        data: MarketData::Ticker(Ticker {
            last_price: 64_000.0,
            bid_price: Some(63_999.0),
            ask_price: Some(64_001.0),
            timestamp: ts as i64,
            ..Default::default()
        }),
    };
    Envelope::new(
        EventType::Market,
        NodeId::new(instance),
        ts,
        Body::Market(body),
    )
    .expect("md envelope")
}

#[test]
fn end_to_end_manage_then_collect_then_converge() {
    let dir = temp_store("e2e-flow");
    let _c = Cleanup::new(dir.clone());

    // ── 控制面：SQLite + 内存事件中心（同一个实例共享，03 §1）────────────────
    let center = MemoryEventCenter::new();
    let mut plane = ControlPlane::open(center.clone(), cfg(&dir)).expect("open plane");

    // ── ① 管理：发布候选池与承载上限（域版本 +1，并广播 ConfigChanged）────────
    let (doc, out) = plane
        .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp/alice", "init")
        .expect("publish plan");
    assert_eq!(doc.version, 1);
    plane.publish_all(&out).expect("notify plan change");

    let (_, out) = plane
        .publish_domain(ConfigDomain::ProcessCapacity, capacity_doc(), "cp/alice", "limits")
        .expect("publish capacity");
    plane.publish_all(&out).expect("notify capacity change");

    // ── ② 管理：元数据同步 → catalog（5 分钟任务的产物，09 §2.4）───────────
    // `docs/14 §2`：同步目标 = `exchange_market` 中 `enabled=1` 的 (venue, market)，
    // **不是 `SubscribePlan` 域**（后者是采集意图，冷启动为空会死锁）。
    {
        let db = plane.store().sqlite().expect("sqlite 后端").clone();
        db.upsert_exchange(&NewExchange {
            venue: "binance".into(),
            name: "BINANCE".into(),
            ..NewExchange::default()
        })
        .expect("登记交易所");
        db.upsert_market(&NewMarket {
            venue: "binance".into(),
            market: "swap".into(),
            dig3_account_type: "FuturesCross".into(),
            ..NewMarket::default()
        })
        .expect("登记市场");
    }
    let fetcher = Arc::new(FakeFetcher::with(vec![
        sym("BTCUSDT", "BTC"),
        sym("ETHUSDT", "ETH"),
    ]));
    plane.set_fetcher(fetcher.clone());
    let outcome = plane.run_metadata_sync(NOW).expect("metadata sync");
    assert!(outcome.changed, "首轮同步应写入 catalog 事实");
    assert!(plane.catalog().symbol_count() >= 2, "catalog 应写入所方事实");

    // 同步产出 md.meta（数据信道上的参考信息事件）
    let meta = read_md(&center, "md.meta", "storage");
    assert!(meta.is_some(), "元数据同步应产出 md.meta 事件");

    // ── ③ 管理：建采集节点（声明式范围）+ 导出配置（02 §7.1 ①②）────────────
    let sqlite = plane.store().sqlite().expect("sqlite backend").clone();
    let node = sqlite
        .create_node(&NewNode {
            name: INSTANCE.to_string(),
            scope: ScopeRules {
                venues: vec!["binance".to_string()],
                markets: vec!["swap".to_string()],
                quote: vec!["USDT".to_string()],
                ..ScopeRules::default()
            },
            runtime_params: serde_json::json!({
                "heartbeat": {"interval_ms": 5000},
                "control": {"pull_on_start": true, "pull_retry_ms": 2000}
            }),
            ..NewNode::default()
        })
        .expect("create node");
    assert_eq!(node.name, INSTANCE);
    assert!(node.scope.venues.contains(&"binance".to_string()));

    let toml_cfg = sqlite.export_node_toml(INSTANCE).expect("export");
    assert!(toml_cfg.contains(&format!("instance_id = \"{INSTANCE}\"")), "{toml_cfg}");
    assert!(toml_cfg.contains("pull_on_start = true"), "导出配置必须开拉取：{toml_cfg}");

    // ── ④ 采集：register（Startup 心跳 → 控制面建会话并首次指派）────────────
    let out = plane
        .dispatch(
            &format!("ops.collector.{INSTANCE}"),
            heartbeat(INSTANCE, Status::Startup, Some(0), Default::default(), NOW),
        )
        .expect("register");
    assert!(!out.is_empty(), "register 后应立即下发初始计划（10 §7）");
    plane.publish_all(&out).expect("deliver initial plan");

    let key = plane
        .registry()
        .get(INSTANCE)
        .expect("snapshot key exists after register");
    let v1 = key
        .last_assignment_version
        .expect("指派应记录 assignment_version");
    assert!(v1 >= 1);

    // ── ⑤ 采集：拉取（ctl.request）→ 再次拿到计划 ────────────────────────────
    let out = plane
        .dispatch(
            &format!("ctl.request.{INSTANCE}"),
            heartbeat(INSTANCE, Status::Startup, Some(0), Default::default(), NOW + 1),
        )
        .expect("pull");
    assert!(!out.is_empty(), "拉取必须有应答（10 §4.1：拉是正确性保证）");
    plane.publish_all(&out).expect("deliver pull answer");

    // ── ⑥ 采集：从事件中心读到计划，且计划落在其声明范围内 ────────────────────
    let plan = read_delivered_plan(&center, INSTANCE).expect("采集应收到 SubscribePlan");
    assert!(
        plan.assignment_version >= 1,
        "计划版本应前进，实际 {}",
        plan.assignment_version
    );
    assert!(!plan.shards.is_empty(), "scope 解析后应有分片");
    for shard in &plan.shards {
        assert_eq!(shard.venue.as_str(), "binance", "只应持有声明的所");
        assert_eq!(shard.market.as_str(), "swap", "只应持有声明的市场");
        for ins in &shard.instruments {
            assert!(
                matches!(
                    ins.meta.get("quote").map(String::as_str),
                    Some("USDT")
                ),
                "只应持有 USDT 计价标的：{}",
                ins.native_code
            );
            assert!(
                ins.channels.iter().any(|c| c.as_str() == "candle_1m"),
                "通道来自节点 scope：{:?}",
                ins.channels
            );
        }
    }

    // ── ⑦ 采集：心跳回报 applied_version → 控制面判定收敛（09 §4.4）──────────
    let mut counters = std::collections::BTreeMap::new();
    for (k, v) in plane.store().current_versions().expect("domain versions") {
        counters.insert(k, v);
    }
    plane
        .dispatch(
            &format!("ops.collector.{INSTANCE}"),
            heartbeat(
                INSTANCE,
                Status::Running,
                Some(plan.assignment_version),
                counters,
                NOW + 5_000,
            ),
        )
        .expect("running heartbeat");
    let conv = plane
        .registry()
        .convergence(plane.store(), NOW + 5_000);
    assert!(
        conv.not_converged
            .iter()
            .all(|n| n.instance_id != INSTANCE),
        "applied_version == assignment_version 后应收敛，实际未收敛：{:?}",
        conv.not_converged
    );
    assert!(conv.converged >= 1);

    // ── ⑧ 数据信道：采集发行情 → 独立消费组读回 ───────────────────────────────
    center
        .publish(
            &Topic::new("md.ticker"),
            &md_ticker_env(INSTANCE, NOW + 6_000),
        )
        .expect("publish md.ticker");
    let md = read_md(&center, "md.ticker", "storage").expect("数据消费组应读到行情");
    match &md.body {
        Body::Market(m) => {
            assert_eq!(m.native_code, "BTCUSDT");
            assert_eq!(m.venue.as_str(), "binance");
            assert!(matches!(m.data, MarketData::Ticker(_)));
        }
        other => panic!("非行情负载：{other:?}"),
    }

    // ── ⑨ 上新：同步发现新标的 → 自动触发重算 → 计划前进（09 §2.3 / §5.4）───
    // （不是改配置域：catalog 变更本身就是触发条件；若签名未变则**不下发**，
    //  这正是"无变更零动作"的语义。）
    fetcher.push(sym("AVAXUSDT", "AVAX"));
    let outcome2 = plane
        .run_metadata_sync(NOW + META_SYNC_INTERVAL_MS + 1)
        .expect("metadata sync #2");
    assert!(outcome2.changed, "上新应被比对为变更");

    let plan2 = read_delivered_plan(&center, INSTANCE).expect("上新后应重发计划");
    assert!(
        plan2.assignment_version > v1,
        "变更后版本应前进（v1={v1}, v2={}）",
        plan2.assignment_version
    );
    assert!(
        plan2
            .shards
            .iter()
            .any(|s| s.instruments.iter().any(|i| i.native_code == "AVAXUSDT")),
        "新上的标的应进入计划（自动纳入，无需改配置）"
    );

    let versions_v2 = plane.store().current_versions().expect("versions");
    plane
        .dispatch(
            &format!("ops.collector.{INSTANCE}"),
            heartbeat(
                INSTANCE,
                Status::Running,
                Some(plan2.assignment_version),
                versions_v2,
                NOW + 10_000,
            ),
        )
        .expect("running heartbeat v2");
    let conv2 = plane.registry().convergence(plane.store(), NOW + 10_000);
    assert!(
        conv2.not_converged.iter().all(|n| n.instance_id != INSTANCE),
        "新计划应用后应重新收敛：{:?}",
        conv2.not_converged
    );

    // ── ⑩ O8 决议：停用节点 → 不再进候选集（不再收到新计划）──────────────────
    sqlite
        .upsert_node(&NewNode {
            name: INSTANCE.to_string(),
            enabled: false,
            scope: ScopeRules {
                venues: vec!["binance".to_string()],
                markets: vec!["swap".to_string()],
                quote: vec!["USDT".to_string()],
                ..ScopeRules::default()
            },
            ..NewNode::default()
        })
        .expect("disable node");

    let v_before = plane
        .registry()
        .get(INSTANCE)
        .and_then(|k| k.last_assignment_version);
    let out = plane.recompute().expect("recompute after disable");
    let plans_after_disable: Vec<_> = out
        .iter()
        .filter(|o| o.topic.as_str() == format!("ctl.collector.{INSTANCE}"))
        .collect();
    assert!(
        plans_after_disable.is_empty(),
        "停用节点不应再收到计划（O8）"
    );
    let v_after = plane
        .registry()
        .get(INSTANCE)
        .and_then(|k| k.last_assignment_version);
    assert_eq!(v_before, v_after, "停用不改变已下发版本");
}

/// 控制面自身的崩溃恢复：重启后实例状态从快照键恢复，版本门不重置
/// （否则实例重启后会被自己的强序门永久拒收，10 §4.3 / 09 §6.3）。
#[test]
fn control_plane_restart_restores_instances_and_versions() {
    let dir = temp_store("e2e-restart");
    let _c = Cleanup::new(dir.clone());

    let center = MemoryEventCenter::new();
    {
        let mut plane = ControlPlane::open(center.clone(), cfg(&dir)).expect("open #1");
        plane
            .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp/alice", "init")
            .expect("publish");
        let out = plane
            .dispatch(
                &format!("ops.collector.{INSTANCE}"),
                heartbeat(INSTANCE, Status::Startup, Some(0), Default::default(), NOW),
            )
            .expect("register");
        plane.publish_all(&out).expect("deliver");
    } // 第一个控制面实例退出（未 shutdown 任何采集）

    let plane2 = ControlPlane::open(center.clone(), cfg(&dir)).expect("open #2");
    let key = plane2
        .registry()
        .get(INSTANCE)
        .expect("重启后应从快照键恢复实例");
    assert_eq!(key.host.as_deref(), Some("tokyo-1"), "host 应回填");
    assert!(
        key.last_assignment_version.unwrap_or(0) >= 1,
        "assignment_version 必须恢复（否则实例被强序门永久拒收）"
    );
    assert!(
        !matches!(key.status, ponk_control::InstStatus::Dead),
        "刚注册的实例不该被判死"
    );
}
