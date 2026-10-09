//! SQLite 持久层（`docs/12-控制面数据库.md`）。
//!
//! 控制面的持久层由**文件树**升级为 **SQLite（WAL）**：配置版本、兜底快照键、
//! 派生计划快照都在同一库内，一次事务保证一致性（`docs/09 §4.2` 要求
//! "先写 meta → 同一事务写订阅集 + 递增版本"）。
//!
//! ## 与文件树实现的关系
//!
//! [`SqliteStore`] 实现与 [`crate::store::FileConfigStore`] **同一个 `ConfigStore` trait**：
//! 后者继续存在，用于（a）从文件树导入（`docs/12 §6` 迁移步骤 2-3）、
//! （b）切换期影子比对、（c）把配置导出成节点配置文件（`docs/02 §7.3`）。
//!
//! ## 硬约束
//!
//! * **回滚 = 发布新版本**（`docs/09 §4.5`）：`config_version` 只追加，`current_version`
//!   单向前进，收敛判据不出现歧义。
//! * **同名重登 = 覆盖 `node_runtime` 本行**（`docs/02 §7.4`）：保留
//!   `last_assignment_version` 继续递增，刷新 `host` / `last_seen_ts`。
//! * **写入串行**：单写多读（WAL），进程内 `Mutex` 串行化，保证"版本号 = 当前 + 1"。

use std::path::Path;
use std::sync::Mutex;

use ponk_protocol::{canonical_view, hash_text, ConfigDomain, SubscribePlan};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use crate::error::ControlError;
use crate::state::SnapshotKey;
use crate::store::{AuditEntry, ConfigStore, StoredDoc};

/// 枚举 → 线格式字符串（复用 serde 的 `rename_all`，不在两处重复维护字符串字面量）。
fn serde_to_str<T: serde::Serialize>(v: &T) -> Result<String, ControlError> {
    serde_json::to_value(v)?
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| ControlError::Illegal("enum did not serialize to a string".into()))
}

/// SQLite 迁移脚本（全量 DDL，`docs/12 §3`）。
const MIGRATION_0001: &str = include_str!("../migrations/0001_init.sql");

/// 迁移容错：`CREATE TABLE IF NOT EXISTS` **不会给已存在的表补列**，
/// 早期建过的库（或 DDL 演进后）会缺列。这里按 `(表, 列, DDL)` 幂等补齐。
const COLUMN_FIXES: &[(&str, &str, &str)] = &[
    (
        "node_runtime",
        "node_kind",
        "TEXT NOT NULL DEFAULT 'collector'",
    ),
    ("node_runtime", "host", "TEXT"),
    // `docs/12 §3.1` / `docs/14 §4`：交易所资产类（stock → 该所交易对跳过链上币信息）
    (
        "exchange",
        "asset_class",
        "TEXT NOT NULL DEFAULT 'crypto'",
    ),
    // `docs/12 §3.2`：`asset` 的八列（外部源 / 匹配状态 / 发行流通 / 原始响应）
    ("asset", "external_id", "TEXT"),
    (
        "asset",
        "match_status",
        "TEXT NOT NULL DEFAULT 'not_found'",
    ),
    ("asset", "twitter_url", "TEXT"),
    ("asset", "total_supply", "TEXT"),
    ("asset", "circulating_supply", "TEXT"),
    ("asset", "detail_source", "TEXT"),
    ("asset", "detail_synced_at", "INTEGER"),
    ("asset", "extra", "TEXT NOT NULL DEFAULT '{}'"),
];

fn ensure_columns(conn: &Connection) -> rusqlite::Result<()> {
    for (table, col, ddl) in COLUMN_FIXES {
        let present: bool = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|name| name == *col);
        if !present {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {ddl}"), [])?;
        }
    }
    Ok(())
}

/// 指派快照行（`docs/12 §3.6` 的 `node_assignment`，`docs/13 §4.5 / §4.11` 读取面）。
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRow {
    /// 实例名 = `collector_node.name`。
    pub instance: String,
    /// 所属节点 id。
    pub node_id: i64,
    /// 指派版本（单调递增，旧版丢弃）。
    pub assignment_version: i64,
    /// `snapshot` / `diff`。
    pub mode: String,
    /// `SubscribePlan` JSON。
    pub plan: Value,
    /// 逐所配额分摊（JSON）。
    pub quota_splits: Value,
    pub generated_at: u64,
}

fn decode_plan_row(row: &Row) -> rusqlite::Result<PlanRow> {
    let plan: String = row.get(4)?;
    let splits: String = row.get(5)?;
    Ok(PlanRow {
        instance: row.get(0)?,
        node_id: row.get(1)?,
        assignment_version: row.get(2)?,
        mode: row.get(3)?,
        plan: serde_json::from_str(&plan).unwrap_or(Value::Null),
        quota_splits: serde_json::from_str(&splits).unwrap_or_else(|_| serde_json::json!({})),
        generated_at: row.get::<_, i64>(6)? as u64,
    })
}

/// SQLite 存储（`docs/12 §1.2`：WAL + 单写多读）。
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// 打开（不存在则创建），执行迁移，设置必要 PRAGMA。
    ///
    /// `path` 可以是目录（则使用 `<dir>/control.db`）或直接指向 `.db` 文件。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ControlError> {
        let path = path.as_ref();
        let db_path = if path.is_dir() || path.extension().is_none() {
            if path.extension().is_none() {
                std::fs::create_dir_all(path)?;
            }
            path.join("control.db")
        } else {
            path.to_path_buf()
        };
        let conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(MIGRATION_0001)?;
        ensure_columns(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 在锁内执行一次写事务。
    pub(crate) fn with_write<F, T>(&self, f: F) -> Result<T, ControlError>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T>,
        T: Default,
    {
        let mut guard = self.lock()?;
        f(&mut guard)
            .map_err(|e| ControlError::Illegal(format!("sqlite: {e}")))
    }

    /// 在锁内只读执行。
    pub(crate) fn with_read<F, T>(&self, f: F) -> Result<T, ControlError>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T>,
        T: Default,
    {
        let guard = self.lock()?;
        f(&guard).map_err(|e| ControlError::Illegal(format!("sqlite: {e}")))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ControlError> {
        self.conn
            .lock()
            .map_err(|_| ControlError::Illegal("sqlite lock poisoned".into()))
    }

    // ── 快照键（`docs/12 §3.6 node_runtime`）───────────────────────────────

    /// 写入一个实例的运行时状态（register / 心跳 / 下线都会调用）。
    ///
    /// **同名重登**：`instance_id` 是主键 → 覆盖；调用方（`state.rs::on_register`）
    /// 已负责保留 `last_assignment_version` 并清除失联状态（`docs/02 §7.4`）。
    ///
    /// `host` 只在非空时覆盖（实例迁移到新机器才变，`docs/09 §2.3`）。
    pub fn save_key(&self, key: &SnapshotKey) -> Result<(), ControlError> {
        let counters = serde_json::to_string(&key.counters)?;
        let status = serde_to_str(&key.status)?;
        let kind = serde_to_str(&key.node_kind)?;
        self.with_write(|c| {
            c.execute(
                "INSERT INTO node_runtime
                   (instance_id, node_kind, host, status, applied_version,
                    last_assignment_version, counters, started_at, last_seen_ts,
                    lost_count, unconverged_since, last_repush_ts)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                 ON CONFLICT(instance_id) DO UPDATE SET
                   node_kind = excluded.node_kind,
                   host = CASE WHEN excluded.host IS NOT NULL THEN excluded.host
                               ELSE node_runtime.host END,
                   status = excluded.status,
                   applied_version = excluded.applied_version,
                   last_assignment_version = excluded.last_assignment_version,
                   counters = excluded.counters,
                   started_at = excluded.started_at,
                   last_seen_ts = excluded.last_seen_ts,
                   lost_count = excluded.lost_count,
                   unconverged_since = excluded.unconverged_since,
                   last_repush_ts = excluded.last_repush_ts",
                params![
                    key.instance_id,
                    kind,
                    key.host,
                    status,
                    key.applied_version.map(|v| v as i64),
                    key.last_assignment_version.map(|v| v as i64),
                    counters,
                    key.started_at as i64,
                    key.last_seen_ts as i64,
                    key.lost_count as i64,
                    key.unconverged_since.map(|v| v as i64),
                    key.last_repush_ts.map(|v| v as i64),
                ],
            )?;
            Ok(())
        })
        .map(|_| ())
    }

    /// 读回全部实例状态（控制面重启后的兜底快照，`docs/09 §6.3`）。
    ///
    /// 反序列化走「按列组装 JSON → `serde` 一次成型」，避免在两处重复维护
    /// 枚举与字段的映射（改枚举只改这里）。
    pub fn load_keys(&self) -> Result<Vec<SnapshotKey>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT instance_id, node_kind, host, status, applied_version,
                        last_assignment_version, counters, started_at, last_seen_ts,
                        lost_count, unconverged_since, last_repush_ts
                 FROM node_runtime ORDER BY instance_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                    row.get::<_, Option<i64>>(11)?,
                ))
            })?;
            let mut out: Vec<SnapshotKey> = Vec::new();
            for r in rows {
                let (id, kind, host, status, applied, last_assign, counters, started, seen, lost, unc, repush) = r?;
                let counters_value: Value = serde_json::from_str(&counters)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                let mut key: SnapshotKey = serde_json::from_value(serde_json::json!({
                    "instance_id": id,
                    "node_kind": Value::String(kind),
                    "host": host,
                    "status": Value::String(status),
                    "applied_version": applied,
                    "last_assignment_version": last_assign,
                    "counters": counters_value,
                    "started_at": started,
                    "last_seen_ts": seen,
                    "lost_count": lost,
                    "unconverged_since": unc,
                    "last_repush_ts": repush,
                }))
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                // `reported` 由 counters 派生（与 `state.rs::from_heartbeat` 同一口径）
                key.reported = crate::assign::ReportedCapacity::from_counters(&key.counters);
                out.push(key);
            }
            Ok(out)
        })
    }

    // ── 派生计划快照（`docs/12 §3.6 node_assignment`）──────────────────────

    /// 保存某实例最新下发的计划（版本单调递增）。
    ///
    /// 计划属派生物（`docs/09 §3.1`）：节点定义若不存在，**自动登记一个最小节点行**
    /// （名称 = instance_id），使运行中的实例在库内可查——操作者可在控制面补全
    /// 定义（范围、期望机器），但**即使无人补全也不影响指派**。
    pub fn save_plan(
        &self,
        instance: &str,
        plan: &SubscribePlan,
        quota_splits: &str,
        generated_at: u64,
    ) -> Result<(), ControlError> {
        let json = serde_json::to_string(plan)?;
        let mode = match plan.mode {
            ponk_protocol::PlanMode::Snapshot => "snapshot",
            ponk_protocol::PlanMode::Diff => "diff",
        };
        self.with_write(|c| {
            c.execute(
                "INSERT INTO collector_node
                   (name, enabled, scope_rules, runtime_params, created_at, updated_at)
                 VALUES (?1, 1, '{}', '{}', ?2, ?2)
                 ON CONFLICT(name) DO UPDATE SET updated_at = excluded.updated_at",
                params![instance, generated_at as i64],
            )?;
            c.execute(
                "INSERT INTO node_assignment
                   (node_id, assignment_version, mode, plan, quota_splits, generated_at)
                 SELECT id, ?2, ?3, ?4, ?5, ?6 FROM collector_node WHERE name = ?1
                 ON CONFLICT(node_id, assignment_version) DO UPDATE SET
                   mode = excluded.mode, plan = excluded.plan,
                   quota_splits = excluded.quota_splits,
                   generated_at = excluded.generated_at",
                params![
                    instance,
                    plan.assignment_version as i64,
                    mode,
                    json,
                    quota_splits,
                    generated_at as i64
                ],
            )?;
            Ok(())
        })
        .map(|_| ())
    }

    /// 读回所有实例的历史计划（按版本升序；调用方取每个实例的最高版本）。
    pub fn load_plans(&self) -> Result<Vec<(String, SubscribePlan)>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT cn.name, na.plan FROM node_assignment na
                 JOIN collector_node cn ON cn.id = na.node_id
                 ORDER BY cn.name, na.assignment_version",
            )?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get(1)?)))?;
            let mut out: Vec<(String, SubscribePlan)> = Vec::new();
            for r in rows {
                let (name, plan_json): (String, String) = r?;
                let plan = serde_json::from_str::<SubscribePlan>(&plan_json)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                out.push((name, plan));
            }
            Ok(out)
        })
    }

    // ── 按节点 / 版本查计划快照（`docs/13 §4.5 / §4.11`：分页与单版本查询）────

    /// 全部计划快照行（按实例、版本升序；`docs/12 §3.6` 的 `node_assignment`）。
    pub fn list_plan_rows(&self) -> Result<Vec<PlanRow>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT cn.name, na.node_id, na.assignment_version, na.mode, na.plan,
                        na.quota_splits, na.generated_at
                 FROM node_assignment na
                 JOIN collector_node cn ON cn.id = na.node_id
                 ORDER BY cn.name, na.assignment_version",
            )?;
            let rows = stmt.query_map([], decode_plan_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 某实例某个版本的快照（`GET /assignments/{instance}/versions/{v}`）。
    pub fn plan_row(
        &self,
        instance: &str,
        version: i64,
    ) -> Result<Option<PlanRow>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT cn.name, na.node_id, na.assignment_version, na.mode, na.plan,
                        na.quota_splits, na.generated_at
                 FROM node_assignment na
                 JOIN collector_node cn ON cn.id = na.node_id
                 WHERE cn.name = ?1 AND na.assignment_version = ?2",
            )?;
            let mut rows = stmt.query_map(params![instance, version], decode_plan_row)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 某实例的**最高**版本快照（`GET /assignments/{instance}`）。
    pub fn latest_plan_row(&self, instance: &str) -> Result<Option<PlanRow>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT cn.name, na.node_id, na.assignment_version, na.mode, na.plan,
                        na.quota_splits, na.generated_at
                 FROM node_assignment na
                 JOIN collector_node cn ON cn.id = na.node_id
                 WHERE cn.name = ?1
                 ORDER BY na.assignment_version DESC LIMIT 1",
            )?;
            let mut rows = stmt.query_map(params![instance], decode_plan_row)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 导入一个域版本（迁移用，`docs/12 §6` 步骤 2）：**不重算版本号**，
    /// 直接按给定版本写入；重复调用被主键冲突跳过，保持幂等。
    pub fn import_version(
        &self,
        domain: ConfigDomain,
        doc: &StoredDoc,
        current: u64,
    ) -> Result<(), ControlError> {
        let content = serde_json::to_string(&doc.content)?;
        let hash = hash_text(&canonical_view(&doc.content));
        let actor = doc.publisher.clone();
        let reason = doc.reason.clone();
        // 导入路径没有"上一版内容"的上下文，变更摘要记为导入标记（真实 diff 在
        // 后续 publish 时才可比）。
        let diff = vec!["<imported>".to_string()];
        let audit = AuditEntry {
            domain,
            version: doc.version,
            prev_version: doc.prev_version,
            publisher: doc.publisher.clone(),
            ts: doc.ts,
            reason: doc.reason.clone(),
            diff,
        };
        let audit_json = serde_json::to_string(&audit)?;
        self.with_write(|c| {
            c.execute(
                "INSERT INTO config_domain (domain, current_version, updated_at)
                 VALUES (?1, ?2, ?3) ON CONFLICT(domain) DO UPDATE SET
                   current_version = excluded.current_version,
                   updated_at = excluded.updated_at",
                params![domain.as_str(), current as i64, doc.ts as i64],
            )?;
            c.execute(
                "INSERT INTO config_version
                   (domain, version, content, content_hash, publisher, published_at, reason, prev_version)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(domain, version) DO NOTHING",
                params![
                    domain.as_str(),
                    doc.version as i64,
                    content,
                    hash,
                    actor,
                    doc.ts as i64,
                    reason,
                    doc.prev_version.map(|v| v as i64)
                ],
            )?;
            c.execute(
                "INSERT INTO audit_log (actor, action, target, after_hash, detail, ts)
                 VALUES (?1, 'publish', ?2, ?3, ?4, ?5)",
                params![actor, domain.as_str(), hash, audit_json, doc.ts as i64],
            )?;
            Ok(())
        })
        .map(|_| ())
    }

    /// 运行时状态行数（运维与测试用）。
    pub fn runtime_count(&self) -> Result<u64, ControlError> {
        self.with_read(|c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM node_runtime", [], |r| r.get(0))?;
            Ok(n as u64)
        })
    }
}

impl ConfigStore for SqliteStore {
    fn current_version(&self, domain: ConfigDomain) -> Result<Option<u64>, ControlError> {
        self.with_read(|c| {
            c.query_row(
                "SELECT current_version FROM config_domain WHERE domain = ?1",
                params![domain.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map(|v: Option<i64>| v.map(|n| n as u64).filter(|&n| n > 0))
        })
    }

    fn current(&self, domain: ConfigDomain) -> Result<Option<StoredDoc>, ControlError> {
        match self.current_version(domain)? {
            Some(v) => self.get(domain, v),
            None => Ok(None),
        }
    }

    fn get(&self, domain: ConfigDomain, version: u64) -> Result<Option<StoredDoc>, ControlError> {
        self.with_read(|c| {
            let row = c
                .query_row(
                    "SELECT content, publisher, published_at, reason, prev_version
                     FROM config_version WHERE domain = ?1 AND version = ?2",
                    params![domain.as_str(), version as i64],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )
                .optional()?;
            match row {
                None => Ok(None),
                Some((content, publisher, ts, reason, prev)) => {
                    let content: Value = serde_json::from_str(&content)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    Ok(Some(StoredDoc {
                        domain,
                        version,
                        publisher,
                        ts: ts as u64,
                        reason,
                        prev_version: prev.map(|v| v as u64),
                        content,
                    }))
                }
            }
        })
    }

    fn audit(&self, domain: ConfigDomain) -> Result<Vec<AuditEntry>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT detail FROM audit_log
                 WHERE action IN ('publish','rollback') AND target = ?1
                 ORDER BY ts, id",
            )?;
            let rows = stmt.query_map(params![domain.as_str()], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                let json: String = r?;
                out.push(serde_json::from_str::<AuditEntry>(&json)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?);
            }
            Ok(out)
        })
    }

    fn publish(
        &self,
        domain: ConfigDomain,
        mut content: Value,
        publisher: &str,
        reason: &str,
    ) -> Result<StoredDoc, ControlError> {
        let mut guard = self.lock()?;
        let tx = guard.transaction()?;
        let prev: Option<i64> = tx
            .query_row(
                "SELECT current_version FROM config_domain WHERE domain = ?1",
                params![domain.as_str()],
                |r| r.get(0),
            )
            .optional()?
            .filter(|&v| v > 0);
        let version = prev.map(|v| v as u64 + 1).unwrap_or(1);
        let prev_content: Option<String> = match prev {
            Some(p) => tx
                .query_row(
                    "SELECT content FROM config_version WHERE domain = ?1 AND version = ?2",
                    params![domain.as_str(), p],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };

        // `docs/09 §3.2`：`version` 由控制面在发布时赋值（提交方自带的值不作数）。
        if let Value::Object(ref mut map) = content {
            map.insert("version".to_string(), Value::from(version));
        }

        let ts = crate::now_ms();
        let diff = match prev_content.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok())
        {
            Some(p) => crate::store::diff_summary(&p, &content, 32),
            None => vec!["<initial>".to_string()],
        };
        let hash = hash_text(&canonical_view(&content));
        let content_json = serde_json::to_string(&content)?;

        tx.execute(
            "INSERT INTO config_domain (domain, current_version, updated_at)
             VALUES (?1, ?2, ?3) ON CONFLICT(domain) DO UPDATE SET
               current_version = excluded.current_version,
               updated_at = excluded.updated_at",
            params![domain.as_str(), version as i64, ts as i64],
        )?;
        tx.execute(
            "INSERT INTO config_version
               (domain, version, content, content_hash, publisher, published_at, reason, prev_version)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                domain.as_str(),
                version as i64,
                content_json,
                hash,
                publisher,
                ts as i64,
                reason,
                prev
            ],
        )?;

        let entry = AuditEntry {
            domain,
            version,
            prev_version: prev.map(|v| v as u64),
            publisher: publisher.to_string(),
            ts,
            reason: reason.to_string(),
            diff,
        };
        tx.execute(
            "INSERT INTO audit_log (actor, action, target, after_hash, detail, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                publisher,
                if reason.starts_with("rollback to") {
                    "rollback"
                } else {
                    "publish"
                },
                domain.as_str(),
                hash,
                serde_json::to_string(&entry)?,
                ts as i64
            ],
        )?;
        tx.commit()?;

        Ok(StoredDoc {
            domain,
            version,
            publisher: publisher.to_string(),
            ts,
            reason: reason.to_string(),
            prev_version: prev.map(|v| v as u64),
            content,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let s = SqliteStore::open(dir.path()).expect("open");
        (s, dir)
    }

    #[test]
    fn opens_creates_schema_and_is_idempotent() {
        let (s, _d) = store();
        assert_eq!(s.runtime_count().unwrap(), 0);
        let again = SqliteStore::open(_d.path()).expect("reopen");
        assert_eq!(again.runtime_count().unwrap(), 0);
    }

    #[test]
    fn publish_is_monotonic_and_current_moves_forward() {
        let (s, _d) = store();
        assert_eq!(s.current_version(ConfigDomain::AlertRules).unwrap(), None);
        let d1 = s
            .publish(ConfigDomain::AlertRules, serde_json::json!({"rules": []}), "cp/alice", "init")
            .unwrap();
        assert_eq!(d1.version, 1);
        let d2 = s
            .publish(ConfigDomain::AlertRules, serde_json::json!({"rules": [1]}), "cp/alice", "add")
            .unwrap();
        assert_eq!(d2.version, 2);
        assert_eq!(s.current_version(ConfigDomain::AlertRules).unwrap(), Some(2));
        assert_eq!(s.get(ConfigDomain::AlertRules, 1).unwrap().unwrap().version, 1);
    }

    #[test]
    fn rollback_publishes_a_new_version_without_going_back() {
        let (s, _d) = store();
        s.publish(ConfigDomain::Channels, serde_json::json!({"a": 1}), "cp/alice", "v1").unwrap();
        s.publish(ConfigDomain::Channels, serde_json::json!({"a": 2}), "cp/alice", "v2").unwrap();
        s.rollback(ConfigDomain::Channels, 1, "cp/alice").unwrap();
        assert_eq!(s.current_version(ConfigDomain::Channels).unwrap(), Some(3));
        let cur = s.current(ConfigDomain::Channels).unwrap().unwrap();
        assert_eq!(cur.content["a"], 1, "回滚取到 v1 的内容");
        assert_eq!(cur.version, 3, "版本号继续前进（09 §4.5）");
        assert_eq!(cur.reason, "rollback to v1");
        let log = s.audit(ConfigDomain::Channels).unwrap();
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn audit_records_diff_and_publisher() {
        let (s, _d) = store();
        s.publish(ConfigDomain::SignalRules, serde_json::json!({"x": 1}), "cp/bob", "first")
            .unwrap();
        s.publish(ConfigDomain::SignalRules, serde_json::json!({"x": 2}), "cp/bob", "second")
            .unwrap();
        let log = s.audit(ConfigDomain::SignalRules).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].publisher, "cp/bob");
        assert!(log[1].diff.iter().any(|d| d.contains('x')), "{:?}", log[1].diff);
    }

    #[test]
    fn current_versions_feeds_convergence_counters() {
        let (s, _d) = store();
        s.publish(ConfigDomain::SubscribePlan, serde_json::json!({}), "cp/a", "x").unwrap();
        let m = s.current_versions().unwrap();
        assert_eq!(m.get("cfg.subscribe_plan"), Some(&1));
        assert_eq!(m.get("cfg.alert_rules"), Some(&0));
    }
}
