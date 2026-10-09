//! 两级信封的「家族体」[`Body`]（03 §5.1）。

use crate::event_type::EventType;
use crate::payload::{
    AlarmEvent, CtlMessage, FactorEvent, MarketBody, OpsHeartbeat, SignalEvent,
};
use serde::{Serialize, Serializer};

/// 家族体类别——`Body` 的分派依据（03 §5.1：反序列化按 `event_type` 分派到对应变体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BodyKind {
    Market,
    Signal,
    Factor,
    Alarm,
    Ops,
    Ctl,
    /// 死信负载（`DeadLetterMeta`），**不是** `Body` 变体，不承载于 `Envelope`。
    DeadLetter,
}

/// 家族体（03 §5.1）。
///
/// `enum Body` 使「没有这个字段」在**编译期**成立，而不是「填一个 `None`」：
/// 心跳事件没有 `venue`，因为它根本不是 `Body::Market`。
///
/// ## 序列化
///
/// `Body` **透明序列化**为内层结构（不包 `{"Market":{…}}` 外壳）——家族体的变体由
/// 信封头 `event_type` 决定（03 §5.1），再套一层标签是冗余的。唯一的例外是
/// [`CtlMessage`]：它的两个变体共享同一个 `event_type`（`ctl`），故其自身保留
/// serde 默认外部标签以供分派。
///
/// 因此 **`Body` 不实现 `Deserialize`**：脱离 `event_type` 无法分派。请通过
/// [`crate::Envelope`] 反序列化。
#[derive(Debug, Clone)]
pub enum Body {
    /// 行情 `md.*`。
    Market(MarketBody),
    /// 信号 `signal.*`。
    Signal(SignalEvent),
    /// 因子 `factor.*`。
    Factor(FactorEvent),
    /// 告警 `alarm.*`。
    Alarm(AlarmEvent),
    /// 心跳 / 指标 `ops.*`。
    Ops(OpsHeartbeat),
    /// 命令 / 配置 `ctl.*`。
    Ctl(CtlMessage),
}

impl Body {
    /// 该负载所属家族（03 §5.2 注册表）。
    pub fn event_type(&self) -> EventType {
        match self {
            Body::Market(_) => EventType::Market,
            Body::Signal(_) => EventType::Signal,
            Body::Factor(_) => EventType::Factor,
            Body::Alarm(_) => EventType::Alarm,
            Body::Ops(_) => EventType::Ops,
            Body::Ctl(_) => EventType::Ctl,
        }
    }

    /// 负载类别。
    pub fn kind(&self) -> BodyKind {
        self.event_type().body_kind()
    }

    /// 便捷构造。
    pub fn market(b: MarketBody) -> Self {
        Body::Market(b)
    }

    /// 便捷构造。
    pub fn signal(e: SignalEvent) -> Self {
        Body::Signal(e)
    }

    /// 便捷构造。
    pub fn factor(e: FactorEvent) -> Self {
        Body::Factor(e)
    }

    /// 便捷构造。
    pub fn alarm(e: AlarmEvent) -> Self {
        Body::Alarm(e)
    }

    /// 便捷构造。
    pub fn ops(e: OpsHeartbeat) -> Self {
        Body::Ops(e)
    }

    /// 便捷构造。
    pub fn ctl(m: CtlMessage) -> Self {
        Body::Ctl(m)
    }
}

impl Serialize for Body {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // 透明：直接写内层结构，不套外部标签。
        match self {
            Body::Market(v) => v.serialize(serializer),
            Body::Signal(v) => v.serialize(serializer),
            Body::Factor(v) => v.serialize(serializer),
            Body::Alarm(v) => v.serialize(serializer),
            Body::Ops(v) => v.serialize(serializer),
            Body::Ctl(v) => v.serialize(serializer),
        }
    }
}
