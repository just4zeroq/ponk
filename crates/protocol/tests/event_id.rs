//! 03 §5.4 ②：`event_id` 跨键序、跨格式一致性 —— 哈希在**规范化视图**上计算，
//! **与序列化格式无关**（硬不变量）。

mod common;

use common::*;
use ponk_protocol::*;
use serde_json::Value;

/// 由一份信封重算 `event_id`。
fn recomputed(env: &Envelope) -> String {
    env.compute_event_id().unwrap()
}

/// 硬不变量：`Envelope::new` 写入的 `event_id` == 按 03 §5.4 重算的值。
#[test]
fn envelope_event_id_matches_recomputation() {
    for body in all_bodies() {
        let env = envelope(body);
        assert_eq!(env.event_id, recomputed(&env), "信封自带 id 与重算不一致");
        assert!(env.verify_event_id().unwrap());
    }
}

/// 硬不变量：**跨格式** —— JSON / MessagePack 往返后 `event_id` 不变。
///
/// 03 §6 登记的两种已实现格式字节表示完全不同（JSON 文本 vs MsgPack 二进制），
/// 若哈希碰过线上字节，这里必然发散。
#[test]
fn event_id_is_format_independent() {
    for body in all_bodies() {
        let env = envelope(body);

        let json_bytes = JsonCodec.serialize_envelope(&env).unwrap();
        let from_json = JsonCodec.deserialize_envelope(&json_bytes).unwrap();

        let mp_bytes = MsgPackCodec.serialize_envelope(&env).unwrap();
        let from_mp = MsgPackCodec.deserialize_envelope(&mp_bytes).unwrap();

        // 两种格式确实产生了不同的字节
        assert_ne!(json_bytes, mp_bytes, "JSON 与 MsgPack 字节不应相同");
        assert!(!json_bytes.is_empty() && !mp_bytes.is_empty());

        assert_eq!(recomputed(&env), recomputed(&from_json), "JSON 改变了 event_id");
        assert_eq!(recomputed(&env), recomputed(&from_mp), "MsgPack 改变了 event_id");

        // 反序列化自带的 event_id 也保持一致（同一份内容 → 同一幂等键）
        assert_eq!(env.event_id, from_json.event_id);
        assert_eq!(env.event_id, from_mp.event_id);
    }
}

/// 硬不变量：**跨键序** —— 文本里字段顺序打乱，`event_id` 不变。
///
/// `serde_json` 开了 `preserve_order`，`Map` 保留文本插入序，
/// 因此这里**真的**把 body 的键序倒过来了；若规范化视图不做递归键排序，本测试会失败。
#[test]
fn event_id_is_key_order_independent() {
    for body in all_bodies() {
        let env = envelope(body);
        let original_text = serde_json::to_string(&env).unwrap();
        let original: Value = serde_json::from_str(&original_text).unwrap();

        // 把 body 的键序整体倒转（preserve_order 下顺序会保留）
        let mut reordered = original.clone();
        let obj = reordered.as_object_mut().unwrap();
        let body_value = obj.get("body").unwrap().clone();
        let body_obj = body_value.as_object().unwrap();
        let mut keys: Vec<String> = body_obj.keys().cloned().collect();
        keys.reverse();
        let mut new_body = serde_json::Map::new();
        for k in keys {
            new_body.insert(k.clone(), body_obj.get(&k).unwrap().clone());
        }
        obj.insert("body".into(), Value::Object(new_body));

        let reordered_text = serde_json::to_string(&reordered).unwrap();

        if body_obj.len() > 1 {
            assert_ne!(
                original_text, reordered_text,
                "测试前提：body 键序必须真的不同（preserve_order）"
            );
        }

        let back: Envelope = serde_json::from_str(&reordered_text).unwrap();
        assert_eq!(
            recomputed(&env),
            recomputed(&back),
            "键序改变导致 event_id 变化"
        );
    }
}

/// 规范化视图本身跨键序一致（数据树层，不经过 Envelope）。
#[test]
fn canonical_view_is_key_order_independent() {
    let a: Value = serde_json::from_str(
        r#"{"z":{"b":1,"a":[3,2,1]},"a":"x","m":{"y":null,"x":true}}"#,
    )
    .unwrap();
    let b: Value = serde_json::from_str(
        r#"{"m":{"x":true,"y":null},"a":"x","z":{"a":[3,2,1],"b":1}}"#,
    )
    .unwrap();
    // preserve_order：两棵树的键序确实不同
    assert_ne!(a.to_string(), b.to_string());
    assert_eq!(canonical_view(&a), canonical_view(&b));
    assert_eq!(hash_text(&canonical_view(&a)), hash_text(&canonical_view(&b)));
}

/// 03 §5.4：`event_id = 身份键 : event_type : 身份时间 : 内容哈希`。
#[test]
fn event_id_shape_follows_spec_formula() {
    let env = envelope(signal_body());
    let parts: Vec<&str> = env.event_id.split(':').collect();
    // 身份键 = instrument_id（三段，含两个冒号）+ event_type + 身份时间 + 64 位哈希
    assert_eq!(parts.len(), 3 + 3, "身份键三段 + 三段尾部: {parts:?}");
    assert_eq!(parts[0], "binance");
    assert_eq!(parts[1], "swap");
    assert_eq!(parts[2], "BTCUSDT");
    assert_eq!(parts[3], "signal");
    assert_eq!(parts[4], TS.to_string());
    let hash = parts[5];
    assert_eq!(hash.len(), 64, "SHA-256 十六进制");
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(hash, content_hash_of(&env.body).unwrap());
}

/// 03 §5.4 身份键按家族取值。
#[test]
fn identity_key_per_family() {
    assert_eq!(
        envelope(kline_body()).identity_key(),
        "binance:swap:BTCUSDT"
    );
    assert_eq!(envelope(signal_body()).identity_key(), INSTRUMENT);
    assert_eq!(envelope(factor_body()).identity_key(), INSTRUMENT);
    // 告警 = 规则与触发源
    assert_eq!(
        envelope(alarm_body()).identity_key(),
        "btc_4h_rsi_overbought|btc_4h_momentum"
    );
    // 心跳 = instance_id
    assert_eq!(envelope(ops_body()).identity_key(), "collector-1");
    // 命令 = assignment_version / domain+version
    assert_eq!(envelope(ctl_plan_body()).identity_key(), "12");
    assert_eq!(
        envelope(ctl_changed_body()).identity_key(),
        "signal_rules+7"
    );
    assert_eq!(
        envelope(ctl_snapshot_body()).identity_key(),
        "alert_rules+21"
    );
}

/// 03 §5.4 / 02 §5：K 线的身份时间对齐 **bar 起点**（不是交易所推送时间）。
#[test]
fn kline_identity_time_is_bar_start() {
    let mut body = kline_body();
    if let Body::Market(ref mut m) = body {
        if let MarketData::Kline(ref mut k) = m.data {
            k.open_time = 1_772_000_000_000;
            // 交易所事件时间晚于 bar 起点（同一根 bar 的后续更新）
            m.exchange_ts = 1_772_000_059_999;
        }
    }
    let env = envelope(body);
    assert_eq!(env.identity_time(), 1_772_000_000_000);
}

/// 03 §5.4：内容敏感 —— 同身份 + 同内容 → 同 `event_id`（可去重）；
/// 内容变 → `event_id` 变（upsert 取代）。
#[test]
fn event_id_is_content_sensitive() {
    let base = envelope(signal_body());

    // 同内容、不同观测头（received_ts / source / shard 变）→ id 不变
    let mut same = signal_body();
    if let Body::Signal(ref mut s) = same {
        s.payload.insert("rsi".to_string(), 71.2);
    }
    let mut env_same = Envelope::new(EventType::Signal, "collect-2", TS + 1, same).unwrap();
    env_same.shard = base.shard.clone();
    assert_eq!(
        base.event_id,
        env_same.event_id,
        "信封头（received_ts/source）不得影响 event_id"
    );

    // 内容变（告警可引用的因子值）→ id 变
    let mut changed = signal_body();
    if let Body::Signal(ref mut s) = changed {
        s.payload.insert("rsi".to_string(), 71.3);
    }
    let env_changed = Envelope::new(EventType::Signal, SOURCE, TS, changed).unwrap();
    assert_ne!(base.event_id, env_changed.event_id);

    // 身份变 → id 变
    let mut other = signal_body();
    if let Body::Signal(ref mut s) = other {
        s.instrument = "okx:swap:BTC-USDT".into();
    }
    let env_other = Envelope::new(EventType::Signal, SOURCE, TS, other).unwrap();
    assert_ne!(base.event_id, env_other.event_id);
}

/// 03 §9-8 + §5.4：`md.meta` 每次轮询的推送身份时间 = `exchange_ts`（会变），
/// 内容不变时若身份时间也变则 event_id 变 —— 这是 §5.4「其余事件族身份时间每次推送均变」
/// 的直接后果；**精度等元数据一旦变化，内容哈希必然变**（下游靠它发现变更）。
#[test]
fn meta_event_id_changes_when_precision_changes() {
    let base = envelope(meta_body());

    // 同一轮（同 exchange_ts）精度变了 → 内容哈希变 → event_id 变
    let mut changed = meta_body();
    if let Body::Market(ref mut m) = changed {
        m.exchange_ts = base.received_ts;
        if let MarketData::Meta(ref mut mp) = m.data {
            mp.symbol.price_precision = 2;
        }
    }
    let env_changed = Envelope::new(EventType::Market, SOURCE, base.received_ts, changed).unwrap();
    assert_eq!(env_changed.identity_time(), base.identity_time(), "身份时间相同");
    assert_ne!(
        base.event_id, env_changed.event_id,
        "精度变化必须改变内容哈希"
    );

    // 同身份时间 + 同内容 → event_id 相同（可去重）
    let mut same = meta_body();
    if let Body::Market(ref mut m) = same {
        m.exchange_ts = base.received_ts;
    }
    let env_same = Envelope::new(EventType::Market, SOURCE, base.received_ts, same).unwrap();
    assert_eq!(base.event_id, env_same.event_id, "内容未变则幂等键不变");
}

/// 03 §5.4 / 02 §5：同一根 bar 的**同一份内容**重复推送 → 相同 `event_id`
/// （存储层去重），而 `x_version` 每次都是 `received_ts`，保证替换取最新。
#[test]
fn kline_same_content_same_event_id_different_x_version() {
    let first = envelope(kline_body());
    let second = Envelope::new(EventType::Market, SOURCE, TS + 60_000, kline_body()).unwrap();
    assert_eq!(first.event_id, second.event_id, "同 bar 同内容应同一幂等键");
    assert_eq!(first.x_version, Some(TS));
    assert_eq!(second.x_version, Some(TS + 60_000));
    assert_ne!(first.x_version, second.x_version, "x_version 必须单调推进");
}
