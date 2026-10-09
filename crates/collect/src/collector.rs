//! 采集进程编排（02 §4 进程模型、10 §7 启动时序）。
//!
//! 一个进程承载 **N 个 `(venue, market)` 连接**（可跨所）：每个连接一条监督任务，
//! 共享一个发布队列与一组在线计数器。
//!
//! ```text
//! ┌ supervisor(venue:market) ──┐   ┌ publisher ──► EventCenter::publish(md.*)
//! │  重连状态机 → connect/subscribe │   │   重试 → 最终失败计数
//! │  event_stream → 样本 → 信封 ─┼──►│
//! │  断线 → REST 降级 ────────────┘   └ mpsc(容量 = publish.queue_capacity，满即丢)
//! ├ heartbeat(每 5s, ops.collector.<instance>) ─────────────────► EventCenter
//! └ 在线计数器 Arc<Counters>（唯一状态，02 §4 不建行情缓存）
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use digdigdig3::core::types::{ConnectionStatus, StreamEvent};
use futures_util::StreamExt;
use ponk_protocol::SubscribePlan;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;

use crate::channel::Channel;
use crate::config::{CollectorConfig, ConnectionKey, ResolvedPlan, Subscription};
use crate::counters::Counters;
use crate::degrade::{self, DegradePlan};
use crate::envelope::{market_envelope, market_topic, PendingEvent};
use crate::error::{Error, Result};
use crate::heartbeat::{now_ms, Heartbeat};
use crate::payload::Classifier;
use crate::plan::{to_config, ApplyOutcome, PlanState, SharedPlan};
use crate::quota::QuotaRegistry;
use crate::reconnect::{Action, Event, ReconnectMachine};
use crate::sink::EventSink;
use crate::source::{MarketSource, SourceMsg};

/// 采集进程。
pub struct Collector {
    cfg: CollectorConfig,
    plan: Arc<ResolvedPlan>,
    sink: Arc<dyn EventSink>,
    counters: Arc<Counters>,
    quota: Arc<QuotaRegistry>,
    sources: HashMap<ConnectionKey, Arc<dyn MarketSource>>,
    /// 已应用版本：与心跳、控制面客户端共享（10 §4.3 隐式 ACK）。
    applied: Arc<AtomicU64>,
    /// 计划状态（版本强序门，10 §4.3）。
    plan_state: SharedPlan,
    /// 是否用 dig3 真建过源（决定重建后是否联网补源）。
    dig3_sources: bool,
    /// 订阅集重建信号：`ControlLink` 写入，[`Collector::run`] 消费。
    reload_tx: watch::Sender<Option<Arc<SubscribePlan>>>,
    reload_rx: watch::Receiver<Option<Arc<SubscribePlan>>>,
}

impl Collector {
    /// 由本地配置构建：解析计划、建令牌桶与计数器（**不联网**）。
    ///
    /// TODO(控制面)：`cfg` 目前只来自本地文件；控制面落地后此处改为
    /// 「启动 → `ctl.request.<instance>` 拉 snapshot → 收 `CtlMessage::SubscribePlan` →
    /// 整包应用」（10 §7），[`Self::apply_plan`] 即应用入口。
    pub fn new(cfg: CollectorConfig, sink: Arc<dyn EventSink>) -> Result<Self> {
        let plan = Arc::new(cfg.resolve()?);
        let quota = Arc::new(QuotaRegistry::new(&cfg.quota));
        let applied = Arc::new(AtomicU64::new(plan.assignment_version));
        let plan_state = Arc::new(Mutex::new(PlanState::new(plan.assignment_version)));
        let (reload_tx, reload_rx) = watch::channel(None);
        Ok(Self {
            cfg,
            plan,
            sink,
            counters: Arc::new(Counters::new()),
            quota,
            sources: HashMap::new(),
            applied,
            plan_state,
            dig3_sources: false,
            reload_tx,
            reload_rx,
        })
    }

    /// 已应用版本的共享句柄（心跳 / 控制面客户端共用）。
    pub fn applied_handle(&self) -> Arc<AtomicU64> {
        self.applied.clone()
    }

    /// 计划状态句柄（交给 [`crate::command::ControlLink`]）。
    pub fn plan_state(&self) -> SharedPlan {
        self.plan_state.clone()
    }

    /// 订阅集重建信号的发送端（交给 [`crate::command::ControlLink`]）。
    pub fn reload_sender(&self) -> watch::Sender<Option<Arc<SubscribePlan>>> {
        self.reload_tx.clone()
    }

    /// 承载能力上报（10 §5）：写进心跳 counters，键前缀 `capacity.`，
    /// 与控制面 `ReportedCapacity::from_counters` 约定一致。
    pub fn report_capacity(&self) {
        let cap = &self.cfg.capacity;
        self.counters.set_extra("capacity.shards", cap.max_shards as i64);
        self.counters
            .set_extra("capacity.instruments", cap.max_instruments as i64);
        self.counters
            .set_extra("capacity.channels", Channel::ALL.len() as i64);
        // 能装哪些所：`venues` 为空表示「不限制，按当前计划实际出现的所自报」
        let venues: Vec<String> = if cap.venues.is_empty() {
            let mut v: Vec<String> = self
                .plan
                .subscriptions
                .keys()
                .map(|k| k.venue.clone())
                .collect();
            v.sort();
            v.dedup();
            v
        } else {
            cap.venues.clone()
        };
        for venue in venues {
            self.counters
                .set_extra(format!("capacity.venue.{venue}"), 1);
        }
    }

    pub fn config(&self) -> &CollectorConfig {
        &self.cfg
    }

    pub fn plan(&self) -> &ResolvedPlan {
        &self.plan
    }

    pub fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }

    pub fn quota(&self) -> Arc<QuotaRegistry> {
        self.quota.clone()
    }

    pub fn instance_id(&self) -> &str {
        &self.cfg.instance_id
    }

    /// 注入行情源（离线测试：`MockSource`）。
    pub fn set_sources(&mut self, sources: HashMap<ConnectionKey, Arc<dyn MarketSource>>) {
        self.sources = sources;
    }

    /// 用 dig3 `ExchangeHub` 构建真实行情源（02 §1）。
    pub async fn build_dig3_sources(&mut self) -> Result<()> {
        let keys: Vec<ConnectionKey> = self.plan.subscriptions.keys().cloned().collect();
        let hub_sources = crate::source::build_sources(
            &keys,
            |v| self.cfg.exchange_id_for(v),
            |m| self.cfg.account_type_for(m),
        )
        .await?;
        let mut sources: HashMap<ConnectionKey, Arc<dyn MarketSource>> = HashMap::new();
        for (key, src) in hub_sources {
            sources.insert(key, src);
        }
        self.sources = sources;
        self.dig3_sources = true;
        Ok(())
    }

    /// 应用一份订阅计划（10 §4.3：**旧版丢弃、新版整包应用**）。
    ///
    /// 与 [`crate::command::ControlLink`] 的分工：`ControlLink` 负责**裁决与记账**
    /// （持有同一个 [`PlanState`] 并更新共享的 `applied_version`）；本方法把计划
    /// **落到运行时** —— 合并进配置、重新解析、换掉 `self.plan`。
    ///
    /// 由 `ControlLink` 触发的路径走 [`Self::rebuild_from_plan`]（已经裁决过，不再二次过门）。
    pub fn apply_plan(&mut self, plan: SubscribePlan) -> Result<ApplyOutcome> {
        let outcome = {
            let mut st = self.plan_state.try_lock().expect("plan state not contended here");
            st.accept(plan)
        };
        if let ApplyOutcome::Applied { version, .. } = outcome {
            // 直接调用路径同样要推进共享版本 —— 心跳读的就是它（10 §4.3 隐式 ACK）
            self.applied.store(version, Ordering::Relaxed);
            let current = self
                .plan_state
                .try_lock()
                .expect("plan state not contended here")
                .current()
                .cloned()
                .ok_or_else(|| Error::Config("applied plan missing".into()))?;
            self.rebuild_from_plan(&current)?;
        }
        Ok(outcome)
    }

    /// 把一份**已被接受**的计划落到运行时：合并配置 → 重新解析 → 换 `self.plan`。
    pub fn rebuild_from_plan(&mut self, plan: &SubscribePlan) -> Result<()> {
        let merged = to_config(&self.cfg, plan)?;
        let resolved = merged.resolve()?;
        self.cfg = merged;
        self.plan = Arc::new(resolved);
        self.report_capacity();
        Ok(())
    }

    /// 为「重建后新增的连接」补数据源；只在真用 dig3 建过源时联网
    /// （测试注入的 [`Self::set_sources`] 不触发）。缺源不致命：`run` 对该连接告警并跳过。
    async fn ensure_sources(&mut self) {
        if !self.dig3_sources {
            return;
        }
        let missing: Vec<ConnectionKey> = self
            .plan
            .subscriptions
            .keys()
            .filter(|k| !self.sources.contains_key(*k))
            .cloned()
            .collect();
        if missing.is_empty() {
            return;
        }
        match crate::source::build_sources(
            &missing,
            |v| self.cfg.exchange_id_for(v),
            |m| self.cfg.account_type_for(m),
        )
        .await
        {
            Ok(m) => {
                for (k, src) in m {
                    self.sources.insert(k, src as Arc<dyn MarketSource>);
                }
            }
            Err(e) => tracing::warn!(target: "ponk::collect", "rebuild sources failed: {e}"),
        }
    }

    /// 运行直到 `shutdown` 变为 `true`。
    /// 跑主循环：按「代」运行连接监督任务；收到重建信号则换代（换新的订阅集）。
    ///
    /// 心跳与发布器**跨代共享**（它们不随订阅集变化）：心跳每次发送都读共享的
    /// `applied_version`，所以计划应用后控制面立刻能看到收敛（10 §4.3）。
    pub async fn run(mut self, shutdown: watch::Receiver<bool>) -> Result<()> {
        let started_at = now_ms();
        let instance_id = self.cfg.instance_id.clone();
        let publish_cfg = self.cfg.publish.clone();
        let reconnect_cfg = self.cfg.reconnect.clone();
        let degrade_cfg = self.cfg.degrade.clone();
        let heartbeat_interval = Duration::from_millis(self.cfg.heartbeat.interval_ms.max(1));

        // 承载能力：register 心跳即上报（10 §5）
        self.report_capacity();

        // 发送队列（跨代共享）—— 10 §3：不做背压，满即丢并计数
        let (tx, rx) = mpsc::channel::<PendingEvent>(publish_cfg.queue_capacity.max(1));
        let publisher = tokio::spawn(publisher_loop(
            rx,
            self.sink.clone(),
            self.counters.clone(),
            publish_cfg.clone(),
        ));

        let heartbeat = tokio::spawn(
            Heartbeat::with_applied_handle(
                instance_id.clone(),
                self.applied.clone(),
                started_at,
                heartbeat_interval,
                self.counters.clone(),
                self.sink.clone(),
            )
            .with_host(self.cfg.host_name())
            .run(shutdown.clone()),
        );

        let mut reload_rx = self.reload_rx.clone();
        let mut sd = shutdown.clone();

        'generations: loop {
            // 每一代都在**新的 shutdown 子通道**上跑，便于换代时只停监督任务
            let (gen_tx, gen_rx) = watch::channel(false);
            let handles = self.spawn_generation(&gen_rx, &tx, &reconnect_cfg, &degrade_cfg, &instance_id);

            tokio::select! {
                _ = async {
                    while !*sd.borrow() {
                        if sd.changed().await.is_err() {
                            break;
                        }
                    }
                } => {
                    let _ = gen_tx.send(true);
                    for h in handles {
                        let _ = h.await;
                    }
                    break 'generations;
                }
                changed = reload_rx.changed() => {
                    let _ = gen_tx.send(true);
                    for h in handles {
                        let _ = h.await;
                    }
                    if changed.is_err() {
                        break 'generations; // 发送端消失（控制面客户端已退出）
                    }
                    let next = reload_rx.borrow().clone();
                    if let Some(plan) = next {
                        match self.rebuild_from_plan(&plan) {
                            Ok(()) => {
                                tracing::info!(
                                    target: "ponk::collect",
                                    version = plan.assignment_version,
                                    connections = self.plan.subscriptions.len(),
                                    "subscribe set rebuilt"
                                );
                                self.ensure_sources().await;
                            }
                            Err(e) => tracing::error!(
                                target: "ponk::collect",
                                "rebuild from plan failed: {e}"
                            ),
                        }
                    }
                }
            }
        }

        // 所有监督任务已结束、tx 的所有克隆随之下线；再显式 drop 主 tx，
        // 发布器才能看到队列关闭并退出（漏了这步 `publisher.await` 会永久挂住）。
        drop(tx);
        let _ = heartbeat.await;
        let _ = publisher.await;
        Ok(())
    }

    /// 起一代监督任务（订阅集变化时换代）。
    fn spawn_generation(
        &self,
        gen_shutdown: &watch::Receiver<bool>,
        tx: &mpsc::Sender<PendingEvent>,
        reconnect_cfg: &crate::config::ReconnectConfig,
        degrade_cfg: &crate::config::DegradeConfig,
        instance_id: &str,
    ) -> Vec<JoinHandle<()>> {
        let plan = self.plan.clone();
        let core_set = plan.core_set.clone();
        let mut handles = Vec::new();
        for (key, subs) in plan.subscriptions.clone() {
            let src = match self.sources.get(&key) {
                Some(s) => s.clone(),
                None => {
                    tracing::warn!(
                        target: "ponk::collect",
                        connection = %key,
                        "no data source for this connection; skipping"
                    );
                    continue;
                }
            };
            let classifier = Arc::new(Classifier::for_connection(&key, &plan));
            let is_core = {
                let core = core_set.clone();
                move |code: &str| {
                    let up = code.to_ascii_uppercase();
                    core.iter().any(|c| c == &up)
                }
            };
            handles.push(tokio::spawn(supervisor(SupervisorArgs {
                key,
                subs,
                src,
                classifier,
                plan: plan.clone(),
                venue: String::new(),
                instance_id: instance_id.to_string(),
                reconnect: reconnect_cfg.clone(),
                degrade: degrade_cfg.clone(),
                is_core: Arc::new(is_core),
                quota: self.quota.clone(),
                counters: self.counters.clone(),
                queue: tx.clone(),
                shutdown: gen_shutdown.clone(),
            })));
        }
        handles
    }
}

/// `apply_plan` 的最小入参（TODO(控制面)：控制面落地后换成 `ponk_protocol::SubscribePlan`）。

struct SupervisorArgs {
    key: ConnectionKey,
    subs: Vec<Subscription>,
    src: Arc<dyn MarketSource>,
    classifier: Arc<Classifier>,
    plan: Arc<ResolvedPlan>,
    venue: String,
    instance_id: String,
    reconnect: crate::config::ReconnectConfig,
    degrade: crate::config::DegradeConfig,
    is_core: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    quota: Arc<QuotaRegistry>,
    counters: Arc<Counters>,
    queue: mpsc::Sender<PendingEvent>,
    shutdown: watch::Receiver<bool>,
}

/// 单连接监督：驱动重连状态机、消费事件流、按需启停 REST 降级。
///
/// 结构：
///
/// 1. **驱动段**：按状态机动作执行 `Connect` / `Resubscribe` / 退避等待；
/// 2. **收流段**：`event_stream` + 静默/状态轮询，任一连接级信号即回到第 1 段。
///
/// 断线（进入退避）时懒启动 REST 降级任务，回到 `Running` 时置 `active = true`
/// 让它空转，监督器退出时置 `shutdown = true` 并**等待其结束**
/// （否则它继续持有发送端，发布队列排不干净）。
///
/// `tokio::select!` 的分支体内不读写 `a.shutdown`——该变量在同一 `select!`
/// 的另一分支被 `changed()` 可变借用；状态一律在 `select!` 之后读取。
async fn supervisor(mut a: SupervisorArgs) {
    let mut machine = ReconnectMachine::new(
        a.reconnect.backoff_min_ms,
        a.reconnect.backoff_max_ms,
        a.reconnect.max_attempts,
    );
    let mut action = machine.on(Event::Start);

    // 降级任务（懒启动：只在「曾经订阅成功过」之后、断线时才起）。
    let (active_tx, active_rx) = watch::channel(true);
    let (deg_shutdown_tx, deg_shutdown_rx) = watch::channel(false);
    let mut degrade_task: Option<JoinHandle<()>> = None;

    'outer: loop {
        // ── 驱动段：到 Running 或 Stop ────────────────────────────────────
        loop {
            if *a.shutdown.borrow() {
                action = machine.on(Event::Shutdown);
            }
            match action {
                Action::Stop => break 'outer,
                Action::Idle => break,
                Action::Wait(ms) => {
                    // 02 §6：进入断线态 → 起 REST 降级（若允许）。
                    if degrade_task.is_none() && machine.ever_subscribed() {
                        let plan = DegradePlan::build(&a.key, &a.plan, &*a.is_core, now_ms());
                        if !plan.is_noop() {
                            let _ = active_tx.send(false);
                            degrade_task = Some(tokio::spawn(degrade::run(
                                plan,
                                a.instance_id.clone(),
                                a.src.clone(),
                                a.classifier.clone(),
                                a.quota.clone(),
                                a.counters.clone(),
                                a.queue.clone(),
                                a.degrade.clone(),
                                active_rx.clone(),
                                deg_shutdown_rx.clone(),
                            )));
                        }
                    }

                    let mut shutdown_seen = false;
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                        _ = a.shutdown.changed() => { shutdown_seen = true; }
                    }
                    if shutdown_seen || *a.shutdown.borrow() {
                        action = machine.on(Event::Shutdown);
                    } else {
                        // 一次退避走完 = 一次重连（02 §6 步骤 2）。
                        a.counters.reconnects.fetch_add(1, Ordering::Relaxed);
                        action = machine.on(Event::BackoffElapsed);
                    }
                }
                Action::Connect => {
                    match a.src.connect().await {
                        Ok(()) => action = machine.on(Event::ConnectOk),
                        Err(e) => {
                            a.counters.connect_errors.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                target: "ponk::collect",
                                connection = %a.key, error = %e, "connect failed"
                            );
                            action = machine.on(Event::ConnectErr);
                        }
                    }
                }
                Action::Resubscribe => {
                    // 出站订阅帧也吃配额（02 §6：所间不串）。
                    let _ = a.quota.acquire(&a.venue, 1.0, Some(Duration::from_secs(5))).await;
                    match a.src.subscribe(&a.subs).await {
                        Ok(ack) => {
                            a.counters.subscribe_replays.fetch_add(1, Ordering::Relaxed);
                            if !ack.rejected.is_empty() {
                                a.counters.subscribe_rejected.fetch_add(
                                    ack.rejected.len() as i64,
                                    Ordering::Relaxed,
                                );
                                tracing::warn!(
                                    target: "ponk::collect",
                                    connection = %a.key,
                                    rejected = ?ack.rejected,
                                    "subscribe partially rejected"
                                );
                            }
                            action = machine.on(Event::SubscribeOk);
                        }
                        Err(e) => {
                            a.counters.subscribe_errors.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                target: "ponk::collect",
                                connection = %a.key, error = %e, "subscribe failed"
                            );
                            action = machine.on(Event::SubscribeErr);
                        }
                    }
                }
            }

            if action == Action::Idle {
                // 回到 Running：降级空转。
                let _ = active_tx.send(true);
            }
        }

        // ── 收流段：Running ───────────────────────────────────────────────
        let mut stream = a.src.event_stream();
        let mut tick = tokio::time::interval(Duration::from_millis(a.reconnect.poll_ms.max(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_activity = std::time::Instant::now();
        let mut disconnected_since: Option<std::time::Instant> = None;
        let silence = Duration::from_millis(a.reconnect.silence_timeout_ms);
        let grace = Duration::from_millis(a.reconnect.disconnected_grace_ms);

        loop {
            let mut reconnect_action: Option<Event> = None;
            let mut shutdown_seen = false;

            tokio::select! {
                item = stream.next() => {
                    match item {
                        Some(SourceMsg::Event(ev)) => {
                            last_activity = std::time::Instant::now();
                            a.counters.ws_events.fetch_add(1, Ordering::Relaxed);
                            match &ev {
                                StreamEvent::SubscriptionFailed { request, error } => {
                                    // 交易所异步拒绝订阅（含重连重放被拒）——
                                    // 按 02 §6 走「重连 + 重放订阅集」恢复。
                                    a.counters.subscribe_rejected.fetch_add(1, Ordering::Relaxed);
                                    tracing::warn!(
                                        target: "ponk::collect",
                                        connection = %a.key,
                                        request = ?request,
                                        error = %error,
                                        "subscription rejected by exchange"
                                    );
                                    reconnect_action = Some(Event::SubscribeErr);
                                }
                                other => {
                                    for sample in a.classifier.classify(other) {
                                        emit_sample(&sample, &a.instance_id, &a.counters, &a.queue);
                                    }
                                }
                            }
                        }
                        Some(SourceMsg::Error(e)) => {
                            a.counters.stream_errors.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                target: "ponk::collect",
                                connection = %a.key, error = %e, "ws stream error"
                            );
                            reconnect_action = Some(Event::StreamError);
                        }
                        None => {
                            a.counters.stream_closed.fetch_add(1, Ordering::Relaxed);
                            reconnect_action = Some(Event::StreamClosed);
                        }
                    }
                }
                _ = tick.tick() => {
                    // 连接级静默超时（02 §6 检测项之一）。
                    // **不报静默**：静默告警由 06 观测 md.* 流得出（02 §7.3、10 §8）。
                    if !silence.is_zero() && last_activity.elapsed() >= silence {
                        // 只记「连接级静默触发的重连」这一独立指标；
                        // `reconnects` 统一在退避走完时计一次，避免重复计数。
                        a.counters.silence_reconnects.fetch_add(1, Ordering::Relaxed);
                        reconnect_action = Some(Event::Silence);
                    } else {
                        match a.src.connection_status() {
                            ConnectionStatus::Disconnected => {
                                let since = *disconnected_since
                                    .get_or_insert_with(std::time::Instant::now);
                                if since.elapsed() >= grace {
                                    reconnect_action = Some(Event::DisconnectedTooLong);
                                }
                            }
                            _ => disconnected_since = None,
                        }
                    }
                }
                _ = a.shutdown.changed() => { shutdown_seen = true; }
            }

            if shutdown_seen {
                action = machine.on(Event::Shutdown);
                break;
            }
            if let Some(ev) = reconnect_action {
                action = machine.on(ev);
                break;
            }
        }

        if matches!(action, Action::Stop) {
            break 'outer;
        }
    }

    // 退出：停降级并等它结束（否则它继续持有发送端，发布管道排不干净）。
    let _ = deg_shutdown_tx.send(true);
    if let Some(h) = degrade_task {
        let _ = h.await;
    }
}

fn emit_sample(
    sample: &crate::payload::MarketSample,
    instance_id: &str,
    counters: &Counters,
    queue: &mpsc::Sender<PendingEvent>,
) {
    let received_ts = now_ms();
    let env = match market_envelope(sample, received_ts, instance_id) {
        Ok(e) => e,
        Err(e) => {
            counters.dropped_samples.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(target: "ponk::collect", error = %e, "envelope build failed");
            return;
        }
    };
    let pending = PendingEvent {
        topic: market_topic(sample),
        envelope: env,
        channel: sample.channel,
    };
    // 10 §3：不做背压 —— 队列满即丢并计数，积压由 06 观测。
    if queue.try_send(pending).is_err() {
        counters.dropped_backlog.fetch_add(1, Ordering::Relaxed);
    }
}

async fn publisher_loop(
    mut rx: mpsc::Receiver<PendingEvent>,
    sink: Arc<dyn EventSink>,
    counters: Arc<Counters>,
    cfg: crate::config::PublishConfig,
) {
    while let Some(pending) = rx.recv().await {
        counters
            .send_backlog
            .store(rx.len() as i64, Ordering::Relaxed);

        let mut attempt = 0u32;
        loop {
            match sink.publish(&pending.topic, &pending.envelope) {
                Ok(()) => {
                    counters.published.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                Err(e) => {
                    attempt += 1;
                    if attempt > cfg.retries {
                        counters.publish_failed.fetch_add(1, Ordering::Relaxed);
                        // 10 §3「最终失败进 dlq.md.*」/ 03 §7.3「携带 DeadLetterMeta」
                        // **当前做不到**：EventCenter::publish 只收 Envelope，而
                        // DeadLetterMeta 不是 Envelope（03 §5.2）。见 sink.rs 模块文档与
                        // 最终报告「缺失 API」。此处只计数，不伪造负载。
                        tracing::error!(
                            target: "ponk::collect",
                            topic = %pending.topic,
                            channel = %pending.channel,
                            attempts = attempt,
                            error = %e,
                            "publish failed after retries (dlq write unavailable)"
                        );
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(cfg.retry_ms.max(1))).await;
                }
            }
        }
    }
    counters.send_backlog.store(0, Ordering::Relaxed);
}

// ─────────────────────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Channel;
    use crate::config::CollectorConfig;
    use crate::sink::RecordingSink;
    use digdigdig3::core::types::{Kline, KlineInterval, Ticker};

    const TOML: &str = r#"
instance_id = "collector-01"
assignment_version = 3
core_set = ["BTCUSDT"]

[reconnect]
silence_timeout_ms = 1000
poll_ms = 50
backoff_min_ms = 10
backoff_max_ms = 40

[degrade]
enabled = true
poll_interval_ms = 20

[heartbeat]
interval_ms = 200

[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["ticker", "trade"]
[[shards.instruments]]
native_code = "BTCUSDT"

[[shards]]
venue = "okx"
market = "spot"
shard_idx = 0
channels = ["trade"]
[[shards.instruments]]
native_code = "BTC-USDT"
"#;

    fn collector(sink: Arc<RecordingSink>) -> Collector {
        Collector::new(CollectorConfig::from_toml(TOML).unwrap(), sink).unwrap()
    }

    /// 只看 `md.*` 行情事件（同一个 sink 里还有 `ops.*` 心跳）。
    fn market_pairs(sink: &RecordingSink) -> Vec<(String, ponk_protocol::Envelope)> {
        sink.pairs()
            .into_iter()
            .filter(|(_, e)| e.event_type == ponk_protocol::EventType::Market)
            .collect()
    }

    fn market_envs(sink: &RecordingSink) -> Vec<ponk_protocol::Envelope> {
        market_pairs(sink).into_iter().map(|(_, e)| e).collect()
    }

    fn tickers(symbol: &str, price: f64, ts: i64) -> StreamEvent {
        StreamEvent::Ticker {
            symbol: symbol.into(),
            ticker: Ticker {
                last_price: price,
                timestamp: ts,
                ..Default::default()
            },
        }
    }

    #[test]
    fn new_builds_plan_without_networking() {
        let c = collector(Arc::new(RecordingSink::new()));
        assert_eq!(c.instance_id(), "collector-01");
        assert_eq!(c.plan().assignment_version, 3);
        assert_eq!(c.plan().subscriptions.len(), 2, "跨所两个连接（02 §4）");
        assert!(c
            .plan()
            .subscriptions
            .contains_key(&ConnectionKey::new("binance", "spot")));
        assert!(c.plan().subscriptions.contains_key(&ConnectionKey::new("okx", "spot")));
    }

    #[tokio::test(start_paused = true)]
    async fn publishes_market_events_to_md_topic_with_correct_envelope() {
        let sink = Arc::new(RecordingSink::new());
        let mut c = collector(sink.clone());
        let binance = crate::source::MockSource::new("binance", "spot");
        let okx = crate::source::MockSource::new("okx", "spot");
        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
        srcs.insert(ConnectionKey::new("okx", "spot"), okx.handle());
        c.set_sources(srcs);

        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));

        // 等到订阅完成
        wait_until(2_000, || binance.subscribe_count() >= 1).await;
        assert!(okx.subscribe_count() >= 1);

        binance.push(tickers("btcusdt", 64_231.5, 1_700_000_000_000));
        binance.push(StreamEvent::Trade {
            symbol: "btcusdt".into(),
            trade: digdigdig3::core::types::PublicTrade {
                id: "1".into(),
                price: 64_231.0,
                quantity: 0.5,
                side: digdigdig3::core::types::TradeSide::Buy,
                timestamp: 1_700_000_001_000,
                ..Default::default()
            },
        });
        // 白名单外：应被丢弃
        binance.push(tickers("SOLUSDT", 1.0, 1));

        wait_until(2_000, || market_envs(&sink).len() >= 2).await;
        tx.send(true).unwrap();
        h.await.unwrap().unwrap();

        let topics: Vec<String> = market_pairs(&sink).into_iter().map(|(t, _)| t).collect();
        assert!(
            topics.contains(&"md.ticker".to_string()) && topics.contains(&"md.trade".to_string()),
            "应同时产出 md.ticker 与 md.trade，got {:?}",
            topics
        );

        for env in market_envs(&sink) {
            assert_eq!(env.event_type, ponk_protocol::EventType::Market);
            assert_eq!(env.shard.as_deref(), Some("binance:spot:BTCUSDT"));
            assert_eq!(env.x_version, None, "非 candle_1m 不带 x_version");
            assert!(env.event_id.starts_with("binance:spot:BTCUSDT:md:"), "{}", env.event_id);
        }
        // 没有 SOL 的事件
        assert!(!market_envs(&sink)
            .iter()
            .any(|e| e.shard.as_deref() == Some("binance:spot:SOLUSDT")));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_replays_the_subscription_set() {
        let sink = Arc::new(RecordingSink::new());
        let mut c = collector(sink.clone());
        let binance = crate::source::MockSource::new("binance", "spot");
        let okx = crate::source::MockSource::new("okx", "spot");
        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
        srcs.insert(ConnectionKey::new("okx", "spot"), okx.handle());
        c.set_sources(srcs);

        let counters = c.counters();
        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));

        wait_until(2_000, || binance.subscribe_count() >= 1).await;
        let before = binance.subscribe_count();
        assert!(before >= 1);

        // 注入连接级错误 → 检测 → 重连 + 重放订阅集（02 §6）
        binance.push_error("connection reset by peer");
        wait_until(2_000, || binance.subscribe_count() > before).await;

        let calls = binance.subscribe_calls();
        assert!(calls.len() >= 2, "应至少重放一次订阅集，got {:?}", calls);
        // 每次重放都是**完整的**订阅集
        let first: Vec<String> = calls[0].clone();
        let last: Vec<String> = calls[calls.len() - 1].clone();
        assert_eq!(first, last, "重放的订阅集必须与首次一致");
        assert!(counters.reconnects() >= 1);

        tx.send(true).unwrap();
        h.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_flows_to_ops_topic_with_counters() {
        let sink = Arc::new(RecordingSink::new());
        let mut c = collector(sink.clone());
        let binance = crate::source::MockSource::new("binance", "spot");
        let okx = crate::source::MockSource::new("okx", "spot");
        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
        srcs.insert(ConnectionKey::new("okx", "spot"), okx.handle());
        c.set_sources(srcs);

        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));
        wait_until(2_000, || {
            sink.topics().iter().any(|t| t.starts_with("ops.collector."))
        })
        .await;
        tx.send(true).unwrap();
        h.await.unwrap().unwrap();

        let ops: Vec<_> = sink
            .envelopes()
            .into_iter()
            .filter(|e| e.event_type == ponk_protocol::EventType::Ops)
            .collect();
        assert!(!ops.is_empty(), "至少应有 Startup 心跳");
        match &ops[0].body {
            ponk_protocol::Body::Ops(hb) => {
                assert_eq!(hb.node_kind, ponk_protocol::NodeKind::Collector);
                assert_eq!(hb.status, ponk_protocol::Status::Startup);
                assert_eq!(hb.applied_version, Some(3));
                assert!(hb.counters.contains_key("send_backlog"));
                assert!(hb.counters.contains_key("degraded_rest"));
            }
            other => panic!("{:?}", other),
        }
        // 最后一条必须是 Shutdown（10 §5：缺失即失联判定）
        match &ops.last().unwrap().body {
            ponk_protocol::Body::Ops(hb) => assert_eq!(hb.status, ponk_protocol::Status::Shutdown),
            other => panic!("{:?}", other),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_publish_retries_then_counts() {
        let sink = Arc::new(RecordingSink::new());
        sink.fail_next(2); // 重试后成功
        let mut c = collector(sink.clone());
        let binance = crate::source::MockSource::new("binance", "spot");
        let okx = crate::source::MockSource::new("okx", "spot");
        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
        srcs.insert(ConnectionKey::new("okx", "spot"), okx.handle());
        c.set_sources(srcs);
        let counters = c.counters();

        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));
        wait_until(2_000, || binance.subscribe_count() >= 1).await;
        binance.push(tickers("btcusdt", 1.0, 1_700_000_000_000));
        wait_until(2_000, || counters.published() >= 1).await;

        assert_eq!(counters.published(), 1);
        assert_eq!(counters.snapshot()["publish_failed"], 0);
        tx.send(true).unwrap();
        h.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn degrade_denies_trade_channel_and_counts_backfill_blocks() {
        // trade 不降级（02 §6），candle 只核心集且不回补。
        let toml = r#"
instance_id = "collector-01"
core_set = ["BTCUSDT"]
[reconnect]
silence_timeout_ms = 500
poll_ms = 20
backoff_min_ms = 10
backoff_max_ms = 20
[degrade]
enabled = true
poll_interval_ms = 20
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["trade"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;
        let sink = Arc::new(RecordingSink::new());
        let mut c = Collector::new(CollectorConfig::from_toml(toml).unwrap(), sink.clone()).unwrap();
        let binance = crate::source::MockSource::new("binance", "spot");
        let okx = crate::source::MockSource::new("okx", "spot");
        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
        srcs.insert(ConnectionKey::new("okx", "spot"), okx.handle());
        c.set_sources(srcs);
        let counters = c.counters();

        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));
        wait_until(2_000, || binance.subscribe_count() >= 1).await;

        // 断线
        binance.push_error("ws died");
        wait_until(2_000, || counters.snapshot()["degrade_denied"] > 0).await;
        assert!(counters.snapshot()["degrade_denied"] > 0, "trade 必须拒绝降级");
        // 没有 ticker/candle 可降级 → 不会产生 REST 数据
        assert_eq!(counters.degraded_rest(), 0);

        tx.send(true).unwrap();
        h.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn ticker_degrade_publishes_latest_ticker_without_backfill() {
        let toml = r#"
instance_id = "collector-01"
core_set = ["BTCUSDT"]
[reconnect]
silence_timeout_ms = 500
poll_ms = 20
backoff_min_ms = 10
backoff_max_ms = 20
[degrade]
enabled = true
poll_interval_ms = 20
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["ticker"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;
        let sink = Arc::new(RecordingSink::new());
        let mut c = Collector::new(CollectorConfig::from_toml(toml).unwrap(), sink.clone()).unwrap();
        let binance = crate::source::MockSource::new("binance", "spot");
        binance.set_rest_ticker(
            "BTCUSDT",
            Ticker {
                last_price: 1.0,
                timestamp: 1_700_000_000_000,
                ..Default::default()
            },
        );
        let okx = crate::source::MockSource::new("okx", "spot");
        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
        srcs.insert(ConnectionKey::new("okx", "spot"), okx.handle());
        c.set_sources(srcs);
        let counters = c.counters();

        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));
        wait_until(2_000, || binance.subscribe_count() >= 1).await;

        binance.push_error("ws died");
        wait_until(3_000, || counters.degraded_rest() > 0).await;
        assert!(binance.rest_ticker_call_count() > 0, "ticker 应全市场 REST 降级");
        assert!(sink
            .topics()
            .iter()
            .any(|t| t == "md.ticker"), "降级数据也必须走 md.*");

        tx.send(true).unwrap();
        h.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn candle_degrade_blocks_bars_from_before_the_disconnect() {
        // 02 §6 步骤 3：断档不补。断线时刻的 bar 与更早的 bar 都必须被挡下。
        let toml = r#"
instance_id = "collector-01"
core_set = ["BTCUSDT"]
[reconnect]
silence_timeout_ms = 500
poll_ms = 20
backoff_min_ms = 10
backoff_max_ms = 20
[degrade]
enabled = true
poll_interval_ms = 20
[[shards]]
venue = "binance"
market = "swap"
shard_idx = 0
channels = ["candle_1m"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;
        let sink = Arc::new(RecordingSink::new());
        let mut c = Collector::new(CollectorConfig::from_toml(toml).unwrap(), sink.clone()).unwrap();
        let binance = crate::source::MockSource::new("binance", "swap");
        // 断线发生在「现在这一分钟」里：REST 同时给出
        //   ① 断线所在分钟开始的 bar（断线前已实时收到一部分 → 必须挡下，属回补）
        //   ② 下一分钟开始的 bar（断线之后才开始 → 允许发布）
        let now = crate::heartbeat::now_ms();
        let old_bar = Kline {
            open_time: now as i64,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
            ..Default::default()
        };
        let new_bar = Kline {
            open_time: (crate::degrade::ceil_to_minute(now) + 60_000) as i64,
            open: 2.0,
            high: 2.0,
            low: 2.0,
            close: 2.0,
            volume: 2.0,
            ..Default::default()
        };
        binance.set_rest_klines("BTCUSDT", vec![old_bar.clone(), new_bar.clone()]);

        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "swap"), binance.handle());
        c.set_sources(srcs);
        let counters = c.counters();

        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));
        wait_until(2_000, || binance.subscribe_count() >= 1).await;
        binance.push_error("ws died");
        wait_until(3_000, || counters.degraded_rest() > 0).await;

        assert!(
            counters.snapshot()["degrade_backfill_blocked"] > 0,
            "断线前开始的 bar 必须被挡下（02 §6 不回补）"
        );
        // 只发布「断线后开始的 bar」；断线前/断线所在分钟的 bar 一条都不能出现。
        // （降级轮询会重复发布同一根 bar —— event_id 内容敏感、幂等，由存储层 upsert 消化。）
        let published_open_times: Vec<i64> = market_envs(&sink)
            .iter()
            .map(|e| match &e.body {
                ponk_protocol::Body::Market(m) => match &m.data {
                    ponk_protocol::MarketData::Kline(k) => k.open_time,
                    other => panic!("{:?}", other),
                },
                other => panic!("{:?}", other),
            })
            .collect();
        assert!(!published_open_times.is_empty(), "降级应至少发布一根 bar");
        assert!(
            !published_open_times.contains(&old_bar.open_time),
            "断线所在分钟的 bar 绝不能发布（= 回补），got {:?}",
            published_open_times
        );
        assert!(
            published_open_times.iter().all(|t| *t == new_bar.open_time),
            "只能发布断线后开始的 bar，got {:?}",
            published_open_times
        );

        tx.send(true).unwrap();
        h.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn candle_1m_events_carry_x_version() {
        let toml = r#"
instance_id = "collector-01"
[reconnect]
silence_timeout_ms = 500
poll_ms = 20
backoff_min_ms = 10
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["candle_1m"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#;
        let sink = Arc::new(RecordingSink::new());
        let mut c = Collector::new(CollectorConfig::from_toml(toml).unwrap(), sink.clone()).unwrap();
        let binance = crate::source::MockSource::new("binance", "spot");
        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
        c.set_sources(srcs);

        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));
        wait_until(2_000, || binance.subscribe_count() >= 1).await;

        binance.push(StreamEvent::Kline {
            symbol: "BTCUSDT".into(),
            interval: KlineInterval::new("1m"),
            kline: Kline {
                open_time: 1_700_000_000_000,
                open: 1.0,
                high: 2.0,
                low: 1.0,
                close: 1.5,
                volume: 10.0,
                ..Default::default()
            },
        });
        wait_until(2_000, || !market_envs(&sink).is_empty()).await;
        let env = &market_envs(&sink)[0];
        assert_eq!(env.x_version, Some(env.received_ts), "03 §5.4：x_version = received_ts");
        assert_eq!(env.shard.as_deref(), Some("binance:spot:BTCUSDT"));
        assert!(env
            .event_id
            .starts_with("binance:spot:BTCUSDT:md:1700000000000:"), "{}", env.event_id);

        tx.send(true).unwrap();
        h.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_emits_shutdown_heartbeat_and_stops_cleanly() {
        let sink = Arc::new(RecordingSink::new());
        let mut c = collector(sink.clone());
        let binance = crate::source::MockSource::new("binance", "spot");
        let okx = crate::source::MockSource::new("okx", "spot");
        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
        srcs.insert(ConnectionKey::new("okx", "spot"), okx.handle());
        c.set_sources(srcs);

        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));
        wait_until(2_000, || binance.subscribe_count() >= 1).await;
        tx.send(true).unwrap();
        // 必须自行结束（不 hang）
        let res = tokio::time::timeout(Duration::from_secs(5), h).await;
        assert!(res.is_ok(), "采集器应在 shutdown 后退出");
        res.unwrap().unwrap().unwrap();
        let statuses: Vec<_> = sink
            .envelopes()
            .into_iter()
            .filter_map(|e| match e.body {
                ponk_protocol::Body::Ops(hb) => Some(hb.status),
                _ => None,
            })
            .collect();
        assert_eq!(statuses.last(), Some(&ponk_protocol::Status::Shutdown));
    }

    #[tokio::test(start_paused = true)]
    async fn every_connection_gets_a_supervisor() {
        // 02 §4：一个进程承载多个 (venue, market) 分片，可跨所。
        let sink = Arc::new(RecordingSink::new());
        let mut c = collector(sink.clone());
        let binance = crate::source::MockSource::new("binance", "spot");
        let okx = crate::source::MockSource::new("okx", "spot");
        let mut srcs = HashMap::new();
        srcs.insert(ConnectionKey::new("binance", "spot"), binance.handle());
        srcs.insert(ConnectionKey::new("okx", "spot"), okx.handle());
        c.set_sources(srcs);

        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(c.run(rx));
        wait_until(3_000, || binance.subscribe_count() >= 1 && okx.subscribe_count() >= 1).await;
        assert_eq!(binance.connect_count(), 1);
        assert_eq!(okx.connect_count(), 1);
        tx.send(true).unwrap();
        h.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn apply_plan_is_monotonic_and_rebuilds_subscriptions() {
        // 10 §4.3：新版整包应用（并重建订阅集）；旧版丢弃且状态不动。
        let mut c = collector(Arc::new(RecordingSink::new()));
        let before = c.plan().assignment_version;

        let newer = crate::plan::plan_for_test(before + 1, "BTCUSDT", &["ticker"]);
        match c.apply_plan(newer).unwrap() {
            ApplyOutcome::Applied { version, .. } => assert_eq!(version, before + 1),
            other => panic!("{other:?}"),
        }
        assert_eq!(c.applied_handle().load(Ordering::Relaxed), before + 1);
        assert!(c.plan().subscriptions.keys().next().is_some(), "订阅集已重建");

        let older = crate::plan::plan_for_test(before, "ETHUSDT", &["trade"]);
        match c.apply_plan(older).unwrap() {
            ApplyOutcome::Ignored { .. } => {}
            other => panic!("旧版应被丢弃，得到 {other:?}"),
        }
        assert_eq!(c.applied_handle().load(Ordering::Relaxed), before + 1, "旧版不得回退版本");
    }

    #[test]
    fn channel_is_used_for_observability() {
        // 队列元素携带 channel，供日志与测试定位（避免只靠 topic 反推）。
        assert_eq!(Channel::Ticker.topic(), "md.ticker");
    }

    async fn wait_until(timeout_ms: u64, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if cond() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("condition not met within {}ms", timeout_ms);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}
