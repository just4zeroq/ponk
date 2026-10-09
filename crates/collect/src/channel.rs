//! 数据通道（docs/02 §2 通道映射、docs/03 §8 主题命名、docs/10 §3 数据信道）。

use std::fmt;

use digdigdig3::core::types::StreamType;
use serde::{Deserialize, Serialize};

/// 平台数据通道 —— 一期四条 + L1 投影（可选）。
///
/// | 通道 | 主题 | dig3 能力 | 分片 |
/// |---|---|---|---|
/// | `candle_1m` | `md.candle.1m` | `StreamType::Kline{interval:"1m"}` / `get_klines` | 按标的 |
/// | `trade` | `md.trade` | `StreamType::Trade` | 按标的 |
/// | `ticker` | `md.ticker` | `StreamType::Ticker` / `get_ticker` | **全市场，不分片** |
/// | `l1` | `md.l1` | `StreamType::BookTicker`（ticker 副产品） | **全市场，不分片** |
/// | `funding` | `md.funding` | `StreamType::FundingRate` | 按标的（仅永续） |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Candle1m,
    Trade,
    Ticker,
    L1,
    Funding,
}

impl Channel {
    /// 全部通道（配置校验用，顺序即文档顺序）。
    pub const ALL: [Channel; 5] = [
        Channel::Candle1m,
        Channel::Trade,
        Channel::Ticker,
        Channel::L1,
        Channel::Funding,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Channel::Candle1m => "candle_1m",
            Channel::Trade => "trade",
            Channel::Ticker => "ticker",
            Channel::L1 => "l1",
            Channel::Funding => "funding",
        }
    }

    /// 数据信道主题（03 §8）。
    pub fn topic(&self) -> &'static str {
        match self {
            Channel::Candle1m => "md.candle.1m",
            Channel::Trade => "md.trade",
            Channel::Ticker => "md.ticker",
            Channel::L1 => "md.l1",
            Channel::Funding => "md.funding",
        }
    }

    /// 发布失败最终落的死信主题（10 §3「发送失败重试，最终失败进 dlq.md.*」）。
    pub fn dlq_topic(&self) -> &'static str {
        match self {
            Channel::Candle1m => "dlq.md.candle.1m",
            Channel::Trade => "dlq.md.trade",
            Channel::Ticker => "dlq.md.ticker",
            Channel::L1 => "dlq.md.l1",
            Channel::Funding => "dlq.md.funding",
        }
    }

    /// 事件族名（03 §5.4 的 `event_type`，也是 `event_id` 的第二段）。
    pub fn event_type(&self) -> &'static str {
        self.as_str()
    }

    /// **全市场通道：不分片**（02 §4「ticker / L1：全市场单调用，不分片」）。
    ///
    /// 这里的「不分片」指**逻辑分片（ShardPlan.shard_idx）不分**：
    /// 全市场通道只允许 `shard_idx == 0` 的单一计划覆盖全部标的；
    /// `Envelope::shard` 仍按 03 §5.4/§8 恒为 `instrument_id`。
    pub fn is_full_market(&self) -> bool {
        matches!(self, Channel::Ticker | Channel::L1)
    }

    /// 是否随标的分片（`is_full_market` 的反面，便于正向阅读）。
    pub fn is_per_instrument(&self) -> bool {
        !self.is_full_market()
    }

    /// 是否只在永续市场存在（02 §2：资金费 / mark / index 为永续特有）。
    pub fn is_swap_only(&self) -> bool {
        matches!(self, Channel::Funding)
    }

    /// REST 降级能力（02 §6）。
    pub fn rest_degrade(&self) -> DegradePolicy {
        match self {
            // 「ticker/L1 可全市场 REST 降级」
            Channel::Ticker | Channel::L1 => DegradePolicy::FullMarket,
            // 「candle 仅核心集可降级，其余交回补」
            Channel::Candle1m => DegradePolicy::CoreSetOnly,
            // 「trade 不降级」
            Channel::Trade => DegradePolicy::Never,
            // 资金费没有被 02 §6 列入可降级项（既非 ticker/L1，也非 candle）——保守取 Never
            Channel::Funding => DegradePolicy::Never,
        }
    }

    /// WS 订阅类型（02 §2 映射表）。
    ///
    /// **`l1` 与 `ticker` 共用 `StreamType::Ticker`** —— 02 §2「L1 即 ticker 副产品」。
    /// 于是「ticker + l1」两条通道在建订阅集时会**去重成一条线上流**，
    /// 同一帧由 [`crate::payload::Classifier`] 分派成 `md.ticker` 与 `md.l1` 两条事件
    /// （同帧双发）。若 `l1` 走 `BookTicker`，两条流各给一帧就会把同一盘口发两遍。
    pub fn stream_type(&self) -> StreamType {
        match self {
            Channel::Candle1m => StreamType::Kline {
                interval: "1m".to_string(),
            },
            Channel::Trade => StreamType::Trade,
            Channel::Ticker | Channel::L1 => StreamType::Ticker,
            Channel::Funding => StreamType::FundingRate,
        }
    }

    /// 解析通道名。两种写法都收：
    ///
    /// * **短名**（本 crate 配置文件用）：`candle_1m` / `trade` / `ticker` / `l1` / `funding`；
    /// * **主题名**（`ponk_protocol::InstrumentMeta::channels` 的示例写法，03 §5.3）：
    ///   `md.candle.1m` / `md.trade` / `md.ticker` / `md.l1` / `md.funding`。
    ///
    /// TODO(控制面)：控制面下发的订阅计划里 `channels` 用哪一种尚未定死；
    /// 两种都收，避免接上 `ctl.*` 时在通道名上翻车。
    pub fn parse(s: &str) -> Option<Channel> {
        match s {
            "candle_1m" | "kline_1m" | "1m" | "md.candle.1m" => Some(Channel::Candle1m),
            "trade" | "trades" | "md.trade" => Some(Channel::Trade),
            "ticker" | "md.ticker" => Some(Channel::Ticker),
            "l1" | "book_ticker" | "md.l1" => Some(Channel::L1),
            "funding" | "funding_rate" | "md.funding" => Some(Channel::Funding),
            _ => None,
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// REST 降级策略（02 §6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradePolicy {
    /// 全市场 REST 降级：该通道断线时可对白名单内全部标的轮询。
    FullMarket,
    /// 仅核心集（core set）可降级；核心集之外交回补节点，采集不碰。
    CoreSetOnly,
    /// 不降级（02 §6 明文：trade 不降级）。
    Never,
}

// ─────────────────────────────────────────────────────────────────────────────
// 与协议层 `ponk_protocol::Channel`（透明字符串 newtype，03 §5.3
// `InstrumentMeta::channels`）的互转：控制面下发的就是它。
// ─────────────────────────────────────────────────────────────────────────────

impl From<Channel> for ponk_protocol::Channel {
    fn from(c: Channel) -> Self {
        // 协议层取值口径 = 通道名而非主题名（`candle_1m` / `trade` / …）。
        ponk_protocol::Channel::new(c.as_str())
    }
}

impl Channel {
    /// 由协议层 `Channel` 解析；未知通道名 → `None`。
    pub fn from_protocol(c: &ponk_protocol::Channel) -> Option<Channel> {
        Channel::parse(c.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_match_doc_03_section_8() {
        assert_eq!(Channel::Candle1m.topic(), "md.candle.1m");
        assert_eq!(Channel::Trade.topic(), "md.trade");
        assert_eq!(Channel::Ticker.topic(), "md.ticker");
        assert_eq!(Channel::L1.topic(), "md.l1");
        assert_eq!(Channel::Funding.topic(), "md.funding");
    }

    #[test]
    fn full_market_channels_are_ticker_and_l1() {
        assert!(Channel::Ticker.is_full_market());
        assert!(Channel::L1.is_full_market());
        assert!(Channel::Candle1m.is_per_instrument());
        assert!(Channel::Trade.is_per_instrument());
        assert!(Channel::Funding.is_per_instrument());
    }

    #[test]
    fn degrade_policy_matches_doc_02_section_6() {
        assert_eq!(Channel::Ticker.rest_degrade(), DegradePolicy::FullMarket);
        assert_eq!(Channel::L1.rest_degrade(), DegradePolicy::FullMarket);
        assert_eq!(Channel::Candle1m.rest_degrade(), DegradePolicy::CoreSetOnly);
        assert_eq!(Channel::Trade.rest_degrade(), DegradePolicy::Never);
        assert_eq!(Channel::Funding.rest_degrade(), DegradePolicy::Never);
    }

    #[test]
    fn swap_only_channel_is_funding() {
        assert!(Channel::Funding.is_swap_only());
        assert!(!Channel::Candle1m.is_swap_only());
        assert!(!Channel::Trade.is_swap_only());
        assert!(!Channel::Ticker.is_swap_only());
    }

    #[test]
    fn stream_types_match_dig3_capability_map() {
        assert!(matches!(Channel::Candle1m.stream_type(), StreamType::Kline { interval } if interval == "1m"));
        assert!(matches!(Channel::Trade.stream_type(), StreamType::Trade));
        assert!(matches!(Channel::Ticker.stream_type(), StreamType::Ticker));
        // L1 与 ticker 共用同一条线上流（02 §2：L1 是 ticker 副产品）
        assert!(matches!(Channel::L1.stream_type(), StreamType::Ticker));
        assert!(matches!(Channel::Funding.stream_type(), StreamType::FundingRate));
    }

    #[test]
    fn protocol_channel_round_trip() {
        for ch in Channel::ALL {
            let proto: ponk_protocol::Channel = ch.into();
            assert_eq!(proto.as_str(), ch.as_str());
            assert_eq!(Channel::from_protocol(&proto), Some(ch));
        }
        // 协议层/控制面可能给主题名写法
        assert_eq!(
            Channel::from_protocol(&ponk_protocol::Channel::new("md.candle.1m")),
            Some(Channel::Candle1m)
        );
        assert_eq!(
            Channel::from_protocol(&ponk_protocol::Channel::new("nope")),
            None
        );
    }

    #[test]
    fn ticker_and_l1_share_one_wire_stream() {
        // 同帧双发的前提：两条通道订阅的是**同一条**线上流，
        // 否则各来一帧会把同一盘口发两遍。
        assert_eq!(
            Channel::Ticker.stream_type(),
            Channel::L1.stream_type()
        );
    }

    #[test]
    fn parse_round_trip() {
        for ch in Channel::ALL {
            assert_eq!(Channel::parse(ch.as_str()), Some(ch));
        }
        assert_eq!(Channel::parse("nope"), None);
    }

    #[test]
    fn parse_accepts_topic_style_names_from_protocol_instrument_meta() {
        // 03 §5.3 `InstrumentMeta::channels` 的示例写法是主题名（`md.candle.1m`）；
        // 控制面下发的计划用哪一种尚未定死，两种都必须能解析。
        assert_eq!(Channel::parse("md.candle.1m"), Some(Channel::Candle1m));
        assert_eq!(Channel::parse("md.trade"), Some(Channel::Trade));
        assert_eq!(Channel::parse("md.ticker"), Some(Channel::Ticker));
        assert_eq!(Channel::parse("md.l1"), Some(Channel::L1));
        assert_eq!(Channel::parse("md.funding"), Some(Channel::Funding));
    }
}
