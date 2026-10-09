//! 分片 → 进程指派（`docs/09-控制面.md` §5）。
//!
//! * **§5.1 切分片**：`(venue, market, shard_idx)`；标的按 `hash(instrument)` 均匀映射，
//!   `shard_count = 2^ceil(log2(ceil(n / shard_size)))`（取 2 的幂 → 增删只改写自身归属）；
//! * **§5.2 承载约束**：每实例可用容量 = **min(域配置上限, register 自报能力)**；
//!   不满足任一条则该实例不再纳入候选；
//! * **§5.3 分布策略**：默认 **`scatter`（同所打散）**，可配 `aggregate`；
//!   `quota_share` = 份额 / **持有该所的进程数**，在生成 `SubscribePlan` 时算好下发；
//! * **§5.4 指派流程**：候选实例按策略排序 → 逐分片分配 → 生成每实例计划 →
//!   按 `assignment_version` 下发并存快照。

use crate::error::ControlError;
use crate::schema::{BlocklistDoc, CapacityLimits, PlanInstrument, QuotaSpec, Strategy, SubscribePlanDoc};
use ponk_protocol::{
    Channel, ConfigDomain, InstrumentMeta, Market, NodeKind, PlanMode, QuotaShare, ShardPlan,
    SubscribePlan, Venue,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// ─────────────────────────────────────────────────────────────────────────────
// §5.1 稳定哈希与分片数
// ─────────────────────────────────────────────────────────────────────────────

/// FNV-1a 64：进程间可复现的稳定哈希（与事件中心内存 backend 的分片映射同款）。
///
/// 不用 `std::hash::DefaultHasher`——其算法未承诺跨版本稳定。
pub fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `shard_count = 2^ceil(log2(ceil(n / shard_size)))`（09 §5.1），下界 1。
pub fn shard_count(n: usize, shard_size: u64) -> u32 {
    if n == 0 {
        return 1;
    }
    let shard_size = shard_size.max(1) as u128;
    let need = (n as u128).div_ceil(shard_size).max(1);
    let mut p: u128 = 1;
    while p < need {
        p <<= 1;
    }
    p.min(u32::MAX as u128) as u32
}

/// 标的 → 分片序号（`hash(instrument) & (shard_count - 1)`，分片数为 2 的幂故用掩码）。
pub fn shard_index(instrument_id: &str, count: u32) -> u32 {
    if count <= 1 {
        return 0;
    }
    (fnv1a64(instrument_id) & (count as u64 - 1)) as u32
}

// ─────────────────────────────────────────────────────────────────────────────
// §5.2 承载能力
// ─────────────────────────────────────────────────────────────────────────────

/// register 自报能力（10 §5「register 携带承载能力（可装哪些所/多少分片）」）。
///
/// > **规格缺口**：03 §5.3 的 `OpsHeartbeat` 没有承载能力字段，只有开放字段
/// > `counters: BTreeMap<String, i64>`。本 crate 以如下键约定承载（见交付报告）：
/// >
/// > | 键 | 含义 |
/// > |---|---|
/// > | `capacity.shards` | 自报最大分片数 |
/// > | `capacity.instruments` | 自报最大标的数 |
/// > | `capacity.channels` | 自报最大通道数 |
/// > | `capacity.venue.<name>` | 可承载的所，值为 `1` |
/// >
/// > 未上报某项 = 该项以域配置上限为准（min 语义下等价于「自报不设限」）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedCapacity {
    /// 自报最大分片数。
    pub max_shards: Option<u64>,
    /// 自报最大标的数。
    pub max_instruments: Option<u64>,
    /// 自报最大通道数。
    pub max_channels: Option<u64>,
    /// 自报可承载的所；`None` = 未上报（视为可承载任意所）。
    pub venues: Option<BTreeSet<String>>,
}

impl ReportedCapacity {
    /// 自报能力键前缀。
    pub const KEY_PREFIX: &'static str = "capacity.";

    /// 从 `OpsHeartbeat::counters` 解析。
    pub fn from_counters(counters: &BTreeMap<String, i64>) -> Self {
        let mut rc = ReportedCapacity::default();
        let mut venues = BTreeSet::new();
        for (k, v) in counters {
            if let Some(rest) = k.strip_prefix(Self::KEY_PREFIX) {
                match rest {
                    "shards" => rc.max_shards = u64::try_from(*v).ok(),
                    "instruments" => rc.max_instruments = u64::try_from(*v).ok(),
                    "channels" => rc.max_channels = u64::try_from(*v).ok(),
                    _ => {
                        if let Some(venue) = rest.strip_prefix("venue.") {
                            if *v > 0 {
                                venues.insert(venue.to_string());
                            }
                        }
                    }
                }
            }
        }
        if !venues.is_empty() {
            rc.venues = Some(venues);
        }
        rc
    }

    /// 序列化回 `counters`（往返用，测试核对）。
    pub fn to_counters(&self) -> BTreeMap<String, i64> {
        let mut out = BTreeMap::new();
        if let Some(v) = self.max_shards {
            out.insert(format!("{}shards", Self::KEY_PREFIX), v as i64);
        }
        if let Some(v) = self.max_instruments {
            out.insert(format!("{}instruments", Self::KEY_PREFIX), v as i64);
        }
        if let Some(v) = self.max_channels {
            out.insert(format!("{}channels", Self::KEY_PREFIX), v as i64);
        }
        if let Some(vs) = &self.venues {
            for v in vs {
                out.insert(format!("{}venue.{}", Self::KEY_PREFIX, v), 1);
            }
        }
        out
    }

    /// 三项都未上报。
    pub fn is_empty(&self) -> bool {
        self.max_shards.is_none() && self.max_instruments.is_none() && self.max_channels.is_none()
    }
}

/// 指派用的**有效承载上限** = min(域配置上限, register 自报能力)（09 §5.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveCapacity {
    /// 有效最大分片数。
    pub max_shards: u64,
    /// 有效最大标的数。
    pub max_instruments: u64,
    /// 有效最大通道数。
    pub max_channels: u64,
}

impl EffectiveCapacity {
    /// `min(配置上限, 自报能力)`；自报缺项时该项取配置上限。
    pub fn min(limits: &CapacityLimits, reported: &ReportedCapacity) -> Self {
        EffectiveCapacity {
            max_shards: reported
                .max_shards
                .map_or(limits.max_shards_per_process, |r| {
                    r.min(limits.max_shards_per_process)
                }),
            max_instruments: reported
                .max_instruments
                .map_or(limits.max_instruments_per_process, |r| {
                    r.min(limits.max_instruments_per_process)
                }),
            max_channels: reported
                .max_channels
                .map_or(limits.max_channels_per_process, |r| r.min(limits.max_channels_per_process)),
        }
    }
}

/// 指派候选实例（register 进入候选集，09 §5.2）。
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 实例 id。
    pub instance_id: String,
    /// 实例所在机器（`None` = 未上报）。分散策略据此避免同所分片挤在同一台机器（09 §2.3）。
    pub host: Option<String>,
    /// 节点种类（只有采集是订阅计划的外部消费者，09 §2）。
    pub node_kind: NodeKind,
    /// register 自报能力。
    pub reported: ReportedCapacity,
}

impl Candidate {
    /// 有效承载上限。
    pub fn capacity(&self, limits: &CapacityLimits) -> EffectiveCapacity {
        EffectiveCapacity::min(limits, &self.reported)
    }

    /// 是否可承载该所（未上报 `venues` 视为可承载任意所）。
    pub fn holds_venue(&self, venue: &str) -> bool {
        match &self.reported.venues {
            None => true,
            Some(set) => set.contains(venue),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §5.3 分布策略
// ─────────────────────────────────────────────────────────────────────────────

/// 分布策略（09 §5.3）。
pub use crate::schema::Strategy as DistributionStrategy;

// ─────────────────────────────────────────────────────────────────────────────
// 指派输入 / 输出
// ─────────────────────────────────────────────────────────────────────────────

/// 指派输入（域文档 + 活实例清单 = 09 §3.1「确定性推导」的全部输入）。
#[derive(Debug, Clone)]
pub struct AssignRequest {
    /// 候选实例（活的注册实例）。
    pub candidates: Vec<Candidate>,
    /// `SubscribePlan` 域当前文档。
    pub plan_doc: SubscribePlanDoc,
    /// `InstrumentBlocklist` 域当前文档。
    pub blocklist: BlocklistDoc,
    /// `ProcessCapacity` 域当前文档（约束与策略）。
    pub limits: CapacityLimits,
    /// 分布策略（默认 scatter）。
    pub strategy: Strategy,
    /// 下发模式：初次 / 重算用 `Snapshot`；局部变更用 `Diff`（09 §3.1 / 10 §4.2）。
    pub mode: PlanMode,
    /// 每实例本次要使用的 `assignment_version`（每次下发 +1）。
    pub versions: BTreeMap<String, u64>,
    /// **逐实例的 scope 允许集**（`docs/12 §5`，A 方案 = 模型 1）：
    /// key = `instance_id`，value = 该实例可持有的标的 canonical 集合
    /// （`venue:market:native_code`）。
    ///
    /// * **空 map = 不限制**（无节点定义时的兼容路径，行为与改造前一致）；
    /// * 有值的实例，**分片必须整体落在其允许集内**才会发给它；
    /// * 分片在 quote 层分组（scope 最常见的区分维度是计价币），避免跨 scope 混装导致无人可接。
    pub allowed: BTreeMap<String, BTreeSet<String>>,
}

/// 一个实例的指派结果。
#[derive(Debug, Clone)]
pub struct InstanceAssignment {
    /// 下发给该实例的订阅计划。
    pub plan: SubscribePlan,
    /// 该实例在各所上的配额分摊（09 §5.3：份额 / 持有该所的进程数）。
    ///
    /// > **规格缺口**：[`SubscribePlan::quota_share`] 是 `Option<QuotaShare>` 单值，
    /// > 而 09 §3.1 要求「多所同进程时按所各给一份」。本结构保留**完整的逐所分摊**，
    /// > 线上只带其中一份（`quota_share`），完整表随派生物快照入库，见交付报告。
    pub quota_splits: BTreeMap<String, QuotaShare>,
}

/// 指派结果。
#[derive(Debug, Clone, Default)]
pub struct AssignmentResult {
    /// 每实例计划（含未分到分片的候选，仍下发空计划以便 register 有应答）。
    pub plans: BTreeMap<String, InstanceAssignment>,
    /// 未能指派的分片 `(venue, market, shard_idx, 原因)`（容量不足 / 无候选所）。
    pub unassigned: Vec<(String, String, u32, String)>,
    /// 每所**持有该所的进程数**（配额分摊分母，09 §5.3）。
    pub holders: BTreeMap<String, usize>,
}

/// 切分片 + 指派（09 §5.1~§5.4）。
pub fn assign(req: AssignRequest) -> Result<AssignmentResult, ControlError> {
    let shard_size = req.plan_doc.shard_size.max(1);
    let block = index_blocklist(&req.blocklist);

    // ① 白名单 − 黑名单（09 §2.1 / §5.1「ShardPlan.instruments」）
    let mut live: Vec<(String /*venue*/, String /*market*/, PlanInstrument)> = Vec::new();
    for ins in &req.plan_doc.instruments {
        let id = ins.instrument_id();
        let default_channels = req.plan_doc.default_channels.clone();
        let mut channels = if ins.channels.is_empty() {
            default_channels
        } else {
            ins.channels.clone()
        };
        if let Some(entry) = block.get(&id) {
            match entry {
                None => continue, // 全通道屏蔽 → 剔除（09 §3.5 channels 缺省 = 全通道）
                Some(blocked) => {
                    channels.retain(|c| !blocked.contains(c));
                    if channels.is_empty() {
                        continue; // 只剩被屏蔽的通道 → 该标的无可采内容
                    }
                }
            }
        }
        let mut norm = ins.clone();
        norm.channels = channels;
        live.push((ins.venue.clone(), ins.market.clone(), norm));
    }

    // ② 每 (venue, market) 切分片（09 §5.1）
    // A 方案（模型 1）：**分桶服从 scope 归属**。
    //
    // 若先在 (venue, market) 内按哈希切桶、再做"整桶必须落在允许集内"的过滤，
    // 桶里只要混进一个范围外的标的就整桶无人可接 —— 运营者声明了标的却一个也采不到，
    // 且是静默的。因此**先按 owner 归组、再切桶**：
    //   owner = 第一个允许持有该标的的候选实例（顺序确定）；
    //   无人允许 → 落到哨兵 owner，放置阶段自然报 "scope 未覆盖"。
    // owner = "" 表示本次没有 scope（无节点定义的兼容路径），维持原行为。
    let use_scope = !req.allowed.is_empty();
    let owner_of = |key: &str| -> String {
        if !use_scope {
            return String::new();
        }
        for c in &req.candidates {
            match req.allowed.get(&c.instance_id) {
                None => return c.instance_id.clone(),          // 无节点定义 = 不限制
                Some(set) if set.contains(key) => return c.instance_id.clone(),
                Some(_) => continue,                           // 有范围但不含该标的
            }
        }
        "__scope_miss__".to_string()
    };

    let mut groups: BTreeMap<(String, String, String), Vec<PlanInstrument>> = BTreeMap::new();
    for (v, m, ins) in live {
        let owner = if use_scope {
            owner_of(&ins.instrument_id())
        } else {
            String::new()
        };
        groups.entry((v, m, owner)).or_default().push(ins);
    }

    // key = (venue, market, owner)；owner = "" 表示不限制
    let mut shards: BTreeMap<(String, String, String), Vec<ShardPlan>> = BTreeMap::new();
    for ((venue, market, owner), items) in &groups {
        let count = shard_count(items.len(), shard_size);
        let mut buckets: Vec<Vec<InstrumentMeta>> = vec![Vec::new(); count as usize];
        for ins in items {
            let idx = shard_index(&ins.instrument_id(), count) as usize;
            buckets[idx].push(InstrumentMeta {
                native_code: ins.native_code.clone(),
                price_precision: ins.price_precision,
                qty_precision: ins.qty_precision,
                channels: ins.channels.iter().map(|c| Channel::new(c.clone())).collect(),
                enabled: true,
                meta: ins.meta.clone(),
            });
        }
        let mut plans = Vec::new();
        for (i, mut bucket) in buckets.into_iter().enumerate() {
            if bucket.is_empty() {
                continue; // 空分片不产生 ShardPlan
            }
            // 确定性：分片内按 native_code 排序
            bucket.sort_by(|a, b| a.native_code.cmp(&b.native_code));
            plans.push(ShardPlan {
                venue: Venue::new(venue.clone()),
                market: Market::new(market.clone()),
                shard_idx: i as u32,
                instruments: bucket,
            });
        }
        shards.insert((venue.clone(), market.clone(), owner.clone()), plans);
    }

    // ③ 逐实例容量账本
    let mut ctx = AssignCtx::new(&req);

    // ④ 分布策略（09 §5.3）
    match req.strategy {
        Strategy::Scatter => scatter(&mut ctx, &shards),
        Strategy::Aggregate => aggregate(&mut ctx, &shards),
    }

    ctx.finish(&shards)
}

/// 黑名单索引：instrument → 被屏蔽的通道集合；`None` = **全通道屏蔽**（09 §3.5）。
fn index_blocklist(doc: &BlocklistDoc) -> BTreeMap<String, Option<BTreeSet<String>>> {
    let mut out: BTreeMap<String, Option<BTreeSet<String>>> = BTreeMap::new();
    for e in &doc.entries {
        if e.channels.is_empty() {
            // channels 缺省 = 全通道（09 §3.5）；全通道覆盖部分屏蔽
            out.insert(e.instrument.clone(), None);
        } else if !matches!(out.get(&e.instrument), Some(None)) {
            out.entry(e.instrument.clone())
                .or_insert_with(|| Some(BTreeSet::new()));
            if let Some(Some(set)) = out.get_mut(&e.instrument) {
                set.extend(e.channels.iter().cloned());
            }
        }
    }
    out
}

struct InstanceState {
    shards: Vec<ShardPlan>,
    /// (venue, 已占用通道集)
    channels: BTreeMap<String, BTreeSet<String>>,
    instruments: u64,
    capacity: EffectiveCapacity,
}

impl InstanceState {
    fn shard_count(&self) -> u64 {
        self.shards.len() as u64
    }

    fn fits(&self, shard: &ShardPlan) -> bool {
        if self.shard_count() + 1 > self.capacity.max_shards {
            return false;
        }
        let n = shard.instruments.len() as u64;
        if self.instruments + n > self.capacity.max_instruments {
            return false;
        }
        let venue = shard.venue.as_str().to_string();
        let existing = self.channels.get(&venue);
        let mut merged: BTreeSet<String> = existing.cloned().unwrap_or_default();
        for ins in &shard.instruments {
            merged.extend(ins.channels.iter().map(|c| c.as_str().to_string()));
        }
        let mut other_units: u64 = 0;
        for (v, set) in &self.channels {
            if *v != venue {
                other_units += set.len() as u64;
            }
        }
        other_units + merged.len() as u64 <= self.capacity.max_channels
    }

    fn place(&mut self, shard: ShardPlan) {
        let venue = shard.venue.as_str().to_string();
        self.instruments += shard.instruments.len() as u64;
        let set = self.channels.entry(venue).or_default();
        for ins in &shard.instruments {
            set.extend(ins.channels.iter().map(|c| c.as_str().to_string()));
        }
        self.shards.push(shard);
    }
}

struct AssignCtx<'a> {
    req: &'a AssignRequest,
    states: BTreeMap<String, InstanceState>,
    /// 实例 id（保持输入顺序，便于确定性与诊断）。
    order: Vec<String>,
    unassigned: Vec<(String, String, u32, String)>,
}

impl<'a> AssignCtx<'a> {
    fn new(req: &'a AssignRequest) -> Self {
        let mut states = BTreeMap::new();
        for c in &req.candidates {
            states.insert(
                c.instance_id.clone(),
                InstanceState {
                    shards: Vec::new(),
                    channels: BTreeMap::new(),
                    instruments: 0,
                    capacity: c.capacity(&req.limits),
                },
            );
        }
        // 候选按 **host 交错**排序：round-robin 放置时连续几次挑选落在不同机器上，
        // 避免同一交易所的分片全挤在一台机器（09 §2.3）。未上报 host 的排最后。
        let unreported = "~unreported";
        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for c in &req.candidates {
            groups
                .entry(c.host.clone().unwrap_or_else(|| unreported.to_string()))
                .or_default()
                .push(c.instance_id.clone());
        }
        for ids in groups.values_mut() {
            ids.sort();
        }
        let mut order = Vec::new();
        let max_len = groups.values().map(|v| v.len()).max().unwrap_or(0);
        for round in 0..max_len {
            for ids in groups.values() {
                if let Some(id) = ids.get(round) {
                    order.push(id.clone());
                }
            }
        }
        AssignCtx {
            req,
            states,
            order,
            unassigned: Vec::new(),
        }
    }

    /// 该所的候选实例（承载约束 + `holds_venue`，09 §5.2）。

    /// **模型 1 的 scope 判定**：实例的允许集须**整体覆盖**该分片，否则不发给它
    /// （`docs/12 §5` / 09 §2.3）。
    ///
    /// 两种状态语义**必须分开**：
    /// * **无该 key** = 实例没有节点定义 → 不限制（兼容既有行为）；
    /// * **key 存在但集合为空** = 节点声明了范围、解析出 0 条 → **什么都不给**。
    ///   （若把空集当"不限制"，一个写错的 scope 会静默地采全市场。）
    /// 分片归属判定：`owner` 为空 = 本次未启用 scope（不限制）；
    /// 否则只有 owner 本人可以持有。
    fn owner_matches(&self, owner: &str, instance: &str) -> bool {
        owner.is_empty() || owner == instance
    }


    fn eligible(&self, venue: &str) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| {
                self.req
                    .candidates
                    .iter()
                    .any(|c| &c.instance_id == *id && c.holds_venue(venue))
            })
            .cloned()
            .collect()
    }

    fn finish(
        &mut self,
        _shards: &BTreeMap<(String, String, String), Vec<ShardPlan>>,
    ) -> Result<AssignmentResult, ControlError> {
        // 配额分摊分母 = **持有该所的进程数**（09 §5.3）
        let mut holders: BTreeMap<String, usize> = BTreeMap::new();
        for st in self.states.values() {
            let mut seen = BTreeSet::new();
            for s in &st.shards {
                seen.insert(s.venue.as_str().to_string());
            }
            for v in seen {
                *holders.entry(v).or_insert(0) += 1;
            }
        }

        let mut plans = BTreeMap::new();
        for id in &self.order {
            let st = self.states.get_mut(id).expect("state exists");
            st.shards.sort_by(|a, b| {
                (a.venue.as_str(), a.market.as_str(), a.shard_idx)
                    .cmp(&(b.venue.as_str(), b.market.as_str(), b.shard_idx))
            });
            let version = self
                .req
                .versions
                .get(id)
                .copied()
                .unwrap_or(1);

            // 逐所配额分摊（09 §5.3）
            let mut quota_splits: BTreeMap<String, QuotaShare> = BTreeMap::new();
            let held: BTreeSet<String> = st.shards.iter().map(|s| s.venue.as_str().to_string()).collect();
            for venue in &held {
                let Some(spec) = self.req.plan_doc.quota.iter().find(|q| &q.venue == venue) else {
                    continue;
                };
                let n = holders.get(venue).copied().unwrap_or(0).max(1) as f64;
                quota_splits.insert(
                    venue.clone(),
                    split_share(spec, venue, version, n),
                );
            }

            // 线上 `quota_share` 单值（规格缺口：多所分摊无法全部上线）
            let primary = st
                .shards
                .first()
                .map(|s| s.venue.as_str().to_string())
                .or_else(|| quota_splits.keys().next().cloned());
            let quota_share = primary.and_then(|v| quota_splits.get(&v).cloned());

            let plan = SubscribePlan {
                assignment_version: version,
                mode: self.req.mode,
                shards: std::mem::take(&mut st.shards),
                quota_share,
            };
            plans.insert(
                id.clone(),
                InstanceAssignment {
                    plan,
                    quota_splits,
                },
            );
        }

        Ok(AssignmentResult {
            plans,
            unassigned: std::mem::take(&mut self.unassigned),
            holders,
        })
    }
}

/// 份额分摊：`share / 持有该所的进程数`（09 §5.3）。
fn split_share(spec: &QuotaSpec, venue: &str, version: u64, holders: f64) -> QuotaShare {
    QuotaShare {
        venue: Venue::new(venue),
        egress_ip: spec.egress_ip.clone(),
        collect: spec.collect / holders,
        backfill: spec.backfill / holders,
        metadata: spec.metadata / holders,
        version,
    }
}

/// `scatter`：同所打散（默认）——同一所的分片在该所的候选实例间**轮转**，
/// 单进程挂掉时每个所各损失一部分，无任一所全断（09 §5.3）。
fn scatter(
    ctx: &mut AssignCtx,
    shards: &BTreeMap<(String, String, String), Vec<ShardPlan>>,
) {
    for ((venue, market, owner), group) in shards {
        if group.is_empty() {
            continue;
        }
        let eligible = ctx.eligible(venue);
        if eligible.is_empty() {
            for s in group {
                ctx.unassigned.push((
                    venue.clone(),
                    market.clone(),
                    s.shard_idx,
                    "无可承载该所的实例".to_string(),
                ));
            }
            continue;
        }
        let mut cursor = 0usize;
        for shard in group {
            let mut placed = false;
            let mut scope_miss = false;
            for attempt in 0..eligible.len() {
                let idx = (cursor + attempt) % eligible.len();
                let id = &eligible[idx];
                if !ctx.owner_matches(owner, id) {
                    scope_miss = true;
                    continue; // 不是该分片的 owner（docs/12 §5 / 模型 1）
                }
                let st = ctx.states.get_mut(id).expect("state exists");
                if st.fits(shard) {
                    let s = shard.clone();
                    st.place(s);
                    cursor = (idx + 1) % eligible.len();
                    placed = true;
                    break;
                }
            }
            if !placed {
                if scope_miss {
                    ctx.unassigned.push((
                        venue.clone(),
                        market.clone(),
                        shard.shard_idx,
                        "scope 未覆盖：无实例声明的范围包含该分片（docs/12 §5）".to_string(),
                    ));
                    continue;
                }
                let id = &eligible[0];
                let reason = {
                    let st = ctx.states.get(id).expect("state exists");
                    format!(
                        "承载不足：有效上限 分片{} 标的{} 通道{}",
                        st.capacity.max_shards,
                        st.capacity.max_instruments,
                        st.capacity.max_channels
                    )
                };
                ctx.unassigned.push((
                    venue.clone(),
                    market.clone(),
                    shard.shard_idx,
                    reason,
                ));
            }
        }
    }
}

/// `aggregate`：同所聚合——同一所的分片尽量集中到少数进程（09 §5.3）。
fn aggregate(
    ctx: &mut AssignCtx,
    shards: &BTreeMap<(String, String, String), Vec<ShardPlan>>,
) {
    for ((venue, market, owner), group) in shards {
        if group.is_empty() {
            continue;
        }
        let mut eligible = ctx.eligible(venue);
        eligible.sort(); // 聚合策略下顺序稳定即可
        if eligible.is_empty() {
            for s in group {
                ctx.unassigned.push((
                    venue.clone(),
                    market.clone(),
                    s.shard_idx,
                    "无可承载该所的实例".to_string(),
                ));
            }
            continue;
        }
        let mut cur = 0usize;
        for shard in group {
            let mut placed = false;
            let mut scope_miss = false;
            while cur < eligible.len() {
                let id = &eligible[cur];
                if !ctx.owner_matches(owner, id) {
                    scope_miss = true;
                    cur += 1;
                    continue; // 不是该分片的 owner（docs/12 §5 / 模型 1）
                }
                let st = ctx.states.get_mut(id).expect("state exists");
                if st.fits(shard) {
                    let s = shard.clone();
                    st.place(s);
                    placed = true;
                    break;
                }
                cur += 1; // 当前实例装不下 → 前移到下一个（聚合语义）
            }
            if !placed {
                let reason = if scope_miss {
                    "scope 未覆盖：无实例声明的范围包含该分片（docs/12 §5）".to_string()
                } else {
                    "承载不足：所有候选实例已满".to_string()
                };
                ctx.unassigned.push((venue.clone(), market.clone(), shard.shard_idx, reason));
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.1 / 10 §4.3：assignment_version 强序
// ─────────────────────────────────────────────────────────────────────────────

/// 命令定序裁决（10 §4.3：**旧版丢弃、新版整包应用**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDecision {
    /// `version <= applied` → 丢弃（旧版或重复）。
    Dropped,
    /// `version > applied` → **整包应用**（不是状态补丁的增量合并）。
    Applied,
}

/// 每实例一条的 `assignment_version` 门。
#[derive(Debug, Clone)]
pub struct VersionGate {
    applied: u64,
}

impl VersionGate {
    /// 已应用版本（首条之前为 0）。
    pub fn new(applied: u64) -> Self {
        VersionGate { applied }
    }

    /// 当前已应用版本。
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// 裁决一个版本号。
    pub fn accept(&mut self, version: u64) -> OrderDecision {
        if version > self.applied {
            self.applied = version;
            OrderDecision::Applied
        } else {
            OrderDecision::Dropped
        }
    }

    /// 裁决并**整包取出**计划：旧版丢弃（`None`），新版整包应用（原样返回整包）。
    pub fn accept_package(&mut self, plan: SubscribePlan) -> Option<SubscribePlan> {
        match self.accept(plan.assignment_version) {
            OrderDecision::Applied => Some(plan),
            OrderDecision::Dropped => None,
        }
    }
}

impl Default for VersionGate {
    fn default() -> Self {
        VersionGate::new(0)
    }
}

/// 控制面侧：为某实例取下一个 `assignment_version`（每次下发 +1，09 §4.1）。
pub fn next_assignment_version(current: Option<u64>) -> u64 {
    current.map(|v| v + 1).unwrap_or(1)
}

/// 域文档中参与指派的标的数（诊断用）。
pub fn plan_instrument_count(doc: &SubscribePlanDoc) -> usize {
    doc.instruments.len()
}

/// 取域文档里的 `shard_size`（09 §5.1）。
pub fn shard_size_of(doc: &SubscribePlanDoc) -> u64 {
    doc.shard_size.max(1)
}

/// `ConfigDomain` 里与指派相关的三个输入域（09 §8「控制面·指派器」）。
pub const ASSIGNMENT_INPUT_DOMAINS: [ConfigDomain; 3] = [
    ConfigDomain::SubscribePlan,
    ConfigDomain::InstrumentBlocklist,
    ConfigDomain::ProcessCapacity,
];
