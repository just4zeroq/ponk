//! 写入侧校验（09 §4.3 步骤 ① / §9-4）：**schema + 跨域引用**。
//!
//! | 检查项 | 出处 |
//! |---|---|
//! | 八域 schema（serde 反序列化 + 字段级约束） | 09 §3 |
//! | 告警引用的 `signal_id` 存在 | 09 §4.3 |
//! | 告警 `route.channel` 存在 | 09 §4.3 |
//! | 黑名单标的格式（canonical） | 09 §9-4 |
//! | 信号 DAG 无环、`in` 引用存在、`emit` 引用存在 | 09 §9-4 |
//! | 序列化族取值、`ctl.*`/`ops.*` 固定 JSON | 09 §2.1 / 03 §6 |
//!
//! 算子取值**不校验**：05 §3 的算子库第一批清单仍标注「待定」，
//! 控制面若硬编一份算子白名单会与 05 的落地冲突。
//!
//! 校验在写入侧完成（09 §4.3）；实例侧仍做一次轻量校验，校验不过则不应用、维持当前版本。

use crate::error::ControlError;
use crate::schema::*;
use crate::store::ConfigStore;
use ponk_protocol::{ConfigDomain, NodeKind};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// 校验一份待发布文档；跨域引用通过 `store` 读取各域当前版本。
pub fn validate(
    domain: ConfigDomain,
    payload: &Value,
    store: &dyn ConfigStore,
) -> Result<(), ControlError> {
    match domain {
        ConfigDomain::SubscribePlan => validate_subscribe_plan(payload)?,
        ConfigDomain::SignalRules => validate_signal_rules(payload)?,
        ConfigDomain::AlertRules => validate_alert_rules(payload, store)?,
        ConfigDomain::FactorRules => validate_factor_rules(payload)?,
        ConfigDomain::Channels => validate_channels(payload)?,
        ConfigDomain::ProcessCapacity => validate_process_capacity(payload)?,
        ConfigDomain::Serialization => validate_serialization(payload)?,
        // 第八域 = 交易对黑名单（09 §2 表序末行 / §2.1「黑名单 ⊂ 白名单计算」）
        ConfigDomain::InstrumentBlocklist => validate_blocklist(payload)?,
    }
    Ok(())
}

fn parse<T: serde::de::DeserializeOwned>(payload: &Value, domain: ConfigDomain) -> Result<T, ControlError> {
    serde_json::from_value::<T>(payload.clone()).map_err(|e| {
        ControlError::validation(format!("{domain} schema 不合法：{e}"))
    })
}

/// 八域各自消费的配置域（09 §8 接口表）。
pub fn consumed_domains(kind: NodeKind) -> Vec<ConfigDomain> {
    match kind {
        // 采集：唯一外部消费者为订阅计划 + 黑名单（09 §8）
        NodeKind::Collector => vec![ConfigDomain::SubscribePlan, ConfigDomain::InstrumentBlocklist],
        // 05 信号引擎
        NodeKind::Signal => vec![
            ConfigDomain::SignalRules,
            ConfigDomain::Serialization,
            ConfigDomain::InstrumentBlocklist,
        ],
        // 06 监控告警
        NodeKind::Alert => vec![
            ConfigDomain::AlertRules,
            ConfigDomain::Channels,
            ConfigDomain::Serialization,
        ],
        // 07 因子挖掘
        NodeKind::Factor => vec![ConfigDomain::FactorRules, ConfigDomain::Serialization],
        // 04 存储：Serialization
        NodeKind::Storage => vec![ConfigDomain::Serialization],
        // 03 事件中心适配层（各节点内嵌）由各节点的 Serialization 收敛承载
        NodeKind::DataSvc => vec![],
        // 控制面 = 版本来源（09 §4.4），无消费域
        NodeKind::Control => vec![],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 各域校验
// ─────────────────────────────────────────────────────────────────────────────

fn validate_subscribe_plan(payload: &Value) -> Result<(), ControlError> {
    let doc: SubscribePlanDoc = parse(payload, ConfigDomain::SubscribePlan)?;
    if doc.shard_size == 0 {
        return Err(ControlError::validation("shard_size 必须 >= 1（09 §5.1）"));
    }
    if doc.instruments.is_empty() {
        return Err(ControlError::validation(
            "SubscribePlan 域白名单为空：没有可指派的标的",
        ));
    }
    let mut seen = BTreeSet::new();
    for ins in &doc.instruments {
        if ins.venue.is_empty() || ins.market.is_empty() || ins.native_code.is_empty() {
            return Err(ControlError::validation(format!(
                "白名单标的字段不完整：{ins:?}"
            )));
        }
        if !seen.insert(ins.instrument_id()) {
            return Err(ControlError::validation(format!(
                "白名单标的重复：{}",
                ins.instrument_id()
            )));
        }
    }
    for q in &doc.quota {
        for (name, v) in [
            ("collect", q.collect),
            ("backfill", q.backfill),
            ("metadata", q.metadata),
        ] {
            if !(0.0..=1.0).contains(&v) || v.is_nan() {
                return Err(ControlError::validation(format!(
                    "quota[{venue}].{name} = {v} 不在 [0,1]",
                    venue = q.venue
                )));
            }
        }
    }
    Ok(())
}

fn validate_signal_rules(payload: &Value) -> Result<(), ControlError> {
    let doc: SignalRulesDoc = parse(payload, ConfigDomain::SignalRules)?;
    if doc.rules.is_empty() {
        return Err(ControlError::validation("SignalRules 域规则为空"));
    }
    let mut ids = BTreeSet::new();
    for r in &doc.rules {
        if r.signal_id.is_empty() {
            return Err(ControlError::validation("signal_id 不能为空"));
        }
        if !ids.insert(r.signal_id.clone()) {
            return Err(ControlError::validation(format!(
                "signal_id 重复：{}",
                r.signal_id
            )));
        }
        check_dag(&r.graph, &r.signal_id)?;
        // emit.value 必须指向图内节点（或输入主题）
        if !r.graph.iter().any(|n| n.id == r.emit.value)
            && !r.inputs.iter().any(|i| i == &r.emit.value)
        {
            return Err(ControlError::validation(format!(
                "规则 {} 的 emit.value `{}` 既不是图节点也不是输入主题",
                r.signal_id, r.emit.value
            )));
        }
        for p in &r.emit.payload {
            if !r.graph.iter().any(|n| &n.id == p) && !r.inputs.iter().any(|i| i == p) {
                return Err(ControlError::validation(format!(
                    "规则 {} 的 emit.payload 引用了不存在的字段 `{}`",
                    r.signal_id, p
                )));
            }
        }
        if let Some(d) = &r.emit.direction {
            for (branch, cond) in [("long", &d.long), ("short", &d.short)] {
                if let Some(c) = cond {
                    check_cond(c, &format!("{}.direction.{}", r.signal_id, branch))?;
                }
            }
            if let Some(def) = &d.default {
                if !matches!(def.as_str(), "Long" | "Short" | "Flat" | "long" | "short" | "flat") {
                    return Err(ControlError::validation(format!(
                        "规则 {} 的 direction.default = `{def}` 非法（Long / Short / Flat）",
                        r.signal_id
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_alert_rules(payload: &Value, store: &dyn ConfigStore) -> Result<(), ControlError> {
    let doc: AlertRulesDoc = parse(payload, ConfigDomain::AlertRules)?;
    if doc.alerts.is_empty() {
        return Err(ControlError::validation("AlertRules 域规则为空"));
    }

    // 跨域引用：route.channel ∈ Channels
    let channel_ids: BTreeSet<String> = store
        .current(ConfigDomain::Channels)?
        .and_then(|c| serde_json::from_value::<ChannelsDoc>(c.content).ok())
        .map(|d| d.channels.into_iter().map(|c| c.id).collect())
        .unwrap_or_default();

    // 跨域引用：match.signal_id ∈ SignalRules
    let signal_ids: BTreeSet<String> = store
        .current(ConfigDomain::SignalRules)?
        .and_then(|c| serde_json::from_value::<SignalRulesDoc>(c.content).ok())
        .map(|d| d.rules.into_iter().map(|r| r.signal_id).collect())
        .unwrap_or_default();

    let mut ids = BTreeSet::new();
    for a in &doc.alerts {
        if a.rule_id.is_empty() {
            return Err(ControlError::validation("rule_id 不能为空"));
        }
        if !ids.insert(a.rule_id.clone()) {
            return Err(ControlError::validation(format!(
                "rule_id 重复：{}",
                a.rule_id
            )));
        }
        check_cond(&a.when, &format!("{}.when", a.rule_id))?;

        if let Some(m) = &a.match_spec {
            if let Some(sid) = &m.signal_id {
                if !signal_ids.is_empty() && !signal_ids.contains(sid) {
                    return Err(ControlError::validation(format!(
                        "告警 {} 引用了不存在的 signal_id `{sid}`（09 §4.3 跨域引用）",
                        a.rule_id
                    )));
                }
            }
        }
        for r in &a.route {
            if !channel_ids.is_empty() && !channel_ids.contains(&r.channel) {
                return Err(ControlError::validation(format!(
                    "告警 {} 引用了不存在的渠道 `{}`（09 §4.3 跨域引用）",
                    a.rule_id, r.channel
                )));
            }
        }
        if let Some(w) = &a.window {
            if w.occurrences == 0 {
                return Err(ControlError::validation(format!(
                    "告警 {} 的 window.occurrences 必须 >= 1",
                    a.rule_id
                )));
            }
            if parse_duration_ms(&w.within).is_none() {
                return Err(ControlError::validation(format!(
                    "告警 {} 的 window.within `{}` 不是合法时长（如 10m）",
                    a.rule_id, w.within
                )));
            }
        }
        if let Some(t) = &a.throttle {
            if parse_duration_ms(t).is_none() {
                return Err(ControlError::validation(format!(
                    "告警 {} 的 throttle `{t}` 不是合法时长",
                    a.rule_id
                )));
            }
        }
        if let Some(l) = &a.level {
            if !matches!(l.as_str(), "Info" | "Warn" | "Critical" | "info" | "warn" | "critical") {
                return Err(ControlError::validation(format!(
                    "告警 {} 的 level = `{l}` 非法",
                    a.rule_id
                )));
            }
        }
    }
    Ok(())
}

fn validate_factor_rules(payload: &Value) -> Result<(), ControlError> {
    let doc: FactorRulesDoc = parse(payload, ConfigDomain::FactorRules)?;
    if doc.factors.is_empty() {
        return Err(ControlError::validation("FactorRules 域规则为空"));
    }
    let mut ids = BTreeSet::new();
    for f in &doc.factors {
        if !ids.insert(f.factor_id.clone()) {
            return Err(ControlError::validation(format!(
                "factor_id 重复：{}",
                f.factor_id
            )));
        }
        if f.inputs.is_empty() {
            return Err(ControlError::validation(format!(
                "因子 {} 的 inputs 为空",
                f.factor_id
            )));
        }
        if parse_duration_ms(&f.window).is_none() {
            return Err(ControlError::validation(format!(
                "因子 {} 的 window `{}` 不是合法时长",
                f.factor_id, f.window
            )));
        }
        check_dag(&f.compute, &f.factor_id)?;
        if !f.compute.iter().any(|n| n.id == f.emit.value)
            && !f.inputs.iter().any(|i| i == &f.emit.value)
        {
            return Err(ControlError::validation(format!(
                "因子 {} 的 emit.value `{}` 既不是计算节点也不是输入通道",
                f.factor_id, f.emit.value
            )));
        }
    }
    Ok(())
}

fn validate_channels(payload: &Value) -> Result<(), ControlError> {
    let doc: ChannelsDoc = parse(payload, ConfigDomain::Channels)?;
    if doc.channels.is_empty() {
        return Err(ControlError::validation("Channels 域渠道为空"));
    }
    let mut ids = BTreeSet::new();
    for c in &doc.channels {
        if !ids.insert(c.id.clone()) {
            return Err(ControlError::validation(format!(
                "渠道 id 重复：{}",
                c.id
            )));
        }
        if c.endpoint.is_empty() {
            return Err(ControlError::validation(format!(
                "渠道 {} 的 endpoint 为空",
                c.id
            )));
        }
        if let Some(r) = &c.retry {
            if parse_duration_ms(&r.backoff).is_none() {
                return Err(ControlError::validation(format!(
                    "渠道 {} 的 retry.backoff `{}` 不是合法时长",
                    c.id, r.backoff
                )));
            }
        }
    }
    Ok(())
}

fn validate_process_capacity(payload: &Value) -> Result<(), ControlError> {
    let doc: ProcessCapacityDoc = parse(payload, ConfigDomain::ProcessCapacity)?;
    Strategy::parse(&doc.distribution.strategy)?;
    let l = &doc.limits;
    if l.max_shards_per_process == 0
        || l.max_instruments_per_process == 0
        || l.max_channels_per_process == 0
    {
        return Err(ControlError::validation(
            "ProcessCapacity 三项上限必须全部 >= 1（09 §5.2）",
        ));
    }
    if !matches!(doc.distribution.order.as_str(), "round_robin" | "first_fit") {
        return Err(ControlError::validation(format!(
            "distribution.order = `{}` 不受支持（09 §3.5: round_robin）",
            doc.distribution.order
        )));
    }
    Ok(())
}

fn validate_serialization(payload: &Value) -> Result<(), ControlError> {
    let doc: SerializationDoc = parse(payload, ConfigDomain::Serialization)?;
    if doc.families.is_empty() {
        return Err(ControlError::validation("Serialization 域 families 为空"));
    }
    for (fam, fmt) in &doc.families {
        if !fam.ends_with(".*") {
            return Err(ControlError::validation(format!(
                "主题族 `{fam}` 必须形如 `md.*`（09 §3.5，粒度 = 主题族）"
            )));
        }
        if !SWITCHABLE_FORMATS.contains(&fmt.as_str()) {
            return Err(ControlError::validation(format!(
                "主题族 `{fam}` 的格式 `{fmt}` 非法（json / messagepack / protobuf）"
            )));
        }
        // 09 §2.1 / 03 §6：ctl.* 与 ops.* 固定 JSON，不开放切换（引导面不能依赖尚未拉到的配置）
        if (fam == "ctl.*" || fam == "ops.*") && fmt != "json" {
            return Err(ControlError::validation(format!(
                "主题族 `{fam}` 固定 JSON，不开放切换（03 §6 / 09 §2.1）"
            )));
        }
    }
    Ok(())
}

fn validate_blocklist(payload: &Value) -> Result<(), ControlError> {
    let doc: BlocklistDoc = parse(payload, ConfigDomain::InstrumentBlocklist)?;
    let mut seen = BTreeSet::new();
    for e in &doc.entries {
        let parts: Vec<&str> = e.instrument.split(':').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
            return Err(ControlError::validation(format!(
                "黑名单标的 `{}` 不是 canonical（venue:market:native_code，02 §3）",
                e.instrument
            )));
        }
        if !seen.insert(e.instrument.clone()) {
            return Err(ControlError::validation(format!(
                "黑名单标的重复：{}",
                e.instrument
            )));
        }
        if let Some(r) = &e.reason {
            if !BLOCK_REASONS.contains(&r.as_str()) {
                return Err(ControlError::validation(format!(
                    "黑名单 `{}` 的 reason = `{r}` 非法（{}）",
                    e.instrument,
                    BLOCK_REASONS.join(" / ")
                )));
            }
        }
    }
    Ok(())
}

/// DAG 结构检查（09 §9-4：`in` 引用存在 + 无环）。
fn check_dag(nodes: &[GraphArg], owner: &str) -> Result<(), ControlError> {
    let ids: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    if ids.len() != nodes.len() {
        return Err(ControlError::validation(format!(
            "{owner} 的 graph 节点 id 有重复"
        )));
    }
    for n in nodes {
        for up in &n.incoming {
            if !ids.contains(up.as_str()) {
                return Err(ControlError::validation(format!(
                    "{owner} 的节点 `{}` 引用了不存在的上游 `{up}`",
                    n.id
                )));
            }
        }
    }
    // Kahn 拓扑排序判环：正向出边 + 入度表
    let mut outdeg: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut indegree: BTreeMap<&str, usize> = nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    for n in nodes {
        for up in &n.incoming {
            outdeg.entry(up.as_str()).or_default().push(n.id.as_str());
            *indegree.entry(n.id.as_str()).or_insert(0) += 1;
        }
    }
    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut visited = 0usize;
    while let Some(cur) = queue.pop() {
        visited += 1;
        for next in outdeg.get(cur).into_iter().flatten() {
            if let Some(d) = indegree.get_mut(next) {
                *d -= 1;
                if *d == 0 {
                    queue.push(next);
                }
            }
        }
    }
    if visited != nodes.len() {
        return Err(ControlError::validation(format!(
            "{owner} 的 graph 存在环（DAG 要求，09 §3.2 / §9-4）"
        )));
    }
    Ok(())
}

fn check_cond(c: &Cond, at: &str) -> Result<(), ControlError> {
    if !c.non_empty() {
        return Err(ControlError::validation(format!("{at}：条件为空或字段为空")));
    }
    if !c.ops_valid() {
        return Err(ControlError::validation(format!(
            "{at}：操作符不合法（09 §3.3 允许 > >= < <= == !=）"
        )));
    }
    Ok(())
}
