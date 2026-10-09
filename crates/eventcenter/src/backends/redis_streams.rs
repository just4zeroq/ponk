//! Redis Streams backend（03 §7.1「起步选择」，feature `redis` 门控）。
//!
//! 03 §7.1 对 Redis Streams 的定案：**满足契约层**；能力层 = **无分区、无重放、
//! XAUTOCLAIM 死信**；03 §7.4「保留清理交给 backend 自带策略（XTRIM / MAXLEN）」、
//! 「主题初始化由首次写入自动创建（自动建流）」。
//!
//! ## 本实现做了什么
//!
//! | 项 | 实现 |
//! |---|---|
//! | 消费组 | `XGROUP CREATE … MKSTREAM`（已存在则忽略 `BUSYGROUP`） |
//! | **无 ack**（03 §4.4） | `XREADGROUP` 读到即 `XACK`（「backend 默认行为：Redis 随读 XACK」） |
//! | 起始位置（03 §4.6） | latest = `$`、earliest = `0`、指定偏移 = `XGROUP SETID` |
//! | XAUTOCLAIM 死信（03 §7.1） | 每次 `next` 先 `XAUTOCLAIM` 回收滞留消息；超过 `max_attempts` → 写 `dlq.<topic>` |
//! | DLQ（03 §7.3） | 发布失败把 `DeadLetterMeta` 写进 `dlq.<topic>` |
//! | 保留 | `XADD MAXLEN`（03 §7.4） |
//!
//! ## 未实测
//!
//! 本仓库的测试环境**没有本地 Redis 服务**（且测试不得依赖外网），因此本 backend
//! **只保证编译通过，未做任何运行时验证**：`capabilities().measured == false`，
//! 能力取值来自 03 §7.1 的文档定案而非实测（03 §4.5）。

use crate::backends::memory::validate_topic;
use crate::capabilities::Capabilities;
use crate::error::ECError;
use crate::session::{DeadLetterReader, EventCenter, Session, SessionOpts, StartPosition};
use ponk_protocol::{
    DeadLetter, DeadLetterMeta, Deserializer, Envelope, FormatPolicy, Group, Serializer, Topic,
};
use redis::streams::{
    StreamAutoClaimOptions, StreamAutoClaimReply, StreamMaxlen, StreamReadOptions, StreamReadReply,
};
use redis::{Commands, Connection, RedisError, RedisResult};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// stream 中承载线负载的字段名。
const PAYLOAD_FIELD: &str = "payload";

/// Redis backend 选项。
#[derive(Debug, Clone)]
pub struct RedisOptions {
    /// `redis://…` 连接串（03 §7.1：backend 选型为配置项）。
    pub url: String,
    /// `XADD MAXLEN`（03 §7.4：保留清理交给 backend 自带策略）。
    pub max_len: Option<usize>,
    /// `XAUTOCLAIM` 的 `min-idle-time`（ms）。
    pub claim_min_idle_ms: u64,
    /// 每次 `XAUTOCLAIM` 的 `COUNT`。
    pub claim_count: usize,
    /// 同一消息被回收的上限，超过即判死信（03 §7.1「XAUTOCLAIM 死信」）。
    pub max_attempts: u32,
    /// 序列化格式策略（粒度 = 主题族，03 §6）。
    pub policy: FormatPolicy,
}

impl Default for RedisOptions {
    fn default() -> Self {
        RedisOptions {
            url: "redis://127.0.0.1:6379/".to_string(),
            max_len: Some(1_000_000),
            claim_min_idle_ms: 60_000,
            claim_count: 64,
            max_attempts: 5,
            policy: FormatPolicy::default(),
        }
    }
}

/// Redis Streams backend。
///
/// 未打开的连接按需建立：发布用一条专用连接（内部加锁），
/// 每个 [`Session`] 各持一条连接（`XREADGROUP BLOCK` 是阻塞调用，不能共享）。
pub struct RedisEventCenter {
    client: redis::Client,
    opts: RedisOptions,
}

impl RedisEventCenter {
    /// 按选项连接（只构造客户端，不建立 TCP 连接）。
    pub fn new(opts: RedisOptions) -> Result<Self, ECError> {
        let client =
            redis::Client::open(opts.url.as_str()).map_err(|e| ECError::Backend(e.to_string()))?;
        Ok(RedisEventCenter { client, opts })
    }

    /// 按连接串构造。
    pub fn from_url(url: impl Into<String>) -> Result<Self, ECError> {
        let mut opts = RedisOptions::default();
        opts.url = url.into();
        Self::new(opts)
    }

    fn connect(&self) -> Result<Connection, ECError> {
        self.client
            .get_connection()
            .map_err(|e| ECError::Backend(format!("redis connect: {e}")))
    }

    fn xadd(
        &self,
        con: &mut Connection,
        topic: &str,
        bytes: &[u8],
    ) -> Result<(), ECError> {
        let items: &[(&str, &[u8])] = &[(PAYLOAD_FIELD, bytes)];
        let _: Option<String> = match self.opts.max_len {
            Some(n) => con
                .xadd_maxlen(topic, StreamMaxlen::Equals(n), "*", items)
                .map_err(|e| redis_err("XADD", e))?,
            None => con
                .xadd(topic, "*", items)
                .map_err(|e| redis_err("XADD", e))?,
        };
        Ok(())
    }

    fn try_publish(&self, topic: &Topic, envelope: &Envelope) -> Result<(), ECError> {
        validate_topic(topic)?;
        envelope.validate()?;
        let codec = self.opts.policy.codec_for_topic(topic.as_str());
        let bytes = codec.serialize_envelope(envelope)?;

        let mut con = self.connect()?;
        match self.xadd(&mut con, topic.as_str(), &bytes) {
            Ok(()) => Ok(()),
            Err(first) => {
                // 连接可能已失效：重建连接再试一次。
                // 写入侧是 at-least-once（03 §3），重复可接受；仍失败则原样返回。
                drop(con);
                match self.connect() {
                    Ok(mut fresh) => match self.xadd(&mut fresh, topic.as_str(), &bytes) {
                        Ok(()) => Ok(()),
                        Err(second) => Err(second),
                    },
                    Err(_) => Err(first),
                }
            }
        }
    }

    fn record_dead_letter(&self, topic: &Topic, envelope: &Envelope, error: &ECError) {
        let meta = DeadLetterMeta {
            origin_topic: topic.clone(),
            error: error.to_string(),
            attempts: 1,
            failed_ts: now_ms(),
            body: Box::new(envelope.clone()),
        };
        let dlq_topic = topic.to_dead_letter();
        let codec = self.opts.policy.codec_for_topic(dlq_topic.as_str());
        let Ok(bytes) = codec.serialize_dead_letter(&meta) else {
            return;
        };
        if let Ok(mut con) = self.connect() {
            let _ = self.xadd(&mut con, dlq_topic.as_str(), &bytes);
        }
    }
}

impl EventCenter for RedisEventCenter {
    fn publish(&self, topic: &Topic, envelope: &Envelope) -> Result<(), ECError> {
        match self.try_publish(topic, envelope) {
            Ok(()) => Ok(()),
            Err(e) => {
                // 03 §7.3：发布失败的事件写入 dlq.<topic>，携带 DeadLetterMeta
                self.record_dead_letter(topic, envelope, &e);
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
        validate_topic(topic)?;
        if topic.is_dead_letter() {
            return Err(ECError::Unsupported(format!(
                "死信主题 `{topic}` 不经 Session 读取：用 DeadLetterReader（03 §7.4）"
            )));
        }
        if group.as_str().is_empty() {
            return Err(ECError::InvalidGroup("空消费组".to_string()));
        }

        let mut con = self.connect()?;
        let start_id = opts.start.redis_group_id();

        if matches!(opts.start, StartPosition::Offset(_)) {
            // 已存在的消费组按请求偏移重置；不存在则直接按该偏移创建
            let created: RedisResult<()> = con.xgroup_create_mkstream(
                topic.as_str(),
                group.as_str(),
                start_id.as_str(),
            );
            if let Err(e) = created {
                if !is_busy_group(&e) {
                    return Err(redis_err("XGROUP CREATE", e));
                }
                let _: () = con
                    .xgroup_setid(topic.as_str(), group.as_str(), start_id.as_str())
                    .map_err(|e| redis_err("XGROUP SETID", e))?;
            }
        } else {
            let created: RedisResult<()> = con.xgroup_create_mkstream(
                topic.as_str(),
                group.as_str(),
                start_id.as_str(),
            );
            if let Err(e) = created {
                if !is_busy_group(&e) {
                    return Err(redis_err("XGROUP CREATE", e));
                }
                // 组已存在：保持既有位点（03 §4.6 起始位置只在建组时生效）
            }
        }

        let consumer = unique_consumer(group.as_str());
        let _: bool = con
            .xgroup_createconsumer(topic.as_str(), group.as_str(), consumer.as_str())
            .map_err(|e| redis_err("XGROUP CREATECONSUMER", e))?;

        Ok(Box::new(RedisSession {
            con,
            topic: topic.clone(),
            group: group.clone(),
            consumer,
            policy: self.opts.policy.clone(),
            claim_min_idle_ms: self.opts.claim_min_idle_ms,
            claim_count: self.opts.claim_count,
            max_attempts: self.opts.max_attempts,
            buffer: VecDeque::new(),
            claims: HashMap::new(),
            closed: false,
        }))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::redis_streams()
    }
}

impl DeadLetterReader for RedisEventCenter {
    /// 从 `dlq.<topic>` 读回死信（03 §7.4：运维工具查看与人工重投），至多 `limit` 条。
    ///
    /// **不推进消费组位点**——用的是 `XRANGE` 而非 `XREADGROUP`，
    /// 因此查看死信不会让任何消费组「消费掉」它。
    fn dead_letters(&self, topic: &Topic, limit: usize) -> Result<Vec<DeadLetter>, ECError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let dlq_topic = topic.to_dead_letter();
        let codec = self.opts.policy.codec_for_topic(dlq_topic.as_str());
        let mut con = self.connect()?;
        let reply: redis::streams::StreamRangeReply = con
            .xrange(dlq_topic.as_str(), "-", "+")
            .map_err(|e| redis_err("XRANGE", e))?;
        let mut out: Vec<DeadLetter> = Vec::new();
        for sid in reply.ids.into_iter().take(limit) {
            let Some(v) = sid.map.get(PAYLOAD_FIELD) else {
                continue;
            };
            let bytes = value_bytes(v)?;
            match codec.deserialize_dead_letter(&bytes) {
                Ok(dl) => out.push(dl),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(out)
    }
}

struct RedisSession {
    con: Connection,
    topic: Topic,
    group: Group,
    consumer: String,
    policy: FormatPolicy,
    claim_min_idle_ms: u64,
    claim_count: usize,
    max_attempts: u32,
    /// 已被本会话回收、等待投递的消息。
    buffer: VecDeque<(String, Vec<u8>)>,
    /// 回收次数（超过 `max_attempts` → 死信）。
    claims: HashMap<String, u32>,
    closed: bool,
}

impl RedisSession {
    fn ack(&mut self, id: &str) -> Result<(), ECError> {
        let _: usize = self
            .con
            .xack(self.topic.as_str(), self.group.as_str(), &[id])
            .map_err(|e| redis_err("XACK", e))?;
        self.claims.remove(id);
        Ok(())
    }

    /// `XAUTOCLAIM`：回收滞留（未 ack）消息（03 §7.1「XAUTOCLAIM 死信」）。
    ///
    /// 因为「读取即推进」（随读 `XACK`），滞留只可能来自读取后进程崩溃的窗口；
    /// 回收后按 `max_attempts` 分流：继续投递 / 判死信。
    fn reclaim(&mut self) -> Result<(), ECError> {
        if self.claim_min_idle_ms == 0 {
            return Ok(());
        }
        let opts = StreamAutoClaimOptions::default().count(self.claim_count);
        let reply: StreamAutoClaimReply = match self.con.xautoclaim_options(
            self.topic.as_str(),
            self.group.as_str(),
            self.consumer.as_str(),
            self.claim_min_idle_ms,
            "0-0",
            opts,
        ) {
            Ok(r) => r,
            Err(e) if is_busy_group(&e) || is_no_group(&e) => return Ok(()),
            Err(e) => return Err(redis_err("XAUTOCLAIM", e)),
        };

        for sid in reply.claimed {
            let id = sid.id.clone();
            let attempts = *self.claims.get(&id).unwrap_or(&0) + 1;
            self.claims.insert(id.clone(), attempts);

            let payload = match sid.map.get(PAYLOAD_FIELD) {
                Some(v) => value_bytes(v)?,
                None => {
                    // 消息已被 XTRIM 掉，无可投递内容 → 直接确认
                    let _ = self.ack(&id);
                    continue;
                }
            };

            if attempts > self.max_attempts {
                // 判死信：写 dlq.<topic> 后确认
                let codec = self.policy.codec_for_topic(self.topic.as_str());
                match codec.deserialize_envelope(&payload) {
                    Ok(env) => self.write_dead_letter(&env, &format!(
                        "XAUTOCLAIM 回收 {attempts} 次仍未消费完成（min_idle={}ms）",
                        self.claim_min_idle_ms
                    )),
                    Err(e) => {
                        // 连载荷都解不出来，记一条「原负载不可解」的死信需要 Envelope，
                        // 这里只能放弃并确认，避免无限回收。
                        let _ = e;
                    }
                }
                let _ = self.ack(&id);
                continue;
            }

            self.buffer.push_back((id, payload));
        }
        Ok(())
    }

    fn write_dead_letter(&mut self, envelope: &Envelope, error: &str) {
        let dlq_topic = self.topic.to_dead_letter();
        let meta = DeadLetterMeta {
            origin_topic: self.topic.clone(),
            error: error.to_string(),
            attempts: self.max_attempts,
            failed_ts: now_ms(),
            body: Box::new(envelope.clone()),
        };
        let codec = self.policy.codec_for_topic(dlq_topic.as_str());
        let Ok(bytes) = codec.serialize_dead_letter(&meta) else {
            return;
        };
        let items: &[(&str, &[u8])] = &[(PAYLOAD_FIELD, bytes.as_slice())];
        let _: Option<String> = self.con.xadd(dlq_topic.as_str(), "*", items).ok();
    }

    /// `XREADGROUP` 读一条（`BLOCK` = 剩余超时）。
    fn read_one(&mut self, timeout: Duration) -> Result<Option<Envelope>, ECError> {
        let mut opts = StreamReadOptions::default()
            .group(self.group.as_str(), self.consumer.as_str())
            .count(1);
        // BLOCK 0 在 Redis 里是「永久阻塞」，故只在真有超时的时候才 BLOCK。
        if !timeout.is_zero() {
            opts = opts.block(timeout.as_millis() as usize);
        }

        let reply: Option<StreamReadReply> = self
            .con
            .xread_options(&[self.topic.as_str()], &[">"], &opts)
            .map_err(|e| redis_err("XREADGROUP", e))?;

        let Some(reply) = reply else {
            return Ok(None); // 超时无消息（03 §4.1）
        };

        for key in reply.keys {
            for sid in key.ids {
                let id = sid.id.clone();
                let payload = match sid.map.get(PAYLOAD_FIELD) {
                    Some(v) => value_bytes(v)?,
                    None => continue,
                };
                // 无 ack：读取即推进位点 —— 随读 XACK（03 §4.4）
                self.ack(&id)?;

                let codec = self.policy.codec_for_topic(self.topic.as_str());
                match codec.deserialize_envelope(&payload) {
                    Ok(env) => return Ok(Some(env)),
                    Err(e) => {
                        // 载荷不可解：`DeadLetterMeta` 需要原 `Envelope`，解析不出来就
                        // 无法构成死信记录 —— 已随读 XACK，丢弃并继续取下一条
                        //（03 §4.4：一致性由下游保证，畸形载荷属异常而非可补数据）。
                        let _ = e;
                    }
                }
            }
        }
        Ok(None)
    }
}

impl Session for RedisSession {
    fn next(&mut self, timeout: Duration) -> Result<Option<Envelope>, ECError> {
        if self.closed {
            return Err(ECError::Closed);
        }
        let deadline = Instant::now() + timeout;

        loop {
            if self.closed {
                return Err(ECError::Closed);
            }

            // 1) 已回收、待投递
            if let Some((id, payload)) = self.buffer.pop_front() {
                self.ack(&id)?;
                let codec = self.policy.codec_for_topic(self.topic.as_str());
                if let Ok(env) = codec.deserialize_envelope(&payload) {
                    return Ok(Some(env));
                }
                continue;
            }

            // 2) XAUTOCLAIM 回收
            self.reclaim()?;
            if !self.buffer.is_empty() {
                continue;
            }

            // 3) XREADGROUP
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let remaining = deadline - now;
            match self.read_one(remaining)? {
                Some(env) => return Ok(Some(env)),
                None => {
                    // BLOCK 已经等到超时；若期间没有新消息则按规格返回 Ok(None)
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn close(&mut self) -> Result<(), ECError> {
        self.closed = true;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 工具
// ─────────────────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn redis_err(op: &str, e: RedisError) -> ECError {
    ECError::Backend(format!("{op}: {e}"))
}

fn is_busy_group(e: &RedisError) -> bool {
    e.code() == Some("BUSYGROUP")
}

fn is_no_group(e: &RedisError) -> bool {
    e.code() == Some("NOGROUP")
}


fn value_bytes(v: &redis::Value) -> Result<Vec<u8>, ECError> {
    match v {
        redis::Value::BulkString(b) => Ok(b.clone()),
        redis::Value::SimpleString(s) => Ok(s.clone().into_bytes()),
        redis::Value::Int(n) => Ok(n.to_string().into_bytes()),
        other => redis::from_redis_value::<Vec<u8>>(other.clone())
            .map_err(|e| ECError::Backend(format!("stream 载荷类型错误：{e}"))),
    }
}

fn unique_consumer(group: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{group}-{pid}-{n}")
}
