//! 事件类型注册表（03 §5.2）。
//!
//! > **本表是 `event_type` 枚举的唯一登记处**；新增家族须先登记，避免出现私有主题。
//!
//! 03 §5.2 的表按「`event_type` **家族** × 主题前缀 × 负载结构」登记，故本枚举是
//! **家族粒度**（7 个变体）：行情家族的通道差异（`md.candle.1m` / `md.trade` / …）
//! 由**主题**承载（03 §8），负载差异由 [`Body`] 内的 [`MarketData`] 变体承载，
//! 二者都不需要额外的 `event_type` 变体。
//!
//! [`Body`]: crate::Body
//! [`MarketData`]: crate::MarketData

use crate::BodyKind;
use serde::{Deserialize, Serialize};

/// 事件类型注册表（03 §5.2）。线格式为家族名，与主题族前缀一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EventType {
    /// 行情，主题族 `md.*`，负载 [`MarketBody`](crate::MarketBody)（03 §5.2 行 1）。
    #[serde(rename = "md")]
    Market,
    /// 信号，主题族 `signal.*`，负载 [`SignalEvent`](crate::SignalEvent)。
    #[serde(rename = "signal")]
    Signal,
    /// 因子，主题族 `factor.*`，负载 [`FactorEvent`](crate::FactorEvent)。
    #[serde(rename = "factor")]
    Factor,
    /// 告警，主题族 `alarm.*`，负载 [`AlarmEvent`](crate::AlarmEvent)。
    #[serde(rename = "alarm")]
    Alarm,
    /// 心跳 / 指标，主题族 `ops.*`，负载 [`OpsHeartbeat`](crate::OpsHeartbeat)。
    #[serde(rename = "ops")]
    Ops,
    /// 命令 / 配置，主题族 `ctl.*`，负载 [`CtlMessage`](crate::CtlMessage)。
    #[serde(rename = "ctl")]
    Ctl,
    /// 死信，主题族 `dlq.<topic>`，负载 [`DeadLetterMeta`](crate::DeadLetterMeta)（03 §7.3）。
    ///
    /// **不承载于 [`Envelope`]**：死信的线负载是 `DeadLetterMeta`（内含原 envelope），
    /// 不经 `Session::next()` 读取，见 §7.4「死信只堆积，不常驻消费」。
    #[serde(rename = "dlq")]
    DeadLetter,
}

impl EventType {
    /// 注册表全量（03 §5.2 七行，顺序即表序）。
    pub const REGISTRY: [EventType; 7] = [
        EventType::Market,
        EventType::Signal,
        EventType::Factor,
        EventType::Alarm,
        EventType::Ops,
        EventType::Ctl,
        EventType::DeadLetter,
    ];

    /// 线格式名（= 注册表「`event_type` 家族」列）。
    pub fn as_str(&self) -> &'static str {
        match self {
            EventType::Market => "md",
            EventType::Signal => "signal",
            EventType::Factor => "factor",
            EventType::Alarm => "alarm",
            EventType::Ops => "ops",
            EventType::Ctl => "ctl",
            EventType::DeadLetter => "dlq",
        }
    }

    /// 主题前缀（03 §5.2「主题前缀」列；死信为 `dlq.<topic>` 故带点）。
    pub fn topic_prefix(&self) -> &'static str {
        match self {
            EventType::Market => "md.",
            EventType::Signal => "signal.",
            EventType::Factor => "factor.",
            EventType::Alarm => "alarm.",
            EventType::Ops => "ops.",
            EventType::Ctl => "ctl.",
            EventType::DeadLetter => "dlq.",
        }
    }

    /// 该家族的负载结构类别（03 §5.1 `enum Body` 分派依据）。
    pub fn body_kind(&self) -> BodyKind {
        match self {
            EventType::Market => BodyKind::Market,
            EventType::Signal => BodyKind::Signal,
            EventType::Factor => BodyKind::Factor,
            EventType::Alarm => BodyKind::Alarm,
            EventType::Ops => BodyKind::Ops,
            EventType::Ctl => BodyKind::Ctl,
            EventType::DeadLetter => BodyKind::DeadLetter,
        }
    }

    /// 是否由 [`Envelope`] 承载。
    ///
    /// `dlq.*` 的线负载是 `DeadLetterMeta` 而非 `Envelope`，故为 `false`。
    pub fn is_envelope_carried(&self) -> bool {
        !matches!(self, EventType::DeadLetter)
    }

    /// 是否按 `instrument_id` 分片（03 §8：行情 / 信号 / 因子为 `instrument_id`，
    /// 心跳 / 告警 / 配置为 `None` 进默认分区）。
    pub fn is_instrument_sharded(&self) -> bool {
        matches!(
            self,
            EventType::Market | EventType::Signal | EventType::Factor
        )
    }
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 03 §5.2 注册表逐行核对：家族名 + 主题前缀 + 负载结构。
    #[test]
    fn registry_matches_spec_table() {
        let expected = [
            (EventType::Market, "md", "md.", BodyKind::Market),
            (EventType::Signal, "signal", "signal.", BodyKind::Signal),
            (EventType::Factor, "factor", "factor.", BodyKind::Factor),
            (EventType::Alarm, "alarm", "alarm.", BodyKind::Alarm),
            (EventType::Ops, "ops", "ops.", BodyKind::Ops),
            (EventType::Ctl, "ctl", "ctl.", BodyKind::Ctl),
            (EventType::DeadLetter, "dlq", "dlq.", BodyKind::DeadLetter),
        ];
        for (et, name, prefix, kind) in expected {
            assert_eq!(et.as_str(), name);
            assert_eq!(et.topic_prefix(), prefix);
            assert_eq!(et.body_kind(), kind);
        }
        assert_eq!(EventType::REGISTRY.len(), 7);
    }

    #[test]
    fn serde_roundtrip_uses_family_wire_name() {
        for et in EventType::REGISTRY {
            let s = serde_json::to_string(&et).unwrap();
            assert_eq!(s, format!("\"{}\"", et.as_str()));
            let back: EventType = serde_json::from_str(&s).unwrap();
            assert_eq!(back, et);
        }
    }

    #[test]
    fn only_dlq_is_not_envelope_carried() {
        for et in EventType::REGISTRY {
            assert_eq!(et.is_envelope_carried(), et != EventType::DeadLetter);
        }
    }

    #[test]
    fn sharding_matches_spec_8() {
        assert!(EventType::Market.is_instrument_sharded());
        assert!(EventType::Signal.is_instrument_sharded());
        assert!(EventType::Factor.is_instrument_sharded());
        assert!(!EventType::Alarm.is_instrument_sharded());
        assert!(!EventType::Ops.is_instrument_sharded());
        assert!(!EventType::Ctl.is_instrument_sharded());
    }
}
