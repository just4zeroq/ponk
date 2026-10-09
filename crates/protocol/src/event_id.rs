//! 幂等键 `event_id` 的构造（03 §5.4）。
//!
//! > `event_id`（幂等，须内容敏感）
//! > `身份键 : event_type : 身份时间 : 内容哈希`，其中**身份键按家族取值**：
//! > 行情 / 信号 / 因子用 `instrument_id`，告警用 `alarm_id` 的规则与触发源，
//! > 心跳用 `instance_id`，命令用 `assignment_version` 或 `domain+version`
//! > ——即身份键始终是「这条事件相对于谁唯一」。
//! >
//! > 哈希在**规范化视图**（递归键排序）上计算……与线上序列化格式无关，
//! > 因此更换格式不改变 `event_id`，去重与替换语义不漂移。
//!
//! ## 为什么这里没有 `x_version`
//!
//! `x_version` 是**定序**（单调、取 `received_ts`），与 `event_id`（幂等）是
//! 两项独立设计，**不可合并**（03 §5.4）。`event_id` 只由「身份键 + 身份时间 + 内容」
//! 决定，因此同一根 bar 的同一份内容重复推送会得到**相同** `event_id`
//! （存储层去重 / upsert 消化），而 `x_version` 保证替换取最新。

use crate::body::Body;
use crate::canonical::content_hash_of;
use crate::envelope::Envelope;
use crate::error::ProtocolError;
use crate::payload::MarketData;

/// 身份键：按家族取值（03 §5.4）。
pub fn identity_key(body: &Body) -> String {
    match body {
        Body::Market(m) => m.instrument_id(),
        Body::Signal(s) => s.instrument.clone(),
        Body::Factor(f) => f.instrument.clone(),
        // 告警用 alarm_id 的「规则与触发源」：由信号触发则带 signal_id，
        // 平台级告警（心跳丢失、总线积压）无触发源，用 `-` 占位。
        Body::Alarm(a) => format!(
            "{}|{}",
            a.rule_id,
            a.signal_id.as_deref().unwrap_or("-")
        ),
        Body::Ops(o) => o.instance_id.clone(),
        Body::Ctl(c) => c.identity_key(),
    }
}

/// 身份时间：**按家族从负载推导**（03 §5.4）。
///
/// `event_type` 只到家族，区分不了 `md.candle.1m` 与 `md.trade`，所以推导规则由
/// 「家族 + 负载类型」共同决定：
///
/// * 行情 K 线族（**全部周期**，不只 1m）→ **bar 起点** `Kline::open_time`（03 §5.4）；
/// * 行情其余通道 → `exchange_ts`（每次推送均变，03 §5.4「其余事件族身份时间每次推送均变」）；
/// * 信号 / 因子 → `timestamp`；告警 → `triggered_ts`；心跳 → `ts`；
/// * `ConfigChanged` / `ConfigSnapshot` → 域版本（`version`）；`SubscribePlan` → `assignment_version`。
///
/// 身份键是 `instrument_id`（03 §5.4 / 05 §7）；`signal_id` / `direction` / `version` / `value`
/// 不进身份键，而是进入**内容哈希**（见 [`content_hash`]）。
pub fn identity_time(body: &Body) -> u64 {
    match body {
        Body::Market(m) => match &m.data {
            MarketData::Kline(k) => k.open_time.max(0) as u64,
            _ => m.exchange_ts,
        },
        Body::Signal(s) => s.timestamp,
        Body::Factor(f) => f.timestamp,
        Body::Alarm(a) => a.triggered_ts,
        Body::Ops(o) => o.ts,
        Body::Ctl(c) => match c {
            crate::payload::CtlMessage::SubscribePlan(p) => p.assignment_version,
            crate::payload::CtlMessage::ConfigChanged(ch) => ch.ts,
            // 快照没有时间戳字段，身份时间即它所代表的域版本。
            crate::payload::CtlMessage::ConfigSnapshot { version, .. } => *version,
        },
    }
}

/// 分片键（03 §8）：行情 / 信号 / 因子 = `instrument_id`；心跳 / 告警 / 配置 = `None`。
pub fn shard_of(body: &Body) -> Option<String> {
    match body {
        Body::Market(m) => Some(m.instrument_id()),
        Body::Signal(s) => Some(s.instrument.clone()),
        Body::Factor(f) => Some(f.instrument.clone()),
        Body::Alarm(_) | Body::Ops(_) | Body::Ctl(_) => None,
    }
}

/// `身份键 : event_type : 身份时间 : 内容哈希`（03 §5.4）。
///
/// 内容哈希在规范化视图上计算，**与序列化格式无关**——本函数只读 `Body` 的
/// Rust 数据树，从不触碰线上字节。
pub fn compute(envelope: &Envelope) -> Result<String, ProtocolError> {
    debug_assert_eq!(envelope.event_type, envelope.body.event_type());
    let key = identity_key(&envelope.body);
    let ts = identity_time(&envelope.body);
    let hash = content_hash_of(&envelope.body)?;
    Ok(format!(
        "{}:{}:{}:{}",
        key,
        envelope.event_type.as_str(),
        ts,
        hash
    ))
}

/// 直接由家族体算内容哈希（测试与运维工具用）。
pub fn content_hash(body: &Body) -> Result<String, ProtocolError> {
    content_hash_of(body)
}
