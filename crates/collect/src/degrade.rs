//! REST 降级（02 §6）。
//!
//! | 通道 | 策略 | 本文档依据 |
//! |---|---|---|
//! | `ticker` / `l1` | 全市场降级：断线期间轮询白名单内全部标的的最新 ticker | 02 §6「ticker/L1 可全市场 REST 降级」 |
//! | `candle_1m` | **仅核心集**降级，且**绝不补断档** | 02 §6「candle 仅核心集可降级，其余交回补」 |
//! | `trade` | **不降级** | 02 §6「trade 不降级」 |
//! | `funding` | 不降级（02 §6 未列入可降级项，保守处理） | — |
//!
//! ## 「不回补」是怎么被机械保证的（02 §6 步骤 3）
//!
//! K 线降级只允许发布 **`open_time >= 断线时刻向上取整到分钟`** 的 bar。
//! 断线发生在 `12:00:30` → 最早允许 `12:01:00`；`12:00:00` 这根 bar 在断线前
//! 已经实时收到过一部分、断线后又缺了一段——把它补出来就是**回补**，而回补
//! 明确不属于采集（归未来的「数据检查与修复节点」，01 §7）。
//! 被挡下的 bar 计入 `degrade_backfill_blocked`，可见、可审计，但绝不发布。
//!
//! ticker 没有「断档」语义（它是周期性的最新状态），因此不做时间过滤——
//! 降级期间轮询到的最新 ticker 直接发布。
//!
//! ## 配额
//!
//! 每次 REST 请求前向**该 venue 的令牌桶**取 1 个 token（02 §6 所间不串）；
//! 桶空则等待，最多等 [`DegradeConfig`] 允许的时长，超时记 `quota_denied` 跳过本轮。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use digdigdig3::core::types::Ticker;
use tokio::sync::{mpsc, watch};

use crate::channel::{Channel, DegradePolicy};
use crate::config::{ConnectionKey, DegradeConfig, ResolvedPlan};
use crate::counters::Counters;
use crate::envelope::{market_envelope, market_topic, PendingEvent};
use crate::payload::{Classifier, MarketSample};
use crate::quota::QuotaRegistry;
use crate::source::MarketSource;

/// 一个连接在断线期间要降级处理的目标集合。
#[derive(Debug, Clone)]
pub struct DegradePlan {
    pub connection: ConnectionKey,
    /// 全市场 ticker / l1 的标的（原生代码）。
    pub ticker_instruments: Vec<String>,
    /// 核心集里带 `candle_1m` 的标的（原生代码）。
    pub candle_core_instruments: Vec<String>,
    /// 存在但**不支持**降级的通道（`trade` / `funding`）——每轮记一次 denied。
    pub denied_channels: Vec<Channel>,
    /// 断线时刻（epoch ms），用于 K 线「不回补」边界。
    pub disconnect_at_ms: u64,
    /// `venue`（配额键）。
    pub venue: String,
}

impl Default for DegradePlan {
    fn default() -> Self {
        Self {
            connection: ConnectionKey::new("", ""),
            ticker_instruments: Vec::new(),
            candle_core_instruments: Vec::new(),
            denied_channels: Vec::new(),
            disconnect_at_ms: 0,
            venue: String::new(),
        }
    }
}

impl DegradePlan {
    /// 由计划 + 配置构造；`disconnect_at_ms` 由调用方在进入退避时打点。
    pub fn build(
        conn: &ConnectionKey,
        plan: &ResolvedPlan,
        is_core: impl Fn(&str) -> bool,
        disconnect_at_ms: u64,
    ) -> Self {
        let mut d = DegradePlan {
            connection: conn.clone(),
            disconnect_at_ms,
            venue: conn.venue.clone(),
            ..Default::default()
        };
        for (instrument_id, meta) in &plan.instruments {
            if &meta.venue != &conn.venue || &meta.market != &conn.market {
                continue;
            }
            let _ = instrument_id;

            if meta.channels.contains(&Channel::Ticker) || meta.channels.contains(&Channel::L1) {
                d.ticker_instruments.push(meta.native_code.clone());
            }
            // 02 §6「candle 仅核心集可降级，其余交回补」：
            // 非核心集的 candle **既不进降级集合、也不计 denied** —— 它不是被拒绝，
            // 而是职责上交给未来的数据检查修复节点。
            if meta.channels.contains(&Channel::Candle1m) && is_core(&meta.native_code) {
                d.candle_core_instruments.push(meta.native_code.clone());
            }
            // DegradePolicy::Never 的通道（trade / funding）记 denied。
            for ch in &meta.channels {
                if ch.rest_degrade() == DegradePolicy::Never && !d.denied_channels.contains(ch) {
                    d.denied_channels.push(*ch);
                }
            }
        }
        d.ticker_instruments.sort();
        d.ticker_instruments.dedup();
        d.candle_core_instruments.sort();
        d.candle_core_instruments.dedup();
        d.denied_channels.sort_unstable();
        d
    }

    pub fn is_noop(&self) -> bool {
        self.ticker_instruments.is_empty()
            && self.candle_core_instruments.is_empty()
            && self.denied_channels.is_empty()
    }

    /// K 线「不回补」边界：断线时刻向上取整到分钟。
    pub fn earliest_allowed_bar_open(&self) -> u64 {
        ceil_to_minute(self.disconnect_at_ms)
    }
}

/// 向上取整到 60_000ms（1m K 线粒度）。
pub fn ceil_to_minute(ms: u64) -> u64 {
    (ms + 59_999) / 60_000 * 60_000
}

/// 降级轮询循环。
///
/// * `active` = `false` 表示连接处于断线态 → 本轮执行降级；`= true` 表示已恢复 → 空转。
/// * `shutdown` = `true` 表示连接监督器退出 → 本任务结束（必须被 await，
///   否则它会继续持有发送端，发布管道排不干净）。
pub async fn run(
    plan: DegradePlan,
    instance_id: String,
    source: Arc<dyn MarketSource>,
    classifier: Arc<Classifier>,
    quota: Arc<QuotaRegistry>,
    counters: Arc<Counters>,
    queue: mpsc::Sender<PendingEvent>,
    cfg: DegradeConfig,
    mut active: watch::Receiver<bool>,
    mut shutdown: watch::Receiver<bool>,
) {
    if !cfg.enabled {
        return;
    }
    let earliest = plan.earliest_allowed_bar_open();
    let max_wait = Some(Duration::from_millis(cfg.poll_interval_ms.max(1) * 5));
    let mut tick = tokio::time::interval(Duration::from_millis(cfg.poll_interval_ms.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::debug!(
        target: "ponk::collect::degrade",
        connection = %plan.connection,
        tickers = plan.ticker_instruments.len(),
        candle_core = plan.candle_core_instruments.len(),
        earliest_bar_open = earliest,
        "REST degrade started"
    );

    loop {
        if *shutdown.borrow() {
            break;
        }

        if !*active.borrow() {
            // 02 §6：trade / funding 不降级。
            for _ in &plan.denied_channels {
                counters.degrade_denied.fetch_add(1, Ordering::Relaxed);
            }

            round_tickers(
                &plan,
                &instance_id,
                &source,
                &classifier,
                &quota,
                &counters,
                &queue,
                &cfg,
                max_wait,
                &active,
                &shutdown,
            )
            .await;
            round_candles(
                &plan,
                &instance_id,
                &source,
                &classifier,
                &quota,
                &counters,
                &queue,
                &cfg,
                earliest,
                max_wait,
                &active,
                &shutdown,
            )
            .await;
            if *shutdown.borrow() {
                break;
            }
        }

        tokio::select! {
            _ = tick.tick() => {}
            _ = active.changed() => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    tracing::debug!(
        target: "ponk::collect::degrade",
        connection = %plan.connection,
        "REST degrade stopped"
    );
}

async fn acquire(quota: &QuotaRegistry, venue: &str, counters: &Counters, max_wait: Option<Duration>) -> bool {
    match quota.acquire(venue, 1.0, max_wait).await {
        Ok(waited) => {
            if waited > Duration::ZERO {
                counters.quota_waits.fetch_add(1, Ordering::Relaxed);
            }
            true
        }
        Err(_) => {
            counters.quota_denied.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

async fn round_tickers(
    plan: &DegradePlan,
    instance_id: &str,
    source: &Arc<dyn MarketSource>,
    classifier: &Arc<Classifier>,
    quota: &QuotaRegistry,
    counters: &Counters,
    queue: &mpsc::Sender<PendingEvent>,
    cfg: &DegradeConfig,
    max_wait: Option<Duration>,
    active: &watch::Receiver<bool>,
    shutdown: &watch::Receiver<bool>,
) {
    if plan.ticker_instruments.is_empty() {
        return;
    }
    for code in plan
        .ticker_instruments
        .iter()
        .take(cfg.max_instruments_per_round)
    {
        if *shutdown.borrow() || *active.borrow() {
            return;
        }
        if !acquire(quota, &plan.venue, counters, max_wait).await {
            return;
        }
        match source.rest_ticker(code).await {
            Ok(Some(ticker)) => {
                if let Some(sample) = ticker_sample(code, ticker, classifier) {
                    emit(sample, instance_id, counters, queue);
                }
            }
            Ok(None) => {}
            Err(_) => {
                counters.degrade_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn round_candles(
    plan: &DegradePlan,
    instance_id: &str,
    source: &Arc<dyn MarketSource>,
    classifier: &Arc<Classifier>,
    quota: &QuotaRegistry,
    counters: &Counters,
    queue: &mpsc::Sender<PendingEvent>,
    cfg: &DegradeConfig,
    earliest: u64,
    max_wait: Option<Duration>,
    active: &watch::Receiver<bool>,
    shutdown: &watch::Receiver<bool>,
) {
    if plan.candle_core_instruments.is_empty() {
        return;
    }
    for code in plan
        .candle_core_instruments
        .iter()
        .take(cfg.max_instruments_per_round)
    {
        if *shutdown.borrow() || *active.borrow() {
            return;
        }
        if !acquire(quota, &plan.venue, counters, max_wait).await {
            return;
        }
        match source.rest_kline_1m_latest(code).await {
            Ok(klines) => {
                for k in klines {
                    // 02 §6 步骤 3：断档区间不补 —— 断线前就开始的 bar 直接丢弃。
                    if (k.open_time.max(0) as u64) < earliest {
                        counters.degrade_backfill_blocked.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let ev = digdigdig3::core::types::StreamEvent::Kline {
                        symbol: code.clone(),
                        interval: digdigdig3::core::types::KlineInterval::new("1m"),
                        kline: k,
                    };
                    for sample in classifier.classify(&ev) {
                        emit(sample, instance_id, counters, queue);
                    }
                }
            }
            Err(_) => {
                counters.degrade_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn ticker_sample(
    code: &str,
    ticker: Ticker,
    classifier: &Arc<Classifier>,
) -> Option<MarketSample> {
    let ev = digdigdig3::core::types::StreamEvent::Ticker {
        symbol: code.to_string(),
        ticker,
    };
    classifier.classify(&ev).into_iter().next()
}

fn emit(
    sample: MarketSample,
    instance_id: &str,
    counters: &Counters,
    queue: &mpsc::Sender<PendingEvent>,
) {
    let received_ts = crate::heartbeat::now_ms();
    let env = match market_envelope(&sample, received_ts, instance_id) {
        Ok(e) => e,
        Err(_) => {
            counters.dropped_samples.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let pending = PendingEvent {
        topic: market_topic(&sample),
        envelope: env,
        channel: sample.channel,
    };
    // 10 §3：不做背压 —— 队列满即丢并计数。
    match queue.try_send(pending) {
        Ok(()) => {
            counters.degraded_rest.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            counters.dropped_backlog.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceil_to_minute_boundary_cases() {
        assert_eq!(ceil_to_minute(0), 0);
        assert_eq!(ceil_to_minute(1), 60_000);
        assert_eq!(ceil_to_minute(60_000), 60_000);
        assert_eq!(ceil_to_minute(60_001), 120_000);
        assert_eq!(ceil_to_minute(1_700_000_030_000), 1_700_000_040_000);
    }

    #[test]
    fn no_backfill_boundary_excludes_the_bar_containing_the_disconnect() {
        // 断线于 12:00:30 → 最早允许 12:01:00 的 bar，12:00 那根被挡下。
        let plan = DegradePlan {
            disconnect_at_ms: 1_700_000_030_000,
            ..Default::default()
        };
        assert_eq!(plan.earliest_allowed_bar_open(), 1_700_000_040_000);
        // 断线正好在整分钟 → 该 bar 之前没有观测到任何数据，允许发布
        let p2 = DegradePlan {
            disconnect_at_ms: 1_700_000_040_000,
            ..Default::default()
        };
        assert_eq!(p2.earliest_allowed_bar_open(), 1_700_000_040_000);
    }

    fn plan_toml(core: bool) -> String {
        format!(
            r#"
instance_id = "c"
core_set = {core_set}
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["ticker", "trade"]
[[shards.instruments]]
native_code = "BTCUSDT"
[[shards.instruments]]
native_code = "ETHUSDT"
channels = ["trade"]
[[shards]]
venue = "binance"
market = "swap"
shard_idx = 0
channels = ["candle_1m", "funding"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#,
            core_set = if core { r#"["BTCUSDT"]"# } else { "[]" }
        )
    }

    #[test]
    fn degrade_plan_classifies_channels_by_policy() {
        let cfg = crate::config::CollectorConfig::from_toml(&plan_toml(true)).unwrap();
        let plan = cfg.resolve().unwrap();

        let spot = DegradePlan::build(
            &ConnectionKey::new("binance", "spot"),
            &plan,
            |c| cfg.is_core(c),
            1_700_000_030_000,
        );
        // ticker 全市场降级：两个标的里只有 BTCUSDT 订了 ticker
        assert_eq!(spot.ticker_instruments, vec!["BTCUSDT".to_string()]);
        // spot 没有 candle
        assert!(spot.candle_core_instruments.is_empty());
        // trade 不降级 → 记 denied
        assert_eq!(spot.denied_channels, vec![Channel::Trade]);

        let swap = DegradePlan::build(
            &ConnectionKey::new("binance", "swap"),
            &plan,
            |c| cfg.is_core(c),
            1_700_000_030_000,
        );
        assert!(swap.ticker_instruments.is_empty());
        assert_eq!(swap.candle_core_instruments, vec!["BTCUSDT".to_string()]);
        assert_eq!(swap.denied_channels, vec![Channel::Funding]);
    }

    #[test]
    fn non_core_candle_is_not_degraded() {
        // 02 §6「candle 仅核心集可降级，其余交回补」——非核心集既不降级也不计 denied。
        let cfg = crate::config::CollectorConfig::from_toml(&plan_toml(false)).unwrap();
        let plan = cfg.resolve().unwrap();
        let swap = DegradePlan::build(
            &ConnectionKey::new("binance", "swap"),
            &plan,
            |c| cfg.is_core(c),
            0,
        );
        assert!(swap.candle_core_instruments.is_empty());
        assert_eq!(swap.denied_channels, vec![Channel::Funding]);
    }

    #[test]
    fn plan_with_no_degradable_channel_is_noop() {
        let cfg = crate::config::CollectorConfig::from_toml(
            r#"
instance_id = "c"
[[shards]]
venue = "okx"
market = "spot"
channels = ["l1"]
[[shards.instruments]]
native_code = "BTC-USDT"
"#,
        )
        .unwrap();
        let plan = cfg.resolve().unwrap();
        let d = DegradePlan::build(
            &ConnectionKey::new("okx", "spot"),
            &plan,
            |_| false,
            0,
        );
        assert!(!d.is_noop());
        assert_eq!(d.ticker_instruments, vec!["BTC-USDT".to_string()]);
        assert!(d.denied_channels.is_empty());
    }
}
