//! 内存 backend（测试与本地开发用）。
//!
//! 03 §4.1 的契约层语义在这里**完整落地**，供测试与本地开发依赖：
//!
//! | 语义 | 实现 |
//! |---|---|
//! | 消费组 | 每 `(主题, 分区, 消费组)` 一个游标；同组多会话**共享**游标（竞争消费） |
//! | 分片 | `Envelope::shard` → 稳定哈希 → 分区（`None` → 分区 0，03 §8） |
//! | **无 ack** | `next` 取到消息即把游标推进一格；**没有确认原语**（03 §4.4） |
//! | 尽力有序 | 分区内严格按写入序；跨分区无序（03 §3） |
//! | 发布失败 → DLQ | 写入 `dlq.<topic>`（03 §7.3），调用方仍收到错误 |
//!
//! `Capabilities::measured = true`，能力值由 `tests/memory_semantics.rs` 逐项核对
//! （03 §4.5「能力取值来自实测而非手写标记」）。

use crate::capabilities::Capabilities;
use crate::error::ECError;
use crate::session::{DeadLetterReader, EventCenter, Session, SessionOpts, StartPosition};
use ponk_protocol::{DeadLetter, DeadLetterMeta, Envelope, Group, Topic};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 内存 backend 选项。
#[derive(Debug, Clone)]
pub struct MemoryOptions {
    /// 分区数（03 §4.5 `partitioning`：是否支持分区、可配分区数）。至少为 1。
    pub partitions: usize,
    /// 每分区保留条数；`None` = 不截断（进程内存为界）。
    pub max_len: Option<usize>,
}

impl Default for MemoryOptions {
    fn default() -> Self {
        MemoryOptions {
            partitions: 4,
            max_len: None,
        }
    }
}

/// 内存 backend。**可克隆**：克隆共享同一份消息存储。
#[derive(Clone)]
pub struct MemoryEventCenter {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    cond: Condvar,
    opts: MemoryOptions,
    /// 注入的发布失败（用于确定性地测试 03 §7.3 的 DLQ 路径）。
    failures: Mutex<BTreeSet<String>>,
}

#[derive(Default)]
struct State {
    topics: BTreeMap<String, TopicState>,
    /// `dlq.<origin>` → 死信（03 §7.3）。
    dead: BTreeMap<String, Vec<DeadLetterMeta>>,
}

struct TopicState {
    partitions: Vec<Partition>,
}

struct Partition {
    /// `(序号, 信封)`，序号单调递增。
    entries: VecDeque<(u64, Envelope)>,
    /// 下一个待分配序号。
    next_seq: u64,
    /// 消费组 → 下一个待读序号（**读取即推进**，无 ack）。
    groups: BTreeMap<String, u64>,
}

impl Default for Partition {
    fn default() -> Self {
        Partition {
            entries: VecDeque::new(),
            next_seq: 0,
            groups: BTreeMap::new(),
        }
    }
}

impl MemoryEventCenter {
    /// 用默认选项创建。
    pub fn new() -> Self {
        Self::with_options(MemoryOptions::default())
    }

    /// 用指定选项创建。
    pub fn with_options(opts: MemoryOptions) -> Self {
        let opts = MemoryOptions {
            partitions: opts.partitions.max(1),
            max_len: opts.max_len,
        };
        MemoryEventCenter {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                cond: Condvar::new(),
                opts,
                failures: Mutex::new(BTreeSet::new()),
            }),
        }
    }

    /// 注入某主题的发布失败（测试 03 §7.3 DLQ 用；真实 backend 的失败来自故障本身）。
    pub fn inject_publish_failure(&self, topic: &Topic) {
        if let Ok(mut f) = self.inner.failures.lock() {
            f.insert(topic.as_str().to_string());
        }
    }

    /// 撤销注入的发布失败。
    pub fn clear_publish_failure(&self, topic: &Topic) {
        if let Ok(mut f) = self.inner.failures.lock() {
            f.remove(topic.as_str());
        }
    }

    /// 某主题当前保留的总条数（跨分区）。
    pub fn retained_len(&self, topic: &Topic) -> usize {
        let state = match self.inner.state.lock() {
            Ok(s) => s,
            Err(_) => return 0,
        };
        state
            .topics
            .get(topic.as_str())
            .map(|t| t.partitions.iter().map(|p| p.entries.len()).sum())
            .unwrap_or(0)
    }

    /// 某主题的分区数。
    pub fn partition_count(&self, topic: &Topic) -> usize {
        let state = match self.inner.state.lock() {
            Ok(s) => s,
            Err(_) => return 0,
        };
        state
            .topics
            .get(topic.as_str())
            .map(|t| t.partitions.len())
            .unwrap_or(0)
    }
}

impl Default for MemoryEventCenter {
    fn default() -> Self {
        Self::new()
    }
}

/// 校验主题：必须落在 03 §8 / §5.2 的注册主题族内（避免私有主题）。
pub(crate) fn validate_topic(topic: &Topic) -> Result<(), ECError> {
    if topic.as_str().is_empty() {
        return Err(ECError::InvalidTopic("空主题".to_string()));
    }
    if topic.family_prefix().is_none() {
        return Err(ECError::InvalidTopic(format!(
            "`{topic}` 不属于 03 §8 的任何主题族（md./signal./factor./alarm./ops./ctl./dlq.）"
        )));
    }
    Ok(())
}

/// 稳定的分片 → 分区映射（FNV-1a 64，进程间可复现）。
fn partition_of(shard: Option<&String>, partitions: usize) -> usize {
    let Some(s) = shard else { return 0 }; // 无分片键 → 默认分区（03 §8）
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % partitions as u64) as usize
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Inner {
    fn try_publish(&self, topic: &Topic, envelope: &Envelope) -> Result<(), ECError> {
        validate_topic(topic)?;
        envelope.validate()?;

        if let Ok(f) = self.failures.lock() {
            if f.contains(topic.as_str()) {
                return Err(ECError::Publish(format!(
                    "injected failure on topic `{topic}`"
                )));
            }
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| ECError::Backend("memory state poisoned".into()))?;

        let n = self.opts.partitions;
        let idx = partition_of(envelope.shard.as_ref(), n);
        let ts = state
            .topics
            .entry(topic.as_str().to_string())
            .or_insert_with(|| TopicState {
                partitions: (0..n).map(|_| Partition::default()).collect(),
            });

        let part = &mut ts.partitions[idx];
        let seq = part.next_seq;
        part.next_seq += 1;
        part.entries.push_back((seq, envelope.clone()));

        if let Some(max) = self.opts.max_len {
            while part.entries.len() > max {
                part.entries.pop_front();
            }
        }

        drop(state);
        self.cond.notify_all();
        Ok(())
    }

    fn record_dead_letter(&self, topic: &Topic, envelope: &Envelope, error: &ECError) {
        let meta = DeadLetterMeta {
            origin_topic: topic.clone(),
            error: error.to_string(),
            attempts: 1,
            failed_ts: now_ms(),
            body: Box::new(envelope.clone()),
        };
        if let Ok(mut state) = self.state.lock() {
            state
                .dead
                .entry(topic.to_dead_letter().into_string())
                .or_default()
                .push(meta);
        }
        self.cond.notify_all();
    }

    fn open_session(
        self: &Arc<Self>,
        topic: &Topic,
        group: &Group,
        opts: &SessionOpts,
    ) -> Result<MemorySession, ECError> {
        validate_topic(topic)?;
        if topic.is_dead_letter() {
            return Err(ECError::Unsupported(format!(
                "死信主题 `{topic}` 不经 Session 读取：用 DeadLetterReader（03 §7.4 死信只堆积、由运维 CLI 查看）"
            )));
        }
        if group.as_str().is_empty() {
            return Err(ECError::InvalidGroup("空消费组".to_string()));
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| ECError::Backend("memory state poisoned".into()))?;

        let ts = state
            .topics
            .entry(topic.as_str().to_string())
            .or_insert_with(|| TopicState {
                partitions: (0..self.opts.partitions).map(|_| Partition::default()).collect(),
            });

        for part in ts.partitions.iter_mut() {
            if part.groups.contains_key(group.as_str()) {
                continue; // 已存在的消费组保持位点
            }
            let cursor = match opts.start {
                StartPosition::Earliest => part
                    .entries
                    .front()
                    .map(|(s, _)| *s)
                    .unwrap_or(part.next_seq),
                StartPosition::Latest => part.next_seq,
                StartPosition::Offset(n) => {
                    let lo = part.entries.front().map(|(s, _)| *s).unwrap_or(part.next_seq);
                    let hi = part.next_seq;
                    if hi == 0 {
                        0
                    } else {
                        n.clamp(lo, hi)
                    }
                }
            };
            part.groups.insert(group.as_str().to_string(), cursor);
        }

        drop(state);
        Ok(MemorySession {
            center: MemoryEventCenter {
                inner: Arc::clone(self),
            },
            topic: topic.clone(),
            group: group.clone(),
            rr: 0,
            closed: false,
        })
    }

    /// 读取并**推进位点**（无 ack）。
    fn take_next(
        state: &mut State,
        topic: &str,
        group: &str,
        rr: &mut usize,
    ) -> Option<Envelope> {
        let ts = state.topics.get_mut(topic)?;
        let n = ts.partitions.len();
        if n == 0 {
            return None;
        }
        for i in 0..n {
            let p = (*rr + i) % n;
            let part = &mut ts.partitions[p];
            let cursor = part.groups.get(group).copied()?;
            let hit = part
                .entries
                .iter()
                .find(|(s, _)| *s >= cursor)
                .map(|(s, e)| (*s, e.clone()));
            if let Some((seq, env)) = hit {
                // 读取即推进消费位点（03 §4.4）
                part.groups.insert(group.to_string(), seq + 1);
                *rr = (p + 1) % n;
                return Some(env);
            }
        }
        None
    }
}

impl EventCenter for MemoryEventCenter {
    fn publish(&self, topic: &Topic, envelope: &Envelope) -> Result<(), ECError> {
        match self.inner.try_publish(topic, envelope) {
            Ok(()) => Ok(()),
            Err(e) => {
                // 03 §7.3：发布失败的事件写入 dlq.<topic>，携带 DeadLetterMeta
                self.inner.record_dead_letter(topic, envelope, &e);
                Err(e)
            }
        }
    }

    fn open(
        &self,
        topic: &Topic,
        group: &Group,
        opts: SessionOpts,
    ) -> Result<Box<dyn Session>, ECError> {
        let session = self.inner.open_session(topic, group, &opts)?;
        Ok(Box::new(session))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::memory(self.inner.opts.partitions, self.inner.opts.max_len)
    }
}

impl DeadLetterReader for MemoryEventCenter {
    fn dead_letters(&self, topic: &Topic, limit: usize) -> Result<Vec<DeadLetter>, ECError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let key = topic.to_dead_letter().into_string();
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| ECError::Backend("memory state poisoned".into()))?;
        let all = state.dead.get(&key).cloned().unwrap_or_default();
        // 取**最早**的 limit 条：死信按写入顺序堆积，运维排查要看的是最早失败的那批。
        Ok(all.into_iter().take(limit).collect())
    }
}

struct MemorySession {
    center: MemoryEventCenter,
    topic: Topic,
    group: Group,
    /// 分区轮转起点（让同一消费组的多个消费者尽量分摊到不同分区）。
    rr: usize,
    closed: bool,
}

impl Session for MemorySession {
    fn next(&mut self, timeout: Duration) -> Result<Option<Envelope>, ECError> {
        if self.closed {
            return Err(ECError::Closed);
        }
        let deadline = Instant::now() + timeout;
        let inner = &self.center.inner;

        let mut state = inner
            .state
            .lock()
            .map_err(|_| ECError::Backend("memory state poisoned".into()))?;

        loop {
            if self.closed {
                return Err(ECError::Closed);
            }
            if let Some(env) =
                Inner::take_next(&mut state, self.topic.as_str(), self.group.as_str(), &mut self.rr)
            {
                return Ok(Some(env));
            }
            if timeout.is_zero() {
                return Ok(None);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let (guard, _) = inner
                .cond
                .wait_timeout(state, deadline - now)
                .map_err(|_| ECError::Backend("memory state poisoned".into()))?;
            state = guard;
        }
    }

    fn close(&mut self) -> Result<(), ECError> {
        self.closed = true;
        Ok(())
    }
}

impl Drop for MemorySession {
    fn drop(&mut self) {
        self.closed = true;
    }
}
