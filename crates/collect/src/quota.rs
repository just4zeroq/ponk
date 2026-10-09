//! 配额仲裁：**按 venue 各自的令牌桶**（02 §6）。
//!
//! # 为什么自建令牌桶，而不是「透传 dig3 的 rateLimit」
//!
//! 实测 dig3 源码后的结论（02 §7 待办 1「dig3 真实 API 核对」）：
//!
//! * `RateLimitCapabilities`（`model` / `rest_pools` / `endpoint_weights` / `ws`）
//!   是**纯声明式能力表**，没有任何运行时执行器按它限流；
//! * dig3 确有 `core::utils::{SimpleRateLimiter, WeightRateLimiter, …}`，
//!   但只在**个别 L1/L2 数据商连接器**（Finnhub / Tiingo / Polygon / AngelOne / Dhan）
//!   内部自用；**Binance / OKX 连接器没有接线**；
//! * 因此「透传 dig3 的 rateLimit」在首批交易所上等于没有限流。
//!
//! 故选**方案一：进程内按 venue 各自的令牌桶**（02 §6 原文）。
//!
//! # 配额域
//!
//! 配额域 = `(venue, 出口 IP)`（01 / 既有 PONK 定案 C）。本模块按 **venue** 建桶：
//! 同一进程内的 `binance:spot` 与 `binance:swap` **共用** binance 的桶（同一出口 IP、
//! 同一所的全局配额），`okx` 另有一只——**所与所之间的配额互不串**（02 §4）。
//! 多出口 IP 的拆分将来由控制面按 `QuotaShare::egress_ip` 下发后在此扩键。
//!
//! # 消耗点
//!
//! * WS 入站行情**不消耗**配额（入站不限流）；
//! * `subscribe_batch` 每次调用计 1 个 token（出站订阅帧受 `WsLimits::max_msg_per_sec` 约束）；
//! * REST 降级每次请求计 1 个 token。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::{BucketConfig, QuotaConfig};

/// 单只令牌桶（漏桶式补充 + 突发容量）。
#[derive(Debug, Clone)]
pub struct TokenBucket {
    pub capacity: f64,
    pub refill_per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(cfg: BucketConfig, now: Instant) -> Self {
        Self {
            capacity: cfg.capacity.max(0.0),
            refill_per_sec: cfg.refill_per_sec.max(0.0),
            tokens: cfg.capacity.max(0.0),
            last: now,
        }
    }

    /// 按 `now` 补充令牌。
    pub fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last);
        if dt.is_zero() {
            return;
        }
        self.last = now;
        self.tokens = (self.tokens + dt.as_secs_f64() * self.refill_per_sec).min(self.capacity);
    }

    /// 尝试扣除 `cost`；成功返回 `true`。
    pub fn try_acquire(&mut self, cost: f64, now: Instant) -> bool {
        self.refill(now);
        if self.tokens + f64::EPSILON >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// 当前可用令牌数（补充后）。
    pub fn available(&mut self, now: Instant) -> f64 {
        self.refill(now);
        self.tokens
    }

    /// 还需多久才能攒够 `cost`（不足则为 0）。
    pub fn time_until(&mut self, cost: f64, now: Instant) -> Duration {
        let avail = self.available(now);
        if avail + f64::EPSILON >= cost || self.refill_per_sec <= 0.0 {
            return Duration::ZERO;
        }
        let missing = cost - avail;
        Duration::from_secs_f64(missing / self.refill_per_sec)
    }
}

/// 按 venue 的令牌桶注册表；每个 venue 一只，互不串。
pub struct QuotaRegistry {
    enabled: bool,
    default: BucketConfig,
    overrides: HashMap<String, BucketConfig>,
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl QuotaRegistry {
    pub fn new(cfg: &QuotaConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            default: cfg.default,
            overrides: cfg.venue.clone(),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// 无配置时的默认注册表（默认桶 60 容量 / 10 每秒）。
    pub fn with_defaults() -> Self {
        Self::new(&QuotaConfig::default())
    }

    fn bucket_for(&self, venue: &str, now: Instant) -> TokenBucket {
        let key = venue.to_ascii_lowercase();
        let cfg = self.overrides.get(&key).copied().unwrap_or(self.default);
        TokenBucket::new(cfg, now)
    }

    fn with_bucket<R>(&self, venue: &str, now: Instant, f: impl FnOnce(&mut TokenBucket) -> R) -> R {
        let key = venue.to_ascii_lowercase();
        let mut guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .entry(key.clone())
            .or_insert_with(|| self.bucket_for(venue, now));
        f(entry)
    }

    /// 尝试扣 `cost`（`now` 由调用方注入，便于离线测试）。
    pub fn try_acquire_at(&self, venue: &str, cost: f64, now: Instant) -> bool {
        if !self.enabled {
            return true;
        }
        self.with_bucket(venue, now, |b| b.try_acquire(cost, now))
    }

    /// 尝试扣 `cost`（用真实时钟）。
    pub fn try_acquire(&self, venue: &str, cost: f64) -> bool {
        self.try_acquire_at(venue, cost, Instant::now())
    }

    /// 阻塞到攒够 `cost` 再扣除；返回实际等待时长（`Duration::ZERO` = 未等待）。
    ///
    /// `max_wait` 为 `None` 表示无限等（降级轮询场景：宁可慢，不可超配额）。
    /// 超时返回 `Err(等待时长)`，调用方应记 `quota_denied`。
    pub async fn acquire(
        &self,
        venue: &str,
        cost: f64,
        max_wait: Option<Duration>,
    ) -> std::result::Result<Duration, Duration> {
        if !self.enabled {
            return Ok(Duration::ZERO);
        }
        let started = Instant::now();
        let mut waited = Duration::ZERO;
        loop {
            let now = Instant::now();
            if self.try_acquire_at(venue, cost, now) {
                return Ok(waited);
            }
            let wait = self.with_bucket(venue, now, |b| b.time_until(cost, now));
            let wait = if wait.is_zero() {
                // refill 为 0 或 cost > capacity —— 永远攒不够
                return Err(Duration::ZERO);
            } else {
                wait.min(Duration::from_millis(250))
            };
            if let Some(max) = max_wait {
                if waited + wait > max {
                    return Err(waited);
                }
            }
            tokio::time::sleep(wait).await;
            waited += wait;
            let _ = started;
        }
    }

    /// 当前可用令牌（观测 / 测试用）。
    pub fn available_at(&self, venue: &str, now: Instant) -> f64 {
        if !self.enabled {
            return f64::INFINITY;
        }
        self.with_bucket(venue, now, |b| b.available(now))
    }
}

impl std::fmt::Debug for QuotaRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaRegistry")
            .field("enabled", &self.enabled)
            .field("buckets", &self.buckets.lock().map(|b| b.len()).unwrap_or(0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(capacity: f64, refill: f64) -> QuotaConfig {
        QuotaConfig {
            enabled: true,
            default: BucketConfig {
                capacity,
                refill_per_sec: refill,
                cost: 1.0,
            },
            venue: HashMap::new(),
        }
    }

    #[test]
    fn bucket_consumes_then_blocks() {
        let q = QuotaRegistry::new(&cfg(3.0, 1.0));
        let t0 = Instant::now();
        assert!(q.try_acquire_at("binance", 1.0, t0));
        assert!(q.try_acquire_at("binance", 1.0, t0));
        assert!(q.try_acquire_at("binance", 1.0, t0));
        assert!(!q.try_acquire_at("binance", 1.0, t0), "容量 3 耗尽后应拒绝");
    }

    #[test]
    fn refill_over_time() {
        let q = QuotaRegistry::new(&cfg(3.0, 2.0));
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(q.try_acquire_at("binance", 1.0, t0));
        }
        assert!(!q.try_acquire_at("binance", 1.0, t0));
        let t1 = t0 + Duration::from_millis(500); // +1.0 token
        assert!(q.try_acquire_at("binance", 1.0, t1));
        assert!(!q.try_acquire_at("binance", 1.0, t1));
    }

    #[test]
    fn venues_do_not_share_buckets() {
        // 02 §4：多所同进程时所间不串。
        let q = QuotaRegistry::new(&cfg(1.0, 0.0));
        let t0 = Instant::now();
        assert!(q.try_acquire_at("binance", 1.0, t0));
        assert!(!q.try_acquire_at("binance", 1.0, t0));
        // okx 仍满
        assert!(q.try_acquire_at("okx", 1.0, t0));
        assert!(!q.try_acquire_at("okx", 1.0, t0));
    }

    #[test]
    fn same_venue_across_markets_shares_one_bucket() {
        // 配额域 = (venue, 出口 IP)：binance:spot 与 binance:swap 共用一只桶。
        let q = QuotaRegistry::new(&cfg(1.0, 0.0));
        let t0 = Instant::now();
        assert!(q.try_acquire_at("binance", 1.0, t0));
        assert!(!q.try_acquire_at("binance", 1.0, t0));
    }

    #[test]
    fn per_venue_override_wins() {
        let mut c = cfg(1.0, 0.0);
        c.venue.insert(
            "okx".into(),
            BucketConfig {
                capacity: 10.0,
                refill_per_sec: 0.0,
                cost: 1.0,
            },
        );
        let q = QuotaRegistry::new(&c);
        let t0 = Instant::now();
        assert!(q.try_acquire_at("binance", 1.0, t0), "默认桶容量 1");
        assert!(!q.try_acquire_at("binance", 1.0, t0));
        for _ in 0..10 {
            assert!(q.try_acquire_at("okx", 1.0, t0));
        }
        assert!(!q.try_acquire_at("okx", 1.0, t0));
    }

    #[test]
    fn disabled_registry_always_allows() {
        let mut c = cfg(1.0, 0.0);
        c.enabled = false;
        let q = QuotaRegistry::new(&c);
        let t0 = Instant::now();
        for _ in 0..100 {
            assert!(q.try_acquire_at("binance", 1.0, t0));
        }
    }

    #[tokio::test]
    async fn impossible_cost_times_out_without_hanging() {
        // cost > capacity 永远攒不够 → time_until 为 0 → acquire 立即 Err（不挂死）。
        let q = QuotaRegistry::new(&cfg(1.0, 0.0));
        let t0 = Instant::now();
        assert!(q.try_acquire_at("binance", 1.0, t0));
        let r = q.acquire("binance", 1.0, Some(Duration::from_millis(50))).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn acquire_waits_until_refill() {
        let q = std::sync::Arc::new(QuotaRegistry::new(&cfg(1.0, 100.0)));
        let t = q.acquire("binance", 1.0, Some(Duration::from_secs(1))).await;
        assert!(t.is_ok());
        // 桶已空，第二次必须等到补充（100/s → 10ms）
        let t2 = q.acquire("binance", 1.0, Some(Duration::from_secs(1))).await.unwrap();
        assert!(t2 > Duration::ZERO);
    }

    #[tokio::test]
    async fn acquire_respects_max_wait() {
        let q = std::sync::Arc::new(QuotaRegistry::new(&cfg(1.0, 0.0)));
        assert!(q.acquire("binance", 1.0, None).await.is_ok());
        let r = q.acquire("binance", 1.0, Some(Duration::from_millis(30))).await;
        assert!(r.is_err());
    }
}
