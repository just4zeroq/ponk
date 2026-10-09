//! 控制面错误（`docs/09-控制面.md` 各流程的失败出口）。

use ponk_eventcenter::ECError;
use ponk_protocol::ProtocolError;

/// 控制面错误。
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// 文件存储读写失败（09 §4.2「版本化文件树」后端）。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// JSON 解析 / 序列化失败。
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// 协议层（信封 / 编解码）错误。
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),

    /// 事件中心 backend 错误。
    #[error("eventcenter: {0}")]
    Ec(#[from] ECError),

    /// 数据库错误（`docs/12`：SQLite 持久层）。
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),

    /// 写入侧校验未通过（09 §4.3 步骤 ①：schema + 跨域引用）。
    #[error("validation: {0}")]
    Validation(String),

    /// 目标不存在（域 / 版本 / 实例）。
    #[error("not found: {0}")]
    NotFound(String),

    /// 状态不允许该操作（09 §4.5 硬约束等）。
    #[error("illegal state: {0}")]
    Illegal(String),
}

impl ControlError {
    /// 便捷构造校验错误。
    pub fn validation(msg: impl Into<String>) -> Self {
        ControlError::Validation(msg.into())
    }

    /// 便捷构造「不存在」。
    pub fn not_found(msg: impl Into<String>) -> Self {
        ControlError::NotFound(msg.into())
    }
}
