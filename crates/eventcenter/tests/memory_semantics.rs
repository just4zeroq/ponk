//! 03 §3 / §4.4 / §4.5 / §7.3 在内存 backend 上的语义测试（必测项 ③④）。

use ponk_eventcenter::{
    Capabilities, DeadLetterReader, ECError, EventCenter, MemoryEventCenter, MemoryOptions,
    Retention, Session, SessionOpts, StartPosition,
};
use ponk_protocol::{
    Body, Direction, Envelope, EventType, Market, MarketBody, MarketData, SignalEvent, Topic, Venue,
};
use std::collections::BTreeMap;
use std::time::Duration;

const TOPIC: &str = "md.trade";
const SOURCE: &str = "collect-1";

fn md_env(seq: u64, shard: Option<&str>) -> Envelope {
    let native = shard.unwrap_or("BTCUSDT").to_string();
    let body = Body::Market(MarketBody {
        venue: Venue::binance(),
        market: Market::swap(),
        native_code: native.clone(),
        exchange_ts: seq,
        data: MarketData::Ticker(digdigdig3_core::core::types::Ticker {
            last_price: seq as f64,
            timestamp: seq as i64,
            ..Default::default()
        }),
    });
    let mut env = Envelope::new(EventType::Market, SOURCE, seq, body).unwrap();
    // 测试里手动钉住 shard，避免依赖哈希分布
    env.shard = shard.map(|s| s.to_string());
    env
}

fn opts(start: StartPosition) -> SessionOpts {
    SessionOpts {
        timeout: Duration::from_millis(50),
        start,
    }
}

fn read_n(session: &mut dyn Session, n: usize, timeout: Duration) -> Vec<Envelope> {
    let deadline = std::time::Instant::now() + timeout;
    let mut out = Vec::new();
    while out.len() < n && std::time::Instant::now() < deadline {
        match session.next(Duration::from_millis(50)) {
            Ok(Some(e)) => out.push(e),
            Ok(None) => continue,
            Err(e) => panic!("next 出错：{e}"),
        }
    }
    out
}

/// ③ 消费组推进 + 无 ack：读取即推进位点，不重复投递、不漏投。
#[test]
fn group_advances_on_read_without_ack() {
    let ec = MemoryEventCenter::new();
    let topic = Topic::new(TOPIC);
    for i in 0..5 {
        ec.publish(&topic, &md_env(i, None)).unwrap();
    }

    let mut s = ec
        .open(&topic, &"g".into(), opts(StartPosition::Earliest))
        .expect("open");

    // 逐条读取：没有任何 ack 调用，位点必须已推进
    let mut seen = Vec::new();
    for _ in 0..5 {
        let e = s.next(Duration::from_millis(50)).unwrap().expect("应有消息");
        assert_eq!(e.received_ts, seen.len() as u64);
        seen.push(e.received_ts);
    }
    assert_eq!(seen, (0..5).collect::<Vec<u64>>(), "顺序必须与写入序一致");

    // 第 6 次：超时无消息（非结束）
    assert!(s.next(Duration::from_millis(30)).unwrap().is_none());

    // 再读仍然从位点继续，不会把已读的重投一遍
    ec.publish(&topic, &md_env(99, None)).unwrap();
    let e = s.next(Duration::from_millis(100)).unwrap().expect("新消息");
    assert_eq!(e.received_ts, 99, "无 ack：位点不得回退");

    s.close().unwrap();
    assert!(matches!(s.next(Duration::from_millis(10)), Err(ECError::Closed)));
}

/// ③ 同一消费组的多个会话共享同一位点（竞争消费，互不重复）。
#[test]
fn sessions_in_same_group_share_cursor() {
    let ec = MemoryEventCenter::new();
    let topic = Topic::new(TOPIC);
    let group = "workers".to_string();

    let mut a = ec.open(&topic, &group.clone().into(), opts(StartPosition::Earliest)).unwrap();
    let mut b = ec.open(&topic, &group.clone().into(), opts(StartPosition::Earliest)).unwrap();

    for i in 0..6 {
        ec.publish(&topic, &md_env(i, None)).unwrap();
    }

    let mut seen = Vec::new();
    for _ in 0..6 {
        // 交替拉取：两个会话分摊同一位点
        for s in [&mut *a as &mut dyn Session, &mut *b as &mut dyn Session] {
            if seen.len() >= 6 {
                break;
            }
            if let Some(e) = s.next(Duration::from_millis(20)).unwrap() {
                seen.push(e.received_ts);
            }
        }
    }
    seen.sort_unstable();
    assert_eq!(seen, (0..6).collect::<Vec<u64>>(), "同组内不得重复也不得漏投");
}

/// ③ 消费组互相独立：各自推进、互不干扰（03 §3）。
#[test]
fn groups_are_independent() {
    let ec = MemoryEventCenter::new();
    let topic = Topic::new(TOPIC);

    // 先写入 3 条历史
    for i in 0..3 {
        ec.publish(&topic, &md_env(i, None)).unwrap();
    }

    // 组 A：Latest 起读 → 跳过历史；组 B：Earliest → 看到全部历史
    let mut a = ec.open(&topic, &"a".into(), opts(StartPosition::Latest)).unwrap();
    let mut b = ec
        .open(&topic, &"b".into(), opts(StartPosition::Earliest))
        .unwrap();

    ec.publish(&topic, &md_env(7, None)).unwrap();

    let got_a = read_n(&mut *a, 1, Duration::from_secs(2));
    assert_eq!(got_a.len(), 1, "Latest 起读应跳过历史");
    assert_eq!(got_a[0].received_ts, 7);

    let got_b = read_n(&mut *b, 4, Duration::from_secs(2));
    assert_eq!(
        got_b.iter().map(|e| e.received_ts).collect::<Vec<_>>(),
        vec![0, 1, 2, 7],
        "Earliest 起读应看到全部历史，且不受组 A 推进影响"
    );

    // 两个组都已读完：互不干扰地各自到达位点末端
    assert!(a.next(Duration::from_millis(20)).unwrap().is_none());
    assert!(b.next(Duration::from_millis(20)).unwrap().is_none());
}

/// 03 §4.6：指定偏移起读。
#[test]
fn start_at_offset() {
    let ec = MemoryEventCenter::new();
    let topic = Topic::new(TOPIC);
    for i in 0..4 {
        ec.publish(&topic, &md_env(i, None)).unwrap();
    }
    let mut s = ec
        .open(&topic, &"off".into(), opts(StartPosition::Offset(2)))
        .unwrap();
    let got = read_n(&mut *s, 2, Duration::from_secs(2));
    assert_eq!(
        got.iter().map(|e| e.received_ts).collect::<Vec<_>>(),
        vec![2, 3],
        "Offset(2) 应从序号 2 起读"
    );
}

/// 03 §8：分片键决定分区；同分片内尽力有序，跨分片各走各的分区。
#[test]
fn shard_maps_to_partition_and_keeps_order() {
    let ec = MemoryEventCenter::with_options(MemoryOptions {
        partitions: 4,
        max_len: None,
    });
    let topic = Topic::new(TOPIC);

    // 三个分片交错写入
    let shards = ["alpha", "beta", "gamma"];
    for i in 0..9 {
        ec.publish(&topic, &md_env(i, Some(shards[(i % 3) as usize]))).unwrap();
    }

    assert_eq!(ec.partition_count(&topic), 4, "分区数应可配（03 §4.5）");
    assert_eq!(ec.retained_len(&topic), 9);

    let mut s = ec.open(&topic, &"shard".into(), opts(StartPosition::Earliest)).unwrap();
    let got = read_n(&mut *s, 9, Duration::from_secs(3));

    let mut per_shard: std::collections::BTreeMap<String, Vec<u64>> = Default::default();
    for e in &got {
        let key = e.shard.clone().expect("行情事件必有 shard");
        per_shard.entry(key).or_default().push(e.received_ts);
    }
    assert_eq!(per_shard.len(), 3, "三个分片都应被消费到");
    for (shard, mut seqs) in per_shard {
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        assert_eq!(seqs, sorted, "同一分片内必须保持写入序：{shard}");
        assert_eq!(seqs.len(), 3, "{shard} 应收到全部 3 条");
        seqs.clear();
    }

    // 无分片键的事件进默认分区（分区 0）
    ec.publish(&topic, &md_env(100, None)).unwrap();
    let mut none_shard = ec
        .open(&topic, &"default".into(), opts(StartPosition::Earliest))
        .unwrap();
    let all = read_n(&mut *none_shard, 10, Duration::from_secs(3));
    assert!(all.iter().any(|e| e.shard.is_none() && e.received_ts == 100));
}

/// 03 §4.5：内存 backend 能力表 `measured = true`，且各能力可被实测核对。
#[test]
fn capabilities_are_measured() {
    let ec = MemoryEventCenter::with_options(MemoryOptions {
        partitions: 8,
        max_len: Some(3),
    });
    let caps: Capabilities = ec.capabilities();
    assert!(caps.measured, "03 §4.5：能力取值来自实测");
    assert_eq!(caps.backend, "memory");
    assert!(caps.partitioning, "partitioning 能力");
    assert_eq!(caps.partitions, Some(8), "可配分区数");
    assert!(caps.replay, "replay = 按偏移起读");
    assert!(caps.dead_letter, "dead_letter 能力");
    assert_eq!(caps.retention, Retention::MaxLen(3));

    // retention 实测：只保留 max_len 条
    let topic = Topic::new(TOPIC);
    for i in 0..10 {
        ec.publish(&topic, &md_env(i, None)).unwrap();
    }
    assert_eq!(ec.retained_len(&topic), 3, "MaxLen 截断必须真的生效");
}

/// 03 §7.3 ④：发布失败 → 写入 `dlq.<topic>`，携带 `DeadLetterMeta`；调用方仍收到错误。
#[test]
fn publish_failure_writes_dead_letter() {
    let ec = MemoryEventCenter::new();
    let topic = Topic::new(TOPIC);
    let env = md_env(1, None);

    ec.inject_publish_failure(&topic);
    let err = ec.publish(&topic, &env).expect_err("注入的失败必须返回错误");
    assert!(matches!(err, ECError::Publish(_)), "{err:?}");

    let dead = ec.dead_letters(&topic, 100).expect("读取死信");
    assert_eq!(dead.len(), 1, "失败必须进 DLQ");
    let dl = &dead[0];
    assert_eq!(dl.origin_topic, topic);
    assert_eq!(dl.attempts, 1);
    assert!(dl.error.contains("injected failure"), "{}", dl.error);
    assert!(dl.failed_ts > 0);
    // 原负载完好保留
    assert_eq!(
        serde_json::to_string(&*dl.body).unwrap(),
        serde_json::to_string(&env).unwrap()
    );

    // 03 §4.1 `limit` 语义：0 条 = 空；1 条 = 截断；超量 = 全量
    assert!(ec.dead_letters(&topic, 0).unwrap().is_empty(), "limit=0 应为空");
    assert_eq!(ec.dead_letters(&topic, 1).unwrap().len(), 1);
    assert_eq!(ec.dead_letters(&topic, 999).unwrap().len(), 1);

    // 故障恢复后可正常发布，且不再新增死信
    ec.clear_publish_failure(&topic);
    ec.publish(&topic, &env).expect("故障恢复后应成功");
    assert_eq!(ec.dead_letters(&topic, 100).unwrap().len(), 1);
}

/// 03 §5.3：**黑名单不进 `InstrumentMeta.enabled`** —— 黑名单是独立配置域
/// （09 §2 `InstrumentBlocklist`），指派时已剔除；`enabled` 只是采集侧运行时停用开关。
#[test]
fn blocklist_is_a_separate_domain_not_instrument_meta_enabled() {
    use ponk_protocol::{ConfigDomain, InstrumentMeta, Channel};

    let meta = InstrumentMeta {
        native_code: "BTCUSDT".into(),
        price_precision: 1,
        qty_precision: 3,
        channels: vec![Channel::candle_1m()],
        enabled: false, // 运行时停用，与黑名单无关
        meta: Default::default(),
    };
    let json = serde_json::to_value(&meta).unwrap();
    let keys: Vec<&str> = json.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "native_code",
            "price_precision",
            "qty_precision",
            "channels",
            "enabled",
            "meta"
        ],
        "InstrumentMeta 不得出现黑名单字段"
    );
    assert_eq!(json["enabled"], false);
    assert!(!keys.iter().any(|k| k.contains("block") || k.contains("black")));

    // 黑名单走独立域：主题与心跳键都是它自己的
    assert_eq!(ConfigDomain::InstrumentBlocklist.topic(), "ctl.instrument_blocklist");
    assert_eq!(
        ConfigDomain::InstrumentBlocklist.counter_key(),
        "cfg.instrument_blocklist"
    );

    // `enabled` 缺省为 true（不是黑名单）
    let defaulted: InstrumentMeta = serde_json::from_str(
        r#"{"native_code":"X","price_precision":1,"qty_precision":1,"channels":[]}"#,
    )
    .unwrap();
    assert!(defaulted.enabled);
}

/// 03 §5.2 / §8：私有主题（不属于任何注册族）→ 发布失败并进 DLQ。
#[test]
fn private_topic_is_rejected_and_dead_lettered() {
    let ec = MemoryEventCenter::new();
    let topic = Topic::new("secret.channel");
    let err = ec.publish(&topic, &md_env(1, None)).expect_err("私有主题必须拒绝");
    assert!(matches!(err, ECError::InvalidTopic(_)), "{err:?}");

    let dead = ec.dead_letters(&topic, 100).unwrap();
    assert_eq!(dead.len(), 1, "被拒的事件同样进 DLQ（03 §7.3）");
    assert_eq!(dead[0].origin_topic, topic);
}

/// 03 §7.4：死信只堆积、不经 `Session` 读取，由运维侧 `DeadLetterReader` 查看。
#[test]
fn dead_letters_are_not_readable_via_session() {
    let ec = MemoryEventCenter::new();
    let topic = Topic::new(TOPIC);
    let dlq_topic = topic.to_dead_letter();

    let err = ec
        .open(&dlq_topic, &"ops".into(), SessionOpts::default())
        .err()
        .expect("dlq 主题不应能开 Session");
    assert!(matches!(err, ECError::Unsupported(_)), "{err:?}");

    ec.inject_publish_failure(&topic);
    let _ = ec.publish(&topic, &md_env(1, None));
    assert_eq!(ec.dead_letters(&topic, 100).unwrap().len(), 1);
}

// ── 小工具 ──────────────────────────────────────────────────────────────────

/// 03 §9-8：`md.meta` 走完 发布 → 消费 全链路（09 §2.3 定时任务的产物）。
#[test]
fn md_meta_events_flow_end_to_end() {
    use ponk_protocol::{Channel, CurrencyInfo, MetaPayload, SymbolInfo};

    let ec = MemoryEventCenter::new();
    let topic = Topic::new(&Channel::meta().topic());
    assert_eq!(topic.as_str(), "md.meta");

    let body = Body::Market(MarketBody {
        venue: Venue::binance(),
        market: Market::swap(),
        native_code: "BTCUSDT".into(),
        exchange_ts: 1_772_000_000_000,
        data: MarketData::Meta(MetaPayload {
            symbol: SymbolInfo {
                symbol: "BTCUSDT".into(),
                base_asset: "BTC".into(),
                quote_asset: "USDT".into(),
                status: "TRADING".into(),
                price_precision: 1,
                quantity_precision: 3,
                ..Default::default()
            },
            currencies: vec![CurrencyInfo::new("USDT")],
        }),
    });
    let env = Envelope::new(EventType::Market, "control-1", 1_772_000_000_000, body).unwrap();
    assert_eq!(env.x_version, None, "md.meta 不是 K 线，不带 x_version");

    ec.publish(&topic, &env).expect("md.meta 发布");
    assert_eq!(ec.retained_len(&topic), 1, "主题在首次写入时自动创建（03 §7.4）");

    let mut s = ec.open(&topic, &"storage".into(), opts(StartPosition::Earliest)).unwrap();
    let got = read_n(&mut *s, 1, Duration::from_secs(2));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].shard.as_deref(), Some("binance:swap:BTCUSDT"), "行情族按 instrument 分片");
    assert_eq!(serde_json::to_string(&got[0]).unwrap(), serde_json::to_string(&env).unwrap());
}

/// 顺带验证信封构造器对信号家族的默认分片（03 §8）。
#[test]
fn signal_events_carry_instrument_shard() {
    let body = Body::Signal(SignalEvent {
        signal_id: "s".into(),
        signal_name: "n".into(),
        version: "v1".into(),
        instrument: "okx:swap:BTC-USDT".into(),
        direction: Direction::Long,
        value: 1.0,
        timeframe: "1m".into(),
        timestamp: 42,
        payload: BTreeMap::new(),
    });
    let env = Envelope::new(EventType::Signal, "signal-1", 42, body).unwrap();
    assert_eq!(env.shard.as_deref(), Some("okx:swap:BTC-USDT"));
    assert_eq!(env.x_version, None, "x_version 仅 K 线需要（03 §5.4）");
}
