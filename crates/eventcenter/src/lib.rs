//! # ponk-eventcenter —— 事件中心抽象 + backend 适配
//!
//! 落地 `docs/03-事件中心.md`：
//!
//! * **契约层**（03 §3）：at-least-once 写入 / 尽力有序 / 消费组；**消费侧无 ack**（§4.4）；
//! * **接口**（03 §4.1）：[`EventCenter`] { [`publish`](EventCenter::publish),
//!   [`open`](EventCenter::open), [`capabilities`](EventCenter::capabilities) }、
//!   [`Session`] { [`next`](Session::next), [`close`](Session::close) }；
//! * **订阅 = 拉取的语法糖**（03 §4.2）：[`subscribe`]；
//! * **能力层**（03 §4.5）：[`Capabilities`]，取值来自实测、未实测者显式标注；
//! * **backend**（03 §7）：内存（完整语义）、Redis Streams（feature 门控）、
//!   Kafka / NATS / Iggy（**仅骨架**，未实现、未实测）。
//!
//! 本 crate 是**库，不是可部署组件**（03 §1）：它以库的形式编入每个节点进程，
//! 消息中间件 runtime（Redis / Kafka / NATS / Iggy 服务端）是外部组件。

pub mod backends;
pub mod capabilities;
pub mod error;
pub mod session;
pub mod subscribe;

pub use backends::{MemoryEventCenter, MemoryOptions};
pub use capabilities::{Capabilities, Retention};
pub use error::ECError;
pub use session::{
    DeadLetterReader, EventCenter, Session, SessionOpts, StartPosition,
};
pub use subscribe::{subscribe, SubscriptionHandle};

#[cfg(feature = "iggy")]
pub use backends::IggyEventCenter;
#[cfg(feature = "kafka")]
pub use backends::KafkaEventCenter;
#[cfg(feature = "nats")]
pub use backends::NatsEventCenter;
#[cfg(feature = "redis")]
pub use backends::{RedisEventCenter, RedisOptions};

/// 主题族 → 便捷构造（03 §8 主题命名），避免各节点手写字符串。
pub mod topics {
    use ponk_protocol::Topic;

    /// `md.<channel>`（03 §8）。
    pub fn md(channel: &str) -> Topic {
        Topic::new(format!("md.{channel}"))
    }

    /// `signal.<rule>`。
    pub fn signal(rule: &str) -> Topic {
        Topic::new(format!("signal.{rule}"))
    }

    /// `factor.<rule>`。
    pub fn factor(rule: &str) -> Topic {
        Topic::new(format!("factor.{rule}"))
    }

    /// `alarm.<rule>`。
    pub fn alarm(rule: &str) -> Topic {
        Topic::new(format!("alarm.{rule}"))
    }

    /// `ops.<node>`。
    pub fn ops(node: &str) -> Topic {
        Topic::new(format!("ops.{node}"))
    }

    /// `ctl.<domain>`：配置变更通知（03 §8 / 09 §4.3）。
    pub fn ctl(domain: &str) -> Topic {
        Topic::new(format!("ctl.{domain}"))
    }

    /// `ctl.<node_kind>.<instance>`：定向命令与拉取应答（03 §8 通则；
    /// `ctl.collector.<instance>` 是它的特例，另有 `ctl.signal.*`、`ctl.alert.*` 等）。
    ///
    /// `node_kind` 用 [`ponk_protocol::NodeKind`] 的 snake_case 线格式
    /// （`collector` / `signal` / `storage` / `alert` / `factor` / `data_svc` / `control`）。
    pub fn ctl_instance(node_kind: &str, instance: &str) -> Topic {
        Topic::new(format!("ctl.{node_kind}.{instance}"))
    }

    /// `ctl.request.<instance>`：拉取请求，携带 `applied_version`（03 §8 / 10 §4.1）。
    pub fn ctl_request(instance: &str) -> Topic {
        Topic::new(format!("ctl.request.{instance}"))
    }

    /// `dlq.<topic>`（03 §8 / §7.3）。
    pub fn dlq(origin: &Topic) -> Topic {
        origin.to_dead_letter()
    }
}
