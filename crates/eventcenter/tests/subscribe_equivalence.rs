//! 必测项 ⑤：`subscribe` 与 `next` 两种消费方式等价
//! （03 §4.2「订阅是拉取的语法糖，不是第二条通路」）。

use ponk_eventcenter::{
    subscribe, EventCenter, MemoryEventCenter, SessionOpts, StartPosition,
    SubscriptionHandle,
};
use ponk_protocol::{
    Body, Envelope, EventType, NodeKind, OpsHeartbeat, Status, Topic,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TOPIC: &str = "ops.collector.c1";
const N: usize = 30;

fn ops_env(i: u64) -> Envelope {
    let body = Body::Ops(OpsHeartbeat {
        host: None,
        instance_id: "collector-1".into(),
        node_kind: NodeKind::Collector,
        status: Status::Running,
        applied_version: Some(i),
        counters: Default::default(),
        started_at: 0,
        ts: i,
    });
    Envelope::new(EventType::Ops, "collect-1", i, body).expect("fixture")
}

fn key(e: &Envelope) -> String {
    serde_json::to_string(e).expect("stable key")
}

fn wait_until(mut cond: impl FnMut() -> bool, budget: Duration, what: &str) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("等待超时：{what}");
}

/// ⑤ 两种消费方式在同一主题、同一消费组语义下给出**逐条相同**的结果（含顺序）。
#[test]
fn subscribe_and_pull_are_equivalent() {
    let ec = MemoryEventCenter::new();
    let topic = Topic::new(TOPIC);

    // 两个消费组都必须在发布前建立（默认起始位置 = Latest，03 §4.6）
    let mut pull = ec
        .open(
            &topic,
            &"g-pull".into(),
            SessionOpts {
                timeout: Duration::from_millis(50),
                start: StartPosition::Latest,
            },
        )
        .expect("拉取式 open");

    let pushed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&pushed);
    let handle: SubscriptionHandle = subscribe(&ec, &topic, &"g-sub".into(), move |env| {
        sink.lock().expect("lock").push(key(&env));
    });
    assert!(
        handle.open_error().is_none(),
        "订阅式 open 不应失败：{:?}",
        handle.open_error()
    );

    // 发布 N 条
    for i in 0..N as u64 {
        ec.publish(&topic, &ops_env(i)).expect("publish");
    }

    // 拉取式：手工 loop { session.next() -> handler }
    let mut pulled: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while pulled.len() < N && Instant::now() < deadline {
        match pull.next(Duration::from_millis(50)) {
            Ok(Some(env)) => pulled.push(key(&env)),
            Ok(None) => continue,
            Err(e) => panic!("拉取式 next 出错：{e}"),
        }
    }

    // 订阅式：后台 loop { session.next() -> handler }
    wait_until(
        || pushed.lock().expect("lock").len() >= N,
        Duration::from_secs(5),
        "订阅式收齐 N 条",
    );

    handle.stop();
    handle.join().expect("订阅线程应干净退出");

    let subscribed = pushed.lock().expect("lock").clone();

    assert_eq!(pulled.len(), N, "拉取式收齐");
    assert_eq!(subscribed.len(), N, "订阅式收齐");
    assert_eq!(
        pulled, subscribed,
        "两种消费方式必须给出逐条相同的结果（03 §4.2）"
    );

    // 两种方式都不产生重复（无 ack 语义一致，03 §4.4）
    let mut sorted = pulled.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), N, "不得重复投递");
}

/// 03 §4.2：`subscribe` 共享同一消费组会话语义 —— 同组内已消费的消息不会再投给第二个消费者。
#[test]
fn subscribe_shares_group_cursor_with_pull() {
    let ec = MemoryEventCenter::new();
    let topic = Topic::new(TOPIC);
    let group = "shared".to_string();

    // 先用拉取式建组并消费 10 条
    let mut pull = ec
        .open(
            &topic,
            &group.clone().into(),
            SessionOpts {
                timeout: Duration::from_millis(50),
                start: StartPosition::Latest,
            },
        )
        .unwrap();
    for i in 0..10u64 {
        ec.publish(&topic, &ops_env(i)).unwrap();
    }
    let first = {
        let mut acc = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while acc.len() < 10 && Instant::now() < deadline {
            if let Some(e) = pull.next(Duration::from_millis(50)).unwrap() {
                acc.push(e.received_ts);
            }
        }
        acc
    };
    assert_eq!(first, (0..10).collect::<Vec<u64>>());

    // 再用订阅式加入**同一个组**：只应收到后续消息
    let pushed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&pushed);
    let handle = subscribe(&ec, &topic, &group.clone().into(), move |env| {
        sink.lock().unwrap().push(env.received_ts);
    });
    assert!(handle.open_error().is_none());

    for i in 10..15u64 {
        ec.publish(&topic, &ops_env(i)).unwrap();
    }
    wait_until(
        || pushed.lock().unwrap().len() >= 5,
        Duration::from_secs(5),
        "同组的订阅者收到后续 5 条",
    );
    handle.stop();
    handle.join().unwrap();

    let got = pushed.lock().unwrap().clone();
    assert_eq!(got, (10..15).collect::<Vec<u64>>(), "同组共享位点：历史不再重投");
}

/// `subscribe` 打开会话失败时，错误在句柄上可查（不静默失败）。
#[test]
fn subscribe_surfaces_open_errors() {
    let ec = MemoryEventCenter::new();
    let bad = Topic::new("not.a.registered.family");
    let handle = subscribe(&ec, &bad, &"g".into(), |_env| {});
    let err = handle
        .open_error()
        .expect("非法主题必须在 open 阶段报错");
    assert!(matches!(err, ponk_eventcenter::ECError::InvalidTopic(_)), "{err:?}");
    assert!(handle.is_finished(), "未启动的订阅应直接处于结束态");
}
