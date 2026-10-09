//! 配置存储（`docs/09-控制面.md` §4.2）：按域分目录的**追加式版本历史** +
//! `current` 指针 + 不可变内容 + 审计记录。
//!
//! ## 目录布局（本地文件树，不依赖任何数据库服务）
//!
//! ```text
//! <root>/
//!   <domain>/                     e.g. signal_rules/
//!     current.json                {"version": N}          ← current 指针
//!     v000001.json                StoredDoc（不可变）
//!     v000002.json
//!     audit.jsonl                 ← 追加式审计（改了什么 / 何时 / 发布者 / 原因）
//!   state/instances/<id>.json     ← 兜底快照键（09 §6.3，见 crate::state）
//! ```
//!
//! ## 回滚的硬约束（09 §4.5）
//!
//! 回滚 = **取历史版本内容重新发布为新版本号**（版本号继续单调），
//! **不是**把 `current` 指针倒回旧号——版本倒流会使收敛判据 `== 当前版本`
//! 出现「已收敛的实例反而显示落后」的歧义。回滚同样走发布全流程，产生
//! `reason: rollback to vN` 的新审计条目。

use crate::error::ControlError;
use ponk_protocol::ConfigDomain;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 一个已发布的域版本（内容 + 版本元数据）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredDoc {
    /// 配置域。
    pub domain: ConfigDomain,
    /// 域版本（单调递增，09 §4.1）。
    pub version: u64,
    /// 发布者（`控制面实例/操作者`，如 `cp-1/alice`，09 §3.1）。
    pub publisher: String,
    /// 发布时刻（ms）。
    pub ts: u64,
    /// 原因 / 工单号（09 §4.5 回滚写 `rollback to vN`）。
    pub reason: String,
    /// 上一版版本号（首版为 `None`）。
    pub prev_version: Option<u64>,
    /// 域文档本体。
    pub content: Value,
}

/// 不可变审计条目（09 §4.2）：域、版本、发布者、时间、变更摘要、原因。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// 配置域。
    pub domain: ConfigDomain,
    /// 本次发布的版本。
    pub version: u64,
    /// 上一版。
    pub prev_version: Option<u64>,
    /// 发布者。
    pub publisher: String,
    /// 发布时刻（ms）。
    pub ts: u64,
    /// 原因 / 工单号。
    pub reason: String,
    /// 与上一版的文档 diff 摘要（09 §4.2「变更摘要」）。
    pub diff: Vec<String>,
}

/// 配置存储抽象边界（09 §4.2：「按域取版本内容 / 写新版本 / 读历史」）。
///
/// 后端可为嵌入式数据库或版本化文件树；本 crate 提供文件树实现
/// [`FileConfigStore`]。
pub trait ConfigStore: Send + Sync {
    /// `current` 指针指向的版本号；该域尚无任何版本时为 `None`。
    fn current_version(&self, domain: ConfigDomain) -> Result<Option<u64>, ControlError>;

    /// 当前版本的内容与元数据。
    fn current(&self, domain: ConfigDomain) -> Result<Option<StoredDoc>, ControlError>;

    /// 指定历史版本的内容（不可变）。
    fn get(&self, domain: ConfigDomain, version: u64) -> Result<Option<StoredDoc>, ControlError>;

    /// 该域的完整审计历史（按版本升序）。
    fn audit(&self, domain: ConfigDomain) -> Result<Vec<AuditEntry>, ControlError>;

    /// 写一个新版本：版本号 = 当前 + 1（首版为 1），内容入历史，`current` 前移，写审计条目。
    fn publish(
        &self,
        domain: ConfigDomain,
        content: Value,
        publisher: &str,
        reason: &str,
    ) -> Result<StoredDoc, ControlError>;

    /// **回滚 = 版本历史恢复**（09 §4.5）：取 `target` 版本内容**重新发布为新版本**。
    ///
    /// 版本号继续单调；`current` 绝不倒回旧号。
    fn rollback(
        &self,
        domain: ConfigDomain,
        target: u64,
        publisher: &str,
    ) -> Result<StoredDoc, ControlError> {
        let src = self
            .get(domain, target)?
            .ok_or_else(|| ControlError::not_found(format!("{domain} v{target} 不存在")))?;
        let reason = format!("rollback to v{target}");
        self.publish(domain, src.content, publisher, &reason)
    }

    /// 八域的当前版本表（09 §4.4：控制面心跳 `counters["cfg.<domain>"]` 的来源）。
    fn current_versions(&self) -> Result<std::collections::BTreeMap<String, i64>, ControlError> {
        let mut out = std::collections::BTreeMap::new();
        for d in ConfigDomain::ALL {
            let v = self.current_version(d)?.unwrap_or(0);
            out.insert(d.counter_key(), v as i64);
        }
        Ok(out)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 文件树实现
// ─────────────────────────────────────────────────────────────────────────────

/// 版本化文件树存储（09 §4.2「版本化文件树」后端）。
///
/// 进程内以 `Mutex` 串行化写入，保证「版本号 = 当前 + 1」的原子递增。
pub struct FileConfigStore {
    root: PathBuf,
    lock: Mutex<()>,
}

impl FileConfigStore {
    /// 打开（不存在则创建目录树）。
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ControlError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        for d in ConfigDomain::ALL {
            fs::create_dir_all(root.join(d.as_str()))?;
        }
        Ok(FileConfigStore {
            root,
            lock: Mutex::new(()),
        })
    }

    /// 存储根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self, domain: ConfigDomain) -> PathBuf {
        self.root.join(domain.as_str())
    }

    fn current_path(&self, domain: ConfigDomain) -> PathBuf {
        self.dir(domain).join("current.json")
    }

    fn version_path(&self, domain: ConfigDomain, version: u64) -> PathBuf {
        self.dir(domain).join(format!("v{version:06}.json"))
    }

    fn audit_path(&self, domain: ConfigDomain) -> PathBuf {
        self.dir(domain).join("audit.jsonl")
    }

    fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, ControlError> {
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(path)?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// 原子写：先写 `.tmp` 再 rename，避免半截文件。
    fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), ControlError> {
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(value)?;
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CurrentPtr {
    version: u64,
}

impl ConfigStore for FileConfigStore {
    fn current_version(&self, domain: ConfigDomain) -> Result<Option<u64>, ControlError> {
        Ok(
            Self::read_json::<CurrentPtr>(&self.current_path(domain))?
                .map(|p| p.version),
        )
    }

    fn current(&self, domain: ConfigDomain) -> Result<Option<StoredDoc>, ControlError> {
        let Some(v) = self.current_version(domain)? else {
            return Ok(None);
        };
        self.get(domain, v)
    }

    fn get(&self, domain: ConfigDomain, version: u64) -> Result<Option<StoredDoc>, ControlError> {
        Self::read_json(&self.version_path(domain, version))
    }

    fn audit(&self, domain: ConfigDomain) -> Result<Vec<AuditEntry>, ControlError> {
        let path = self.audit_path(domain);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(path)?;
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            out.push(serde_json::from_str::<AuditEntry>(line)?);
        }
        out.sort_by_key(|e| e.version);
        Ok(out)
    }

    fn publish(
        &self,
        domain: ConfigDomain,
        mut content: Value,
        publisher: &str,
        reason: &str,
    ) -> Result<StoredDoc, ControlError> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| ControlError::Illegal("config store lock poisoned".into()))?;

        let prev = self.current_version(domain)?;
        let version = prev.map(|v| v + 1).unwrap_or(1);
        let prev_content = match prev {
            Some(p) => self.get(domain, p)?.map(|d| d.content),
            None => None,
        };

        // 09 §3.2：`version` 由控制面在发布时赋值（提交方自带的值不作数）。
        if let Value::Object(ref mut map) = content {
            map.insert("version".to_string(), Value::from(version));
        }

        let ts = crate::now_ms();
        let diff = match &prev_content {
            Some(p) => diff_summary(p, &content, 32),
            None => vec!["<initial>".to_string()],
        };

        let doc = StoredDoc {
            domain,
            version,
            publisher: publisher.to_string(),
            ts,
            reason: reason.to_string(),
            prev_version: prev,
            content,
        };

        // ① 内容入历史（不可变）
        Self::write_json_atomic(&self.version_path(domain, version), &doc)?;
        // ② current 指针前移
        Self::write_json_atomic(
            &self.current_path(domain),
            &CurrentPtr { version },
        )?;
        // ③ 审计条目
        let entry = AuditEntry {
            domain,
            version,
            prev_version: prev,
            publisher: publisher.to_string(),
            ts,
            reason: reason.to_string(),
            diff,
        };
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.audit_path(domain))?;
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        file.flush()?;

        Ok(doc)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 后端选择：SQLite 为默认，文件树保留用于导入 / 影子比对 / 导出
// ─────────────────────────────────────────────────────────────────────────────

/// 配置存储后端（`docs/12 §1.2`：**默认 SQLite**）。
///
/// 两种实现满足同一个 [`ConfigStore`]：
/// * [`SqliteStore`](crate::sqlite::SqliteStore) —— `ControlPlane::open` 使用的默认后端；
/// * [`FileConfigStore`] —— 用于（a）从文件树导入历史版本（`docs/12 §6` 步骤 2）、
///   （b）切换期影子比对、（c）把配置导出成节点配置文件（`docs/02 §7.3`）。
pub enum AnyConfigStore {
    /// 版本化文件树（兼容 / 导出）。
    File(FileConfigStore),
    /// SQLite（`docs/12`）——持有 `Arc` 以便与快照键注册表共用同一连接。
    Sqlite(std::sync::Arc<crate::sqlite::SqliteStore>),
}

impl AnyConfigStore {
    /// 与 `ControlPlane::open` **一致**的打开方式（CLI 查询必须与写入方看同一个库）。
    ///
    /// * `<root>/control.db` 存在或目录里没有文件树 → **SQLite**（默认）；
    /// * 库不存在但文件树在 → **文件树**（迁移期读旧数据，`docs/12 §6` 步骤 2）。
    pub fn open(root: impl AsRef<std::path::Path>) -> Result<Self, ControlError> {
        let root = root.as_ref();
        let db = root.join("control.db");
        let has_files = root
            .join(ponk_protocol::ConfigDomain::SubscribePlan.as_str())
            .exists();
        if !db.exists() && has_files {
            return Ok(AnyConfigStore::File(FileConfigStore::open(root)?));
        }
        Ok(AnyConfigStore::Sqlite(std::sync::Arc::new(
            crate::sqlite::SqliteStore::open(root)?,
        )))
    }

    /// SQLite 后端（用作运行时状态与计划快照的持久化目标）。
    pub fn sqlite(&self) -> Option<&std::sync::Arc<crate::sqlite::SqliteStore>> {
        match self {
            AnyConfigStore::Sqlite(s) => Some(s),
            AnyConfigStore::File(_) => None,
        }
    }
}

impl ConfigStore for AnyConfigStore {
    fn current_version(&self, domain: ConfigDomain) -> Result<Option<u64>, ControlError> {
        match self {
            AnyConfigStore::File(f) => f.current_version(domain),
            AnyConfigStore::Sqlite(s) => s.current_version(domain),
        }
    }
    fn current(&self, domain: ConfigDomain) -> Result<Option<StoredDoc>, ControlError> {
        match self {
            AnyConfigStore::File(f) => f.current(domain),
            AnyConfigStore::Sqlite(s) => s.current(domain),
        }
    }
    fn get(&self, domain: ConfigDomain, version: u64) -> Result<Option<StoredDoc>, ControlError> {
        match self {
            AnyConfigStore::File(f) => f.get(domain, version),
            AnyConfigStore::Sqlite(s) => s.get(domain, version),
        }
    }
    fn audit(&self, domain: ConfigDomain) -> Result<Vec<AuditEntry>, ControlError> {
        match self {
            AnyConfigStore::File(f) => f.audit(domain),
            AnyConfigStore::Sqlite(s) => s.audit(domain),
        }
    }
    fn publish(
        &self,
        domain: ConfigDomain,
        content: Value,
        publisher: &str,
        reason: &str,
    ) -> Result<StoredDoc, ControlError> {
        match self {
            AnyConfigStore::File(f) => f.publish(domain, content, publisher, reason),
            AnyConfigStore::Sqlite(s) => s.publish(domain, content, publisher, reason),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 变更摘要（09 §4.2「与上一版的文档 diff」）
// ─────────────────────────────────────────────────────────────────────────────

/// 递归结构 diff，产出最多 `limit` 条变更摘要路径。
///
/// 形如 `~ rules[0].emit.value`（改）、`+ rules[1]`（增）、`- channels[1]`（删）。
pub fn diff_summary(a: &Value, b: &Value, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    diff_walk(a, b, String::new(), &mut out, limit);
    out
}

fn diff_walk(a: &Value, b: &Value, path: String, out: &mut Vec<String>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    if a == b {
        return;
    }
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            for (k, av) in am {
                let p = join(&path, k);
                match bm.get(k) {
                    Some(bv) => diff_walk(av, bv, p, out, limit),
                    None => push(out, format!("- {p}"), limit),
                }
            }
            for (k, _) in bm {
                if !am.contains_key(k) {
                    push(out, format!("+ {}", join(&path, k)), limit);
                }
            }
        }
        (Value::Array(aa), Value::Array(ba)) => {
            for (i, av) in aa.iter().enumerate() {
                match ba.get(i) {
                    Some(bv) => diff_walk(av, bv, format!("{path}[{i}]"), out, limit),
                    None => push(out, format!("- {path}[{i}]"), limit),
                }
            }
            for i in aa.len()..ba.len() {
                push(out, format!("+ {path}[{i}]"), limit);
            }
        }
        _ => push(out, format!("~ {}", if path.is_empty() { "(root)" } else { &path }), limit),
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn push(out: &mut Vec<String>, item: String, limit: usize) {
    if out.len() < limit {
        out.push(item);
    }
}
