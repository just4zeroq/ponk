//! 03 §9-6 ①：`Body` 各变体的 serde 往返；以及 03 §5.1 的按 `event_type` 分派。

mod common;

use common::*;
use ponk_protocol::*;

fn json_roundtrip(env: &Envelope) -> Envelope {
    let bytes = JsonCodec.serialize_envelope(env).expect("json serialize");
    JsonCodec
        .deserialize_envelope(&bytes)
        .expect("json deserialize")
}

fn msgpack_roundtrip(env: &Envelope) -> Envelope {
    let bytes = MsgPackCodec.serialize_envelope(env).expect("msgpack serialize");
    MsgPackCodec
        .deserialize_envelope(&bytes)
        .expect("msgpack deserialize")
}

/// 03 §9-6 ①：`Body` 全部变体（含 `CtlMessage` 三个变体）的 serde 往返。
///
/// 比较口径 = 重新序列化后的 JSON 文本，能同时覆盖结构、字段值与字段归属。
#[test]
fn every_body_variant_roundtrips_json() {
    for body in all_bodies() {
        let env = envelope(body);
        let expected = serde_json::to_string(&env).unwrap();
        let back = json_roundtrip(&env);
        let actual = serde_json::to_string(&back).unwrap();
        assert_eq!(expected, actual, "JSON 往返不一致");
        // 分派正确：event_type 与 body 家族仍匹配
        back.validate().unwrap();
        assert_eq!(back.event_type, env.event_type);
        assert_eq!(back.body.kind(), env.body.kind());
        assert_eq!(back.shard, env.shard);
        assert_eq!(back.event_id, env.event_id);
    }
}

/// 03 §6：MessagePack 同样无损往返，且与 JSON 表达同一份数据。
#[test]
fn every_body_variant_roundtrips_msgpack() {
    for body in all_bodies() {
        let env = envelope(body);
        let via_msgpack = msgpack_roundtrip(&env);
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            serde_json::to_string(&via_msgpack).unwrap(),
            "MessagePack 往返与 JSON 不等价"
        );
        assert_eq!(
            JsonCodec.format(),
            Format::Json,
            "默认格式必须是 JSON（03 §6）"
        );
        assert_eq!(default_codec().format(), Format::Json);
    }
}

/// 03 §5.1：反序列化按 `event_type` 分派到对应变体。
#[test]
fn dispatch_follows_event_type() {
    let cases = [
        (EventType::Market, kline_body()),
        (EventType::Market, meta_body()),
        (EventType::Signal, signal_body()),
        (EventType::Factor, factor_body()),
        (EventType::Alarm, alarm_body()),
        (EventType::Ops, ops_body()),
        (EventType::Ctl, ctl_plan_body()),
        (EventType::Ctl, ctl_snapshot_body()),
    ];
    for (expected, body) in cases {
        let env = envelope(body);
        let back = json_roundtrip(&env);
        assert_eq!(back.event_type, expected);
        assert_eq!(back.body.kind(), expected.body_kind());
    }
}

/// 03 §5.1：`event_type` 与 `body` 不同家族 → 拒绝。
#[test]
fn mismatched_event_type_and_body_is_rejected() {
    let err = Envelope::new(EventType::Signal, SOURCE, TS, ops_body())
        .err()
        .expect("必须拒绝 event_type/body 不匹配");
    assert!(matches!(err, ProtocolError::TypeMismatch { .. }), "{err:?}");

    // 线上字节同样拒绝：分派按 `event_type` 走，信号体解析成 OpsHeartbeat 必然失败
    let env = envelope(signal_body());
    let mut json: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&env).unwrap()).unwrap();
    json["event_type"] = serde_json::json!("ops");
    let err = JsonCodec
        .deserialize_envelope(serde_json::to_string(&json).unwrap().as_bytes())
        .err()
        .expect("必须拒绝");
    assert!(matches!(err, ProtocolError::Deserialize(_)), "{err:?}");

    // 信封自带 event_id 不变也不豁免
    assert!(Envelope::from_parts(
        EventType::Ops,
        env.shard.clone(),
        env.event_id.clone(),
        env.received_ts,
        env.x_version,
        env.source.clone(),
        signal_body(),
    )
    .is_err());
}

/// 03 §5.2：`dlq` 不由 `Envelope` 承载。
#[test]
fn dlq_event_type_is_not_envelope_carried() {
    let err = Envelope::new(EventType::DeadLetter, SOURCE, TS, signal_body())
        .err()
        .expect("必须拒绝");
    assert!(matches!(err, ProtocolError::Invalid(_)), "{err:?}");
}

/// 03 §5.1：`body` 必须排在 `event_type` 之后（否则无法分派）。
#[test]
fn body_before_event_type_is_rejected() {
    let bytes = br#"{"body":{"signal_id":"x"},"event_type":"signal"}"#;
    let err = JsonCodec
        .deserialize_envelope(bytes)
        .err()
        .expect("必须拒绝");
    assert!(format!("{err}").contains("event_type"), "{err}");
}

/// 03 §9-8：`md.meta` 负载 = dig3 `SymbolInfo` + 平台 `CurrencyInfo`，两种格式往返；
/// 且 `CurrencyInfo.extra` / `SymbolInfo.extra` 的原始 JSON **原样保留**（03 §5.5 例外）。
#[test]
fn meta_variant_roundtrips_and_keeps_raw_passthrough() {
    let env = envelope(meta_body());

    // 负载形状：body.data = {"meta": {"symbol": …, "currencies": […]}}
    // （`MarketData` 外部标签，snake_case → `meta`，不是默认的 `Meta`）
    let json = serde_json::to_value(&env).unwrap();
    assert!(json["body"]["data"].get("meta").is_some(), "变体标签应为 meta");
    assert!(json["body"]["data"].get("Meta").is_none(), "不得是默认驼峰标签");
    let meta = &json["body"]["data"]["meta"];
    assert_eq!(meta["symbol"]["status"], "TRADING");
    assert_eq!(meta["symbol"]["price_precision"], 1);
    assert_eq!(meta["symbol"]["instrument_type"], "PERPETUAL");
    assert_eq!(meta["currencies"][0]["currency"], "USDT");
    assert_eq!(meta["currencies"][0]["networks"], serde_json::json!(["TRC20", "ERC20"]));
    // 两处原始透传都不被类型化（03 §5.5 例外）
    assert_eq!(meta["currencies"][0]["extra"]["canWithdraw"], true);
    assert_eq!(
        meta["symbol"]["extra"]["lotSizeFilter"]["minQty"],
        "0.001"
    );

    let back_json = json_roundtrip(&env);
    assert_eq!(
        serde_json::to_string(&back_json).unwrap(),
        serde_json::to_string(&env).unwrap()
    );

    let back_mp = msgpack_roundtrip(&env);
    assert_eq!(
        serde_json::to_string(&back_mp).unwrap(),
        serde_json::to_string(&env).unwrap(),
        "MessagePack 往返不得丢失 extra 原始 JSON"
    );

    // 分派正确
    assert_eq!(back_json.event_type, EventType::Market);
    assert_eq!(back_json.body.kind(), BodyKind::Market);
    match &back_json.body {
        Body::Market(m) => assert!(matches!(m.data, MarketData::Meta(_))),
        other => panic!("应分派到 Market::Meta，实际 {other:?}"),
    }
}

/// 03 §9-8 / §8：`md.meta` 主题可用（`Channel::meta()`）。
#[test]
fn meta_channel_topic_is_md_meta() {
    use ponk_protocol::Channel;
    assert_eq!(Channel::META, "meta");
    assert_eq!(Channel::meta().topic(), "md.meta");
    assert!(Topic::new(&Channel::meta().topic()).family_prefix() == Some("md."));
}

/// `md.meta` 不是 K 线，**不得**被赋予 `x_version`（03 §5.4）。
#[test]
fn meta_has_no_x_version() {
    assert_eq!(envelope(meta_body()).x_version, None);
}

/// 03 §7.3：死信负载 `DeadLetterMeta`（内含原 envelope）两种格式往返。
#[test]
fn dead_letter_roundtrips() {
    let origin = Topic::new("md.candle.1m");
    let dl = DeadLetterMeta {
        origin_topic: origin.clone(),
        error: "backend write failed: connection refused".into(),
        attempts: 3,
        failed_ts: TS,
        body: Box::new(envelope(kline_body())),
    };

    let json = JsonCodec.serialize_dead_letter(&dl).unwrap();
    let back_json = JsonCodec.deserialize_dead_letter(&json).unwrap();
    assert_eq!(back_json.origin_topic, dl.origin_topic);
    assert_eq!(back_json.error, dl.error);
    assert_eq!(back_json.attempts, 3);
    assert_eq!(back_json.failed_ts, dl.failed_ts);
    assert_eq!(
        serde_json::to_string(&*back_json.body).unwrap(),
        serde_json::to_string(&*dl.body).unwrap()
    );

    let mp = MsgPackCodec.serialize_dead_letter(&dl).unwrap();
    let back_mp = MsgPackCodec.deserialize_dead_letter(&mp).unwrap();
    assert_eq!(
        serde_json::to_string(&back_mp).unwrap(),
        serde_json::to_string(&dl).unwrap()
    );
}

/// 03 §6：`ctl.*` / `ops.*` 固定 JSON，不开放切换（09 §2 Serialization 域）；
/// `md.*` 等可按主题族配置；`dlq.<topic>` 跟随原主题族。
#[test]
fn format_policy_pinned_families_and_configurable_ones() {
    let mut policy = FormatPolicy::default();
    policy.overrides.insert("md.".into(), Format::MessagePack);
    policy.overrides.insert("signal.".into(), Format::MessagePack);
    // 即便写入也必须被忽略
    policy.overrides.insert("ctl.".into(), Format::MessagePack);
    policy.overrides.insert("ops.".into(), Format::MessagePack);

    assert_eq!(policy.format_for_topic("md.candle.1m"), Format::MessagePack);
    assert_eq!(policy.format_for_topic("signal.rule_x"), Format::MessagePack);
    assert_eq!(policy.format_for_topic("factor.rule_x"), Format::Json);
    assert_eq!(policy.format_for_topic("ctl.collector.c1"), Format::Json);
    assert_eq!(policy.format_for_topic("ctl.signal_rules"), Format::Json);
    assert_eq!(policy.format_for_topic("ops.collector.c1"), Format::Json);
    // 死信跟随原主题族
    assert_eq!(policy.format_for_topic("dlq.md.candle.1m"), Format::MessagePack);
    assert_eq!(policy.format_for_topic("dlq.ctl.collector.c1"), Format::Json);
    // 默认全 JSON
    assert_eq!(FormatPolicy::default().format_for_topic("md.trade"), Format::Json);
}

/// 03 §5.1：公共头只有六个字段 + body，**不含** venue/market/native_code/exchange_ts。
#[test]
fn public_header_has_no_market_specific_fields() {
    let env = envelope(kline_body());
    let json = serde_json::to_value(&env).unwrap();
    let obj = json.as_object().unwrap();
    let keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "event_type",
            "shard",
            "event_id",
            "received_ts",
            "x_version",
            "source",
            "body"
        ]
    );
    for forbidden in ["venue", "market", "native_code", "exchange_ts"] {
        assert!(
            !obj.contains_key(forbidden),
            "公共头不得出现行情专属字段 `{forbidden}`（03 §5.1）"
        );
    }
    // 这些字段必须在 Body::Market 内
    let body = &json["body"];
    assert_eq!(body["venue"], "binance");
    assert_eq!(body["market"], "swap");
    assert_eq!(body["native_code"], "BTCUSDT");
    assert!(body.get("exchange_ts").is_some());
}

/// 03 §5.1：`shard` 预计算 —— 行情/信号/因子 = instrument_id，心跳/告警/配置 = None。
#[test]
fn shard_precomputed_per_family() {
    assert_eq!(
        envelope(kline_body()).shard.as_deref(),
        Some("binance:swap:BTCUSDT")
    );
    assert_eq!(envelope(signal_body()).shard.as_deref(), Some(INSTRUMENT));
    assert_eq!(envelope(factor_body()).shard.as_deref(), Some(INSTRUMENT));
    assert_eq!(envelope(alarm_body()).shard, None);
    assert_eq!(envelope(ops_body()).shard, None);
    assert_eq!(envelope(ctl_plan_body()).shard, None);
}

/// 03 §5.4：`x_version` 仅 K 线需要，取 `received_ts`。
#[test]
fn x_version_only_for_kline() {
    assert_eq!(envelope(kline_body()).x_version, Some(TS));
    assert_eq!(envelope(ticker_body()).x_version, None);
    assert_eq!(envelope(trade_body()).x_version, None);
    assert_eq!(envelope(signal_body()).x_version, None);
    assert_eq!(envelope(ops_body()).x_version, None);
}

/// 09 §4.4：心跳逐域收敛键 `cfg.<domain>`。
#[test]
fn config_domain_derived_names() {
    assert_eq!(ConfigDomain::ALL.len(), 8);
    assert_eq!(ConfigDomain::SignalRules.as_str(), "signal_rules");
    assert_eq!(ConfigDomain::SignalRules.topic(), "ctl.signal_rules");
    assert_eq!(ConfigDomain::SignalRules.counter_key(), "cfg.signal_rules");
    assert_eq!(
        ConfigDomain::Serialization.counter_key(),
        "cfg.serialization"
    );
    assert_eq!(
        ConfigDomain::InstrumentBlocklist.topic(),
        "ctl.instrument_blocklist"
    );
    assert_eq!(
        ConfigDomain::InstrumentBlocklist.counter_key(),
        "cfg.instrument_blocklist"
    );
    // 早期出现过的 blacklist 拼法仍可反序列化（兼容别名）
    let legacy: ConfigDomain = serde_json::from_str("\"blacklist\"").unwrap();
    assert_eq!(legacy, ConfigDomain::InstrumentBlocklist);
    // 线格式 = snake_case
    for d in ConfigDomain::ALL {
        let s = serde_json::to_string(&d).unwrap();
        assert_eq!(s, format!("\"{}\"", d.as_str()));
        let back: ConfigDomain = serde_json::from_str(&s).unwrap();
        assert_eq!(back, d);
    }
}
