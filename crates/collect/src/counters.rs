//! 在线计数器（02 §4：「进程内只留**在线计数器**（发送积压、降级计数），不是缓存」）。
//!
//! 这是采集进程**唯一**的状态：没有 per-symbol 最后值槽、没有 ring、没有「最近 N 条」。
//! 心跳（10 §5）把 [`Counters::snapshot`] 塞进 `OpsHeartbeat::counters`，
//! 06 据此观测积压与降级（06 §5）。
//!
//! **不报静默**：断流/静默的判定由 06 直接观测 `md.*` 流得出（02 §7.3、10 §8），
//! 采集侧只上报**连接级**重连计数（`reconnects` / `silence_reconnects`）。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};

/// 采集进程在线计数器。`Clone` 共享同一组原子量。
#[derive(Debug, Default)]
pub struct Counters {
    // ── 数据信道（10 §3）─────────────────────────────────────────────────
    /// 成功发布到事件中心的行情事件数。
    pub published: AtomicI64,
    /// 发布失败（重试耗尽）数。
    pub publish_failed: AtomicI64,
    /// 发送队列当前深度（**积压**，06 观测 ops.lag）。
    pub send_backlog: AtomicI64,
    /// 队列满被丢弃的事件数（10 §3：**不做背压**，满即丢并计数）。
    pub dropped_backlog: AtomicI64,
    /// 收到的 WS 帧数（含被过滤掉的）。
    pub ws_events: AtomicI64,
    /// 因白名单外 / 通道不匹配 / 非有限值而未产出的样本数。
    pub dropped_samples: AtomicI64,

    // ── 控制信道 / 连接（10 §5、02 §6）──────────────────────────────────
    /// `connect()` 失败次数。
    pub connect_errors: AtomicI64,
    /// `subscribe()` 失败次数。
    pub subscribe_errors: AtomicI64,
    /// 重连次数（进入退避即计一次）。
    pub reconnects: AtomicI64,
    /// **连接级**静默超时触发的重连次数（不是通道静默告警，02 §7.3）。
    pub silence_reconnects: AtomicI64,
    /// 订阅集重放次数（02 §6 步骤 2）。
    pub subscribe_replays: AtomicI64,
    /// 交易所异步拒绝订阅（`StreamEvent::SubscriptionFailed`）的次数。
    pub subscribe_rejected: AtomicI64,
    /// WS 流报错次数。
    pub stream_errors: AtomicI64,
    /// WS 流关闭次数。
    pub stream_closed: AtomicI64,

    // ── REST 降级（02 §6）───────────────────────────────────────────────
    /// 降级期间成功拉取并发布的事件数。
    pub degraded_rest: AtomicI64,
    /// 通道不允许降级却处于断线态的轮次（如 `trade` 不降级，02 §6）。
    pub degrade_denied: AtomicI64,
    /// 降级拉取失败次数。
    pub degrade_errors: AtomicI64,
    /// 降级期间被「不回补」边界过滤掉的 K 线数（02 §6：断档不补）。
    pub degrade_backfill_blocked: AtomicI64,

    // ── 心跳 / 配额 ──────────────────────────────────────────────────────
    /// 已发布的心跳数（含 Startup / Running / Shutdown）。
    pub heartbeats: AtomicI64,
    /// 因令牌桶空而等待过的次数。
    pub quota_waits: AtomicI64,
    /// 令牌桶耗尽直接失败的次数。
    pub quota_denied: AtomicI64,

    // ── 计划应用（10 §4.3）───────────────────────────────────────────────
    /// 成功应用的订阅计划数（版本更新）。
    pub plans_applied: AtomicI64,
    /// 被版本强序门丢弃的计划数（旧版 / 重复投递）。
    pub plans_ignored: AtomicI64,

    // ── 静态上报项（register 承载能力，10 §5）─────────────────────────────
    /// 不随运行变化的常量上报项，原样并入 [`Counters::snapshot`]。
    ///
    /// 用于 10 §5 的「register 上报承载能力」：控制面 [`ReportedCapacity`]
    /// 约定键前缀 `capacity.`（`capacity.shards` / `capacity.instruments` /
    /// `capacity.channels` / `capacity.venue.<name>`）。
    /// 放这里而不是行情缓存——它是**配置常量**，不是采集状态（02 §4）。
    ///
    /// [`ReportedCapacity`]: ../../../control/src/assign.rs
    pub extra: std::sync::Mutex<BTreeMap<String, i64>>,
}

impl Counters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc(counter: &AtomicI64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// 取一份快照（`OpsHeartbeat::counters`，`BTreeMap<String, i64>`）。
    ///
    /// 键名即 06 侧观测字段名，保持稳定；新增键不影响既有告警规则。
    /// 设置一个静态上报项（承载能力等；见 [`Counters::extra`]）。
    pub fn set_extra(&self, key: impl Into<String>, value: i64) {
        self.extra
            .lock()
            .expect("counters.extra poisoned")
            .insert(key.into(), value);
    }

    pub fn snapshot(&self) -> BTreeMap<String, i64> {
        let mut m = BTreeMap::new();
        m.insert("published".into(), self.published.load(Ordering::Relaxed));
        m.insert("publish_failed".into(), self.publish_failed.load(Ordering::Relaxed));
        m.insert("send_backlog".into(), self.send_backlog.load(Ordering::Relaxed));
        m.insert("dropped_backlog".into(), self.dropped_backlog.load(Ordering::Relaxed));
        m.insert("ws_events".into(), self.ws_events.load(Ordering::Relaxed));
        m.insert("dropped_samples".into(), self.dropped_samples.load(Ordering::Relaxed));
        m.insert("connect_errors".into(), self.connect_errors.load(Ordering::Relaxed));
        m.insert("subscribe_errors".into(), self.subscribe_errors.load(Ordering::Relaxed));
        m.insert("reconnects".into(), self.reconnects.load(Ordering::Relaxed));
        m.insert("silence_reconnects".into(), self.silence_reconnects.load(Ordering::Relaxed));
        m.insert("subscribe_replays".into(), self.subscribe_replays.load(Ordering::Relaxed));
        m.insert("subscribe_rejected".into(), self.subscribe_rejected.load(Ordering::Relaxed));
        m.insert("stream_errors".into(), self.stream_errors.load(Ordering::Relaxed));
        m.insert("stream_closed".into(), self.stream_closed.load(Ordering::Relaxed));
        m.insert("degraded_rest".into(), self.degraded_rest.load(Ordering::Relaxed));
        m.insert("degrade_denied".into(), self.degrade_denied.load(Ordering::Relaxed));
        m.insert("degrade_errors".into(), self.degrade_errors.load(Ordering::Relaxed));
        m.insert(
            "degrade_backfill_blocked".into(),
            self.degrade_backfill_blocked.load(Ordering::Relaxed),
        );
        m.insert("heartbeats".into(), self.heartbeats.load(Ordering::Relaxed));
        m.insert("quota_waits".into(), self.quota_waits.load(Ordering::Relaxed));
        m.insert("quota_denied".into(), self.quota_denied.load(Ordering::Relaxed));
        m.insert("plans_applied".into(), self.plans_applied.load(Ordering::Relaxed));
        m.insert("plans_ignored".into(), self.plans_ignored.load(Ordering::Relaxed));
        // 静态上报项（承载能力，10 §5）：只读，不覆盖动态计数的同名键。
        for (k, v) in self.extra.lock().expect("counters.extra poisoned").iter() {
            m.entry(k.clone()).or_insert(*v);
        }
        m
    }

    pub fn published(&self) -> i64 {
        self.published.load(Ordering::Relaxed)
    }
    pub fn send_backlog(&self) -> i64 {
        self.send_backlog.load(Ordering::Relaxed)
    }
    pub fn reconnects(&self) -> i64 {
        self.reconnects.load(Ordering::Relaxed)
    }
    pub fn degraded_rest(&self) -> i64 {
        self.degraded_rest.load(Ordering::Relaxed)
    }
    pub fn degrade_denied(&self) -> i64 {
        self.degrade_denied.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn snapshot_is_a_btree_with_stable_keys() {
        let c = Counters::new();
        let snap = c.snapshot();
        assert!(snap.contains_key("send_backlog"));
        assert!(snap.contains_key("degraded_rest"));
        assert!(snap.contains_key("degrade_denied"));
        assert_eq!(snap["published"], 0);
        // BTreeMap 有序，键名可直接给 06 做字段引用（06 §5）。
        let keys: Vec<&String> = snap.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn counters_are_shared_across_handles() {
        let c = Arc::new(Counters::new());
        let c2 = c.clone();
        c2.published.fetch_add(3, Ordering::Relaxed);
        assert_eq!(c.published(), 3);
    }
}
