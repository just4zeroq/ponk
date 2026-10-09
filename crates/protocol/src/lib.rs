//! # ponk-protocol — PONK 事件中心消息协议
//!
//! 本 crate 是 [`docs/03-事件中心.md`] §5「消息协议」的权威落地：
//!
//! * **两级信封**（§5.1）：公共头 [`Envelope`] 只留跨家族字段，
//!   行情专属字段（venue / market / native_code / exchange_ts）下移进 [`Body::Market`]；
//! * **事件类型注册表**（§5.2）：[`EventType`]，新增家族须先登记；
//! * **负载结构**（§5.3）：[`MarketBody`] / [`SignalEvent`] / [`FactorEvent`] /
//!   [`AlarmEvent`] / [`OpsHeartbeat`] / [`CtlMessage`] / [`DeadLetterMeta`]；
//! * **幂等键**（§5.4）：[`Envelope::compute_event_id`]，内容哈希在**递归键排序的规范化视图**
//!   （[`canonical`]）上计算，**与序列化格式无关**；
//! * **序列化**（§6）：[`Serializer`] / [`Deserializer`] trait，默认 JSON（[`JsonCodec`]），
//!   另提供 MessagePack（[`MsgPackCodec`]）作为配置项。
//!
//! 数值约定（§5.5）：**价量一律为十进制字符串**，任何序列化格式下都不降级为浮点数。
//!
//! [`docs/03-事件中心.md`]: https://github.com/ponk/ponk

pub mod body;
pub mod canonical;
pub mod envelope;
pub mod error;
pub mod event_id;
pub mod event_type;
pub mod ids;
pub mod payload;
pub mod serialize;

pub use body::{Body, BodyKind};
pub use canonical::{canonical_view, content_hash_of, hash_text};
pub use envelope::Envelope;
pub use error::ProtocolError;
pub use event_type::EventType;
pub use ids::{instrument_id, Channel, Group, Market, NodeId, Topic, Venue};
pub use payload::{
    AccountType, AlarmEvent, ConfigChanged, ConfigDomain, ConfigPayload, CtlMessage, CurrencyInfo,
    DeadLetter, DeadLetterMeta, Direction, FactorEvent, InstrumentMeta, MarketBody, MarketData,
    MetaPayload, NodeKind, OpsHeartbeat, PlanMode, QuotaShare, Severity, ShardPlan, SignalEvent,
    Status, SubscribePlan, SymbolInfo,
};
/// dig3 行情载荷类型：`MarketData::Ticker(..)` 等需要它们才能构造或读取
/// （`payload.rs` 内部使用，此处补对外可达性）。
pub use digdigdig3_core::core::types::{Kline, PublicTrade, Ticker};

pub use serialize::{
    codec_for_topic, default_codec, AnyCodec, Deserializer, Format, FormatPolicy, JsonCodec,
    MsgPackCodec, Serializer,
};
