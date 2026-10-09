//! 事件中心错误（03 §4.1 `Result<(), ECError>`）。

use ponk_protocol::ProtocolError;
use thiserror::Error;

/// 事件中心错误。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ECError {
    /// 发布失败（03 §7.3：发布失败的事件写入 `dlq.<topic>` 后仍返回本错误）。
    #[error("publish failed: {0}")]
    Publish(String),

    /// 协议层（序列化 / 信封校验）错误。
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),

    /// backend 故障。
    #[error("backend: {0}")]
    Backend(String),

    /// 会话已关闭。
    #[error("session closed")]
    Closed,

    /// 该 backend 不支持此操作。
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// 主题不合法（03 §5.2「避免出现私有主题」/ §8 主题命名）。
    #[error("invalid topic: {0}")]
    InvalidTopic(String),

    /// 消费组不合法。
    #[error("invalid group: {0}")]
    InvalidGroup(String),
}
