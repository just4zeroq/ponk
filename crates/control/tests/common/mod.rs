//! 测试公共设施：**离线**、无外部服务（无 Redis、无网络）。
//!
//! 每个测试用独立的临时目录做配置存储，用进程内内存 backend 做总线。

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// 唯一的临时存储目录（测试结束不留残留：由 `Cleanup` 自动删除）。
pub fn temp_store(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "ponk-control-{}-{}-{}-{}",
        tag,
        std::process::id(),
        n,
        nanos
    ));
    std::fs::create_dir_all(&dir).expect("create temp store dir");
    dir
}

/// 测试退出时删除临时目录。
pub struct Cleanup(PathBuf);

impl Cleanup {
    pub fn new(dir: PathBuf) -> Self {
        Cleanup(dir)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
