//! 交付物 4 的端到端证据：**采集器 → `EventCenter::publish` → `md.*`**
//!
//! 离线（`MemoryEventCenter` 是 03 §7.1 的内存 backend，契约层语义完整），
//! 不需要 Redis，也不需要访问交易所。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use digdigdig3::core::types::{StreamEvent, Ticker};
use ponk_collect::config::{CollectorConfig, ConnectionKey};
use ponk_collect::sink::CenterSink;
use ponk_collect::source::MockSource;
use ponk_collect::{Collector, EventSink};
use ponk_eventcenter::{
    DeadLetterReader, EventCenter, MemoryEventCenter, SessionOpts, StartPosition,
};
use ponk_protocol::{Body, EventType, Group, Status, Topic};

const TOML: &str = r#"
instance_id = "collector-ec"
assignment_version = 11

[heartbeat]
interval_ms = 200

[reconnect]
silence_timeout_ms = 2000
poll_ms = 50
backoff_min_ms = 10

[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["ticker", "trade"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;

fn ticker_event() -> StreamEvent {
    StreamEvent::Ticker {
        symbol: "btcusdt".into(),
        ticker: Ticker {
            last_price: 64_231.5,
            bid_price: Some(64_231.0),
            ask_price: Some(64_232.0),
            timestamp: 1_700_000_000_000,
            ..Default::default()
        },
    }
}

async fn wait_until(timeout_ms: u64, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if cond() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition not met within {}ms",
            timeout_ms
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collector_publishes_md_events_through_event_center() {
    let center = MemoryEventCenter::new();
    let sink = Arc::new(CenterSink::new(center.clone()));

    let mut collector = Collector::new(CollectorConfig::from_toml(TOML).unwrap(), sink).unwrap();
    let binance = MockSource::new("binance", "spot");
    let mut srcs: HashMap<ConnectionKey, Arc<dyn ponk_collect::source::MarketSource>> =
        HashMap::new();
    srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
    collector.set_sources(srcs);

    let counters = collector.counters();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(collector.run(shutdown_rx));

    wait_until(3_000, || binance.subscribe_count() >= 1).await;
    binance.push(ticker_event());
    wait_until(3_000, || counters.published() >= 1).await;

    // ── 数据信道：md.ticker ────────────────────────────────────────────────
    let mut session = center
        .open(
            &Topic::new("md.ticker"),
            &Group::new("collector-test"),
            SessionOpts {
                timeout: Duration::from_millis(1_000),
                start: StartPosition::Earliest,
            },
        )
        .expect("open md.ticker session");
    let env = session
        .next(Duration::from_millis(2_000))
        .expect("next")
        .expect("应读到一条行情事件");
    session.close().unwrap();

    assert_eq!(env.event_type, EventType::Market);
    assert_eq!(env.shard.as_deref(), Some("binance:spot:BTCUSDT"));
    assert_eq!(env.x_version, None, "非 candle_1m 不带 x_version");
    assert!(env.event_id.starts_with("binance:spot:BTCUSDT:md:1700000000000:"), "{}", env.event_id);
    assert_eq!(env.source.as_str(), "collector-ec");
    match &env.body {
        Body::Market(m) => {
            assert_eq!(m.venue.as_str(), "binance");
            assert_eq!(m.market.as_str(), "spot");
            assert_eq!(m.native_code, "BTCUSDT");
            assert_eq!(m.exchange_ts, 1_700_000_000_000);
            match &m.data {
                ponk_protocol::payload::MarketData::Ticker(t) => {
                    assert_eq!(t.last_price, 64_231.5);
                    assert_eq!(t.bid_price, Some(64_231.0));
                    assert_eq!(t.ask_price, Some(64_232.0));
                }
                other => panic!("unexpected payload {other:?}"),
            }
        }
        other => panic!("unexpected body {other:?}"),
    }

    // ── 控制信道：ops.collector.<instance> ────────────────────────────────
    let mut ops = center
        .open(
            &Topic::new("ops.collector.collector-ec"),
            &Group::new("collector-test"),
            SessionOpts {
                timeout: Duration::from_millis(1_000),
                start: StartPosition::Earliest,
            },
        )
        .expect("open ops session");
    let hb = ops
        .next(Duration::from_millis(2_000))
        .expect("next")
        .expect("应读到 Startup 心跳");
    match &hb.body {
        Body::Ops(h) => {
            assert_eq!(h.node_kind, ponk_protocol::NodeKind::Collector);
            assert_eq!(h.status, Status::Startup);
            assert_eq!(h.applied_version, Some(11));
            assert!(h.counters.contains_key("send_backlog"));
        }
        other => panic!("unexpected body {other:?}"),
    }
    ops.close().unwrap();

    let _ = shutdown_tx.send(true);
    handle.await.unwrap().unwrap();
    assert!(
        counters.snapshot()["publish_failed"] == 0,
        "不应有发布失败：{:?}",
        counters.snapshot()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_failure_retries_then_surfaces_and_backend_writes_dead_letter() {
    // 10 §3：发送失败重试；03 §4.1/§7.3：最终失败由 backend 写 `dlq.<topic>`，
    // 调用方仍收到错误（采集器只计数，不自行拼装 DeadLetterMeta）。
    let center = MemoryEventCenter::new();
    let sink = CenterSink::new(center.clone());
    let topic = Topic::new("md.ticker");
    let env = {
        let sample = ponk_collect::payload::MarketSample {
            channel: ponk_collect::Channel::Ticker,
            venue: "binance".into(),
            market: "spot".into(),
            native_code: "BTCUSDT".into(),
            exchange_ts: 1,
            data: ponk_protocol::payload::MarketData::Ticker(Default::default()),
        };
        ponk_collect::envelope::market_envelope(&sample, 100, "collector-ec").unwrap()
    };

    // 注入故障 → 采集器视角是 Err；backend 已把这条写进 dlq.md.ticker
    center.inject_publish_failure(&topic);
    let err = sink.publish(&topic, &env).unwrap_err();
    let msg = err.to_string();
    assert!(!msg.is_empty());

    let dead = center.dead_letters(&topic, 16).expect("dead_letters");
    assert_eq!(dead.len(), 1, "03 §7.3：失败的事件应进 dlq.<topic>");
    assert_eq!(dead[0].origin_topic.as_str(), "md.ticker");
    assert_eq!(dead[0].body.event_id, env.event_id);

    // 故障解除后发布成功
    center.clear_publish_failure(&topic);
    sink.publish(&topic, &env).unwrap();
    assert_eq!(center.retained_len(&topic), 1);
}

#[test]
fn shipped_example_config_parses_and_resolves() {
    // 交付物 1：本地 TOML/JSON 订阅计划（结构对齐 03 §5.3 SubscribePlan/ShardPlan）。
    let cfg = CollectorConfig::load("config/collector.example.toml")
        .expect("config/collector.example.toml 应能加载");
    assert_eq!(cfg.instance_id, "collector-01");
    assert_eq!(cfg.mode, "snapshot");
    let plan = cfg.resolve().expect("计划应能解析");
    // 02 §4：一个进程承载多个 (venue, market) 分片，**可跨所**
    assert_eq!(plan.subscriptions.len(), 3);
    assert!(plan.subscriptions.contains_key(&ConnectionKey::new("binance", "spot")));
    assert!(plan.subscriptions.contains_key(&ConnectionKey::new("binance", "swap")));
    assert!(plan.subscriptions.contains_key(&ConnectionKey::new("okx", "spot")));
    // 全市场通道（ticker / l1）不分片 → shard_idx 必须为 0（02 §4）
    for shard in &cfg.shards {
        for c in &shard.channels {
            if c == "ticker" || c == "l1" {
                assert_eq!(shard.shard_idx, 0, "全市场通道不分片");
            }
        }
    }
}
