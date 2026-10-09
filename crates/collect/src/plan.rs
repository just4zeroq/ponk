//! 订阅计划的应用（10 §4.3 版本强序、09 §4.1/§5.4 下发语义）。
//!
//! # 语义：状态补丁，不是动作流（10 §4.3）
//!
//! 命令信道是 at-least-once 且**无 ack**（03 §4.4），同一条计划可能重复投递、
//! 也可能乱序抵达（推 + 拉双路径）。因此采集侧对每份计划只做一件事：
//!
//! * `assignment_version <= applied` → **整条丢弃**（旧版 / 重复）；
//! * `assignment_version >  applied` → **整包应用**（不做增量合并）。
//!
//! 心跳里的 `applied_version` 就是隐式 ACK（10 §4.3）：控制面据此判收敛，
//! 不收敛超时则重推。采集侧**不需要**为计划回任何确认消息。
//!
//! # 与本地配置的关系
//!
//! 控制面未接入时，计划来自本地文件（[`crate::config::CollectorConfig::load`]）。
//! 接入后，**控制面是计划的事实来源**；本地文件只保留**运行参数**
//! （发布、降级、重连、心跳、配额桶、控制信道本身），计划部分由控制面下发覆盖。
//! [`to_config`] 就按这个分界合并：以本地配置为底，覆盖 `assignment_version` /
//! `mode` / `shards` / `quota_share`。

use std::sync::Arc;

use ponk_protocol::{ConfigDomain, CtlMessage, PlanMode, SubscribePlan};
use tokio::sync::Mutex;

/// 计划状态的共享句柄（采集主循环与命令信道客户端共用同一份）。
pub type SharedPlan = Arc<Mutex<PlanState>>;

use crate::channel::Channel;
use crate::config::{CollectorConfig, InstrumentConfig, QuotaShareConfig, ShardConfig};
use crate::error::{Error, Result};

/// 一次计划应用的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 版本更新：整包应用。
    Applied {
        /// 新的生效版本（= 控制面下发的 `assignment_version`）。
        version: u64,
        /// 分片集合是否发生变化（决定要不要重建订阅集；**不是**增量合并）。
        changed: bool,
    },
    /// `version <= applied`：丢弃（旧版或重复投递）。
    Ignored {
        /// 本次到达的版本。
        incoming: u64,
        /// 当前已生效版本。
        applied: u64,
    },
}

/// 采集侧计划状态（10 §4.3 强序门在采集侧的镜像）。
#[derive(Debug, Clone, Default)]
pub struct PlanState {
    applied: u64,
    plan: Option<SubscribePlan>,
}

impl PlanState {
    /// 以「已应用版本」初始化（本地文件启动时取文件里的 `assignment_version`）。
    pub fn new(applied: u64) -> Self {
        Self { applied, plan: None }
    }

    /// 当前生效版本（心跳 `applied_version` 的来源）。
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// 当前生效计划。
    pub fn current(&self) -> Option<&SubscribePlan> {
        self.plan.as_ref()
    }

    /// 裁决并应用一份计划；返回结果供调用方决定是否重建订阅集。
    pub fn accept(&mut self, plan: SubscribePlan) -> ApplyOutcome {
        if plan.assignment_version <= self.applied {
            return ApplyOutcome::Ignored {
                incoming: plan.assignment_version,
                applied: self.applied,
            };
        }
        let changed = match &self.plan {
            Some(prev) => signature(prev) != signature(&plan),
            None => true,
        };
        self.applied = plan.assignment_version;
        self.plan = Some(plan);
        ApplyOutcome::Applied {
            version: self.applied,
            changed,
        }
    }
}

/// 从命令信道消息里取出订阅计划（09 §2.2 B）。
///
/// * `CtlMessage::SubscribePlan` —— 订阅计划域的强类型特例（定向推 / 拉取应答都可能用它）；
/// * `CtlMessage::ConfigSnapshot { domain: SubscribePlan, payload }` —— 通用拉取应答，
///   负载是 JSON 文档，此处按域反序列化；
/// * 其余域 / 通知类消息 → `None`（本模块只认计划）。
pub fn plan_from_ctl(msg: &CtlMessage) -> Result<Option<SubscribePlan>> {
    match msg {
        CtlMessage::SubscribePlan(p) => Ok(Some(p.clone())),
        CtlMessage::ConfigSnapshot { domain, payload, .. }
            if *domain == ConfigDomain::SubscribePlan =>
        {
            let plan: SubscribePlan = serde_json::from_value(payload.as_json().clone())
                .map_err(|e| Error::Config(format!("ConfigSnapshot 负载不是订阅计划: {e}")))?;
            Ok(Some(plan))
        }
        _ => Ok(None),
    }
}

/// 计划签名：分片集合（venue/market/shard_idx/instrument 与通道）的稳定表示。
///
/// 只用于回答「订阅集变了吗」——变了才重建连接监督任务。
/// 与 09 §5.4 的 `signature` 同口径：**不含** `quota_share` 与版本号，
/// 因为它们的变化不影响订阅集。
pub fn signature(plan: &SubscribePlan) -> String {
    let mut parts: Vec<String> = plan
        .shards
        .iter()
        .map(|s| {
            let mut codes: Vec<String> = s
                .instruments
                .iter()
                .filter(|i| i.enabled)
                .map(|i| {
                    let mut ch: Vec<&str> = i.channels.iter().map(|c| c.as_str()).collect();
                    ch.sort_unstable();
                    format!("{}[{}]", i.native_code, ch.join(","))
                })
                .collect();
            codes.sort();
            format!(
                "{}:{}:{}={}",
                s.venue.as_str(),
                s.market.as_str(),
                s.shard_idx,
                codes.join("|")
            )
        })
        .collect();
    parts.sort();
    parts.join(";")
}

/// 以本地运行参数为底、用控制面下发的计划覆盖计划部分（见模块头）。
pub fn to_config(base: &CollectorConfig, plan: &SubscribePlan) -> Result<CollectorConfig> {
    let mut cfg = base.clone();
    cfg.assignment_version = plan.assignment_version;
    cfg.mode = match plan.mode {
        PlanMode::Snapshot => "snapshot".to_string(),
        PlanMode::Diff => "diff".to_string(),
    };
    cfg.shards = plan
        .shards
        .iter()
        .map(|s| {
            let instruments = s
                .instruments
                .iter()
                .map(|m| {
                    let mut channels = Vec::new();
                    for c in &m.channels {
                        // 未知通道名**报错**而不是静默丢弃：控制面下发了采集器不认识的通道，
                        // 静默丢弃会让「配了却收不到」重现（03 §5.2 的登记表就是防这个）。
                        let ch = Channel::from_protocol(c).ok_or_else(|| {
                            Error::Config(format!("未知通道 `{}`（02 §2 通道表）", c.as_str()))
                        })?;
                        channels.push(ch.as_str().to_string());
                    }
                    Ok(InstrumentConfig {
                        native_code: m.native_code.clone(),
                        base: m.meta.get("base").cloned().unwrap_or_default(),
                        quote: m.meta.get("quote").cloned().unwrap_or_default(),
                        price_precision: Some(m.price_precision),
                        amount_precision: Some(m.qty_precision),
                        tick_size: m.meta.get("tick_size").cloned(),
                        lot_size: m.meta.get("lot_size").cloned(),
                        channels,
                        enabled: m.enabled,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(ShardConfig {
                venue: s.venue.as_str().to_string(),
                market: s.market.as_str().to_string(),
                shard_idx: s.shard_idx,
                // 通道集按标的各自给出（03 §5.3 `InstrumentMeta::channels`），
                // 分片级保持为空 —— `resolve()` 会按标的求并集。
                channels: Vec::new(),
                instruments,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    cfg.quota_share = plan.quota_share.as_ref().map(|q| QuotaShareConfig {
        share_bp: Some((q.collect.clamp(0.0, 1.0) * 10_000.0).round() as u32),
        weight_per_min: None,
        egress_ip: Some(q.egress_ip.clone()),
    });
    Ok(cfg)
}

/// 供本 crate 其他模块的测试构造共享计划状态（仅测试编译）。
#[cfg(test)]
pub fn plan_state_for_test(applied: u64) -> SharedPlan {
    Arc::new(Mutex::new(PlanState::new(applied)))
}

/// 供本 crate 其他模块的测试构造计划（仅测试编译）。
#[cfg(test)]
pub fn plan_for_test(version: u64, code: &str, channels: &[&str]) -> SubscribePlan {
    tests::plan(version, code, channels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ponk_protocol::{Channel as ProtoChannel, InstrumentMeta, Market, ShardPlan, Venue};
    use std::collections::BTreeMap;

    /// 本地运行参数底稿（计划部分随后由控制面覆盖）。
    fn base_config() -> CollectorConfig {
        CollectorConfig::from_toml(
            r#"
instance_id = "col-1"
[[shards]]
venue = "binance"
market = "spot"
shard_idx = 0
channels = ["ticker"]
[[shards.instruments]]
native_code = "BTCUSDT"
"#,
        )
        .unwrap()
    }

    pub(super) fn plan(version: u64, code: &str, channels: &[&str]) -> SubscribePlan {
        SubscribePlan {
            assignment_version: version,
            mode: PlanMode::Snapshot,
            shards: vec![ShardPlan {
                venue: Venue::new("binance"),
                market: Market::new("spot"),
                shard_idx: 0,
                instruments: vec![InstrumentMeta {
                    native_code: code.to_string(),
                    price_precision: 2,
                    qty_precision: 6,
                    channels: channels.iter().map(|c| ProtoChannel::new(*c)).collect(),
                    enabled: true,
                    meta: BTreeMap::new(),
                }],
            }],
            quota_share: None,
        }
    }

    #[test]
    fn newer_version_is_applied_whole() {
        let mut st = PlanState::new(1);
        let out = st.accept(plan(2, "BTCUSDT", &["ticker"]));
        assert_eq!(
            out,
            ApplyOutcome::Applied {
                version: 2,
                changed: true
            }
        );
        assert_eq!(st.applied(), 2);
        assert!(st.current().is_some());
    }

    #[test]
    fn older_or_equal_version_is_dropped() {
        let mut st = PlanState::new(5);
        assert_eq!(
            st.accept(plan(5, "BTCUSDT", &["ticker"])),
            ApplyOutcome::Ignored {
                incoming: 5,
                applied: 5
            }
        );
        assert_eq!(
            st.accept(plan(3, "BTCUSDT", &["ticker"])),
            ApplyOutcome::Ignored {
                incoming: 3,
                applied: 5
            }
        );
        // 丢弃的旧版**不得**改动状态
        assert_eq!(st.applied(), 5);
        assert!(st.current().is_none(), "旧版不入库");
    }

    #[test]
    fn same_shards_new_version_reports_unchanged() {
        let mut st = PlanState::new(1);
        st.accept(plan(2, "BTCUSDT", &["ticker"]));
        let out = st.accept(plan(3, "BTCUSDT", &["ticker"]));
        assert_eq!(
            out,
            ApplyOutcome::Applied {
                version: 3,
                changed: false
            },
            "分片未变 → 不必重建订阅集，但版本仍要前进"
        );
        assert_eq!(st.applied(), 3);
    }

    #[test]
    fn channel_change_is_detected() {
        let mut st = PlanState::new(1);
        st.accept(plan(2, "BTCUSDT", &["ticker"]));
        let out = st.accept(plan(3, "BTCUSDT", &["ticker", "l1"]));
        assert!(matches!(out, ApplyOutcome::Applied { changed: true, .. }));
    }

    #[test]
    fn plan_from_ctl_reads_both_variants() {
        let direct = CtlMessage::SubscribePlan(plan(4, "ETHUSDT", &["trade"]));
        assert_eq!(
            plan_from_ctl(&direct).unwrap().unwrap().assignment_version,
            4
        );

        let payload = ponk_protocol::ConfigPayload::from_subscribe_plan(&plan(9, "ETHUSDT", &["trade"]))
            .unwrap();
        let snap = CtlMessage::ConfigSnapshot {
            domain: ConfigDomain::SubscribePlan,
            version: 9,
            payload,
        };
        let parsed = plan_from_ctl(&snap).unwrap().unwrap();
        assert_eq!(parsed.assignment_version, 9);
        assert_eq!(parsed.shards[0].instruments[0].native_code, "ETHUSDT");
    }

    #[test]
    fn plan_from_ctl_ignores_other_domains() {
        let payload = ponk_protocol::ConfigPayload::new(serde_json::json!({"a": 1}));
        let other = CtlMessage::ConfigSnapshot {
            domain: ConfigDomain::InstrumentBlocklist,
            version: 1,
            payload,
        };
        assert!(plan_from_ctl(&other).unwrap().is_none());
    }

    #[test]
    fn to_config_maps_plan_and_keeps_runtime_params() {
        let base = base_config();
        let mut p = plan(7, "BTCUSDT", &["candle_1m", "ticker"]);
        p.quota_share = Some(ponk_protocol::QuotaShare {
            venue: Venue::new("binance"),
            egress_ip: "10.0.0.1".into(),
            collect: 0.5,
            backfill: 0.35,
            metadata: 0.15,
            version: 7,
        });
        let merged = to_config(&base, &p).unwrap();
        assert_eq!(merged.instance_id, "col-1", "运行参数与实例标识保持本地值");
        assert_eq!(merged.assignment_version, 7);
        assert_eq!(merged.mode, "snapshot");
        assert_eq!(merged.shards.len(), 1);
        assert_eq!(merged.shards[0].venue, "binance");
        assert_eq!(merged.shards[0].instruments[0].native_code, "BTCUSDT");
        assert_eq!(
            merged.shards[0].instruments[0].channels,
            vec!["candle_1m".to_string(), "ticker".to_string()]
        );
        assert_eq!(
            merged.quota_share.as_ref().unwrap().share_bp,
            Some(5_000)
        );
        // 合并后的配置必须能解析（否则「应用成功但起不来」）
        merged.resolve().unwrap();
    }

    #[test]
    fn unknown_channel_is_rejected_not_silently_dropped() {
        let base = base_config();
        let err = to_config(&base, &plan(1, "BTCUSDT", &["no_such_channel"])).unwrap_err();
        assert!(err.to_string().contains("no_such_channel"), "{err}");
    }
}
