//! 命令 / 控制信道客户端（10 §4、§5、§7）。
//!
//! # 采集侧参与的两条通路
//!
//! | 方向 | 主题 | 内容 | 时机 |
//! |---|---|---|---|
//! | 上行（控制信道） | `ops.collector.<instance>` | register / heartbeat / shutdown | 由 [`crate::heartbeat`] 负责 |
//! | 上行（拉取请求） | `ctl.request.<instance>` | 携带 `applied_version` 的请求 | 启动、或版本不收敛时 |
//! | 下行（命令信道） | `ctl.collector.<instance>` | `CtlMessage::SubscribePlan` / `ConfigSnapshot` | 控制面推 / 应答 |
//!
//! # 一轮为什么这么快：**它们自己就是强序门**
//!
//! 10 §4.3 规定命令是「状态补丁而非动作流」：采集不需要为每条计划回 ACK——
//! 心跳里的 `applied_version` 就是隐式 ACK。因此这里**只做两件事**：
//! 把收到的计划交给 [`PlanState`] 裁决，并在版本真正前进时报给上层重建订阅集。
//! 重复投递、乱序抵达、控制面重推，全部由版本门吸收（[`crate::plan`]）。
//!
//! # 拉取请求的载荷
//!
//! 控制面 `handle_pull` 认三种载荷，采采集侧用**第一种**（最省事、语义也最准）：
//! `Body::Ops(OpsHeartbeat)` —— 它已经带了 `node_kind` 与 `applied_version`，
//! 正是控制面判断「该给谁、给什么版本」所需的全部信息。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ponk_eventcenter::EventCenter;
use ponk_protocol::{Body, CtlMessage, Group, Status, SubscribePlan, Topic};
use tokio::sync::watch;

use crate::counters::Counters;
use crate::envelope::{ctl_collector_topic, ctl_request_topic, heartbeat_envelope};
use crate::error::{Error, Result};
use crate::plan::{plan_from_ctl, ApplyOutcome};
use crate::sink::EventSink;

pub use crate::plan::SharedPlan;

/// 控制信道客户端。字段都是 `Arc` / 值类型，可克隆给「拉取重试」任务共享。
#[derive(Clone)]
pub struct ControlLink {
    instance_id: String,
    /// 上报的机器名（与心跳一致，09 §2.3）。
    host: Option<String>,
    started_at: u64,
    /// 已应用版本（心跳读取；计划应用后由本模块更新）。
    applied: Arc<AtomicU64>,
    counters: Arc<Counters>,
    sink: Arc<dyn EventSink>,
    session_timeout: Duration,
}

impl ControlLink {
    /// `applied` 与心跳共享同一个原子量。
    pub fn new(
        instance_id: impl Into<String>,
        host: Option<String>,
        started_at: u64,
        applied: Arc<AtomicU64>,
        counters: Arc<Counters>,
        sink: Arc<dyn EventSink>,
        session_timeout_ms: u64,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            host,
            started_at,
            applied,
            counters,
            sink,
            session_timeout: Duration::from_millis(session_timeout_ms.max(1)),
        }
    }

    /// 10 §7 第 2 步：向 `ctl.request.<instance>` 发拉取请求。
    ///
    /// 载荷 = `OpsHeartbeat`（`Startup` 语义：我是谁 + 我应用到哪版）。
    /// **拉是正确性保证**（10 §4.1）：即便推送全丢，这一次拉取也能拿到当前计划。
    pub fn request_pull(&self) -> Result<()> {
        let env = heartbeat_envelope(
            &self.instance_id,
            self.host.as_deref(),
            Status::Startup,
            Some(self.applied.load(Ordering::Relaxed)),
            self.counters.snapshot(),
            self.started_at,
            crate::heartbeat::now_ms(),
        )?;
        let topic = ctl_request_topic(&self.instance_id);
        self.sink
            .publish(&topic, &env)
            .map_err(|msg| Error::Publish {
                topic: topic.as_str().to_string(),
                msg,
            })
    }

    /// 启动期拉取重试（10 §7 第 2/3 步）：**拉是正确性保证**。
    ///
    /// 从 `baseline`（本地文件里的版本）出发，按 `retry_ms` 重发拉取请求，
    /// 直到 `applied` 前进（控制面给了新版本）或重试次数用尽。
    /// 次数用尽仍无进展只**记警告**，不阻止采集继续跑——采集按本地计划先跑起来
    /// 比空等控制面更符合「拉是兜底、推是加速」的分工（10 §4.1）。
    pub async fn pull_until_applied(
        &self,
        baseline: u64,
        retry_ms: u64,
        max_retries: u32,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut attempt = 0u32;
        loop {
            if *shutdown.borrow() {
                return;
            }
            if self.applied.load(Ordering::Relaxed) != baseline {
                tracing::debug!(
                    target: "ponk::collect::command",
                    version = self.applied.load(Ordering::Relaxed),
                    "plan arrived from control plane"
                );
                return;
            }
            if max_retries != 0 && attempt >= max_retries {
                tracing::warn!(
                    target: "ponk::collect::command",
                    baseline, attempts = attempt,
                    "pull retries exhausted; running on the local plan until the control plane answers"
                );
                return;
            }
            if let Err(e) = self.request_pull() {
                tracing::warn!(target: "ponk::collect::command", "pull request failed: {e}");
            }
            attempt += 1;

            let sleep = tokio::time::sleep(Duration::from_millis(retry_ms.max(1)));
            tokio::pin!(sleep);
            tokio::select! {
                _ = sleep => {}
                _ = shutdown.changed() => return,
            }
        }
    }

    /// 消费 `ctl.<kind>.<instance>`：收到计划 → 版本门裁决 → 上报重建信号。
    ///
    /// `reload` 只在**版本真正前进**时发送（含 `changed = false` 的情况：
    /// 版本要前进、订阅集不必重建，由上层决定）。
    pub async fn run<C: EventCenter>(
        self,
        center: &C,
        state: SharedPlan,
        reload: watch::Sender<Option<Arc<SubscribePlan>>>,
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let topic = ctl_collector_topic(&self.instance_id);
        // 09 §2.2：通知类主题的消费组**以 instance_id 独占** —— 每条都必须到达本人，
        // 不能像数据信道那样多实例分摊。
        let group = Group::new(self.instance_id.clone());
        let mut session = center
            .open(&topic, &group, Default::default())
            .map_err(|e| Error::EventCenter(format!("open {}: {e}", topic.as_str())))?;

        loop {
            if *shutdown.borrow() {
                break;
            }
            match session.next(self.session_timeout) {
                Ok(Some(env)) => {
                    if let Err(e) = self.handle(&env, &state, &reload).await {
                        // 单条坏消息不结束会话：记录后继续（at-least-once，坏消息会重投，
                        // 由版本门吸收；不静默吞掉）。
                        tracing::warn!(target: "ponk::collect::command", "bad ctl message: {e}");
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(target: "ponk::collect::command", "ctl session error: {e}");
                    break;
                }
            }
        }
        let _ = session.close();
        Ok(())
    }

    async fn handle(
        &self,
        env: &ponk_protocol::Envelope,
        state: &SharedPlan,
        reload: &watch::Sender<Option<Arc<SubscribePlan>>>,
    ) -> Result<()> {
        let Body::Ctl(msg) = &env.body else {
            return Ok(()); // 命令主题上的非 Ctl 载荷：忽略（不静默——调用方记 warn）
        };
        let Some(plan) = plan_from_ctl(msg)? else {
            // 其他域的 `ConfigSnapshot`（如 `InstrumentBlocklist`）与纯通知：
            // 采集**只消费订阅计划域**（09 §8 接口表）。黑名单在指派时已剔除，
            // 采集侧不解释它（03 §5.3 `InstrumentMeta::enabled` 注）。
            return Ok(());
        };

        let outcome = {
            let mut st = state.lock().await;
            st.accept(plan)
        };
        match outcome {
            ApplyOutcome::Ignored { incoming, applied } => {
                Counters::inc(&self.counters.plans_ignored);
                tracing::debug!(
                    target: "ponk::collect::command",
                    incoming, applied,
                    "plan dropped by version gate (older or duplicate)"
                );
            }
            ApplyOutcome::Applied { version, changed } => {
                Counters::inc(&self.counters.plans_applied);
                self.applied.store(version, Ordering::Relaxed);
                tracing::info!(
                    target: "ponk::collect::command",
                    version, changed,
                    "subscribe plan applied"
                );
                if changed {
                    // 只有订阅集真的变了才重建连接监督任务（避免无谓断连）
                    let snapshot = state.lock().await.current().cloned();
                    let _ = reload.send(snapshot.map(Arc::new));
                }
            }
        }
        Ok(())
    }
}

/// 把一个 `CtlMessage` 的载荷类型名用于日志/断言（便于排查非计划消息）。
pub fn ctl_kind(msg: &CtlMessage) -> &'static str {
    match msg {
        CtlMessage::SubscribePlan(_) => "SubscribePlan",
        CtlMessage::ConfigChanged(_) => "ConfigChanged",
        CtlMessage::ConfigSnapshot { .. } => "ConfigSnapshot",
    }
}

/// 命令信道主题（供集成测试断言用）。
pub fn topics_for(instance: &str) -> (Topic, Topic) {
    (ctl_request_topic(instance), ctl_collector_topic(instance))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ponk_eventcenter::MemoryEventCenter;
    use ponk_protocol::{
        Channel as ProtoChannel, Envelope, InstrumentMeta, Market, NodeId, ShardPlan, Venue,
    };
    use crate::sink::RecordingSink;
    use std::collections::BTreeMap;

    fn plan(version: u64) -> SubscribePlan {
        SubscribePlan {
            assignment_version: version,
            mode: ponk_protocol::PlanMode::Snapshot,
            shards: vec![ShardPlan {
                venue: Venue::new("binance"),
                market: Market::new("spot"),
                shard_idx: 0,
                instruments: vec![InstrumentMeta {
                    native_code: "BTCUSDT".into(),
                    price_precision: 2,
                    qty_precision: 6,
                    channels: vec![ProtoChannel::new("ticker")],
                    enabled: true,
                    meta: BTreeMap::new(),
                }],
            }],
            quota_share: None,
        }
    }

    fn ctl_envelope(msg: CtlMessage) -> Envelope {
        Envelope::new(
            ponk_protocol::EventType::Ctl,
            NodeId::new("cp-1"),
            1_000,
            Body::Ctl(msg),
        )
        .unwrap()
    }

    fn parts() -> (Arc<RecordingSink>, Arc<Counters>, Arc<AtomicU64>, SharedPlan) {
        (
            Arc::new(RecordingSink::new()),
            Arc::new(Counters::new()),
            Arc::new(AtomicU64::new(0)),
            crate::plan::plan_state_for_test(0),
        )
    }

    #[tokio::test]
    async fn pull_request_goes_to_ctl_request_with_applied_version() {
        let (sink, counters, applied, _) = parts();
        applied.store(3, Ordering::Relaxed);
        let link = ControlLink::new("col-1", None, 0, applied, counters, sink.clone(), 50);
        link.request_pull().unwrap();

        assert_eq!(sink.topics(), vec!["ctl.request.col-1".to_string()]);
        match &sink.envelopes()[0].body {
            Body::Ops(hb) => {
                assert_eq!(hb.instance_id, "col-1");
                assert_eq!(hb.applied_version, Some(3), "拉取请求必须带已应用版本");
                assert_eq!(hb.node_kind, ponk_protocol::NodeKind::Collector);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pull_retry_stops_once_version_moves() {
        let (sink, counters, applied, _) = parts();
        // 真实用法：applied 初值 = 本地文件版本（= baseline）；控制面给了新版本才会前进
        applied.store(1, Ordering::Relaxed);
        let link = ControlLink::new("col-1", None, 0, applied.clone(), counters, sink.clone(), 50);
        let (_sd_tx, sd_rx) = watch::channel(false);

        // 模拟控制面在前几轮之后应答：把版本推到 2
        let setter = applied.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            setter.store(2, Ordering::Relaxed);
        });
        link.pull_until_applied(1, 10, 100, sd_rx).await;
        assert_eq!(applied.load(Ordering::Relaxed), 2);
        assert!(
            sink.count() <= 5,
            "拿到版本后立即停止拉取，不应跑满 100 次重试（实际 {} 次）",
            sink.count()
        );
    }

    #[tokio::test]
    async fn newer_plan_is_applied_and_signals_reload() {
        let (sink, counters, applied, state) = parts();
        let link = ControlLink::new("col-1", None, 0, applied.clone(), counters.clone(), sink, 50);
        let (tx, rx) = watch::channel(None);

        link.handle(&ctl_envelope(CtlMessage::SubscribePlan(plan(1))), &state, &tx)
            .await
            .unwrap();

        assert_eq!(applied.load(Ordering::Relaxed), 1, "applied_version 前进");
        assert!(rx.borrow().is_some(), "订阅集变化 → 报重建信号");
        assert_eq!(counters.snapshot()["plans_applied"], 1);
    }

    #[tokio::test]
    async fn older_plan_is_ignored_without_reload() {
        let (sink, counters, applied, state) = parts();
        let link = ControlLink::new("col-1", None, 0, applied.clone(), counters.clone(), sink, 50);
        let (tx, _rx) = watch::channel(None);

        link.handle(&ctl_envelope(CtlMessage::SubscribePlan(plan(5))), &state, &tx)
            .await
            .unwrap();
        let (tx2, rx2) = watch::channel(None);
        link.handle(&ctl_envelope(CtlMessage::SubscribePlan(plan(4))), &state, &tx2)
            .await
            .unwrap();

        assert_eq!(applied.load(Ordering::Relaxed), 5, "旧版不得回退版本");
        assert!(rx2.borrow().is_none(), "旧版不触发重建");
        assert_eq!(counters.snapshot()["plans_ignored"], 1);
    }

    #[tokio::test]
    async fn identical_shards_advance_version_without_reload() {
        let (sink, counters, applied, state) = parts();
        let link = ControlLink::new("col-1", None, 0, applied.clone(), counters, sink, 50);
        let (tx, _rx) = watch::channel(None);
        link.handle(&ctl_envelope(CtlMessage::SubscribePlan(plan(1))), &state, &tx)
            .await
            .unwrap();

        let (tx2, rx2) = watch::channel(None);
        link.handle(&ctl_envelope(CtlMessage::SubscribePlan(plan(2))), &state, &tx2)
            .await
            .unwrap();
        assert_eq!(applied.load(Ordering::Relaxed), 2);
        assert!(rx2.borrow().is_none(), "分片未变 → 不回退连接");
    }

    #[tokio::test]
    async fn non_plan_messages_are_left_to_others() {
        let (sink, counters, applied, state) = parts();
        let link = ControlLink::new("col-1", None, 0, applied.clone(), counters, sink, 50);
        let (tx, rx) = watch::channel(None);

        let msg = CtlMessage::ConfigSnapshot {
            domain: ponk_protocol::ConfigDomain::InstrumentBlocklist,
            version: 1,
            payload: ponk_protocol::ConfigPayload::new(serde_json::json!({})),
        };
        link.handle(&ctl_envelope(msg), &state, &tx).await.unwrap();

        assert_eq!(applied.load(Ordering::Relaxed), 0);
        assert!(rx.borrow().is_none());
    }

    /// 会话读取是**阻塞式**的（内存 backend 用 std 同步原语实现），
    /// 因此这个测试必须跑在多线程运行时上 —— 单线程下阻塞的 `next()`
    /// 会占死唯一执行线程，测试主体永远得不到调度。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_consumes_from_ctl_collector_topic() {
        let center = Arc::new(MemoryEventCenter::new());
        let (sink, counters, applied, state) = parts();
        let link = ControlLink::new("col-9", None, 0, applied.clone(), counters, sink, 20);
        let (tx, rx) = watch::channel(None);
        let (sd_tx, sd_rx) = watch::channel(false);

        let center_for_task = center.clone();
        let handle =
            tokio::spawn(async move { link.run(&*center_for_task, state, tx, sd_rx).await });

        // 控制面侧发一份计划
        center
            .publish(
                &ctl_collector_topic("col-9"),
                &ctl_envelope(CtlMessage::SubscribePlan(plan(2))),
            )
            .unwrap();

        // 等它被应用（最多 2s）
        for _ in 0..200 {
            if applied.load(Ordering::Relaxed) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(applied.load(Ordering::Relaxed), 2, "会话读到计划并应用");
        assert!(rx.borrow().is_some(), "订阅集变化 → 报重建信号");

        // 收尾：让会话循环退出（`next` 有超时，退出前最多再阻塞一个 timeout）
        let _ = sd_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}
