//! 能力表（03 §4.5）。
//!
//! > 能力取值来自**实测**而非手写标记。
//!
//! 因此 [`Capabilities::measured`] 是能力表的一等字段：
//!
//! * `measured = true` —— 取值来自本仓库可执行的测试（内存 backend 见 `tests/memory_semantics.rs`），
//!   或对该 backend 的实测核对（03 §9-6「四款 backend 能力表实测核对」）；
//! * `measured = false` —— **标注「未实测」**，Kafka / NATS / Iggy 骨架一律如此（03 §7.1）。

use serde::{Deserialize, Serialize};

/// 保留策略（03 §4.5 `retention`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// 仅驻内存，进程退出即丢。
    Volatile,
    /// 按条数截断（如 Redis `XTRIM` / `MAXLEN`，03 §7.4）。
    MaxLen(usize),
    /// 按时间保留（如 Kafka retention）。
    MaxAgeMs(u64),
    /// backend 自带持久化 + 保留配置。
    Persistent,
    /// 未实测 / 未知。
    Unverified,
}

impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Retention::Volatile => write!(f, "volatile"),
            Retention::MaxLen(n) => write!(f, "max_len={n}"),
            Retention::MaxAgeMs(ms) => write!(f, "max_age_ms={ms}"),
            Retention::Persistent => write!(f, "persistent"),
            Retention::Unverified => write!(f, "unverified"),
        }
    }
}

/// 能力表（03 §4.5 四项 + 实测标记）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// backend 名。
    pub backend: String,
    /// `partitioning`：是否支持分区、可配分区数。
    pub partitioning: bool,
    /// 分区数（`partitioning = false` 时为 `None`）。
    pub partitions: Option<u32>,
    /// `replay`：是否保留消息、是否支持按时间或偏移重放。
    pub replay: bool,
    /// `dead_letter`：能否按主题建立独立死信流（**不含自动重投**，03 §7.4）。
    pub dead_letter: bool,
    /// `retention`：持久化与保留策略。
    pub retention: Retention,
    /// 取值是否来自实测（03 §4.5）。
    pub measured: bool,
    /// 说明 / 未实测原因。
    pub notes: Vec<String>,
}

impl Capabilities {
    /// 未实测的能力表（03 §4.5：能力取值来自实测 —— 故一律 `measured = false`）。
    pub fn untested(backend: &str, notes: &[&str]) -> Self {
        Capabilities {
            backend: backend.to_string(),
            partitioning: false,
            partitions: None,
            replay: false,
            dead_letter: false,
            retention: Retention::Unverified,
            measured: false,
            notes: notes.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 内存 backend 能力表（`tests/memory_semantics.rs` 逐项实测核对）。
    pub fn memory(partitions: usize, max_len: Option<usize>) -> Self {
        Capabilities {
            backend: "memory".to_string(),
            partitioning: true,
            partitions: Some(partitions as u32),
            replay: true,
            dead_letter: true,
            retention: match max_len {
                Some(n) => Retention::MaxLen(n),
                None => Retention::Volatile,
            },
            measured: true,
            notes: vec![
                "进程内实现；能力值由 tests/memory_semantics.rs 实测核对".to_string(),
                "replay = 按偏移起读（StartPosition::Offset），受保留窗口限制".to_string(),
            ],
        }
    }

    /// Redis Streams 能力表（03 §7.1：**无分区、无重放、XAUTOCLAIM 死信**）。
    pub fn redis_streams() -> Self {
        Capabilities {
            backend: "redis_streams".to_string(),
            partitioning: false,
            partitions: None,
            replay: false,
            dead_letter: true,
            retention: Retention::Unverified,
            // 03 §7.1 的能力表来自文档定案；本仓库无本地 Redis，未在 CI 实测核对。
            measured: false,
            notes: vec![
                "03 §7.1：无分区、XAUTOCLAIM 死信".to_string(),
                "replay=false 但保留窗口内可回读：`XGROUP SETID`（改起读偏移）与                  `XRANGE`（死信读取）可用，受 XADD MAXLEN 截断限制，**不承诺跨会话重放**"
                    .to_string(),
                "未实测：测试环境无 Redis 服务（见交付报告）".to_string(),
            ],
        }
    }
}
