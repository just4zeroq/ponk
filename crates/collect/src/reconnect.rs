//! 重连状态机（02 §6「WS 断线处理：**只重连，不回补**」）。
//!
//! ```text
//!  1. 检测：连接级信号（WS `Err` / 流关闭 / 静默超时按连接分类 / 状态长时间 Disconnected）
//!  2. 恢复：重连 + `subscribe` 重放订阅集
//!  3. 不做数据回补 —— 断档区间不补（K 线留洞、trade/ticker 本就不可补）
//! ```
//!
//! 本模块是**纯状态机**：不含 IO、不含时钟（退避时长由它算出、由调用方 sleep），
//! 因此可以完全离线测试。
//!
//! **状态机里没有任何「回补 / backfill / REST 补历史」的动作变体**——这是刻意的：
//! 02 §6 明确把回补职责从采集侧整体删除（station 原 auto-heal 不重建），
//! 回补是数据质量职责，归未来的「数据检查与修复节点」（01 §7）。
//! 唯一的 REST 使用是 [02 §6 的**降级**]（实时轮询当前值），见 `degrade`，
//! 它不产生、也不允许产生断档区间内的事件。

/// 连接生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// 尚未启动。
    Idle,
    /// 正在建立连接。
    Connecting,
    /// 已连接，正在（重）放订阅集。
    Subscribing,
    /// 正常收流。
    Running,
    /// 断线退避中（此时降级轮询可启动，02 §6）。
    Backoff,
    /// 已停止（Shutdown 或重连次数耗尽）。
    Stopped,
}

/// 驱动状态机的事件（全部来自连接级信号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    Start,
    ConnectOk,
    ConnectErr,
    SubscribeOk,
    SubscribeErr,
    /// WS 流报 `Err`。
    StreamError,
    /// WS 流结束（`None`）。
    StreamClosed,
    /// 连接级静默超时（**不是**通道静默告警——后者由 06 观测 `md.*` 得出，02 §7.3）。
    Silence,
    /// 连续 `Disconnected` 超过宽限期。
    DisconnectedTooLong,
    BackoffElapsed,
    Shutdown,
}

/// 状态机要求调用方执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// 无事可做（正在收流）。
    Idle,
    /// 建立连接。
    Connect,
    /// 连接后**重放订阅集**（02 §6 步骤 2）。
    Resubscribe,
    /// 退避等待给定毫秒后发 `BackoffElapsed`。
    Wait(u64),
    /// 停止。
    Stop,
}

/// 纯重连状态机。
#[derive(Debug, Clone)]
pub struct ReconnectMachine {
    state: State,
    attempt: u32,
    backoff_min_ms: u64,
    backoff_max_ms: u64,
    /// `0` = 无限重试（02 §6「只重连」默认不放弃）。
    max_attempts: u32,
    /// 累计重连次数（进入 Backoff 计一次）。
    total_reconnects: u32,
    /// 是否已成功订阅过（决定降级是否允许启动）。
    ever_subscribed: bool,
}

impl ReconnectMachine {
    pub fn new(backoff_min_ms: u64, backoff_max_ms: u64, max_attempts: u32) -> Self {
        Self {
            state: State::Idle,
            attempt: 0,
            backoff_min_ms: backoff_min_ms.max(1),
            backoff_max_ms: backoff_max_ms.max(1),
            max_attempts,
            total_reconnects: 0,
            ever_subscribed: false,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// 当前重试序号（0 = 尚未失败过）。
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// 累计进入退避的次数。
    pub fn total_reconnects(&self) -> u32 {
        self.total_reconnects
    }

    /// 是否已至少成功订阅过一次。
    pub fn ever_subscribed(&self) -> bool {
        self.ever_subscribed
    }

    /// 当前退避时长（指数退避，封顶 `backoff_max_ms`）。
    ///
    /// `attempt = 1 → min`，之后每次翻倍，直到 `max`。
    pub fn backoff(&self) -> u64 {
        if self.attempt == 0 {
            return self.backoff_min_ms;
        }
        let shift = (self.attempt - 1).min(31);
        let raw = self.backoff_min_ms.saturating_mul(1u64 << shift);
        raw.min(self.backoff_max_ms)
    }

    /// 驱动一步，返回调用方应执行的动作。
    pub fn on(&mut self, ev: Event) -> Action {
        use Action::*;
        use Event::*;

        // 任何状态下 Shutdown 都立即停（10 §5：shutdown 心跳由上层发）。
        if matches!(ev, Shutdown) {
            self.state = State::Stopped;
            return Stop;
        }
        if self.state == State::Stopped {
            return Stop;
        }

        match (self.state, ev) {
            (State::Idle, Start) => {
                self.state = State::Connecting;
                Connect
            }

            (State::Connecting, ConnectOk) => {
                // 连上就必须重放订阅集（02 §6 步骤 2；首次连接同样是这一步）。
                self.state = State::Subscribing;
                Resubscribe
            }
            (State::Connecting, ConnectErr) => self.fail(),

            (State::Subscribing, SubscribeOk) => {
                self.state = State::Running;
                self.attempt = 0;
                self.ever_subscribed = true;
                Idle
            }
            (State::Subscribing, SubscribeErr) => self.fail(),

            (State::Running, StreamError) => self.fail(),
            (State::Running, StreamClosed) => self.fail(),
            (State::Running, Silence) => self.fail(),
            (State::Running, DisconnectedTooLong) => self.fail(),
            // 交易所异步拒绝订阅（含重连重放被拒）在**活连接**上也按连接级信号处理：
            // 恢复手段只有「重连 + 重放订阅集」（02 §6 步骤 2）。
            (State::Running, SubscribeErr) => self.fail(),

            (State::Backoff, BackoffElapsed) => {
                self.state = State::Connecting;
                Connect
            }

            // 退避期间又收到事件级信号：维持退避，不重复计数。
            (State::Backoff, _) => Wait(self.backoff()),

            // 状态不匹配的事件一律忽略（例如 Connecting 时收到 StreamError）。
            _ => Idle,
        }
    }

    /// 一次失败：`attempt += 1`，进入（或保持）退避。
    fn fail(&mut self) -> Action {
        self.attempt = self.attempt.saturating_add(1);
        if self.max_attempts > 0 && self.attempt >= self.max_attempts {
            self.state = State::Stopped;
            return Action::Stop;
        }
        if self.state != State::Backoff {
            self.total_reconnects = self.total_reconnects.saturating_add(1);
        }
        self.state = State::Backoff;
        Action::Wait(self.backoff())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> ReconnectMachine {
        ReconnectMachine::new(1_000, 30_000, 0)
    }

    #[test]
    fn happy_path_connects_then_replays_subscriptions() {
        let mut m = m();
        assert_eq!(m.state(), State::Idle);
        assert_eq!(m.on(Event::Start), Action::Connect);
        assert_eq!(m.state(), State::Connecting);
        assert_eq!(m.on(Event::ConnectOk), Action::Resubscribe);
        assert_eq!(m.state(), State::Subscribing);
        assert_eq!(m.on(Event::SubscribeOk), Action::Idle);
        assert_eq!(m.state(), State::Running);
        assert!(m.ever_subscribed());
        assert_eq!(m.total_reconnects(), 0);
    }

    #[test]
    fn connect_failure_backs_off_exponentially_and_caps() {
        let mut m = ReconnectMachine::new(1_000, 8_000, 0);
        m.on(Event::Start);
        let mut seen = Vec::new();
        for _ in 0..6 {
            if let Action::Wait(ms) = m.on(Event::ConnectErr) {
                seen.push(ms);
                m.on(Event::BackoffElapsed);
            } else {
                panic!("expected Wait");
            }
        }
        assert_eq!(seen, vec![1_000, 2_000, 4_000, 8_000, 8_000, 8_000]);
    }

    #[test]
    fn running_disconnection_replays_the_whole_subscription_set() {
        let mut m = m();
        m.on(Event::Start);
        m.on(Event::ConnectOk);
        m.on(Event::SubscribeOk);
        assert_eq!(m.state(), State::Running);

        // 断线 → 退避 → 重连 → **重放订阅集**
        let act = m.on(Event::StreamError);
        assert!(matches!(act, Action::Wait(_)), "got {:?}", act);
        assert_eq!(m.state(), State::Backoff);
        assert_eq!(m.total_reconnects(), 1);
        assert_eq!(m.on(Event::BackoffElapsed), Action::Connect);
        assert_eq!(m.on(Event::ConnectOk), Action::Resubscribe, "必须重放订阅集");
        assert_eq!(m.on(Event::SubscribeOk), Action::Idle);
        assert_eq!(m.state(), State::Running);
        // 重连成功后退避序号清零
        assert_eq!(m.attempt(), 0);
    }

    #[test]
    fn all_connection_level_signals_trigger_reconnect() {
        for ev in [
            Event::StreamError,
            Event::StreamClosed,
            Event::Silence,
            Event::DisconnectedTooLong,
        ] {
            let mut m = m();
            m.on(Event::Start);
            m.on(Event::ConnectOk);
            m.on(Event::SubscribeOk);
            let act = m.on(ev);
            assert!(
                matches!(act, Action::Wait(_)),
                "{:?} 应触发退避，got {:?}",
                ev,
                act
            );
        }
    }

    #[test]
    fn shutdown_wins_from_any_state() {
        for ev in [Event::Start, Event::ConnectOk, Event::SubscribeOk, Event::StreamError] {
            let mut m = m();
            m.on(Event::Start);
            if ev != Event::Start {
                m.on(ev);
            }
            assert_eq!(m.on(Event::Shutdown), Action::Stop);
            assert_eq!(m.state(), State::Stopped);
            // 停止后不再接受任何事件
            assert_eq!(m.on(Event::Start), Action::Stop);
        }
    }

    #[test]
    fn max_attempts_stops_instead_of_looping_forever() {
        let mut m = ReconnectMachine::new(10, 100, 3);
        m.on(Event::Start);
        assert!(matches!(m.on(Event::ConnectErr), Action::Wait(_)));
        m.on(Event::BackoffElapsed);
        assert!(matches!(m.on(Event::ConnectErr), Action::Wait(_)));
        m.on(Event::BackoffElapsed);
        assert_eq!(m.on(Event::ConnectErr), Action::Stop);
        assert_eq!(m.state(), State::Stopped);
    }

    #[test]
    fn zero_max_attempts_means_infinite_retry() {
        // 02 §6「只重连」：默认不放弃。
        let mut m = ReconnectMachine::new(1, 2, 0);
        m.on(Event::Start);
        for _ in 0..1000 {
            assert!(matches!(m.on(Event::ConnectErr), Action::Wait(_)));
            m.on(Event::BackoffElapsed);
        }
        // 永不进入 Stopped —— 02 §6「只重连」默认不放弃
        assert_ne!(m.state(), State::Stopped);
    }

    /// 02 §6 步骤 3：**不做数据回补**。
    ///
    /// 用一段脚本把状态机能产出的**全部**动作都跑出来，断言集合恰好是
    /// `{Connect, Wait, Resubscribe, Idle, Stop}` —— 里面没有任何回补动作，
    /// 调用方在类型层面就无法触发「补历史」。
    #[test]
    fn action_set_contains_no_backfill() {
        let mut m = ReconnectMachine::new(1_000, 1_000, 0);
        let mut seen = std::collections::BTreeSet::new();
        for ev in [
            Event::Start,
            Event::ConnectErr,
            Event::BackoffElapsed,
            Event::ConnectOk,
            Event::SubscribeOk,
            Event::StreamError,
            Event::BackoffElapsed,
            Event::ConnectOk,
            Event::SubscribeOk,
            Event::Shutdown,
        ] {
            seen.insert(format!("{:?}", m.on(ev)));
        }
        let names: Vec<String> = seen.into_iter().collect();
        assert_eq!(
            names,
            vec![
                "Connect".to_string(),
                "Idle".to_string(),
                "Resubscribe".to_string(),
                "Stop".to_string(),
                "Wait(1000)".to_string()
            ]
        );
        assert!(!names.iter().any(|n| n.contains("ackfill") || n.contains("Backfill")));
    }

    #[test]
    fn noise_during_backoff_does_not_double_count() {
        let mut m = m();
        m.on(Event::Start);
        m.on(Event::ConnectErr);
        let n = m.total_reconnects();
        m.on(Event::StreamError);
        m.on(Event::Silence);
        assert_eq!(m.total_reconnects(), n);
    }

    #[test]
    fn backoff_duration_is_at_least_min() {
        let m = ReconnectMachine::new(500, 5_000, 0);
        assert_eq!(m.backoff(), 500);
    }
}
