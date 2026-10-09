//! 控制信道消费的状态面（`docs/09-控制面.md` §6）：register / heartbeat / shutdown、
//! **兜底快照键**（§6.3）、失联判定（§6.4）、收敛比对（§4.4）。
//!
//! > 快照键存放于**控制面自身的状态存储**（与配置存储同库）：事件中心明确不提供
//! > K/V 查找（03 §3），快照键不占总线能力；06 的观测面直接订阅 `ops.*`，不读本键。

use crate::assign::ReportedCapacity;
use crate::error::ControlError;
use crate::validate::consumed_domains;
use crate::store::ConfigStore;
use ponk_protocol::{ConfigDomain, NodeKind, OpsHeartbeat, Status};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// 实例状态（控制面视角；与 `OpsHeartbeat::status` 的映射见 [`SnapshotKey::from_heartbeat`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstStatus {
    /// 已 register，尚未见到心跳。
    Registered,
    /// 运行中（见到 `Running` 心跳）。
    Running,
    /// 正常下线（收到 `shutdown`，09 §6.2）。
    Stopped,
    /// 判死（`shutdown` 缺失 + 心跳断 3×5s，10 §5 / 09 §6.4）。
    Dead,
}

/// 兜底快照键（09 §6.3）：每实例一条。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotKey {
    /// 实例 id。
    pub instance_id: String,
    /// 节点种类。
    pub node_kind: NodeKind,
    /// 实例所在机器（register 上报；09 §2.3 据此分散分片）。重登/迁移时随心跳刷新。
    #[serde(default)]
    pub host: Option<String>,
    /// 状态。
    pub status: InstStatus,
    /// register 自报承载能力（10 §5）。
    #[serde(default)]
    pub reported: ReportedCapacity,
    /// 最近一次**下发**给它的 `assignment_version`。
    #[serde(default)]
    pub last_assignment_version: Option<u64>,
    /// 心跳上报的 `applied_version`（采集 = `assignment_version`，10 §4.3）。
    #[serde(default)]
    pub applied_version: Option<u64>,
    /// 逐域收敛判据 `cfg.<domain>`（09 §4.4）。
    #[serde(default)]
    pub counters: BTreeMap<String, i64>,
    /// 启动时间（ms）。
    pub started_at: u64,
    /// 最近一次见到该实例的时间（ms）。
    pub last_seen_ts: u64,
    /// 首次「不收敛」的时刻（ms）；已收敛为 `None`（09 §4.4 超时重推的计时基准）。
    #[serde(default)]
    pub unconverged_since: Option<u64>,
    /// 已按 09 §4.4 重推过的时刻（ms）；同一超时窗口内只重推一次。
    #[serde(default)]
    pub last_repush_ts: Option<u64>,
    /// 累计失联次数（诊断）。
    #[serde(default)]
    pub lost_count: u64,
}

impl SnapshotKey {
    /// 由 register / 心跳构造（不覆盖控制面自身的计时字段）。
    pub fn from_heartbeat(hb: &OpsHeartbeat, now: u64) -> Self {
        SnapshotKey {
            instance_id: hb.instance_id.clone(),
            node_kind: hb.node_kind,
            host: hb.host.clone(),
            status: match hb.status {
                Status::Startup => InstStatus::Registered,
                Status::Running => InstStatus::Running,
                Status::Shutdown => InstStatus::Stopped,
            },
            reported: ReportedCapacity::from_counters(&hb.counters),
            last_assignment_version: None,
            applied_version: hb.applied_version,
            counters: hb.counters.clone(),
            started_at: hb.started_at,
            last_seen_ts: now,
            unconverged_since: None,
            last_repush_ts: None,
            lost_count: 0,
        }
    }

    /// 心跳里的 `cfg.<domain>` 值。
    pub fn domain_applied(&self, domain: ConfigDomain) -> Option<i64> {
        self.counters.get(&domain.counter_key()).copied()
    }
}

/// 心跳与收敛的运行参数（09 §4.4：「心跳间隔等为控制面自身运行参数，
/// 存于控制面内部，不构成对外配置域」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivenessConfig {
    /// 心跳周期（10 §5：5s）。
    pub heartbeat_interval_ms: u64,
    /// 失联阈值（10 §5：3×5s）。
    pub miss_threshold: u32,
    /// 不收敛超时（09 §4.4：30s = 6 个心跳周期）。
    pub converge_timeout_ms: u64,
}

impl Default for LivenessConfig {
    fn default() -> Self {
        LivenessConfig {
            heartbeat_interval_ms: 5_000,
            miss_threshold: 3,
            converge_timeout_ms: 30_000,
        }
    }
}

impl LivenessConfig {
    /// 判死时间窗：`miss_threshold × heartbeat_interval`（10 §5 = 15_000ms）。
    pub fn lost_after_ms(&self) -> u64 {
        self.heartbeat_interval_ms.saturating_mul(self.miss_threshold as u64)
    }
}

/// 收敛比对结果（09 §4.4）。
#[derive(Debug, Clone, Default)]
pub struct Convergence {
    /// 已收敛实例数。
    pub converged: usize,
    /// 未收敛实例（超过 30s 仍未收敛者计入 `ops.control` 不收敛计数器）。
    pub not_converged: Vec<NotConverged>,
    /// 运行中实例数。
    pub running: usize,
    /// 正常下线实例数。
    pub stopped: usize,
    /// 判死实例数。
    pub dead: usize,
}

impl Convergence {
    /// 超过 09 §4.4 超时（默认 30s）的未收敛实例数——写入 `ops.control` 计数器。
    pub fn timed_out(&self, timeout_ms: u64) -> usize {
        self.not_converged
            .iter()
            .filter(|n| n.held_ms >= timeout_ms)
            .count()
    }
}

/// 一条未收敛记录。
#[derive(Debug, Clone)]
pub struct NotConverged {
    /// 实例 id。
    pub instance_id: String,
    /// 不匹配的域（采集为 `None`：它比的是 `applied_version == assignment_version`）。
    pub domain: Option<ConfigDomain>,
    /// 期望值（当前版本 / 最新 assignment_version）。
    pub expected: i64,
    /// 实际值（心跳上报；未上报视为 0）。
    pub actual: i64,
    /// 已持续多久（ms）。
    pub held_ms: u64,
}

/// 快照键注册表（09 §6.3）。
pub struct OpsRegistry {
    keys: BTreeMap<String, SnapshotKey>,
    liveness: LivenessConfig,
    persist: Persist,
}

/// 快照键的持久化后端（`docs/12 §3.6 node_runtime`）。
///
/// 文件树实现保留用于迁移期影子比对与历史兼容（`docs/12 §6`）；**默认 SQLite**。
#[derive(Clone)]
pub enum Persist {
    /// 不持久化（纯内存，仅测试）。
    None,
    /// 文件树：`<root>/state/instances/<id>.json`。
    Files(PathBuf),
    /// SQLite：`node_runtime` 表。
    Sqlite(std::sync::Arc<crate::sqlite::SqliteStore>),
}

impl OpsRegistry {
    /// `root = Some(dir)` 时启用持久化（控制面重启后恢复，09 §6.3）。
    pub fn new(root: Option<PathBuf>, liveness: LivenessConfig) -> Self {
        OpsRegistry {
            keys: BTreeMap::new(),
            liveness,
            persist: match root {
                Some(r) => Persist::Files(r),
                None => Persist::None,
            },
        }
    }

    /// **默认后端**：SQLite 持久化（`docs/12 §3.6 node_runtime`）。
    pub fn new_with_store(
        store: std::sync::Arc<crate::sqlite::SqliteStore>,
        liveness: LivenessConfig,
    ) -> Self {
        OpsRegistry {
            keys: BTreeMap::new(),
            liveness,
            persist: Persist::Sqlite(store),
        }
    }

    /// 心跳 / 收敛参数。
    pub fn liveness(&self) -> &LivenessConfig {
        &self.liveness
    }

    fn state_dir(&self) -> Option<PathBuf> {
        match &self.persist {
            Persist::Files(root) => Some(root.join("state").join("instances")),
            _ => None,
        }
    }

    fn key_path(&self, instance: &str) -> Option<PathBuf> {
        self.state_dir().map(|d| d.join(format!("{instance}.json")))
    }

    /// 从快照键恢复「每实例最新状态」（09 §6.3 恢复场景：控制面重启 / 消费组重建）。
    pub fn restore(&mut self) -> Result<usize, ControlError> {
        if let Persist::Sqlite(store) = &self.persist {
            let keys = store.load_keys()?;
            let n = keys.len();
            for k in keys {
                self.keys.insert(k.instance_id.clone(), k);
            }
            return Ok(n);
        }
        let Some(dir) = self.state_dir() else {
            return Ok(0);
        };
        if !dir.exists() {
            return Ok(0);
        }
        let mut n = 0;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            let key: SnapshotKey = serde_json::from_slice(&bytes)?;
            self.keys.insert(key.instance_id.clone(), key);
            n += 1;
        }
        Ok(n)
    }

    fn persist(&self, key: &SnapshotKey) -> Result<(), ControlError> {
        if let Persist::Sqlite(store) = &self.persist {
            return store.save_key(key);
        }
        let Some(path) = self.key_path(&key.instance_id) else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(key)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// register：记录能力 → 写快照键（09 §6.2）。
    pub fn on_register(&mut self, hb: &OpsHeartbeat, now: u64) -> Result<&SnapshotKey, ControlError> {
        let mut key = SnapshotKey::from_heartbeat(hb, now);
        // 重新注册（含 Dead 判死后重启）：保留单调版本等历史字段，且 **必须覆盖已存在记录**，
        // 否则 entry().or_insert() 对已存在的 Dead 键不更新 → 状态停在 Dead，
        // recompute 的候选过滤（Registered|Running）把它排除 → 重启实例收不到计划。
        if let Some(prev) = self.keys.get(&hb.instance_id) {
            key.last_assignment_version = prev.last_assignment_version;
            key.lost_count = prev.lost_count;
            key.last_repush_ts = prev.last_repush_ts;
        }
        key.status = InstStatus::Registered;
        self.persist(&key)?;
        self.keys.insert(key.instance_id.clone(), key);
        Ok(self.keys.get(&hb.instance_id).expect("just inserted"))
    }

    /// heartbeat：更新快照键（状态、`applied_version`、`counters`、`last_seen_ts`）。
    pub fn on_heartbeat(&mut self, hb: &OpsHeartbeat, now: u64) -> Result<&SnapshotKey, ControlError> {
        let entry = self.keys.entry(hb.instance_id.clone()).or_insert_with(|| {
            SnapshotKey::from_heartbeat(hb, now)
        });
        // 已判死 / 已下线的实例收到心跳 → 复活（09 §6.4：失联只标记状态，可被后续心跳恢复）
        if entry.status != InstStatus::Running {
            entry.status = InstStatus::Running;
        }
        entry.node_kind = hb.node_kind;
        if hb.host.is_some() {
            entry.host = hb.host.clone(); // 实例迁移到新机器：刷新映射（09 §2.3）
        }
        entry.started_at = hb.started_at;
        entry.last_seen_ts = now;
        entry.applied_version = hb.applied_version;
        entry.counters = hb.counters.clone();
        let reported = ReportedCapacity::from_counters(&hb.counters);
        if !reported.is_empty() || reported.venues.is_some() {
            entry.reported = reported;
        }
        let key = self.keys.get(&hb.instance_id).expect("just inserted").clone();
        self.persist(&key)?;
        Ok(self.keys.get(&hb.instance_id).expect("just inserted"))
    }

    /// shutdown：标记正常下线（09 §6.2）。
    pub fn on_shutdown(&mut self, hb: &OpsHeartbeat, now: u64) -> Result<&SnapshotKey, ControlError> {
        let entry = self.keys.entry(hb.instance_id.clone()).or_insert_with(|| {
            SnapshotKey::from_heartbeat(hb, now)
        });
        entry.status = InstStatus::Stopped;
        entry.last_seen_ts = now;
        entry.applied_version = hb.applied_version;
        entry.counters = hb.counters.clone();
        let key = entry.clone();
        self.persist(&key)?;
        Ok(self.keys.get(&hb.instance_id).expect("just inserted"))
    }

    /// 记录控制面下发的 `assignment_version`（09 §6.3 内容项）。
    pub fn set_assignment(&mut self, instance: &str, version: u64) -> Result<(), ControlError> {
        let Some(key) = self.keys.get_mut(instance) else {
            return Ok(());
        };
        key.last_assignment_version = Some(version);
        let key = key.clone();
        self.persist(&key)
    }

    /// 失联判定（09 §6.4 / 10 §5：`shutdown` 缺失 + 心跳断 3×5s → 判死）。
    ///
    /// 返回本次**新判死**的实例 id。控制面侧动作限定为：
    /// 快照键 `status = Dead`、关闭该实例主题会话、失联计数写入 `ops.control`；
    /// **不触发任何自动指派变更**（09 §7 已排除再平衡）。
    pub fn sweep(&mut self, now: u64) -> Result<Vec<String>, ControlError> {
        let window = self.liveness.lost_after_ms();
        let mut newly = Vec::new();
        for key in self.keys.values_mut() {
            if !matches!(key.status, InstStatus::Registered | InstStatus::Running) {
                continue;
            }
            if now.saturating_sub(key.last_seen_ts) >= window {
                key.status = InstStatus::Dead;
                key.lost_count += 1;
                newly.push(key.instance_id.clone());
            }
        }
        for id in &newly {
            if let Some(key) = self.keys.get(id) {
                let key = key.clone();
                self.persist(&key)?;
            }
        }
        Ok(newly)
    }

    /// 取一个实例的快照键。
    pub fn get(&self, instance: &str) -> Option<&SnapshotKey> {
        self.keys.get(instance)
    }

    /// 全部快照键（按 id 排序）。
    pub fn iter(&self) -> impl Iterator<Item = &SnapshotKey> {
        self.keys.values()
    }

    /// 实例数。
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// 收敛比对（09 §4.4）：
    ///
    /// * **采集**：`applied_version == 最新 assignment_version`；
    /// * **05 / 06 / 07**：`counters["cfg.<domain>"] == 该域当前版本`（对每个消费域）；
    /// * **控制面**：它是版本来源，不参与比对。
    pub fn convergence(&self, store: &dyn ConfigStore, now: u64) -> Convergence {
        let mut out = Convergence::default();
        let mut versions = BTreeMap::new();
        for d in ConfigDomain::ALL {
            versions.insert(d, store.current_version(d).ok().flatten().unwrap_or(0));
        }

        for key in self.keys.values() {
            match key.status {
                InstStatus::Stopped => {
                    out.stopped += 1;
                    continue;
                }
                InstStatus::Dead => {
                    out.dead += 1;
                    continue;
                }
                InstStatus::Registered | InstStatus::Running => {
                    out.running += 1;
                }
            }

            let mut bad: Option<NotConverged> = None;

            if key.node_kind == NodeKind::Collector {
                // 采集只有一个强序域（09 §4.4）
                let expected = key.last_assignment_version.unwrap_or(0) as i64;
                let actual = key.applied_version.unwrap_or(0) as i64;
                if key.last_assignment_version.is_some() && actual != expected {
                    bad = Some(NotConverged {
                        instance_id: key.instance_id.clone(),
                        domain: None,
                        expected,
                        actual,
                        held_ms: 0,
                    });
                }
            } else {
                for d in consumed_domains(key.node_kind) {
                    let expected = versions.get(&d).copied().unwrap_or(0) as i64;
                    let actual = key.domain_applied(d).unwrap_or(0);
                    if actual != expected {
                        bad = Some(NotConverged {
                            instance_id: key.instance_id.clone(),
                            domain: Some(d),
                            expected,
                            actual,
                            held_ms: 0,
                        });
                        break;
                    }
                }
            }

            match bad {
                Some(mut n) => {
                    n.held_ms = match key.unconverged_since {
                        Some(since) => now.saturating_sub(since),
                        None => 0,
                    };
                    out.not_converged.push(n);
                }
                None => out.converged += 1,
            }
        }
        out
    }

    /// 记录 / 清除「不收敛起始时刻」（超时重推的计时基准，09 §4.4）。
    pub fn mark_convergence(&mut self, instance: &str, converged: bool, now: u64) -> Result<(), ControlError> {
        let Some(key) = self.keys.get_mut(instance) else {
            return Ok(());
        };
        if converged {
            key.unconverged_since = None;
        } else if key.unconverged_since.is_none() {
            key.unconverged_since = Some(now);
        }
        let key = key.clone();
        self.persist(&key)
    }

    /// 记录一次重推（同一超时窗口内只推一次）。
    pub fn mark_repush(&mut self, instance: &str, now: u64) -> Result<(), ControlError> {
        let Some(key) = self.keys.get_mut(instance) else {
            return Ok(());
        };
        key.last_repush_ts = Some(now);
        let key = key.clone();
        self.persist(&key)
    }

    /// 是否已到重推时机（09 §4.4：不收敛超时默认 30s → 重推 ①）。
    pub fn repush_due(&self, instance: &str, now: u64) -> bool {
        let Some(key) = self.keys.get(instance) else {
            return false;
        };
        let Some(since) = key.unconverged_since else {
            return false;
        };
        if now.saturating_sub(since) < self.liveness.converge_timeout_ms {
            return false;
        }
        match key.last_repush_ts {
            None => true,
            Some(ts) => now.saturating_sub(ts) >= self.liveness.converge_timeout_ms,
        }
    }
}
