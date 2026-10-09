//! 心跳（10 §5 控制信道）。
//!
//! | 信号 | 时机 | 主题 |
//! |---|---|---|
//! | register / **Startup** | 一次（进程起来、应用计划之前） | `ops.collector.<instance>` |
//! | **Running** | 每 5s | 同上 |
//! | **Shutdown** | 一次（正常下线；缺失即失联判定） | 同上 |
//!
//! 心跳内容 = `applied_version`（隐式 ACK，10 §4.3）+ 在线计数器（10 §5：
//! **发送积压、降级计数**；**不报静默**——静默由 06 观测 `md.*` 得出，06 §5）。
//!
//! `applied_version` 取自 [`crate::plan::PlanState`]（经共享原子量）：
//! 本地文件启动时 = 文件里的 `assignment_version`，控制面下发后用
//! `CtlMessage::SubscribePlan::assignment_version` 覆盖（10 §4.3 隐式 ACK）。
//! 心跳因此**必须读当前值而不是构造时的快照**，否则控制面永远看不到收敛。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ponk_protocol::Status;
use tokio::sync::watch;

use crate::counters::Counters;
use crate::envelope::{heartbeat_envelope, heartbeat_topic};
use crate::error::Result;
use crate::sink::EventSink;

/// 心跳发送器：`Startup` 一次 → 每 `interval` 一次 `Running` → `Shutdown` 一次。
pub struct Heartbeat {
    instance_id: String,
    /// 实例所在机器（register 上报；控制面按 host 分散分片，09 §2.3）。
    host: Option<String>,
    /// 已应用版本；`0` 表示「尚未应用任何版本」（发出 `None`）。
    /// 与控制面客户端共享同一原子量（[`crate::command::ControlLink`]）。
    applied: Arc<AtomicU64>,
    started_at: u64,
    interval: Duration,
    counters: Arc<Counters>,
    sink: Arc<dyn EventSink>,
}

impl Heartbeat {
    pub fn new(
        instance_id: impl Into<String>,
        applied_version: Option<u64>,
        started_at: u64,
        interval: Duration,
        counters: Arc<Counters>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            host: None,
            applied: Arc::new(AtomicU64::new(applied_version.unwrap_or(0))),
            started_at,
            interval,
            counters,
            sink,
        }
    }

    /// 用**共享**的已应用版本构造（控制面下发计划后由 `ControlLink` 更新）。
    pub fn with_applied_handle(
        instance_id: impl Into<String>,
        applied: Arc<AtomicU64>,
        started_at: u64,
        interval: Duration,
        counters: Arc<Counters>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            host: None,
            applied,
            started_at,
            interval,
            counters,
            sink,
        }
    }

    /// 设置上报的机器名（register 心跳带 `host`，控制面据此分散分片）。
    pub fn with_host(mut self, host: Option<String>) -> Self {
        self.host = host;
        self
    }

    /// 当前生效版本（`0` → `None`，即「尚未应用任何版本」）。
    pub fn applied_version(&self) -> Option<u64> {
        match self.applied.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }

    fn emit(&self, status: Status, now_ms: u64) -> Result<()> {
        let env = heartbeat_envelope(
            &self.instance_id,
            self.host.as_deref(),
            status,
            self.applied_version(),
            self.counters.snapshot(),
            self.started_at,
            now_ms,
        )?;
        let topic = heartbeat_topic(&self.instance_id);
        self.sink
            .publish(&topic, &env)
            .map_err(|e| crate::Error::Publish {
                topic: topic.as_str().to_string(),
                msg: e,
            })?;
        self.counters.heartbeats.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 立即发一次指定状态的心跳（不等周期）。
    pub fn emit_now(&self, status: Status) -> Result<()> {
        self.emit(status, now_ms())
    }

    /// 周期循环，直到 `shutdown` 变为 `true`；退出前发 `Shutdown` 心跳。
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        // 10 §7：先 register（Startup，applied_version 已带本地计划版本）。
        if let Err(e) = self.emit_now(Status::Startup) {
            tracing::warn!(target: "ponk::collect::heartbeat", "startup heartbeat failed: {e}");
        }

        if *shutdown.borrow() {
            if let Err(e) = self.emit_now(Status::Shutdown) {
                tracing::warn!(target: "ponk::collect::heartbeat", "shutdown heartbeat failed: {e}");
            }
            return;
        }

        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 立即触发的第一次 tick 对应「刚发过 Startup」，跳过。
        tick.tick().await;

        // 分支体内不读 `shutdown`：它在同一 select! 的另一分支被 changed() 可变借用。
        loop {
            let mut shutdown_seen = false;
            tokio::select! {
                _ = tick.tick() => {}
                _ = shutdown.changed() => { shutdown_seen = true; }
            }
            if shutdown_seen || *shutdown.borrow() {
                break;
            }
            if let Err(e) = self.emit_now(Status::Running) {
                tracing::warn!(target: "ponk::collect::heartbeat", "heartbeat failed: {e}");
            }
        }

        if let Err(e) = self.emit_now(Status::Shutdown) {
            tracing::warn!(target: "ponk::collect::heartbeat", "shutdown heartbeat failed: {e}");
        }
    }
}

/// 当前毫秒时间戳（UTC epoch ms）。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::RecordingSink;
    use ponk_protocol::{Body, EventType, Status};

    #[tokio::test(start_paused = true)]
    async fn emits_startup_running_then_shutdown() {
        let sink = Arc::new(RecordingSink::new());
        let counters = Arc::new(Counters::new());
        let hb = Heartbeat::new(
            "collector-01",
            Some(7),
            1_000,
            Duration::from_millis(5_000),
            counters.clone(),
            sink.clone(),
        );
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(hb.run(rx));

        // 驱动：Startup 立即发出，之后每 5s 一条 Running。
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(sink.count(), 1, "第一条应是 Startup");

        tokio::time::sleep(Duration::from_millis(6_000)).await;
        assert!(sink.count() >= 2, "5s 后应有 Running，got {}", sink.count());

        tx.send(true).unwrap();
        handle.await.unwrap();

        let envs = sink.envelopes();
        let statuses: Vec<Status> = envs
            .iter()
            .map(|e| match &e.body {
                Body::Ops(hb) => hb.status,
                other => panic!("unexpected {:?}", other),
            })
            .collect();
        assert_eq!(statuses.first(), Some(&Status::Startup));
        assert_eq!(statuses.last(), Some(&Status::Shutdown));
        assert!(statuses.iter().skip(1).all(|s| *s == Status::Running || *s == Status::Shutdown));
        assert!(statuses.iter().any(|s| *s == Status::Running));
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_carries_applied_version_and_counters() {
        let sink = Arc::new(RecordingSink::new());
        let counters = Arc::new(Counters::new());
        counters.published.fetch_add(42, std::sync::atomic::Ordering::Relaxed);
        counters.send_backlog.store(3, std::sync::atomic::Ordering::Relaxed);
        let hb = Heartbeat::new(
            "c1",
            Some(7),
            1_000,
            Duration::from_millis(5_000),
            counters,
            sink.clone(),
        );
        hb.emit_now(Status::Running).unwrap();

        let env = &sink.envelopes()[0];
        assert_eq!(env.event_type, EventType::Ops);
        match &env.body {
            Body::Ops(h) => {
                assert_eq!(h.applied_version, Some(7));
                assert_eq!(h.node_kind, ponk_protocol::NodeKind::Collector);
                assert_eq!(h.counters["published"], 42);
                assert_eq!(h.counters["send_backlog"], 3);
                assert_eq!(h.started_at, 1_000);
            }
            other => panic!("{:?}", other),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn topic_is_ops_collector_instance() {
        let sink = Arc::new(RecordingSink::new());
        let hb = Heartbeat::new(
            "collector-77",
            None,
            0,
            Duration::from_secs(5),
            Arc::new(Counters::new()),
            sink.clone(),
        );
        hb.emit_now(Status::Startup).unwrap();
        assert_eq!(sink.topics(), vec!["ops.collector.collector-77".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_publish_is_surfaced_as_error() {
        let sink = Arc::new(RecordingSink::new());
        sink.fail_always();
        let hb = Heartbeat::new(
            "c",
            None,
            0,
            Duration::from_secs(5),
            Arc::new(Counters::new()),
            sink,
        );
        let err = hb.emit_now(Status::Startup).unwrap_err();
        assert!(err.to_string().contains("ops.collector.c"), "{err}");
    }
}
