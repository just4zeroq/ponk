//! 协议层错误。

use thiserror::Error;

/// 协议层（序列化 / 规范化 / 信封校验）错误。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("serialize failed: {0}")]
    Serialize(String),

    #[error("deserialize failed: {0}")]
    Deserialize(String),

    #[error("canonical view failed: {0}")]
    Canonical(String),

    #[error("event_type {event_type} 与 body {body} 不匹配")]
    TypeMismatch { event_type: String, body: String },

    #[error("invalid envelope: {0}")]
    Invalid(String),
}

impl From<serde_json::Error> for ProtocolError {
    fn from(e: serde_json::Error) -> Self {
        ProtocolError::Serialize(e.to_string())
    }
}

impl From<rmp_serde::encode::Error> for ProtocolError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        ProtocolError::Serialize(e.to_string())
    }
}

impl From<rmp_serde::decode::Error> for ProtocolError {
    fn from(e: rmp_serde::decode::Error) -> Self {
        ProtocolError::Deserialize(e.to_string())
    }
}
