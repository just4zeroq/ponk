//! 服务器与采集节点的管理（`docs/12 §3.5 / §3.6`，对应 `docs/09 §2.3`）。
//!
//! 三条职责：
//!
//! 1. **服务器台账**（`server`）：人工登记的机器信息，register 上报的 `host` 与之
//!    **关联但不强校验**（未登记的 host 记录并告警，**不拒绝注册**——否则改个机器名
//!    就把采集卡死）；
//! 2. **节点定义**（`collector_node`）：**名称即唯一标识**（= `instance_id`），声明范围
//!    （`ScopeRules` + 例外清单）与运行参数（生成配置文件用）；
//! 3. **导出配置**（`docs/02 §7.3`）：控制面**只生成内容，不远程部署**——拉起进程归部署方。
//!
//! > 计划快照（`node_assignment`）与运行时状态（`node_runtime`）见 [`crate::sqlite`]。

use std::collections::BTreeMap;

use rusqlite::{params, OptionalExtension, Row};
use serde_json::Value;

use crate::error::ControlError;
use crate::scope::ScopeRules;
use crate::sqlite::SqliteStore;

// ─────────────────────────────────────────────────────────────────────────────
// 记录类型
// ─────────────────────────────────────────────────────────────────────────────

/// 服务器台账记录（`docs/12 §3.5`）。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ServerRecord {
    pub id: i64,
    pub host: String,
    pub region: Option<String>,
    pub roles: Vec<String>,
    pub cpu_cores: Option<i64>,
    pub mem_gb: Option<i64>,
    pub disk_gb: Option<i64>,
    pub status: String,
    pub labels: BTreeMap<String, String>,
    pub notes: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 采集节点记录（`docs/12 §3.6`）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NodeRecord {
    pub id: i64,
    /// **唯一标识** = `instance_id`（`docs/10 §5` 的 `<instance>`）。
    pub name: String,
    pub enabled: bool,
    /// 期望落在哪台机器（`docs/02 §7.3`：控制面按 host 分散分片）。
    pub server_id: Option<i64>,
    /// 标的选择规则（`docs/12 §5`，A 方案主路径）。
    pub scope: ScopeRules,
    /// 例外：强制纳入（canonical；catalog 无事实时不臆造）。
    pub scope_include: Vec<String>,
    /// 例外：排除（canonical 或裸码）。
    pub scope_exclude: Vec<String>,
    /// 运行参数（生成配置文件时写出；**不含计划本身**，计划由控制面下发）。
    pub runtime_params: Value,
    pub note: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// 新建服务器的入参。
#[derive(Debug, Clone, Default)]
pub struct NewServer {
    pub host: String,
    pub region: Option<String>,
    pub roles: Vec<String>,
    pub cpu_cores: Option<i64>,
    pub mem_gb: Option<i64>,
    pub disk_gb: Option<i64>,
    pub labels: BTreeMap<String, String>,
    pub notes: Option<String>,
}

/// 新建 / 更新节点的入参。
#[derive(Debug, Clone)]
pub struct NewNode {
    pub name: String,
    pub scope: ScopeRules,
    pub scope_include: Vec<String>,
    pub scope_exclude: Vec<String>,
    pub runtime_params: Value,
    pub server_id: Option<i64>,
    pub note: Option<String>,
    pub enabled: bool,
}

impl Default for NewNode {
    fn default() -> Self {
        Self {
            name: String::new(),
            scope: ScopeRules::default(),
            scope_include: Vec::new(),
            scope_exclude: Vec::new(),
            runtime_params: serde_json::json!({}),
            server_id: None,
            note: None,
            enabled: true,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 服务器台账
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 登记一台服务器（host 唯一；重复登记 = 更新）。
    pub fn upsert_server(&self, srv: &NewServer) -> Result<ServerRecord, ControlError> {
        if srv.host.trim().is_empty() {
            return Err(ControlError::validation("server.host 不能为空"));
        }
        let host = srv.host.clone();
        let region = srv.region.clone();
        let roles = serde_json::to_string(&srv.roles)?;
        let labels = serde_json::to_string(&srv.labels)?;
        let (cpu, mem, disk) = (srv.cpu_cores, srv.mem_gb, srv.disk_gb);
        let notes = srv.notes.clone();
        let now = crate::now_ms();
        self.with_write(|c| {
            c.execute(
                "INSERT INTO server (host, region, roles, cpu_cores, mem_gb, disk_gb,
                                     status, labels, notes, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,'active',?7,?8,?9,?9)
                 ON CONFLICT(host) DO UPDATE SET
                   region = excluded.region, roles = excluded.roles,
                   cpu_cores = excluded.cpu_cores, mem_gb = excluded.mem_gb,
                   disk_gb = excluded.disk_gb, labels = excluded.labels,
                   notes = excluded.notes, updated_at = excluded.updated_at",
                params![host, region, roles, cpu, mem, disk, labels, notes, now as i64],
            )?;
            Ok(())
        })?;
        self.server_by_host(&srv.host)
    }

    /// 按 host 取服务器。
    pub fn server_by_host(&self, host: &str) -> Result<ServerRecord, ControlError> {
        self.with_read(|c| {
            c.query_row(
                "SELECT id, host, region, roles, cpu_cores, mem_gb, disk_gb, status,
                        labels, notes, created_at, updated_at
                 FROM server WHERE host = ?1",
                params![host],
                |row| decode_server(row),
            )
            .optional()?
            .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)
        })
        .or_else(|_| {
            Err(ControlError::not_found(format!(
                "服务器 `{host}` 未登记（09 §2.3：未登记只告警，不拒绝注册）"
            )))
        })
    }

    /// 全部服务器（按 host 升序）。
    pub fn list_servers(&self) -> Result<Vec<ServerRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, host, region, roles, cpu_cores, mem_gb, disk_gb, status,
                        labels, notes, created_at, updated_at
                 FROM server ORDER BY host",
            )?;
            let rows = stmt.query_map([], |row| decode_server(row))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }
}

fn decode_server(row: &Row) -> rusqlite::Result<ServerRecord> {
    let roles: String = row.get(3)?;
    let labels: String = row.get(8)?;
    Ok(ServerRecord {
        id: row.get(0)?,
        host: row.get(1)?,
        region: row.get(2)?,
        roles: serde_json::from_str(&roles).unwrap_or_default(),
        cpu_cores: row.get(4)?,
        mem_gb: row.get(5)?,
        disk_gb: row.get(6)?,
        status: row.get(7)?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        notes: row.get(9)?,
        created_at: row.get::<_, i64>(10)? as u64,
        updated_at: row.get::<_, i64>(11)? as u64,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 采集节点
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 新建节点；**名称重复即报错**（名称 = 唯一标识，`docs/02 §7.1`）。
    pub fn create_node(&self, req: &NewNode) -> Result<NodeRecord, ControlError> {
        req.scope.validate()?;
        if req.name.trim().is_empty() {
            return Err(ControlError::validation("节点名称不能为空（它是 instance_id）"));
        }
        if self.node_by_name(&req.name)?.is_some() {
            return Err(ControlError::Illegal(format!(
                "采集节点 `{}` 已存在（名称即唯一标识）",
                req.name
            )));
        }
        self.write_node(req)
    }

    /// 新建或整体更新（名称不变时）。用于 CLI 的 upsert 语义。
    pub fn upsert_node(&self, req: &NewNode) -> Result<NodeRecord, ControlError> {
        req.scope.validate()?;
        if req.name.trim().is_empty() {
            return Err(ControlError::validation("节点名称不能为空（它是 instance_id）"));
        }
        self.write_node(req)
    }

    fn write_node(&self, req: &NewNode) -> Result<NodeRecord, ControlError> {
        let name = req.name.clone();
        let scope = serde_json::to_string(&req.scope)?;
        let include = serde_json::to_string(&req.scope_include)?;
        let exclude = serde_json::to_string(&req.scope_exclude)?;
        let params_json = serde_json::to_string(&req.runtime_params)?;
        let (server_id, note, enabled) = (req.server_id, req.note.clone(), req.enabled);
        let now = crate::now_ms();
        self.with_write(|c| {
            c.execute(
                "INSERT INTO collector_node
                   (name, enabled, server_id, scope_rules, scope_exclude, scope_include,
                    runtime_params, note, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)
                 ON CONFLICT(name) DO UPDATE SET
                   enabled = excluded.enabled, server_id = excluded.server_id,
                   scope_rules = excluded.scope_rules, scope_exclude = excluded.scope_exclude,
                   scope_include = excluded.scope_include,
                   runtime_params = excluded.runtime_params, note = excluded.note,
                   updated_at = excluded.updated_at",
                params![
                    name,
                    enabled,
                    server_id,
                    scope,
                    exclude,
                    include,
                    params_json,
                    note,
                    now as i64
                ],
            )?;
            Ok(())
        })?;
        self.node_by_name(&req.name)?
            .ok_or_else(|| ControlError::Illegal("node write lost".into()))
    }

    /// 按名称取节点。
    pub fn node_by_name(&self, name: &str) -> Result<Option<NodeRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, enabled, server_id, scope_rules, scope_exclude,
                        scope_include, runtime_params, note, created_at, updated_at
                 FROM collector_node WHERE name = ?1",
            )?;
            let mut rows = stmt.query_map(params![name], |row| decode_node(row))?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 全部节点（按名称升序）。
    pub fn list_nodes(&self) -> Result<Vec<NodeRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, enabled, server_id, scope_rules, scope_exclude,
                        scope_include, runtime_params, note, created_at, updated_at
                 FROM collector_node ORDER BY name",
            )?;
            let rows = stmt.query_map([], |row| decode_node(row))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 删除节点（连带删派生计划；运行时状态保留，避免运行中的实例失忆）。
    pub fn delete_node(&self, name: &str) -> Result<bool, ControlError> {
        self.with_write(|c| {
            let n = c.execute("DELETE FROM collector_node WHERE name = ?1", params![name])?;
            Ok(n > 0)
        })
    }
}

fn decode_node(row: &Row) -> rusqlite::Result<NodeRecord> {
    let scope_json: String = row.get(4)?;
    let exclude_json: String = row.get(5)?;
    let include_json: String = row.get(6)?;
    let params_json: String = row.get(7)?;
    let parse = |s: String, what: &str| -> Result<Value, rusqlite::Error> {
        serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
                "{what}: {e}"
            ))))
        })
    };
    let scope: ScopeRules = serde_json::from_str(&scope_json).map_err(|e| {
        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
            "scope: {e}"
        ))))
    })?;
    Ok(NodeRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        enabled: row.get::<_, i64>(2)? != 0,
        server_id: row.get(3)?,
        scope,
        scope_exclude: serde_json::from_str(&exclude_json).unwrap_or_default(),
        scope_include: serde_json::from_str(&include_json).unwrap_or_default(),
        runtime_params: parse(params_json, "runtime_params").unwrap_or(Value::Object(Default::default())),
        note: row.get(8)?,
        created_at: row.get::<_, i64>(9)? as u64,
        updated_at: row.get::<_, i64>(10)? as u64,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 导出节点配置（docs/02 §7.3：控制面只生成内容，不远程部署）
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 生成采集器的 `collector.toml` 内容。
    ///
    /// 内容 = **节点标识 + 运行参数**（`runtime_params` 原样展开）；**不含计划**——
    /// 订阅计划由控制面经命令信道下发（`docs/02 §7.1` 第 ⑤ 步），文件里只保留
    /// 一份可选的**冷启动兜底**（`control.pull_on_start` 默认开）。
    ///
    /// 控制面地址等部署相关参数由 `runtime_params.control` 给出；缺失时补默认值，
    /// 保证导出的文件**直接可被 `ponk-collect` 读取**（`CollectorConfig::from_toml`）。
    pub fn export_node_toml(&self, name: &str) -> Result<String, ControlError> {
        let node = self
            .node_by_name(name)?
            .ok_or_else(|| ControlError::not_found(format!("采集节点 `{name}` 不存在")))?;

        // runtime_params 逐段展开；instance_id 与 host 由本函数决定（覆盖 params 里的同名项）
        let mut doc = serde_json::json!({ "instance_id": node.name });
        if let Value::Object(map) = &node.runtime_params {
            for (k, v) in map {
                doc[k] = v.clone();
            }
        }
        doc["instance_id"] = Value::String(node.name.clone());
        if let Some(sid) = node.server_id {
            if let Some(srv) = self.server_by_id(sid)? {
                doc["host"] = Value::String(srv.host);
            }
        }
        // 兜底默认：拉取必须开着，否则进程起来后不与控制面对齐
        if doc.get("control").is_none() {
            doc["control"] = serde_json::json!({
                "pull_on_start": true,
                "watch_changed": true,
            });
        }
        toml::to_string(&doc)
            .map_err(|e| ControlError::Illegal(format!("导出 TOML 失败: {e}")))
    }

    /// 按 id 取服务器。
    pub fn server_by_id(&self, id: i64) -> Result<Option<ServerRecord>, ControlError> {
        self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT id, host, region, roles, cpu_cores, mem_gb, disk_gb, status,
                        labels, notes, created_at, updated_at
                 FROM server WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map(params![id], |row| decode_server(row))?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteStore;

    fn store() -> SqliteStore {
        let dir = tempfile::tempdir().expect("temp dir");
        SqliteStore::open(dir.path()).expect("open")
    }

    #[test]
    fn server_upsert_is_idempotent_by_host() {
        let s = store();
        let a = s
            .upsert_server(&NewServer {
                host: "tokyo-1".into(),
                region: Some("ap-northeast-1".into()),
                roles: vec!["collector".into(), "storage".into()],
                cpu_cores: Some(8),
                ..Default::default()
            })
            .unwrap();
        let b = s
            .upsert_server(&NewServer {
                host: "tokyo-1".into(),
                region: Some("ap-northeast-1".into()),
                roles: vec!["collector".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(a.id, b.id, "同 host = 同一行");
        assert_eq!(b.roles, vec!["collector".to_string()], "重复登记为更新");
        assert_eq!(s.list_servers().unwrap().len(), 1);
        assert!(s
            .upsert_server(&NewServer::default())
            .is_err(), "host 为空要报错");
    }

    #[test]
    fn node_crud_and_duplicate_name_rejected() {
        let s = store();
        let srv = s
            .upsert_server(&NewServer {
                host: "tokyo-2".into(),
                ..Default::default()
            })
            .unwrap();

        let mut scope = ScopeRules::default();
        scope.venues = vec!["binance".into()];
        scope.quote = vec!["USDT".into()];
        let node = s
            .create_node(&NewNode {
                name: "col-binance".into(),
                scope: scope.clone(),
                scope_exclude: vec!["BTCUSDT".into()],
                runtime_params: serde_json::json!({"heartbeat": {"interval_ms": 5000}}),
                server_id: Some(srv.id),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(node.name, "col-binance");
        assert_eq!(node.scope.venues, vec!["binance".to_string()]);
        assert_eq!(node.scope_exclude, vec!["BTCUSDT".to_string()]);
        assert_eq!(node.server_id, Some(srv.id), "期望机器回填");

        // 同名新建必须拒绝（名称 = 唯一标识）
        let err = s.create_node(&NewNode { name: "col-binance".into(), ..Default::default() });
        assert!(err.is_err(), "同名新建要报错");

        // upsert 更新
        let mut upd = NewNode { name: "col-binance".into(), ..Default::default() };
        upd.scope = scope;
        upd.runtime_params = serde_json::json!({"heartbeat": {"interval_ms": 10000}});
        let node2 = s.upsert_node(&upd).unwrap();
        assert_eq!(node2.id, node.id, "更新不换行");
        assert_eq!(node2.runtime_params["heartbeat"]["interval_ms"], 10000);

        assert_eq!(s.list_nodes().unwrap().len(), 1);
        assert!(s.delete_node("col-binance").unwrap());
        assert!(s.node_by_name("col-binance").unwrap().is_none());
    }

    #[test]
    fn export_produces_readable_toml_with_defaults() {
        let s = store();
        let srv = s
            .upsert_server(&NewServer {
                host: "tokyo-3".into(),
                ..Default::default()
            })
            .unwrap();
        s.create_node(&NewNode {
            name: "col-okx".into(),
            runtime_params: serde_json::json!({
                "heartbeat": {"interval_ms": 5000},
                "control": {"pull_on_start": true, "pull_retry_ms": 3000}
            }),
            server_id: Some(srv.id),
            ..Default::default()
        })
        .unwrap();

        let toml = s.export_node_toml("col-okx").unwrap();
        assert!(toml.contains("instance_id = \"col-okx\""), "{toml}");
        assert!(toml.contains("host = \"tokyo-3\""), "期望机器写入配置 {toml}");
        assert!(toml.contains("interval_ms = 5000"), "{toml}");
        assert!(toml.contains("pull_retry_ms = 3000"), "{toml}");
        assert!(toml.contains("pull_on_start = true"), "拉取必须开着 {toml}");

        // 缺 control 段时补默认
        s.create_node(&NewNode {
            name: "col-bare".into(),
            ..Default::default()
        })
        .unwrap();
        let bare = s.export_node_toml("col-bare").unwrap();
        assert!(bare.contains("pull_on_start = true"), "{bare}");
        assert!(bare.contains("instance_id = \"col-bare\""), "{bare}");

        // 导出的内容必须能被采集器解析（跨 crate 契约）
        let cfg: toml::Value = toml::from_str(&bare).unwrap();
        assert_eq!(cfg["instance_id"].as_str(), Some("col-bare"));

        assert!(s.export_node_toml("nope").is_err(), "不存在的节点要报错");
    }
}
