//! 控制面与总线的闭环（09 §4.3 发布 / §4.6 拉取应答 / §6 控制信道消费 / §6.5 自身心跳）。
//!
//! 全部**离线**：进程内内存 backend（03 §7），无 Redis、无外网。
//!
//! 必测（交付要求）：**拉取请求 → `ConfigSnapshot` 应答的编解码**、
//! **快照键写入与恢复、失联判定时间窗**。

mod common;

use common::{temp_store, Cleanup};
use ponk_control::{
    ConfigStore, ControlConfig, ControlPlane, InstStatus, LivenessConfig, OpsRegistry,
};
use ponk_eventcenter::{
    DeadLetterReader, EventCenter, MemoryEventCenter, SessionOpts, StartPosition,
};
use ponk_protocol::{
    Body, ConfigDomain, CtlMessage, Envelope, Group, NodeKind, OpsHeartbeat, PlanMode, Status,
    Topic,
};
use std::time::Duration;

const NOW: u64 = 1_770_000_000_000;

fn cfg(dir: &std::path::Path) -> ControlConfig {
    ControlConfig {
        store_root: dir.to_path_buf(),
        ..ControlConfig::default()
    }
}

fn hb(
    kind: NodeKind,
    id: &str,
    status: Status,
    applied: Option<u64>,
    counters: Vec<(&str, i64)>,
    ts: u64,
) -> OpsHeartbeat {
    OpsHeartbeat {
        host: None,
        instance_id: id.to_string(),
        node_kind: kind,
        status,
        applied_version: applied,
        counters: counters
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        started_at: ts,
        ts,
    }
}

fn env_of(hb: OpsHeartbeat) -> Envelope {
    ponk_control::ops_envelope("test-peer", hb.ts, hb).expect("信封构造")
}

fn plan_doc() -> serde_json::Value {
    serde_json::json!({
        "shard_size": 8,
        "default_channels": ["md.candle.1m", "md.trade"],
        "instruments": [
            {"venue": "binance", "market": "swap", "native_code": "BTCUSDT",
             "price_precision": 1, "qty_precision": 0},
            {"venue": "binance", "market": "swap", "native_code": "ETHUSDT",
             "price_precision": 2, "qty_precision": 3},
            {"venue": "binance", "market": "swap", "native_code": "SOLUSDT",
             "price_precision": 2, "qty_precision": 2}
        ],
        "quota": [{"venue": "binance", "egress_ip": "1.2.3.4",
                   "collect": 0.5, "backfill": 0.35, "metadata": 0.15}]
    })
}

fn channels_doc() -> serde_json::Value {
    serde_json::json!({
        "channels": [{"id": "feishu", "backend": "feishu",
                      "endpoint": "https://open.feishu.cn/open-apis/bot/v2/hook/abc",
                      "retry": {"attempts": 3, "backoff": "2s"},
                      "limits": {"rate_per_min": 60}}]
    })
}

fn alert_doc(signal: &str, channel: &str) -> serde_json::Value {
    serde_json::json!({
        "alerts": [{
            "rule_id": "btc_4h_rsi_overbought",
            "name": "BTC 4h RSI 超买",
            "match": {"signal_id": signal},
            "when": {"all": [
                {"field": "rsi", "op": ">", "value": 70},
                {"field": "basis", "op": "<", "value": -0.01}
            ]},
            "window": {"occurrences": 2, "within": "10m"},
            "level": "Warn",
            "route": [{"channel": channel, "target": "ops-room"}],
            "throttle": "5m"
        }]
    })
}

fn signal_doc() -> serde_json::Value {
    serde_json::json!({
        "rules": [{
            "signal_id": "btc_4h_momentum",
            "signal_name": "BTC 4h 动量",
            "timeframe": "4h",
            "scope": ["binance:swap:*"],
            "inputs": ["md.candle.1m"],
            "graph": [
                {"id": "close", "op": "pick", "args": {"input": "md.candle.1m", "field": "close"}},
                {"id": "rsi", "op": "rsi", "args": {"period": 14}, "in": ["close"]}
            ],
            "emit": {"value": "rsi", "payload": ["rsi"]}
        }]
    })
}

// ── ① 发布 → ConfigChanged ──────────────────────────────────────────────────

#[test]
fn publish_emits_config_changed_carrying_only_domain_and_version() {
    let dir = temp_store("publish");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();

    let (doc, out) = plane
        .publish_domain(ConfigDomain::SignalRules, signal_doc(), "cp-1/alice", "init")
        .unwrap();
    assert_eq!(doc.version, 1);

    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].topic.as_str(),
        ConfigDomain::SignalRules.topic(),
        "变更通知发到 ctl.<domain>"
    );
    match &out[0].envelope.body {
        Body::Ctl(CtlMessage::ConfigChanged(c)) => {
            assert_eq!(c.domain, ConfigDomain::SignalRules);
            assert_eq!(c.version, 1);
            assert_eq!(c.publisher, "cp-1/alice");
            assert!(c.ts > 0);
        }
        other => panic!("期望 ConfigChanged，实际 {other:?}"),
    }
    // ConfigChanged **不携带配置本体**（03 §5.3 / 09 §3.1）
    let json = serde_json::to_value(&out[0].envelope).unwrap();
    assert!(json["body"]["config_changed"].get("payload").is_none());
}

#[test]
fn process_capacity_publish_sends_no_notification() {
    let dir = temp_store("pc");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();
    let (_doc, out) = plane
        .publish_domain(
            ConfigDomain::ProcessCapacity,
            serde_json::json!({
                "limits": {"max_shards_per_process": 4, "max_instruments_per_process": 100,
                           "max_channels_per_process": 4},
                "distribution": {"strategy": "scatter", "order": "round_robin"}
            }),
            "cp-1/api",
            "tune",
        )
        .unwrap();
    assert!(
        out.iter().all(|o| !o.topic.as_str().starts_with("ctl.process_capacity")),
        "09 §2.2 C：ProcessCapacity 无外部消费者，不发通知"
    );
}

#[test]
fn cross_domain_reference_is_validated_on_write() {
    let dir = temp_store("xref");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();

    plane
        .publish_domain(ConfigDomain::Channels, channels_doc(), "cp-1", "init")
        .unwrap();
    plane
        .publish_domain(ConfigDomain::SignalRules, signal_doc(), "cp-1", "init")
        .unwrap();

    // 引用不存在的渠道 → 拒绝
    let bad = alert_doc("btc_4h_momentum", "no_such_channel");
    assert!(
        plane
            .publish_domain(ConfigDomain::AlertRules, bad, "cp-1", "bad")
            .is_err(),
        "09 §4.3：route.channel 跨域引用必须存在"
    );
    // 引用不存在的信号 → 拒绝
    let bad = alert_doc("no_such_signal", "feishu");
    assert!(
        plane
            .publish_domain(ConfigDomain::AlertRules, bad, "cp-1", "bad")
            .is_err(),
        "09 §4.3：signal_id 跨域引用必须存在"
    );
    assert_eq!(
        plane.store().current_version(ConfigDomain::AlertRules).unwrap(),
        None,
        "校验不过不得写入"
    );

    // 正确引用 → 通过
    let ok = alert_doc("btc_4h_momentum", "feishu");
    let (doc, _) = plane
        .publish_domain(ConfigDomain::AlertRules, ok, "cp-1", "ok")
        .unwrap();
    assert_eq!(doc.version, 1);
}

// ── ② register → 初始 SubscribePlan ────────────────────────────────────────

#[test]
fn register_replies_with_initial_subscribe_plan_snapshot() {
    let dir = temp_store("register");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();
    plane
        .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1", "init")
        .unwrap();

    let start = hb(
        NodeKind::Collector,
        "col-1",
        Status::Startup,
        Some(0),
        vec![
            ("capacity.shards", 16),
            ("capacity.instruments", 100),
            ("capacity.channels", 4),
            ("capacity.venue.binance", 1),
        ],
        NOW,
    );
    let out = plane.dispatch("ops.collector.col-1", env_of(start)).unwrap();

    let plan_out = out
        .iter()
        .find(|o| o.topic.as_str() == "ctl.collector.col-1")
        .expect("register 后应回初始计划（10 §5：省一次拉取）");
    match &plan_out.envelope.body {
        Body::Ctl(CtlMessage::SubscribePlan(p)) => {
            assert_eq!(p.assignment_version, 1, "首次下发 assignment_version = 1");
            assert_eq!(p.mode, PlanMode::Snapshot);
            assert!(!p.shards.is_empty());
            let q = p.quota_share.as_ref().expect("带配额份额");
            assert!((q.collect - 0.5).abs() < 1e-12, "唯一持有进程拿全额份额");
            assert_eq!(q.version, 1);
        }
        other => panic!("期望 SubscribePlan，实际 {other:?}"),
    }

    // 快照键已写入
    let key = plane.registry().get("col-1").expect("register 应写快照键");
    assert_eq!(key.status, InstStatus::Registered);
    assert_eq!(key.reported.max_shards, Some(16), "自报能力入快照键");
    assert_eq!(key.last_assignment_version, Some(1));
}

// ── ③ 拉取请求 → ConfigSnapshot 应答（编解码，走总线闭环）──────────────────

#[test]
fn pull_request_is_answered_with_config_snapshot_over_the_bus() {
    let dir = temp_store("pull");
    let _c = Cleanup::new(dir.clone());
    let ec = MemoryEventCenter::new();
    let bus = ec.clone();
    let mut plane = ControlPlane::open(ec, cfg(&dir)).unwrap();

    plane
        .publish_domain(ConfigDomain::Channels, channels_doc(), "cp-1", "init")
        .unwrap();
    let (_doc, _) = plane
        .publish_domain(ConfigDomain::AlertRules, alert_doc("s", "feishu"), "cp-1", "init")
        .unwrap();

    plane.attach_instance(NodeKind::Alert, "alert-1").unwrap();

    // 消费方：等在 ctl.alert.alert-1 上（应答主题，03 §8）
    let mut watcher = bus
        .open(
            &Topic::new("ctl.alert.alert-1"),
            &Group::new("alert-1-watcher"),
            SessionOpts {
                timeout: Duration::from_millis(500),
                start: StartPosition::Earliest,
            },
        )
        .unwrap();

    // 实例在 ctl.request.<instance> 上拉取，携带 applied_version（10 §4.1）
    let pull = env_of(hb(
        NodeKind::Alert,
        "alert-1",
        Status::Running,
        Some(0),
        vec![],
        NOW,
    ));
    bus.publish(&Topic::new("ctl.request.alert-1"), &pull).unwrap();

    let n = plane.poll(Duration::from_millis(200)).unwrap();
    assert!(n >= 1, "控制面应读到拉取请求，实际 {n}");

    let got = watcher
        .next(Duration::from_millis(500))
        .unwrap()
        .expect("应有 ConfigSnapshot 应答");

    // **编解码**：线上字节 → 信封 → 负载
    let wire = serde_json::to_vec(&got).unwrap();
    let back: Envelope = serde_json::from_slice(&wire).unwrap();
    assert_eq!(back.event_type, ponk_protocol::EventType::Ctl);
    match back.body {
        Body::Ctl(CtlMessage::ConfigSnapshot {
            domain,
            version,
            payload,
        }) => {
            assert_eq!(domain, ConfigDomain::AlertRules);
            assert_eq!(version, 1, "应答带该域当前版本");
            assert_eq!(payload.0["alerts"][0]["rule_id"], "btc_4h_rsi_overbought");
            assert_eq!(payload.0["alerts"][0]["route"][0]["channel"], "feishu");
        }
        other => panic!("期望 ConfigSnapshot，实际 {other:?}"),
    }

    // 只应答请求方：同一批里 Channels 不在消费域内（09 §8：06 消费 AlertRules/Channels/Serialization）
    let second = watcher
        .next(Duration::from_millis(200))
        .unwrap()
        .expect("06 还应收到 Channels 应答");
    match second.body {
        Body::Ctl(CtlMessage::ConfigSnapshot { domain, .. }) => {
            assert_eq!(domain, ConfigDomain::Channels);
        }
        other => panic!("期望 ConfigSnapshot，实际 {other:?}"),
    }
}

#[test]
fn pull_for_each_consumed_domain_follows_the_interface_table() {
    let dir = temp_store("pull-domains");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();
    plane
        .publish_domain(ConfigDomain::SignalRules, signal_doc(), "cp-1", "i")
        .unwrap();
    plane
        .publish_domain(
            ConfigDomain::Serialization,
            serde_json::json!({
                "families": {"md.*": "json", "signal.*": "json", "ctl.*": "json", "ops.*": "json"}
            }),
            "cp-1",
            "i",
        )
        .unwrap();

    // 09 §8：05 消费 SignalRules / Serialization / InstrumentBlocklist
    let out = plane
        .dispatch(
            "ctl.request.signal-1",
            env_of(hb(NodeKind::Signal, "signal-1", Status::Running, Some(0), vec![], NOW)),
        )
        .unwrap();
    let domains: Vec<ConfigDomain> = out
        .iter()
        .filter_map(|o| match &o.envelope.body {
            Body::Ctl(CtlMessage::ConfigSnapshot { domain, .. }) => Some(*domain),
            _ => None,
        })
        .collect();
    assert!(domains.contains(&ConfigDomain::SignalRules), "实际 {domains:?}");
    assert!(domains.contains(&ConfigDomain::Serialization));
    assert!(
        !domains.contains(&ConfigDomain::ProcessCapacity),
        "09 §2.2 C：ProcessCapacity 不应答"
    );
    assert!(
        out.iter().all(|o| o.topic.as_str() == "ctl.signal.signal-1"),
        "应答一律回 ctl.<node_kind>.<instance>"
    );
}

// ── ④ 回滚后拉取到的是新版本号 ──────────────────────────────────────────────

#[test]
fn rollback_is_visible_to_the_next_pull_as_a_new_version() {
    let dir = temp_store("rollback-pull");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();
    plane
        .publish_domain(ConfigDomain::Channels, channels_doc(), "cp-1", "v1")
        .unwrap();

    let mut v2 = channels_doc();
    v2["channels"][0]["endpoint"] = serde_json::json!("https://example.invalid/hook/xyz");
    plane
        .publish_domain(ConfigDomain::Channels, v2, "cp-1", "v2")
        .unwrap();

    let (rolled, _out) = plane
        .rollback(ConfigDomain::Channels, 1, "cp-1/alice")
        .unwrap();
    assert_eq!(rolled.version, 3, "回滚产生新版本号");

    let out = plane
        .dispatch(
            "ctl.request.alert-9",
            env_of(hb(NodeKind::Alert, "alert-9", Status::Running, Some(0), vec![], NOW)),
        )
        .unwrap();
    let snap = out
        .iter()
        .find_map(|o| match &o.envelope.body {
            Body::Ctl(CtlMessage::ConfigSnapshot { domain, version, payload })
                if *domain == ConfigDomain::Channels =>
            {
                Some((*version, payload.0.clone()))
            }
            _ => None,
        })
        .expect("Channels 应答");
    assert_eq!(snap.0, 3, "收敛判据仍单调：应答给最新版本");
    assert_eq!(
        snap.1["channels"][0]["endpoint"],
        "https://open.feishu.cn/open-apis/bot/v2/hook/abc",
        "回滚内容 = v1 的内容"
    );
}

// ── ⑤ 控制面自身心跳（09 §6.5）─────────────────────────────────────────────

#[test]
fn control_heartbeat_carries_current_domain_versions_and_no_applied_version() {
    let dir = temp_store("hb");
    let _c = Cleanup::new(dir.clone());
    let ec = MemoryEventCenter::new();
    let bus = ec.clone();
    let mut plane = ControlPlane::open(ec, cfg(&dir)).unwrap();
    plane
        .publish_domain(ConfigDomain::SignalRules, signal_doc(), "cp-1", "i")
        .unwrap();
    plane.set_status(Status::Running);

    let mut watcher = bus
        .open(
            &Topic::new("ops.control"),
            &Group::new("watcher"),
            SessionOpts {
                timeout: Duration::from_millis(300),
                start: StartPosition::Earliest,
            },
        )
        .unwrap();

    plane.publish_heartbeat(NOW).unwrap();
    let env = watcher
        .next(Duration::from_millis(500))
        .unwrap()
        .expect("ops.control 心跳");
    match env.body {
        Body::Ops(hb) => {
            assert_eq!(hb.node_kind, NodeKind::Control);
            assert_eq!(hb.instance_id, "cp-1");
            assert_eq!(hb.status, Status::Running);
            assert_eq!(
                hb.applied_version, None,
                "09 §4.4：控制面是版本来源，applied_version = None"
            );
            assert_eq!(
                hb.counters[ConfigDomain::SignalRules.counter_key().as_str()],
                1,
                "counters 携带各域当前版本"
            );
            assert_eq!(hb.counters[ConfigDomain::SubscribePlan.counter_key().as_str()], 0);
            assert!(hb.counters.contains_key("instances.running"));
            assert!(hb.counters.contains_key("not_converged"));
        }
        other => panic!("期望 OpsHeartbeat，实际 {other:?}"),
    }

    // 控制面不消费自己的主题（09 §6.5）
    let out = plane
        .dispatch("ops.control", env_of(hb(NodeKind::Control, "cp-1", Status::Running, None, vec![], NOW)))
        .unwrap();
    assert!(out.is_empty());
}

// ── ⑥ 快照键恢复 + 失联判定时间窗（09 §6.3 / §6.4）─────────────────────────

#[test]
fn snapshot_keys_are_persisted_and_restored() {
    let dir = temp_store("snap");
    let _c = Cleanup::new(dir.clone());

    let mut reg = OpsRegistry::new(
        Some(dir.clone()),
        LivenessConfig::default(),
    );
    let start = hb(NodeKind::Collector, "col-1", Status::Startup, Some(0), vec![], NOW);
    reg.on_register(&start, NOW).unwrap();
    let beat = hb(NodeKind::Collector, "col-1", Status::Running, Some(7), vec![("cfg.channels", 3)], NOW + 5_000);
    reg.on_heartbeat(&beat, NOW + 5_000).unwrap();

    // 控制面重启：从快照键恢复「每实例最新状态」（09 §6.3）
    let mut reg2 = OpsRegistry::new(Some(dir.clone()), LivenessConfig::default());
    let n = reg2.restore().unwrap();
    assert_eq!(n, 1, "应恢复 1 条快照键");
    let key = reg2.get("col-1").expect("恢复的快照键");
    assert_eq!(key.status, InstStatus::Running);
    assert_eq!(key.applied_version, Some(7));
    assert_eq!(key.last_seen_ts, NOW + 5_000, "last_seen 不因重启丢失");
    assert_eq!(key.counters["cfg.channels"], 3);
    assert_eq!(key.reported.max_shards, None);
}

#[test]
fn liveness_window_is_three_times_heartbeat_interval() {
    let dir = temp_store("liveness");
    let _c = Cleanup::new(dir.clone());
    let liveness = LivenessConfig::default();
    assert_eq!(liveness.heartbeat_interval_ms, 5_000, "10 §5：心跳 5s");
    assert_eq!(liveness.miss_threshold, 3, "10 §5：3×5s");
    assert_eq!(
        liveness.lost_after_ms(),
        15_000,
        "判死时间窗 = 3 × 5s"
    );

    let mut reg = OpsRegistry::new(Some(dir.clone()), liveness);
    let start = hb(NodeKind::Collector, "col-1", Status::Startup, Some(0), vec![], NOW);
    reg.on_register(&start, NOW).unwrap();
    let beat = hb(NodeKind::Collector, "col-1", Status::Running, Some(1), vec![], NOW + 5_000);
    reg.on_heartbeat(&beat, NOW + 5_000).unwrap();

    let last_seen = NOW + 5_000;
    // 窗口内：不判死
    assert!(reg.sweep(last_seen + 14_999).unwrap().is_empty(), "14.999s 不判死");
    assert!(reg.sweep(last_seen + 14_000).unwrap().is_empty(), "14s 不判死");
    // 窗口边界：判死
    let dead = reg.sweep(last_seen + 15_000).unwrap();
    assert_eq!(dead, vec!["col-1".to_string()], "3×5s 无心跳 → 判死");
    assert_eq!(reg.get("col-1").unwrap().status, InstStatus::Dead);

    // 判死后再次扫描不再重复计入
    assert!(reg.sweep(last_seen + 20_000).unwrap().is_empty());
    assert_eq!(reg.get("col-1").unwrap().lost_count, 1);

    // 收到心跳可复活（09 §6.4 只标记状态，不改指派）
    let revive = hb(NodeKind::Collector, "col-1", Status::Running, Some(1), vec![], last_seen + 21_000);
    reg.on_heartbeat(&revive, last_seen + 21_000).unwrap();
    assert_eq!(reg.get("col-1").unwrap().status, InstStatus::Running);
}

#[test]
fn shutdown_marks_stopped_and_detaches_the_instance_sessions() {
    let dir = temp_store("shutdown");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();
    plane
        .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1", "i")
        .unwrap();
    plane
        .dispatch(
            "ops.collector.col-1",
            env_of(hb(NodeKind::Collector, "col-1", Status::Startup, Some(0), vec![], NOW)),
        )
        .unwrap();
    assert_eq!(plane.session_count(), 1, "register 建立会话");

    plane
        .dispatch(
            "ops.collector.col-1",
            env_of(hb(NodeKind::Collector, "col-1", Status::Shutdown, Some(1), vec![], NOW + 1_000)),
        )
        .unwrap();
    assert_eq!(
        plane.registry().get("col-1").unwrap().status,
        InstStatus::Stopped
    );
    assert_eq!(plane.session_count(), 0, "shutdown 关闭该实例的主题会话（09 §6.2）");
}

// ── ⑦ 收敛判据（09 §4.4）────────────────────────────────────────────────────

#[test]
fn convergence_compares_assignment_version_and_per_domain_counters() {
    let dir = temp_store("conv");
    let _c = Cleanup::new(dir.clone());
    let store = ponk_control::FileConfigStore::open(&dir).unwrap();
    store
        .publish(ConfigDomain::SignalRules, signal_doc(), "cp-1", "i")
        .unwrap();

    let mut reg = OpsRegistry::new(Some(dir.clone()), LivenessConfig::default());

    // 采集：applied_version == 最新 assignment_version
    let start = hb(NodeKind::Collector, "col-1", Status::Startup, Some(0), vec![], NOW);
    reg.on_register(&start, NOW).unwrap();
    reg.set_assignment("col-1", 5).unwrap();
    let beat = hb(NodeKind::Collector, "col-1", Status::Running, Some(3), vec![], NOW + 5_000);
    reg.on_heartbeat(&beat, NOW + 5_000).unwrap();

    let conv = reg.convergence(&store, NOW + 5_000);
    assert_eq!(conv.not_converged.len(), 1);
    assert_eq!(conv.not_converged[0].instance_id, "col-1");
    assert_eq!(conv.not_converged[0].domain, None, "采集比的是 assignment_version");
    assert_eq!(conv.not_converged[0].expected, 5);
    assert_eq!(conv.not_converged[0].actual, 3);

    // 应用到位 → 收敛
    let beat = hb(NodeKind::Collector, "col-1", Status::Running, Some(5), vec![], NOW + 10_000);
    reg.on_heartbeat(&beat, NOW + 10_000).unwrap();
    let conv = reg.convergence(&store, NOW + 10_000);
    assert!(conv.not_converged.is_empty(), "applied == assignment → 收敛");

    // 05 信号：逐域 counters["cfg.<domain>"] == 该域当前版本
    let start = hb(NodeKind::Signal, "sig-1", Status::Startup, Some(1), vec![], NOW);
    reg.on_register(&start, NOW).unwrap();
    let sig_key = ConfigDomain::SignalRules.counter_key();
    let counters = vec![(sig_key.as_str(), 0)]; // 落后
    let beat = hb(NodeKind::Signal, "sig-1", Status::Running, Some(1), counters, NOW + 5_000);
    reg.on_heartbeat(&beat, NOW + 5_000).unwrap();

    let conv = reg.convergence(&store, NOW + 5_000);
    let nc = conv
        .not_converged
        .iter()
        .find(|n| n.instance_id == "sig-1")
        .expect("cfg.signal_rules = 0 ≠ 当前版本 1");
    assert_eq!(nc.domain, Some(ConfigDomain::SignalRules));
    assert_eq!(nc.expected, 1);
    assert_eq!(nc.actual, 0);

    // 应用到当前版本 → 收敛
    let beat = hb(
        NodeKind::Signal,
        "sig-1",
        Status::Running,
        Some(1),
        vec![(sig_key.as_str(), 1)],
        NOW + 10_000,
    );
    reg.on_heartbeat(&beat, NOW + 10_000).unwrap();
    let conv = reg.convergence(&store, NOW + 10_000);
    assert!(
        conv.not_converged.iter().all(|n| n.instance_id != "sig-1"),
        "逐域 counters 收敛"
    );
}

#[test]
fn no_dead_letter_is_produced_by_a_healthy_round() {
    // 控制面不应产生死信（03 §7.3：只有发布失败才进 dlq）
    let dir = temp_store("dlq");
    let _c = Cleanup::new(dir.clone());
    let ec = MemoryEventCenter::new();
    let bus = ec.clone();
    let mut plane = ControlPlane::open(ec, cfg(&dir)).unwrap();
    plane
        .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1", "i")
        .unwrap();
    let (_, out) = (
        (),
        plane
            .dispatch(
                "ops.collector.col-1",
                env_of(hb(NodeKind::Collector, "col-1", Status::Startup, Some(0), vec![], NOW)),
            )
            .unwrap(),
    );
    plane.publish_all(&out).unwrap();
    assert!(bus
        .dead_letters(&Topic::new("ctl.collector.col-1"), 16)
        .unwrap()
        .is_empty());
}

// ── 采集器管理：register / heartbeat 重推 / 失联不改指派 ────────────────────

fn register_collector(
    plane: &mut ControlPlane<MemoryEventCenter>,
    id: &str,
    ts: u64,
) -> Vec<ponk_control::Outbound> {
    let env = env_of(hb(NodeKind::Collector, id, Status::Startup, Some(0), vec![], ts));
    plane.dispatch(&format!("ops.collector.{id}"), env).unwrap()
}

fn heartbeat(
    plane: &mut ControlPlane<MemoryEventCenter>,
    id: &str,
    applied: u64,
    ts: u64,
) -> Vec<ponk_control::Outbound> {
    let env = env_of(hb(
        NodeKind::Collector,
        id,
        Status::Running,
        Some(applied),
        vec![],
        ts,
    ));
    plane.dispatch(&format!("ops.collector.{id}"), env).unwrap()
}

fn plan_of(out: &[ponk_control::Outbound], id: &str) -> Option<ponk_protocol::SubscribePlan> {
    out.iter().find_map(|o| match &o.envelope.body {
        Body::Ctl(CtlMessage::SubscribePlan(p)) if o.topic.as_str() == format!("ctl.collector.{id}") => {
            Some(p.clone())
        }
        _ => None,
    })
}

#[test]
fn non_converging_collector_is_repushed_after_the_timeout() {
    let dir = temp_store("repush");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();
    plane
        .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1", "i")
        .unwrap();

    let out = register_collector(&mut plane, "col-1", NOW);
    let first = plan_of(&out, "col-1").expect("register 回初始计划");
    assert_eq!(first.assignment_version, 1);
    assert_eq!(
        plane.registry().get("col-1").unwrap().last_assignment_version,
        Some(1)
    );

    // ① 心跳 applied_version=0 ≠ 1 → 不收敛，但还在 30s 窗口内
    let out = heartbeat(&mut plane, "col-1", 0, NOW + 5_000);
    assert!(out.is_empty(), "30s 内不重推");
    assert!(
        !plane
            .registry()
            .convergence(plane.store(), NOW + 5_000)
            .not_converged
            .is_empty(),
        "applied_version != assignment_version → 不收敛"
    );
    assert!(
        !plane.registry().repush_due("col-1", NOW + 5_000),
        "未到 09 §4.4 的 30s 超时"
    );

    // ② 仍未收敛，但没到 30s → 依然不重推
    let out = heartbeat(&mut plane, "col-1", 0, NOW + 20_000);
    assert!(out.is_empty());
    assert!(!plane.registry().repush_due("col-1", NOW + 20_000));

    // ③ 到 30s → 重推：assignment_version 递增（09 §4.4 / §5.4）
    let out = heartbeat(&mut plane, "col-1", 0, NOW + 35_000);
    let repushed = plan_of(&out, "col-1").expect("不收敛超时应重推");
    assert_eq!(repushed.assignment_version, 2, "重推 = 新版本整包下发");
    assert_eq!(repushed.mode, PlanMode::Snapshot);
    assert_eq!(
        plane.registry().get("col-1").unwrap().last_assignment_version,
        Some(2)
    );
    assert!(!plane.registry().repush_due("col-1", NOW + 35_000), "同一窗口不重复重推");

    // ④ 应用到位 → 收敛，不再重推
    let out = heartbeat(&mut plane, "col-1", 2, NOW + 40_000);
    assert!(out.is_empty(), "已收敛不再重推");
    assert!(
        plane.registry()
            .convergence(plane.store(), NOW + 40_000)
            .not_converged
            .is_empty()
    );
    assert!(plane.registry().get("col-1").unwrap().unconverged_since.is_none());
}

#[test]
fn lost_instance_does_not_trigger_reassignment() {
    let dir = temp_store("no-reassign");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();
    plane
        .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1", "i")
        .unwrap();

    let out = register_collector(&mut plane, "col-1", NOW);
    let before = plan_of(&out, "col-1").expect("初次指派");
    let _ = register_collector(&mut plane, "col-2", NOW);
    let shards_before = serde_json::to_value(&before.shards).unwrap();

    // 判死（3×5s 无心跳）
    let dead = plane.sweep(NOW + 15_000).unwrap();
    assert_eq!(dead.len(), 2, "两个实例都失联判死");
    assert_eq!(
        plane.registry().get("col-1").unwrap().status,
        InstStatus::Dead
    );
    assert_eq!(plane.session_count(), 0, "判死后关闭该实例主题会话");

    // 09 §7：失联只标记状态，**不改任何分片归属**、不下发
    let after = plane.plan_of("col-1").expect("计划记录不被删除");
    assert_eq!(
        after.assignment_version,
        before.assignment_version,
        "assignment_version 不因失联而变化"
    );
    assert_eq!(
        serde_json::to_value(&after.shards).unwrap(),
        shards_before,
        "分片归属不因失联而改变"
    );

    let out = plane.recompute().unwrap();
    assert!(out.is_empty(), "候选为空 → 一次都不下发");
    assert_eq!(
        plane.plan_of("col-1").unwrap().assignment_version,
        before.assignment_version
    );
}

#[test]
fn re_register_after_death_keeps_assignment_version_monotonic() {
    let dir = temp_store("reregister");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();
    plane
        .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1", "i")
        .unwrap();

    let out = register_collector(&mut plane, "col-1", NOW);
    let p1 = plan_of(&out, "col-1").expect("col-1 初次计划");
    assert_eq!(p1.assignment_version, 1);
    let out = register_collector(&mut plane, "col-2", NOW);
    let p2 = plan_of(&out, "col-2").expect("col-2 初次计划");
    assert_eq!(p2.assignment_version, 1);

    // 全部失联 → 再启动 register（09 §5.4：恢复由实例重启 register 触发）
    let dead = plane.sweep(NOW + 15_000).unwrap();
    assert_eq!(dead.len(), 2);

    // col-2 重启：接住失联方腾出的分片 → 必须是**递增**的新版本，
    // 而不是从 1 重来被自己的强序门丢弃（10 §4.3）
    let out = register_collector(&mut plane, "col-2", NOW + 30_000);
    let show: Vec<(String, u64)> = out.iter().filter_map(|o| match &o.envelope.body {
        ponk_protocol::Body::Ctl(ponk_protocol::CtlMessage::SubscribePlan(p)) =>
            Some((o.topic.as_str().to_string(), p.assignment_version)), _ => None,
    }).collect();
    eprintln!("REREG OUT msgs={} plan_topics={show:?}", out.len());
    eprintln!("REREG registry col-2 last_ver={:?} status={:?}", plane.registry().get("col-2").map(|k| k.last_assignment_version), plane.registry().get("col-2").map(|k| k.status));
    let again = plan_of(&out, "col-2").expect("重启后应仍能收到计划");
    assert_eq!(
        again.assignment_version, 2,
        "assignment_version 跨失联/重启保持单调"
    );
    assert_eq!(
        plane.registry().get("col-2").unwrap().last_assignment_version,
        Some(2)
    );
}

#[test]
fn heartbeat_updates_the_snapshot_key() {
    let dir = temp_store("hb-key");
    let _c = Cleanup::new(dir.clone());
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg(&dir)).unwrap();
    let _ = heartbeat(&mut plane, "col-9", 42, NOW); // 未 register 的心跳也要落快照键

    let key = plane.registry().get("col-9").expect("心跳写入快照键");
    assert_eq!(key.status, InstStatus::Running);
    assert_eq!(key.applied_version, Some(42));
    assert_eq!(key.last_seen_ts, NOW);
    assert_eq!(key.node_kind, NodeKind::Collector);
}
