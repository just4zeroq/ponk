//! dig3 `StreamEvent` → 采集侧样本（通道归属 + 白名单匹配 + 交易所时间）。
//!
//! # 白名单（02 §1 / §4）
//!
//! 采集**不做发现**：WS 连接上涌来的任何标的，只要不在订阅计划的白名单里就直接丢弃。
//! 匹配用**大小写不敏感**比较——dig3 各所 WS 帧里的 symbol 大小写不统一
//! （Binance 现货 `btcusdt` / 合约 `BTCUSDT` / OKX `BTC-USDT`），而配置与
//! `instrument_id` 用交易所原生写法。
//!
//! # `exchange_ts` 的取值
//!
//! | 通道 | 来源 | 备注 |
//! |---|---|---|
//! | `candle_1m` | `Kline::open_time` | dig3 的 K 线帧**不带事件时间**，只有 bar 起点 |
//! | `trade` | `PublicTrade::timestamp` | |
//! | `ticker` / `l1` | `Ticker::timestamp` | `md.l1` 由同帧派生，见 `l1_from_ticker` |
//! | `funding` | `FundingRate::timestamp` | |
//!
//! > **规格问题**：03 §5.3 `exchange_ts` = 「交易所事件时间」，但 K 线没有事件时间。
//! > 取 `open_time` 是唯一不引入未来时间、也不用 `received_ts` 冒充交易所时间的选择；
//! > 代价是 06 对 `candle_1m` 算出的 `received_ts - exchange_ts` 是「bar 起点 → 收到」，
//! > 不是端到端延迟。见最终报告「规格文档问题」。

use std::collections::HashMap;

use digdigdig3::core::types::{
    FundingRate, Kline, OrderBook, OrderBookLevel, PublicTrade, StreamEvent, Ticker,
};
use ponk_protocol::MarketData;

use crate::channel::Channel;
use crate::config::{ConnectionKey, ResolvedPlan};

/// 一条可入信封的行情样本。
#[derive(Debug, Clone)]
pub struct MarketSample {
    pub channel: Channel,
    pub venue: String,
    pub market: String,
    /// 配置里的原生代码（**不是** WS 帧里的大小写），即 canonical 的第三段。
    pub native_code: String,
    pub exchange_ts: u64,
    pub data: MarketData,
}

impl MarketSample {
    /// `venue:market:native_code`（02 §3 定案 C）。
    pub fn instrument_id(&self) -> String {
        format!("{}:{}:{}", self.venue, self.market, self.native_code)
    }
}

/// 按连接实例化的分派器：白名单 + 已订阅通道集。
#[derive(Debug, Clone)]
pub struct Classifier {
    venue: String,
    market: String,
    /// `lower(native_code)` → (原生代码, 已订阅通道集)
    whitelist: HashMap<String, (String, Vec<Channel>)>,
}

impl Classifier {
    /// 由解析后的计划构造某个连接的分派器。
    pub fn for_connection(conn: &ConnectionKey, plan: &ResolvedPlan) -> Self {
        let mut whitelist: HashMap<String, (String, Vec<Channel>)> = HashMap::new();
        for (_instrument_id, meta) in &plan.instruments {
            if &meta.venue != &conn.venue || &meta.market != &conn.market {
                continue;
            }
            whitelist.insert(
                meta.native_code.to_ascii_lowercase(),
                (meta.native_code.clone(), meta.channels.clone()),
            );
        }
        Self {
            venue: conn.venue.clone(),
            market: conn.market.clone(),
            whitelist,
        }
    }

    pub fn whitelist_len(&self) -> usize {
        self.whitelist.len()
    }

    pub fn is_whitelisted(&self, symbol: &str) -> bool {
        self.whitelist.contains_key(&symbol.to_ascii_lowercase())
    }

    /// 把一条（可能嵌套 `Batch` 的）WS 事件分派成若干样本。
    ///
    /// 不在白名单 / 通道未订阅 / 关键数值非有限 → 不产出（由调用方按需计数）。
    pub fn classify(&self, event: &StreamEvent) -> Vec<MarketSample> {
        let mut out = Vec::new();
        self.classify_into(event, &mut out);
        out
    }

    fn classify_into(&self, event: &StreamEvent, out: &mut Vec<MarketSample>) {
        match event {
            // 一帧多条（dig3 的 batch 扇出，03 对应「一帧 N 事件」）。
            StreamEvent::Batch(items) => {
                for it in items {
                    self.classify_into(it, out);
                }
                return;
            }
            StreamEvent::Kline { symbol, interval, kline } => {
                if interval.as_str() != "1m" {
                    return; // 采集器一期只做 1m（02 §2 通道表）
                }
                if let Some((code, channels)) = self.lookup(symbol) {
                    if channels.contains(&Channel::Candle1m) && sane_kline(kline) {
                        out.push(MarketSample {
                            channel: Channel::Candle1m,
                            venue: self.venue.clone(),
                            market: self.market.clone(),
                            native_code: code,
                            exchange_ts: kline.open_time.max(0) as u64,
                            data: MarketData::Kline(kline.clone()),
                        });
                    }
                }
            }
            StreamEvent::Trade { symbol, trade } => {
                if let Some((code, channels)) = self.lookup(symbol) {
                    if channels.contains(&Channel::Trade) && sane_trade(trade) {
                        out.push(MarketSample {
                            channel: Channel::Trade,
                            venue: self.venue.clone(),
                            market: self.market.clone(),
                            native_code: code,
                            exchange_ts: trade.timestamp.max(0) as u64,
                            data: MarketData::Trade(trade.clone()),
                        });
                    }
                }
            }
            StreamEvent::Ticker { symbol, ticker } => {
                // **同帧双发**：一条 WS Ticker 帧同时喂 `md.ticker` 与 `md.l1`。
                //
                // * `md.ticker`：全字段 `Ticker`；
                // * `md.l1`：**轻量派生视图** —— `OrderBook` 只含一档 bid/ask
                //   （02 §2「L1 即 ticker 副产品」，不需要单独 orderbook 端点）。
                //
                // 两者载荷结构不同 ⇒ 规范化视图不同 ⇒ `event_id` 不同，互不去重
                // （03 §5.4：内容敏感）。配置了哪条就发哪条，不会出现
                // 「配了 l1 却收不到」。
                //
                // L1 骑在 **Ticker 流**上（`Channel::L1::stream_type()` 同样返回
                // `StreamType::Ticker`），因此两条通道的订阅在建订阅集时去重成**一条**
                // 线上流 —— 这才是「同帧」的由来，也避免双流带来的重复帧。
                let Some((code, channels)) = self.lookup(symbol) else {
                    return;
                };
                if !sane_ticker(ticker) {
                    return;
                }
                let base = MarketSample {
                    channel: Channel::Ticker,
                    venue: self.venue.clone(),
                    market: self.market.clone(),
                    native_code: code.clone(),
                    exchange_ts: ticker.timestamp.max(0) as u64,
                    data: MarketData::Ticker(ticker.clone()),
                };
                if channels.contains(&Channel::Ticker) {
                    out.push(base.clone());
                }
                if channels.contains(&Channel::L1) {
                    if let Some(book) = l1_from_ticker(ticker) {
                        out.push(MarketSample {
                            channel: Channel::L1,
                            data: MarketData::OrderBook(book),
                            ..base
                        });
                    }
                }
            }
            StreamEvent::FundingRate { symbol, funding } => {
                if let Some((code, channels)) = self.lookup(symbol) {
                    if channels.contains(&Channel::Funding) && sane_funding(funding) {
                        out.push(MarketSample {
                            channel: Channel::Funding,
                            venue: self.venue.clone(),
                            market: self.market.clone(),
                            native_code: code,
                            exchange_ts: funding.timestamp.max(0) as u64,
                            data: MarketData::Funding(funding.clone()),
                        });
                    }
                }
            }
            // 其余变体（orderbook / liquidation / OI / mark price / 私有流 …）
            // 不在一期四条通道内 —— 02 §2 标为二期或明确排除。
            _ => {}
        }
    }

    fn lookup(&self, symbol: &str) -> Option<(String, Vec<Channel>)> {
        self.whitelist.get(&symbol.to_ascii_lowercase()).cloned()
    }
}

/// 由 ticker 派生 L1 视图：`OrderBook` 只放一档 bid / 一档 ask。
///
/// 返回 `None` = 该帧没有可展示的盘口（bid/ask 缺失或非有限），
/// 此时**不产出** `md.l1` —— 宁可不发也不发空档位。
fn l1_from_ticker(t: &Ticker) -> Option<OrderBook> {
    let bid = t.bid_price?;
    let ask = t.ask_price?;
    if !bid.is_finite() || !ask.is_finite() {
        return None;
    }
    let bid_qty = t.bid_qty.unwrap_or(0.0);
    let ask_qty = t.ask_qty.unwrap_or(0.0);
    if !bid_qty.is_finite() || !ask_qty.is_finite() {
        return None;
    }
    Some(OrderBook {
        bids: vec![OrderBookLevel {
            price: bid,
            size: bid_qty,
            order_count: None,
        }],
        asks: vec![OrderBookLevel {
            price: ask,
            size: ask_qty,
            order_count: None,
        }],
        timestamp: t.timestamp,
        sequence: None,
        last_update_id: None,
        first_update_id: None,
        prev_update_id: None,
        event_time: None,
        transaction_time: None,
        checksum: None,
        prev_change_id: None,
        cts: None,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 有限性检查：`Envelope::new` 的内容哈希经 serde_json，NaN/±inf 无法承载
// （03 §5.4 / protocol `ProtocolError`），且非有限价量本就是脏数据。
// ─────────────────────────────────────────────────────────────────────────────

fn sane_kline(k: &Kline) -> bool {
    [k.open, k.high, k.low, k.close, k.volume]
        .iter()
        .all(|v| v.is_finite())
}

fn sane_ticker(t: &Ticker) -> bool {
    t.last_price.is_finite()
        && t.bid_price.map(|v| v.is_finite()).unwrap_or(true)
        && t.ask_price.map(|v| v.is_finite()).unwrap_or(true)
        && t.bid_qty.map(|v| v.is_finite()).unwrap_or(true)
        && t.ask_qty.map(|v| v.is_finite()).unwrap_or(true)
        && t.volume_24h.map(|v| v.is_finite()).unwrap_or(true)
}

fn sane_trade(t: &PublicTrade) -> bool {
    t.price.is_finite() && t.quantity.is_finite()
}

fn sane_funding(f: &FundingRate) -> bool {
    f.rate.is_finite()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CollectorConfig;

    const PLAN: &str = r#"
instance_id = "c"
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["ticker", "trade", "candle_1m"]
[[shards.instruments]]
native_code = "BTCUSDT"
[[shards.instruments]]
native_code = "ETHUSDT"
"#;

    fn classifier() -> Classifier {
        let plan = CollectorConfig::from_toml(PLAN).unwrap().resolve().unwrap();
        Classifier::for_connection(&ConnectionKey::new("binance", "spot"), &plan)
    }

    fn kline(symbol: &str, open_time: i64) -> StreamEvent {
        StreamEvent::Kline {
            symbol: symbol.to_string(),
            interval: digdigdig3::core::types::KlineInterval::new("1m"),
            kline: Kline {
                open_time,
                open: 1.0,
                high: 2.0,
                low: 0.5,
                close: 1.5,
                volume: 10.0,
                ..Default::default()
            },
        }
    }

    #[test]
    fn whitelisted_symbol_case_insensitively() {
        let c = classifier();
        // Binance 现货 WS 帧给的是小写
        let out = c.classify(&StreamEvent::Ticker {
            symbol: "btcusdt".into(),
            ticker: Ticker {
                last_price: 1.0,
                timestamp: 1_700_000_000_000,
                ..Default::default()
            },
        });
        assert_eq!(out.len(), 1);
        // 但产出的 native_code 是**配置里的写法**，保证 instrument_id 稳定
        assert_eq!(out[0].native_code, "BTCUSDT");
        assert_eq!(out[0].instrument_id(), "binance:spot:BTCUSDT");
        assert_eq!(out[0].channel, Channel::Ticker);
    }

    #[test]
    fn non_whitelisted_symbol_is_dropped() {
        // 02 §1：白名单之外的标的一律不采（采集不做发现）。
        let c = classifier();
        let out = c.classify(&StreamEvent::Ticker {
            symbol: "SOLUSDT".into(),
            ticker: Ticker {
                last_price: 1.0,
                timestamp: 1,
                ..Default::default()
            },
        });
        assert!(out.is_empty());
        assert!(!c.is_whitelisted("SOLUSDT"));
        assert!(c.is_whitelisted("ethusdt"));
    }

    #[test]
    fn kline_identity_time_is_bar_open() {
        let c = classifier();
        let out = c.classify(&kline("BTCUSDT", 1_700_000_000_000));
        assert_eq!(out.len(), 1);
        // 03 §5.4：candle_1m 身份时间对齐 bar 起点
        assert_eq!(out[0].exchange_ts, 1_700_000_000_000);
        assert_eq!(out[0].channel, Channel::Candle1m);
        assert!(matches!(out[0].data, MarketData::Kline(_)));
    }

    #[test]
    fn non_1m_kline_is_ignored() {
        let c = classifier();
        let ev = StreamEvent::Kline {
            symbol: "BTCUSDT".into(),
            interval: digdigdig3::core::types::KlineInterval::new("5m"),
            kline: Kline {
                open_time: 1,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
                ..Default::default()
            },
        };
        assert!(c.classify(&ev).is_empty());
    }

    #[test]
    fn batch_frames_are_flattened() {
        let c = classifier();
        let ev = StreamEvent::Batch(vec![
            kline("BTCUSDT", 1),
            kline("SOLUSDT", 2), // 白名单外
            kline("ethusdt", 3),
        ]);
        let out = c.classify(&ev);
        assert_eq!(out.len(), 2);
        let codes: Vec<&str> = out.iter().map(|s| s.native_code.as_str()).collect();
        assert_eq!(codes, vec!["BTCUSDT", "ETHUSDT"]);
    }

    #[test]
    fn trade_and_funding_map_to_their_channels() {
        let c = classifier();
        let trade = StreamEvent::Trade {
            symbol: "BTCUSDT".into(),
            trade: PublicTrade {
                id: "1".into(),
                price: 1.0,
                quantity: 2.0,
                side: digdigdig3::core::types::TradeSide::Buy,
                timestamp: 42,
                ..Default::default()
            },
        };
        let out = c.classify(&trade);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel, Channel::Trade);
        assert_eq!(out[0].exchange_ts, 42);
        assert!(matches!(out[0].data, MarketData::Trade(_)));
    }

    #[test]
    fn non_finite_values_are_dropped_not_published() {
        let c = classifier();
        let ev = StreamEvent::Ticker {
            symbol: "BTCUSDT".into(),
            ticker: Ticker {
                last_price: f64::NAN,
                timestamp: 1,
                ..Default::default()
            },
        };
        assert!(c.classify(&ev).is_empty());

        let ev2 = StreamEvent::Trade {
            symbol: "BTCUSDT".into(),
            trade: PublicTrade {
                id: "1".into(),
                price: f64::INFINITY,
                quantity: 1.0,
                side: digdigdig3::core::types::TradeSide::Buy,
                timestamp: 1,
                ..Default::default()
            },
        };
        assert!(c.classify(&ev2).is_empty());
    }

    #[test]
    fn unsubscribed_channel_yields_nothing() {
        // 计划里没有 l1；只订阅了 ticker → Ticker 帧归 ticker。
        let c = classifier();
        let ev = StreamEvent::Ticker {
            symbol: "BTCUSDT".into(),
            ticker: Ticker {
                last_price: 1.0,
                timestamp: 1,
                ..Default::default()
            },
        };
        assert_eq!(c.classify(&ev)[0].channel, Channel::Ticker);
    }

    #[test]
    fn l1_only_plan_maps_ticker_frames_to_l1() {
        let plan_toml = r#"
instance_id = "c"
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["l1"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;
        let plan = CollectorConfig::from_toml(plan_toml).unwrap().resolve().unwrap();
        let c = Classifier::for_connection(&ConnectionKey::new("binance", "spot"), &plan);
        // L1 由 ticker 的 bid/ask 派生（02 §2），故夹具必须带一档买卖价 —— 真实
        // 能产出 L1 的帧本就含 bid/ask；无 bid/ask 的 ticker 无法派生 L1。
        let ev = StreamEvent::Ticker {
            symbol: "BTCUSDT".into(),
            ticker: Ticker {
                last_price: 1.0,
                bid_price: Some(0.99),
                ask_price: Some(1.01),
                bid_qty: Some(10.0),
                ask_qty: Some(20.0),
                timestamp: 1,
                ..Default::default()
            },
        };
        assert_eq!(c.classify(&ev)[0].channel, Channel::L1);
    }
}
