//! 序列化（03 §6）。
//!
//! > 序列化是协议的内部实现，不属于契约层；更换格式不改变任何上层代码。
//! >
//! > 抽象为 `Serializer` / `Deserializer` trait，**默认 JSON**。
//! > 配置粒度为**主题族**（`md.*`、`signal.*`、…），由控制面下发；
//! > 同一主题的生产方与所有消费方必须一致。
//!
//! | 格式 | 保真 | 体积 / 吞吐 | 用途 |
//! |---|---|---|---|
//! | **JSON（默认）** | 价量为字符串，无损 | 中 | 默认；死信与运维工具可直接阅读 |
//! | MessagePack | 无损 | 优于 JSON | 吞吐优化项 |
//!
//! `event_id` 与线上格式无关（03 §5.4），故格式切换不影响去重与替换语义
//! ——见 [`crate::canonical`] 与 `tests/event_id.rs` 的跨格式单测。
//!
//! ## 远期格式的已知限制（03 §6）
//!
//! Protobuf / Avro 一类**非自描述**格式当前**不可用**，实测两点：
//!
//! 1. dig3 行情类型大量使用 `#[serde(skip_serializing_if)]`：序列化时字段会消失，
//!    非自描述格式的反序列化按位读取 → `DeserializeUnexpectedEnd`（按字段名的
//!    JSON / MessagePack 不受影响）；
//! 2. [`ConfigPayload`] 包着 `serde_json::Value`，其反序列化需要 `deserialize_any`
//!    → 非自描述格式直接 `WontImplement`。
//!
//! 启用前须先解决这两点（03 §6 表已标注「远期且受限」）。

use crate::envelope::Envelope;
use crate::error::ProtocolError;
use crate::payload::DeadLetterMeta;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 线上格式（03 §6 表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// JSON（默认）。
    Json,
    /// MessagePack（03 §9-4：作为配置项）。
    MessagePack,
}

impl Format {
    /// 线格式标识（用于日志与能力表）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::MessagePack => "messagepack",
        }
    }
}

impl Default for Format {
    fn default() -> Self {
        Format::Json
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 序列化器（03 §6）。
pub trait Serializer: Send + Sync {
    /// 该序列化器产出的格式。
    fn format(&self) -> Format;

    /// 序列化一个信封。
    fn serialize_envelope(&self, envelope: &Envelope) -> Result<Vec<u8>, ProtocolError>;

    /// 序列化一条死信（03 §7.3，负载是 `DeadLetterMeta` 而非 `Envelope`）。
    fn serialize_dead_letter(&self, dead_letter: &DeadLetterMeta) -> Result<Vec<u8>, ProtocolError>;
}

/// 反序列化器（03 §6）。
pub trait Deserializer: Send + Sync {
    /// 该反序列化器接受的格式。
    fn format(&self) -> Format;

    /// 反序列化一个信封。
    fn deserialize_envelope(&self, bytes: &[u8]) -> Result<Envelope, ProtocolError>;

    /// 反序列化一条死信。
    fn deserialize_dead_letter(&self, bytes: &[u8]) -> Result<DeadLetterMeta, ProtocolError>;
}

// ─────────────────────────────────────────────────────────────────────────────
// JSON（默认）
// ─────────────────────────────────────────────────────────────────────────────

/// JSON 编解码（03 §6 默认格式）。同时实现 [`Serializer`] 与 [`Deserializer`]。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JsonCodec;

impl JsonCodec {
    /// 固有方法：避免 `Serializer::format` / `Deserializer::format` 同名歧义。
    pub fn format(&self) -> Format {
        Format::Json
    }
}

impl Serializer for JsonCodec {
    fn format(&self) -> Format {
        Format::Json
    }

    fn serialize_envelope(&self, envelope: &Envelope) -> Result<Vec<u8>, ProtocolError> {
        serde_json::to_vec(envelope).map_err(Into::into)
    }

    fn serialize_dead_letter(&self, dead_letter: &DeadLetterMeta) -> Result<Vec<u8>, ProtocolError> {
        serde_json::to_vec(dead_letter).map_err(Into::into)
    }
}

impl Deserializer for JsonCodec {
    fn format(&self) -> Format {
        Format::Json
    }

    fn deserialize_envelope(&self, bytes: &[u8]) -> Result<Envelope, ProtocolError> {
        serde_json::from_slice(bytes)
            .map_err(|e| ProtocolError::Deserialize(e.to_string()))
    }

    fn deserialize_dead_letter(&self, bytes: &[u8]) -> Result<DeadLetterMeta, ProtocolError> {
        serde_json::from_slice(bytes)
            .map_err(|e| ProtocolError::Deserialize(e.to_string()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MessagePack（配置项）
// ─────────────────────────────────────────────────────────────────────────────

/// MessagePack 编解码（03 §6 / §9-4「MessagePack 作为配置项」）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MsgPackCodec;

impl MsgPackCodec {
    /// 固有方法：避免 `Serializer::format` / `Deserializer::format` 同名歧义。
    pub fn format(&self) -> Format {
        Format::MessagePack
    }
}

impl Serializer for MsgPackCodec {
    fn format(&self) -> Format {
        Format::MessagePack
    }

    fn serialize_envelope(&self, envelope: &Envelope) -> Result<Vec<u8>, ProtocolError> {
        rmp_serde::to_vec_named(envelope).map_err(Into::into)
    }

    fn serialize_dead_letter(&self, dead_letter: &DeadLetterMeta) -> Result<Vec<u8>, ProtocolError> {
        rmp_serde::to_vec_named(dead_letter).map_err(Into::into)
    }
}

impl Deserializer for MsgPackCodec {
    fn format(&self) -> Format {
        Format::MessagePack
    }

    fn deserialize_envelope(&self, bytes: &[u8]) -> Result<Envelope, ProtocolError> {
        rmp_serde::from_slice(bytes).map_err(|e| ProtocolError::Deserialize(e.to_string()))
    }

    fn deserialize_dead_letter(&self, bytes: &[u8]) -> Result<DeadLetterMeta, ProtocolError> {
        rmp_serde::from_slice(bytes).map_err(|e| ProtocolError::Deserialize(e.to_string()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 运行期选择（配置粒度 = 主题族）
// ─────────────────────────────────────────────────────────────────────────────

/// 同时实现两个 trait 的编解码器，供 backend 运行期按主题族选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnyCodec {
    /// JSON。
    Json(JsonCodec),
    /// MessagePack。
    MessagePack(MsgPackCodec),
}

impl AnyCodec {
    /// 固有方法：避免 `Serializer::format` / `Deserializer::format` 同名歧义。
    pub fn format(&self) -> Format {
        match self {
            AnyCodec::Json(_) => Format::Json,
            AnyCodec::MessagePack(_) => Format::MessagePack,
        }
    }

    /// 按格式构造。
    pub fn from_format(format: Format) -> Self {
        match format {
            Format::Json => AnyCodec::Json(JsonCodec),
            Format::MessagePack => AnyCodec::MessagePack(MsgPackCodec),
        }
    }
}

impl Serializer for AnyCodec {
    fn format(&self) -> Format {
        match self {
            AnyCodec::Json(c) => c.format(),
            AnyCodec::MessagePack(c) => c.format(),
        }
    }

    fn serialize_envelope(&self, envelope: &Envelope) -> Result<Vec<u8>, ProtocolError> {
        match self {
            AnyCodec::Json(c) => c.serialize_envelope(envelope),
            AnyCodec::MessagePack(c) => c.serialize_envelope(envelope),
        }
    }

    fn serialize_dead_letter(&self, dead_letter: &DeadLetterMeta) -> Result<Vec<u8>, ProtocolError> {
        match self {
            AnyCodec::Json(c) => c.serialize_dead_letter(dead_letter),
            AnyCodec::MessagePack(c) => c.serialize_dead_letter(dead_letter),
        }
    }
}

impl Deserializer for AnyCodec {
    fn format(&self) -> Format {
        Serializer::format(self)
    }

    fn deserialize_envelope(&self, bytes: &[u8]) -> Result<Envelope, ProtocolError> {
        match self {
            AnyCodec::Json(c) => c.deserialize_envelope(bytes),
            AnyCodec::MessagePack(c) => c.deserialize_envelope(bytes),
        }
    }

    fn deserialize_dead_letter(&self, bytes: &[u8]) -> Result<DeadLetterMeta, ProtocolError> {
        match self {
            AnyCodec::Json(c) => c.deserialize_dead_letter(bytes),
            AnyCodec::MessagePack(c) => c.deserialize_dead_letter(bytes),
        }
    }
}

/// 默认序列化器 = JSON（03 §6）。
pub fn default_codec() -> &'static JsonCodec {
    &JsonCodec
}

/// 序列化格式策略：粒度 = **主题族**（03 §6），由控制面 `Serialization` 域下发（09 §2）。
///
/// ## 固定 JSON 的族（不开放切换）
///
/// * `ctl.*` / `ops.*` —— 09 §2「引导面不能依赖尚未拉到的配置」；
/// * `dlq.<topic>` —— 委托给**原主题**的格式（死信内容就是原事件 + `DeadLetterMeta`）。
///
/// 因此 `overrides` 中即使写入了这三类键也会被忽略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatPolicy {
    /// 默认格式（03 §6：JSON）。
    #[serde(default)]
    pub default: Format,
    /// 主题族前缀（03 §8：`md.` / `signal.` / `factor.` / `alarm.` …）→ 格式。
    #[serde(default)]
    pub overrides: BTreeMap<String, Format>,
}

impl Default for FormatPolicy {
    fn default() -> Self {
        Self {
            default: Format::Json,
            overrides: BTreeMap::new(),
        }
    }
}

impl FormatPolicy {
    /// 全 JSON（默认策略）。
    pub fn all_json() -> Self {
        Self::default()
    }

    /// 族前缀提取：`md.candle.1m` → `md.`；无前缀返回 `None`。
    pub fn family_of(topic: &str) -> Option<&'static str> {
        const FAMILIES: [&str; 7] = ["md.", "signal.", "factor.", "alarm.", "ops.", "ctl.", "dlq."];
        FAMILIES.into_iter().find(|f| topic.starts_with(f))
    }

    /// 解析某主题应使用的格式。
    pub fn format_for_topic(&self, topic: &str) -> Format {
        let Some(family) = Self::family_of(topic) else {
            return self.default;
        };
        match family {
            // 09 §2 Serialization：ctl.* / ops.* 固定 JSON，不开放切换。
            "ctl." | "ops." => Format::Json,
            // 03 §6：死信内容 = 原事件 + DeadLetterMeta，跟随原主题族。
            "dlq." => {
                let origin = topic.strip_prefix("dlq.").unwrap_or(topic);
                self.format_for_topic(origin)
            }
            other => self.overrides.get(other).copied().unwrap_or(self.default),
        }
    }

    /// 按主题取编解码器。
    pub fn codec_for_topic(&self, topic: &str) -> AnyCodec {
        AnyCodec::from_format(self.format_for_topic(topic))
    }
}

/// 按主题取编解码器（默认全 JSON）。
pub fn codec_for_topic(topic: &str) -> AnyCodec {
    FormatPolicy::default().codec_for_topic(topic)
}
