//! 发布出口：采集器 → `EventCenter::publish`（03 §4.1、10 §3 数据信道）。
//!
//! # 为什么要这层窄接口
//!
//! 采集器只用到 `EventCenter` 三个原语里的**一个**（`publish`）。把它窄化成
//! [`EventSink`] 有两个好处：
//!
//! 1. 单元测试可以注入 [`RecordingSink`]，**离线**跑完整条
//!    「WS 帧 → 样本 → 信封 → 发布」链路（本机访问不了 binance/okx，也没有本地 Redis）；
//! 2. 真实后端只需 [`CenterSink`] 十行适配，采集器不感知 backend 差异（03 §2）。
//!
//! 生产路径**确实**走 `EventCenter::publish`——见 [`CenterSink`]。
//!
//! # 失败与死信的分工
//!
//! 10 §3：「发送失败**重试**，最终失败进 `dlq.md.*`」。
//! 03 §4.1 对 `publish` 的定义是：「失败的事件会写入 `dlq.<topic>`（03 §7.3），
//! **调用方仍收到错误**」——即 **DLQ 写入是 backend 的职责**，采集器只负责
//! 重试 + 计数（`publish_failed`）。
//!
//! 采集器**不自行拼装 `DeadLetterMeta`**：03 §5.2 明确死信不承载于 `Envelope`，
//! 而 `publish` 只收 `Envelope`，由采集侧代写只会得到一条负载不符的死信。
//!
//! 已知边界：若 backend 因**自身不可用**而失败（非可控的序列化失败），
//! 它同样写不了 `dlq.<topic>`，该条即丢——这是 03 §7.3 在「backend 全挂」场景下的
//! 固有缺口，属 backend 契约问题，不由采集器补偿。

use std::sync::Mutex;

use ponk_eventcenter::EventCenter;
use ponk_protocol::{Envelope, Topic};

/// 采集器的发布出口。
pub trait EventSink: Send + Sync {
    /// 把信封写入 `topic`。失败由调用方重试；最终失败计数。
    fn publish(&self, topic: &Topic, envelope: &Envelope) -> std::result::Result<(), String>;
}

/// 生产适配：`EventSink` → `EventCenter::publish`（03 §4.1）。
pub struct CenterSink<C: EventCenter>(pub C);

impl<C: EventCenter> CenterSink<C> {
    pub fn new(center: C) -> Self {
        Self(center)
    }

    pub fn into_inner(self) -> C {
        self.0
    }
}

impl<C: EventCenter> EventSink for CenterSink<C> {
    fn publish(&self, topic: &Topic, envelope: &Envelope) -> std::result::Result<(), String> {
        self.0.publish(topic, envelope).map_err(|e| e.to_string())
    }
}

impl<C: EventCenter> EventSink for std::sync::Arc<C> {
    fn publish(&self, topic: &Topic, envelope: &Envelope) -> std::result::Result<(), String> {
        (**self).publish(topic, envelope).map_err(|e| e.to_string())
    }
}

/// 记录型出口：**离线测试**用，把 (topic, envelope) 收下来供断言。
#[derive(Default)]
pub struct RecordingSink {
    published: Mutex<Vec<(String, Envelope)>>,
    /// 注入的失败次数：前 N 次 `publish` 返回 Err，之后成功。
    fail_first: Mutex<u32>,
    /// 恒失败。
    always_fail: Mutex<bool>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// 让接下来的 `n` 次发布失败（测试重试逻辑）。
    pub fn fail_next(&self, n: u32) {
        *self.fail_first.lock().unwrap() = n;
    }

    /// 让所有发布失败（测试最终失败路径）。
    pub fn fail_always(&self) {
        *self.always_fail.lock().unwrap() = true;
    }

    pub fn count(&self) -> usize {
        self.published.lock().unwrap().len()
    }

    /// `(topic, envelope)` 列表，按发布顺序（topic 与 envelope 一一对应）。
    pub fn pairs(&self) -> Vec<(String, Envelope)> {
        self.published.lock().unwrap().clone()
    }

    pub fn topics(&self) -> Vec<String> {
        self.published
            .lock()
            .unwrap()
            .iter()
            .map(|(t, _)| t.clone())
            .collect()
    }

    pub fn envelopes(&self) -> Vec<Envelope> {
        self.published.lock().unwrap().iter().map(|(_, e)| e.clone()).collect()
    }

    pub fn clear(&self) {
        self.published.lock().unwrap().clear();
    }
}

impl EventSink for RecordingSink {
    fn publish(&self, topic: &Topic, envelope: &Envelope) -> std::result::Result<(), String> {
        {
            let always = self.always_fail.lock().unwrap();
            if *always {
                return Err("injected always-fail".into());
            }
        }
        {
            let mut n = self.fail_first.lock().unwrap();
            if *n > 0 {
                *n -= 1;
                return Err(format!("injected failure ({n} left)"));
            }
        }
        self.published
            .lock()
            .unwrap()
            .push((topic.as_str().to_string(), envelope.clone()));
        Ok(())
    }
}

/// 无总线时的出口：把信封序列化成 JSON 行打到 stdout。
///
/// 用于本地调试与「事件中心 backend 尚未落地」的过渡期（03 §9-3）。
/// 生产路径用 [`CenterSink`]（`EventCenter::publish`）。
pub struct StdoutSink {
    encode: Mutex<bool>,
}

impl StdoutSink {
    pub fn new() -> Self {
        Self {
            encode: Mutex::new(true),
        }
    }

    /// 关掉后只打印 `topic`（测试/静音用）。
    pub fn with_body(self, on: bool) -> Self {
        *self.encode.lock().unwrap() = on;
        self
    }
}

impl Default for StdoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for StdoutSink {
    fn publish(&self, topic: &Topic, envelope: &Envelope) -> std::result::Result<(), String> {
        if *self.encode.lock().unwrap() {
            let json = serde_json::to_string(envelope).map_err(|e| e.to_string())?;
            println!("{}	{}", topic.as_str(), json);
        } else {
            println!("{}", topic.as_str());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{heartbeat_envelope, market_envelope, market_topic};
    use crate::payload::MarketSample;
    use ponk_protocol::MarketData;

    fn sample() -> MarketSample {
        MarketSample {
            channel: crate::channel::Channel::Ticker,
            venue: "binance".into(),
            market: "spot".into(),
            native_code: "BTCUSDT".into(),
            exchange_ts: 1,
            data: MarketData::Ticker(Default::default()),
        }
    }

    #[test]
    fn recording_sink_captures_topic_and_envelope() {
        let sink = RecordingSink::new();
        let env = market_envelope(&sample(), 10, "c").unwrap();
        let topic = market_topic(&sample());
        sink.publish(&topic, &env).unwrap();
        assert_eq!(sink.count(), 1);
        assert_eq!(sink.topics(), vec!["md.ticker".to_string()]);
        assert_eq!(sink.envelopes()[0].event_id, env.event_id);
    }

    #[test]
    fn injected_failures_are_reported_to_caller() {
        let sink = RecordingSink::new();
        sink.fail_next(2);
        let env = market_envelope(&sample(), 10, "c").unwrap();
        let topic = market_topic(&sample());
        assert!(sink.publish(&topic, &env).is_err());
        assert!(sink.publish(&topic, &env).is_err());
        assert!(sink.publish(&topic, &env).is_ok());
        assert_eq!(sink.count(), 1);
    }

    #[test]
    fn heartbeat_goes_to_ops_topic() {
        let sink = RecordingSink::new();
        let env = heartbeat_envelope(
            "collector-01",
            None, // 上报机器名（可选，09 §2.3）
            ponk_protocol::Status::Startup,
            Some(1),
            Default::default(),
            0,
            1,
        )
        .unwrap();
        sink.publish(&crate::envelope::heartbeat_topic("collector-01"), &env)
            .unwrap();
        assert_eq!(sink.topics(), vec!["ops.collector.collector-01".to_string()]);
    }
}
