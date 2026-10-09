//! 事件产出：样本 → `Envelope`（03 §5.1 两级信封）→ `md.*` 主题。
//!
//! # 交给协议层的东西
//!
//! `event_id`（03 §5.4 幂等键）、`shard`（= `instrument_id`，03 §8）与
//! `x_version`（**仅 candle_1m** = `received_ts`，03 §5.4）**由
//! `ponk_protocol::Envelope::new` 统一计算**——03 §5.2 明确「结构（类型、字段、
//! 序列化）集中在协议层」，采集侧再实现一遍只会造成两套哈希口径漂移。
//! 本模块只负责：**填对字段**、**选对主题**、**把 `x_version` 的适用范围钉死**。
//!
//! # 规格冲突：`03 §5.5` 的十进制字符串
//!
//! 03 §5.5 要求「价量一律为十进制字符串，任何序列化格式下都不降级为浮点数」，
//! 而 03 §5.3 / 01 E5 要求 `MarketBody::data` **直接复用 dig3 `core::types`**
//! ——dig3 的 `Kline` / `Ticker` / `PublicTrade` / `FundingRate` 价格与成交量字段
//! **全部是 `f64`**（见 `digdigdig3-core/src/core/types/market_data.rs`）。
//! 两者互斥：只要 `data` 还是 dig3 结构，JSON 里就一定是 `64231.5` 而不是 `"64231.5"`。
//!
//! 采集侧**无法**绕开该字段（`Body::Market(MarketBody)` 是协议定义的），
//! 参见 [`crate::decimal`] 中的十进制转换原语与最终报告「规格文档问题 #1」。
//! 这里刻意**不**做「f64 → String → 再 parse 回 f64」的假转换。

use ponk_protocol::{
    Body, Envelope, EventType, Market, MarketBody, NodeId, OpsHeartbeat, Status, Topic, Venue,
};

use crate::channel::Channel;
use crate::error::{Error, Result};
use crate::payload::MarketSample;

/// 一条待发布的事件（行情与降级共用的队列元素）。
#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub topic: Topic,
    pub envelope: Envelope,
    /// 仅用于观测 / 测试（主题已由它推导，此处冗余一份便于日志）。
    pub channel: Channel,
}

/// 心跳主题：`ops.collector.<instance>`（03 §8、10 §5）。
pub fn heartbeat_topic(instance_id: &str) -> Topic {
    Topic::new(format!("ops.collector.{}", instance_id))
}

/// 拉取请求主题：`ctl.request.<instance>`（10 §4.1 / §7）。
///
/// TODO(控制面)：控制面尚未实现，当前**不发送**任何拉取请求；
/// 启动时序（10 §7 第 2 步）留待控制面落地后接入。
pub fn ctl_request_topic(instance_id: &str) -> Topic {
    Topic::new(format!("ctl.request.{}", instance_id))
}

/// 命令信道主题：`ctl.collector.<instance>`（10 §4.1 push）。
///
/// TODO(控制面)：同上，当前不订阅。
pub fn ctl_collector_topic(instance_id: &str) -> Topic {
    Topic::new(format!("ctl.collector.{}", instance_id))
}

/// 把一条行情样本构造成 `md.<channel>` 的信封。
///
/// * `shard` = `instrument_id`（03 §8，由协议层自动填）；
/// * `x_version` = `received_ts`，**仅 `candle_1m`**（03 §5.4，由协议层按
///   `MarketData::Kline` 判定——一期唯一 K 线通道就是 `candle_1m`，两者等价）；
/// * `event_id` = `instrument_id:event_type:身份时间:内容哈希`（03 §5.4）。
pub fn market_envelope(sample: &MarketSample, received_ts: u64, source: &str) -> Result<Envelope> {
    let body = MarketBody {
        venue: Venue::new(sample.venue.clone()),
        market: Market::new(sample.market.clone()),
        native_code: sample.native_code.clone(),
        exchange_ts: sample.exchange_ts,
        data: sample.data.clone(),
    };
    let env = Envelope::new(
        EventType::Market,
        NodeId::new(source),
        received_ts,
        Body::Market(body),
    )
    .map_err(|e| Error::Envelope(e.to_string()))?;

    // 钉死 03 §5.4 的两条硬规则，协议层改动会在这里暴露。
    debug_assert_eq!(env.shard.as_deref(), Some(sample.instrument_id().as_str()));
    let expect_x_version = matches!(sample.channel, Channel::Candle1m);
    debug_assert_eq!(env.x_version.is_some(), expect_x_version);
    if !expect_x_version {
        debug_assert!(env.x_version.is_none());
    }
    Ok(env)
}

/// 行情事件的目标主题（03 §8）。
pub fn market_topic(sample: &MarketSample) -> Topic {
    Topic::new(sample.channel.topic())
}

/// 构造心跳信封（03 §5.3 `OpsHeartbeat`；发送时机见 10 §5）。
pub fn heartbeat_envelope(
    instance_id: &str,
    host: Option<&str>,
    status: Status,
    applied_version: Option<u64>,
    counters: std::collections::BTreeMap<String, i64>,
    started_at: u64,
    ts: u64,
) -> Result<Envelope> {
    let hb = OpsHeartbeat {
        host: host.map(|h| h.to_string()),
        instance_id: instance_id.to_string(),
        node_kind: ponk_protocol::NodeKind::Collector,
        status,
        applied_version,
        counters,
        started_at,
        ts,
    };
    Envelope::new(
        EventType::Ops,
        NodeId::new(instance_id),
        ts,
        Body::Ops(hb),
    )
    .map_err(|e| Error::Envelope(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConnectionKey;
    // `MarketData` 的载荷来自 **ponk-protocol**（crates.io 的 digdigdig3-core 实例）；
    // `StreamEvent` 来自 **本地 path 的 dig3**（连接器层）。两者是不同类型，见
    // `payload::bridge`。
    use digdigdig3::core::types::{
        PublicTrade as DigPublicTrade, StreamEvent, TradeSide as DigTradeSide,
    };
    use ponk_protocol::payload::{FundingRate, Kline, PublicTrade, Ticker, TradeSide};
    use ponk_protocol::MarketData;

    fn sample(channel: Channel) -> MarketSample {
        let venue = "binance".to_string();
        let market = "spot".to_string();
        let native_code = "BTCUSDT".to_string();
        let data = match channel {
            Channel::Candle1m => MarketData::Kline(Kline {
                open_time: 1_700_000_000_000,
                open: 1.0,
                high: 2.0,
                low: 0.5,
                close: 1.5,
                volume: 10.0,
                ..Default::default()
            }),
            Channel::Trade => MarketData::Trade(PublicTrade {
                id: "t1".into(),
                price: 1.0,
                quantity: 2.0,
                side: TradeSide::Buy,
                timestamp: 1_700_000_001_000,
                ..Default::default()
            }),
            Channel::Ticker | Channel::L1 => MarketData::Ticker(Ticker {
                last_price: 64_231.5,
                bid_price: Some(64_231.0),
                ask_price: Some(64_232.0),
                timestamp: 1_700_000_002_000,
                ..Default::default()
            }),
            Channel::Funding => MarketData::Funding(FundingRate {
                rate: 0.0001,
                timestamp: 1_700_000_003_000,
                ..Default::default()
            }),
        };
        MarketSample {
            channel,
            venue,
            market,
            native_code,
            exchange_ts: match channel {
                Channel::Candle1m => 1_700_000_000_000,
                Channel::Trade => 1_700_000_001_000,
                Channel::Ticker | Channel::L1 => 1_700_000_002_000,
                Channel::Funding => 1_700_000_003_000,
            },
            data,
        }
    }

    #[test]
    fn shard_is_instrument_id_for_every_market_channel() {
        // 03 §8：行情事件 shard = instrument_id。
        for ch in Channel::ALL {
            let s = sample(ch);
            let env = market_envelope(&s, 1_700_000_009_000, "collector-01").unwrap();
            assert_eq!(env.shard.as_deref(), Some("binance:spot:BTCUSDT"), "{:?}", ch);
            assert_eq!(env.event_type, EventType::Market);
        }
    }

    #[test]
    fn x_version_only_on_candle_1m() {
        // 03 §5.4：x_version = received_ts，仅 candle_1m。
        let env = market_envelope(&sample(Channel::Candle1m), 1_700_000_009_000, "c").unwrap();
        assert_eq!(env.x_version, Some(1_700_000_009_000));

        for ch in [Channel::Trade, Channel::Ticker, Channel::L1, Channel::Funding] {
            let env = market_envelope(&sample(ch), 1_700_000_009_000, "c").unwrap();
            assert_eq!(env.x_version, None, "{:?}", ch);
        }
    }

    #[test]
    fn event_id_is_content_sensitive_and_idempotent() {
        // 同一 bar 同一内容 → 同一 event_id（存储层 upsert 消化）。
        let a = market_envelope(&sample(Channel::Candle1m), 1_000, "c").unwrap();
        let b = market_envelope(&sample(Channel::Candle1m), 2_000, "c").unwrap();
        assert_eq!(a.event_id, b.event_id, "同内容不同 received_ts 应同 event_id");
        // 而 x_version 不同 —— 定序与幂等是两项独立设计（03 §5.4）。
        assert_ne!(a.x_version, b.x_version);

        // event_id 形状：身份键 : event_type : 身份时间 : 内容哈希
        let id = &a.event_id;
        assert!(id.starts_with("binance:spot:BTCUSDT:md:1700000000000:"), "{}", id);

        // 内容变了 → event_id 变
        let mut changed = sample(Channel::Candle1m);
        if let MarketData::Kline(k) = &mut changed.data {
            k.close = 9.99;
        }
        let c = market_envelope(&changed, 1_000, "c").unwrap();
        assert_ne!(a.event_id, c.event_id);
    }

    #[test]
    fn identity_time_differs_per_channel() {
        // 03 §5.4：其余事件族身份时间每次推送均变。
        let trade = market_envelope(&sample(Channel::Trade), 9_999, "c").unwrap();
        assert!(trade.event_id.contains(":md:1700000001000:"), "{}", trade.event_id);
        let ticker = market_envelope(&sample(Channel::Ticker), 9_999, "c").unwrap();
        assert!(ticker.event_id.contains(":md:1700000002000:"), "{}", ticker.event_id);
    }

    #[test]
    fn topics_match_doc_03_section_8() {
        for ch in Channel::ALL {
            let s = sample(ch);
            assert_eq!(market_topic(&s).as_str(), ch.topic());
        }
    }

    #[test]
    fn heartbeat_envelope_shape() {
        let env = heartbeat_envelope(
            "collector-01",
            None, // 上报机器名（可选，09 §2.3）
            Status::Running,
            Some(7),
            std::collections::BTreeMap::from([("published".to_string(), 10i64)]),
            1_000,
            6_000,
        )
        .unwrap();
        assert_eq!(env.event_type, EventType::Ops);
        assert_eq!(env.shard, None, "03 §8：心跳无标的 → None → 默认分区");
        assert_eq!(env.x_version, None, "x_version 仅 candle_1m");
        assert!(env.event_id.starts_with("collector-01:ops:6000:"), "{}", env.event_id);
        match &env.body {
            Body::Ops(hb) => {
                assert_eq!(hb.instance_id, "collector-01");
                assert_eq!(hb.status, Status::Running);
                assert_eq!(hb.applied_version, Some(7));
                assert_eq!(hb.counters["published"], 10);
            }
            other => panic!("unexpected body {:?}", other),
        }
    }

    #[test]
    fn topics_helper_shapes() {
        assert_eq!(heartbeat_topic("i1").as_str(), "ops.collector.i1");
        assert_eq!(ctl_collector_topic("i1").as_str(), "ctl.collector.i1");
        assert_eq!(ctl_request_topic("i1").as_str(), "ctl.request.i1");
    }

    #[test]
    fn market_envelope_from_classifier_roundtrip() {
        // 用真实分派路径造样本，避免测试与生产路径脱节。
        let cfg = crate::config::CollectorConfig::from_toml(
            r#"
instance_id = "c"
[[shards]]
venue = "binance"
market = "spot"
channels = ["trade"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#,
        )
        .unwrap();
        let plan = cfg.resolve().unwrap();
        let cls = crate::payload::Classifier::for_connection(
            &ConnectionKey::new("binance", "spot"),
            &plan,
        );
        let ev = StreamEvent::Trade {
            symbol: "btcusdt".into(),
            trade: DigPublicTrade {
                id: "7".into(),
                price: 1.0,
                quantity: 1.0,
                side: DigTradeSide::Sell,
                timestamp: 55,
                ..Default::default()
            },
        };
        let samples = cls.classify(&ev);
        assert_eq!(samples.len(), 1);
        let env = market_envelope(&samples[0], 100, "c").unwrap();
        assert_eq!(env.shard.as_deref(), Some("binance:spot:BTCUSDT"));
        assert_eq!(env.x_version, None);
    }
}
