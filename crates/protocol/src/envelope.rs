//! 两级信封：公共头 + 家族体（03 §5.1）。

use crate::body::Body;
use crate::error::ProtocolError;
use crate::event_id;
use crate::event_type::EventType;
use crate::ids::NodeId;
use crate::payload::{MarketData, SubscribePlan};
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// 两级信封的公共头（03 §5.1）。
///
/// 只留**跨家族都有意义**的字段：
///
/// | 字段 | 职责 |
/// |---|---|
/// | `event_type` / `shard` | 路由 |
/// | `event_id` | 幂等（03 §5.4） |
/// | `received_ts` / `source` | 观测与定序 |
/// | `x_version` | 定序版本（仅 candle_1m 需要） |
///
/// 行情专属字段（`venue` / `market` / `native_code` / `exchange_ts`）**不在这里**，
/// 它们在 [`Body::Market`]（`MarketBody`）内——把它们放进公共头会让非行情事件
/// 只能填占位值，污染按 venue 的过滤与索引（03 §5.1 / 01 E5）。
#[derive(Debug, Clone)]
pub struct Envelope {
    /// 解析与路由（03 §5.2 注册表）。
    pub event_type: EventType,
    /// 分片键：预计算的 `instrument_id`；`None` → 默认分区（03 §5.1 / §8）。
    pub shard: Option<String>,
    /// 幂等键（03 §5.4）。
    pub event_id: String,
    /// 观测与定序：采集 / 产生时刻（ms）。
    pub received_ts: u64,
    /// 定序版本，取 `received_ts`；适用于**全部 K 线周期**（03 §5.4）。
    pub x_version: Option<u64>,
    /// 产生该事件的节点实例（观测用）。
    pub source: NodeId,
    /// 家族体。
    pub body: Body,
}

impl Envelope {
    /// 构造信封：**自动**填充分片键、`x_version` 与 `event_id`。
    ///
    /// * `shard`：行情 / 信号 / 因子取 `instrument_id`，其余家族为 `None`（03 §8）；
    /// * `x_version`：行情家族的 K 线取 `received_ts`（03 §5.4，**全部 K 线周期**），其余为 `None`；
    /// * `event_id`：按 03 §5.4 构造。
    ///
    /// `event_id` 计算需要把家族体转成规范化视图，**可能失败**
    /// （例如 `f64` 字段出现 NaN / ±inf，`serde_json` 无法承载），
    /// 故本构造器返回 [`Result`]。
    pub fn new(
        event_type: EventType,
        source: impl Into<NodeId>,
        received_ts: u64,
        body: Body,
    ) -> Result<Self, ProtocolError> {
        let source = source.into();
        let mut env = Envelope {
            event_type,
            shard: None,
            event_id: String::new(),
            received_ts,
            x_version: None,
            source,
            body,
        };
        env.validate()?;
        env.shard = event_id::shard_of(&env.body);
        if matches!(&env.body, Body::Market(m) if matches!(m.data, MarketData::Kline(_))) {
            // 03 §5.4：x_version = received_ts，适用于全部 K 线周期。
            // 同一根 bar 的身份时间不变，仅靠 event_id 会使版本停滞，
            // 故另设 x_version 作为替换定序版本（存储侧按其取最新）。02 §5 同。
            env.x_version = Some(received_ts);
        }
        env.event_id = env.compute_event_id()?;
        Ok(env)
    }

    /// 用一份**已知正确**的 `event_id` 与 `shard` 重建信封
    /// （反序列化路径——线上字节已自带这两项，不重算）。
    pub fn from_parts(
        event_type: EventType,
        shard: Option<String>,
        event_id: String,
        received_ts: u64,
        x_version: Option<u64>,
        source: NodeId,
        body: Body,
    ) -> Result<Self, ProtocolError> {
        let env = Envelope {
            event_type,
            shard,
            event_id,
            received_ts,
            x_version,
            source,
            body,
        };
        env.validate()?;
        Ok(env)
    }

    /// 校验 `event_type` 与 `body` 必须同属一个家族（03 §5.2 注册表），
    /// 且 `event_type` 必须是 `Envelope` 可承载的家族（`dlq` 不是）。
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if !self.event_type.is_envelope_carried() {
            return Err(ProtocolError::Invalid(format!(
                "event_type `{}` 不由 Envelope 承载（死信负载是 DeadLetterMeta，03 §5.2/§7.3）",
                self.event_type
            )));
        }
        let actual = self.body.event_type();
        if actual != self.event_type {
            return Err(ProtocolError::TypeMismatch {
                event_type: self.event_type.to_string(),
                body: actual.to_string(),
            });
        }
        Ok(())
    }

    /// 重算 `event_id`（03 §5.4）。
    pub fn compute_event_id(&self) -> Result<String, ProtocolError> {
        event_id::compute(self)
    }

    /// 校验信封自带的 `event_id` 是否与按 03 §5.4 重算的一致。
    pub fn verify_event_id(&self) -> Result<bool, ProtocolError> {
        Ok(self.compute_event_id()? == self.event_id)
    }

    /// 按 03 §5.4 取身份键（按家族取值）。
    pub fn identity_key(&self) -> String {
        event_id::identity_key(&self.body)
    }

    /// 按 03 §5.4 取身份时间。
    pub fn identity_time(&self) -> u64 {
        event_id::identity_time(&self.body)
    }

    /// 家族体的规范化视图（03 §5.4 哈希输入）。
    pub fn canonical_view(&self) -> Result<String, ProtocolError> {
        crate::canonical::canonical_view_of_body(&self.body)
    }
}

impl Serialize for Envelope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // 固定顺序：`event_type` 必须排在 `body` 之前 —— 反序列化按 event_type 分派。
        let mut map = serializer.serialize_map(Some(7))?;
        map.serialize_entry("event_type", &self.event_type)?;
        map.serialize_entry("shard", &self.shard)?;
        map.serialize_entry("event_id", &self.event_id)?;
        map.serialize_entry("received_ts", &self.received_ts)?;
        map.serialize_entry("x_version", &self.x_version)?;
        map.serialize_entry("source", &self.source)?;
        map.serialize_entry("body", &self.body)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EnvelopeVisitor;

        impl<'de> Visitor<'de> for EnvelopeVisitor {
            type Value = Envelope;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "一个信封 map，且 `event_type` 必须出现在 `body` 之前（03 §5.1）",
                )
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Envelope, A::Error> {
                let mut event_type: Option<EventType> = None;
                let mut shard: Option<Option<String>> = None;
                let mut event_id: Option<String> = None;
                let mut received_ts: Option<u64> = None;
                let mut x_version: Option<Option<u64>> = None;
                let mut source: Option<NodeId> = None;
                let mut body: Option<Body> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "event_type" => {
                            if event_type.is_some() {
                                return Err(de::Error::duplicate_field("event_type"));
                            }
                            event_type = Some(map.next_value()?);
                        }
                        "shard" => shard = Some(map.next_value()?),
                        "event_id" => {
                            if event_id.is_some() {
                                return Err(de::Error::duplicate_field("event_id"));
                            }
                            event_id = Some(map.next_value()?);
                        }
                        "received_ts" => received_ts = Some(map.next_value()?),
                        "x_version" => x_version = Some(map.next_value()?),
                        "source" => source = Some(map.next_value()?),
                        "body" => {
                            if body.is_some() {
                                return Err(de::Error::duplicate_field("body"));
                            }
                            let et = event_type.ok_or_else(|| {
                                de::Error::custom(
                                    "`body` 出现在 `event_type` 之前，无法按 event_type 分派（03 §5.1）",
                                )
                            })?;
                            body = Some(dispatch_body(et, &mut map)?);
                        }
                        _ => {
                            // 前向兼容：忽略未知字段（协议演进不破坏旧读者）。
                            let _ = map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }

                let event_type = event_type.ok_or_else(|| de::Error::missing_field("event_type"))?;
                let event_id = event_id.ok_or_else(|| de::Error::missing_field("event_id"))?;
                let received_ts =
                    received_ts.ok_or_else(|| de::Error::missing_field("received_ts"))?;
                let source = source.ok_or_else(|| de::Error::missing_field("source"))?;
                let body = body.ok_or_else(|| de::Error::missing_field("body"))?;

                Envelope::from_parts(
                    event_type,
                    shard.unwrap_or(None),
                    event_id,
                    received_ts,
                    x_version.unwrap_or(None),
                    source,
                    body,
                )
                .map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_map(EnvelopeVisitor)
    }
}

/// 按 `event_type` 分派到对应变体（03 §5.1：反序列化按 `event_type` 分派）。
fn dispatch_body<'de, A: MapAccess<'de>>(
    event_type: EventType,
    map: &mut A,
) -> Result<Body, A::Error> {
    use crate::payload::{AlarmEvent, CtlMessage, FactorEvent, MarketBody, OpsHeartbeat, SignalEvent};
    match event_type {
        EventType::Market => Ok(Body::Market(map.next_value::<MarketBody>()?)),
        EventType::Signal => Ok(Body::Signal(map.next_value::<SignalEvent>()?)),
        EventType::Factor => Ok(Body::Factor(map.next_value::<FactorEvent>()?)),
        EventType::Alarm => Ok(Body::Alarm(map.next_value::<AlarmEvent>()?)),
        EventType::Ops => Ok(Body::Ops(map.next_value::<OpsHeartbeat>()?)),
        EventType::Ctl => Ok(Body::Ctl(map.next_value::<CtlMessage>()?)),
        EventType::DeadLetter => Err(de::Error::custom(
            "event_type `dlq` 不由 Envelope 承载（03 §5.2 / §7.3）",
        )),
    }
}

/// 便捷构造：`Ctl` 家族信封。
///
/// 供 10 §4 命令信道调用方使用。
pub fn ctl_envelope(
    source: impl Into<NodeId>,
    received_ts: u64,
    plan: SubscribePlan,
) -> Result<Envelope, ProtocolError> {
    Envelope::new(
        EventType::Ctl,
        source,
        received_ts,
        Body::Ctl(crate::payload::CtlMessage::SubscribePlan(plan)),
    )
}
