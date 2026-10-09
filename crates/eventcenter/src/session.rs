//! 契约层接口：`EventCenter` / `Session` / `SessionOpts`（03 §3、§4.1、§4.6）。

use crate::capabilities::Capabilities;
use crate::error::ECError;
use ponk_protocol::{Envelope, Group, Topic};
use std::time::Duration;

/// 起始位置（03 §4.6：latest / earliest / 指定偏移）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPosition {
    /// 只收打开会话之后新写入的消息（Redis `XGROUP CREATE … $` 的默认语义）。
    Latest,
    /// 从保留窗口内最早的消息开始读。
    Earliest,
    /// 从指定偏移开始读（backend 相关：内存为分区内序号，Redis 为 stream id 的 ms 部分）。
    Offset(u64),
}

impl Default for StartPosition {
    fn default() -> Self {
        StartPosition::Latest
    }
}

impl StartPosition {
    /// Redis `XGROUP CREATE` 的起始 id 参数。
    #[cfg(feature = "redis")]
    pub(crate) fn redis_group_id(&self) -> String {
        match self {
            StartPosition::Latest => "$".to_string(),
            StartPosition::Earliest => "0".to_string(),
            StartPosition::Offset(n) => format!("{n}-0"),
        }
    }
}

/// 会话选项（03 §4.6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOpts {
    /// 消费超时：[`Session::next`] 无消息时最多等这么久，到点返回 `Ok(None)`（03 §4.1）。
    pub timeout: Duration,
    /// 起始位置（03 §4.6）。
    pub start: StartPosition,
}

impl Default for SessionOpts {
    fn default() -> Self {
        SessionOpts {
            timeout: Duration::from_millis(500),
            start: StartPosition::Latest,
        }
    }
}

/// 读取会话（03 §4.1）。
///
/// **无 ack**：`next` 返回消息即推进消费位点，不提供确认原语（03 §4.4）。
pub trait Session: Send {
    /// 按调用方节奏取消息；`Ok(None)` 表示**超时无消息，非结束**。
    fn next(&mut self, timeout: Duration) -> Result<Option<Envelope>, ECError>;

    /// 关闭会话。
    fn close(&mut self) -> Result<(), ECError>;
}

/// 三个原语之一（03 §4.1）。
///
/// 抽象分两层：**契约层**（本 trait，所有 backend 必须满足）+ **能力层**
/// （[`capabilities`](EventCenter::capabilities)，上层按能力分支，03 §2）。
pub trait EventCenter: Send + Sync {
    /// 发布。数据信道、命令信道、控制信道共用，不做区分（03 §4.1 / 10 §1「单一传输」）。
    ///
    /// 写入侧 at-least-once：失败的事件会写入 `dlq.<topic>`（03 §7.3），
    /// 调用方仍收到错误。
    fn publish(&self, topic: &Topic, envelope: &Envelope) -> Result<(), ECError>;

    /// 打开「消费组 × 主题」的读取会话。
    fn open(
        &self,
        topic: &Topic,
        group: &Group,
        opts: SessionOpts,
    ) -> Result<Box<dyn Session>, ECError>;

    /// 能力查询（03 §4.3、§4.5）。
    fn capabilities(&self) -> Capabilities;
}

/// 死信读取（03 §4.1，独立于 [`Session`]；03 §7.4：死信只堆积、不常驻消费，
/// 由**运维工具**查看与人工重投）。
///
/// 为什么单独一条读取面：`dlq.*` 的线负载是 [`DeadLetter`]（内含原 `Envelope`），
/// **类型就不是 `Envelope`**，塞进 `Session::next` 的返回类型只会破坏契约层。
pub trait DeadLetterReader {
    /// 取某原主题对应的死信（`dlq.<topic>`），按写入顺序，**至多 `limit` 条**。
    fn dead_letters(
        &self,
        topic: &Topic,
        limit: usize,
    ) -> Result<Vec<ponk_protocol::DeadLetter>, ECError>;
}
