//! Redis backend 的**离线**检查（feature `redis`）。
//!
//! 本仓库测试环境**没有本地 Redis 服务**，且测试不得依赖外网，因此这里只验证
//! 不需要服务端的部分：客户端构造、能力表标注、以及连接失败时的错误路径
//! （含 03 §7.3「发布失败 → 写 DLQ」的 best-effort 分支）。
//!
//! **消费组 / XAUTOCLAIM / DLQ 的运行时行为未实测**，见交付报告。

#![cfg(feature = "redis")]

use ponk_eventcenter::{DeadLetterReader, ECError, EventCenter, RedisEventCenter, SessionOpts};
use ponk_protocol::{Body, Envelope, EventType, OpsHeartbeat, Topic};

/// 一个**没有监听**的本机端口：连接会被立刻拒绝，不涉及外网。
const DEAD_URL: &str = "redis://127.0.0.1:1/";

fn fixture() -> Envelope {
    let body = Body::Ops(OpsHeartbeat {
        host: None,
        instance_id: "probe".into(),
        node_kind: ponk_protocol::NodeKind::Collector,
        status: ponk_protocol::Status::Running,
        applied_version: None,
        counters: Default::default(),
        started_at: 0,
        ts: 1,
    });
    Envelope::new(EventType::Ops, "probe", 1, body).unwrap()
}

#[test]
fn redis_client_constructs_without_a_server() {
    let ec = RedisEventCenter::from_url(DEAD_URL).expect("Client::open 只解析连接串，不建连接");
    let caps = ec.capabilities();
    assert_eq!(caps.backend, "redis_streams");
    assert!(
        !caps.measured,
        "03 §4.5：本仓库无本地 Redis，能力取值未实测，必须标 measured=false"
    );
    assert!(!caps.partitioning, "03 §7.1：Redis Streams 无分区");
    assert!(!caps.replay, "03 §7.1：Redis Streams 无重放");
    assert!(caps.dead_letter, "03 §7.1：XAUTOCLAIM 死信");
}

#[test]
fn redis_publish_without_server_fails_with_backend_error() {
    let ec = RedisEventCenter::from_url(DEAD_URL).unwrap();
    let topic = Topic::new("md.trade");
    let err = ec.publish(&topic, &fixture()).expect_err("无服务端必然失败");
    assert!(matches!(err, ECError::Backend(_)), "{err:?}");
    // 03 §7.3 的 DLQ 写入在这里也无法完成（同一服务端不可达），错误仍原样返回
}

#[test]
fn redis_open_without_server_fails_with_backend_error() {
    let ec = RedisEventCenter::from_url(DEAD_URL).unwrap();
    let err = ec
        .open(&Topic::new("md.trade"), &"g".into(), SessionOpts::default())
        .err()
        .expect("无服务端必然失败");
    assert!(matches!(err, ECError::Backend(_)), "{err:?}");
}

#[test]
fn redis_dlq_topic_is_not_openable_as_session() {
    let ec = RedisEventCenter::from_url(DEAD_URL).unwrap();
    let err = ec
        .open(
            &Topic::new("dlq.md.trade"),
            &"ops".into(),
            SessionOpts::default(),
        )
        .err()
        .expect("死信主题不得开 Session（03 §7.4）");
    assert!(matches!(err, ECError::Unsupported(_)), "{err:?}");
}

#[test]
fn redis_dead_letters_need_a_server() {
    let ec = RedisEventCenter::from_url(DEAD_URL).unwrap();
    let err = ec
        .dead_letters(&Topic::new("md.trade"), 10)
        .err()
        .expect("无服务端必然失败");
    assert!(matches!(err, ECError::Backend(_)), "{err:?}");
}
