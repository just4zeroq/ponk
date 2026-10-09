//! 行情源抽象 + dig3 连接器适配（02 §1：**在 dig3 连接器层之上做平台适配**）。
//!
//! [`MarketSource`] 是采集器与 dig3 之间的唯一边界：
//! 采集器的重连状态机、降级轮询、发布管道全部只依赖这个 trait，
//! 因此可以用 [`MockSource`] **完全离线**地测（本机访问不了 binance/okx）。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use digdigdig3::connector_manager::ExchangeHub;
use digdigdig3::core::traits::{CoreConnector, WebSocketConnector};
use digdigdig3::core::types::{
    AccountType, ConnectionStatus, ExchangeId, Kline, StreamEvent, SubscriptionRequest, Symbol,
    SymbolInput, Ticker,
};
use futures_util::{Stream, StreamExt};

use crate::config::{ConnectionKey, Subscription};
use crate::error::{Error, Result};

/// 从行情源出来的消息：数据帧或连接级错误（02 §6 的检测信号）。
#[derive(Debug, Clone)]
pub enum SourceMsg {
    Event(StreamEvent),
    Error(String),
}

/// 订阅结果。
#[derive(Debug, Clone, Default)]
pub struct SubAck {
    /// 帧构建成功并入队的请求数。
    pub submitted: usize,
    /// 构建阶段就被拒绝的描述（`WsBatchAck::dropped`）。
    pub rejected: Vec<String>,
}

impl SubAck {
    pub fn is_fully_accepted(&self) -> bool {
        self.rejected.is_empty()
    }
}

/// 采集器眼中的行情源。
#[async_trait]
pub trait MarketSource: Send + Sync {
    /// 连接标识（`venue:market`）。
    fn key(&self) -> &ConnectionKey;

    /// 建立连接（REST + WS，02 §4 的 dig3 ExchangeHub 入口）。
    async fn connect(&self) -> Result<()>;

    /// （重）放订阅集（02 §6 步骤 2）。幂等：重复调用不产生重复数据。
    async fn subscribe(&self, subs: &[Subscription]) -> Result<SubAck>;

    /// 事件流。每次进入 `Running` 都应拿到一条新的流。
    fn event_stream(&self) -> Pin<Box<dyn Stream<Item = SourceMsg> + Send>>;

    /// 连接状态（`Disconnected / Connecting / Connected / Reconnecting`）。
    fn connection_status(&self) -> ConnectionStatus;

    /// REST：最新 24h ticker（降级用，02 §6）。`Ok(None)` = 该标的支持查询但无数据。
    async fn rest_ticker(&self, native_code: &str) -> Result<Option<Ticker>>;

    /// REST：最近的 1m K 线（**降级用，不是回补**——调用方按
    /// [`crate::degrade::DegradePlan::earliest_allowed_bar_open`] 过滤，02 §6 步骤 3）。
    async fn rest_kline_1m_latest(&self, native_code: &str) -> Result<Vec<Kline>>;
}

// ─────────────────────────────────────────────────────────────────────────────
// dig3 适配
// ─────────────────────────────────────────────────────────────────────────────

/// dig3 `ExchangeHub` 适配（02 §1「连接器层直接采用 digdigdig3」）。
///
/// **不依赖 `digdigdig3-station`**：station 原提供的自愈由本 crate 的
/// [`crate::reconnect`] 自建，落盘与 warm-start 归存储层（04）。
pub struct Dig3Source {
    key: ConnectionKey,
    exchange_id: ExchangeId,
    account_type: AccountType,
    hub: ExchangeHub,
    ws: Mutex<Option<Arc<dyn WebSocketConnector>>>,
}

impl Dig3Source {
    pub fn new(key: ConnectionKey, exchange_id: ExchangeId, account_type: AccountType, hub: ExchangeHub) -> Self {
        Self {
            key,
            exchange_id,
            account_type,
            hub,
            ws: Mutex::new(None),
        }
    }

    pub fn hub(&self) -> &ExchangeHub {
        &self.hub
    }

    fn ws_conn(&self) -> Result<Arc<dyn WebSocketConnector>> {
        self.ws
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| Error::connector(self.key.venue.clone(), "WS 连接尚未建立"))
    }

    fn rest_conn(&self) -> Result<Arc<dyn CoreConnector>> {
        self.hub.rest(self.exchange_id).ok_or_else(|| {
            Error::connector(self.key.venue.clone(), "REST 连接尚未建立")
        })
    }
}

#[async_trait]
impl MarketSource for Dig3Source {
    fn key(&self) -> &ConnectionKey {
        &self.key
    }

    async fn connect(&self) -> Result<()> {
        // connect_full = REST + WS 一次接上（REST 是降级路径的前提）。
        self.hub
            .connect_full(self.exchange_id, &[self.account_type], false)
            .await
            .map_err(|e| Error::connector(self.key.venue.clone(), e))?;

        let ws = self
            .hub
            .ws(self.exchange_id, self.account_type)
            .ok_or_else(|| {
                Error::connector(
                    self.key.venue.clone(),
                    "hub 未给出 WS 连接器（该所在此 target 上不支持 WS）",
                )
            })?;
        // dig3 的 transport 是自主驱动的，connect() 是唤醒触发器（见 transport.rs）。
        ws.connect(self.account_type)
            .await
            .map_err(|e| Error::connector(self.key.venue.clone(), e))?;

        *self.ws.lock().unwrap_or_else(|e| e.into_inner()) = Some(ws);
        Ok(())
    }

    async fn subscribe(&self, subs: &[Subscription]) -> Result<SubAck> {
        let ws = self.ws_conn()?;
        let reqs: Vec<SubscriptionRequest> = subs
            .iter()
            .map(|s| {
                // dig3 明确「连接器只接受交易所原生字符串」——这里传原生代码，
                // canonical `venue:market:code` 只用于平台侧身份（02 §3）。
                let mut r = SubscriptionRequest::new(
                    Symbol::with_raw("", "", s.native_code.clone()),
                    s.channel.stream_type(),
                );
                r.account_type = self.account_type;
                r
            })
            .collect();
        if reqs.is_empty() {
            return Ok(SubAck::default());
        }
        let ack = ws
            .subscribe_batch(reqs)
            .await
            .map_err(|e| Error::subscribe(self.key.venue.clone(), e))?;
        Ok(SubAck {
            submitted: ack.submitted.len(),
            rejected: ack
                .dropped
                .iter()
                .map(|(_, e)| e.to_string())
                .collect(),
        })
    }

    fn event_stream(&self) -> Pin<Box<dyn Stream<Item = SourceMsg> + Send>> {
        let ws = match self.ws.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(ws) => ws,
            None => return Box::pin(futures_util::stream::empty()),
        };
        Box::pin(ws.event_stream().map(|item| match item {
            Ok(ev) => SourceMsg::Event(ev),
            Err(e) => SourceMsg::Error(e.to_string()),
        }))
    }

    fn connection_status(&self) -> ConnectionStatus {
        match self.ws.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(ws) => ws.connection_status(),
            None => ConnectionStatus::Disconnected,
        }
    }

    async fn rest_ticker(&self, native_code: &str) -> Result<Option<Ticker>> {
        let rest = self.rest_conn()?;
        match rest.get_ticker(SymbolInput::Raw(native_code), self.account_type).await {
            Ok(t) => Ok(Some(t)),
            Err(e) if is_empty_payload(&e) => Ok(None),
            Err(e) => Err(Error::connector(self.key.venue.clone(), e)),
        }
    }

    async fn rest_kline_1m_latest(&self, native_code: &str) -> Result<Vec<Kline>> {
        let rest = self.rest_conn()?;
        // limit=2：拿「上一根 + 当前根」，由调用方按不回补边界过滤。
        let bars = rest
            .get_klines(
                SymbolInput::Raw(native_code),
                "1m",
                Some(2),
                self.account_type,
                None,
            )
            .await
            .map_err(|e| Error::connector(self.key.venue.clone(), e))?;
        Ok(bars)
    }
}

fn is_empty_payload(e: &digdigdig3::core::types::ExchangeError) -> bool {
    let s = format!("{e:?}").to_ascii_lowercase();
    s.contains("notfound") || s.contains("not found") || s.contains("invalid symbol")
}

/// 一次性把 hub 接到若干 `(venue, market)` 上（02 §4：一个进程承载多个分片，可跨所）。
pub async fn build_sources(
    keys: &[ConnectionKey],
    venue_to_exchange: impl Fn(&str) -> Option<ExchangeId>,
    market_to_account: impl Fn(&str) -> Option<AccountType>,
) -> Result<HashMap<ConnectionKey, Arc<Dig3Source>>> {
    let hub = ExchangeHub::new();
    let mut out = HashMap::new();
    for key in keys {
        let ex = venue_to_exchange(&key.venue).ok_or_else(|| {
            Error::Config(format!("未知 venue `{}`", key.venue))
        })?;
        let at = market_to_account(&key.market).ok_or_else(|| {
            Error::Config(format!("未知 market `{}`", key.market))
        })?;
        out.insert(
            key.clone(),
            Arc::new(Dig3Source::new(key.clone(), ex, at, hub.clone())),
        );
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// 测试替身
// ─────────────────────────────────────────────────────────────────────────────

/// 离线测试用的行情源：**不碰网络**。
///
/// * `push` / `push_error` 注入事件与连接级错误；
/// * `connects` / `subscribes` / `subscribe_calls` 断言「重连后重放订阅集」；
/// * `fail_connect` / `fail_subscribe` 注入故障，驱动重连状态机；
/// * `set_status` 驱动 `Disconnected` 宽限期路径。
pub struct MockSource {
    key: ConnectionKey,
    connects: AtomicUsize,
    subscribes: AtomicUsize,
    fail_connect: AtomicBool,
    fail_subscribe: AtomicBool,
    subscribe_calls: Mutex<Vec<Vec<String>>>,
    status: Mutex<ConnectionStatus>,
    tx: tokio::sync::broadcast::Sender<SourceMsg>,
    rest_tickers: Mutex<HashMap<String, Ticker>>,
    rest_klines: Mutex<HashMap<String, Vec<Kline>>>,
    rest_ticker_calls: AtomicUsize,
}

impl MockSource {
    pub fn new(venue: &str, market: &str) -> Arc<Self> {
        let (tx, _) = tokio::sync::broadcast::channel(1024);
        Arc::new(Self {
            key: ConnectionKey::new(venue, market),
            connects: AtomicUsize::new(0),
            subscribes: AtomicUsize::new(0),
            fail_connect: AtomicBool::new(false),
            fail_subscribe: AtomicBool::new(false),
            subscribe_calls: Mutex::new(Vec::new()),
            status: Mutex::new(ConnectionStatus::Disconnected),
            tx,
            rest_tickers: Mutex::new(HashMap::new()),
            rest_klines: Mutex::new(HashMap::new()),
            rest_ticker_calls: AtomicUsize::new(0),
        })
    }

    pub fn push(&self, ev: StreamEvent) {
        let _ = self.tx.send(SourceMsg::Event(ev));
    }

    pub fn push_error(&self, msg: &str) {
        let _ = self.tx.send(SourceMsg::Error(msg.to_string()));
    }

    pub fn close_stream(&self) {
        // 关闭 broadcast → 所有 event_stream 读到结束。
        // broadcast 没有显式 close，故用 status + push_error 模拟；
        // 真正的流结束由 `Dig3Source` 在 WS 关闭时给出。
        self.set_status(ConnectionStatus::Disconnected);
        self.push_error("stream closed");
    }

    pub fn set_fail_connect(&self, v: bool) {
        self.fail_connect.store(v, Ordering::Relaxed);
    }

    pub fn set_fail_subscribe(&self, v: bool) {
        self.fail_subscribe.store(v, Ordering::Relaxed);
    }

    pub fn set_status(&self, s: ConnectionStatus) {
        *self.status.lock().unwrap() = s;
    }

    pub fn connect_count(&self) -> usize {
        self.connects.load(Ordering::Relaxed)
    }

    pub fn subscribe_count(&self) -> usize {
        self.subscribes.load(Ordering::Relaxed)
    }

    pub fn rest_ticker_call_count(&self) -> usize {
        self.rest_ticker_calls.load(Ordering::Relaxed)
    }

    /// 每次 `subscribe` 收到的标的集（断言重放用）。
    pub fn subscribe_calls(&self) -> Vec<Vec<String>> {
        self.subscribe_calls.lock().unwrap().clone()
    }

    pub fn set_rest_ticker(&self, native_code: &str, t: Ticker) {
        self.rest_tickers.lock().unwrap().insert(native_code.to_string(), t);
    }

    pub fn set_rest_klines(&self, native_code: &str, bars: Vec<Kline>) {
        self.rest_klines.lock().unwrap().insert(native_code.to_string(), bars);
    }

    pub fn handle(self: &Arc<Self>) -> Arc<dyn MarketSource> {
        let this: Arc<Self> = self.clone();
        this
    }
}

#[async_trait]
impl MarketSource for MockSource {
    fn key(&self) -> &ConnectionKey {
        &self.key
    }

    async fn connect(&self) -> Result<()> {
        self.connects.fetch_add(1, Ordering::Relaxed);
        if self.fail_connect.load(Ordering::Relaxed) {
            self.set_status(ConnectionStatus::Disconnected);
            return Err(Error::connector(self.key.venue.clone(), "injected connect failure"));
        }
        self.set_status(ConnectionStatus::Connected);
        Ok(())
    }

    async fn subscribe(&self, subs: &[Subscription]) -> Result<SubAck> {
        self.subscribes.fetch_add(1, Ordering::Relaxed);
        self.subscribe_calls.lock().unwrap().push(
            subs.iter()
                .map(|s| format!("{}:{}", s.channel, s.native_code))
                .collect(),
        );
        if self.fail_subscribe.load(Ordering::Relaxed) {
            return Err(Error::subscribe(self.key.venue.clone(), "injected subscribe failure"));
        }
        Ok(SubAck {
            submitted: subs.len(),
            rejected: Vec::new(),
        })
    }

    fn event_stream(&self) -> Pin<Box<dyn Stream<Item = SourceMsg> + Send>> {
        let rx = self.tx.subscribe();
        // 状态 = receiver 本身：每次推进都把 receiver 交还下去，保证不重复投递。
        Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(m) => return Some((m, rx)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        }))
    }

    fn connection_status(&self) -> ConnectionStatus {
        *self.status.lock().unwrap()
    }

    async fn rest_ticker(&self, native_code: &str) -> Result<Option<Ticker>> {
        self.rest_ticker_calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.rest_tickers.lock().unwrap().get(native_code).cloned())
    }

    async fn rest_kline_1m_latest(&self, native_code: &str) -> Result<Vec<Kline>> {
        Ok(self.rest_klines.lock().unwrap().get(native_code).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Channel;

    #[test]
    fn mock_status_defaults_to_disconnected() {
        let m = MockSource::new("binance", "spot");
        assert_eq!(m.connection_status(), ConnectionStatus::Disconnected);
        assert_eq!(m.connect_count(), 0);
    }

    #[tokio::test]
    async fn mock_connect_and_subscribe_are_counted() {
        let m = MockSource::new("binance", "spot");
        m.connect().await.unwrap();
        assert_eq!(m.connect_count(), 1);
        assert_eq!(m.connection_status(), ConnectionStatus::Connected);

        let subs = vec![Subscription {
            connection: ConnectionKey::new("binance", "spot"),
            shard_idx: 0,
            channel: Channel::Trade,
            native_code: "BTCUSDT".into(),
            instrument_id: "binance:spot:BTCUSDT".into(),
        }];
        let ack = m.subscribe(&subs).await.unwrap();
        assert_eq!(ack.submitted, 1);
        assert_eq!(m.subscribe_calls(), vec![vec!["trade:BTCUSDT".to_string()]]);
    }

    #[tokio::test]
    async fn mock_stream_delivers_pushed_events() {
        use digdigdig3::core::types::Ticker;
        let m = MockSource::new("binance", "spot");
        let mut st = m.event_stream();
        m.push(StreamEvent::Ticker {
            symbol: "BTCUSDT".into(),
            ticker: Ticker {
                last_price: 1.0,
                timestamp: 5,
                ..Default::default()
            },
        });
        m.push_error("boom");
        let first = futures_util::StreamExt::next(&mut st).await.unwrap();
        assert!(matches!(first, SourceMsg::Event(_)));
        let second = futures_util::StreamExt::next(&mut st).await.unwrap();
        assert!(matches!(second, SourceMsg::Error(_)));
    }
}
