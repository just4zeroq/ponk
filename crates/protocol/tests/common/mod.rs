//! 测试夹具：03 §5.3 各家族负载的样例构造器。
#![allow(dead_code)]

use digdigdig3_core::core::types::{FundingRate, Kline, PublicTrade, Ticker, TradeSide};
use ponk_protocol::*;
use std::collections::BTreeMap;

pub const TS: u64 = 1_772_000_000_123;
pub const SOURCE: &str = "collect-1";
pub const INSTRUMENT: &str = "binance:swap:BTCUSDT";

pub fn kline_body() -> Body {
    Body::Market(MarketBody {
        venue: Venue::binance(),
        market: Market::swap(),
        native_code: "BTCUSDT".into(),
        exchange_ts: TS,
        data: MarketData::Kline(Kline {
            open_time: 1_772_000_000_000,
            open: 100.5,
            high: 101.0,
            low: 99.75,
            close: 100.25,
            volume: 12.5,
            quote_volume: Some(1253.125),
            ..Default::default()
        }),
    })
}

pub fn ticker_body() -> Body {
    Body::Market(MarketBody {
        venue: Venue::okx(),
        market: Market::spot(),
        native_code: "BTC-USDT".into(),
        exchange_ts: TS,
        data: MarketData::Ticker(Ticker {
            last_price: 100.5,
            bid_price: Some(100.4),
            ask_price: Some(100.6),
            timestamp: TS as i64,
            ..Default::default()
        }),
    })
}

pub fn trade_body() -> Body {
    Body::Market(MarketBody {
        venue: Venue::binance(),
        market: Market::spot(),
        native_code: "ETHUSDT".into(),
        exchange_ts: TS,
        data: MarketData::Trade(PublicTrade {
            id: "t-1".into(),
            price: 3000.25,
            quantity: 0.5,
            side: TradeSide::Buy,
            timestamp: TS as i64,
            ..Default::default()
        }),
    })
}

pub fn funding_body() -> Body {
    Body::Market(MarketBody {
        venue: Venue::binance(),
        market: Market::swap(),
        native_code: "BTCUSDT".into(),
        exchange_ts: TS,
        data: MarketData::Funding(FundingRate {
            rate: 0.0001,
            timestamp: TS as i64,
            ..Default::default()
        }),
    })
}

pub fn signal_body() -> Body {
    let mut payload = BTreeMap::new();
    payload.insert("rsi".to_string(), 71.2);
    payload.insert("ma".to_string(), 50.0);
    payload.insert("basis".to_string(), -0.0123);
    Body::Signal(SignalEvent {
        signal_id: "btc_4h_momentum".into(),
        signal_name: "BTC 4h 动量".into(),
        version: "v7".into(),
        instrument: INSTRUMENT.into(),
        direction: Direction::Long,
        value: 71.2,
        timeframe: "4h".into(),
        timestamp: TS,
        payload,
    })
}

pub fn factor_body() -> Body {
    let mut meta = BTreeMap::new();
    meta.insert("funding_rate".to_string(), 0.0001);
    meta.insert("mark_price".to_string(), 100.5);
    Body::Factor(FactorEvent {
        factor_id: "funding_basis".into(),
        factor_name: "资金费偏离".into(),
        version: "v3".into(),
        instrument: INSTRUMENT.into(),
        timeframe: "1m".into(),
        value: -0.0123,
        timestamp: TS,
        meta,
    })
}

pub fn alarm_body() -> Body {
    let mut detail = BTreeMap::new();
    detail.insert("rule_version".to_string(), "21".into());
    detail.insert("channel".to_string(), "feishu".into());
    Body::Alarm(AlarmEvent {
        alarm_id: "a-1".into(),
        rule_id: "btc_4h_rsi_overbought".into(),
        signal_id: Some("btc_4h_momentum".into()),
        level: Severity::Warn,
        title: "BTC 4h RSI 超买".into(),
        detail,
        triggered_ts: TS,
    })
}

pub fn ops_body() -> Body {
    let mut counters = BTreeMap::new();
    counters.insert("cfg.serialization".to_string(), 2i64);
    counters.insert("cfg.signal_rules".to_string(), 7i64);
    counters.insert("send_backlog".to_string(), 0i64);
    Body::Ops(OpsHeartbeat {
        host: None,
        instance_id: "collector-1".into(),
        node_kind: NodeKind::Collector,
        status: Status::Running,
        applied_version: Some(12),
        counters,
        started_at: TS - 5_000,
        ts: TS,
    })
}

pub fn ctl_plan_body() -> Body {
    Body::Ctl(CtlMessage::SubscribePlan(SubscribePlan {
        assignment_version: 12,
        mode: PlanMode::Snapshot,
        shards: vec![ShardPlan {
            venue: Venue::binance(),
            market: Market::swap(),
            shard_idx: 3,
            instruments: vec![InstrumentMeta {
                native_code: "BTCUSDT".into(),
                price_precision: 1,
                qty_precision: 3,
                channels: vec![Channel::candle_1m(), Channel::trade()],
                enabled: true,
                meta: BTreeMap::new(),
            }],
        }],
        quota_share: Some(QuotaShare {
            venue: Venue::binance(),
            egress_ip: "1.2.3.4".into(),
            collect: 0.50,
            backfill: 0.35,
            metadata: 0.15,
            version: 12,
        }),
    }))
}

pub fn ctl_changed_body() -> Body {
    Body::Ctl(CtlMessage::ConfigChanged(ConfigChanged {
        domain: ConfigDomain::SignalRules,
        version: 7,
        publisher: "cp-1/alice".into(),
        ts: TS,
    }))
}

pub fn ctl_snapshot_body() -> Body {
    Body::Ctl(CtlMessage::config_snapshot(
        ConfigDomain::AlertRules,
        21,
        ConfigPayload::new(
            serde_json::json!({ "alerts": [ { "rule_id": "btc_4h_rsi_overbought" } ] }),
        ),
    ))
}

/// `md.meta`（09 §2.3 定时任务；03 §9-8）—— 唯一的平台自建行情负载。
pub fn meta_body() -> Body {
    let mut meta = BTreeMap::new();
    meta.insert("deposit_precision".to_string(), "6".to_string());
    Body::Market(MarketBody {
        venue: Venue::binance(),
        market: Market::swap(),
        native_code: "BTCUSDT".into(),
        exchange_ts: TS,
        data: MarketData::Meta(MetaPayload {
            symbol: SymbolInfo {
                symbol: "BTCUSDT".into(),
                base_asset: "BTC".into(),
                quote_asset: "USDT".into(),
                status: "TRADING".into(),
                price_precision: 1,
                quantity_precision: 3,
                min_quantity: Some(0.001),
                max_quantity: None,
                tick_size: Some(0.1),
                step_size: Some(0.001),
                min_notional: Some(5.0),
                instrument_type: Some("PERPETUAL".into()),
                extra: serde_json::json!({"lotSizeFilter": {"minQty": "0.001"}}),
                ..Default::default()
            },
            currencies: vec![CurrencyInfo {
                currency: "USDT".into(),
                name: Some("Tether USD".into()),
                networks: vec!["TRC20".into(), "ERC20".into()],
                precision: Some(6),
                meta: { let mut m = BTreeMap::new(); m.insert("deposit_precision_trc20".into(), "6".into()); m },
                extra: serde_json::json!({ "canWithdraw": true, "chainIndex": { "1": 1 } }),
            }],
        }),
    })
}

/// 03 §5.3 全部家族体（含 `CtlMessage` 三个变体）。
pub fn all_bodies() -> Vec<Body> {
    vec![
        kline_body(),
        ticker_body(),
        trade_body(),
        funding_body(),
        meta_body(),
        signal_body(),
        factor_body(),
        alarm_body(),
        ops_body(),
        ctl_plan_body(),
        ctl_changed_body(),
        ctl_snapshot_body(),
    ]
}

/// 造一个 `event_id` / `shard` / `x_version` 均已填好的信封。
pub fn envelope(body: Body) -> Envelope {
    let et = body.event_type();
    Envelope::new(et, SOURCE, TS, body).expect("fixture envelope")
}
