//! 标识与取值类型。
//!
//! 03 / 01 / 02 / 10 只给了这些类型的名字与取值口径，未给出结构定义，此处为落地定义：
//!
//! | 类型 | 出处 | 取值口径 |
//! |---|---|---|
//! | [`Venue`] | 03 §5.3 `venue` | `binance` / `okx` / …（01 C3，可扩容） |
//! | [`Market`] | 03 §5.3 `market` | `spot` / `swap` / … |
//! | [`NodeId`] | 03 §5.1 `source` | 产生该事件的节点实例（观测用） |
//! | [`Topic`] | 03 §4.1 / §5.3 `origin_topic` | §8 主题命名 |
//! | [`Group`] | 03 §4.1 | 消费组名 |
//! | [`Channel`] | 03 §5.3 `InstrumentMeta::channels` | 02 §2 通道表 |
//!
//! 设计取舍：`Venue` / `Market` 用**透明字符串 newtype** 而非闭合 enum ——
//! 新增交易所（01 C3「扩容再加 Bitget / Hyperliquid」）不应触发 03 §5.2 的
//! 「改结构 = 全平台重编译」高变更门槛。（配置域则是**封闭八域**，见 09 §2，
//! 故 [`crate::ConfigDomain`] 用枚举。）

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// 用任意可转字符串的值构造。
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// 取出底层字符串。
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// 消费并取出底层字符串。
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    /// 交易所（03 §5.3 `MarketBody::venue`）。
    ///
    /// 已知取值：`binance` / `okx`（01 C3 首批）；`bitget` / `hyperliquid` 为扩容项。
    Venue
);

string_id!(
    /// 市场类型（03 §5.3 `MarketBody::market`）。
    ///
    /// 已知取值：`spot` / `swap`（01 C3：现货 + U 本位永续）。
    Market
);

string_id!(
    /// 事件产生节点实例（03 §5.1 `Envelope::source`，观测用）。
    ///
    /// 与 [`crate::OpsHeartbeat::instance_id`] 同一口径（10 §5）。
    NodeId
);

string_id!(
    /// 主题（03 §4.1 / §8）。
    Topic
);

string_id!(
    /// 消费组（03 §4.1）。
    Group
);

string_id!(
    /// 数据通道（03 §5.3 `InstrumentMeta::channels`；通道表见 02 §2）。
    ///
    /// 取值口径 = 通道名而非主题名：`candle_1m` / `trade` / `ticker` / `l1` / `funding`。
    /// 透明字符串 newtype：02 §2 把 OI / 清算 / LSR 列为二期，新通道不应触发 03 §5.2
    /// 的高变更门槛。
    Channel
);

impl Venue {
    /// Binance（01 C3）。
    pub const BINANCE: &'static str = "binance";
    /// OKX（01 C3）。
    pub const OKX: &'static str = "okx";

    /// `binance`。
    pub fn binance() -> Self {
        Self::new(Self::BINANCE)
    }

    /// `okx`。
    pub fn okx() -> Self {
        Self::new(Self::OKX)
    }
}

impl Channel {
    /// 1m K 线（02 §2）。
    pub const CANDLE_1M: &'static str = "candle_1m";
    /// 全市场成交。
    pub const TRADE: &'static str = "trade";
    /// ticker（含 L1 bid/ask）。
    pub const TICKER: &'static str = "ticker";
    /// L1 档位（02 §2：L1 即 ticker 副产品）。
    pub const L1: &'static str = "l1";
    /// 资金费（含 mark / index，02 §2）。
    pub const FUNDING: &'static str = "funding";
    /// 交易对 / 币参考信息（09 §2.3 定时任务，03 §9-8）。
    pub const META: &'static str = "meta";

    /// `candle_1m`。
    pub fn candle_1m() -> Self {
        Self::new(Self::CANDLE_1M)
    }

    /// `trade`。
    pub fn trade() -> Self {
        Self::new(Self::TRADE)
    }

    /// `ticker`。
    pub fn ticker() -> Self {
        Self::new(Self::TICKER)
    }

    /// `l1`。
    pub fn l1() -> Self {
        Self::new(Self::L1)
    }

    /// `funding`。
    pub fn funding() -> Self {
        Self::new(Self::FUNDING)
    }

    /// `meta`（`md.meta`，09 §2.3）。
    pub fn meta() -> Self {
        Self::new(Self::META)
    }

    /// 该通道对应的主题 `md.<channel>`（03 §8）。
    pub fn topic(&self) -> String {
        format!("md.{}", self.0)
    }
}

impl Market {
    /// 现货。
    pub const SPOT: &'static str = "spot";
    /// 永续（U 本位）。
    pub const SWAP: &'static str = "swap";

    /// `spot`。
    pub fn spot() -> Self {
        Self::new(Self::SPOT)
    }

    /// `swap`。
    pub fn swap() -> Self {
        Self::new(Self::SWAP)
    }
}

impl Topic {
    /// 该主题所属主题族前缀（03 §8）；无法归类时为 `None`。
    pub fn family_prefix(&self) -> Option<&'static str> {
        const FAMILIES: [&str; 7] = ["md.", "signal.", "factor.", "alarm.", "ops.", "ctl.", "dlq."];
        FAMILIES.into_iter().find(|f| self.0.starts_with(f))
    }

    /// 是否是死信主题 `dlq.<topic>`（03 §7.3）。
    pub fn is_dead_letter(&self) -> bool {
        self.0.starts_with("dlq.")
    }

    /// 取 `dlq.<topic>` 的原始主题部分；本身即死信主题时返回 `None`。
    pub fn dead_letter_origin(&self) -> Option<&str> {
        self.0.strip_prefix("dlq.")
    }

    /// 由原主题构造死信主题 `dlq.<topic>`（03 §7.3）。
    pub fn to_dead_letter(&self) -> Topic {
        Topic::new(format!("dlq.{}", self.0))
    }
}

impl Default for Topic {
    fn default() -> Self {
        Topic::new(String::new())
    }
}

impl Default for Group {
    fn default() -> Self {
        Group::new(String::new())
    }
}

/// instrument canonical id = `venue:market:native_code`（02 §3，沿用既有 PONK 定案 C）。
///
/// 这是行情 / 信号 / 因子事件的**身份键**（03 §5.4），也是 `Envelope::shard`
/// 分片键的取值来源（03 §8）。
pub fn instrument_id(venue: &Venue, market: &Market, native_code: &str) -> String {
    format!("{}:{}:{}", venue.as_str(), market.as_str(), native_code)
}
