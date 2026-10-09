//! Kafka backend —— **骨架，未实现、未实测**（03 §7.1 / §9-3）。
//!
//! 03 §7.1 把 Kafka 定位为「放量候选，引入 JVM 运行时」；本仓库**不引入任何 Kafka 客户端依赖**，
//! 也不声称已实现：所有操作返回明确的 [`ECError::Unsupported`]，
//! 能力表 `measured = false`（03 §4.5「能力取值来自实测而非手写标记」）。
//!
//! 03 §7.1 对它的能力定案（供将来实测核对）：**分区、重放、死信、持久化**。

use crate::capabilities::Capabilities;
use crate::error::ECError;
use crate::session::{EventCenter, Session, SessionOpts};
use ponk_protocol::{Envelope, Group, Topic};

const UNSUPPORTED: &str =
    "kafka backend 仅为骨架（03 §7.1 放量候选）：未接入客户端、未实测，见交付报告";

/// Kafka backend 骨架。
#[derive(Debug, Default, Clone, Copy)]
pub struct KafkaEventCenter;

impl EventCenter for KafkaEventCenter {
    fn publish(&self, _topic: &Topic, _envelope: &Envelope) -> Result<(), ECError> {
        Err(ECError::Unsupported(UNSUPPORTED.to_string()))
    }

    fn open(
        &self,
        _topic: &Topic,
        _group: &Group,
        _opts: SessionOpts,
    ) -> Result<Box<dyn Session>, ECError> {
        Err(ECError::Unsupported(UNSUPPORTED.to_string()))
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::untested(
            "kafka",
            &[
                "03 §7.1 定案：分区、重放、死信、持久化",
                "未实现：无 Kafka 客户端依赖，publish/open 返回 Unsupported",
                "未实测：能力表 measured=false（03 §4.5）",
            ],
        );
        caps.notes.push("03 §7.1 待实测核对的目标值：partitioning=true".into());
        caps
    }
}
