//! 订阅式消费（03 §4.2）。
//!
//! > 两种消费方式**都支持**，且共享同一消费组会话：**订阅是拉取的语法糖**
//! > （内部为 `loop { session.next() -> handler }`），不是第二条通路。

use crate::error::ECError;
use crate::session::{EventCenter, SessionOpts};
use ponk_protocol::{Envelope, Group, Topic};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// 订阅句柄：控制后台 `loop { next -> handler }` 的生命周期。
pub struct SubscriptionHandle {
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    error: Arc<Mutex<Option<ECError>>>,
    thread: Option<JoinHandle<()>>,
}

impl SubscriptionHandle {
    /// 打开会话时的即时错误（`open` 在 `subscribe` 返回前同步执行）。
    pub fn open_error(&self) -> Option<ECError> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    /// 请求停止：下一次 `next` 返回后退出循环。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// 循环是否已结束（正常停止 / 打开会话失败 / handler panic）。
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    /// 循环期间遇到的错误（handler panic 或 backend 错误）。
    pub fn error(&self) -> Option<ECError> {
        self.error.lock().ok().and_then(|e| e.clone())
    }

    /// 停止并等待后台线程退出。
    pub fn join(mut self) -> Result<(), ECError> {
        self.stop.store(true, Ordering::SeqCst);
        self.join_inner()
    }

    fn join_inner(&mut self) -> Result<(), ECError> {
        if let Some(t) = self.thread.take() {
            match t.join() {
                Ok(()) => {}
                Err(_) => {
                    return Err(ECError::Backend(
                        "subscribe handler panicked".to_string(),
                    ))
                }
            }
        }
        if let Some(e) = self.error.lock().ok().and_then(|e| e.clone()) {
            return Err(e);
        }
        Ok(())
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.join_inner();
    }
}

/// 保证后台线程即使 panic 也会把 `finished` 置位（否则 join 方会一直认为还在跑）。
struct FinishGuard(Arc<AtomicBool>);
impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// 订阅 = 拉取的语法糖（03 §4.2）：内部为 `loop { session.next() -> handler }`。
///
/// `subscribe` **同步打开会话**（因此 `open` 的错误直接体现在
/// [`SubscriptionHandle::open_error`] 里），随后在后台线程跑消费循环。
///
/// ## 与规格的差异（必要补充）
///
/// 03 §4.2 写的是 `where C: EventCenter, H: FnMut(Envelope)`。
/// 把 handler 移入后台线程**必须**额外要求 `H: Send + 'static`——
/// 这是 Rust 的并发安全要求，不是语义变化；`C` 与返回签名与规格一致。
///
/// ## 语义
///
/// * 与 [`Session::next`] 共享**同一个消费组会话**（同一 `group`，同一位点）；
/// * 无 ack：读取即推进位点（03 §4.4）；
/// * `Ok(None)`（超时无消息）不结束循环，只继续等。
pub fn subscribe<C, H>(center: &C, topic: &Topic, group: &Group, handler: H) -> SubscriptionHandle
where
    C: EventCenter,
    H: FnMut(Envelope) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let error: Arc<Mutex<Option<ECError>>> = Arc::new(Mutex::new(None));

    let opts = SessionOpts::default();
    let mut session = match center.open(topic, group, opts.clone()) {
        Ok(s) => s,
        Err(e) => {
            if let Ok(mut slot) = error.lock() {
                *slot = Some(e);
            }
            finished.store(true, Ordering::SeqCst);
            return SubscriptionHandle {
                stop,
                finished,
                error,
                thread: None,
            };
        }
    };

    let t_stop = Arc::clone(&stop);
    let t_finished = Arc::clone(&finished);
    let t_error = Arc::clone(&error);
    let timeout: Duration = opts.timeout;

    let thread = std::thread::Builder::new()
        .name(format!("ec-subscribe-{topic}"))
        .spawn(move || {
            let _guard = FinishGuard(t_finished);
            let mut handler = handler;
            loop {
                if t_stop.load(Ordering::SeqCst) {
                    break;
                }
                match session.next(timeout) {
                    Ok(Some(env)) => handler(env),
                    Ok(None) => continue, // 超时无消息，非结束（03 §4.1）
                    Err(ECError::Closed) => break,
                    Err(e) => {
                        if let Ok(mut slot) = t_error.lock() {
                            *slot = Some(e);
                        }
                        break;
                    }
                }
            }
            let _ = session.close();
        })
        .expect("spawn subscribe thread");

    SubscriptionHandle {
        stop,
        finished,
        error,
        thread: Some(thread),
    }
}
