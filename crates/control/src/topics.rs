//! 主题命名（`docs/03-事件中心.md` §8 / `docs/09-控制面.md` §8）。
//!
//! | 主题 | 用途 | 方向 |
//! |---|---|---|
//! | `ctl.<domain>` | 配置变更通知 `ConfigChanged` | 控制面 → 广播 |
//! | `ctl.request.<instance>` | 拉取请求（携带 `applied_version`） | 实例 → 控制面 |
//! | `ctl.<node_kind>.<instance>` | 定向命令与拉取应答 | 控制面 → 单实例 |
//! | `ops.<node_kind>.<instance>` | register / heartbeat / shutdown | 实例 → 控制面 |
//! | `ops.control` | 控制面自身心跳（09 §6.5） | 控制面 → 广播 |
//!
//! 09 §6.1：按实例命名的主题**由控制面在 register 时建立会话**，shutdown/失联后关闭。

use ponk_protocol::{ConfigDomain, NodeKind, Topic};

/// `NodeKind` → 线格式名（与协议 `#[serde(rename_all = "snake_case")]` 一致）。
pub fn node_kind_str(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Collector => "collector",
        NodeKind::Signal => "signal",
        NodeKind::Storage => "storage",
        NodeKind::Alert => "alert",
        NodeKind::Factor => "factor",
        NodeKind::DataSvc => "data_svc",
        NodeKind::Control => "control",
    }
}

/// `ctl.<domain>`：配置变更通知（09 §4.3 步骤 ②）。
pub fn changed(domain: ConfigDomain) -> Topic {
    Topic::new(format!("ctl.{}", domain.as_str()))
}

/// `ctl.request.<instance>`：拉取请求（03 §8 / 10 §4.1）。
pub fn request(instance: &str) -> Topic {
    Topic::new(format!("ctl.request.{instance}"))
}

/// `ctl.<node_kind>.<instance>`：定向命令与拉取应答（03 §8 通则）。
pub fn to_node(kind: NodeKind, instance: &str) -> Topic {
    Topic::new(format!("ctl.{}.{}", node_kind_str(kind), instance))
}

/// `ops.<node_kind>.<instance>`：register / heartbeat / shutdown（10 §5）。
pub fn ops_of(kind: NodeKind, instance: &str) -> Topic {
    Topic::new(format!("ops.{}.{}", node_kind_str(kind), instance))
}

/// `ops.<node_kind>`：按种类聚合的控制信道（03 §8 `ops.<node>` 通则）。
///
/// 仅作为**注册发现**的补充入口（09 §6.1 通配订阅按能力表可选，见交付报告的规格缺口清单）。
pub fn ops_kind(kind: NodeKind) -> Topic {
    Topic::new(format!("ops.{}", node_kind_str(kind)))
}

/// `ops.control`：控制面自身心跳（09 §6.5）。
pub fn control_heartbeat() -> Topic {
    Topic::new("ops.control")
}

/// `md.meta`：交易对 + 币参考信息（09 §2.3 定时任务产物第 3 路 / 03 §8 `md.<channel>`）。
///
/// 控制面是该事件的生产者；**经总线**发布，不直连存储后端（01 §6 红线 4）。
pub fn md_meta() -> Topic {
    Topic::new(format!("md.{}", ponk_protocol::Channel::META))
}

/// `ctl.request.<instance>` 的实例 id（反解）。
pub fn instance_of_request(topic: &str) -> Option<&str> {
    topic.strip_prefix("ctl.request.")
}

/// `ops.<kind>.<instance>` 的 `(kind, instance)`（反解）。
pub fn instance_of_ops(topic: &str) -> Option<(NodeKind, &str)> {
    let rest = topic.strip_prefix("ops.")?;
    let (kind, instance) = rest.split_once('.')?;
    let node_kind = match kind {
        "collector" => NodeKind::Collector,
        "signal" => NodeKind::Signal,
        "storage" => NodeKind::Storage,
        "alert" => NodeKind::Alert,
        "factor" => NodeKind::Factor,
        "data_svc" => NodeKind::DataSvc,
        "control" => NodeKind::Control,
        _ => return None,
    };
    Some((node_kind, instance))
}
