//! Iggy backend —— **骨架，未实现、未实测**（03 §7.1 / §9-3）。
//!
//! 03 §7.1 定位：「Rust 原生，与平台同语言栈，**能力待实测**」。
//! 本仓库**不引入 Iggy 客户端依赖**，所有操作返回明确的 [`ECError::Unsupported`]，
//! 能力表 `measured = false`（03 §4.5）。
//!
//! 03 §7.1 对它的能力定案（供将来实测核对）：**分区、持久化、保留、消费组、按偏移读**。

use crate::capabilities::Capabilities;
use crate::error::ECError;
use crate::session::{EventCenter, Session, SessionOpts};
use ponk_protocol::{Envelope, Group, Topic};

const UNSUPPORTED: &str =
    "iggy backend 仅为骨架（03 §7.1 能力待实测）：未接入客户端、未实测，见交付报告";

/// Iggy backend 骨架。
#[derive(Debug, Default, Clone, Copy)]
pub struct IggyEventCenter;

impl EventCenter for IggyEventCenter {
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
        Capabilities::untested(
            "iggy",
            &[
                "03 §7.1 定案：分区、持久化、保留、消费组、按偏移读（能力待实测）",
                "未实现：无 Iggy 客户端依赖，publish/open 返回 Unsupported",
                "未实测：能力表 measured=false（03 §4.5）",
            ],
        )
    }
}
