//! # ponk-collect — PONK 行情采集器（`docs/02-采集.md`）
//!
//! 本 crate 是平台与外部行情源的**唯一边界**（02 §1）：一切行情数据从这里进入事件中心。
//! 它**不重新实现连接器**，只在 digdigdig3 **连接器层**（`ExchangeHub` + REST/WS 连接器 +
//! core 类型）之上做平台适配：
//!
//! | 关注点 | 落地 |
//! |---|---|
//! | 本地引导配置 | [`config`]（TOML/JSON；结构对齐 03 §5.3 `SubscribePlan`/`ShardPlan`） |
//! | 订阅计划应用 | [`plan`]（版本强序 + Snapshot/Diff + 白名单执行） |
//! | 命令 / 控制信道 | [`command`](register → 拉取 → 持续监听，10 §7 启动时序） |
//! | 数据通道 | [`channel`]（`candle_1m` / `trade` / `ticker` / `l1` / `funding`） |
//! | 事件产出 | [`payload`] → [`envelope`]（`Envelope` / `Body::Market`） |
//! | 发布 / 订阅 | [`sink`]（`EventCenter::publish` + `EventCenter::open`） |
//! | 心跳 + 承载能力 | [`heartbeat`]（`ops.collector.<instance>`，10 §5） |
//! | 断线自愈 | [`reconnect`]（**只重连不回补**，02 §6） |
//! | REST 降级 | [`degrade`]（02 §6 三种降级策略） |
//! | 配额 | [`quota`]（**按 venue 各自的令牌桶**，02 §6，所间不串） |
//! | 在线计数器 | [`counters`]（02 §4：进程内只留计数器，**不建行情缓存**） |
//!
//! ## 硬约束（02 §4 / §6）
//!
//! * **无状态转发者**：进程内不做「最近 N 条」环形流水、不维护 per-symbol 最后值槽；
//!   只有在线计数器与连接级时钟（判定静默用，非行情缓存）。
//! * **断线只重连**：检测 → 重连 + 重放订阅集；**不实现任何 REST 回补路径**。
//!   断档的发现、标记与修复归未来的「数据检查与修复节点」（01 §7）。
//! * **不做背压**（10 §3）：队列满即丢并计数，积压由 06 观测。

pub mod channel;
pub mod command;
pub mod config;
pub mod counters;
pub mod degrade;
pub mod envelope;
pub mod error;
pub mod heartbeat;
pub mod payload;
pub mod plan;
pub mod quota;
pub mod reconnect;
pub mod sink;
pub mod source;

pub mod collector;

pub use channel::Channel;
pub use collector::Collector;
pub use config::{CollectorConfig, ResolvedPlan};
pub use counters::Counters;
pub use error::{Error, Result};
pub use plan::{ApplyOutcome, PlanState, SharedPlan};
pub use quota::QuotaRegistry;
pub use sink::{EventSink, RecordingSink};
