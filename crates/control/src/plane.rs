//! 控制面装配（`docs/09-控制面.md`）：配置写入面 + 状态观测面，
//! 一切交互经事件中心（01 §6 红线 4：星型拓扑，不设第二条传输）。
//!
//! ```text
//! 操作者/自动化 ──publish_domain──► 控制面 ──ConfigChanged──► ctl.<domain>       (②)
//!                                        ▲                        │
//!                                        │ ③ ConfigSnapshot / SubscribePlan
//!                                        │                        ▼
//!                                        └──── ctl.request.<inst> ┴─ ctl.<kind>.<inst>
//!                                        ▲
//!                                        └── ops.<kind>.<inst>   (register/heartbeat/shutdown)
//! ```

use crate::assign::{
    assign, next_assignment_version, AssignRequest, AssignmentResult, Candidate, InstanceAssignment,
    VersionGate,
};
use crate::catalog::{
    CatalogSync, MetaError, MetadataFetcher, SyncOutcome, UnwiredFetcher,
};
use crate::error::ControlError;
use crate::offchain::{OffchainConfig, OffchainOutcome, OffchainProvider, OffchainSync, UnwiredProvider};
use crate::schema::{BlocklistDoc, CapacityLimits, ProcessCapacityDoc, Strategy, SubscribePlanDoc};
use crate::state::{Convergence, InstStatus, LivenessConfig, OpsRegistry, SnapshotKey};
use crate::store::{AnyConfigStore, ConfigStore, StoredDoc};
use crate::topics;
use crate::validate::{self, consumed_domains};
use ponk_eventcenter::{EventCenter, Session, SessionOpts};
use ponk_protocol::{
    Body, ConfigChanged, ConfigDomain, ConfigPayload, CtlMessage, Envelope, EventType, NodeKind,
    OpsHeartbeat, PlanMode, Status, SubscribePlan, Topic,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─────────────────────────────────────────────────────────────────────────────
// 配置
// ─────────────────────────────────────────────────────────────────────────────

/// 控制面自身运行参数（09 §4.4：不构成对外配置域）。
#[derive(Debug, Clone)]
pub struct ControlConfig {
    /// 控制面实例 id（`ops.control` 的 `instance_id`）。
    pub instance_id: String,
    /// 发布者标识（09 §3.1：`控制面实例/操作者`，如 `cp-1/alice`、自动化为 `cp-1/api`）。
    pub publisher: String,
    /// 控制面消费组（09 §6.1：以消费组读 `ops.*`）。
    pub group: String,
    /// 心跳 / 失联 / 收敛超时参数。
    pub liveness: LivenessConfig,
    /// 存储根目录。
    pub store_root: PathBuf,
}

impl Default for ControlConfig {
    fn default() -> Self {
        ControlConfig {
            instance_id: "cp-1".to_string(),
            publisher: "cp-1/api".to_string(),
            group: "control-plane".to_string(),
            liveness: LivenessConfig::default(),
            store_root: PathBuf::from("./control-store"),
        }
    }
}

/// 一条待发布消息。
#[derive(Debug, Clone)]
pub struct Outbound {
    /// 目标主题。
    pub topic: Topic,
    /// 信封。
    pub envelope: Envelope,
}

impl Outbound {
    /// 构造。
    pub fn new(topic: Topic, envelope: Envelope) -> Self {
        Outbound { topic, envelope }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 会话
// ─────────────────────────────────────────────────────────────────────────────

struct InstanceSessions {
    /// `ops.<kind>.<instance>`（10 §5 控制信道）。
    ops_topic: Topic,
    /// `ctl.request.<instance>`（10 §4.1 拉取）。
    request_topic: Topic,
    ops: Option<Box<dyn Session>>,
    request: Option<Box<dyn Session>>,
}

// ─────────────────────────────────────────────────────────────────────────────
// 控制面
// ─────────────────────────────────────────────────────────────────────────────

/// 控制面：配置事实来源 + 指派器 + 状态观测面（09 §1）。
pub struct ControlPlane<E: EventCenter> {
    ec: E,
    store: Arc<AnyConfigStore>,
    registry: OpsRegistry,
    cfg: ControlConfig,
    sessions: BTreeMap<String, InstanceSessions>,
    /// 每实例最新一次下发的计划（派生物，09 §3.1）。
    plans: BTreeMap<String, InstanceAssignment>,
    /// 每实例 `assignment_version` 门（10 §4.3 强序的控制面侧镜像）。
    gates: BTreeMap<String, VersionGate>,
    /// 09 §2.3 定时任务：交易对与币信息同步（catalog 是**内部状态，不是配置域**）。
    catalog: CatalogSync,
    /// 元数据拉取器（默认未接线；测试注入假实现）。
    fetcher: Arc<dyn MetadataFetcher>,
    /// `docs/14 §3` 外部（币）同步任务：**独立节拍**，与 5 分钟的交易所同步分离。
    offchain: OffchainSync,
    /// 外部币信息拉取器（默认未接线；测试注入假实现）。
    offchain_provider: Arc<dyn OffchainProvider>,
    /// `[providers]` 段（`docs/14 §8`）。
    offchain_cfg: OffchainConfig,
    started_at: u64,
    status: Status,
}

impl<E: EventCenter> ControlPlane<E> {
    /// 打开控制面：加载存储 → 恢复兜底快照键（09 §6.3）→ 校验八域是否齐备。
    pub fn open(ec: E, cfg: ControlConfig) -> Result<Self, ControlError> {
        // `docs/12 §1.2`：**默认 SQLite**（`<store_root>/control.db`，WAL）。
        let sqlite = Arc::new(crate::sqlite::SqliteStore::open(cfg.store_root.clone())?);
        let store = Arc::new(AnyConfigStore::Sqlite(sqlite.clone()));
        let mut registry = OpsRegistry::new_with_store(sqlite, cfg.liveness.clone());
        let restored = registry.restore()?;
        let mut plane = ControlPlane {
            ec,
            store,
            registry,
            cfg: cfg.clone(),
            sessions: BTreeMap::new(),
            plans: BTreeMap::new(),
            gates: BTreeMap::new(),
            catalog: CatalogSync::new(Some(cfg.store_root.clone())),
            fetcher: Arc::new(UnwiredFetcher),
            offchain: OffchainSync::new(),
            offchain_provider: Arc::new(UnwiredProvider),
            offchain_cfg: OffchainConfig::default(),
            started_at: crate::now_ms(),
            status: Status::Startup,
        };
        // 恢复实例会话（09 §6.3：控制面重启后从快照键恢复，避免历史心跳过期造成状态空洞）
        let known: Vec<(NodeKind, String)> = plane
            .registry
            .iter()
            .filter(|k| !matches!(k.status, InstStatus::Stopped))
            .map(|k| (k.node_kind, k.instance_id.clone()))
            .collect();
        for (kind, id) in known {
            let _ = plane.attach_instance(kind, &id);
        }
        let _ = restored;
        plane.load_plans()?;
        Ok(plane)
    }

    /// 配置存储。
    pub fn store(&self) -> &AnyConfigStore {
        &self.store
    }

    /// 快照键注册表。
    pub fn registry(&self) -> &OpsRegistry {
        &self.registry
    }

    /// 快照键注册表（可变）。
    pub fn registry_mut(&mut self) -> &mut OpsRegistry {
        &mut self.registry
    }

    /// 控制面配置。
    pub fn config(&self) -> &ControlConfig {
        &self.cfg
    }

    /// 某实例最新下发的计划。
    pub fn plan_of(&self, instance: &str) -> Option<&SubscribePlan> {
        self.plans.get(instance).map(|a| &a.plan)
    }

    /// 某实例的 `assignment_version` 门。
    pub fn gate(&self, instance: &str) -> Option<&VersionGate> {
        self.gates.get(instance)
    }

    /// 运行状态（心跳 `status`）。
    pub fn status(&self) -> Status {
        self.status
    }

    /// 置运行状态。
    pub fn set_status(&mut self, status: Status) {
        self.status = status;
    }

    // ── 会话 ────────────────────────────────────────────────────────────────

    /// 为某实例建立会话：`ops.<kind>.<instance>` + `ctl.request.<instance>`
    /// （09 §6.1：按实例命名的主题由控制面在 register 时建立，失联/下线后关闭）。
    pub fn attach_instance(&mut self, kind: NodeKind, instance: &str) -> Result<(), ControlError> {
        if self.sessions.contains_key(instance) {
            return Ok(());
        }
        let opts = SessionOpts {
            timeout: Duration::from_millis(20),
            ..SessionOpts::default()
        };
        let group = ponk_protocol::Group::new(self.cfg.group.clone());
        let ops_topic = topics::ops_of(kind, instance);
        let req_topic = topics::request(instance);

        let ops = match self.ec.open(&ops_topic, &group, opts.clone()) {
            Ok(s) => Some(s),
            Err(_) => None,
        };
        let request = match self.ec.open(&req_topic, &group, opts) {
            Ok(s) => Some(s),
            Err(_) => None,
        };
        if ops.is_none() && request.is_none() {
            return Err(ControlError::Illegal(format!(
                "无法为主题 {ops_topic} / {req_topic} 打开会话"
            )));
        }
        self.sessions.insert(
            instance.to_string(),
            InstanceSessions {
                ops_topic,
                request_topic: req_topic,
                ops,
                request,
            },
        );
        Ok(())
    }

    /// 关闭某实例的主题会话（09 §6.2 shutdown / §6.4 判死后）。
    pub fn detach_instance(&mut self, instance: &str) {
        if let Some(mut sess) = self.sessions.remove(instance) {
            if let Some(mut s) = sess.ops.take() {
                let _ = s.close();
            }
            if let Some(mut s) = sess.request.take() {
                let _ = s.close();
            }
        }
    }

    /// 从已恢复的快照键批量建立会话，返回成功数。
    pub fn attach_known(&mut self) -> usize {
        let known: Vec<(NodeKind, String)> = self
            .registry
            .iter()
            .filter(|k| !matches!(k.status, InstStatus::Stopped | InstStatus::Dead))
            .map(|k| (k.node_kind, k.instance_id.clone()))
            .collect();
        let mut n = 0;
        for (kind, id) in known {
            if self.attach_instance(kind, &id).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// 已建立会话的实例数。
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    // ── 轮询 ────────────────────────────────────────────────────────────────

    /// 拉取所有已建立会话的消息并分发、回发。
    ///
    /// 消费侧**无 ack**（03 §4.4）：读到即推进位点，处理失败的唯一可见出口是
    /// 版本不收敛（09 §4.4）。
    pub fn poll(&mut self, timeout: Duration) -> Result<usize, ControlError> {
        let mut incoming: Vec<(Topic, Envelope)> = Vec::new();

        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in &ids {
            let Some(sess) = self.sessions.get_mut(id) else {
                continue;
            };
            let ops_topic = sess.ops_topic.clone();
            let req_topic = sess.request_topic.clone();
            let mut first = true;
            if let Some(s) = sess.ops.as_mut() {
                let t = if first { timeout } else { Duration::ZERO };
                first = false;
                drain(s, t, &ops_topic, &mut incoming);
            }
            if let Some(s) = sess.request.as_mut() {
                let t = if first { timeout } else { Duration::ZERO };
                drain(s, t, &req_topic, &mut incoming);
            }
        }

        let n = incoming.len();
        let mut outbox: Vec<Outbound> = Vec::new();
        for (topic, env) in incoming {
            match self.dispatch(topic.as_str(), env) {
                Ok(msgs) => outbox.extend(msgs),
                // 单条消息的校验/解析失败不中断轮询：控制面须保持可观测（09 §4.4）
                Err(_) => {}
            }
        }
        self.publish_all(&outbox)?;
        Ok(n)
    }

    /// 分发一条入站消息，返回应答（**不发布**，便于离线测试）。
    pub fn dispatch(&mut self, topic: &str, env: Envelope) -> Result<Vec<Outbound>, ControlError> {
        let now = if env.received_ts > 0 {
            env.received_ts
        } else {
            crate::now_ms()
        };
        if let Some(instance) = topics::instance_of_request(topic) {
            return self.handle_pull(instance, &env, now);
        }
        if let Some((kind, instance)) = topics::instance_of_ops(topic) {
            if kind == NodeKind::Control {
                // 09 §6.5：控制面不消费自己的主题
                return Ok(Vec::new());
            }
            return self.handle_ops(kind, instance, &env, now);
        }
        Ok(Vec::new())
    }

    /// 批量发布。
    pub fn publish_all(&self, msgs: &[Outbound]) -> Result<usize, ControlError> {
        for m in msgs {
            self.ec.publish(&m.topic, &m.envelope)?;
        }
        Ok(msgs.len())
    }

    // ── 控制信道消费（09 §6）────────────────────────────────────────────────

    fn handle_ops(
        &mut self,
        _kind: NodeKind,
        topic_instance: &str,
        env: &Envelope,
        now: u64,
    ) -> Result<Vec<Outbound>, ControlError> {
        let Body::Ops(hb) = &env.body else {
            return Ok(Vec::new());
        };
        // 会话按 instance_id 建立；topic 与 body 不一致时以 body 的 instance_id 为准
        let instance = hb.instance_id.as_str();
        debug_assert_eq!(topic_instance, instance, "ops 主题与心跳 instance_id 不一致");
        if !self.sessions.contains_key(instance) {
            self.attach_instance(hb.node_kind, instance)?;
        }

        match hb.status {
            Status::Startup => {
                self.registry.on_register(hb, now)?;
                // 09 §6.2：记录能力 → 写快照键 → 指派 → 经命令信道回初始 snapshot
                // 重新注册（此前已有计划 / 判死后重登）：必须强制重推并递增版本，
                // 否则实例分片签名未变会走 emit_changed_plans 的「无变化不下发」而收不到计划
                if self.plans.contains_key(instance) {
                    // 强制重推对所有活实例递增版本；已死重登者经 registry.on_register
                    // 已置为 Registered，会被 recompute_inner 纳入候选 → 拿到递增版本的计划
                    self.recompute_forced()
                } else {
                    self.recompute()
                }
            }
            Status::Running => {
                self.registry.on_heartbeat(hb, now)?;
                self.handle_heartbeat(hb, now)
            }
            Status::Shutdown => {
                self.registry.on_shutdown(hb, now)?;
                // 09 §6.2：标记正常下线、关闭该实例的主题会话、快照键 status = Stopped
                self.detach_instance(instance);
                Ok(Vec::new())
            }
        }
    }

    /// 心跳 → 逐域收敛比对（09 §4.4）→ 不收敛则超时重推。
    fn handle_heartbeat(
        &mut self,
        hb: &OpsHeartbeat,
        now: u64,
    ) -> Result<Vec<Outbound>, ControlError> {
        let instance = hb.instance_id.as_str();
        let conv = self.registry.convergence(&*self.store, now);
        let mine = conv
            .not_converged
            .iter()
            .find(|n| n.instance_id == instance)
            .cloned();
        let converged = mine.is_none();
        self.registry.mark_convergence(instance, converged, now)?;

        if converged || !self.registry.repush_due(instance, now) {
            return Ok(Vec::new());
        }

        // 09 §4.4：不收敛超时（默认 30s）→ 控制面重推
        let mut msgs = Vec::new();
        if let Some(n) = &mine {
            match n.domain {
                None => {
                    // 采集：整包重推 Snapshot，assignment_version 递增（09 §5.4）
                    msgs.extend(self.repush_plan(instance)?);
                }
                Some(domain) => {
                    // 其余节点：重发变更通知 + 直接定向推配置体
                    msgs.push(self.make_changed(domain, &self.cfg.publisher, now)?);
                    let kind = self
                        .registry
                        .get(instance)
                        .map(|k| k.node_kind)
                        .unwrap_or(NodeKind::Collector);
                    if let Some(m) = self.make_snapshot(instance, kind, domain)? {
                        msgs.push(m);
                    }
                }
            }
        }
        self.registry.mark_repush(instance, now)?;
        Ok(msgs)
    }

    /// **公开的重推入口**（`docs/13 §4.10 POST /runtime/instances/{id}/repush`）。
    ///
    /// 只产出 `Outbound`、**不发布**——与 `recompute` 同口径，投递由调用方
    /// `publish_all` 负责（与 09 §4.3 / §4.4 的重推时序一致）。
    ///
    /// 返回空向量表示**没有可重推的计划**（该实例从未被指派，或已从计划表移除），
    /// 调用方据此回 404 而不是 200 空成功。
    pub fn repush(&mut self, instance: &str) -> Result<Vec<Outbound>, ControlError> {
        self.repush_plan(instance)
    }

    /// 该实例是否存在**可重推的计划**（HTTP 侧用于 404 判定）。
    pub fn has_plan(&self, instance: &str) -> bool {
        self.plans.contains_key(instance)
    }

    /// 重推某实例的订阅计划（`assignment_version` 递增，09 §5.4 / §4.4）。
    fn repush_plan(&mut self, instance: &str) -> Result<Vec<Outbound>, ControlError> {
        let kind = self
            .registry
            .get(instance)
            .map(|k| k.node_kind)
            .unwrap_or(NodeKind::Collector);
        self.repush_plan_to(instance, kind)
    }

    /// 指定应答主题种类的重推（拉取应答在实例尚未 register 时只能以请求方自报的
    /// `node_kind` 寻址，03 §8 `ctl.<node_kind>.<instance>`）。
    fn repush_plan_to(
        &mut self,
        instance: &str,
        kind: NodeKind,
    ) -> Result<Vec<Outbound>, ControlError> {
        let Some(prev) = self.plans.get(instance) else {
            return Ok(Vec::new());
        };
        let mut next = prev.clone();
        let version = next_assignment_version(Some(prev.plan.assignment_version));
        next.plan.assignment_version = version;
        next.plan.mode = PlanMode::Snapshot;
        for a in next.quota_splits.values_mut() {
            a.version = version;
        }
        let out = self.dispatch_plan(instance, kind, &next)?;
        // **下发成功必须同步 `self.plans`**：`dispatch_plan` 只推进版本门与快照键，
        // 不更新派生物就会出现「门在 v2、计划还是 v1」→ 下次重算算出 v2 → 被门拒收 →
        // 该轮计划被静默丢弃（拉取后第一次重算必丢，且会反复丢失）。
        if !out.is_empty() {
            self.plans.insert(instance.to_string(), next);
        }
        Ok(out)
    }

    /// 下发一份计划：过 `assignment_version` 门 → 存派生物快照 → 发布。
    ///
    /// `kind` 决定应答主题 `ctl.<node_kind>.<instance>`（03 §8）：指派重算时取自
    /// 快照键，拉取应答时取自请求方自报（实例可能尚未 register）。
    fn dispatch_plan(
        &mut self,
        instance: &str,
        kind: NodeKind,
        assignment: &InstanceAssignment,
    ) -> Result<Vec<Outbound>, ControlError> {
        let gate = self
            .gates
            .entry(instance.to_string())
            .or_insert_with(VersionGate::default);
        let version = assignment.plan.assignment_version;
        if matches!(
            gate.accept(version),
            crate::assign::OrderDecision::Dropped
        ) {
            return Ok(Vec::new()); // 10 §4.3：旧版丢弃
        }

        if !self.sessions.contains_key(instance) {
            let _ = self.attach_instance(kind, instance);
        }

        self.persist_plan(instance, assignment)?;
        self.registry.set_assignment(instance, version)?;

        let env = crate::ctl_envelope(self.cfg.instance_id.clone(), crate::now_ms(), {
            CtlMessage::SubscribePlan(assignment.plan.clone())
        })?;
        Ok(vec![Outbound::new(topics::to_node(kind, instance), env)])
    }

    // ── 拉取应答（09 §4.3 步骤 ③④）────────────────────────────────────────

    fn handle_pull(
        &mut self,
        instance: &str,
        env: &Envelope,
        now: u64,
    ) -> Result<Vec<Outbound>, ControlError> {
        let (kind, domains, applied) = match &env.body {
            Body::Ops(hb) => {
                // 拉取请求承载 `applied_version`（10 §4.1 / §5「pull snapshot」）
                (hb.node_kind, consumed_domains(hb.node_kind), hb.applied_version)
            }
            Body::Ctl(CtlMessage::ConfigChanged(c)) => {
                let kind = self
                    .registry
                    .get(instance)
                    .map(|k| k.node_kind)
                    .unwrap_or(NodeKind::Collector);
                (kind, vec![c.domain], Some(c.version))
            }
            Body::Ctl(CtlMessage::SubscribePlan(p)) => {
                let kind = self
                    .registry
                    .get(instance)
                    .map(|k| k.node_kind)
                    .unwrap_or(NodeKind::Collector);
                (kind, vec![ConfigDomain::SubscribePlan], Some(p.assignment_version))
            }
            _ => return Ok(Vec::new()),
        };

        let _ = applied;
        if !self.sessions.contains_key(instance) {
            self.attach_instance(kind, instance)?;
        }

        let mut out = Vec::new();
        for domain in domains {
            match domain {
                // 09 §2.2 C：ProcessCapacity 无外部消费者，不发通知也不应答
                ConfigDomain::ProcessCapacity => {}
                ConfigDomain::SubscribePlan => {
                    // 09 §4.3：SubscribePlan 域应答 SubscribePlan（每次下发 +1）
                    out.extend(self.repush_or_new_plan(instance, kind)?);
                }
                _ => {
                    if let Some(m) = self.make_snapshot(instance, kind, domain)? {
                        out.push(m);
                    }
                }
            }
        }
        let _ = now;
        Ok(out)
    }

    /// 构造 `ConfigSnapshot` 应答 → `ctl.<node_kind>.<instance>`（03 §8 主题通则）。
    ///
    /// `kind` 由**请求方**决定（见 [`ControlPlane::handle_pull`]），不查快照键：
    /// 实例可以先拉取、后 register（10 §7 启动时序里 register 在 pull 之前，
    /// 但两条消息各自独立，控制面不假定顺序）。
    fn make_snapshot(
        &self,
        instance: &str,
        kind: NodeKind,
        domain: ConfigDomain,
    ) -> Result<Option<Outbound>, ControlError> {
        let Some(doc) = self.store.current(domain)? else {
            return Ok(None); // 该域尚未发布过 → 无可应答内容
        };
        let env = crate::ctl_envelope(
            self.cfg.instance_id.clone(),
            crate::now_ms(),
            CtlMessage::ConfigSnapshot {
                domain,
                version: doc.version,
                payload: ConfigPayload(doc.content),
            },
        )?;
        Ok(Some(Outbound::new(topics::to_node(kind, instance), env)))
    }

    /// 拉取订阅计划域：已有计划则递增重发；尚未指派则先算一次。
    fn repush_or_new_plan(
        &mut self,
        instance: &str,
        kind: NodeKind,
    ) -> Result<Vec<Outbound>, ControlError> {
        if self.plans.contains_key(instance) {
            return self.repush_plan_to(instance, kind);
        }
        // 尚无任何采集在场：先按当前域文档算一次，再把本实例的计划定向回出
        let msgs = self.recompute()?;
        if let Some(a) = self.plans.get(instance).cloned() {
            return self.dispatch_plan(instance, kind, &a).map(|m| {
                let mut all = msgs;
                all.extend(m);
                all
            });
        }
        Ok(msgs)
    }

    // ── 发布流程（09 §4.3）──────────────────────────────────────────────────

    /// 发布一个域：校验 → 版本 +1 → （非 ProcessCapacity）发 `ConfigChanged` →
    /// 触发指派重算（订阅计划 / 黑名单 / 承载参数三个域，09 §5.4）。
    pub fn publish_domain(
        &mut self,
        domain: ConfigDomain,
        content: serde_json::Value,
        publisher: &str,
        reason: &str,
    ) -> Result<(StoredDoc, Vec<Outbound>), ControlError> {
        validate::validate(domain, &content, &*self.store)?;
        let doc = self.store.publish(domain, content, publisher, reason)?;
        let now = crate::now_ms();
        let mut out = Vec::new();

        // 09 §2.2 C：ProcessCapacity 不发通知（消费者在控制面内）
        if domain != ConfigDomain::ProcessCapacity {
            out.push(self.make_changed(domain, publisher, now)?);
        }
        // 09 §5.4：这三个域的发布触发指派重算
        if matches!(
            domain,
            ConfigDomain::SubscribePlan
                | ConfigDomain::InstrumentBlocklist
                | ConfigDomain::ProcessCapacity
        ) {
            out.extend(self.recompute()?);
        }
        Ok((doc, out))
    }

    /// 回滚 = 版本历史恢复（09 §4.5）：取历史内容**重新发布为新版本号**。
    pub fn rollback(
        &mut self,
        domain: ConfigDomain,
        target: u64,
        publisher: &str,
    ) -> Result<(StoredDoc, Vec<Outbound>), ControlError> {
        let src = self
            .store
            .get(domain, target)?
            .ok_or_else(|| ControlError::not_found(format!("{domain} v{target} 不存在")))?;
        let reason = format!("rollback to v{target}");
        self.publish_domain(domain, src.content, publisher, &reason)
    }

    /// `ConfigChanged{domain, version, publisher, ts}` → `ctl.<domain>`（09 §4.3 步骤 ②）。
    ///
    /// `publisher` = **本次发布的发布者**（09 §3.1：`控制面实例/操作者`，
    /// 如 `cp-1/alice`、自动化为 `cp-1/api`），不是控制面自身标识——
    /// 审计要能回答「谁改的」。控制面内部触发的重推（09 §4.4）才用 `cfg.publisher`。
    fn make_changed(
        &self,
        domain: ConfigDomain,
        publisher: &str,
        now: u64,
    ) -> Result<Outbound, ControlError> {
        let version = self.store.current_version(domain)?.unwrap_or(0);
        let env = crate::ctl_envelope(self.cfg.instance_id.clone(), now, {
            CtlMessage::ConfigChanged(ConfigChanged {
                domain,
                version,
                publisher: publisher.to_string(),
                ts: now,
            })
        })?;
        Ok(Outbound::new(topics::changed(domain), env))
    }

    // ── 指派（09 §5）────────────────────────────────────────────────────────

    /// 载入八域当前文档（缺域时用默认空文档，不阻塞启动）。
    pub fn load_domains(&self) -> Result<BTreeMap<ConfigDomain, StoredDoc>, ControlError> {
        let mut out = BTreeMap::new();
        for d in ConfigDomain::ALL {
            if let Some(doc) = self.store.current(d)? {
                out.insert(d, doc);
            }
        }
        Ok(out)
    }

    fn domain_or_default<T: Default + serde::de::DeserializeOwned>(
        &self,
        domain: ConfigDomain,
    ) -> Result<T, ControlError> {
        match self.store.current(domain)? {
            Some(doc) => Ok(serde_json::from_value::<T>(doc.content)?),
            None => Ok(T::default()),
        }
    }

    /// 重算指派（09 §5.4）：候选 = 活着的采集实例；只对**计划发生变化**的实例下发。
    pub fn recompute(&mut self) -> Result<Vec<Outbound>, ControlError> {
        self.recompute_inner(false)
    }

    /// 强制重算并**对每个活实例下发**（09 §5.4 触发表：
    /// 「定时任务发现标的变更 → 更新 catalog → 重算受影响实例，
    ///   下发同 `SubscribePlan` 域发布」）。
    ///
    /// 与 [`recompute`] 的差别：catalog 变更不一定改变分片集合
    /// （例如只改了某个标的的 `status`），但 §5.4 要求它仍然下发，
    /// 因此这里不比对签名、直接递增 `assignment_version`。
    pub fn recompute_forced(&mut self) -> Result<Vec<Outbound>, ControlError> {
        self.recompute_inner(true)
    }

    /// O8 决议：**定义层的启停参与候选过滤** —— 停用节点（`enabled = 0`）与其
    /// 关联机器非 `active`（维护中 / 已退役）的实例不再接收新计划。
    ///
    /// 注意边界：停用只是**不再下发**，实例上一次的计划仍在其上（不自动撤回，
    /// 与 09 §7「失联不自动再指派」同一口径）。
    fn disabled_instances(&self) -> Result<BTreeSet<String>, ControlError> {
        let Some(sqlite) = self.store.sqlite() else {
            return Ok(BTreeSet::new()); // 文件树后端无节点定义 → 不参与过滤
        };
        let mut out = BTreeSet::new();
        for node in sqlite.list_nodes()? {
            if !node.enabled {
                out.insert(node.name);
                continue;
            }
            if let Some(sid) = node.server_id {
                if let Some(srv) = sqlite.server_by_id(sid)? {
                    if srv.status.as_str() != "active" {
                        out.insert(node.name);
                    }
                }
            }
        }
        Ok(out)
    }

    fn recompute_inner(&mut self, force: bool) -> Result<Vec<Outbound>, ControlError> {
        let disabled = self.disabled_instances()?;
        let candidates: Vec<Candidate> = self
            .registry
            .iter()
            .filter(|k| {
                k.node_kind == NodeKind::Collector
                    && matches!(k.status, InstStatus::Registered | InstStatus::Running)
                    && !disabled.contains(&k.instance_id) // O8：停用节点不进候选
            })
            .map(|k| Candidate {
                instance_id: k.instance_id.clone(),
                host: k.host.clone(),
                node_kind: k.node_kind,
                reported: k.reported.clone(),
            })
            .collect();

        // 09 §7 失联 / shutdown **不自动再指派**：这里只是把它们排除出候选集，
        // **不删除**已下发计划与 `assignment_version` 门——否则实例重启 register 后
        // 版本号会从 1 重来，被自己的强序门（10 §4.3「旧版丢弃」）永久拒收。
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let mut plan_doc: SubscribePlanDoc = self.domain_or_default(ConfigDomain::SubscribePlan)?;
        let blocklist: BlocklistDoc = self.domain_or_default(ConfigDomain::InstrumentBlocklist)?;
        let capacity: ProcessCapacityDoc = self.domain_or_default(ConfigDomain::ProcessCapacity)?;
        let limits: CapacityLimits = capacity.limits.clone();
        let strategy = Strategy::parse(&capacity.distribution.strategy)?;

        let mut versions = BTreeMap::new();
        for c in &candidates {
            let cur = self.plans.get(&c.instance_id).map(|a| a.plan.assignment_version);
            versions.insert(c.instance_id.clone(), next_assignment_version(cur));
        }


        // A 方案（docs/12 §5 / 模型 1）：逐实例 scope → 允许集，**并集池统一切分片**。
        // * 有节点定义的实例：允许集 = 节点 scope 解析结果 ∪ 手工池；
        // * 无节点定义的实例：不写入允许集 = 不限制（兼容既有行为）；
        // * 无 SQLite 后端（文件树 / 纯内存）时整段跳过，行为与改造前一致。
        let mut allowed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        if let Some(sqlite) = self.store.sqlite() {
            let catalog = self.catalog.catalog();
            let canon = |i: &crate::schema::PlanInstrument| -> String {
                format!("{}:{}:{}", i.venue, i.market, i.native_code)
            };
            let mut union: BTreeMap<String, crate::schema::PlanInstrument> = BTreeMap::new();
            for i in &plan_doc.instruments {
                union.insert(canon(i), i.clone());
            }
            for c in &candidates {
                let Some(node) = sqlite.node_by_name(&c.instance_id)? else {
                    continue; // 无节点定义 → 不限制（09 §2.3：节点定义是操作者输入）
                };
                let out = crate::scope::resolve(
                    &node.scope,
                    &node.scope_include,
                    &node.scope_exclude,
                    catalog,
                );
                let mut set: BTreeSet<String> = out.instruments.iter().map(|i| canon(i)).collect();
                // 手工池对每个实例开放（docs/12 §5：逐条 instruments = 手工例外）
                for i in &plan_doc.instruments {
                    set.insert(canon(i));
                }
                allowed.insert(c.instance_id.clone(), set);
                for i in out.instruments {
                    union.insert(canon(&i), i);
                }
            }
            if !allowed.is_empty() {
                plan_doc.instruments = union.into_values().collect();
            }
        }

        let result = assign(AssignRequest {
            candidates,
            plan_doc,
            blocklist,
            limits,
            strategy,
            mode: PlanMode::Snapshot,
            versions,
            allowed,
        })?;

        self.emit_changed_plans(&result, force)
    }

    /// 只下发「分片集合发生变化」的实例（局部变更 vs 分片重切，09 §5.4）。
    fn emit_changed_plans(
        &mut self,
        result: &AssignmentResult,
        force: bool,
    ) -> Result<Vec<Outbound>, ControlError> {
        let mut out = Vec::new();
        let mut pending: Vec<(String, InstanceAssignment)> = Vec::new();

        for (id, assignment) in &result.plans {
            let changed = force
                || match self.plans.get(id) {
                    None => true,
                    Some(prev) => signature(&prev.plan) != signature(&assignment.plan),
                };
            if changed {
                pending.push((id.clone(), assignment.clone()));
            }
        }

        for (id, assignment) in pending {
            let kind = self
                .registry
                .get(&id)
                .map(|k| k.node_kind)
                .unwrap_or(NodeKind::Collector);
            out.extend(self.dispatch_plan(&id, kind, &assignment)?);
            self.plans.insert(id, assignment);
        }
        Ok(out)
    }

    fn persist_plan(
        &self,
        instance: &str,
        assignment: &InstanceAssignment,
    ) -> Result<(), ControlError> {
        // 09 §4.2：每次下发的 SubscribePlan 按 assignment_version 存内容快照
        if let Some(sqlite) = self.store.sqlite() {
            let splits = serde_json::to_string(&assignment.quota_splits)?;
            return sqlite.save_plan(
                instance,
                &assignment.plan,
                &splits,
                crate::now_ms(),
            );
        }
        let dir = self
            .cfg
            .store_root
            .join("state")
            .join("plans")
            .join(instance);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("v{}.json", assignment.plan.assignment_version));
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&assignment.plan)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn load_plans(&mut self) -> Result<(), ControlError> {
        if let Some(sqlite) = self.store.sqlite() {
            // 按版本升序返回 → 逐条覆盖，最终留下每个实例的最高版本
            for (instance, plan) in sqlite.load_plans()? {
                self.gates
                    .insert(instance.clone(), VersionGate::new(plan.assignment_version));
                self.plans.insert(
                    instance,
                    InstanceAssignment {
                        quota_splits: BTreeMap::new(),
                        plan,
                    },
                );
            }
            return Ok(());
        }
        let dir = self.cfg.store_root.join("state").join("plans");
        if !dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let instance = entry.file_name().to_string_lossy().to_string();
            let mut best: Option<InstanceAssignment> = None;
            for f in std::fs::read_dir(entry.path())? {
                let f = f?;
                if f.path().extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let plan: SubscribePlan = serde_json::from_slice(&std::fs::read(f.path())?)?;
                let better = match &best {
                    None => true,
                    Some(b) => plan.assignment_version > b.plan.assignment_version,
                };
                if better {
                    best = Some(InstanceAssignment {
                        quota_splits: BTreeMap::new(),
                        plan,
                    });
                }
            }
            if let Some(a) = best {
                self.gates.insert(
                    instance.clone(),
                    VersionGate::new(a.plan.assignment_version),
                );
                self.plans.insert(instance, a);
            }
        }
        Ok(())
    }

    // ── 控制面自身心跳（09 §6.5）────────────────────────────────────────────

    /// 构造 `ops.control` 心跳：`applied_version = None`（它是版本来源，09 §4.4），
    /// `counters` 携带各域**当前版本** `cfg.<domain>` 与不收敛/失联实例计数。
    pub fn control_heartbeat(&self, now: u64) -> Result<Envelope, ControlError> {
        let mut counters = self.store.current_versions()?;
        let conv = self.registry.convergence(&*self.store, now);
        counters.insert("instances.running".into(), conv.running as i64);
        counters.insert("instances.stopped".into(), conv.stopped as i64);
        counters.insert("instances.dead".into(), conv.dead as i64);
        counters.insert("not_converged".into(), conv.not_converged.len() as i64);
        counters.insert(
            "not_converged_timeout".into(),
            conv.timed_out(self.cfg.liveness.converge_timeout_ms) as i64,
        );
        counters.insert(
            "lost_total".into(),
            self.registry.iter().map(|k| k.lost_count).sum::<u64>() as i64,
        );
        Envelope::new(
            EventType::Ops,
            self.cfg.instance_id.clone(),
            now,
            Body::Ops(OpsHeartbeat {
                host: None,
                instance_id: self.cfg.instance_id.clone(),
                node_kind: NodeKind::Control,
                status: self.status,
                applied_version: None,
                counters,
                started_at: self.started_at,
                ts: now,
            }),
        )
        .map_err(ControlError::from)
    }

    /// 发布控制面自身心跳到 `ops.control`。
    pub fn publish_heartbeat(&self, now: u64) -> Result<Envelope, ControlError> {
        let env = self.control_heartbeat(now)?;
        self.ec
            .publish(&topics::control_heartbeat(), &env)
            .map_err(ControlError::from)?;
        Ok(env)
    }

    /// 收敛比对快照（09 §4.4）。
    pub fn convergence(&self, now: u64) -> Convergence {
        self.registry.convergence(&*self.store, now)
    }

    // ── 09 §2.3 定时任务：交易对与币信息同步 ──────────────────────────────

    /// 注入元数据拉取器（真现实现接 dig3 `get_exchange_info` / 所方公开端点；
    /// 测试注入假实现，全离线）。
    pub fn set_fetcher(&mut self, fetcher: Arc<dyn MetadataFetcher>) {
        self.fetcher = fetcher;
    }

    /// catalog 内部状态（**不是配置域**，09 §2.3 产物第 1 路）。
    pub fn catalog(&self) -> &crate::catalog::Catalog {
        self.catalog.catalog()
    }

    /// 定时任务本体（09 §2.3 / §5.4 新增触发行）。
    ///
    /// 返回的 [`SyncOutcome`] 里：
    /// * `ran = false` —— 未到 5 分钟周期；
    /// * `changed = false` —— 与上一轮 catalog **零差异 → 零动作**
    ///   （不 bump 域版本、不发 `ConfigChanged`、不重算指派）；
    /// * `recompute_ran = true` —— 上新/下架/状态或精度变化已触发指派重算。
    pub fn run_metadata_sync(&mut self, now: u64) -> Result<SyncOutcome, ControlError> {
        let targets = self.metadata_targets()?;
        let quotas = self.quota_specs();
        let mut outcome = self
            .catalog
            .run(&targets, &quotas, &*self.fetcher, now)
            .map_err(|e: MetaError| match e {
                MetaError::Unsupported(m) | MetaError::Fetch(m) | MetaError::Protocol(m) => {
                    ControlError::Illegal(m)
                }
            })?;

        if !outcome.changed {
            return Ok(outcome); // 硬要求：无差异 = 零动作（已在 catalog 侧打了日志）
        }

        eprintln!(
            "[sync] 变更生效：md.meta {} 条 → 发布到数据信道",
            outcome.md_meta.len()
        );

        // 产物第 3 路：`md.meta` 经总线发布（红线 4，不直连存储）
        for env in &outcome.md_meta {
            self.ec
                .publish(&topics::md_meta(), env)
                .map_err(ControlError::from)?;
        }

        // 产物第 2 路：标的变更 → 指派重算 → **投递**（09 §5.4 触发表）
        //
        // 算出的 `Outbound` 必须 `publish_all`：重算只产出"该发什么"，不发就等于
        // 上新永远到不了采集实例（曾出现：recompute_ran = true 却无人收到计划）。
        let dispatched = if outcome.diff.triggers_reassignment() {
            let out = self.recompute_forced()?;
            let n = self.publish_all(&out)?;
            eprintln!("[sync] 标的变更触发指派重算 → 投递 {n} 条到 ctl.collector.*");
            n > 0
        } else {
            eprintln!("[sync] 差异不触发重算（触发条件见 09 §5.4 表）");
            false
        };
        outcome.recompute_ran = outcome.diff.triggers_reassignment() && dispatched;
        Ok(outcome)
    }

    /// 交易所同步目标：`exchange_market` 中 **`enabled = 1` 的 `(venue, market)`**
    /// （`docs/14 §2`——**不是 `SubscribePlan` 域**）。
    ///
    /// 理由（`docs/14 §2` 原文口径）：`SubscribePlan` 是「采集意图」，派生自节点
    /// scope，冷启动为空 → 造成「先有鸡还是先有蛋」的死锁；而 `exchange_market`
    /// 是「接入台账」。**目录同步要回答的是"这个所有什么标的"，不是"我要采什么"**。
    /// `exchange.enabled = 0` 或市场 `enabled = 0` 的所停止同步（省配额与存储）。
    ///
    /// 没有 SQLite 后端时返回空集合（同步零动作，不报错）。
    pub fn metadata_targets(&self) -> Result<Vec<(String, String)>, ControlError> {
        match self.store.sqlite() {
            Some(db) => db.enabled_market_targets(),
            None => Ok(Vec::new()),
        }
    }

    /// 外部（币）同步任务（`docs/14 §3`）—— `GET /sync/offchain/status` 读
    /// `interval_ms` / `last_round_ms` / `is_running` / `due`。
    pub fn offchain_sync(&self) -> &OffchainSync {
        &self.offchain
    }

    /// 外部同步配置（`control.toml [providers]`，`docs/14 §8`）。
    pub fn offchain_config(&self) -> &OffchainConfig {
        &self.offchain_cfg
    }

    /// 注入外部币信息拉取器（真现实现接 CoinGecko；测试注入假实现，全离线）。
    pub fn set_offchain_provider(&mut self, provider: Arc<dyn OffchainProvider>) {
        self.offchain_provider = provider;
    }

    /// 下发 `[providers]` 配置（含节拍）。
    pub fn set_offchain_config(&mut self, cfg: OffchainConfig) {
        self.offchain.interval_ms = cfg.offchain_interval_ms;
        self.offchain_cfg = cfg;
    }

    /// 定时任务本体：一轮外部（币）信息同步（`docs/14 §3`）。
    ///
    /// 失败/限速**全部折进** [`OffchainOutcome`]（`error` / `rate_limited`），
    /// 落一条 `offchain_sync_run`——**不向上传播**，避免伪装成交易所同步失败。
    pub fn run_offchain_sync(&mut self, now: u64) -> OffchainOutcome {
        let Some(db) = self.store.sqlite().cloned() else {
            return OffchainOutcome {
                ran: true,
                error: Some("控制面未启用 SQLite 后端，外部同步无法落库".to_string()),
                ..OffchainOutcome::default()
            };
        };
        let cfg = self.offchain_cfg.clone();
        self.offchain.run(&db, &*self.offchain_provider, &cfg, now)
    }

    /// **强制**触发一轮外部同步（`POST /sync/offchain/runs`，`docs/14 §9`）。
    ///
    /// 与 `run_metadata_sync_forced` 同一口径：临时把周期置 0 跑完恢复，
    /// `last_round_ms` 已在轮首前移，之后的定时任务仍按原周期节流。
    pub fn run_offchain_sync_forced(&mut self, now: u64) -> OffchainOutcome {
        let Some(db) = self.store.sqlite().cloned() else {
            return OffchainOutcome {
                ran: true,
                error: Some("控制面未启用 SQLite 后端，外部同步无法落库".to_string()),
                ..OffchainOutcome::default()
            };
        };
        let cfg = self.offchain_cfg.clone();
        self.offchain.run_forced(&db, &*self.offchain_provider, &cfg, now)
    }

    /// 各所的元数据份额（02 §6：`quota[].metadata`，缺省 0.15）。
    fn quota_specs(&self) -> Vec<crate::schema::QuotaSpec> {
        self.domain_or_default::<SubscribePlanDoc>(ConfigDomain::SubscribePlan)
            .map(|d| d.quota)
            .unwrap_or_default()
    }

    // ── 失联判定（09 §6.4）──────────────────────────────────────────────────

    /// 扫描失联：`shutdown` 缺失 + 心跳断 3×5s → 判死（10 §5）。
    ///
    /// 控制面侧动作限定为：快照键 `status = Dead`、关闭该实例主题会话、
    /// 失联计数写入 `ops.control`；**不触发任何自动指派变更**（09 §7）。
    pub fn sweep(&mut self, now: u64) -> Result<Vec<String>, ControlError> {
        let newly = self.registry.sweep(now)?;
        for id in &newly {
            self.detach_instance(id);
        }
        Ok(newly)
    }

    /// 列出全部快照键。
    pub fn instances(&self) -> Vec<&SnapshotKey> {
        self.registry.iter().collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 工具
// ─────────────────────────────────────────────────────────────────────────────

fn drain(
    s: &mut Box<dyn Session>,
    timeout: Duration,
    topic: &Topic,
    incoming: &mut Vec<(Topic, Envelope)>,
) {
    let mut t = timeout;
    loop {
        match s.next(t) {
            Ok(Some(env)) => {
                incoming.push((topic.clone(), env));
                t = Duration::ZERO; // 首条之后不阻塞
            }
            Ok(None) => break,
            Err(_) => break, // 会话关闭 / backend 错误：本轮停止该会话
        }
    }
}

/// 计划签名：分片集合（用于判断「计划是否变化」，09 §5.4）。
fn signature(plan: &SubscribePlan) -> String {
    let quota = plan
        .quota_share
        .as_ref()
        .map(|q| format!("{}@{}={}", q.venue, q.egress_ip, q.collect))
        .unwrap_or_default();
    let mut parts: Vec<String> = plan
        .shards
        .iter()
        .map(|s| {
            let codes: Vec<&str> = s
                .instruments
                .iter()
                .map(|i| i.native_code.as_str())
                .collect();
            format!(
                "{}:{}:{}=[{}]",
                s.venue,
                s.market,
                s.shard_idx,
                codes.join("|")
            )
        })
        .collect();
    parts.sort();
    format!("{}|quota={quota}", parts.join(";"))
}

/// `Instant` → ms（供运行循环使用）。
pub fn since_ms(t0: Instant) -> u64 {
    t0.elapsed().as_millis() as u64
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP 服务层所需的访问器（docs/13 §4.3 / §4.12）
//
// 只在文件末尾**追加**一个新 `impl` 块，不改动上方任何既有函数体。
// ─────────────────────────────────────────────────────────────────────────────

impl<E: EventCenter> ControlPlane<E> {
    /// 控制面启动时刻（ms）—— `GET /healthz` 的 `started_at`（docs/13 §4.3）。
    pub fn started_at(&self) -> u64 {
        self.started_at
    }

    /// 元数据同步任务（09 §2.3）—— `GET /sync/status` 需要读
    /// `interval_ms` / `last_round_ms` / `is_running` / `due` / `quota`
    /// （docs/13 §6.1 明列「`ControlPlane` 未暴露 `&CatalogSync`，需补访问器」）。
    pub fn catalog_sync(&self) -> &CatalogSync {
        &self.catalog
    }

    /// **强制**触发一轮元数据同步（`POST /sync/runs`，docs/13 §4.12）。
    ///
    /// `run_metadata_sync` 受 `CatalogSync::due()` 的 5 分钟周期门槛约束，
    /// 界面的「立即触发」要绕过它：这里临时把 `interval_ms` 置 0，
    /// 跑完立即恢复原值——`last_round_ms` 已在轮首前移，因此**之后的定时任务
    /// 仍按原周期节流**，`running` 单飞语义（09 §2.3 一轮不重入）完全不动。
    pub fn run_metadata_sync_forced(&mut self, now: u64) -> Result<SyncOutcome, ControlError> {
        eprintln!("[sync] 手动触发（forced：临时把 5 分钟门槛置 0，跑完恢复）");
        let saved = self.catalog.interval_ms;
        self.catalog.interval_ms = 0;
        let out = self.run_metadata_sync(now);
        self.catalog.interval_ms = saved;
        out
    }
}
