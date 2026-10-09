//! HTTP 服务层（`docs/13-控制面管理界面与API.md`，O4 选定 axum）。
//!
//! 落位（§2.4）：与 `serve` **同进程**、同一把 `ControlPlane` 锁、同一个 `SqliteStore`；
//! 界面与自动化脚本是控制面的**客户端**，不是平台节点（§1.1 A4），HTTP 只存在于
//! 「人 ↔ 控制面」这一条边界上，配置下发仍只走总线（§5.5）。
//!
//! ## 本模块负责的四件事
//!
//! 1. **鉴权**（§5.1）：用户名 + 密码登录换 **JWT（HS256）**，后续请求带
//!    `Authorization: Bearer <jwt>`；角色取自 JWT `role`（源头是 `control_user` 表）；
//!    `X-Actor` → `publisher = <instance>/<actor>`；
//!    **免鉴权只有** `/healthz`（含 `/api/v1/healthz`）与 `/auth/login`；
//! 2. **错误模型**（§4.2）：`ControlError` → `{"error":{code,message,detail}}` + HTTP 状态；
//! 3. **并发控制**（§5.3）：配置域 `base_version` → 409；行级 `If-Match` → 412 / 428；
//! 4. **路由与分页**（§4.1）：`?limit=&offset=` → `{items,total,limit,offset}`。
//!
//! **未实现的接口返回 501 `not_implemented`**，未知路径返回 404 `not_found`
//! —— 「没有这条路」与「这条路还没修」必须可区分（不静默 404）。
//! 501 是 §4.2 错误表之外**新增**的一个 `code`，见报告的偏差清单。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex, MutexGuard};

use crate::assign::{assign, next_assignment_version, AssignRequest, AssignmentResult, Candidate};
use crate::catalog_repo::{
    paginate, AssetContractRecord, AssetFilter, AssetPatch, AssetRecord, AuditFilter, AuditRow,
    ChangeFilter, ChangeRecord, ExchangeFilter, ExchangeMarketRecord, ExchangeRecord,
    InstrumentFilter, InstrumentRecord, NewExchange, NewMarket, OffchainSyncRunRecord, PageOut,
    SyncRunFilter, SyncRunRecord, VenueAssetFilter, VenueAssetRecord,
};
use crate::auth::{AuthError, BootstrapInfo, BootstrapMode, Claims, Jwt, NewUser, UserPatch};
use crate::error::ControlError;
use crate::plane::ControlPlane;
use crate::repo::{NewNode, NewServer, NodeRecord, ServerRecord};
use crate::scope::{self, ScopeRules};
use crate::schema::{
    BlocklistDoc, CapacityLimits, PlanInstrument, ProcessCapacityDoc, QuotaSpec, Strategy,
    SubscribePlanDoc,
};
use crate::sqlite::{PlanRow, SqliteStore};
use crate::state::{Convergence, InstStatus, NotConverged, SnapshotKey};
use crate::store::{self, AuditEntry, ConfigStore, StoredDoc};
use crate::validate;
use ponk_eventcenter::EventCenter;
use ponk_protocol::{
    canonical_view, hash_text, ConfigDomain, NodeKind, PlanMode, ShardPlan, SubscribePlan,
};

// ─────────────────────────────────────────────────────────────────────────────
// §4.2 错误模型
// ─────────────────────────────────────────────────────────────────────────────

/// 统一错误（响应体 `{ "error": { code, message, detail } }`）。
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    detail: Value,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
            detail: Value::Null,
        }
    }

    /// 携带结构化补充（当前版本、diff 摘要等）。
    pub fn detail(mut self, d: Value) -> Self {
        self.detail = d;
        self
    }

    pub fn unauthenticated(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthenticated", m)
    }
    /// `docs/13 §5.1.1`：凭证错误（**不区分**用户不存在 / 密码错，防枚举）。
    pub fn invalid_credentials(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "invalid_credentials", m)
    }
    /// `docs/13 §5.1.1`：JWT 过期。
    pub fn token_expired(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "token_expired", m)
    }
    /// `docs/13 §5.1.1`：用户表为空（`--bootstrap none` + 空库）。
    pub fn no_users(m: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "no_users", m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", m)
    }
    pub fn validation(m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "validation", m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", m)
    }
    pub fn precondition_failed(m: impl Into<String>) -> Self {
        Self::new(StatusCode::PRECONDITION_FAILED, "precondition_failed", m)
    }
    pub fn precondition_required(m: impl Into<String>) -> Self {
        Self::new(
            StatusCode::PRECONDITION_REQUIRED,
            "precondition_required",
            m,
        )
    }
    pub fn illegal_state(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "illegal_state", m)
    }
    pub fn busy(m: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "busy", m)
    }
    /// `ControlError::{Db,Io,Json,Protocol,Ec}` —— 不回显内部细节。
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "控制面内部错误（详见进程日志）",
        )
    }
    /// **§4.2 之外新增**：已登记但尚未实现的接口。
    pub fn not_implemented(what: impl Into<String>) -> Self {
        let what = what.into();
        Self::new(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented",
            format!("接口 {what} 尚未实现（docs/13 §8 阶段三 / 阶段四）"),
        )
        .detail(json!({ "api": what, "stage": "docs/13 §8" }))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": { "code": self.code, "message": self.message, "detail": self.detail }
        });
        Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(body.to_string()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

/// §4.2：`ControlError` 变体 → `code` + HTTP 状态。
///
/// 注意 `ControlError::Illegal` 既承载「状态不允许」也承载部分 SQLite 失败
/// （`SqliteStore::with_write` 把 `rusqlite::Error` 归到 `Illegal`），
/// 因此后者会落 422 而非 500 —— 见报告的偏差清单。
impl From<ControlError> for ApiError {
    fn from(e: ControlError) -> Self {
        match e {
            ControlError::NotFound(m) => ApiError::not_found(m),
            ControlError::Validation(m) => ApiError::validation(m),
            ControlError::Illegal(m) => ApiError::illegal_state(m),
            ControlError::Io(_) | ControlError::Json(_) | ControlError::Protocol(_)
            | ControlError::Ec(_) | ControlError::Db(_) => ApiError::internal(),
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// `AuthError`（`docs/13 §5.1.1`）→ HTTP 状态 + `code`。
impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::InvalidCredentials => {
                ApiError::invalid_credentials("用户名或密码错误")
            }
            AuthError::NoUsers => ApiError::no_users(
                "控制面还没有任何用户：请先执行 `ponk-control user add --username <u> --role <write>`",
            ),
            AuthError::TokenExpired => ApiError::token_expired("登录已过期，请重新登录"),
            AuthError::InvalidToken => ApiError::unauthenticated("令牌无效"),
            AuthError::UsernameExists => ApiError::conflict("用户名已存在"),
            AuthError::UserNotFound => ApiError::not_found("用户不存在"),
            AuthError::Validation(m) => ApiError::validation(m),
            // 内部错误：不回显库 / 哈希细节（其中可能含 DDL 片段）。
            AuthError::Db(_) | AuthError::Crypto(_) => ApiError::internal(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §5.1 鉴权：用户名密码登录 + JWT（HS256）
// ─────────────────────────────────────────────────────────────────────────────

/// 角色（`read` / `write`）——定义在 [`crate::auth`]，此处重导出以保留 `http::Role` 路径。
pub use crate::auth::Role;

/// 鉴权配置（`docs/13 §5.1`：启动参数，**不进八域、不进 `control.db`**）。
///
/// 承载三件事：JWT 密钥与有效期、首启引导口令、首启引导开关。
#[derive(Debug, Clone)]
pub struct AuthConfig {
    jwt: Jwt,
    ttl_secs: u64,
    /// 首启引导口令（`PONK_ADMIN_PASSWORD` / `--admin-password`）。
    pub admin_password: Option<String>,
    /// 首启引导开关（`--bootstrap default|none`）。
    pub bootstrap: BootstrapMode,
}

impl AuthConfig {
    /// 构造鉴权配置。
    ///
    /// * `secret`：`--jwt-secret` 优先，缺省读 `PONK_JWT_SECRET`；
    ///   **缺失或为空一律报错**——`docs/13 §5.1`：不得自动生成随机密钥
    ///   （重启会使全部令牌失效，多实例也不一致），故 HTTP 服务拒绝启动。
    /// * `ttl_secs`：`--jwt-ttl`（支持 `8h` / `30m` / `900` / `45s`），缺省 8h。
    /// * `admin_password`：`--admin-password` 优先，缺省读 `PONK_ADMIN_PASSWORD`。
    pub fn new(
        secret: Option<String>,
        ttl_secs: Option<u64>,
        admin_password: Option<String>,
        bootstrap: BootstrapMode,
    ) -> Result<Self, String> {
        let secret = secret.ok_or_else(|| {
            "缺少 JWT 签名密钥：HTTP 面拒绝启动。\n  请二选一：\n    1) 传参 --jwt-secret <secret>\n    2) 环境变量 PONK_JWT_SECRET=<secret>\n  docs/13 §5.1：**不会**自动生成随机密钥（重启会让所有令牌失效、多实例不一致）。".to_string()
        })?;
        if secret.is_empty() {
            return Err(
                "JWT 签名密钥为空：`--jwt-secret ''` / `PONK_JWT_SECRET=''` 不被接受。"
                    .to_string(),
            );
        }
        Ok(AuthConfig {
            jwt: Jwt::new(secret),
            ttl_secs: ttl_secs.unwrap_or(crate::auth::DEFAULT_JWT_TTL_SECS),
            admin_password,
            bootstrap,
        })
    }

    /// 未开 HTTP 时的占位配置：没有鉴权面，故不需要密钥，也不做首启引导。
    pub fn offline() -> Self {
        AuthConfig {
            jwt: Jwt::new(String::new()),
            ttl_secs: crate::auth::DEFAULT_JWT_TTL_SECS,
            admin_password: None,
            bootstrap: BootstrapMode::Disabled,
        }
    }

    /// 令牌有效期（秒）。
    pub fn ttl(&self) -> u64 {
        self.ttl_secs
    }

    /// 签发 JWT（`docs/13 §5.1.1` `POST /auth/login`）。
    pub fn issue(&self, username: &str, role: Role, ttl_secs: u64) -> Result<String, AuthError> {
        self.jwt.issue(username, role, ttl_secs)
    }

    /// 校验 JWT：过期 → [`AuthError::TokenExpired`]。
    pub fn verify(&self, token: &str) -> Result<Claims, AuthError> {
        self.jwt.verify(token)
    }
}

/// 请求上下文（中间件注入）。
#[derive(Debug, Clone)]
pub struct AuthCtx {
    /// 操作者（`X-Actor` 覆盖 `sub`；缺省 = `sub`）。
    pub actor: String,
    /// 角色（来自 JWT `role`，其源头是用户表）。
    pub role: Role,
    /// JWT `sub`（用户名；不受 `X-Actor` 影响）。
    pub sub: String,
    /// JWT `exp`（epoch 秒）。
    pub exp: u64,
    /// JWT `iat`（epoch 秒）。
    pub iat: u64,
}

/// 免鉴权路径（`docs/13 §5.1`：**只有** `/healthz`（含 `/api/v1/healthz`）与 `/auth/login`）。
fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/healthz" | "/api/v1/healthz" | "/auth/login" | "/api/v1/auth/login"
    )
}

/// 取 `Authorization: Bearer <jwt>`。
fn bearer(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        })
        .filter(|t| !t.is_empty())
}

/// 鉴权中间件：校验 JWT → **回查用户表确认存续并取角色** → 方法级角色判定
/// （`GET`/`HEAD` = `read`，其余需 `write`）→ 注入 [`AuthCtx`]
/// （`X-Actor` 仍可覆盖 `sub`，但**仍受角色约束**）。
///
/// **为什么回查**：令牌的角色是签发时的快照；停用账号 / 降权后若只验签，
/// 旧令牌在 `exp`（默认 8h）内仍有效——见 `docs/13 §5.1` 的吊销语义。
async fn auth_middleware<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    if is_public(&path) {
        return next.run(req).await;
    }

    let Some(token) = bearer(&req) else {
        return ApiError::unauthenticated(
            "缺少 `Authorization: Bearer <jwt>`（先 `POST /auth/login` 换取令牌，docs/13 §5.1）",
        )
        .into_response();
    };

    let claims = match st.auth.verify(token) {
        Ok(c) => c,
        Err(AuthError::TokenExpired) => {
            return ApiError::token_expired("登录已过期，请重新登录").into_response();
        }
        Err(AuthError::InvalidToken) => {
            return ApiError::unauthenticated("令牌无效").into_response();
        }
        Err(_) => return ApiError::internal().into_response(),
    };
    let Some(role) = Role::parse(&claims.role) else {
        return ApiError::unauthenticated("令牌角色非法").into_response();
    };

    // ── 账号级吊销（docs/13 §5.1）──────────────────────────────────────────
    // JWT 只证明「**是谁 + 是否过期**」，**存续与权限一律以用户表为准**：
    // 否则把 write 降为 read、或停用账号之后，旧令牌在 `exp` 之前仍持有原权限，
    // 而 `exp` 默认 8 小时——等于没有吊销。
    //
    // 代价是每请求一次本地 SQLite 读（WAL 单文件，亚毫秒级）；
    // 失败**闭合**：读不出用户表就不放行（宁可 500，也不放行一个无法确认存续的令牌）。
    let mut role = role;
    if let Some(users) = st.users.as_deref() {
        match users.get_user(&claims.sub) {
            Ok(Some(u)) if u.disabled => {
                return ApiError::unauthenticated("账号已停用或已删除，请重新登录").into_response();
            }
            Ok(Some(u)) => role = u.role, // 库里的角色覆盖令牌（降权即时生效）
            Ok(None) => {
                return ApiError::unauthenticated("账号不存在，请重新登录").into_response();
            }
            Err(_) => {
                return ApiError::internal().into_response();
            }
        }
    }

    let method = req.method().clone();
    let is_read = matches!(method, axum::http::Method::GET | axum::http::Method::HEAD);
    if !is_read && role == Role::Read {
        return ApiError::forbidden(format!(
            "当前角色为 read，不允许 {method}（写接口与触发类需 write，docs/13 §5.1）"
        ))
        .into_response();
    }

    let actor = req
        .headers()
        .get("X-Actor")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| claims.sub.clone());

    let mut req = req;
    req.extensions_mut().insert(AuthCtx {
        actor,
        role,
        sub: claims.sub,
        exp: claims.exp,
        iat: claims.iat,
    });
    next.run(req).await
}

// ─────────────────────────────────────────────────────────────────────────────
// 应用状态与锁
// ─────────────────────────────────────────────────────────────────────────────

/// HTTP 侧的锁等待上限（§2.4「锁等待超时返回 503」/ §4.2 `busy`）。
pub const DEFAULT_LOCK_WAIT_MS: u64 = 2_000;

/// 路由共享状态。
pub struct AppState<E: EventCenter + 'static> {
    /// 与 `poll` 循环共用的一把锁（§2.4）。
    plane: Arc<Mutex<ControlPlane<E>>>,
    auth: AuthConfig,
    /// `control_user` 持久层（`docs/12 §3.8`）——认证端点全走它。
    users: Option<Arc<SqliteStore>>,
    /// 首启引导结果（启动日志 + `POST /auth/login` 的 `default_password`）。
    bootstrap: Option<BootstrapInfo>,
    /// `admin` 是否仍在用内置默认口令（`PATCH` 改密后清零）。
    admin_default: Arc<AtomicBool>,
    instance_id: String,
    started_at: u64,
    lock_wait: Duration,
}

// 手写 Clone：`#[derive]` 会给 `E` 加上 `E: Clone`，而 `EventCenter` 不要求 Clone。
impl<E: EventCenter + 'static> Clone for AppState<E> {
    fn clone(&self) -> Self {
        AppState {
            plane: self.plane.clone(),
            auth: self.auth.clone(),
            users: self.users.clone(),
            bootstrap: self.bootstrap.clone(),
            admin_default: self.admin_default.clone(),
            instance_id: self.instance_id.clone(),
            started_at: self.started_at,
            lock_wait: self.lock_wait,
        }
    }
}

impl<E: EventCenter + 'static> AppState<E> {
    /// 构造状态：**首启引导**（`docs/13 §5.1`：空库自动建内置管理员）
    /// → 读出 `instance_id` / `started_at` → 把控制面**移入**共享锁。
    ///
    /// 返回 `(状态, 共享锁)`：调用方（`serve` 的轮询线程 / 测试）拿后者驱动控制面。
    pub fn build(
        plane: ControlPlane<E>,
        auth: AuthConfig,
        lock_wait: Duration,
    ) -> Result<(Self, Arc<Mutex<ControlPlane<E>>>), ControlError> {
        let instance_id = plane.config().instance_id.clone();
        let started_at = plane.started_at();
        let users = plane.store().sqlite().cloned();
        let bootstrap = match &users {
            Some(store) => crate::auth::ensure_bootstrap_admin(
                store,
                auth.admin_password.as_deref(),
                auth.bootstrap,
            )?,
            None => None,
        };
        let admin_default = Arc::new(AtomicBool::new(
            bootstrap.as_ref().map(|b| b.default_password).unwrap_or(false),
        ));
        let shared = Arc::new(Mutex::new(plane));
        let st = AppState {
            plane: shared.clone(),
            auth,
            users,
            bootstrap,
            admin_default,
            instance_id,
            started_at,
            lock_wait,
        };
        Ok((st, shared))
    }

    /// `publisher` / `audit_log.actor` 的口径：`<instance>/<actor>`（09 §3.1）。
    pub fn publisher(&self, ctx: &AuthCtx) -> String {
        format!("{}/{}", self.instance_id, ctx.actor)
    }

    /// 控制面实例 id。
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// 用户表句柄（`control_user`）；`None` = 文件后端（无用户表，认证端点 500）。
    pub fn users(&self) -> Option<&SqliteStore> {
        self.users.as_deref()
    }

    /// 首启引导结果（本次启动是否创建了内置管理员）。
    pub fn bootstrap(&self) -> Option<&BootstrapInfo> {
        self.bootstrap.as_ref()
    }

    /// `admin` 是否仍在用内置默认口令。
    pub fn admin_uses_default_password(&self) -> bool {
        self.admin_default.load(Ordering::SeqCst)
    }

    /// 管理员改密后清除「默认口令」标注。
    pub fn clear_default_password_flag(&self) {
        self.admin_default.store(false, Ordering::SeqCst);
    }

    /// 取用户表（缺失 → 500）。
    fn store(&self) -> ApiResult<&SqliteStore> {
        self.users
            .as_deref()
            .ok_or_else(|| ApiError::internal())
    }
}

/// 取控制面锁；等待超过上限 → 503 `busy`（§2.4 / §4.2）。
async fn lock_plane<E: EventCenter + 'static>(
    st: &AppState<E>,
) -> ApiResult<MutexGuard<'_, ControlPlane<E>>> {
    match tokio::time::timeout(st.lock_wait, st.plane.lock()).await {
        Ok(g) => Ok(g),
        Err(_) => Err(ApiError::busy(
            "控制面锁等待超时（poll 循环持锁），请稍后重试",
        )),
    }
}

/// 阻塞版取锁（给 [`run_blocking`] 用）：`spawn_blocking` 的线程不在 runtime 上下文里，
/// 不能用 `tokio::time::timeout`（要 timer reactor），改成带截止时间的 `try_lock` 轮询。
fn lock_plane_blocking<E: EventCenter + 'static>(
    plane: &Arc<Mutex<ControlPlane<E>>>,
    wait: Duration,
) -> ApiResult<MutexGuard<'_, ControlPlane<E>>> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        if let Ok(g) = plane.try_lock() {
            return Ok(g);
        }
        if std::time::Instant::now() >= deadline {
            return Err(ApiError::busy(
                "控制面锁等待超时（poll 循环持锁），请稍后重试",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 在**阻塞线程池**上持锁执行一段重活（同步触发类 handler 专用）。
///
/// 这不是风格问题，是**必须**——两类问题一起解决：
///
/// 1. **panic（实测踩到）**：`reqwest::blocking` 在 debug 构建里会
///    `Builder::new_current_thread().build()` 造一个"壳" Runtime 再立刻 drop
///    （reqwest 0.12 `blocking/wait.rs::enter` 的 `cfg(debug_assertions)` 分支），
///    而 tokio **禁止在异步上下文 drop Runtime** ——
///    `Cannot drop a runtime in a context where blocking is not allowed`，
///    handler 任务直接死掉，连接被掐断（curl 只看到 `000`）。
///    在 `spawn_blocking` 线程上则完全合法（那里"允许阻塞"）。
/// 2. **饿死 worker**：一轮同步要串行打多个交易所端点，超时叠加可达 30s+；
///    在 handler 里同步执行会把整个 worker 占满，同期请求全部 503。
///
/// 调用方必须确保闭包里没有 `.await`，且**不捕获** handler 的任何借用。
async fn run_blocking<E, F, T>(st: &AppState<E>, f: F) -> ApiResult<T>
where
    E: EventCenter + 'static,
    F: FnOnce(&mut ControlPlane<E>) -> ApiResult<T> + Send + 'static,
    T: Send + 'static,
{
    let plane = st.plane.clone();
    let lock_wait = st.lock_wait;
    let joined = tokio::task::spawn_blocking(move || -> ApiResult<T> {
        let mut g = lock_plane_blocking(&plane, lock_wait)?;
        f(&mut g)
    })
    .await;
    match joined {
        Ok(r) => r,
        Err(e) => {
            // 闭包 panic（内部 bug）：如实记日志，对外只给 500，不回显细节
            eprintln!("[http] 同步任务线程异常退出：{e}");
            Err(ApiError::internal())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 通用工具
// ─────────────────────────────────────────────────────────────────────────────

fn json_response(status: StatusCode, body: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| ApiError::internal().into_response())
}

fn json_ok(body: Value) -> Response {
    json_response(StatusCode::OK, body)
}

/// 带 `ETag` 响应头的 JSON（§5.3：行级 ETag 从记录的 `etag` 字段取）。
fn json_etag(status: StatusCode, body: Value) -> Response {
    let etag = body
        .get("etag")
        .and_then(|e| e.as_str())
        .map(str::to_string);
    let mut r = json_response(status, body);
    if let Some(e) = etag {
        if let Ok(v) = HeaderValue::from_str(&e) {
            r.headers_mut().insert(header::ETAG, v);
        }
    }
    r
}

fn json_created(body: Value) -> Response {
    json_response(StatusCode::CREATED, body)
}

fn json_accepted(body: Value) -> Response {
    json_response(StatusCode::ACCEPTED, body)
}

fn no_content() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap_or_else(|_| ApiError::internal().into_response())
}

/// 解析配置域（与 CLI `publish` / `audit` / `current` **同一实现**，§6.3 不得分叉）。
pub fn parse_domain(s: &str) -> Result<ConfigDomain, ControlError> {
    let norm = s
        .trim()
        .trim_start_matches("cfg.")
        .trim_start_matches("ctl.")
        .to_ascii_lowercase()
        .replace('-', "_");
    let flat = norm.replace('_', "");
    ConfigDomain::ALL
        .into_iter()
        .find(|d| d.as_str() == norm || format!("{d:?}").to_ascii_lowercase() == flat)
        .ok_or_else(|| {
            ControlError::not_found(format!(
                "未知配置域 `{s}`（八域：{}）",
                ConfigDomain::ALL
                    .iter()
                    .map(|d| d.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            ))
        })
}

/// §4.1 分页参数（默认 50，最大 500）。
#[derive(Debug, Deserialize, Default)]
pub struct Page {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

impl Page {
    /// 归一化为 `(limit, offset)`。
    pub fn clamp(&self) -> (usize, usize) {
        let limit = self.limit.unwrap_or(50).clamp(1, 500);
        let offset = self.offset.unwrap_or(0);
        (limit, offset)
    }

    /// 切片并组装 `(页, total, limit, offset)`。
    pub fn slice<T>(&self, items: Vec<T>) -> (Vec<T>, usize, usize, usize) {
        let (limit, offset) = self.clamp();
        let total = items.len();
        let page = if offset >= total {
            Vec::new()
        } else {
            let end = (offset + limit).min(total);
            items.into_iter().skip(offset).take(end - offset).collect()
        };
        (page, total, limit, offset)
    }
}

fn paged<T: serde::Serialize>(items: &[T], total: usize, limit: usize, offset: usize) -> Value {
    json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    })
}

/// 行级 ETag（§5.3：`W/"<id>-<updated_at>"`）。
fn row_etag(id: i64, updated_at: u64) -> String {
    format!("W/\"{id}-{updated_at}\"")
}

/// 域文档 ETag（§4.9：内容哈希）。
fn content_etag(hash: &str) -> String {
    format!("\"{hash}\"")
}

/// 归一化 `If-Match`：剥掉弱标记与引号后比较。
fn normalize_etag(v: &str) -> String {
    let v = v.trim();
    let v = v.strip_prefix("W/").unwrap_or(v);
    v.trim().trim_matches('"').to_string()
}

/// §5.3 行级并发：缺 `If-Match` → 428；不符 → 412。
fn require_if_match(headers: &HeaderMap, current: &str) -> ApiResult<()> {
    let raw = headers
        .get(header::IF_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return Err(ApiError::precondition_required(
            "写接口缺少 If-Match（docs/13 §5.3）",
        ));
    };
    if raw == "*" || normalize_etag(raw) == normalize_etag(current) {
        return Ok(());
    }
    Err(ApiError::precondition_failed(format!(
        "If-Match 与当前 ETag 不符：期望 {current}，得到 {raw}"
    )))
}

fn content_hash_of(v: &Value) -> String {
    hash_text(&canonical_view(v))
}

/// 某类节点是否消费该域（09 §4.4 收敛判据同口径）。
fn consumes_domain(kind: NodeKind, d: ConfigDomain) -> bool {
    if kind == NodeKind::Collector {
        matches!(
            d,
            ConfigDomain::SubscribePlan | ConfigDomain::InstrumentBlocklist
        )
    } else {
        validate::consumed_domains(kind).contains(&d)
    }
}

fn not_converged_json(n: &NotConverged) -> Value {
    json!({
        "instance_id": n.instance_id,
        "domain": n.domain.map(|d| d.as_str()),
        "expected": n.expected,
        "actual": n.actual,
        "held_ms": n.held_ms,
    })
}

fn convergence_json(c: &Convergence, timeout_ms: u64) -> Value {
    json!({
        "converged": c.converged,
        "running": c.running,
        "stopped": c.stopped,
        "dead": c.dead,
        "not_converged": c.not_converged.iter().map(not_converged_json).collect::<Vec<_>>(),
        "not_converged_timeout": c.timed_out(timeout_ms),
    })
}

/// 快照键 + 收敛判定（P7 展示行）。
fn instance_json(key: &SnapshotKey, conv: &Convergence, timeout_ms: u64) -> Value {
    let mut v = serde_json::to_value(key).unwrap_or(Value::Null);
    match conv.not_converged.iter().find(|n| n.instance_id == key.instance_id) {
        Some(n) => {
            let mut o = not_converged_json(n);
            o["held_ms"] = json!(n.held_ms);
            o["timed_out"] = json!(n.held_ms >= timeout_ms);
            v["converged"] = json!(false);
            v["convergence"] = o;
        }
        None => {
            // §3.4：收敛 = 不在 `Convergence::not_converged` 里
            v["converged"] = json!(true);
        }
    }
    v
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.3 八域元数据（前端渲染用）
// ─────────────────────────────────────────────────────────────────────────────

const NODE_KINDS: [NodeKind; 7] = [
    NodeKind::Collector,
    NodeKind::Signal,
    NodeKind::Storage,
    NodeKind::Alert,
    NodeKind::Factor,
    NodeKind::DataSvc,
    NodeKind::Control,
];

fn kind_name(k: NodeKind) -> &'static str {
    match k {
        NodeKind::Collector => "collector",
        NodeKind::Signal => "signal",
        NodeKind::Storage => "storage",
        NodeKind::Alert => "alert",
        NodeKind::Factor => "factor",
        NodeKind::DataSvc => "data_svc",
        NodeKind::Control => "control",
    }
}

fn domain_display(d: ConfigDomain) -> &'static str {
    match d {
        ConfigDomain::SubscribePlan => "订阅计划",
        ConfigDomain::SignalRules => "信号规则",
        ConfigDomain::AlertRules => "告警规则",
        ConfigDomain::FactorRules => "因子规则",
        ConfigDomain::Channels => "渠道配置",
        ConfigDomain::ProcessCapacity => "进程承载",
        ConfigDomain::Serialization => "序列化",
        ConfigDomain::InstrumentBlocklist => "交易对黑名单",
    }
}

/// 传播类别（09 §2.2）：A 广播通知 + 定向应答；B A + 定向推送；C 不发通知。
fn push_class(d: ConfigDomain) -> &'static str {
    match d {
        ConfigDomain::SubscribePlan => "B",
        ConfigDomain::ProcessCapacity => "C",
        _ => "A",
    }
}

fn consumers_of(d: ConfigDomain) -> Vec<&'static str> {
    NODE_KINDS
        .into_iter()
        .filter(|k| validate::consumed_domains(*k).contains(&d))
        .map(kind_name)
        .collect()
}

fn domain_meta_json(d: ConfigDomain) -> Value {
    json!({
        "name": d.as_str(),
        "display": domain_display(d),
        "consumers": consumers_of(d),
        "push": push_class(d),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 请求体
// ─────────────────────────────────────────────────────────────────────────────

fn body_object(bytes: &Bytes) -> ApiResult<serde_json::Map<String, Value>> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| {
        ApiError::validation(format!("请求体不是合法 JSON：{e}"))
    })?;
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(ApiError::validation("请求体必须是 JSON 对象")),
    }
}

fn opt_bool(m: &serde_json::Map<String, Value>, k: &str) -> Option<bool> {
    m.get(k).and_then(|v| v.as_bool())
}

fn opt_i64(m: &serde_json::Map<String, Value>, k: &str) -> Option<i64> {
    m.get(k).and_then(|v| v.as_i64())
}

fn opt_str<'a>(m: &'a serde_json::Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(|v| v.as_str())
}

fn opt_str_vec(m: &serde_json::Map<String, Value>, k: &str) -> Option<Vec<String>> {
    m.get(k).and_then(|v| v.as_array()).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect()
    })
}

/// 由请求体 + （PUT 时的）既有记录组装 [`NewNode`]：
/// **出现即生效，缺省则沿用既有值**（`null` / `[]` 表示显式清空）。
fn node_from_body(
    m: &serde_json::Map<String, Value>,
    base: Option<&NodeRecord>,
    fallback_name: Option<&str>,
) -> ApiResult<NewNode> {
    let name = opt_str(m, "name")
        .map(str::to_string)
        .or_else(|| base.map(|b| b.name.clone()))
        .or_else(|| fallback_name.map(str::to_string))
        .unwrap_or_default();

    let mut req = match base {
        Some(b) => NewNode {
            name: b.name.clone(),
            scope: b.scope.clone(),
            scope_include: b.scope_include.clone(),
            scope_exclude: b.scope_exclude.clone(),
            runtime_params: b.runtime_params.clone(),
            server_id: b.server_id,
            note: b.note.clone(),
            enabled: b.enabled,
        },
        None => NewNode {
            name: name.clone(),
            ..NewNode::default()
        },
    };
    req.name = name;

    if m.contains_key("enabled") {
        req.enabled = opt_bool(m, "enabled").unwrap_or(true);
    }
    if let Some(v) = m.get("scope") {
        if !v.is_null() {
            req.scope = serde_json::from_value::<ScopeRules>(v.clone()).map_err(|e| {
                ApiError::validation(format!("scope 结构不合法：{e}"))
            })?;
        }
    }
    if let Some(v) = opt_str_vec(m, "scope_include") {
        req.scope_include = v;
    }
    if let Some(v) = opt_str_vec(m, "scope_exclude") {
        req.scope_exclude = v;
    }
    if let Some(v) = m.get("runtime_params") {
        req.runtime_params = if v.is_null() { json!({}) } else { v.clone() };
    }
    if m.contains_key("server_id") {
        req.server_id = opt_i64(m, "server_id");
    }
    if m.contains_key("note") {
        req.note = opt_str(m, "note").map(str::to_string);
    }
    Ok(req)
}

/// 服务器请求体 → [`NewServer`]（同上：出现即生效，缺省沿用既有）。
fn server_from_body(
    m: &serde_json::Map<String, Value>,
    base: Option<&ServerRecord>,
    fallback_host: Option<&str>,
) -> ApiResult<NewServer> {
    let host = opt_str(m, "host")
        .map(str::to_string)
        .or_else(|| fallback_host.map(str::to_string))
        .or_else(|| base.map(|b| b.host.clone()))
        .unwrap_or_default();

    let mut req = match base {
        Some(b) => NewServer {
            host: b.host.clone(),
            region: b.region.clone(),
            roles: b.roles.clone(),
            cpu_cores: b.cpu_cores,
            mem_gb: b.mem_gb,
            disk_gb: b.disk_gb,
            labels: b.labels.clone(),
            notes: b.notes.clone(),
        },
        None => NewServer {
            host: host.clone(),
            ..NewServer::default()
        },
    };
    // 路径里的 host 优先（§4.4：`/servers/{host}` 是寻址键）
    if let Some(h) = fallback_host {
        req.host = h.to_string();
    }
    if let Some(v) = opt_str(m, "region") {
        req.region = Some(v.to_string());
    } else if m.contains_key("region") {
        req.region = None;
    }
    if let Some(v) = opt_str_vec(m, "roles") {
        req.roles = v;
    }
    if let Some(v) = opt_i64(m, "cpu_cores") {
        req.cpu_cores = Some(v);
    } else if m.contains_key("cpu_cores") {
        req.cpu_cores = None;
    }
    if let Some(v) = opt_i64(m, "mem_gb") {
        req.mem_gb = Some(v);
    } else if m.contains_key("mem_gb") {
        req.mem_gb = None;
    }
    if let Some(v) = opt_i64(m, "disk_gb") {
        req.disk_gb = Some(v);
    } else if m.contains_key("disk_gb") {
        req.disk_gb = None;
    }
    if let Some(v) = m.get("labels").and_then(|v| v.as_object()) {
        let mut labels = BTreeMap::new();
        for (k, val) in v {
            if let Some(s) = val.as_str() {
                labels.insert(k.clone(), s.to_string());
            }
        }
        req.labels = labels;
    }
    if m.contains_key("notes") {
        req.notes = opt_str(m, "notes").map(str::to_string);
    }
    if req.host.trim().is_empty() {
        return Err(ApiError::validation("server.host 不能为空"));
    }
    Ok(req)
}

/// 服务器状态白名单（12 §3.5）。
fn check_server_status(s: &str) -> ApiResult<()> {
    match s {
        "active" | "maintenance" | "retired" => Ok(()),
        other => Err(ApiError::validation(format!(
            "server.status 取值必须是 active / maintenance / retired，得到 `{other}`"
        ))),
    }
}

/// R6（docs/13 §7.2）：节点与服务器写接口同步写 `audit_log`。
fn audit_write(
    db: &crate::sqlite::SqliteStore,
    actor: &str,
    action: &str,
    target: &str,
    before: Option<&Value>,
    after: Option<&Value>,
) -> ApiResult<()> {
    let detail = serde_json::to_string(&json!({ "before": before, "after": after }))
        .unwrap_or_else(|_| "{}".to_string());
    let now = crate::now_ms();
    let before_hash = before.map(content_hash_of);
    let after_hash = after.map(content_hash_of);
    db.with_write(|c| {
        c.execute(
            "INSERT INTO audit_log (actor, action, target, before_hash, after_hash, detail, ts)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![
                actor,
                action,
                target,
                before_hash,
                after_hash,
                detail,
                now as i64
            ],
        )?;
        Ok(())
    })
    .map_err(ApiError::from)
}

// ─────────────────────────────────────────────────────────────────────────────
// 路由
// ─────────────────────────────────────────────────────────────────────────────

/// 由状态构造 `axum::Router`（测试用 `tower::ServiceExt::oneshot` 进程内驱动）。
pub fn router<E: EventCenter + 'static>(st: AppState<E>) -> Router {
    Router::new()
        // ── §4.3 概览 ───────────────────────────────────────────────────────
        .route("/healthz", get(healthz::<E>))
        .route("/api/v1/healthz", get(healthz::<E>))
        // ── §5.1.1 认证（`/auth/login` 免鉴权，其余需 JWT）────────────────────
        // 同时挂 `/auth/*` 与 `/api/v1/auth/*`：前者是 §5.1.1 表格里的路径，
        // 后者是前端 `API_BASE = /api/v1` 拼出来的实际请求路径。
        .route("/auth/login", post(auth_login::<E>))
        .route("/api/v1/auth/login", post(auth_login::<E>))
        .route("/auth/me", get(auth_me::<E>))
        .route("/api/v1/auth/me", get(auth_me::<E>))
        .route("/auth/users", get(auth_list_users::<E>).post(auth_create_user::<E>))
        .route(
            "/api/v1/auth/users",
            get(auth_list_users::<E>).post(auth_create_user::<E>),
        )
        .route("/auth/users/{username}", patch(auth_patch_user::<E>))
        .route("/api/v1/auth/users/{username}", patch(auth_patch_user::<E>))
        .route("/api/v1/overview", get(overview::<E>))
        .route("/api/v1/meta", get(meta::<E>))
        // ── §4.4 服务器台账 ─────────────────────────────────────────────────
        .route("/api/v1/servers", get(list_servers::<E>).post(create_server::<E>))
        .route(
            "/api/v1/servers/{host}",
            get(get_server::<E>).put(put_server::<E>).delete(delete_server::<E>),
        )
        .route("/api/v1/servers/{host}/status", patch(patch_status::<E>))
        .route("/api/v1/servers/{host}/nodes", get(server_nodes::<E>))
        .route("/api/v1/servers/{host}/instances", get(server_instances::<E>))
        // ── §4.5 采集节点 ───────────────────────────────────────────────────
        .route("/api/v1/nodes", get(list_nodes::<E>).post(create_node::<E>))
        .route(
            "/api/v1/nodes/{name}",
            get(get_node::<E>).put(put_node::<E>).delete(delete_node::<E>),
        )
        .route("/api/v1/nodes/{name}/enabled", patch(patch_enabled::<E>))
        .route("/api/v1/nodes/{name}/scope/preview", get(scope_preview::<E>))
        .route("/api/v1/nodes/{name}/config.toml", get(node_toml::<E>))
        .route("/api/v1/nodes/{name}/runtime", get(node_runtime::<E>))
        .route("/api/v1/nodes/{name}/assignments", get(node_assignments::<E>))
        .route(
            "/api/v1/nodes/{name}/assignments/latest",
            get(node_assignment_latest::<E>),
        )
        // ── §4.9 配置域 ─────────────────────────────────────────────────────
        .route("/api/v1/config/domains", get(config_domains::<E>))
        .route(
            "/api/v1/config/{domain}",
            get(config_current::<E>).put(config_put::<E>),
        )
        .route("/api/v1/config/{domain}/versions", get(config_versions::<E>))
        .route(
            "/api/v1/config/{domain}/versions/{version}",
            get(config_version::<E>),
        )
        .route(
            "/api/v1/config/{domain}/versions/{version}/diff",
            get(config_version_diff::<E>),
        )
        .route("/api/v1/config/{domain}/audit", get(config_audit::<E>))
        .route("/api/v1/config/{domain}/validate", post(config_validate::<E>))
        .route("/api/v1/config/{domain}/publish", post(config_publish::<E>))
        .route("/api/v1/config/{domain}/rollback", post(config_rollback::<E>))
        .route("/api/v1/config/{domain}/export", get(config_export::<E>))
        .route(
            "/api/v1/config/{domain}/convergence",
            get(config_convergence::<E>),
        )
        // ── §4.10 运行时状态 ────────────────────────────────────────────────
        .route("/api/v1/runtime/instances", get(runtime_instances::<E>))
        .route(
            "/api/v1/runtime/instances/{instance_id}",
            get(runtime_instance::<E>),
        )
        .route(
            "/api/v1/runtime/instances/{instance_id}/repush",
            post(repush_instance::<E>),
        )
        .route("/api/v1/runtime/counts", get(runtime_counts::<E>))
        .route("/api/v1/convergence", get(convergence::<E>))
        // ── §4.12 元数据同步 ────────────────────────────────────────────────
        .route("/api/v1/sync/status", get(sync_status::<E>))
        .route("/api/v1/sync/targets", get(sync_targets::<E>))
        .route(
            "/api/v1/sync/runs",
            get(list_sync_runs::<E>).post(sync_run_now::<E>),
        )
        .route("/api/v1/sync/runs/{id}", get(get_sync_run::<E>))
        // ── docs/14 §9 外部（币）信息同步：独立节拍、独立记录 ────────────
        .route(
            "/api/v1/sync/offchain/runs",
            post(offchain_sync_run_now::<E>),
        )
        .route("/api/v1/sync/offchain/status", get(offchain_sync_status::<E>))
        // ── §4.6 交易所与市场 ──────────────────────────────────────────────
        .route("/api/v1/exchanges", get(list_exchanges::<E>).post(create_exchange::<E>))
        .route(
            "/api/v1/exchanges/{venue}",
            get(get_exchange::<E>)
                .put(put_exchange::<E>)
                .delete(delete_exchange::<E>),
        )
        .route(
            "/api/v1/exchanges/{venue}/enabled",
            patch(patch_exchange_enabled::<E>),
        )
        .route(
            "/api/v1/exchanges/{venue}/markets",
            get(list_markets::<E>).post(create_market::<E>),
        )
        .route(
            "/api/v1/exchanges/{venue}/markets/{market}",
            put(put_market::<E>),
        )
        .route(
            "/api/v1/exchanges/{venue}/markets/{market}/enabled",
            patch(patch_market_enabled::<E>),
        )
        // ── §4.7 交易对 ────────────────────────────────────────────────────
        .route("/api/v1/instruments/export", get(export_instruments::<E>))
        .route(
            "/api/v1/instruments/changes",
            get(list_instrument_changes::<E>),
        )
        .route(
            "/api/v1/instruments/changes/{id}",
            get(get_instrument_change::<E>),
        )
        .route("/api/v1/instruments", get(list_instruments::<E>))
        .route(
            "/api/v1/instruments/{venue}/{market}/{native_code}",
            get(get_instrument::<E>),
        )
        // ── §4.8 币信息 ────────────────────────────────────────────────────
        .route("/api/v1/assets/export", get(export_assets::<E>))
        .route("/api/v1/assets", get(list_assets::<E>))
        .route(
            "/api/v1/assets/{code}",
            get(get_asset::<E>).patch(patch_asset::<E>),
        )
        // docs/14 §9：详情抽屉的链 + 合约地址（`asset_contract` 子表）
        .route(
            "/api/v1/assets/{code}/contracts",
            get(get_asset_contracts::<E>),
        )
        .route("/api/v1/venues/{venue}/assets", get(list_venue_assets::<E>))
        // ── §4.13 审计日志（跨域全表）──────────────────────────────────────
        .route("/api/v1/audit/export", get(export_audit::<E>))
        .route("/api/v1/audit", get(list_audit::<E>))
        .route("/api/v1/audit/{id}", get(get_audit::<E>))
        // ── §4.11 指派结果 ────────────────────────────────────────────────
        .route(
            "/api/v1/assignments/holders",
            get(assignment_holders::<E>),
        )
        .route(
            "/api/v1/assignments/unassigned",
            get(assignment_unassigned::<E>),
        )
        .route(
            "/api/v1/assignments/dry-run",
            post(assignments_dry_run::<E>),
        )
        .route("/api/v1/assignments", get(list_assignments::<E>))
        .route(
            "/api/v1/assignments/{instance_id}/versions/{version}",
            get(assignment_version::<E>),
        )
        .route(
            "/api/v1/assignments/{instance_id}",
            get(get_assignment::<E>),
        )
        // ── 未知路径 → 404（与 501 可区分）──────────────────────────────────
        .fallback(fallback_404)
        .layer(middleware::from_fn_with_state(st.clone(), auth_middleware::<E>))
        .with_state(st)
}

/// 未实现的接口：501 + `{"error":{"code":"not_implemented",…}}`。
///
/// **保留此类别**：`docs/13 §4.2` 要求"已登记但未实现"必须是 501 而非 404，
/// 让界面能把"没实现"与"路径不存在"区分开。当前**所有已登记路由都已接线**，
/// 因此该 handler 暂无调用方——新增路由在实现前应挂它，不要用 404 顶替。
#[allow(dead_code)]
async fn not_implemented(req: Request) -> Response {
    ApiError::not_implemented(format!("{} {}", req.method(), req.uri().path())).into_response()
}

/// 未知路径：404。
async fn fallback_404(req: Request) -> Response {
    ApiError::not_found(format!(
        "未知路径 {} {}",
        req.method(),
        req.uri().path()
    ))
    .into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// §5.1.1 认证接口：login / me / users
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /auth/login` 请求体。
#[derive(Debug, Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

/// `POST /auth/users` 请求体。
#[derive(Debug, Deserialize)]
struct CreateUserBody {
    username: String,
    password: String,
    role: Role,
}

/// `write` 门禁：`GET /auth/users` 例外地要求 `write`（§5.1.1，方法级规则不覆盖它）。
fn require_write(ctx: &AuthCtx) -> Result<(), ApiError> {
    if ctx.role == Role::Write {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "该接口需要 write 角色（docs/13 §5.1.1）",
        ))
    }
}

/// `POST /auth/login`：用户名 + 密码 → JWT（**免鉴权**，`is_public`）。
///
/// * 用户表为空 → `403 no_users`；
/// * 凭证错误（不存在 / 密码错 / 已停用）→ 一律 `401 invalid_credentials`（防枚举）；
/// * 口令只做 argon2id 比对，**不返回、不进日志**。
async fn auth_login<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    body: Bytes,
) -> ApiResult<Response> {
    let req: LoginBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("请求体不合法：{e}")))?;
    let store = st.store()?;
    if store.count_users()? < 1 {
        return Err(AuthError::NoUsers.into());
    }
    let role = store
        .verify_credentials(&req.username, &req.password)?
        .ok_or(AuthError::InvalidCredentials)?;
    let ttl = st.auth.ttl();
    let token = st.auth.issue(&req.username, role, ttl)?;
    Ok(json_ok(json!({
        "access_token": token,
        "token_type": "Bearer",
        "expires_in": ttl,
        "role": role,
        "username": req.username,
        // §5.1：内置管理员仍在用内置默认口令时标注（改密后自动清除）
        "default_password": st.admin_uses_default_password()
            && req.username == crate::auth::DEFAULT_ADMIN_USERNAME,
    })))
}

/// `GET /auth/me`：从 JWT 解析身份（前端恢复会话与角色）。
async fn auth_me<E: EventCenter + 'static>(
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    State(_st): State<AppState<E>>,
) -> ApiResult<Response> {
    Ok(json_ok(json!({
        "username": ctx.sub,
        "role": ctx.role,
        "exp": ctx.exp,
        "iat": ctx.iat,
    })))
}

/// `GET /auth/users`：列表（**不含口令哈希**；`write` 才可读）。
async fn auth_list_users<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
) -> ApiResult<Response> {
    require_write(&ctx)?;
    let items = st.store()?.list_users()?;
    Ok(json_ok(json!({ "items": items })))
}

/// `POST /auth/users`：建用户（`write`；用户名重复 → `409`）。
async fn auth_create_user<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    body: Bytes,
) -> ApiResult<Response> {
    require_write(&ctx)?;
    let req: CreateUserBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("请求体不合法：{e}")))?;
    let rec = st.store()?.create_user(&NewUser {
        username: req.username,
        password: req.password,
        role: req.role,
    })?;
    Ok(json_response(
        StatusCode::CREATED,
        serde_json::to_value(&rec).map_err(|_| ApiError::internal())?,
    ))
}

/// `PATCH /auth/users/{username}`：改密 / 改角色 / 停用（`write`）。
async fn auth_patch_user<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(username): AxPath<String>,
    body: Bytes,
) -> ApiResult<Response> {
    require_write(&ctx)?;
    let patch: UserPatch = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("请求体不合法：{e}")))?;
    let rec = st.store()?.update_user(&username, &patch)?;
    if patch.password.is_some() && rec.username == crate::auth::DEFAULT_ADMIN_USERNAME {
        st.clear_default_password_flag();
    }
    Ok(json_ok(
        serde_json::to_value(&rec).map_err(|_| ApiError::internal())?,
    ))
}
// ─────────────────────────────────────────────────────────────────────────────
// §4.3 概览
// ─────────────────────────────────────────────────────────────────────────────

async fn healthz<E: EventCenter + 'static>(State(st): State<AppState<E>>) -> Response {
    json_ok(json!({
        "status": "ok",
        "instance_id": st.instance_id,
        "started_at": st.started_at,
    }))
}

async fn overview<E: EventCenter + 'static>(State(st): State<AppState<E>>) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let now = crate::now_ms();

    let mut domains = Vec::new();
    let mut recent: Vec<AuditEntry> = Vec::new();
    for d in ConfigDomain::ALL {
        let cur = g.store().current(d)?;
        let version = cur.as_ref().map(|c| c.version).unwrap_or(0);
        let audit = g.store().audit(d)?;
        let last = audit.last().cloned();
        recent.extend(audit);
        domains.push(json!({
            "domain": d.as_str(),
            "display": domain_display(d),
            "current_version": version,
            "updated_at": cur.as_ref().map(|c| c.ts).unwrap_or(0),
            "consumers": consumers_of(d),
            "push": push_class(d),
            "last_publisher": last.as_ref().map(|a| a.publisher.clone()),
            "last_published_at": last.as_ref().map(|a| a.ts).unwrap_or(0),
        }));
    }
    recent.sort_by_key(|a| std::cmp::Reverse(a.ts));
    recent.truncate(10);

    let conv = g.convergence(now);
    let timeout_ms = g.config().liveness.converge_timeout_ms;

    let cs = g.catalog_sync();
    let cat = cs.catalog();
    let sync = json!({
        "interval_ms": cs.interval_ms,
        "last_round_ms": cs.last_round_ms(),
        "running": cs.is_running(),
        "due": cs.due(now),
        "next_due_at": cs.last_round_ms().saturating_add(cs.interval_ms),
    });
    let catalog = json!({
        "updated_at": cat.updated_at,
        "symbols": cat.symbol_count(),
        "currencies": cat.currencies.values().map(|m| m.len()).sum::<usize>(),
    });

    Ok(json_ok(json!({
        "domains": domains,
        "instances": {
            "running": conv.running,
            "stopped": conv.stopped,
            "dead": conv.dead,
            "converged": conv.converged,
            "not_converged": conv.not_converged.len(),
            "not_converged_timeout": conv.timed_out(timeout_ms),
        },
        "not_converged": conv.not_converged.iter().map(not_converged_json).collect::<Vec<_>>(),
        "recent_publishes": recent,
        "sync": sync,
        "catalog": catalog,
    })))
}

async fn meta<E: EventCenter + 'static>(_st: State<AppState<E>>) -> Response {
    json_ok(json!({
        "domains": ConfigDomain::ALL.iter().map(|d| domain_meta_json(*d)).collect::<Vec<_>>(),
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.4 服务器台账
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct ServerQuery {
    status: Option<String>,
    role: Option<String>,
    q: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl ServerQuery {
    fn page(&self) -> Page {
        Page { limit: self.limit, offset: self.offset }
    }
}

fn sqlite_of<E: EventCenter + 'static>(
    g: &ControlPlane<E>,
) -> ApiResult<Arc<crate::sqlite::SqliteStore>> {
    g.store()
        .sqlite()
        .cloned()
        .ok_or_else(|| ApiError::internal())
}

fn filter_servers(all: Vec<ServerRecord>, q: &ServerQuery) -> Vec<ServerRecord> {
    let mut out: Vec<ServerRecord> = all
        .into_iter()
        .filter(|s| {
            if let Some(want) = q.status.as_deref() {
                if s.status != want {
                    return false;
                }
            }
            if let Some(role) = q.role.as_deref() {
                if !s.roles.iter().any(|r| r.eq_ignore_ascii_case(role)) {
                    return false;
                }
            }
            if let Some(kw) = q.q.as_deref().filter(|k| !k.is_empty()) {
                if !s.host.to_ascii_lowercase().contains(&kw.to_ascii_lowercase()) {
                    return false;
                }
            }
            true
        })
        .collect();
    let desc = q.order.as_deref() == Some("desc");
    out.sort_by(|a, b| {
        let ord = match q.sort.as_deref() {
            Some("status") => a.status.cmp(&b.status),
            Some("created_at") => a.created_at.cmp(&b.created_at),
            Some("updated_at") => a.updated_at.cmp(&b.updated_at),
            _ => a.host.cmp(&b.host),
        };
        if desc {
            ord.reverse()
        } else {
            ord
        }
    });
    out
}

fn server_json(s: &ServerRecord) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
    v["etag"] = json!(row_etag(s.id, s.updated_at));
    v
}

async fn list_servers<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<ServerQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let all = filter_servers(db.list_servers()?, &q);
    let (items, total, limit, offset) = q.page().slice(all);
    let body: Vec<Value> = items.iter().map(server_json).collect();
    Ok(json_ok(paged(&body, total, limit, offset)))
}

async fn create_server<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let req = server_from_body(&m, None, None)?;
    let before = db.server_by_host(&req.host).ok().map(|s| server_json(&s));
    let srv = db.upsert_server(&req)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        if before.is_some() {
            "server_update"
        } else {
            "server_create"
        },
        &srv.host,
        before.as_ref(),
        Some(&server_json(&srv)),
    )?;
    Ok(json_etag(StatusCode::CREATED, server_json(&srv)))
}

async fn get_server<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(host): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let srv = db.server_by_host(&host)?;
    Ok(json_etag(StatusCode::OK, server_json(&srv)))
}

async fn put_server<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(host): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = db.server_by_host(&host)?;
    require_if_match(&headers, &row_etag(existing.id, existing.updated_at))?;
    let req = server_from_body(&m, Some(&existing), Some(&host))?;
    let before = server_json(&existing);
    let srv = db.upsert_server(&req)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        "server_update",
        &host,
        Some(&before),
        Some(&server_json(&srv)),
    )?;
    Ok(json_etag(StatusCode::OK, server_json(&srv)))
}

async fn patch_status<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(host): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let status = opt_str(&m, "status").ok_or_else(|| ApiError::validation("缺少 status 字段"))?;
    check_server_status(status)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = db.server_by_host(&host)?;
    require_if_match(&headers, &row_etag(existing.id, existing.updated_at))?;
    let before = server_json(&existing);
    let now = crate::now_ms();
    db.with_write(|c| {
        c.execute(
            "UPDATE server SET status = ?1, updated_at = ?2 WHERE host = ?3",
            rusqlite::params![status, now as i64, host],
        )?;
        Ok(())
    })?;
    let srv = db.server_by_host(&host)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        "server_status",
        &host,
        Some(&before),
        Some(&server_json(&srv)),
    )?;
    Ok(json_etag(StatusCode::OK, server_json(&srv)))
}

async fn delete_server<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(host): AxPath<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = db.server_by_host(&host)?;
    require_if_match(&headers, &row_etag(existing.id, existing.updated_at))?;
    let linked = db
        .list_nodes()?
        .iter()
        .filter(|n| n.server_id == Some(existing.id))
        .count();
    if linked > 0 {
        return Err(ApiError::conflict(format!(
            "服务器 `{host}` 上仍登记了 {linked} 个采集节点，删除被关联记录阻止"
        ))
        .detail(json!({ "linked_nodes": linked })));
    }
    let before = server_json(&existing);
    db.with_write(|c| {
        c.execute("DELETE FROM server WHERE host = ?1", rusqlite::params![host])?;
        Ok(())
    })?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        "server_delete",
        &host,
        Some(&before),
        None,
    )?;
    Ok(no_content())
}

async fn server_nodes<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(host): AxPath<String>,
    Query(page): Query<Page>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let srv = db.server_by_host(&host)?;
    let mut nodes: Vec<NodeRecord> = db
        .list_nodes()?
        .into_iter()
        .filter(|n| n.server_id == Some(srv.id))
        .collect();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    let (items, total, limit, offset) = page.slice(nodes);
    let body: Vec<Value> = items.iter().map(node_json).collect();
    Ok(json_ok(paged(&body, total, limit, offset)))
}

async fn server_instances<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(host): AxPath<String>,
    Query(page): Query<Page>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let srv = db.server_by_host(&host)?;
    let now = crate::now_ms();
    let conv = g.convergence(now);
    let timeout_ms = g.config().liveness.converge_timeout_ms;
    let mut items: Vec<Value> = g
        .instances()
        .into_iter()
        .filter(|k| k.host.as_deref() == Some(srv.host.as_str()))
        .map(|k| instance_json(k, &conv, timeout_ms))
        .collect();
    items.sort_by(|a, b| {
        a["instance_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["instance_id"].as_str().unwrap_or(""))
    });
    let (page_items, total, limit, offset) = page.slice(items);
    Ok(json_ok(paged(&page_items, total, limit, offset)))
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.5 采集节点
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct NodeQuery {
    enabled: Option<String>,
    server_id: Option<i64>,
    status: Option<String>,
    converged: Option<String>,
    host: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl NodeQuery {
    fn page(&self) -> Page {
        Page { limit: self.limit, offset: self.offset }
    }
}

fn parse_bool(v: Option<&str>) -> Option<bool> {
    v.map(|s| matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn node_json(n: &NodeRecord) -> Value {
    let mut v = serde_json::to_value(n).unwrap_or(Value::Null);
    v["etag"] = json!(row_etag(n.id, n.updated_at));
    v
}

/// 列表行 = 定义 + 运行时 + 收敛（§4.5「节点 + 运行时联表行」）。
fn node_row_json(
    n: &NodeRecord,
    key: Option<&SnapshotKey>,
    conv: &Convergence,
    timeout_ms: u64,
    server_host: Option<&str>,
) -> Value {
    let mut v = node_json(n);
    v["server_host"] = match server_host {
        Some(h) => json!(h),
        None => Value::Null,
    };
    match key {
        Some(k) => {
            let runtime = instance_json(k, conv, timeout_ms);
            v["runtime_status"] = json!(k.status);
            v["runtime_host"] = json!(k.host);
            v["applied_version"] = json!(k.applied_version);
            v["last_assignment_version"] = json!(k.last_assignment_version);
            v["last_seen_ts"] = json!(k.last_seen_ts);
            v["converged"] = runtime["converged"].clone();
            v["runtime"] = runtime;
        }
        None => {
            v["runtime_status"] = Value::Null;
            v["runtime_host"] = Value::Null;
            v["applied_version"] = Value::Null;
            v["last_assignment_version"] = Value::Null;
            v["last_seen_ts"] = Value::Null;
            v["converged"] = Value::Null;
            v["runtime"] = Value::Null;
        }
    }
    v
}

async fn list_nodes<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<NodeQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let now = crate::now_ms();
    let conv = g.convergence(now);
    let timeout_ms = g.config().liveness.converge_timeout_ms;
    let keys: BTreeMap<&str, &SnapshotKey> =
        g.instances().into_iter().map(|k| (k.instance_id.as_str(), k)).collect();
    let hosts: BTreeMap<i64, String> = db
        .list_servers()?
        .into_iter()
        .map(|s| (s.id, s.host))
        .collect();

    let want_enabled = parse_bool(q.enabled.as_deref());
    let want_converged = parse_bool(q.converged.as_deref());
    let kw = q
        .q
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| !s.is_empty());

    let mut rows: Vec<NodeRecord> = db
        .list_nodes()?
        .into_iter()
        .filter(|n| {
            if let Some(w) = want_enabled {
                if n.enabled != w {
                    return false;
                }
            }
            if let Some(sid) = q.server_id {
                if n.server_id != Some(sid) {
                    return false;
                }
            }
            if let Some(host) = q.host.as_deref() {
                let expected = n.server_id.and_then(|id| hosts.get(&id)).map(|s| s.as_str());
                let runtime = keys.get(n.name.as_str()).and_then(|k| k.host.as_deref());
                if expected != Some(host) && runtime != Some(host) {
                    return false;
                }
            }
            if let Some(k) = &kw {
                if !n.name.to_ascii_lowercase().contains(k.as_str()) {
                    return false;
                }
            }
            let runtime_key = keys.get(n.name.as_str()).copied();
            if let Some(status) = q.status.as_deref() {
                let actual = runtime_key
                    .and_then(|k| serde_json::to_value(k.status).ok())
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unregistered".to_string());
                if actual != status {
                    return false;
                }
            }
            if let Some(want) = want_converged {
                let is_converged = conv
                    .not_converged
                    .iter()
                    .all(|nc| nc.instance_id != n.name);
                if is_converged != want {
                    return false;
                }
            }
            true
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    let (items, total, limit, offset) = q.page().slice(rows);
    let body: Vec<Value> = items
        .iter()
        .map(|n| {
            let key = keys.get(n.name.as_str()).copied();
            let host = n.server_id.and_then(|id| hosts.get(&id)).map(|s| s.as_str());
            node_row_json(n, key, &conv, timeout_ms, host)
        })
        .collect();
    Ok(json_ok(paged(&body, total, limit, offset)))
}

/// 取一个节点（不存在 → 404）。
fn require_node(db: &crate::sqlite::SqliteStore, name: &str) -> ApiResult<NodeRecord> {
    db.node_by_name(name)?
        .ok_or_else(|| ApiError::not_found(format!("采集节点 `{name}` 不存在")))
}

async fn create_node<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let req = node_from_body(&m, None, None)?;
    if req.name.trim().is_empty() {
        return Err(ApiError::validation("节点名称不能为空（它是 instance_id）"));
    }
    // §4.2：重复节点名在适配层先行判重 → conflict（而非 422）
    if db.node_by_name(&req.name)?.is_some() {
        return Err(ApiError::conflict(format!(
            "采集节点 `{}` 已存在（名称即唯一标识）",
            req.name
        )));
    }
    if let Some(sid) = req.server_id {
        if db.server_by_id(sid)?.is_none() {
            return Err(ApiError::validation(format!(
                "server_id={sid} 指向的服务器不存在"
            )));
        }
    }
    let node = db.create_node(&req)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        "node_create",
        &node.name,
        None,
        Some(&node_json(&node)),
    )?;
    Ok(json_etag(StatusCode::CREATED, node_json(&node)))
}

async fn get_node<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let node = require_node(&db, &name)?;
    let now = crate::now_ms();
    let conv = g.convergence(now);
    let timeout_ms = g.config().liveness.converge_timeout_ms;
    let key = g.registry().get(&name);
    let server_host = node
        .server_id
        .and_then(|id| db.server_by_id(id).ok().flatten())
        .map(|s| s.host);
    let mut v = node_row_json(&node, key, &conv, timeout_ms, server_host.as_deref());
    // 最新指派摘要（§4.5「+ 最新指派摘要」）
    v["assignment"] = match g.plan_of(&name) {
        Some(p) => json!({
            "assignment_version": p.assignment_version,
            "mode": p.mode,
            "shard_count": p.shards.len(),
            "instrument_count": p.shards.iter().map(|s| s.instruments.len()).sum::<usize>(),
        }),
        None => Value::Null,
    };
    Ok(json_etag(StatusCode::OK, v))
}

async fn put_node<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(name): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = require_node(&db, &name)?;
    require_if_match(&headers, &row_etag(existing.id, existing.updated_at))?;
    let mut req = node_from_body(&m, Some(&existing), Some(&name))?;
    req.name = name.clone();
    if let Some(sid) = req.server_id {
        if db.server_by_id(sid)?.is_none() {
            return Err(ApiError::validation(format!(
                "server_id={sid} 指向的服务器不存在"
            )));
        }
    }
    let before = node_json(&existing);
    let node = db.upsert_node(&req)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        "node_update",
        &name,
        Some(&before),
        Some(&node_json(&node)),
    )?;
    Ok(json_etag(StatusCode::OK, node_json(&node)))
}

async fn patch_enabled<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(name): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let enabled = opt_bool(&m, "enabled")
        .ok_or_else(|| ApiError::validation("缺少 enabled 字段"))?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = require_node(&db, &name)?;
    require_if_match(&headers, &row_etag(existing.id, existing.updated_at))?;
    let before = node_json(&existing);
    let req = NewNode {
        name: name.clone(),
        scope: existing.scope.clone(),
        scope_include: existing.scope_include.clone(),
        scope_exclude: existing.scope_exclude.clone(),
        runtime_params: existing.runtime_params.clone(),
        server_id: existing.server_id,
        note: existing.note.clone(),
        enabled,
    };
    let node = db.upsert_node(&req)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        if enabled {
            "node_enable"
        } else {
            "node_disable"
        },
        &name,
        Some(&before),
        Some(&node_json(&node)),
    )?;
    Ok(json_etag(StatusCode::OK, node_json(&node)))
}

async fn delete_node<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(name): AxPath<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = require_node(&db, &name)?;
    require_if_match(&headers, &row_etag(existing.id, existing.updated_at))?;
    let before = node_json(&existing);
    if !db.delete_node(&name)? {
        return Err(ApiError::not_found(format!("采集节点 `{name}` 不存在")));
    }
    audit_write(
        &db,
        &st.publisher(&ctx),
        "node_delete",
        &name,
        Some(&before),
        None,
    )?;
    Ok(no_content())
}

/// §4.5 范围预览：规则 + 例外 → 入选标的与 `unresolved`（不落库、不下发）。
async fn scope_preview<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let node = require_node(&db, &name)?;
    let out = scope::resolve(&node.scope, &node.scope_include, &node.scope_exclude, g.catalog());
    Ok(json_ok(json!({
        "node": name,
        "instruments": out.instruments,
        "unresolved": out.unresolved,
        "counts": {
            "instruments": out.instruments.len(),
            "unresolved": out.unresolved.len(),
        },
        "preview_only": true,
    })))
}

async fn node_toml<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let text = db.export_node_toml(&name)?;
    let filename = format!("{name}.toml");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(text))
        .map_err(|_| ApiError::internal())
}

async fn node_runtime<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    require_node(&db, &name)?;
    let key = g
        .registry()
        .get(&name)
        .ok_or_else(|| ApiError::not_found(format!("节点 `{name}` 尚无运行时实例")))?;
    let now = crate::now_ms();
    let conv = g.convergence(now);
    let timeout_ms = g.config().liveness.converge_timeout_ms;
    let v = instance_json(key, &conv, timeout_ms);
    Ok(json_ok(v))
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.9 配置域
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PublishBody {
    content: Value,
    base_version: Option<u64>,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RollbackBody {
    target_version: u64,
    base_version: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ValidateBody {
    content: Value,
}

/// §5.3 版本基线：缺 → 428；不符 → 409（带当前版本与 diff 摘要）。
fn check_baseline(
    store: &dyn ConfigStore,
    domain: ConfigDomain,
    base: Option<u64>,
) -> ApiResult<u64> {
    let base = base.ok_or_else(|| {
        ApiError::precondition_required("缺少 base_version（docs/13 §5.3）")
    })?;
    let current = store.current_version(domain)?.unwrap_or(0);
    if base != current {
        let prev = match current {
            0 => None,
            v => store.get(domain, v - 1)?,
        };
        let cur_doc = store.get(domain, current)?;
        let diff = match (&prev, &cur_doc) {
            (Some(p), Some(c)) => store::diff_summary(&p.content, &c.content, 32),
            _ => vec!["<initial>".to_string()],
        };
        return Err(ApiError::conflict(format!(
            "版本基线不符：当前 v{current}，提交 base_version={base}（他人已发布）"
        ))
        .detail(json!({
            "current_version": current,
            "base_version": base,
            "current_publisher": cur_doc.as_ref().map(|d| d.publisher.clone()),
            "current_ts": cur_doc.as_ref().map(|d| d.ts),
            "diff": diff,
        })));
    }
    Ok(current)
}

fn doc_json(d: &StoredDoc) -> Value {
    let hash = content_hash_of(&d.content);
    json!({
        "domain": d.domain.as_str(),
        "version": d.version,
        "content": d.content,
        "publisher": d.publisher,
        "ts": d.ts,
        "reason": d.reason,
        "prev_version": d.prev_version,
        "content_hash": hash,
        "etag": content_etag(&hash),
    })
}

async fn config_domains<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(page): Query<Page>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let mut items = Vec::new();
    for d in ConfigDomain::ALL {
        let cur = g.store().current(d)?;
        let audit = g.store().audit(d)?;
        let last = audit.last().cloned();
        items.push(json!({
            "domain": d.as_str(),
            "display": domain_display(d),
            "current_version": cur.as_ref().map(|c| c.version).unwrap_or(0),
            "updated_at": cur.as_ref().map(|c| c.ts).unwrap_or(0),
            "consumers": consumers_of(d),
            "push": push_class(d),
            "last_publisher": last.as_ref().map(|a| a.publisher.clone()),
            "last_published_at": last.as_ref().map(|a| a.ts).unwrap_or(0),
            "etag": match &cur {
                Some(c) => json!(content_etag(&content_hash_of(&c.content))),
                None => json!(format!("W/\"{}-0\"", d.as_str())),
            },
        }));
    }
    let (page_items, total, limit, offset) = page.slice(items);
    Ok(json_ok(paged(&page_items, total, limit, offset)))
}

/// 域的 ETag（未发布 = `W/"<domain>-0"`）。
fn domain_etag(g: &ControlPlane<impl EventCenter>, d: ConfigDomain) -> ApiResult<String> {
    match g.store().current(d)? {
        Some(doc) => Ok(content_etag(&content_hash_of(&doc.content))),
        None => Ok(format!("W/\"{}-0\"", d.as_str())),
    }
}

async fn config_current<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(domain): AxPath<String>,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let g = lock_plane(&st).await?;
    let doc = g.store().current(d)?.ok_or_else(|| {
        ApiError::not_found(format!("{d} 尚未发布（current_version = 0）"))
    })?;
    let body = doc_json(&doc);
    let etag = body["etag"].as_str().unwrap_or_default().to_string();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ETAG, etag)
        .body(Body::from(body.to_string()))
        .map_err(|_| ApiError::internal())
}

async fn config_put<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(domain): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let req: PublishBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("请求体不合法：{e}")))?;
    let mut g = lock_plane(&st).await?;
    let etag = domain_etag(&g, d)?;
    require_if_match(&headers, &etag)?;
    publish_now(&st, &ctx, &mut g, d, req).await
}

async fn config_publish<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(domain): AxPath<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let req: PublishBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("请求体不合法：{e}")))?;
    let mut g = lock_plane(&st).await?;
    publish_now(&st, &ctx, &mut g, d, req).await
}

/// 发布（§5.2 步骤 ③④）：基线校验 → `publish_domain` → `publish_all` → 快照。
async fn publish_now<E: EventCenter + 'static>(
    st: &AppState<E>,
    ctx: &AuthCtx,
    g: &mut MutexGuard<'_, ControlPlane<E>>,
    d: ConfigDomain,
    req: PublishBody,
) -> ApiResult<Response> {
    let publisher = st.publisher(ctx);
    check_baseline(&*g.store(), d, req.base_version)?;
    let reason = req
        .reason
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("publish");
    let (doc, out) = g.publish_domain(d, req.content, &publisher, reason)?;
    g.publish_all(&out)?;
    let notified = out.len();
    let diff = g
        .store()
        .audit(d)?
        .last()
        .map(|a| a.diff.clone())
        .unwrap_or_default();
    let recompute = matches!(
        d,
        ConfigDomain::SubscribePlan
            | ConfigDomain::InstrumentBlocklist
            | ConfigDomain::ProcessCapacity
    );
    let now = crate::now_ms();
    let timeout_ms = g.config().liveness.converge_timeout_ms;
    let conv = g.convergence(now);
    let mut dj = json!({
        "domain": doc.domain.as_str(),
        "version": doc.version,
        "publisher": doc.publisher,
        "ts": doc.ts,
        "reason": doc.reason,
        "prev_version": doc.prev_version,
        "content_hash": content_hash_of(&doc.content),
    });
    dj["content"] = doc.content.clone();
    Ok(json_created(json!({
        "doc": dj,
        "diff": diff,
        "notified": notified,
        "recompute": recompute,
        "convergence": convergence_json(&conv, timeout_ms),
    })))
}

async fn config_rollback<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(domain): AxPath<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let req: RollbackBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("请求体不合法：{e}")))?;
    let mut g = lock_plane(&st).await?;
    let publisher = st.publisher(&ctx);
    check_baseline(&*g.store(), d, req.base_version)?;
    if g.store().get(d, req.target_version)?.is_none() {
        return Err(ApiError::not_found(format!(
            "{d} v{} 不存在",
            req.target_version
        )));
    }
    let (doc, out) = g.rollback(d, req.target_version, &publisher)?;
    g.publish_all(&out)?;
    let notified = out.len();
    let diff = g
        .store()
        .audit(d)?
        .last()
        .map(|a| a.diff.clone())
        .unwrap_or_default();
    let recompute = matches!(
        d,
        ConfigDomain::SubscribePlan
            | ConfigDomain::InstrumentBlocklist
            | ConfigDomain::ProcessCapacity
    );
    let now = crate::now_ms();
    let timeout_ms = g.config().liveness.converge_timeout_ms;
    let conv = g.convergence(now);
    let mut dj = json!({
        "domain": doc.domain.as_str(),
        "version": doc.version,
        "publisher": doc.publisher,
        "ts": doc.ts,
        "reason": doc.reason,
        "prev_version": doc.prev_version,
        "content_hash": content_hash_of(&doc.content),
    });
    dj["content"] = doc.content.clone();
    Ok(json_created(json!({
        "doc": dj,
        "diff": diff,
        "notified": notified,
        "recompute": recompute,
        "convergence": convergence_json(&conv, timeout_ms),
    })))
}

async fn config_validate<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(domain): AxPath<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let req: ValidateBody = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("请求体不合法：{e}")))?;
    let g = lock_plane(&st).await?;
    validate::validate(d, &req.content, &*g.store())?;
    Ok(json_ok(json!({ "ok": true, "domain": d.as_str() })))
}

fn audit_item_json(a: &AuditEntry, content_hash: Option<String>) -> Value {
    json!({
        "domain": a.domain.as_str(),
        "version": a.version,
        "prev_version": a.prev_version,
        "publisher": a.publisher,
        "ts": a.ts,
        "reason": a.reason,
        "diff": a.diff,
        "action": if a.reason.starts_with("rollback to") { "rollback" } else { "publish" },
        "content_hash": content_hash,
    })
}

async fn config_audit<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(domain): AxPath<String>,
    Query(page): Query<Page>,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let g = lock_plane(&st).await?;
    let mut entries = g.store().audit(d)?;
    entries.reverse(); // 最新在前
    let (slice, total, limit, offset) = page.slice(entries);
    let mut items = Vec::new();
    for a in &slice {
        let hash = g
            .store()
            .get(d, a.version)?
            .map(|doc| content_hash_of(&doc.content));
        items.push(audit_item_json(a, hash));
    }
    Ok(json_ok(paged(&items, total, limit, offset)))
}

async fn config_versions<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(domain): AxPath<String>,
    Query(q): Query<Page>,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let g = lock_plane(&st).await?;
    let mut entries = g.store().audit(d)?;
    entries.reverse();
    let (slice, total, limit, offset) = q.slice(entries);
    let mut items = Vec::new();
    for a in &slice {
        let hash = g
            .store()
            .get(d, a.version)?
            .map(|doc| content_hash_of(&doc.content));
        items.push(audit_item_json(a, hash));
    }
    Ok(json_ok(paged(&items, total, limit, offset)))
}

async fn config_version<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath((domain, version)): AxPath<(String, String)>,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let v: u64 = version
        .parse()
        .map_err(|_| ApiError::validation(format!("版本号 `{version}` 不是整数")))?;
    let g = lock_plane(&st).await?;
    let doc = g
        .store()
        .get(d, v)?
        .ok_or_else(|| ApiError::not_found(format!("{d} v{v} 不存在")))?;
    Ok(json_ok(doc_json(&doc)))
}

async fn config_version_diff<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath((domain, version, _suffix)): AxPath<(String, String, String)>,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let v: u64 = version
        .parse()
        .map_err(|_| ApiError::validation(format!("版本号 `{version}` 不是整数")))?;
    let g = lock_plane(&st).await?;
    let cur = g
        .store()
        .get(d, v)?
        .ok_or_else(|| ApiError::not_found(format!("{d} v{v} 不存在")))?;
    let diff = if v <= 1 {
        vec!["<initial>".to_string()]
    } else {
        match g.store().get(d, v - 1)? {
            Some(prev) => store::diff_summary(&prev.content, &cur.content, 64),
            None => vec![format!("<missing v{}>", v - 1)],
        }
    };
    Ok(json_ok(json!({
        "domain": d.as_str(),
        "version": v,
        "prev_version": v.saturating_sub(1),
        "diff": diff,
    })))
}

async fn config_export<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(domain): AxPath<String>,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let g = lock_plane(&st).await?;
    let doc = g
        .store()
        .current(d)?
        .ok_or_else(|| ApiError::not_found(format!("{d} 尚未发布（current_version = 0）")))?;
    let hash = content_hash_of(&doc.content);
    let filename = format!("{d}-v{}.json", doc.version);
    let body = json!({
        "domain": d.as_str(),
        "version": doc.version,
        "publisher": doc.publisher,
        "ts": doc.ts,
        "reason": doc.reason,
        "content_hash": hash,
        "content": doc.content,
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(body.to_string()))
        .map_err(|_| ApiError::internal())
}

/// 域级收敛视图（§4.9「该域消费实例的 expected / actual / held_ms」）。
fn domain_convergence_json(
    g: &ControlPlane<impl EventCenter>,
    d: ConfigDomain,
    conv: &Convergence,
    now: u64,
) -> Value {
    let current = g
        .store()
        .current_version(d)
        .ok()
        .flatten()
        .unwrap_or(0) as i64;
    let mut items = Vec::new();
    for k in g.instances() {
        if !consumes_domain(k.node_kind, d) {
            continue;
        }
        let (expected, actual) = if k.node_kind == NodeKind::Collector {
            (
                k.last_assignment_version.unwrap_or(0) as i64,
                k.applied_version.unwrap_or(0) as i64,
            )
        } else {
            (current, k.domain_applied(d).unwrap_or(0))
        };
        let held = conv
            .not_converged
            .iter()
            .find(|n| n.instance_id == k.instance_id && n.domain.map(|x| x) == Some(d))
            .map(|n| n.held_ms)
            .unwrap_or(0);
        items.push(json!({
            "instance_id": k.instance_id,
            "expected": expected,
            "actual": actual,
            "held_ms": held,
            "converged": expected == actual,
        }));
    }
    items.sort_by(|a, b| {
        a["instance_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["instance_id"].as_str().unwrap_or(""))
    });
    json!({
        "domain": d.as_str(),
        "expected": current,
        "items": items,
        "converged": items.iter().filter(|i| i["converged"] == json!(true)).count(),
        "not_converged": items.iter().filter(|i| i["converged"] == json!(false)).count(),
        "ts": now,
    })
}

async fn config_convergence<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(domain): AxPath<String>,
) -> ApiResult<Response> {
    let d = parse_domain(&domain)?;
    let g = lock_plane(&st).await?;
    let now = crate::now_ms();
    let conv = g.convergence(now);
    Ok(json_ok(domain_convergence_json(&g, d, &conv, now)))
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.10 运行时状态
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct InstanceQuery {
    node_kind: Option<String>,
    status: Option<String>,
    converged: Option<String>,
    host: Option<String>,
    domain: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl InstanceQuery {
    fn page(&self) -> Page {
        Page { limit: self.limit, offset: self.offset }
    }
}

async fn runtime_instances<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<InstanceQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let now = crate::now_ms();
    let conv = g.convergence(now);
    let timeout_ms = g.config().liveness.converge_timeout_ms;
    let want_converged = parse_bool(q.converged.as_deref());
    let want_kind = q.node_kind.as_deref();

    let mut items: Vec<Value> = g
        .instances()
        .into_iter()
        .filter(|k| {
            if let Some(w) = want_kind {
                if kind_name(k.node_kind) != w {
                    return false;
                }
            }
            if let Some(s) = q.status.as_deref() {
                let actual = serde_json::to_value(k.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                if actual != s {
                    return false;
                }
            }
            if let Some(host) = q.host.as_deref() {
                if k.host.as_deref() != Some(host) {
                    return false;
                }
            }
            if let Some(w) = want_converged {
                let is = conv.not_converged.iter().all(|n| n.instance_id != k.instance_id);
                if is != w {
                    return false;
                }
            }
            if let Some(dom) = q.domain.as_deref() {
                match parse_domain(dom) {
                    Ok(d) => {
                        if !consumes_domain(k.node_kind, d) {
                            return false;
                        }
                    }
                    Err(_) => return false,
                }
            }
            true
        })
        .map(|k| instance_json(k, &conv, timeout_ms))
        .collect();
    items.sort_by(|a, b| {
        a["instance_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["instance_id"].as_str().unwrap_or(""))
    });
    let (page_items, total, limit, offset) = q.page().slice(items);
    Ok(json_ok(paged(&page_items, total, limit, offset)))
}

async fn runtime_instance<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let key = g
        .registry()
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("实例 `{id}` 不存在")))?;
    let now = crate::now_ms();
    let conv = g.convergence(now);
    let timeout_ms = g.config().liveness.converge_timeout_ms;
    let mut v = instance_json(key, &conv, timeout_ms);
    v["gate"] = match g.gate(&id) {
        Some(gate) => json!({ "applied": gate.applied() }),
        None => Value::Null,
    };
    Ok(json_ok(v))
}

async fn runtime_counts<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let now = crate::now_ms();
    let conv = g.convergence(now);
    let timeout_ms = g.config().liveness.converge_timeout_ms;
    Ok(json_ok(json!({
        "running": conv.running,
        "stopped": conv.stopped,
        "dead": conv.dead,
        "converged": conv.converged,
        "not_converged": conv.not_converged.len(),
        "not_converged_timeout": conv.timed_out(timeout_ms),
        "total": conv.running + conv.stopped + conv.dead,
    })))
}

#[derive(Debug, Deserialize, Default)]
struct ConvergenceQuery {
    domain: Option<String>,
    timeout_ms: Option<u64>,
}

async fn convergence<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<ConvergenceQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let now = crate::now_ms();
    let timeout_ms = q
        .timeout_ms
        .unwrap_or(g.config().liveness.converge_timeout_ms);
    let mut conv = g.convergence(now);
    let mut body = convergence_json(&conv, timeout_ms);
    if let Some(dom) = q.domain.as_deref() {
        let d = parse_domain(dom)?;
        conv.not_converged.retain(|n| n.domain == Some(d));
        body = json!({
            "domain": d.as_str(),
            "expected": g.store().current_version(d)?.unwrap_or(0),
            "not_converged": conv.not_converged.iter().map(not_converged_json).collect::<Vec<_>>(),
            "not_converged_timeout": conv.timed_out(timeout_ms),
            "ts": now,
        });
    }
    Ok(json_ok(body))
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.12 元数据同步
// ─────────────────────────────────────────────────────────────────────────────

async fn sync_status<E: EventCenter + 'static>(State(st): State<AppState<E>>) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let now = crate::now_ms();
    let cs = g.catalog_sync();
    let cat = cs.catalog();
    Ok(json_ok(json!({
        "interval_ms": cs.interval_ms,
        "last_round_ms": cs.last_round_ms(),
        "running": cs.is_running(),
        "due": cs.due(now),
        "next_due_at": cs.last_round_ms().saturating_add(cs.interval_ms),
        "quota": {
            "share": cs.quota.share,
            "base_requests_per_minute": cs.quota.base_requests_per_minute,
            "burst": cs.burst,
        },
        "catalog": {
            "updated_at": cat.updated_at,
            "symbols": cat.symbol_count(),
            "currencies": cat.currencies.values().map(|m| m.len()).sum::<usize>(),
        },
    })))
}

async fn sync_targets<E: EventCenter + 'static>(State(st): State<AppState<E>>) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let targets = g.metadata_targets()?;
    let fallback = g.catalog_sync().quota.share;
    let quotas: Vec<QuotaSpec> = match g.store().current(ConfigDomain::SubscribePlan)? {
        Some(doc) => serde_json::from_value::<SubscribePlanDoc>(doc.content)
            .map(|d| d.quota)
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let items: Vec<Value> = targets
        .iter()
        .map(|(venue, market)| {
            let share = quotas
                .iter()
                .find(|q| q.venue == venue.as_str())
                .map(|q| q.metadata)
                .unwrap_or(fallback);
            json!({ "venue": venue, "market": market, "share": share })
        })
        .collect();
    Ok(json_ok(json!({ "items": items, "count": items.len() })))
}

/// §4.12 立即触发（绕过 `due()` 门槛）：202 + 本轮 `SyncOutcome`。
/// 重推（`docs/13 §4.10 POST /runtime/instances/{instance_id}/repush`）：
/// 对某个实例**重发订阅计划**，`assignment_version` 递增（09 §5.4 / §4.4）。
///
/// 边界：
/// * 实例**从未被指派** → `404`（不是 200 空成功）；
/// * 版本门拒收（该实例已收到等价或更新的计划，10 §4.3）→ `409`；
/// * 成功返回新版本与实际投递条数；角色由中间件按方法判定（POST ⇒ write，§5.1）。
async fn repush_instance<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(instance_id): AxPath<String>,
) -> ApiResult<Response> {
    let mut g = lock_plane(&st).await?;
    if !g.has_plan(&instance_id) {
        return Err(ApiError::not_found(format!(
            "实例 `{instance_id}` 没有可重推的计划（尚未被指派）"
        )));
    }
    let out = g.repush(&instance_id)?;
    if out.is_empty() {
        return Err(ApiError::conflict(
            "重推被版本门拒收：该实例已收到等价或更新的计划（10 §4.3）",
        ));
    }
    let delivered = g.publish_all(&out)?;
    let version = g.plan_of(&instance_id).map(|p| p.assignment_version);
    Ok(json_ok(serde_json::json!({
        "instance_id": instance_id,
        "delivered": delivered,
        "assignment_version": version,
        "ts": crate::now_ms(),
    })))
}

async fn sync_run_now<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
) -> ApiResult<Response> {
    // 走阻塞线程池：本轮要打交易所公开端点（`reqwest::blocking` + 长超时），
    // 既不能在 async 上下文里 drop 那个壳 Runtime，也不能占满 worker —— 见 `run_blocking`。
    run_blocking(&st, |g| {
        if g.catalog_sync().is_running() {
            return Err(ApiError::conflict(
                "上一轮同步尚未结束（09 §2.3：一轮未完成不重入）",
            ));
        }
        let now = crate::now_ms();
        let t0 = std::time::Instant::now();
        let outcome = g.run_metadata_sync_forced(now)?;
        // 关键步骤日志：给「点了按钮没反应」一个可对账的出口（HTTP 侧结果）
        eprintln!(
            "[sync] POST /sync/runs → 202 ran={} changed={} 跳过 {}，{} ms",
            outcome.ran,
            outcome.changed,
            outcome.skipped.len(),
            t0.elapsed().as_millis()
        );
        Ok(json_accepted(json!({
            "ran": outcome.ran,
            "changed": outcome.changed,
            "recompute_ran": outcome.recompute_ran,
            "diff": outcome.diff,
            "skipped": outcome.skipped,
            "fetched_symbols": outcome.fetched_symbols,
            "fetched_currencies": outcome.fetched_currencies,
            "summary": outcome.diff.summary(),
            "sync_run_id": outcome.sync_run_id,
            "persist_error": outcome.persist_error,
            "ts": now,
        })))
    })
    .await
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.6 交易所与市场（`exchange` / `exchange_market`）
// ─────────────────────────────────────────────────────────────────────────────

/// 分页信封 `{items,total,limit,offset}`（§4.1），`items` 经 `f` 组装成行 JSON。
fn paged_json<T, F>(p: &PageOut<T>, f: F) -> Value
where
    F: Fn(&T) -> Value,
{
    let items: Vec<Value> = p.items.iter().map(|x| f(x)).collect();
    json!({ "items": items, "total": p.total, "limit": p.limit, "offset": p.offset })
}

/// 行级 ETag（§5.3）：由**行内容**的规范化哈希给出，天然随行变化。
///
/// 与 `row_etag(id, updated_at)` 并存：后者服务于有自增 id + `updated_at` 的
/// 服务器 / 节点；目录表的主键是自然键，故按内容取哈希。
fn row_etag_of(v: &Value) -> String {
    format!("W/\"{}\"", content_hash_of(v))
}

fn with_etag(mut v: Value) -> Value {
    let e = row_etag_of(&v);
    v["etag"] = json!(e);
    v
}

fn exchange_json(e: &ExchangeRecord) -> Value {
    with_etag(serde_json::to_value(e).unwrap_or(Value::Null))
}

fn exchange_etag(e: &ExchangeRecord) -> String {
    row_etag_of(&serde_json::to_value(e).unwrap_or(Value::Null))
}

fn market_json(m: &ExchangeMarketRecord) -> Value {
    with_etag(serde_json::to_value(m).unwrap_or(Value::Null))
}

fn market_etag(m: &ExchangeMarketRecord) -> String {
    row_etag_of(&serde_json::to_value(m).unwrap_or(Value::Null))
}

#[derive(Debug, Deserialize, Default)]
struct ExchangeQuery {
    enabled: Option<String>,
    q: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl ExchangeQuery {
    fn filter(&self) -> ExchangeFilter {
        ExchangeFilter {
            enabled: parse_bool(self.enabled.as_deref()),
            q: self.q.clone(),
            sort: self.sort.clone(),
            order: self.order.clone(),
            limit: self.limit,
            offset: self.offset,
        }
    }
}

/// DELETE 的二次确认参数（§4.6：级联删除需显式确认）。
#[derive(Debug, Deserialize, Default)]
struct ConfirmQuery {
    confirm: Option<String>,
}

/// 请求体 → [`NewExchange`]（**出现即生效，缺省沿用既有值**）。
fn exchange_from_body(
    m: &serde_json::Map<String, Value>,
    base: Option<&ExchangeRecord>,
    fallback: Option<&str>,
) -> ApiResult<NewExchange> {
    let venue = opt_str(m, "venue")
        .map(str::to_string)
        .or_else(|| fallback.map(str::to_string))
        .or_else(|| base.map(|b| b.venue.clone()))
        .unwrap_or_default();

    let mut req = match base {
        Some(b) => NewExchange {
            venue: b.venue.clone(),
            name: b.name.clone(),
            dig3_exchange_id: b.dig3_exchange_id.clone(),
            enabled: b.enabled,
            asset_class: b.asset_class.clone(),
            extra: b.extra.clone(),
        },
        None => NewExchange {
            venue: venue.clone(),
            name: venue.clone(),
            dig3_exchange_id: venue.clone(),
            ..NewExchange::default()
        },
    };
    if let Some(h) = fallback {
        req.venue = h.to_string(); // 路径是寻址键
    }
    if let Some(v) = opt_str(m, "name") {
        req.name = v.to_string();
    }
    if let Some(v) = opt_str(m, "dig3_exchange_id") {
        req.dig3_exchange_id = v.to_string();
    }
    if m.contains_key("enabled") {
        req.enabled = opt_bool(m, "enabled").unwrap_or(true);
    }
    if let Some(v) = opt_str(m, "asset_class") {
        req.asset_class = crate::catalog_repo::validate_asset_class(v).map_err(|e| match e {
            ControlError::Validation(msg) => ApiError::validation(msg),
            other => ApiError::from(other),
        })?;
    }
    if let Some(v) = m.get("extra") {
        req.extra = if v.is_null() { json!({}) } else { v.clone() };
    }
    if req.venue.trim().is_empty() {
        return Err(ApiError::validation("venue 不能为空"));
    }
    Ok(req)
}

/// 请求体 → [`NewMarket`]。`dig3_account_type` **新建时必填**（12 §3.1 无默认值）。
fn market_from_body(
    m: &serde_json::Map<String, Value>,
    base: Option<&ExchangeMarketRecord>,
    venue: &str,
    fallback_market: Option<&str>,
) -> ApiResult<NewMarket> {
    let market = fallback_market
        .map(str::to_string)
        .or_else(|| opt_str(m, "market").map(str::to_string))
        .or_else(|| base.map(|b| b.market.clone()))
        .unwrap_or_default();

    let mut req = match base {
        Some(b) => NewMarket {
            venue: venue.to_string(),
            market: b.market.clone(),
            dig3_account_type: b.dig3_account_type.clone(),
            enabled: b.enabled,
            quota_collect: b.quota_collect,
            quota_backfill: b.quota_backfill,
            quota_metadata: b.quota_metadata,
            egress_ip: b.egress_ip.clone(),
        },
        None => NewMarket {
            venue: venue.to_string(),
            market: market.clone(),
            ..NewMarket::default()
        },
    };
    req.venue = venue.to_string();
    if let Some(mm) = fallback_market {
        req.market = mm.to_string();
    }
    match opt_str(m, "dig3_account_type") {
        Some(v) => req.dig3_account_type = v.to_string(),
        None if base.is_none() => {
            return Err(ApiError::validation(
                "dig3_account_type 必填（docs/12 §3.1：不设默认值，02 §6 映射如 swap → FuturesCross）",
            ))
        }
        None => {}
    }
    if m.contains_key("enabled") {
        req.enabled = opt_bool(m, "enabled").unwrap_or(true);
    }
    if let Some(v) = m.get("quota_collect").and_then(Value::as_f64) {
        req.quota_collect = v;
    }
    if let Some(v) = m.get("quota_backfill").and_then(Value::as_f64) {
        req.quota_backfill = v;
    }
    if let Some(v) = m.get("quota_metadata").and_then(Value::as_f64) {
        req.quota_metadata = v;
    }
    if let Some(v) = opt_str(m, "egress_ip") {
        req.egress_ip = Some(v.to_string());
    } else if m.contains_key("egress_ip") {
        req.egress_ip = None;
    }
    if req.market.trim().is_empty() {
        return Err(ApiError::validation("market 不能为空"));
    }
    Ok(req)
}

async fn list_exchanges<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<ExchangeQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let page = q.filter().apply(db.list_exchanges_all()?);
    Ok(json_ok(paged_json(&page, exchange_json)))
}

async fn create_exchange<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let req = exchange_from_body(&m, None, None)?;
    if db.exchange(&req.venue)?.is_some() {
        return Err(ApiError::conflict(format!(
            "交易所 `{}` 已存在（改用 PUT）",
            req.venue
        )));
    }
    let rec = db.upsert_exchange(&req)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        "exchange_create",
        &rec.venue,
        None,
        Some(&exchange_json(&rec)),
    )?;
    Ok(json_etag(StatusCode::CREATED, exchange_json(&rec)))
}

async fn get_exchange<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(venue): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let rec = db.require_exchange(&venue)?;
    let mut v = exchange_json(&rec);
    let markets: Vec<Value> = db.list_markets(&venue)?.iter().map(market_json).collect();
    v["markets"] = json!(markets);
    Ok(json_etag(StatusCode::OK, v))
}

async fn put_exchange<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(venue): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = db.require_exchange(&venue)?;
    require_if_match(&headers, &exchange_etag(&existing))?;
    let req = exchange_from_body(&m, Some(&existing), Some(&venue))?;
    let before = exchange_json(&existing);
    let rec = db.upsert_exchange(&req)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        "exchange_update",
        &venue,
        Some(&before),
        Some(&exchange_json(&rec)),
    )?;
    Ok(json_etag(StatusCode::OK, exchange_json(&rec)))
}

async fn patch_exchange_enabled<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(venue): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let enabled = opt_bool(&m, "enabled")
        .ok_or_else(|| ApiError::validation("缺少 enabled 字段"))?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = db.require_exchange(&venue)?;
    require_if_match(&headers, &exchange_etag(&existing))?;
    let before = exchange_json(&existing);
    db.set_exchange_enabled(&venue, enabled)?;
    let rec = db.require_exchange(&venue)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        if enabled { "exchange_enable" } else { "exchange_disable" },
        &venue,
        Some(&before),
        Some(&exchange_json(&rec)),
    )?;
    Ok(json_etag(StatusCode::OK, exchange_json(&rec)))
}

async fn delete_exchange<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(venue): AxPath<String>,
    headers: HeaderMap,
    Query(q): Query<ConfirmQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = db.require_exchange(&venue)?;
    require_if_match(&headers, &exchange_etag(&existing))?;
    if existing.market_count > 0 {
        let ok = q.confirm.as_deref() == Some(venue.as_str());
        if !ok {
            return Err(ApiError::validation(format!(
                "该交易所下仍有 {} 个市场（及其交易对）会被级联删除，需带 ?confirm={venue} 二次确认",
                existing.market_count
            ))
            .detail(json!({ "markets": existing.market_count, "confirm": venue })));
        }
    }
    let before = exchange_json(&existing);
    db.delete_exchange(&venue)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        "exchange_delete",
        &venue,
        Some(&before),
        None,
    )?;
    Ok(no_content())
}

async fn list_markets<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(venue): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    db.require_exchange(&venue)?;
    let rows: Vec<Value> = db.list_markets(&venue)?.iter().map(market_json).collect();
    // §4.6 本行无 `limit`/`offset`，前端按裸数组取（见交付报告的偏差说明）
    Ok(json_ok(json!(rows)))
}

async fn create_market<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(venue): AxPath<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    db.require_exchange(&venue)?;
    let req = market_from_body(&m, None, &venue, None)?;
    if db.market(&venue, &req.market)?.is_some() {
        return Err(ApiError::conflict(format!(
            "市场 `{}/{}` 已存在（改用 PUT）",
            venue, req.market
        )));
    }
    let rec = db.upsert_market(&req)?;
    let target = format!("{}/{}", rec.venue, rec.market);
    audit_write(
        &db,
        &st.publisher(&ctx),
        "market_create",
        &target,
        None,
        Some(&market_json(&rec)),
    )?;
    Ok(json_etag(StatusCode::CREATED, market_json(&rec)))
}

async fn put_market<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath((venue, market)): AxPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = db.require_market(&venue, &market)?;
    require_if_match(&headers, &market_etag(&existing))?;
    let req = market_from_body(&m, Some(&existing), &venue, Some(&market))?;
    let before = market_json(&existing);
    let rec = db.upsert_market(&req)?;
    let target = format!("{}/{}", rec.venue, rec.market);
    audit_write(
        &db,
        &st.publisher(&ctx),
        "market_update",
        &target,
        Some(&before),
        Some(&market_json(&rec)),
    )?;
    Ok(json_etag(StatusCode::OK, market_json(&rec)))
}

async fn patch_market_enabled<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath((venue, market)): AxPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let enabled = opt_bool(&m, "enabled")
        .ok_or_else(|| ApiError::validation("缺少 enabled 字段"))?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = db.require_market(&venue, &market)?;
    require_if_match(&headers, &market_etag(&existing))?;
    let before = market_json(&existing);
    db.set_market_enabled(&venue, &market, enabled)?;
    let rec = db.require_market(&venue, &market)?;
    let target = format!("{venue}/{market}");
    audit_write(
        &db,
        &st.publisher(&ctx),
        if enabled { "market_enable" } else { "market_disable" },
        &target,
        Some(&before),
        Some(&market_json(&rec)),
    )?;
    Ok(json_etag(StatusCode::OK, market_json(&rec)))
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.7 交易对 / 变更历史（`instrument` / `instrument_change`）
// ─────────────────────────────────────────────────────────────────────────────

/// CSV 单元格转义（§5.4：交易对 / 币 / 审计的导出）。
fn csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// CSV 下载响应（`Content-Disposition`）。
fn csv_response(filename: &str, cols: &[&str], rows: Vec<Vec<String>>) -> ApiResult<Response> {
    let mut out = String::with_capacity(1024);
    out.push_str(&cols.join(","));
    out.push('\n');
    for r in rows {
        out.push_str(&r.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(out))
        .map_err(|_| ApiError::internal())
}

fn csv_opt(v: Option<impl std::fmt::Display>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn csv_u64(v: Option<u64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn instrument_json(i: &InstrumentRecord) -> Value {
    with_etag(serde_json::to_value(i).unwrap_or(Value::Null))
}

fn change_json(c: &ChangeRecord) -> Value {
    // §7.2 R7：DDL 列名 `before_v` / `after_v`；接口**同时**给出 `before` / `after`
    // 别名（`ChangeRecord` 两者都带），前端两种取法都能工作。
    with_etag(serde_json::to_value(c).unwrap_or(Value::Null))
}

#[derive(Debug, Deserialize, Default)]
struct InstrumentQuery {
    venue: Option<String>,
    market: Option<String>,
    status: Option<String>,
    native_code: Option<String>,
    base: Option<String>,
    quote: Option<String>,
    instrument_type: Option<String>,
    synced_after: Option<u64>,
    synced_before: Option<u64>,
    sort: Option<String>,
    order: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl InstrumentQuery {
    fn filter(&self) -> InstrumentFilter {
        InstrumentFilter {
            venue: self.venue.clone(),
            market: self.market.clone(),
            status: self.status.clone(),
            native_code: self.native_code.clone(),
            base: self.base.clone(),
            quote: self.quote.clone(),
            instrument_type: self.instrument_type.clone(),
            synced_after: self.synced_after,
            synced_before: self.synced_before,
            sort: self.sort.clone(),
            order: self.order.clone(),
            limit: self.limit,
            offset: self.offset,
        }
    }
}

async fn list_instruments<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<InstrumentQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let page = q.filter().apply(db.list_instruments_all()?);
    Ok(json_ok(paged_json(&page, instrument_json)))
}

async fn get_instrument<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath((venue, market, native_code)): AxPath<(String, String, String)>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let rec = db
        .instrument(&venue, &market, &native_code)?
        .ok_or_else(|| {
            ApiError::not_found(format!("交易对 `{venue}:{market}:{native_code}` 不存在"))
        })?;
    Ok(json_etag(StatusCode::OK, instrument_json(&rec)))
}

/// CSV 流式导出（筛选条件随导出，§5.4）。
async fn export_instruments<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<InstrumentQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let rows = q.filter().select(db.list_instruments_all()?);
    let cols: [&str; 17] = [
        "venue",
        "market",
        "native_code",
        "base",
        "quote",
        "status",
        "instrument_type",
        "price_precision",
        "qty_precision",
        "tick_size",
        "step_size",
        "min_notional",
        "listed_at",
        "delisted_at",
        "first_seen_at",
        "last_synced_at",
        "row_hash",
    ];
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|i| {
            vec![
                i.venue.clone(),
                i.market.clone(),
                i.native_code.clone(),
                i.base.clone().unwrap_or_default(),
                i.quote.clone().unwrap_or_default(),
                i.status.clone(),
                i.instrument_type.clone().unwrap_or_default(),
                csv_opt(i.price_precision),
                csv_opt(i.qty_precision),
                i.tick_size.clone().unwrap_or_default(),
                i.step_size.clone().unwrap_or_default(),
                i.min_notional.clone().unwrap_or_default(),
                csv_u64(i.listed_at),
                csv_u64(i.delisted_at),
                i.first_seen_at.to_string(),
                i.last_synced_at.to_string(),
                i.row_hash.clone(),
            ]
        })
        .collect();
    csv_response("instruments.csv", &cols, body)
}

#[derive(Debug, Deserialize, Default)]
struct ChangeQuery {
    venue: Option<String>,
    market: Option<String>,
    native_code: Option<String>,
    change_kind: Option<String>,
    since: Option<u64>,
    until: Option<u64>,
    sync_run_id: Option<i64>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl ChangeQuery {
    fn filter(&self) -> ChangeFilter {
        ChangeFilter {
            venue: self.venue.clone(),
            market: self.market.clone(),
            native_code: self.native_code.clone(),
            change_kind: self.change_kind.clone(),
            since: self.since,
            until: self.until,
            sync_run_id: self.sync_run_id,
            limit: self.limit,
            offset: self.offset,
        }
    }
}

async fn list_instrument_changes<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<ChangeQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let page = q.filter().apply(db.list_changes_all()?);
    Ok(json_ok(paged_json(&page, change_json)))
}

async fn get_instrument_change<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(id): AxPath<i64>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let rec = db
        .change(id)?
        .ok_or_else(|| ApiError::not_found(format!("变更记录 #{id} 不存在")))?;
    Ok(json_etag(StatusCode::OK, change_json(&rec)))
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.8 币信息（`asset` / `venue_asset`）
// ─────────────────────────────────────────────────────────────────────────────

/// 币行 JSON（**不含** `etag`）——`asset_json` 与 `asset_etag` 必须同源，
/// 否则 `PATCH /assets/{code}` 的 `If-Match` 永远比不中。
fn asset_base_json(a: &AssetRecord) -> Value {
    let mut v = serde_json::to_value(a).unwrap_or(Value::Null);
    // §3.3 P5「各所覆盖数」：两种别名都给出（前端类型两者皆声明为可选）
    v["venues"] = json!(a.venue_count);
    v
}

fn asset_json(a: &AssetRecord) -> Value {
    with_etag(asset_base_json(a))
}

/// 币行 ETag（§5.3 行级并发，`PATCH /assets/{code}` 用）。
fn asset_etag(a: &AssetRecord) -> String {
    row_etag_of(&asset_base_json(a))
}

fn venue_asset_json(r: &VenueAssetRecord) -> Value {
    with_etag(serde_json::to_value(r).unwrap_or(Value::Null))
}

#[derive(Debug, Deserialize, Default)]
struct AssetQuery {
    kind: Option<String>,
    q: Option<String>,
    venue: Option<String>,
    deposit_enabled: Option<String>,
    withdraw_enabled: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl AssetQuery {
    fn filter(&self) -> AssetFilter {
        AssetFilter {
            kind: self.kind.clone(),
            q: self.q.clone(),
            venue: self.venue.clone(),
            deposit_enabled: parse_bool(self.deposit_enabled.as_deref()),
            withdraw_enabled: parse_bool(self.withdraw_enabled.as_deref()),
            sort: self.sort.clone(),
            order: self.order.clone(),
            limit: self.limit,
            offset: self.offset,
        }
    }
}

async fn list_assets<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<AssetQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let venue_assets = db.list_venue_assets()?;
    let page = q.filter().apply(db.list_assets_all()?, &venue_assets);
    Ok(json_ok(paged_json(&page, asset_json)))
}

async fn get_asset<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(code): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let rec = db
        .asset(&code)?
        .ok_or_else(|| ApiError::not_found(format!("币 `{code}` 不存在")))?;
    let mut v = asset_json(&rec);
    let vas: Vec<Value> = db
        .venue_assets_of(&code)?
        .iter()
        .map(venue_asset_json)
        .collect();
    v["venue_assets"] = json!(vas);
    Ok(json_etag(StatusCode::OK, v))
}

/// 请求体 → [`AssetPatch`]。
///
/// 用 `Map` 而不是 serde 结构体，因为 `{"external_id": null}`（**清空**）
/// 必须与「字段没提交」（不改）区分开——serde 的 `Option<Option<T>>` 会把两者都读成 `None`。
fn asset_patch_from_body(m: &serde_json::Map<String, Value>) -> ApiResult<AssetPatch> {
    let mut patch = AssetPatch::default();
    if let Some(v) = m.get("kind") {
        match v.as_str() {
            Some(s) => patch.kind = Some(s.to_string()),
            None => return Err(ApiError::validation("kind 必须是字符串")),
        }
    }
    if let Some(v) = m.get("external_id") {
        patch.external_id = Some(match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            _ => return Err(ApiError::validation(
                "external_id 必须是字符串（CG coin id）或 null（清空）",
            )),
        });
    }
    if patch.kind.is_none() && patch.external_id.is_none() {
        return Err(ApiError::validation(
            "请求体至少要包含 kind 或 external_id（docs/14 §9）",
        ));
    }
    Ok(patch)
}

/// `PATCH /assets/{code}`（`docs/14 §9`）—— 币信息页的**唯一写入口**。
///
/// * 只允许改 `kind` 与 `external_id`（`docs/14 §4` 判断 3 的人工兜底）；
/// * 手填 `external_id` 后服务端置 `match_status = 'matched'`，传 `null` 清空并回落 `not_found`；
/// * 二者**不参与** `instrument.row_hash`，因此不会被下一轮同步冲掉（`docs/14 §1` 单写入方）；
/// * 行级 `If-Match`：缺 → 428，不符 → 412（§5.3）。
async fn patch_asset<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    axum::Extension(ctx): axum::Extension<AuthCtx>,
    AxPath(code): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let m = body_object(&body)?;
    let patch = asset_patch_from_body(&m)?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let existing = db.require_asset(&code)?;
    require_if_match(&headers, &asset_etag(&existing))?;
    let before = asset_json(&existing);
    let rec = db.patch_asset(&code, &patch)?;
    audit_write(
        &db,
        &st.publisher(&ctx),
        "asset_update",
        &code,
        Some(&before),
        Some(&asset_json(&rec)),
    )?;
    Ok(json_etag(StatusCode::OK, asset_json(&rec)))
}

/// `GET /assets/{code}/contracts`（`docs/14 §9`）：链 + 合约地址列表（子表）。
///
/// **裸数组**（`docs/13 §4.1`：未列 `limit=&offset=` 的端点返回裸数组），
/// 每项**只给** `chain` / `address` / `standard` / `is_primary` / `source` / `updated_at`
/// ——字段集合按 `docs/14 §9` 的响应形状定死。
/// 一币 1~10 条，不分页。**资产存在但没合约信息 → `[]`**（合法状态，不是 404）；
/// 资产本身不存在 → 404。
fn contract_json(r: &AssetContractRecord) -> Value {
    json!({
        "chain": r.chain,
        "address": r.address,
        "standard": r.standard,
        "is_primary": r.is_primary,   // 0/1 整数，按 DDL 的 INTEGER 原样返回
        "source": r.source,
        "updated_at": r.updated_at,
    })
}

async fn get_asset_contracts<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(code): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    db.require_asset(&code)?;
    let rows: Vec<AssetContractRecord> = db.asset_contracts(&code)?;
    let items: Vec<Value> = rows.iter().map(contract_json).collect();
    Ok(json_ok(Value::Array(items)))
}

async fn list_venue_assets<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(venue): AxPath<String>,
    Query(q): Query<VenueAssetQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    db.require_exchange(&venue)?;
    let f = VenueAssetFilter {
        q: q.q.clone(),
        limit: q.limit,
        offset: q.offset,
    };
    let page = f.apply(db.list_venue_assets_all(&venue)?);
    Ok(json_ok(paged_json(&page, venue_asset_json)))
}

#[derive(Debug, Deserialize, Default)]
struct VenueAssetQuery {
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn export_assets<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<AssetQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let venue_assets = db.list_venue_assets()?;
    let rows = q.filter().select(db.list_assets_all()?, &venue_assets);
    let cols: [&str; 7] = [
        "id", "code", "name", "kind", "venue_count", "created_at", "updated_at",
    ];
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|a| {
            vec![
                a.id.to_string(),
                a.code.clone(),
                a.name.clone().unwrap_or_default(),
                a.kind.clone(),
                a.venue_count.to_string(),
                a.created_at.to_string(),
                a.updated_at.to_string(),
            ]
        })
        .collect();
    csv_response("assets.csv", &cols, body)
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.12 元数据同步轮次（`sync_run`）
// ─────────────────────────────────────────────────────────────────────────────

fn sync_run_json(r: &SyncRunRecord) -> Value {
    with_etag(serde_json::to_value(r).unwrap_or(Value::Null))
}

/// 路径段 → 整数 id（失败走 §4.2 `validation`，不落到 axum 的裸 400）。
fn parse_id(raw: &str, what: &str) -> ApiResult<i64> {
    raw.parse::<i64>()
        .map_err(|_| ApiError::validation(format!("{what} `{raw}` 不是整数")))
}

async fn list_sync_runs<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<Page>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let f = SyncRunFilter {
        limit: q.limit,
        offset: q.offset,
    };
    let page = f.apply(db.list_sync_runs_all()?);
    Ok(json_ok(paged_json(&page, sync_run_json)))
}

async fn get_sync_run<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Response> {
    let id = parse_id(&id, "轮次 id")?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let rec = db
        .sync_run(id)?
        .ok_or_else(|| ApiError::not_found(format!("同步轮次 #{id} 不存在")))?;
    Ok(json_etag(StatusCode::OK, sync_run_json(&rec)))
}

// ─────────────────────────────────────────────────────────────────────────────
// docs/14 §9 外部（币）信息同步（`offchain_sync_run`，与 `sync_run` 分离）
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /sync/offchain/runs`：立即触发外部同步（**202**，复用 forced 模式）。
///
/// 与 `POST /sync/runs` 同口径：409 = 上一轮未收尾（一轮未完成不重入）。
/// 体为 `{ran, offchain_run_id, outcome, error, ts}`，其中 `outcome` 与
/// `GET /sync/offchain/status` 的 `last_run.outcome` **同一份对象**（`docs/14 §9`）。
async fn offchain_sync_run_now<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
) -> ApiResult<Response> {
    // 同 `POST /sync/runs`：CoinGecko 也是 `reqwest::blocking`（同一处壳 Runtime 缺陷），
    // 且 CG 免费档单轮可达数十秒 —— 一并挪进阻塞线程池。
    run_blocking(&st, |g| {
        if g.offchain_sync().is_running() {
            return Err(ApiError::conflict(
                "上一轮外部同步尚未结束（docs/14 §3：一轮未完成不重入）",
            ));
        }
        let now = crate::now_ms();
        let out = g.run_offchain_sync_forced(now);
        eprintln!(
            "[sync] POST /sync/offchain/runs → 202 ran={} error={}",
            out.ran,
            out.error.as_deref().unwrap_or("无")
        );
        Ok(json_accepted(json!({
            "ran": out.ran,
            "offchain_run_id": out.offchain_run_id,
            "outcome": out.outcome_value(now),
            "error": out.error,
            "ts": now,
        })))
    })
    .await
}

/// `GET /sync/offchain/status`：归一化三字段（`docs/14 §9`）。
///
/// * `running`：布尔（上一轮是否尚未收尾）；
/// * `last_run`：最近一条 `offchain_sync_run`（`id` / `started_at` / `finished_at` /
///   `outcome` / `error`），**无记录时为 `null`**；
/// * `detail_synced_at`：`asset.detail_synced_at` 的最大值，没有则 `null`。
async fn offchain_sync_status<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let last: Option<OffchainSyncRunRecord> = db.last_offchain_sync_run()?;
    let detail_synced_at = db.max_asset_detail_synced_at()?;
    let oc = g.offchain_sync();
    let last_run = last.map(|r| {
        json!({
            "id": r.id,
            "started_at": r.started_at,
            "finished_at": r.finished_at,
            "outcome": r.outcome,
            "error": r.error,
        })
    });
    Ok(json_ok(json!({
        "running": oc.is_running(),
        "last_run": last_run,
        "detail_synced_at": detail_synced_at,
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.13 审计日志（跨域全表）
// ─────────────────────────────────────────────────────────────────────────────

fn audit_json(a: &AuditRow) -> Value {
    let mut v = serde_json::to_value(a).unwrap_or(Value::Null);
    // `detail` 里若内嵌 `AuditEntry`（publish / rollback），把常用字段提到顶层
    if let Value::Object(m) = &a.detail {
        for k in ["domain", "version", "prev_version", "publisher", "reason", "diff"] {
            if let Some(x) = m.get(k) {
                v[k] = x.clone();
            }
        }
    }
    with_etag(v)
}

#[derive(Debug, Deserialize, Default)]
struct AuditListQuery {
    action: Option<String>,
    target: Option<String>,
    actor: Option<String>,
    since: Option<u64>,
    until: Option<u64>,
    q: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl AuditListQuery {
    fn filter(&self) -> AuditFilter {
        AuditFilter {
            action: self.action.clone(),
            target: self.target.clone(),
            actor: self.actor.clone(),
            since: self.since,
            until: self.until,
            q: self.q.clone(),
            sort: self.sort.clone(),
            order: self.order.clone(),
            limit: self.limit,
            offset: self.offset,
        }
    }
}

async fn list_audit<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<AuditListQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let page = q.filter().apply(db.list_audit_all()?);
    Ok(json_ok(paged_json(&page, audit_json)))
}

async fn get_audit<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(id): AxPath<String>,
) -> ApiResult<Response> {
    let id = parse_id(&id, "审计 id")?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let rec = db
        .audit_row(id)?
        .ok_or_else(|| ApiError::not_found(format!("审计记录 #{id} 不存在")))?;
    Ok(json_etag(StatusCode::OK, audit_json(&rec)))
}

async fn export_audit<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<AuditListQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let rows = q.filter().select(db.list_audit_all()?);
    let cols: [&str; 7] = ["id", "actor", "action", "target", "before_hash", "after_hash", "ts"];
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|a| {
            vec![
                a.id.to_string(),
                a.actor.clone(),
                a.action.clone(),
                a.target.clone(),
                a.before_hash.clone().unwrap_or_default(),
                a.after_hash.clone().unwrap_or_default(),
                a.ts.to_string(),
            ]
        })
        .collect();
    csv_response("audit.csv", &cols, body)
}

// ─────────────────────────────────────────────────────────────────────────────
// §4.11 指派结果（`node_assignment` 视图 + 试算）
// ─────────────────────────────────────────────────────────────────────────────

/// 每实例的承载机器（§3.3 P10「按机器」视图：运行时 host，未上报取期望机器）。
#[derive(Debug, Clone, Default)]
struct InstanceMeta {
    /// `node_runtime.host`，缺省回落到期望机器。
    host: Option<String>,
    /// 期望机器（`server_id`）。
    server_id: Option<i64>,
    /// 期望机器的 host。
    server_host: Option<String>,
}

fn instance_meta<E: EventCenter + 'static>(
    g: &ControlPlane<E>,
    db: &crate::sqlite::SqliteStore,
) -> ApiResult<BTreeMap<String, InstanceMeta>> {
    let mut map: BTreeMap<String, InstanceMeta> = BTreeMap::new();
    for k in g.instances() {
        map.insert(
            k.instance_id.clone(),
            InstanceMeta {
                host: k.host.clone(),
                ..InstanceMeta::default()
            },
        );
    }
    for n in db.list_nodes()? {
        let server_host = n
            .server_id
            .and_then(|id| db.server_by_id(id).ok().flatten())
            .map(|s| s.host);
        let e = map.entry(n.name.clone()).or_default();
        e.server_id = n.server_id;
        if e.host.is_none() {
            e.host = server_host.clone();
        }
        e.server_host = server_host;
    }
    Ok(map)
}

/// `node_assignment.plan` → `SubscribePlan`。
fn plan_of_row(row: &PlanRow) -> ApiResult<SubscribePlan> {
    serde_json::from_value::<SubscribePlan>(row.plan.clone()).map_err(|_| ApiError::internal())
}

/// `quota_splits`（存的是 `venue → QuotaShare` 映射）→ 前端要的 `[{venue, …}]`。
fn quota_split_rows(v: &Value) -> Value {
    match v {
        Value::Object(map) => Value::Array(
            map.iter()
                .map(|(venue, val)| {
                    let mut o = match val {
                        Value::Object(m) => m.clone(),
                        _ => serde_json::Map::new(),
                    };
                    o.insert("venue".to_string(), json!(venue));
                    Value::Object(o)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// 分片行（§3.3 P10「按实例 / 按机器 / 按交易所」的行单位）。
fn shard_rows(row: &PlanRow, meta: Option<&InstanceMeta>) -> ApiResult<Vec<Value>> {
    let plan = plan_of_row(row)?;
    let host = meta
        .and_then(|m| m.host.clone())
        .or_else(|| meta.and_then(|m| m.server_host.clone()));
    let server_id = meta.and_then(|m| m.server_id);
    let mut out = Vec::with_capacity(plan.shards.len());
    for s in &plan.shards {
        out.push(json!({
            "instance_id": row.instance.as_str(),
            "assignment_version": row.assignment_version,
            "mode": row.mode.as_str(),
            "venue": s.venue.as_str(),
            "market": s.market.as_str(),
            "shard_idx": s.shard_idx,
            "instruments": s.instruments.len(),
            "instrument_count": s.instruments.len(),
            "host": host.as_deref(),
            "server_id": server_id,
            "generated_at": row.generated_at,
        }));
    }
    Ok(out)
}

fn plan_counts(row: &PlanRow) -> ApiResult<(usize, usize)> {
    let plan = plan_of_row(row)?;
    let instruments: usize = plan.shards.iter().map(|s| s.instruments.len()).sum();
    Ok((plan.shards.len(), instruments))
}

fn plan_summary_json(row: &PlanRow) -> ApiResult<Value> {
    let (shards, instruments) = plan_counts(row)?;
    Ok(json!({
        "instance_id": row.instance.as_str(),
        "node_name": row.instance.as_str(),
        "assignment_version": row.assignment_version,
        "mode": row.mode.as_str(),
        "generated_at": row.generated_at,
        "shard_count": shards,
        "instrument_count": instruments,
        "quota_splits": quota_split_rows(&row.quota_splits),
    }))
}

fn assignment_detail_json(row: &PlanRow, meta: Option<&InstanceMeta>) -> ApiResult<Value> {
    let shards = shard_rows(row, meta)?;
    Ok(with_etag(json!({
        "instance_id": row.instance.as_str(),
        "assignment_version": row.assignment_version,
        "mode": row.mode.as_str(),
        "generated_at": row.generated_at,
        "shards": shards,
        "quota_splits": quota_split_rows(&row.quota_splits),
        "plan": row.plan,
        "node_id": row.node_id,
    })))
}

#[derive(Debug, Deserialize, Default)]
struct AssignmentQuery {
    instance: Option<String>,
    venue: Option<String>,
    market: Option<String>,
    host: Option<String>,
    assignment_version: Option<i64>,
    view: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// 每实例的最高版本（`assignment_version` 未指定时只看最新一版）。
fn latest_versions(rows: &[PlanRow]) -> BTreeMap<String, i64> {
    let mut m: BTreeMap<String, i64> = BTreeMap::new();
    for r in rows {
        let e = m.entry(r.instance.clone()).or_insert(i64::MIN);
        *e = (*e).max(r.assignment_version);
    }
    m
}

async fn list_assignments<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<AssignmentQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let meta = instance_meta(&g, &db)?;
    let all = db.list_plan_rows()?;
    let latest = latest_versions(&all);

    let mut items: Vec<Value> = Vec::new();
    for row in &all {
        if let Some(v) = q.assignment_version {
            if row.assignment_version != v {
                continue;
            }
        } else if latest.get(&row.instance) != Some(&row.assignment_version) {
            continue;
        }
        if let Some(want) = q.instance.as_deref() {
            if row.instance != want {
                continue;
            }
        }
        let m = meta.get(&row.instance);
        for s in shard_rows(row, m)? {
            if let Some(want) = q.venue.as_deref() {
                if s["venue"] != json!(want) {
                    continue;
                }
            }
            if let Some(want) = q.market.as_deref() {
                if s["market"] != json!(want) {
                    continue;
                }
            }
            if let Some(want) = q.host.as_deref() {
                let h = s["host"].as_str().unwrap_or("");
                if h != want {
                    continue;
                }
            }
            items.push(s);
        }
    }

    let view = q.view.as_deref().unwrap_or("instance");
    items.sort_by(|a, b| {
        let ord = match view {
            "host" | "machine" => a["host"]
                .as_str()
                .unwrap_or("")
                .cmp(b["host"].as_str().unwrap_or("")),
            "venue" | "exchange" => a["venue"]
                .as_str()
                .unwrap_or("")
                .cmp(b["venue"].as_str().unwrap_or(""))
                .then_with(|| a["market"].as_str().unwrap_or("").cmp(b["market"].as_str().unwrap_or(""))),
            _ => a["instance_id"]
                .as_str()
                .unwrap_or("")
                .cmp(b["instance_id"].as_str().unwrap_or("")),
        };
        ord.then_with(|| {
            a["assignment_version"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&b["assignment_version"].as_i64().unwrap_or(0))
        })
        .then_with(|| {
            a["venue"]
                .as_str()
                .unwrap_or("")
                .cmp(b["venue"].as_str().unwrap_or(""))
        })
        .then_with(|| {
            a["shard_idx"]
                .as_u64()
                .unwrap_or(0)
                .cmp(&b["shard_idx"].as_u64().unwrap_or(0))
        })
    });

    let page: PageOut<Value> = paginate(items, q.limit, q.offset);
    Ok(json_ok(paged_json(&page, |v: &Value| v.clone())))
}

async fn get_assignment<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(instance_id): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let row = db.latest_plan_row(&instance_id)?.ok_or_else(|| {
        ApiError::not_found(format!("实例 `{instance_id}` 尚无指派记录"))
    })?;
    let meta = instance_meta(&g, &db)?;
    Ok(json_etag(
        StatusCode::OK,
        assignment_detail_json(&row, meta.get(&instance_id))?,
    ))
}

async fn assignment_version<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath((instance_id, version)): AxPath<(String, String)>,
) -> ApiResult<Response> {
    let v = parse_id(&version, "assignment_version")?;
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let row = db.plan_row(&instance_id, v)?.ok_or_else(|| {
        ApiError::not_found(format!(
            "实例 `{instance_id}` 的 assignment_version={v} 不存在"
        ))
    })?;
    let meta = instance_meta(&g, &db)?;
    Ok(json_etag(
        StatusCode::OK,
        assignment_detail_json(&row, meta.get(&instance_id))?,
    ))
}

async fn assignment_holders<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let all = db.list_plan_rows()?;
    let latest = latest_versions(&all);
    let mut holders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in &all {
        if latest.get(&row.instance) != Some(&row.assignment_version) {
            continue;
        }
        let plan = plan_of_row(row)?;
        for s in &plan.shards {
            holders
                .entry(s.venue.as_str().to_string())
                .or_default()
                .insert(row.instance.clone());
        }
    }
    let items: Vec<Value> = holders
        .into_iter()
        .map(|(venue, set)| json!({ "venue": venue, "holders": set.len() }))
        .collect();
    Ok(json_ok(json!(items)))
}

#[derive(Debug, Deserialize, Default)]
struct UnassignedQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

async fn assignment_unassigned<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    Query(q): Query<UnassignedQuery>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let result = dry_run(&g)?;
    let items: Vec<Value> = result
        .unassigned
        .iter()
        .map(|(venue, market, idx, reason)| {
            json!({
                "venue": venue,
                "market": market,
                "shard_idx": idx,
                "reason": reason,
            })
        })
        .collect();
    let page: PageOut<Value> = paginate(items, q.limit, q.offset);
    Ok(json_ok(paged_json(&page, |v: &Value| v.clone())))
}

/// 分片签名（试算差异判定：**只比分片集合**，不含 `assignment_version`）。
fn shard_signature(shards: &[ShardPlan]) -> String {
    content_hash_of(&serde_json::to_value(shards).unwrap_or(Value::Null))
}

/// 按当前域文档与活实例**试算**指派（§4.11：不下发、不改任何状态）。
///
/// 与 `ControlPlane::recompute_inner` 同一套输入（候选过滤含 O8 的停用节点 /
/// 非 active 机器、A 方案的逐实例 scope 允许集），差别只在**不 emit、不落库**。
fn dry_run<E: EventCenter + 'static>(g: &ControlPlane<E>) -> ApiResult<AssignmentResult> {
    let db = g.store().sqlite().cloned();

    // O8：停用节点与非 active 机器不进候选（与 `disabled_instances` 同口径）
    let mut disabled: BTreeSet<String> = BTreeSet::new();
    if let Some(db) = &db {
        for node in db.list_nodes()? {
            if !node.enabled {
                disabled.insert(node.name);
                continue;
            }
            if let Some(sid) = node.server_id {
                if let Some(srv) = db.server_by_id(sid)? {
                    if srv.status.as_str() != "active" {
                        disabled.insert(node.name);
                    }
                }
            }
        }
    }

    let candidates: Vec<Candidate> = g
        .instances()
        .into_iter()
        .filter(|k| {
            k.node_kind == NodeKind::Collector
                && matches!(k.status, InstStatus::Registered | InstStatus::Running)
                && !disabled.contains(&k.instance_id)
        })
        .map(|k| Candidate {
            instance_id: k.instance_id.clone(),
            host: k.host.clone(),
            node_kind: k.node_kind,
            reported: k.reported.clone(),
        })
        .collect();
    if candidates.is_empty() {
        return Ok(AssignmentResult::default());
    }

    let mut plan_doc: SubscribePlanDoc = domain_doc(g, ConfigDomain::SubscribePlan)?;
    let blocklist: BlocklistDoc = domain_doc(g, ConfigDomain::InstrumentBlocklist)?;
    let capacity: ProcessCapacityDoc = domain_doc(g, ConfigDomain::ProcessCapacity)?;
    let limits: CapacityLimits = capacity.limits.clone();
    let strategy = Strategy::parse(&capacity.distribution.strategy)?;

    let mut versions = BTreeMap::new();
    for c in &candidates {
        let cur = g.plan_of(&c.instance_id).map(|p| p.assignment_version);
        versions.insert(c.instance_id.clone(), next_assignment_version(cur));
    }

    // A 方案（12 §5）：逐实例 scope → 允许集，并集池统一切分片
    let mut allowed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Some(db) = &db {
        let catalog = g.catalog();
        let canon = |i: &PlanInstrument| -> String {
            format!("{}:{}:{}", i.venue, i.market, i.native_code)
        };
        let mut union: BTreeMap<String, PlanInstrument> = BTreeMap::new();
        for i in &plan_doc.instruments {
            union.insert(canon(i), i.clone());
        }
        for c in &candidates {
            let Some(node) = db.node_by_name(&c.instance_id)? else {
                continue;
            };
            let out = scope::resolve(&node.scope, &node.scope_include, &node.scope_exclude, catalog);
            let mut set: BTreeSet<String> = out.instruments.iter().map(|i| canon(i)).collect();
            for i in &plan_doc.instruments {
                set.insert(canon(i));
            }
            allowed.insert(c.instance_id.clone(), set);
            for i in out.instruments {
                union.insert(canon(&i), i);
            }
        }
        if !allowed.is_empty() {
            plan_doc.instruments = union.into_values().collect();
        }
    }

    Ok(assign(AssignRequest {
        candidates,
        plan_doc,
        blocklist,
        limits,
        strategy,
        mode: PlanMode::Snapshot,
        versions,
        allowed,
    })?)
}

/// 读一个配置域并反序列化（缺失 → 该域的 `Default`）。
fn domain_doc<T: Default + serde::de::DeserializeOwned>(
    g: &ControlPlane<impl EventCenter>,
    d: ConfigDomain,
) -> ApiResult<T> {
    match g.store().current(d)? {
        Some(doc) => serde_json::from_value::<T>(doc.content).map_err(|e| {
            ApiError::validation(format!("{d} 域文档结构不合法：{e}"))
        }),
        None => Ok(T::default()),
    }
}

async fn assignments_dry_run<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    body: Bytes,
) -> ApiResult<Response> {
    let m = if body.is_empty() {
        serde_json::Map::new()
    } else {
        body_object(&body)?
    };
    let want = opt_str(&m, "instance_id").map(str::to_string);
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    let meta = instance_meta(&g, &db)?;
    let result = dry_run(&g)?;

    let mut plans = Vec::new();
    let mut diff = Vec::new();
    for (id, a) in &result.plans {
        if let Some(w) = want.as_deref() {
            if id != w {
                continue;
            }
        }
        let cur = g.plan_of(id);
        let changed = cur
            .map(|p| shard_signature(&p.shards) != shard_signature(&a.plan.shards))
            .unwrap_or(true);
        let instrument_count: usize = a.plan.shards.iter().map(|s| s.instruments.len()).sum();
        let quota = serde_json::to_value(&a.quota_splits).unwrap_or_else(|_| json!({}));
        plans.push(json!({
            "instance_id": id,
            "assignment_version": a.plan.assignment_version,
            "mode": format!("{:?}", a.plan.mode).to_ascii_lowercase(),
            "shard_count": a.plan.shards.len(),
            "instrument_count": instrument_count,
            "quota_splits": quota_split_rows(&quota),
            "shards": a.plan.shards.iter().map(|s| json!({
                "venue": s.venue.as_str(),
                "market": s.market.as_str(),
                "shard_idx": s.shard_idx,
                "instruments": s.instruments.len(),
            })).collect::<Vec<_>>(),
        }));
        diff.push(json!({
            "instance_id": id,
            "changed": changed,
            "current_version": cur.map(|p| p.assignment_version),
            "next_version": a.plan.assignment_version,
            "current_shard_count": cur.map(|p| p.shards.len()),
            "next_shard_count": a.plan.shards.len(),
            "host": meta.get(id).and_then(|m| m.host.clone()),
        }));
    }

    let unassigned: Vec<Value> = result
        .unassigned
        .iter()
        .map(|(venue, market, idx, reason)| {
            json!({ "venue": venue, "market": market, "shard_idx": idx, "reason": reason })
        })
        .collect();
    let holders: Vec<Value> = result
        .holders
        .iter()
        .map(|(venue, n)| json!({ "venue": venue, "holders": n }))
        .collect();

    Ok(json_ok(json!({
        "dry_run": true,
        "plans": plans,
        "diff": diff,
        "unassigned": unassigned,
        "holders": holders,
        "ts": crate::now_ms(),
    })))
}

async fn node_assignments<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(name): AxPath<String>,
    Query(q): Query<Page>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    require_node(&db, &name)?;
    let all = db.list_plan_rows()?;
    let mut rows: Vec<&PlanRow> = all.iter().filter(|r| r.instance == name).collect();
    rows.sort_by(|a, b| b.assignment_version.cmp(&a.assignment_version)); // 版本降序
    let owned: Vec<PlanRow> = rows.into_iter().cloned().collect();
    let (items, total, limit, offset) = q.slice(owned);
    let mut body: Vec<Value> = Vec::with_capacity(items.len());
    for r in &items {
        body.push(plan_summary_json(r)?);
    }
    Ok(json_ok(paged(&body, total, limit, offset)))
}

async fn node_assignment_latest<E: EventCenter + 'static>(
    State(st): State<AppState<E>>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Response> {
    let g = lock_plane(&st).await?;
    let db = sqlite_of(&g)?;
    require_node(&db, &name)?;
    let row = db.latest_plan_row(&name)?.ok_or_else(|| {
        ApiError::not_found(format!("节点 `{name}` 尚无指派记录"))
    })?;
    let meta = instance_meta(&g, &db)?;
    Ok(json_etag(
        StatusCode::OK,
        assignment_detail_json(&row, meta.get(&name))?,
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// 服务入口
// ─────────────────────────────────────────────────────────────────────────────

/// 起 HTTP 服务（`ponk-control serve --http <addr>`，§2.4 同进程监听）。
///
/// 调用方先用 [`AppState::build`] 取得状态与共享锁，再把状态交进来——
/// 控制循环留在调用方的线程上，与本服务共用同一把锁。
pub async fn serve<E: EventCenter + 'static>(
    addr: &str,
    state: AppState<E>,
) -> Result<(), Box<dyn std::error::Error>> {
    let router = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!(
        "[control] HTTP 服务已启动：http://{addr}/api/v1 （JWT HS256；免鉴权仅 /healthz 与 /auth/login）"
    );
    axum::serve(listener, router).await?;
    Ok(())
}
