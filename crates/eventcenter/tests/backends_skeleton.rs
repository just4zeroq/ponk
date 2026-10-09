//! backend 骨架与未实测能力表的行为约束（03 §4.5 / §7.1 / §9-3）。
//!
//! Kafka / NATS / Iggy **只做骨架**：不假装实现 —— 未实现方法必须返回明确的
//! `Unsupported`，能力表必须标注未实测（`measured = false`）。

#![cfg(any(feature = "kafka", feature = "nats", feature = "iggy"))]

use ponk_eventcenter::{ECError, EventCenter, SessionOpts};
use ponk_protocol::{Body, Envelope, EventType, OpsHeartbeat, Topic};
use std::time::Duration;

fn fixture() -> Envelope {
    let body = Body::Ops(OpsHeartbeat {
        host: None,
        instance_id: "x".into(),
        node_kind: ponk_protocol::NodeKind::Control,
        status: ponk_protocol::Status::Startup,
        applied_version: None,
        counters: Default::default(),
        started_at: 0,
        ts: 1,
    });
    Envelope::new(EventType::Ops, "probe", 1, body).unwrap()
}

fn assert_untested_skeleton(center: &dyn EventCenter, backend: &str) {
    let caps = center.capabilities();
    assert_eq!(caps.backend, backend);
    assert!(
        !caps.measured,
        "03 §4.5：能力取值来自实测 —— 未实测的 backend 必须标 measured=false"
    );
    assert_eq!(caps.retention, ponk_eventcenter::Retention::Unverified);
    assert!(
        !caps.notes.is_empty(),
        "必须写明未实现 / 未实测的原因"
    );

    let topic = Topic::new("md.trade");
    let err = center
        .publish(&topic, &fixture())
        .expect_err("骨架 backend 不得假装发布成功");
    assert!(
        matches!(err, ECError::Unsupported(_)),
        "publish 必须返回明确的 Unsupported：{err:?}"
    );

    let err = center
        .open(&topic, &"g".into(), SessionOpts::default())
        .err()
        .expect("骨架 backend 不得假装开得了会话");
    assert!(
        matches!(err, ECError::Unsupported(_)),
        "open 必须返回明确的 Unsupported：{err:?}"
    );
    let _ = Duration::from_millis(1);
}

#[cfg(feature = "kafka")]
#[test]
fn kafka_is_skeleton_only() {
    assert_untested_skeleton(&ponk_eventcenter::KafkaEventCenter, "kafka");
}

#[cfg(feature = "nats")]
#[test]
fn nats_is_skeleton_only() {
    assert_untested_skeleton(&ponk_eventcenter::NatsEventCenter, "nats_jetstream");
}

#[cfg(feature = "iggy")]
#[test]
fn iggy_is_skeleton_only() {
    assert_untested_skeleton(&ponk_eventcenter::IggyEventCenter, "iggy");
}

/// 03 §7.1 的能力定案必须以「未实测」的形式出现在能力表里，供实测时核对。
#[cfg(feature = "kafka")]
#[test]
fn kafka_capability_claims_come_from_spec_not_measurement() {
    let caps = ponk_eventcenter::KafkaEventCenter.capabilities();
    let notes = caps.notes.join("\n");
    assert!(notes.contains("03 §7.1"), "必须引用 03 §7.1 的能力定案");
    assert!(notes.contains("未实测"), "必须显式标注未实测");
}
