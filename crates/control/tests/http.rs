//! `docs/13-控制面管理界面与API.md` HTTP 服务层的进程内请求测试。
//!
//! 全部用 `axum::Router` + `tower::ServiceExt::oneshot` 驱动——**不占端口、不依赖网络**。
//!
//! 鉴权按 §5.1 走完整链路：夹具先**登录**换 JWT（[`login`] 是公共 helper），
//! 旧的 9 个用例全部改用「登录 → 用 JWT」，不再有静态 token。

use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use ponk_control::http::{self, AppState, AuthConfig};
use ponk_control::{
    BootstrapMode, ControlConfig, ControlPlane, MetaError, MetadataFetcher, NewExchange, NewMarket,
    NewUser, Role,
};
use ponk_eventcenter::MemoryEventCenter;
use ponk_protocol::{ConfigDomain, CurrencyInfo, NodeKind, OpsHeartbeat, Status, SymbolInfo};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tower::ServiceExt;

/// 测试夹具里的两个账号（一个 `read`、一个 `write`），密码只在这里出现。
const READ_USER: &str = "viewer";
const READ_PASS: &str = "read-pass-123";
const WRITE_USER: &str = "alice";
const WRITE_PASS: &str = "write-pass-456";

/// 测试用 JWT 签名密钥（仅测试进程内使用，不落任何日志）。
const TEST_SECRET: &str = "test-only-jwt-secret-not-a-production-key";

type Plane = ControlPlane<MemoryEventCenter>;

// ── 夹具 ────────────────────────────────────────────────────────────────────

struct Fixture {
    app: Router,
    #[allow(dead_code)]
    shared: Arc<Mutex<Plane>>,
    #[allow(dead_code)]
    dir: TempDir,
    /// `POST /auth/login` 换来的 JWT（`read` 角色）。
    read_token: String,
    /// `POST /auth/login` 换来的 JWT（`write` 角色）。
    write_token: String,
}

/// 缺省鉴权配置：密钥 + 8h TTL + 空库时首启引导（`admin`/`admin123`）。
fn default_auth() -> AuthConfig {
    AuthConfig::new(
        Some(TEST_SECRET.to_string()),
        Some(8 * 3600),
        None,
        BootstrapMode::Auto,
    )
    .expect("鉴权配置合法")
}

/// 只开控制面、**不预置任何用户**（首启引导 / `no_users` / 过期令牌用例用）。
async fn setup_raw(auth: AuthConfig) -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = ControlConfig {
        instance_id: "cp-1".into(),
        store_root: dir.path().to_path_buf(),
        ..ControlConfig::default()
    };
    let plane = ControlPlane::open(MemoryEventCenter::new(), cfg).expect("open plane");
    let (state, shared) = AppState::build(
        plane,
        auth,
        Duration::from_millis(http::DEFAULT_LOCK_WAIT_MS),
    )
    .expect("构建 HTTP 状态（含首启引导）");
    Fixture {
        app: http::router(state),
        shared,
        dir,
        read_token: String::new(),
        write_token: String::new(),
    }
}

/// 标准夹具：首启引导建 `admin` → 再建 `viewer`(read) / `alice`(write) → 登录取 JWT。
async fn setup() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = ControlConfig {
        instance_id: "cp-1".into(),
        store_root: dir.path().to_path_buf(),
        ..ControlConfig::default()
    };
    let plane = ControlPlane::open(MemoryEventCenter::new(), cfg).expect("open plane");
    let store = plane.store().sqlite().expect("SQLite 后端").clone();
    let (state, shared) = AppState::build(
        plane,
        default_auth(),
        Duration::from_millis(http::DEFAULT_LOCK_WAIT_MS),
    )
    .expect("构建 HTTP 状态");
    // 首启引导已在 build 里建了 admin；这里再补两个角色账号
    store
        .create_user(&NewUser {
            username: READ_USER.into(),
            password: READ_PASS.into(),
            role: Role::Read,
        })
        .expect("创建 read 用户");
    store
        .create_user(&NewUser {
            username: WRITE_USER.into(),
            password: WRITE_PASS.into(),
            role: Role::Write,
        })
        .expect("创建 write 用户");

    let mut f = Fixture {
        app: http::router(state),
        shared,
        dir,
        read_token: String::new(),
        write_token: String::new(),
    };
    f.read_token = login(&f, READ_USER, READ_PASS).await;
    f.write_token = login(&f, WRITE_USER, WRITE_PASS).await;
    f
}

/// 发一个请求，返回 `(状态, 响应头, JSON 体)`。
async fn call(
    f: &Fixture,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let (status, headers, text) = call_raw(f, method, path, token, None, body).await;
    let v = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    (status, headers, v)
}

/// 发一个请求，返回 `(状态, 响应头, 原始响应体)`；可附带额外请求头（如 `If-Match`）。
async fn call_raw(
    f: &Fixture,
    method: &str,
    path: &str,
    token: Option<&str>,
    extra: Option<(&str, &str)>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, String) {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b = b.header(header::CONTENT_TYPE, "application/json");
    if let Some((k, v)) = extra {
        b = b.header(k, v);
    }
    let req = b
        .body(Body::from(
            body.map(|v| v.to_string()).unwrap_or_default(),
        ))
        .expect("build request");
    let res = f.app.clone().oneshot(req).await.expect("oneshot");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = to_bytes(res.into_body(), usize::MAX).await.expect("body");
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

fn err_code(v: &Value) -> String {
    v["error"]["code"].as_str().unwrap_or_default().to_string()
}

/// **公共登录 helper**（§5.1.1 `POST /auth/login`）：9 个旧用例都从这里拿 JWT，
/// 登录逻辑不重复。失败时**不回显响应体**（令牌不进断言输出）。
async fn login(f: &Fixture, username: &str, password: &str) -> String {
    let (status, _, text) = call_raw(
        f,
        "POST",
        "/api/v1/auth/login",
        None,
        None,
        Some(json!({ "username": username, "password": password })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "登录 {username} 失败（HTTP {status}）"
    );
    let v: Value = serde_json::from_str(&text).expect("登录响应是 JSON");
    let token = v["access_token"].as_str().unwrap_or_default().to_string();
    assert!(!token.is_empty(), "登录响应缺 access_token");
    assert_eq!(v["token_type"], "Bearer");
    assert_eq!(v["username"], username);
    token
}

/// 一份能通过 `validate::validate` 的 SignalRules 文档。
fn signal_doc() -> Value {
    json!({
        "rules": [{
            "signal_id": "btc_4h_momentum",
            "signal_name": "BTC 4h 动量",
            "timeframe": "4h",
            "scope": ["binance:swap:*"],
            "inputs": ["md.candle.1m"],
            "graph": [
                {"id": "close", "op": "pick", "args": {"input": "md.candle.1m", "field": "close"}},
                {"id": "rsi", "op": "rsi", "args": {"period": 14}, "in": ["close"]}
            ],
            "emit": {"value": "rsi", "payload": ["rsi"]}
        }]
    })
}

// ── 1. 鉴权：无令牌 401 / read 角色调写接口 403 / write 角色 200 ──────────

#[tokio::test]
async fn auth_requires_token_and_enforces_read_write_roles() {
    let f = setup().await;

    // /healthz 免鉴权（§5.1）
    let (s, _, v) = call(&f, "GET", "/api/v1/healthz", None, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "ok");

    // 无 token → 401
    let (s, _, v) = call(&f, "GET", "/api/v1/overview", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(err_code(&v), "unauthenticated");

    // 错误 token → 401
    let (s, _, v) = call(&f, "GET", "/api/v1/overview", Some("nope"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(err_code(&v), "unauthenticated");

    // read 角色读 → 200
    let (s, _, v) = call(&f, "GET", "/api/v1/overview", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // read 角色调写接口 → 403
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/nodes",
        Some(f.read_token.as_str()),
        Some(json!({"name": "col-x"})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(err_code(&v), "forbidden");

    // write 角色写 → 201
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/nodes",
        Some(f.write_token.as_str()),
        Some(json!({"name": "col-x"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["name"], "col-x");

    // `X-Actor` 仍可覆盖 `sub`（§5.1「操作者」行）：publisher = <instance>/<actor>
    let (s, _, v) = call_raw(
        &f,
        "POST",
        "/api/v1/nodes",
        Some(f.write_token.as_str()),
        Some(("X-Actor", "carol")),
        Some(json!({"name": "col-y"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/audit?action=node_create",
        Some(f.write_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let actors: Vec<String> = v["items"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|a| a["actor"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        actors.iter().any(|a| a == "cp-1/alice"),
        "缺省 actor = JWT sub：{actors:?}"
    );
    assert!(
        actors.iter().any(|a| a == "cp-1/carol"),
        "X-Actor 覆盖 sub：{actors:?}"
    );

    // 内容类型（§4.1）
    let (_, h, _) = call(&f, "GET", "/api/v1/overview", Some(f.read_token.as_str()), None).await;
    let ct = h
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(ct, "application/json; charset=utf-8", "§4.1 内容类型");
}

// ── 2. 节点：创建 → 列表 → 详情 → 启停（If-Match）→ 导出 TOML ──────────────

#[tokio::test]
async fn node_lifecycle_list_detail_enable_toml() {
    let f = setup().await;

    // 创建 201
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/nodes",
        Some(f.write_token.as_str()),
        Some(json!({
            "name": "col-binance",
            "enabled": true,
            "scope": {"venues": ["binance"], "channels": ["trade"], "shard_size": 10},
            "scope_exclude": ["BTCUSDT"],
            "runtime_params": {"heartbeat": {"interval_ms": 5000}},
            "note": "集成测试节点"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["name"], "col-binance");
    let etag = v["etag"].as_str().expect("行级 etag").to_string();
    assert!(etag.starts_with("W/\""), "§5.3 ETag 形如 W/\"<id>-<updated_at>\"：{etag}");

    // 列表
    let (s, _, v) = call(&f, "GET", "/api/v1/nodes", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1, "{v}");
    assert_eq!(v["items"][0]["name"], "col-binance");
    assert_eq!(v["limit"], 50, "§4.1 默认 limit=50");
    assert_eq!(v["offset"], 0);
    assert_eq!(v["total"], 1);

    // 详情
    let (s, _, v) = call(&f, "GET", "/api/v1/nodes/col-binance", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "col-binance");
    assert_eq!(v["scope"]["shard_size"], 10);
    assert_eq!(v["scope_exclude"][0], "BTCUSDT");

    // `If-Match` 用 `updated_at` 当代用版本（§5.3），跨毫秒才可分辨
    tokio::time::sleep(Duration::from_millis(5)).await;

    // 启停：缺 If-Match → 428
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/nodes/col-binance/enabled",
        Some(f.write_token.as_str()),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED, "{v}");
    assert_eq!(err_code(&v), "precondition_required");

    // If-Match 不匹配 → 412
    let (s, _, v) = call_raw(
        &f,
        "PATCH",
        "/api/v1/nodes/col-binance/enabled",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), "W/\"0-0\"")),
        Some(json!({"enabled": false})),
    )
    .await;
    let v: Value = serde_json::from_str(&v).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::PRECONDITION_FAILED, "{v}");
    assert_eq!(err_code(&v), "precondition_failed");

    // 正确 If-Match → 200，enabled 翻转
    let (s, h, v) = call_raw(
        &f,
        "PATCH",
        "/api/v1/nodes/col-binance/enabled",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), etag.as_str())),
        Some(json!({"enabled": false})),
    )
    .await;
    let v: Value = serde_json::from_str(&v).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["enabled"], false);
    let new_etag = v["etag"].as_str().expect("新 etag").to_string();
    assert_ne!(new_etag, etag, "updated_at 变了 → etag 必须变");
    assert!(
        h.get(header::ETAG).is_some(),
        "§5.3 行级 ETag 也要在响应头里"
    );

    // 导出 TOML：响应体必须能被 toml 解析
    let (s, h, text) = call_raw(
        &f,
        "GET",
        "/api/v1/nodes/col-binance/config.toml",
        Some(f.read_token.as_str()),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{text}");
    let ct = h
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(ct.starts_with("text/plain"), "实际 {ct}");
    let parsed: toml::Value = toml::from_str(&text).expect("导出内容必须是合法 TOML");
    assert_eq!(parsed["instance_id"].as_str(), Some("col-binance"));

    // 范围预览（不落库）
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/nodes/col-binance/scope/preview",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v.get("counts").is_some(), "{v}");

    // 删除需 If-Match；无 → 428
    let (s, _, _) = call(
        &f,
        "DELETE",
        "/api/v1/nodes/col-binance",
        Some(f.write_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED);
    // 带对的 If-Match → 204
    let (s, _, text) = call_raw(
        &f,
        "DELETE",
        "/api/v1/nodes/col-binance",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), new_etag.as_str())),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{text}");
    let (s, _, v) = call(&f, "GET", "/api/v1/nodes", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["total"], 0);
}

/// 发一版 SignalRules（`base_version` = 提交方持有的基线）。
async fn publish_signal(f: &Fixture, base: u64, reason: &str) -> (StatusCode, HeaderMap, Value) {
    call(
        f,
        "POST",
        "/api/v1/config/signal_rules/publish",
        Some(f.write_token.as_str()),
        Some(json!({"content": signal_doc(), "base_version": base, "reason": reason})),
    )
    .await
}

// ── 3. 配置域：publish → current → 错误基线 409 → rollback → audit 3 条 ────

#[tokio::test]
async fn config_publish_conflict_rollback_and_audit() {
    let f = setup().await;

    // ① 发布 → 版本 +1
    let (s, _, v) = publish_signal(&f, 0, "首次发布").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["doc"]["version"], 1, "{v}");
    assert_eq!(
        v["doc"]["publisher"], "cp-1/alice",
        "§5.1 X-Actor 缺省取 JWT `sub` → publisher = <instance>/<actor>"
    );
    assert_eq!(v["notified"], 1, "SignalRules 走 ConfigChanged 广播（09 §2.2 A）");
    assert!(v["diff"].is_array());

    // ② current
    let (s, h, v) = call(&f, "GET", "/api/v1/config/signal_rules", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["version"], 1);
    assert!(v["content_hash"].as_str().unwrap_or("").len() == 64, "{v}");
    let etag = h
        .get(header::ETAG)
        .and_then(|x| x.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(!etag.is_empty(), "§4.9 必须回 ETag 头");

    // ③ 错误 base_version → 409
    let (s, _, v) = publish_signal(&f, 0, "过期基线").await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(err_code(&v), "conflict");
    assert_eq!(v["error"]["detail"]["current_version"], 1);
    assert_eq!(v["error"]["detail"]["base_version"], 0);

    // 缺 base_version → 428
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/config/signal_rules/publish",
        Some(f.write_token.as_str()),
        Some(json!({"content": signal_doc()})),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED, "{v}");
    assert_eq!(err_code(&v), "precondition_required");

    // ④ 正确基线再发一版
    let (s, _, v) = publish_signal(&f, 1, "第二次发布").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["doc"]["version"], 2);

    // ⑤ 回滚 = 取历史内容发布为**新版本**（09 §4.5）
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/config/signal_rules/rollback",
        Some(f.write_token.as_str()),
        Some(json!({"target_version": 1, "base_version": 2})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["doc"]["version"], 3, "版本号只前进，不倒回");
    assert_eq!(v["doc"]["reason"], "rollback to v1");

    // ⑥ 审计 3 条
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/config/signal_rules/audit",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 3, "两次发布 + 一次回滚 = 3 条：{v}");
    assert_eq!(v["items"][0]["version"], 3);
    assert_eq!(v["items"][0]["action"], "rollback");
    assert_eq!(v["items"][2]["version"], 1);

    // 校验接口（不落库）—— §5.1 只读角色 = 「全部 GET」，POST 即需 write
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/config/signal_rules/validate",
        Some(f.read_token.as_str()),
        Some(json!({"content": json!({"rules": []})})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "只读 token 调 POST 一律 403：{v}");
    assert_eq!(err_code(&v), "forbidden");

    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/config/signal_rules/validate",
        Some(f.write_token.as_str()),
        Some(json!({"content": json!({"rules": []})})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(err_code(&v), "validation");

    // 合法文档 → 200 {ok:true}
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/config/signal_rules/validate",
        Some(f.write_token.as_str()),
        Some(json!({"content": signal_doc()})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["domain"], "signal_rules");

    // 导出
    let (s, h, text) = call_raw(
        &f,
        "GET",
        "/api/v1/config/signal_rules/export",
        Some(f.read_token.as_str()),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{text}");
    let cd = h
        .get(header::CONTENT_DISPOSITION)
        .and_then(|x| x.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(cd.contains("signal_rules-v3.json"), "实际 {cd}");
    let exported: Value = serde_json::from_str(&text).expect("导出是合法 JSON");
    assert_eq!(exported["version"], 3);
    assert!(exported["content_hash"].as_str().is_some());
}

// ── 4. 未知路径 404 vs 未实现接口 501 ───────────────────────────────────────

#[tokio::test]
async fn unknown_path_is_404_and_no_registered_path_is_501() {
    let f = setup().await;

    // 未知路径 → 404（不是 501）
    let (s, _, v) = call(&f, "GET", "/api/v1/definitely-not-here", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(err_code(&v), "not_found");

    // 阶段三已接线的端点：**不再返回 501**（docs/13 §4.6/4.7/4.8/4.11/4.12/4.13）
    for (method, path) in [
        ("GET", "/api/v1/exchanges"),
        ("POST", "/api/v1/exchanges"),
        ("GET", "/api/v1/exchanges/binance"),
        ("GET", "/api/v1/instruments"),
        ("GET", "/api/v1/assets"),
        ("GET", "/api/v1/venues/binance/assets"),
        ("GET", "/api/v1/audit"),
        ("GET", "/api/v1/audit/export"),
        ("GET", "/api/v1/assignments"),
        ("GET", "/api/v1/assignments/holders"),
        ("GET", "/api/v1/assignments/unassigned"),
        ("POST", "/api/v1/assignments/dry-run"),
        ("GET", "/api/v1/sync/runs"),
        ("GET", "/api/v1/sync/runs/1"),
        ("GET", "/api/v1/nodes/nope/assignments"),
        ("GET", "/api/v1/nodes/nope/assignments/latest"),
    ] {
        let (s, _, v) = call(&f, method, path, Some(f.write_token.as_str()), Some(json!({}))).await;
        assert_ne!(
            s,
            StatusCode::NOT_IMPLEMENTED,
            "{method} {path} 已接线，不该再 501：{v}"
        );
        assert_ne!(err_code(&v), "not_implemented", "{method} {path}");
    }

    // `repush` 也已接线（需改 plane.rs 的公开入口）→ 未指派实例回 **404**，不是 501
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/runtime/instances/never-assigned/repush",
        Some(f.write_token.as_str()),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(err_code(&v), "not_found", "已接线接口不得再回 501：{v}");

    // 已实现的路径不该被 501 波及
    let (s, _, _) = call(&f, "GET", "/api/v1/sync/status", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK);

    // 未知域 → 404（§4.2 not_found 覆盖「域」）
    let (s, _, v) = call(&f, "GET", "/api/v1/config/no_such_domain", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(err_code(&v), "not_found");
}

#[tokio::test]
async fn repush_redelivers_plan_and_advances_assignment_version() {
    let f = setup().await;

    // ① 锁内只做控制面操作：发布候选池 + 注册实例 → 指派；取出首个版本后**立即放锁**
    //    （HTTP handler 也要拿同一把锁；占着不放会撞上 `控制面锁等待超时`）
    let v1 = {
        let mut g = f.shared.lock().await;
        let (_doc, out) = g
            .publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1/alice", "init")
            .expect("publish plan");
        g.publish_all(&out).expect("notify");

        let hb = OpsHeartbeat {
            host: Some("host-r".into()),
            instance_id: "col-r".into(),
            node_kind: NodeKind::Collector,
            status: Status::Startup,
            applied_version: Some(0),
            counters: Default::default(),
            started_at: 1_000,
            ts: 2_000,
        };
        let env = ponk_control::ops_envelope("peer", 2_000, hb).expect("ops env");
        let out = g
            .dispatch("ops.collector.col-r", env)
            .expect("register → 指派");
        g.publish_all(&out).expect("deliver plan");

        g.plan_of("col-r")
            .expect("指派后应有计划")
            .assignment_version
    }; // ← 放锁
    assert!(v1 >= 1, "首次指派应有版本，得到 {v1}");

    // ② 重推：成功且版本前进（09 §5.4 / §4.4）
    let (s, _, body) = call(
        &f,
        "POST",
        "/api/v1/runtime/instances/col-r/repush",
        Some(f.write_token.as_str()),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(body["delivered"].as_i64().unwrap_or(0) >= 1, "{body}");
    let v2 = body["assignment_version"].as_u64().unwrap_or(0);
    assert!(v2 > v1, "重推应递增版本：{v1} → {v2}");

    // ③ 再重推一次：版本继续前进（不是"同一个版本重复发"）
    let (s, _, body) = call(
        &f,
        "POST",
        "/api/v1/runtime/instances/col-r/repush",
        Some(f.write_token.as_str()),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let v3 = body["assignment_version"].as_u64().unwrap_or(0);
    assert!(v3 > v2, "连续重推应单调递增：{v2} → {v3}");

    // ④ 只读角色不得重推（触发类，§5.1）
    let (s, _, body) = call(
        &f,
        "POST",
        "/api/v1/runtime/instances/col-r/repush",
        Some(f.read_token.as_str()),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "forbidden");
}

// ── 5. 状态 / 收敛：applied_version 与 last_assignment_version ──────────────

#[tokio::test]
async fn runtime_and_convergence_expose_applied_and_assignment_versions() {
    let f = setup().await;

    // 注册一个采集实例：applied = 7，最近下发 = 7 → 已收敛
    let hb = OpsHeartbeat {
        host: Some("host-a".into()),
        instance_id: "col-1".into(),
        node_kind: NodeKind::Collector,
        status: Status::Startup,
        applied_version: Some(7),
        counters: Default::default(),
        started_at: 1_000,
        ts: 2_000,
    };
    {
        let mut g = f.shared.lock().await;
        g.registry_mut().on_register(&hb, 3_000).expect("register");
        g.registry_mut()
            .set_assignment("col-1", 7)
            .expect("set assignment");
    }

    // 实例列表
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/runtime/instances",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1, "{v}");
    let item = &v["items"][0];
    assert_eq!(item["instance_id"], "col-1");
    assert_eq!(item["applied_version"], 7, "必须带 applied_version：{v}");
    assert_eq!(
        item["last_assignment_version"], 7,
        "必须带 last_assignment_version：{v}"
    );
    assert_eq!(item["converged"], true, "{v}");
    assert_eq!(item["host"], "host-a");

    // 实例详情
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/runtime/instances/col-1",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["applied_version"], 7);
    assert_eq!(v["last_assignment_version"], 7);

    // 收敛快照
    let (s, _, v) = call(&f, "GET", "/api/v1/convergence", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["converged"], 1, "{v}");
    assert_eq!(v["not_converged"], json!([]));
    assert_eq!(v["running"], 1);

    // 计数器（P0）
    let (s, _, v) = call(&f, "GET", "/api/v1/runtime/counts", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["running"], 1);
    assert_eq!(v["total"], 1);

    // 把 applied_version 落后一档 → 未收敛
    {
        let mut g = f.shared.lock().await;
        let mut hb2 = hb.clone();
        hb2.applied_version = Some(6);
        hb2.status = Status::Running;
        g.registry_mut().on_heartbeat(&hb2, 4_000).expect("heartbeat");
        g.registry_mut().mark_convergence("col-1", false, 4_000).unwrap();
    }
    let (s, _, v) = call(&f, "GET", "/api/v1/convergence", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["converged"], 0, "{v}");
    assert_eq!(v["not_converged"][0]["instance_id"], "col-1");
    assert_eq!(v["not_converged"][0]["expected"], 7);
    assert_eq!(v["not_converged"][0]["actual"], 6);

    // 不存在的实例 → 404
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/runtime/instances/nope",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(err_code(&v), "not_found");
}

// ── 附加：服务器台账、概览、元数据同步触发 ──────────────────────────────────

#[tokio::test]
async fn servers_overview_and_forced_sync_roundtrip() {
    let f = setup().await;

    // 服务器：新建 201 → 详情 → 改状态（If-Match）→ 关联节点时删除 409
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/servers",
        Some(f.write_token.as_str()),
        Some(json!({
            "host": "tokyo-1",
            "region": "ap-northeast-1",
            "roles": ["collector"],
            "cpu_cores": 8,
            "mem_gb": 32,
            "disk_gb": 500
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["host"], "tokyo-1");
    assert_eq!(v["status"], "active");
    let etag = v["etag"].as_str().expect("etag").to_string();

    let (s, _, v) = call(&f, "GET", "/api/v1/servers", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1);

    // PATCH status 缺 If-Match → 428
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/servers/tokyo-1/status",
        Some(f.write_token.as_str()),
        Some(json!({"status": "maintenance"})),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED, "{v}");

    // 对的 If-Match → 200
    let (s, _, v) = call_raw(
        &f,
        "PATCH",
        "/api/v1/servers/tokyo-1/status",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), etag.as_str())),
        Some(json!({"status": "maintenance"})),
    )
    .await;
    let v: Value = serde_json::from_str(&v).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "maintenance");

    // 非法状态 → 400
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/servers/tokyo-1/status",
        Some(f.write_token.as_str()),
        Some(json!({"status": "zzz"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(err_code(&v), "validation");

    // 挂一个节点上去 → 删除 409
    let srv_id = v_id(&f, "tokyo-1").await;
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/nodes",
        Some(f.write_token.as_str()),
        Some(json!({"name": "col-on-srv", "server_id": srv_id})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let srv_etag = {
        let (_, _, sv) = call(&f, "GET", "/api/v1/servers/tokyo-1", Some(f.read_token.as_str()), None).await;
        sv["etag"].as_str().unwrap_or_default().to_string()
    };
    let (s, _, v) = call_raw(
        &f,
        "DELETE",
        "/api/v1/servers/tokyo-1",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), srv_etag.as_str())),
        None,
    )
    .await;
    let v: Value = serde_json::from_str(&v).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(err_code(&v), "conflict");

    // 关联查询
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/servers/tokyo-1/nodes",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1);
    assert_eq!(v["items"][0]["name"], "col-on-srv");

    // 概览
    let (s, _, v) = call(&f, "GET", "/api/v1/overview", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["domains"].as_array().map(|a| a.len()), Some(8));
    assert_eq!(v["catalog"]["symbols"], 0);
    assert!(v.get("sync").is_some());

    // /meta：八域 + 消费关系 + 传播类别
    let (s, _, v) = call(&f, "GET", "/api/v1/meta", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["domains"].as_array().map(|a| a.len()), Some(8));
    let sp = v["domains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "subscribe_plan")
        .expect("订阅计划域");
    assert_eq!(sp["push"], "B", "SubscribePlan = B（09 §2.2）");
    assert!(sp["consumers"].as_array().unwrap().iter().any(|c| c == "collector"));

    // 同步状态 + 强制触发
    let (s, _, v) = call(&f, "GET", "/api/v1/sync/status", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["interval_ms"], 300_000, "09 §2.3 默认 5 分钟");
    assert_eq!(v["running"], false);

    let (s, _, v) = call(&f, "POST", "/api/v1/sync/runs", Some(f.write_token.as_str()), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert!(v.get("ran").is_some(), "{v}");

    let (s, _, v) = call(&f, "GET", "/api/v1/sync/targets", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v.get("count").is_some(), "{v}");

    // 只读 token 不能触发同步
    let (s, _, v) = call(&f, "POST", "/api/v1/sync/runs", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
}

/// 取某服务器的 id（构造 `server_id` 引用用）。
async fn v_id(f: &Fixture, host: &str) -> i64 {
    let (_, _, v) = call(f, "GET", &format!("/api/v1/servers/{host}"), Some(f.read_token.as_str()), None).await;
    v["id"].as_i64().expect("server.id")
}

// ── 6. §4.6 交易所与市场：CRUD / ETag / 分页筛选 / 02 §6 硬映射 ─────────────

#[tokio::test]
async fn exchanges_crud_requires_if_match_and_pages_correctly() {
    let f = setup().await;

    for v in ["binance", "okx", "bitget"] {
        let (s, hdr, val) = call(
            &f,
            "POST",
            "/api/v1/exchanges",
            Some(f.write_token.as_str()),
            Some(json!({ "venue": v, "name": v.to_uppercase(), "dig3_exchange_id": v })),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{val}");
        assert_eq!(err_code(&val), "", "201 不是错误体");
        assert!(val["etag"].as_str().is_some(), "行资源带 etag 字段：{val}");
        assert!(hdr.get(header::ETAG).is_some(), "201 响应带 ETag 头：{val}");
    }

    // 重复创建 → 409（§4.2 conflict：同名已存在，改用 PUT）
    let (s, _, val) = call(
        &f,
        "POST",
        "/api/v1/exchanges",
        Some(f.write_token.as_str()),
        Some(json!({ "venue": "binance" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{val}");
    assert_eq!(err_code(&val), "conflict");

    // venue 缺失 → 400 validation
    let (s, _, val) = call(&f, "POST", "/api/v1/exchanges", Some(f.write_token.as_str()), Some(json!({}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{val}");
    assert_eq!(err_code(&val), "validation");

    // 分页信封 §4.1
    let (s, _, val) = call(
        &f,
        "GET",
        "/api/v1/exchanges?limit=2&offset=1",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{val}");
    assert_eq!(val["total"], 3);
    assert_eq!(val["limit"], 2, "§4.1 limit 生效");
    assert_eq!(val["offset"], 1);
    assert_eq!(val["items"].as_array().unwrap().len(), 2);
    assert_eq!(val["items"][0]["venue"], "bitget", "venue 升序");

    let (_, _, val) = call(&f, "GET", "/api/v1/exchanges", Some(f.read_token.as_str()), None).await;
    assert_eq!(val["limit"], 50, "§4.1 默认 50");
    assert!(
        val["items"][0]["market_count"].is_number(),
        "§4.6 交易所行 + 市场数：{val}"
    );

    // 筛选
    let (_, _, val) = call(&f, "GET", "/api/v1/exchanges?q=BIT", Some(f.read_token.as_str()), None).await;
    assert_eq!(val["total"], 1, "q 模糊筛选：{val}");

    // 详情：ETag 响应头 + markets 列表
    let (s, hdr, val) = call(&f, "GET", "/api/v1/exchanges/binance", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{val}");
    let etag = hdr
        .get(header::ETAG)
        .expect("行资源 GET 返回 ETag（§5.3）")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(val["markets"].as_array().unwrap().len(), 0);
    assert_eq!(val["name"], "BINANCE");

    // PUT：缺 If-Match → 428；不符 → 412；相符 → 200 且 ETag 变化
    let body = json!({ "venue": "binance", "name": "Binance Global" });
    let (s, _, val) = call(&f, "PUT", "/api/v1/exchanges/binance", Some(f.write_token.as_str()), Some(body.clone())).await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED, "{val}");
    assert_eq!(err_code(&val), "precondition_required");

    let (s, _, val) = call_raw(
        &f,
        "PUT",
        "/api/v1/exchanges/binance",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), "W/\"stale-etag\"")),
        Some(body.clone()),
    )
    .await;
    let val: Value = serde_json::from_str(&val).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::PRECONDITION_FAILED, "{val}");
    assert_eq!(err_code(&val), "precondition_failed");

    let (s, hdr, val) = call_raw(
        &f,
        "PUT",
        "/api/v1/exchanges/binance",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), etag.as_str())),
        Some(body),
    )
    .await;
    let val: Value = serde_json::from_str(&val).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::OK, "{val}");
    assert_eq!(val["name"], "Binance Global");
    let etag2 = hdr
        .get(header::ETAG)
        .expect("更新后仍返回 ETag")
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(etag2, etag, "行变了 ETag 必须变（§5.3）");

    // PATCH enabled：同样要 If-Match
    let (s, _, val) = call(
        &f,
        "PATCH",
        "/api/v1/exchanges/binance/enabled",
        Some(f.write_token.as_str()),
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED, "{val}");

    let (s, _, val) = call_raw(
        &f,
        "PATCH",
        "/api/v1/exchanges/binance/enabled",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), etag2.as_str())),
        Some(json!({ "enabled": false })),
    )
    .await;
    let val: Value = serde_json::from_str(&val).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::OK, "{val}");
    assert_eq!(val["enabled"], false);

    let (_, _, val) = call(&f, "GET", "/api/v1/exchanges?enabled=false", Some(f.read_token.as_str()), None).await;
    assert_eq!(val["total"], 1, "{val}");
    assert_eq!(val["items"][0]["venue"], "binance");

    // 市场：02 §6 硬映射 + 必填
    let (s, _, val) = call(
        &f,
        "POST",
        "/api/v1/exchanges/binance/markets",
        Some(f.write_token.as_str()),
        Some(json!({ "market": "swap", "dig3_account_type": "FuturesIsolated" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{val}");
    assert_eq!(err_code(&val), "validation");
    assert!(
        val["error"]["message"].as_str().unwrap_or_default().contains("FuturesCross"),
        "写错连不上：{val}"
    );

    let (s, _, val) = call(
        &f,
        "POST",
        "/api/v1/exchanges/binance/markets",
        Some(f.write_token.as_str()),
        Some(json!({ "market": "swap" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "dig3_account_type 必填：{val}");

    let (s, hdr, val) = call(
        &f,
        "POST",
        "/api/v1/exchanges/binance/markets",
        Some(f.write_token.as_str()),
        Some(json!({ "market": "swap", "dig3_account_type": "FuturesCross" })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{val}");
    assert_eq!(val["venue"], "binance");
    assert!((val["quota_collect"].as_f64().unwrap() - 0.50).abs() < 1e-9, "{val}");
    assert!((val["quota_metadata"].as_f64().unwrap() - 0.15).abs() < 1e-9, "{val}");
    let market_etag = hdr.get(header::ETAG).expect("市场行 ETag").to_str().unwrap().to_string();

    // §4.6：市场列表是裸数组（该接口没有 limit/offset）
    let (s, _, val) = call(
        &f,
        "GET",
        "/api/v1/exchanges/binance/markets",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{val}");
    assert!(val.is_array(), "§4.6 市场行：{val}");
    assert_eq!(val.as_array().unwrap().len(), 1);
    assert_eq!(val[0]["dig3_account_type"], "FuturesCross");

    let (_, _, val) = call(&f, "GET", "/api/v1/exchanges/binance", Some(f.read_token.as_str()), None).await;
    assert_eq!(val["market_count"], 1, "{val}");
    assert_eq!(val["markets"].as_array().unwrap().len(), 1);

    // PATCH 市场启停
    let (s, _, val) = call_raw(
        &f,
        "PATCH",
        "/api/v1/exchanges/binance/markets/swap/enabled",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), market_etag.as_str())),
        Some(json!({ "enabled": false })),
    )
    .await;
    let val: Value = serde_json::from_str(&val).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::OK, "{val}");
    assert_eq!(val["enabled"], false);

    // 删除：级联要二次确认
    let (_, hdr, _val) = call(&f, "GET", "/api/v1/exchanges/binance", Some(f.read_token.as_str()), None).await;
    let del_etag = hdr.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    let (s, _, val) = call_raw(
        &f,
        "DELETE",
        "/api/v1/exchanges/binance",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), del_etag.as_str())),
        None,
    )
    .await;
    let val: Value = serde_json::from_str(&val).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::BAD_REQUEST, "级联删除需二次确认：{val}");
    assert_eq!(err_code(&val), "validation");
    assert_eq!(val["error"]["detail"]["confirm"], "binance");

    let (s, _, _) = call_raw(
        &f,
        "DELETE",
        "/api/v1/exchanges/binance?confirm=binance",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), del_etag.as_str())),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "确认后 204");

    let (_, _, val) = call(&f, "GET", "/api/v1/exchanges", Some(f.read_token.as_str()), None).await;
    assert_eq!(val["total"], 2, "级联删除：{val}");

    // 删除不存在的所 → 404
    let (s, _, val) = call_raw(
        &f,
        "DELETE",
        "/api/v1/exchanges/nope",
        Some(f.write_token.as_str()),
        Some((header::IF_MATCH.as_str(), "W/\"x\"")),
        None,
    )
    .await;
    let val: Value = serde_json::from_str(&val).unwrap_or(Value::Null);
    assert_eq!(s, StatusCode::NOT_FOUND, "{val}");

    // 写操作审计（R6）
    let (_, _, val) = call(&f, "GET", "/api/v1/audit", Some(f.read_token.as_str()), None).await;
    let actions: Vec<String> = val["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["action"].as_str().map(str::to_string))
        .collect();
    assert!(actions.iter().any(|a| a == "exchange_create"), "{val}");
    assert!(actions.iter().any(|a| a == "exchange_update"), "{val}");
    assert!(actions.iter().any(|a| a == "exchange_delete"), "{val}");
    assert!(actions.iter().any(|a| a == "market_create"), "{val}");
}

// ── 7. §4.7 / §4.8 / §4.12 / §4.13 / §4.11：catalog 落库后的读写面 ──────────

/// 离线假拉取器（元数据同步的 `MetadataFetcher` 注入点）。
struct FakeMeta {
    symbols: std::sync::Mutex<Vec<SymbolInfo>>,
    currencies: std::sync::Mutex<Vec<CurrencyInfo>>,
}

impl FakeMeta {
    fn new() -> Arc<Self> {
        Arc::new(FakeMeta {
            symbols: std::sync::Mutex::new(vec![
                sym_of("BTCUSDT", "BTC", "TRADING", 1),
                sym_of("ETHUSDT", "ETH", "TRADING", 2),
            ]),
            currencies: std::sync::Mutex::new(vec![CurrencyInfo::new("USDT")]),
        })
    }
}

impl MetadataFetcher for FakeMeta {
    fn fetch_symbols(&self, _venue: &str, _market: &str) -> Result<Vec<SymbolInfo>, MetaError> {
        Ok(self.symbols.lock().unwrap().clone())
    }
    fn fetch_currencies(&self, _venue: &str) -> Result<Vec<CurrencyInfo>, MetaError> {
        Ok(self.currencies.lock().unwrap().clone())
    }
}

fn sym_of(code: &str, base: &str, status: &str, pp: u8) -> SymbolInfo {
    SymbolInfo {
        symbol: code.to_string(),
        base_asset: base.to_string(),
        quote_asset: "USDT".to_string(),
        status: status.to_string(),
        price_precision: pp,
        quantity_precision: 2,
        ..Default::default()
    }
}

fn plan_doc() -> Value {
    json!({
        "shard_size": 8,
        "default_channels": ["md.candle.1m"],
        "instruments": [
            {"venue": "binance", "market": "swap", "native_code": "BTCUSDT",
             "price_precision": 1, "qty_precision": 2},
            {"venue": "binance", "market": "swap", "native_code": "ETHUSDT",
             "price_precision": 2, "qty_precision": 2}
        ],
        "quota": [{"venue": "binance", "egress_ip": "1.2.3.4",
                   "collect": 0.5, "backfill": 0.35, "metadata": 0.15}]
    })
}

#[tokio::test]
async fn catalog_tables_are_persisted_and_served_over_http() {
    let f = setup().await;

    // `docs/14 §2`：同步目标 = `exchange_market` 中 `enabled=1` 的 (venue, market)，
    // **不是 `SubscribePlan` 域**——先在接入台账里登记 binance/swap。
    {
        let mut g = f.shared.lock().await;
        g.publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1/alice", "init")
            .expect("发布订阅计划");
        let db = g.store().sqlite().expect("sqlite 后端").clone();
        db.upsert_exchange(&NewExchange {
            venue: "binance".into(),
            name: "BINANCE".into(),
            ..NewExchange::default()
        })
        .expect("登记交易所");
        db.upsert_market(&NewMarket {
            venue: "binance".into(),
            market: "swap".into(),
            dig3_account_type: "FuturesCross".into(),
            ..NewMarket::default()
        })
        .expect("登记市场");
    }
    let fetcher = FakeMeta::new();
    f.shared.lock().await.set_fetcher(fetcher.clone());

    // ① 触发首轮同步 → 202 + 本轮 sync_run_id（12 §4.1 每轮一条）
    let (s, _, v) = call(&f, "POST", "/api/v1/sync/runs", Some(f.write_token.as_str()), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["ran"], true);
    assert_eq!(v["changed"], true, "空 catalog → 有数据");
    assert_eq!(v["persist_error"], Value::Null, "落库失败要如实汇报：{v}");
    let run_id = v["sync_run_id"].as_i64().expect("本轮落一条 sync_run");

    // ② 轮次历史（分页信封）+ 单条详情（ETag）
    let (s, _, v) = call(&f, "GET", "/api/v1/sync/runs?limit=10", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1);
    assert_eq!(v["limit"], 10);
    assert_eq!(v["items"][0]["id"], json!(run_id));
    assert!(v["items"][0]["finished_at"].is_number());
    assert_eq!(
        v["items"][0]["outcome"]["venues"]["binance"]["instruments"],
        2,
        "12 §3.4 outcome 是逐所结果：{v}"
    );
    assert!(v["items"][0]["error"].is_null());

    let (s, hdr, v) = call(&f, "GET", &format!("/api/v1/sync/runs/{run_id}"), Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(hdr.get(header::ETAG).is_some(), "行资源 GET 返回 ETag");
    let (s, _, v) = call(&f, "GET", "/api/v1/sync/runs/99999", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(err_code(&v), "not_found");
    let (s, _, v) = call(&f, "GET", "/api/v1/sync/runs/abc", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(err_code(&v), "validation");

    // ③ /instruments：分页信封 + 筛选 + 单条 ETag
    let (s, _, v) = call(&f, "GET", "/api/v1/instruments", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 2, "instrument 已落库：{v}");
    assert_eq!(v["limit"], 50);
    assert_eq!(v["offset"], 0);
    assert_eq!(v["items"][0]["native_code"], "BTCUSDT");
    assert_eq!(v["items"][0]["base"], "BTC", "§4.7 base 返回 asset.code");
    assert_eq!(v["items"][0]["quote"], "USDT");
    assert_eq!(v["items"][0]["venue"], "binance");
    assert_eq!(v["items"][0]["market"], "swap");
    assert!(v["items"][0]["etag"].as_str().is_some(), "行带 etag");

    let (_, _, v) = call(
        &f,
        "GET",
        "/api/v1/instruments?native_code=BTC&status=trading",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(v["total"], 1, "组合筛选：{v}");
    let (_, _, v) = call(&f, "GET", "/api/v1/instruments?market=spot", Some(f.read_token.as_str()), None).await;
    assert_eq!(v["total"], 0, "{v}");
    let (_, _, v) = call(&f, "GET", "/api/v1/instruments?base=ETH", Some(f.read_token.as_str()), None).await;
    assert_eq!(v["total"], 1, "{v}");

    let (s, hdr, v) = call(
        &f,
        "GET",
        "/api/v1/instruments/binance/swap/BTCUSDT",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(hdr.get(header::ETAG).is_some(), "canonical 三段路径返回 ETag");
    assert_eq!(v["status"], "TRADING");

    let (s, _, v) = call(&f, "GET", "/api/v1/instruments/binance/swap/NOPE", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(err_code(&v), "not_found");

    // CSV 导出（筛选随导出）
    let (s, hdr, body) = call_raw(
        &f,
        "GET",
        "/api/v1/instruments/export?venue=binance",
        Some(f.read_token.as_str()),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(
        hdr.get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/csv"),
        "{:?}",
        hdr.get(header::CONTENT_TYPE)
    );
    assert!(
        hdr.get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("instruments.csv"),
        "{:?}",
        hdr.get(header::CONTENT_DISPOSITION)
    );
    assert!(body.starts_with("venue,market,native_code"), "{body}");
    assert_eq!(body.lines().count(), 3, "表头 + 2 行：{body}");

    // ④ 零动作：同数据再跑一轮 → 不写 instrument_change
    let (s, _, v) = call(&f, "POST", "/api/v1/sync/runs", Some(f.write_token.as_str()), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["changed"], false, "row_hash 相同 → 零动作：{v}");
    let (_, _, v) = call(&f, "GET", "/api/v1/instruments/changes", Some(f.read_token.as_str()), None).await;
    assert_eq!(v["total"], 2, "零动作不写变更：{v}",);
    let (_, _, v) = call(&f, "GET", "/api/v1/sync/runs", Some(f.read_token.as_str()), None).await;
    assert_eq!(v["total"], 2, "每轮仍落一条 sync_run：{v}");

    // ⑤ 状态变化 → 写 instrument_change（before / after 都给）
    fetcher.symbols.lock().unwrap()[0].status = "BREAK".to_string();
    let (s, _, v) = call(&f, "POST", "/api/v1/sync/runs", Some(f.write_token.as_str()), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["changed"], true, "{v}");

    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/instruments/changes?change_kind=status",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1, "{v}");
    let ch = &v["items"][0];
    assert_eq!(ch["native_code"], "BTCUSDT");
    assert_eq!(ch["change_kind"], "status");
    assert_eq!(ch["venue"], "binance");
    assert!(ch["before"].is_object() && ch["after"].is_object(), "§4.7 before/after：{ch}");
    assert!(
        ch["before_v"].is_object() && ch["after_v"].is_object(),
        "DDL 列名 before_v / after_v（§7.2 R7）：{ch}"
    );
    assert!(ch["sync_run_id"].is_number());
    assert!(ch["detected_at"].is_number());
    let change_id = ch["id"].as_i64().unwrap();

    let (s, hdr, ch) = call(
        &f,
        "GET",
        &format!("/api/v1/instruments/changes/{change_id}"),
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{ch}");
    assert!(hdr.get(header::ETAG).is_some(), "变更行 ETag");
    let (s, _, v) = call(&f, "GET", "/api/v1/instruments/changes/99999", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // ⑥ 币信息两层（§4.8）
    let (s, _, v) = call(&f, "GET", "/api/v1/assets", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 3, "BTC + ETH + USDT：{v}");
    assert!(v["items"][0]["venue_count"].is_number(), "{v}");

    let (s, _, v) = call(&f, "GET", "/api/v1/assets?venue=binance", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1, "只有 USDT 有各所口径：{v}");

    let (s, hdr, v) = call(&f, "GET", "/api/v1/assets/USDT", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(hdr.get(header::ETAG).is_some(), "币行 ETag");
    assert_eq!(v["venue_assets"].as_array().unwrap().len(), 1);
    assert_eq!(v["venue_assets"][0]["venue"], "binance");
    assert!(v["venue_assets"][0]["row_hash"].is_string());

    let (s, _, v) = call(&f, "GET", "/api/v1/assets/NOPE", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    let (s, _, v) = call(&f, "GET", "/api/v1/venues/binance/assets", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1, "{v}");
    assert_eq!(v["items"][0]["code"], "USDT");
    assert_eq!(v["limit"], 50);

    let (s, _, v) = call(&f, "GET", "/api/v1/venues/nope/assets", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    let (s, hdr, body) = call_raw(&f, "GET", "/api/v1/assets/export", Some(f.read_token.as_str()), None, None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(body.starts_with("id,code,name,kind"), "{body}");
    assert_eq!(body.lines().count(), 4, "表头 + 3 行：{body}");
    assert!(hdr.get(header::CONTENT_DISPOSITION).is_some());

    // ⑦ 全局审计（§4.13）
    let (s, _, v) = call(&f, "GET", "/api/v1/audit?action=publish", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1, "本轮只发布过一次 SubscribePlan：{v}");
    assert_eq!(v["items"][0]["target"], "subscribe_plan");
    assert_eq!(v["items"][0]["action"], "publish");
    assert!(v["items"][0]["ts"].is_number());
    assert!(v["items"][0]["diff"].is_array(), "detail 里的 diff 提到顶层：{v}");
    assert!(v["items"][0]["detail"].is_object(), "detail 全文：{v}");

    let (s, _, v) = call(&f, "GET", "/api/v1/audit?actor=alice", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1, "actor 大小写不敏感：{v}");

    let audit_id = {
        let (_, _, v) = call(&f, "GET", "/api/v1/audit", Some(f.read_token.as_str()), None).await;
        assert!(v["total"].as_i64().unwrap() >= 1, "{v}");
        v["items"][0]["id"].as_i64().unwrap()
    };
    let (s, hdr, v) = call(&f, "GET", &format!("/api/v1/audit/{audit_id}"), Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(hdr.get(header::ETAG).is_some(), "审计行 ETag");
    let (s, _, v) = call(&f, "GET", "/api/v1/audit/99999", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    let (s, hdr, body) = call_raw(&f, "GET", "/api/v1/audit/export", Some(f.read_token.as_str()), None, None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(body.starts_with("id,actor,action,target"), "{body}");
    assert!(body.lines().count() >= 2, "{body}");
    assert!(hdr.get(header::CONTENT_DISPOSITION).is_some());

    // ⑧ 指派结果（§4.11）：只读视图 + 试算不下发
    let (s, _, v) = call(&f, "GET", "/api/v1/assignments", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 0, "无活实例 → 无分片行：{v}");
    assert!(v.get("limit").is_some() && v.get("offset").is_some(), "{v}");

    let (s, _, v) = call(&f, "GET", "/api/v1/assignments/holders", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v.is_array(), "holders = [{{venue, holders}}]：{v}");

    let (s, _, v) = call(&f, "GET", "/api/v1/assignments/unassigned", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v.get("total").is_some(), "未指派也是分页信封：{v}");

    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/assignments/dry-run",
        Some(f.write_token.as_str()),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["dry_run"], true);
    assert!(v.get("plans").is_some() && v.get("diff").is_some(), "{v}");
    assert!(v.get("unassigned").is_some() && v.get("holders").is_some(), "{v}");

    // 试算不下发：库里没有该实例的指派记录
    let (s, _, v) = call(&f, "GET", "/api/v1/assignments/col-1", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "dry-run 不落库：{v}");
    let (s, _, v) = call(&f, "GET", "/api/v1/assignments/nope", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // 节点维度的指派查询（§4.5）
    let (s, _, v) = call(&f, "GET", "/api/v1/nodes/nope/assignments", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, _, v) = call(&f, "GET", "/api/v1/nodes/nope/assignments/latest", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // 触发类接口的鉴权不变（§5.1）
    let (s, _, v) = call(&f, "POST", "/api/v1/sync/runs", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
}

// ── 10. §5.1.1 认证接口：login / me / users + 首启引导 + 过期令牌 ────────────

/// 凭证错误统一 `401 invalid_credentials`：**不区分**「用户不存在 / 密码错」（防枚举）。
#[tokio::test]
async fn login_failures_share_one_invalid_credentials_code() {
    let f = setup().await;

    // 现有用户 + 错密码
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": READ_USER, "password": "wrong-pass"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "invalid_credentials");

    // 不存在的用户 → **同一个** code（枚举不出用户名）
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "ghost", "password": "wrong-pass"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "invalid_credentials");

    // 请求体缺字段 → 400 validation（与凭证错误可区分）
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": READ_USER})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "validation");
}

/// 首启引导（§5.1）：空库 → `admin`/`admin123` 可登录且标 `default_password`；
/// `--bootstrap none` → 空库登录回 `403 no_users`。
#[tokio::test]
async fn bootstrap_creates_default_admin_and_none_mode_returns_no_users() {
    // ① 缺省引导：空库自动建内置管理员
    let f = setup_raw(default_auth()).await;
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "admin", "password": "admin123"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{:?}", v["error"]);
    assert_eq!(v["role"], "write");
    assert_eq!(v["username"], "admin");
    assert_eq!(v["token_type"], "Bearer");
    assert_eq!(
        v["expires_in"].as_u64().expect("expires_in"),
        8 * 3600,
        "默认 8h"
    );
    assert_eq!(
        v["default_password"], true,
        "内置默认口令必须在登录响应里标注"
    );
    let raw = v.to_string();
    assert!(!raw.contains("admin123"), "登录响应不得含明文口令");
    assert!(!raw.contains("password_hash"), "登录响应不得含口令哈希");

    // ② 改密后标注消失，旧口令立即失效
    let admin_tok = login(&f, "admin", "admin123").await;
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/auth/users/admin",
        Some(admin_tok.as_str()),
        Some(json!({"password": "changed-pw-1"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{:?}", v["error"]);
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "admin", "password": "admin123"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "invalid_credentials");
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "admin", "password": "changed-pw-1"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{:?}", v["error"]);
    assert_eq!(v["default_password"], false, "改密后不得再标默认口令");

    // ③ `--bootstrap none` + 空库 → 403 no_users
    let none = AuthConfig::new(
        Some(TEST_SECRET.to_string()),
        Some(3600),
        None,
        BootstrapMode::Disabled,
    )
    .expect("鉴权配置合法");
    let f2 = setup_raw(none).await;
    let (s, _, v) = call(
        &f2,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "admin", "password": "admin123"})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "no_users");
}

/// 过期 JWT → `401 token_expired`（用 `--jwt-ttl 0` 构造，不靠 sleep）。
#[tokio::test]
async fn expired_jwt_maps_to_401_token_expired() {
    let auth = AuthConfig::new(
        Some(TEST_SECRET.to_string()),
        Some(0),
        None,
        BootstrapMode::Auto,
    )
    .expect("鉴权配置合法");
    let f = setup_raw(auth).await;
    let token = login(&f, "admin", "admin123").await;

    let (s, _, v) = call(&f, "GET", "/api/v1/auth/me", Some(token.as_str()), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "token_expired");

    // 任意受保护端点同码
    let (s, _, v) = call(&f, "GET", "/api/v1/overview", Some(token.as_str()), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "token_expired");
}

/// `/healthz` 免鉴权；`GET /auth/me` 回 `sub` / `role` / `exp` / `iat`；缺令牌 401。
#[tokio::test]
async fn healthz_is_public_me_returns_claims_and_missing_auth_is_401() {
    let f = setup().await;

    // 两个 healthz 都免鉴权（§5.1 公开端点）
    for p in ["/healthz", "/api/v1/healthz"] {
        let (s, _, v) = call(&f, "GET", p, None, None).await;
        assert_eq!(s, StatusCode::OK, "{p} 应免鉴权：{:?}", v["error"]);
        assert_eq!(v["status"], "ok");
    }

    // write 令牌 → me 返回 sub/role/exp/iat
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/auth/me",
        Some(f.write_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{:?}", v["error"]);
    assert_eq!(v["username"], WRITE_USER, "`sub` = 用户名");
    assert_eq!(v["role"], "write");
    let exp = v["exp"].as_u64().expect("exp");
    let iat = v["iat"].as_u64().expect("iat");
    assert_eq!(exp - iat, 8 * 3600, "默认 8h");

    // read 令牌 → role=read
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/auth/me",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{:?}", v["error"]);
    assert_eq!(v["username"], READ_USER);
    assert_eq!(v["role"], "read");

    // 缺 Authorization → 401 unauthenticated（login 与 healthz 之外一律要令牌）
    for (m, p) in [
        ("GET", "/api/v1/auth/me"),
        ("GET", "/api/v1/overview"),
        ("POST", "/api/v1/auth/users"),
    ] {
        let (s, _, v) = call(&f, m, p, None, Some(json!({}))).await;
        assert_eq!(
            s,
            StatusCode::UNAUTHORIZED,
            "{m} {p} 需要令牌：{:?}",
            v["error"]
        );
        assert_eq!(err_code(&v), "unauthenticated");
    }
}

/// `GET/POST/PATCH /auth/users*`：`write` 门禁、409 重复、**永不含口令哈希**。
#[tokio::test]
async fn user_management_requires_write_and_never_leaks_hash() {
    let f = setup().await;

    // read 角色：列表也需 write（方法级规则管不到 GET）
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/auth/users",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "forbidden");
    // read 角色：建用户 → 方法级 403
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/users",
        Some(f.read_token.as_str()),
        Some(json!({"username": "x", "password": "y", "role": "read"})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "forbidden");

    // write：列表 200，且绝无口令字段
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/auth/users",
        Some(f.write_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{:?}", v["error"]);
    let items = v["items"].as_array().expect("items[]");
    assert_eq!(items.len(), 3, "admin + viewer + alice");
    let raw = v.to_string();
    assert!(!raw.contains("password_hash"), "列表不得含口令哈希");
    assert!(!raw.contains("argon2"), "列表不得含口令哈希");
    assert!(items.iter().all(|u| u.get("password").is_none()));

    // 建用户 → 201，可立即登录
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/users",
        Some(f.write_token.as_str()),
        Some(json!({"username": "carol", "password": "carol-pw-1", "role": "write"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{:?}", v["error"]);
    assert_eq!(v["username"], "carol");
    assert_eq!(v["role"], "write");
    assert_eq!(v["disabled"], false);
    let carol = login(&f, "carol", "carol-pw-1").await;

    // 重复用户名 → 409
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/users",
        Some(f.write_token.as_str()),
        Some(json!({"username": "carol", "password": "x", "role": "read"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "conflict");

    // PATCH ①：只降角色（不停用）
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/auth/users/carol",
        Some(f.write_token.as_str()),
        Some(json!({"role": "read"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{:?}", v["error"]);
    assert_eq!(v["role"], "read");
    assert_eq!(v["disabled"], false);

    // **已签发的旧令牌**立即反映新角色（§5.1：角色取自用户表，不是签发时的快照）
    let (s, _, v) = call(&f, "GET", "/api/v1/auth/me", Some(carol.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{:?}", v["error"]);
    assert_eq!(v["role"], "read", "降权应对旧令牌立即生效，无需重新登录");

    // PATCH ②：停用
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/auth/users/carol",
        Some(f.write_token.as_str()),
        Some(json!({"disabled": true})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{:?}", v["error"]);
    assert_eq!(v["disabled"], true);

    // 停用 → 该令牌**立即失效**（存续取自用户表）
    let (s, _, v) = call(&f, "GET", "/api/v1/auth/me", Some(carol.as_str()), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "unauthenticated", "停用应对已签发令牌立即生效");

    // 停用 → 登录也失败（对外表现为凭证错误，不暴露「已停用」）
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username": "carol", "password": "carol-pw-1"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "invalid_credentials");

    // PATCH 不存在的用户 → 404
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/auth/users/ghost",
        Some(f.write_token.as_str()),
        Some(json!({"password": "x"})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "not_found");

    // 空补丁 → 400 validation
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/auth/users/carol",
        Some(f.write_token.as_str()),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{:?}", v["error"]);
    assert_eq!(err_code(&v), "validation");
}

/// 账号级吊销（`docs/13 §5.1`）：JWT 只证明「是谁 + 是否过期」，
/// **存续与权限以用户表为准**——降权 / 停用必须对**已签发的旧令牌立即生效**，
/// 否则在 `exp`（默认 8h）之前旧令牌仍持有原权限，等于没有吊销。
#[tokio::test]
async fn role_change_and_disable_apply_to_tokens_already_issued() {
    let f = setup().await;

    // ① write 令牌此刻可写
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/nodes",
        Some(&f.write_token),
        Some(json!({ "name": "col-rev" })),
    )
    .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::OK,
        "降权前应可写：{s} {v}"
    );

    // ② 管理员把该用户降为 read
    let admin = login(&f, "admin", "admin123").await;
    let (s, _, v) = call(
        &f,
        "PATCH",
        &format!("/api/v1/auth/users/{}", WRITE_USER),
        Some(&admin),
        Some(json!({ "role": "read" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // ③ **同一枚旧令牌**立即变为只读（角色取自库，不是签发时的快照）
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/nodes",
        Some(&f.write_token),
        Some(json!({ "name": "col-rev-2" })),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "降权应对已签发令牌立即生效（无需重新登录）：{v}"
    );
    assert_eq!(err_code(&v), "forbidden");

    // ④ 停用 → 同一枚令牌连读都不放行
    let (s, _, v) = call(
        &f,
        "PATCH",
        &format!("/api/v1/auth/users/{}", WRITE_USER),
        Some(&admin),
        Some(json!({ "disabled": true })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");

    let (s, _, v) = call(&f, "GET", "/api/v1/overview", Some(&f.write_token), None).await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "停用应对已签发令牌立即生效：{v}"
    );
    assert_eq!(err_code(&v), "unauthenticated");
}

// ── 11. docs/14 §9 目录类页面接口：PATCH /assets、contracts、外部同步两接口 ─

/// 离线假外部源（`OffchainProvider` 注入点）：索引里只有 `bitcoin/BTC`。
struct FakeOffchain;

impl ponk_control::OffchainProvider for FakeOffchain {
    fn source(&self) -> &'static str {
        "coingecko"
    }

    fn fetch_markets_page(
        &self,
        page: u32,
        _per_page: u32,
    ) -> Result<Vec<ponk_control::MarketEntry>, ponk_control::ProviderError> {
        if page > 1 {
            return Ok(Vec::new());
        }
        Ok(vec![ponk_control::MarketEntry {
            id: "bitcoin".into(),
            symbol: "BTC".into(),
            name: Some("Bitcoin".into()),
            market_cap_rank: Some(1),
            total_supply: Some("21000000".into()),
            circulating_supply: Some("19000000".into()),
            raw: json!({ "id": "bitcoin", "symbol": "BTC" }),
        }])
    }

    fn fetch_coin_detail(
        &self,
        id: &str,
    ) -> Result<ponk_control::CoinDetail, ponk_control::ProviderError> {
        let mut platforms = std::collections::BTreeMap::new();
        platforms.insert(
            "TRON".to_string(),
            "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t".to_string(),
        );
        Ok(ponk_control::CoinDetail {
            id: id.to_string(),
            platforms,
            twitter_screen_name: Some("bitcoin".into()),
            raw: json!({ "id": id }),
        })
    }
}

/// 带 `If-Match` 的请求（`docs/13 §5.3` 行级并发）。
async fn call_if_match(
    f: &Fixture,
    method: &str,
    path: &str,
    token: Option<&str>,
    etag: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let (s, h, t) = call_raw(
        f,
        method,
        path,
        token,
        Some((header::IF_MATCH.as_str(), etag)),
        body,
    )
    .await;
    let v = serde_json::from_str::<Value>(&t).unwrap_or(Value::Null);
    (s, h, v)
}

/// `docs/14 §9` 四条新接口 + `PATCH` 的 `match_status` 规则（全离线）。
#[tokio::test]
async fn docs14_assets_patch_contracts_and_offchain_endpoints() {
    let f = setup().await;

    // 外部同步尚无轮次 → 三字段全是空态（`last_run` 必须是 null 而不是空对象）
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/sync/offchain/status",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["running"], false);
    assert!(v["last_run"].is_null(), "无记录时 last_run = null：{v}");
    assert!(v["detail_synced_at"].is_null(), "{v}");

    // ── 准备：接入台账 → 交易所同步 → 有 BTC / ETH / USDT ────────────────
    {
        let mut g = f.shared.lock().await;
        g.publish_domain(ConfigDomain::SubscribePlan, plan_doc(), "cp-1/alice", "init")
            .expect("发布订阅计划");
        let db = g.store().sqlite().expect("sqlite 后端").clone();
        db.upsert_exchange(&NewExchange {
            venue: "binance".into(),
            name: "BINANCE".into(),
            ..NewExchange::default()
        })
        .expect("登记交易所");
        db.upsert_market(&NewMarket {
            venue: "binance".into(),
            market: "swap".into(),
            dig3_account_type: "FuturesCross".into(),
            ..NewMarket::default()
        })
        .expect("登记市场");
        g.set_fetcher(FakeMeta::new());
    }
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/sync/runs",
        Some(f.write_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["changed"], true, "{v}");

    // ── ① PATCH /assets/{code}：If-Match 语义 ───────────────────────────
    let (s, hdr, v) = call(&f, "GET", "/api/v1/assets/BTC", Some(f.read_token.as_str()), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let etag = v["etag"].as_str().expect("行级 etag").to_string();
    assert!(etag.starts_with("W/\""), "内容哈希 ETag：{etag}");
    assert_eq!(
        hdr.get(header::ETAG).and_then(|x| x.to_str().ok()),
        Some(etag.as_str()),
        "body.etag 与 ETag 头双通道同值"
    );
    let row_hash_before = {
        let (_, _, ins) = call(
            &f,
            "GET",
            "/api/v1/instruments/binance/swap/BTCUSDT",
            Some(f.read_token.as_str()),
            None,
        )
        .await;
        ins["row_hash"].clone()
    };

    // 只读 token → 403
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/assets/BTC",
        Some(f.read_token.as_str()),
        Some(json!({ "kind": "token" })),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");

    // 缺 If-Match → 428
    let (s, _, v) = call(
        &f,
        "PATCH",
        "/api/v1/assets/BTC",
        Some(f.write_token.as_str()),
        Some(json!({ "kind": "token" })),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED, "{v}");
    assert_eq!(err_code(&v), "precondition_required");

    // If-Match 不符 → 412
    let (s, _, v) = call_if_match(
        &f,
        "PATCH",
        "/api/v1/assets/BTC",
        Some(f.write_token.as_str()),
        "W/\"stale-etag\"",
        Some(json!({ "kind": "token" })),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_FAILED, "{v}");

    // 资产不存在 → 404
    let (s, _, v) = call_if_match(
        &f,
        "PATCH",
        "/api/v1/assets/NOPE",
        Some(f.write_token.as_str()),
        "*",
        Some(json!({ "kind": "coin" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // 两个字段都没给 → 400 validation
    let (s, _, v) = call_if_match(
        &f,
        "PATCH",
        "/api/v1/assets/BTC",
        Some(f.write_token.as_str()),
        &etag,
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(err_code(&v), "validation");

    // 对的 If-Match → 200，kind 改掉，ETag 随之变化
    let (s, hdr2, v) = call_if_match(
        &f,
        "PATCH",
        "/api/v1/assets/BTC",
        Some(f.write_token.as_str()),
        &etag,
        Some(json!({ "kind": "token" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["kind"], "token", "{v}");
    let etag2 = v["etag"].as_str().expect("新 etag").to_string();
    assert_ne!(etag2, etag, "行变了 → ETag 必须变");
    assert!(hdr2.get(header::ETAG).is_some());

    // 手填 external_id → 服务端置 match_status = matched
    let (s, _, v) = call_if_match(
        &f,
        "PATCH",
        "/api/v1/assets/BTC",
        Some(f.write_token.as_str()),
        &etag2,
        Some(json!({ "external_id": "manual-bitcoin" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["external_id"], "manual-bitcoin");
    assert_eq!(v["match_status"], "matched", "{v}");
    let etag3 = v["etag"].as_str().unwrap().to_string();

    // 传 null → 清空并回落 not_found（允许撤销手填）
    let (s, _, v) = call_if_match(
        &f,
        "PATCH",
        "/api/v1/assets/BTC",
        Some(f.write_token.as_str()),
        &etag3,
        Some(json!({ "external_id": null })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["external_id"].is_null(), "{v}");
    assert_eq!(v["match_status"], "not_found", "{v}");

    // kind 改动不参与 instrument.row_hash（docs/14 §4 判断 3）
    let (_, _, ins) = call(
        &f,
        "GET",
        "/api/v1/instruments/binance/swap/BTCUSDT",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(ins["row_hash"], row_hash_before, "手改字段不得进 row_hash");

    // 审计留痕（动作名 asset_update）
    let (_, _, v) = call(
        &f,
        "GET",
        "/api/v1/audit?action=asset_update",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(v["total"], 3, "PATCH 三次都留审计：{v}");

    // ── ② GET /assets/{code}/contracts：裸数组 ──────────────────────────
    let (s, _, body) = call_raw(
        &f,
        "GET",
        "/api/v1/assets/BTC/contracts",
        Some(f.read_token.as_str()),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let arr: Value = serde_json::from_str(&body).expect("裸数组");
    assert!(arr.is_array(), "未列 limit/offset 的端点返回裸数组：{body}");
    assert_eq!(
        arr.as_array().unwrap().len(),
        0,
        "没合约信息 = []（不是 404）：{body}"
    );

    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/assets/NOPE/contracts",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "资产本身不存在才 404：{v}");

    // ── ③ POST /sync/offchain/runs（202，forced 模式）──────────────────
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/sync/offchain/runs",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "触发类接口只读 token 不放行：{v}");

    // 未接线的外部源 → error 有值，但**照样 202 + 落行**
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/sync/offchain/runs",
        Some(f.write_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["ran"], true, "{v}");
    let run1 = v["offchain_run_id"]
        .as_i64()
        .expect("每轮落一条 offchain_sync_run");
    assert!(v["error"].as_str().is_some(), "未接线 → error 非空：{v}");
    assert_eq!(v["outcome"]["rate_limited"], 0, "限速字段是计数：{v}");

    // 注入真适配器（离线假实现）→ 重跑
    f.shared
        .lock()
        .await
        .set_offchain_provider(Arc::new(FakeOffchain));
    let (s, _, v) = call(
        &f,
        "POST",
        "/api/v1/sync/offchain/runs",
        Some(f.write_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["error"], Value::Null, "成功轮 error = null：{v}");
    let outcome = v["outcome"].clone();
    assert_eq!(outcome["matched"], 1, "BTC 精确唯一命中：{outcome}");
    assert_eq!(outcome["not_found"], 2, "ETH / USDT 未收录：{outcome}");
    assert_eq!(outcome["fetched"], 1, "{outcome}");
    assert_eq!(outcome["detail_pulled"], 1, "{outcome}");
    assert_eq!(outcome["contracts"], 1, "{outcome}");
    assert_eq!(outcome["rate_limited"], 0, "{outcome}");
    let run2 = v["offchain_run_id"].as_i64().expect("第二轮也落行");
    assert!(run2 > run1, "{run1} → {run2}");
    assert!(v["ts"].as_u64().is_some(), "{v}");

    // ── ④ GET /sync/offchain/status：归一化三字段 ───────────────────────
    let (s, _, v) = call(
        &f,
        "GET",
        "/api/v1/sync/offchain/status",
        Some(f.read_token.as_str()),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["running"], false, "{v}");
    let last = &v["last_run"];
    assert!(last.is_object(), "last_run 应是最近一轮：{v}");
    assert_eq!(last["id"].as_i64(), Some(run2), "{last}");
    assert!(
        last["started_at"].is_number() && last["finished_at"].is_number(),
        "{last}"
    );
    assert_eq!(last["error"], Value::Null, "{last}");
    assert_eq!(last["outcome"]["matched"], 1, "{last}");
    assert!(
        v["detail_synced_at"].is_number(),
        "详情拉过 → detail_synced_at 有值：{v}"
    );

    // 匹配结果真的落到币行上（P5 全局资产 Tab）
    let (_, _, v) = call(&f, "GET", "/api/v1/assets/BTC", Some(f.read_token.as_str()), None).await;
    assert_eq!(v["external_id"], "bitcoin", "{v}");
    assert_eq!(v["match_status"], "matched", "{v}");
    assert_eq!(v["detail_source"], "coingecko", "{v}");
    assert!(v["twitter_url"].as_str().is_some(), "{v}");

    // 合约地址子表（详情抽屉）
    let (s, _, body) = call_raw(
        &f,
        "GET",
        "/api/v1/assets/BTC/contracts",
        Some(f.read_token.as_str()),
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let arr: Value = serde_json::from_str(&body).expect("裸数组");
    let items = arr.as_array().expect("数组");
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["chain"], "TRON", "{body}");
    assert_eq!(
        items[0]["address"].as_str().unwrap(),
        "TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t"
    );
    assert_eq!(items[0]["is_primary"], 1, "0/1 整数（接口契约）：{body}");
    assert_eq!(items[0]["source"], "coingecko", "{body}");

    // `sync_run` 与 `offchain_sync_run` 完全分离（14 §3 失败隔离）
    let (_, _, v) = call(&f, "GET", "/api/v1/sync/runs", Some(f.read_token.as_str()), None).await;
    assert_eq!(v["total"], 1, "外部同步不写 sync_run：{v}");
}
