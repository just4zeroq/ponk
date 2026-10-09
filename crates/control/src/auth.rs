//! 认证层（`docs/13-控制面管理界面与API.md` §5.1 / §5.1.1、`docs/12-控制面数据库.md` §3.8）。
//!
//! 三件事：
//!
//! 1. **口令**：`control_user.password_hash` 只存 **argon2id**（PHC 串）；
//!    比对走 `password-hash` 的恒定时间比较（`Output: PartialEq → CtEq`），
//!    任何接口不返回、不进日志。
//! 2. **JWT（HS256）**：claims `sub`（用户名）/ `role`（`read`\|`write`）/ `iat` /
//!    `exp` / `iss = ponk-control`；默认 8h，到期返回**专用错误** `token_expired`。
//! 3. **首启引导**：`control_user` 为空且未以 `--bootstrap none` 启动时，
//!    HTTP 启动自动创建内置管理员 `admin`（§5.1「首启引导」）。
//!
//! **硬约束**（§5.1）：密钥缺失则 HTTP 拒绝启动（不自动生成随机密钥）；
//! 凭证错误一律 `401 invalid_credentials`，**不区分「用户不存在 / 密码错」**（防枚举）。

use std::sync::OnceLock;

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use jsonwebtoken::errors::ErrorKind as JwtErrorKind;
use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::ControlError;
use crate::sqlite::SqliteStore;

// ─────────────────────────────────────────────────────────────────────────────
// 角色（一期两类，docs/13 §5.1）
// ─────────────────────────────────────────────────────────────────────────────

/// token 角色（一期两类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 全部 `GET` / `HEAD`。
    Read,
    /// 写接口与触发类。
    Write,
}

impl Role {
    /// 线格式字符串（`read` / `write`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
        }
    }

    /// 解析 `read` / `write`（大小写不敏感）；未知值 → `None`。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" | "r" => Some(Role::Read),
            "write" | "w" => Some(Role::Write),
            _ => None,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 错误（§5.1.1 的 code 集合；HTTP 层据此映射状态码）
// ─────────────────────────────────────────────────────────────────────────────

/// 认证错误（`code` 即 §5.1.1 / `docs/13 §4.2` 的错误码）。
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// 凭证错误（**不区分**「用户不存在 / 密码错 / 已停用」，防用户名枚举）。
    #[error("invalid_credentials")]
    InvalidCredentials,

    /// 用户表为空（`--bootstrap none` + 空库）。
    #[error("no_users")]
    NoUsers,

    /// JWT 已过期（`exp`）。
    #[error("token_expired")]
    TokenExpired,

    /// JWT 无效（签名 / `iss` / 结构不符）。
    #[error("invalid_token")]
    InvalidToken,

    /// 用户名已存在。
    #[error("username_exists")]
    UsernameExists,

    /// 用户不存在。
    #[error("user_not_found")]
    UserNotFound,

    /// 入参不合法（空口令、空补丁、`--jwt-ttl` 之外的字段级错误）。
    #[error("validation: {0}")]
    Validation(String),

    /// 数据库 / IO 错误。
    #[error(transparent)]
    Db(#[from] ControlError),

    /// 口令哈希或 JWT 签发失败（内部错误，**不回显口令与密钥**）。
    #[error("crypto: {0}")]
    Crypto(String),
}

impl AuthError {
    /// 「入参不合法」便捷构造。
    pub fn validation(msg: impl Into<String>) -> Self {
        AuthError::Validation(msg.into())
    }
}

impl From<AuthError> for ControlError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Db(c) => c,
            other => ControlError::Illegal(other.to_string()),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 口令：argon2id
// ─────────────────────────────────────────────────────────────────────────────

/// 计算 argon2id 口令哈希（PHC 串，随机盐）。
///
/// 明文只在本进程内存中短暂出现，**不写日志、不进错误消息**。
pub fn hash_password(plain: &str) -> Result<String, AuthError> {
    if plain.is_empty() {
        return Err(AuthError::validation("密码不能为空"));
    }
    let hash = Argon2::default()
        .hash_password(plain.as_bytes())
        .map_err(|e| AuthError::Crypto(format!("argon2id 哈希失败：{e}")))?;
    Ok(hash.to_string())
}

/// 恒定时间校验口令（`password-hash` 的 `Output` 用 `CtEq` 比较）。
///
/// `stored` 损坏（非 PHC 串）时返回 `false` 而非报错——对外一律表现为「凭证错误」。
pub fn verify_password(plain: &str, stored: &str) -> bool {
    Argon2::default().verify_password(plain.as_bytes(), stored).is_ok()
}

/// 「用户不存在」时也要跑一次等价开销的哈希比对，避免**用响应时间区分**用户是否存在。
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        // 常量、非机密；哈希失败在本实现下不可能发生。
        hash_password("ponk-control-dummy-password").expect("argon2id 哈希常量失败")
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 用户记录（`docs/12 §3.8 control_user`）
// ─────────────────────────────────────────────────────────────────────────────

/// 用户（**永不含口令哈希**；`GET /auth/users` / `POST` / `PATCH` 的出站形态）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserRecord {
    /// 用户名 = JWT 的 `sub` = 审计 actor 的缺省值。
    pub username: String,
    /// `read` / `write`。
    pub role: Role,
    /// 是否停用。
    pub disabled: bool,
    /// 创建时间（UTC 毫秒）。
    pub created_at: u64,
    /// 更新时间（UTC 毫秒）。
    pub updated_at: u64,
}

/// 新建用户入参（`POST /auth/users` / `ponk-control user add`）。
#[derive(Debug, Clone, Deserialize)]
pub struct NewUser {
    /// 用户名（唯一）。
    pub username: String,
    /// 明文口令（**只在入站出现**，落库前转 argon2id）。
    pub password: String,
    /// `read` / `write`。
    pub role: Role,
}

/// 改用户补丁（`PATCH /auth/users/{username}`）：三字段均可缺省。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserPatch {
    /// 新口令。
    pub password: Option<String>,
    /// 新角色。
    pub role: Option<Role>,
    /// 停用 / 启用。
    pub disabled: Option<bool>,
}

impl UserPatch {
    /// 是否为空补丁（三字段全缺省）。
    pub fn is_empty(&self) -> bool {
        self.password.is_none() && self.role.is_none() && self.disabled.is_none()
    }
}

fn user_from_row(username: String, role: String, disabled: i64, created_at: i64, updated_at: i64) -> Result<UserRecord, AuthError> {
    let role = Role::parse(&role).ok_or_else(|| AuthError::Crypto("control_user.role 非法".into()))?;
    Ok(UserRecord {
        username,
        role,
        disabled: disabled != 0,
        created_at: created_at as u64,
        updated_at: updated_at as u64,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// SqliteStore 上的用户 CRUD（`docs/12 §3.8`）
// ─────────────────────────────────────────────────────────────────────────────

impl SqliteStore {
    /// 用户总数（首启引导判空 + `POST /auth/login` 判 `403 no_users`）。
    pub fn count_users(&self) -> Result<u64, AuthError> {
        let n: i64 = self.with_read(|c| {
            c.query_row("SELECT COUNT(*) FROM control_user", [], |r| r.get(0))
        })?;
        Ok(n.max(0) as u64)
    }

    /// 用户名是否已存在。
    pub fn user_exists(&self, username: &str) -> Result<bool, AuthError> {
        let found: Option<i64> = self.with_read(|c| {
            c.query_row(
                "SELECT id FROM control_user WHERE username = ?1",
                params![username],
                |r| r.get(0),
            )
            .optional()
        })?;
        Ok(found.is_some())
    }

    /// 建用户（口令在此转 argon2id；用户名重复 → [`AuthError::UsernameExists`]）。
    pub fn create_user(&self, user: &NewUser) -> Result<UserRecord, AuthError> {
        let username = user.username.trim();
        if username.is_empty() {
            return Err(AuthError::validation("用户名不能为空"));
        }
        if self.user_exists(username)? {
            return Err(AuthError::UsernameExists);
        }
        let hash = hash_password(&user.password)?;
        let ts = crate::now_ms() as i64;
        // `ON CONFLICT DO NOTHING`：并发下让唯一约束说话，避免双写。
        let inserted: usize = self.with_write(|c| {
            c.execute(
                "INSERT INTO control_user
                   (username, password_hash, role, disabled, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 0, ?4, ?4)
                 ON CONFLICT(username) DO NOTHING",
                params![username, hash, user.role.as_str(), ts],
            )
        })?;
        if inserted == 0 {
            return Err(AuthError::UsernameExists);
        }
        self.get_user(username)?.ok_or(AuthError::UsernameExists)
    }

    /// 读单个用户（**不含口令哈希**）。
    pub fn get_user(&self, username: &str) -> Result<Option<UserRecord>, AuthError> {
        let row: Option<(String, String, i64, i64, i64)> = self.with_read(|c| {
            c.query_row(
                "SELECT username, role, disabled, created_at, updated_at
                 FROM control_user WHERE username = ?1",
                params![username],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
        })?;
        row.map(|(u, role, d, ca, ua)| user_from_row(u, role, d, ca, ua))
            .transpose()
    }

    /// 列全部用户（**不含口令哈希**；`GET /auth/users`）。
    pub fn list_users(&self) -> Result<Vec<UserRecord>, AuthError> {
        let rows: Vec<(String, String, i64, i64, i64)> = self.with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT username, role, disabled, created_at, updated_at
                 FROM control_user ORDER BY username",
            )?;
            let mapped = stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            let mut out = Vec::new();
            for r in mapped {
                out.push(r?);
            }
            Ok(out)
        })?;
        rows.into_iter()
            .map(|(u, role, d, ca, ua)| user_from_row(u, role, d, ca, ua))
            .collect()
    }

    /// 改用户：口令（转 argon2id）/ 角色 / 停用（`PATCH /auth/users/{username}`）。
    pub fn update_user(&self, username: &str, patch: &UserPatch) -> Result<UserRecord, AuthError> {
        if self.get_user(username)?.is_none() {
            return Err(AuthError::UserNotFound);
        }
        if patch.is_empty() {
            return Err(AuthError::validation(
                "补丁为空：password / role / disabled 至少给一个",
            ));
        }
        let hash = match &patch.password {
            Some(p) => Some(hash_password(p)?),
            None => None,
        };
        let ts = crate::now_ms() as i64;
        self.with_write(|c| {
            c.execute(
                "UPDATE control_user SET
                   password_hash = COALESCE(?2, password_hash),
                   role          = COALESCE(?3, role),
                   disabled      = COALESCE(?4, disabled),
                   updated_at    = ?5
                 WHERE username = ?1",
                params![
                    username,
                    hash,
                    patch.role.map(Role::as_str),
                    patch.disabled.map(|b| b as i64),
                    ts
                ],
            )?;
            Ok(())
        })?;
        self.get_user(username)?.ok_or(AuthError::UserNotFound)
    }

    /// 登录校验：返回该用户的角色；**任何失败都折叠为 `None`**（防枚举）。
    ///
    /// * 用户不存在 → 跑一次等价开销的哑哈希比对，避免时间侧信道；
    /// * 已停用 → 走完真实比对再返回 `None`（开销与正常登录一致）。
    pub fn verify_credentials(&self, username: &str, password: &str) -> Result<Option<Role>, AuthError> {
        let row: Option<(String, i64, String)> = self.with_read(|c| {
            c.query_row(
                "SELECT role, disabled, password_hash FROM control_user WHERE username = ?1",
                params![username],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
        })?;
        let Some((role, disabled, hash)) = row else {
            let _ = verify_password(password, dummy_hash());
            return Ok(None);
        };
        let ok = verify_password(password, &hash);
        let role = Role::parse(&role);
        if !ok || disabled != 0 || role.is_none() {
            return Ok(None);
        }
        Ok(role)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JWT（HS256）
// ─────────────────────────────────────────────────────────────────────────────

/// 签发者（§5.1：`iss = ponk-control`）。
pub const JWT_ISSUER: &str = "ponk-control";

/// 默认令牌有效期：**8h**（§5.1 `--jwt-ttl`）。
pub const DEFAULT_JWT_TTL_SECS: u64 = 8 * 60 * 60;

/// JWT claims（§5.1：`sub` / `role` / `iat` / `exp` / `iss`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// 用户名。
    pub sub: String,
    /// `read` / `write`（**取自用户表**，不由客户端自声明）。
    pub role: String,
    /// 固定 `ponk-control`。
    pub iss: String,
    /// 签发时刻（epoch 秒）。
    pub iat: u64,
    /// 到期时刻（epoch 秒）。
    pub exp: u64,
}

/// HS256 JWT 签发 / 校验（密钥来自 `PONK_JWT_SECRET` 或 `--jwt-secret`）。
#[derive(Debug, Clone)]
pub struct Jwt {
    secret: String,
}

impl Jwt {
    /// 用给定密钥构造（调用方负责校验密钥非空——见 `http::AuthConfig::new`）。
    pub fn new(secret: impl Into<String>) -> Self {
        Jwt {
            secret: secret.into(),
        }
    }

    /// 签发一枚 JWT：`exp = iat + ttl_secs`。
    pub fn issue(
        &self,
        username: &str,
        role: Role,
        ttl_secs: u64,
    ) -> Result<String, AuthError> {
        let iat = jsonwebtoken::get_current_timestamp();
        let claims = Claims {
            sub: username.to_string(),
            role: role.as_str().to_string(),
            iss: JWT_ISSUER.to_string(),
            iat,
            exp: iat.saturating_add(ttl_secs),
        };
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(self.secret.as_bytes()),
        )
        .map_err(|e| AuthError::Crypto(format!("签发 JWT 失败：{e}")))
    }

    /// 校验 JWT：签名 + `iss` + `exp`（**零宽容时钟偏移**）。
    ///
    /// 过期 → [`AuthError::TokenExpired`]（HTTP 映射 `401 token_expired`）；
    /// 其余一切失败 → [`AuthError::InvalidToken`]（`401 unauthenticated`）。
    pub fn verify(&self, token: &str) -> Result<Claims, AuthError> {
        let mut validation = Validation::new(Algorithm::HS256);
        // §5.1「到期返回 401 token_expired」：不允许默认 60s 宽容把过期当有效。
        validation.leeway = 0;
        validation.set_issuer(&[JWT_ISSUER]);
        validation.set_required_spec_claims(&["exp", "sub", "iss"]);

        let data = decode::<Claims>(
            token,
            &DecodingKey::from_secret(self.secret.as_bytes()),
            &validation,
        )
        .map_err(|e| match *e.kind() {
            JwtErrorKind::ExpiredSignature => AuthError::TokenExpired,
            _ => AuthError::InvalidToken,
        })?;

        // `jsonwebtoken` 的判据是 `exp < now`（等于视为未过期）；此处收紧为
        // `exp <= now`，使 `--jwt-ttl 0` 也立即失效，行为与直觉一致。
        if data.claims.exp <= jsonwebtoken::get_current_timestamp() {
            return Err(AuthError::TokenExpired);
        }
        if Role::parse(&data.claims.role).is_none() {
            return Err(AuthError::InvalidToken);
        }
        Ok(data.claims)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 首启引导（§5.1「首启引导（默认账号）」）
// ─────────────────────────────────────────────────────────────────────────────

/// 内置管理员用户名（§5.1）。
pub const DEFAULT_ADMIN_USERNAME: &str = "admin";

/// 内置默认口令（§5.1：**只在空库首次创建**，登录响应与启动日志强制标注）。
pub const DEFAULT_ADMIN_PASSWORD: &str = "admin123";

/// `--bootstrap` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BootstrapMode {
    /// 缺省：用户表为空时创建内置管理员。
    #[default]
    Auto,
    /// `--bootstrap none`：不创建（空库登录回 `403 no_users`）。
    Disabled,
}

impl BootstrapMode {
    /// 解析 CLI 取值（`default` / `auto` / `none` / `off`）。
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" | "auto" | "on" | "1" => Ok(BootstrapMode::Auto),
            "none" | "off" | "disabled" | "0" => Ok(BootstrapMode::Disabled),
            other => Err(format!(
                "--bootstrap 取值必须是 default / none，得到 `{other}`"
            )),
        }
    }
}

/// 首启引导结果（供启动日志与 `POST /auth/login` 的 `default_password` 使用）。
#[derive(Debug, Clone, PartialEq)]
pub struct BootstrapInfo {
    /// 被创建的用户名。
    pub username: String,
    /// 是否由本次启动创建（空库 + `--bootstrap default`）。
    pub created: bool,
    /// 该账号是否仍使用**内置默认口令**。
    pub default_password: bool,
}

/// 首启引导：`control_user` **为空**且 `mode = Auto` 时创建内置管理员。
///
/// * 口令优先级：`password_opt`（`PONK_ADMIN_PASSWORD` / `--admin-password`）
///   > 内置默认 `admin123`；
/// * 仅在表为空时创建一次，之后永不重建、永不动已有用户；
/// * `mode = Disabled`（`--bootstrap none`）→ 不创建，直接 `Ok(None)`。
pub fn ensure_bootstrap_admin(
    store: &SqliteStore,
    password_opt: Option<&str>,
    mode: BootstrapMode,
) -> Result<Option<BootstrapInfo>, AuthError> {
    if mode == BootstrapMode::Disabled {
        return Ok(None);
    }
    if store.count_users()? > 0 {
        return Ok(None);
    }
    // 空串视同「未设置」——避免把空口令当成管理员口令。
    let (password, is_default) = match password_opt.filter(|p| !p.is_empty()) {
        Some(p) => (p, false),
        None => (DEFAULT_ADMIN_PASSWORD, true),
    };
    store.create_user(&NewUser {
        username: DEFAULT_ADMIN_USERNAME.to_string(),
        password: password.to_string(),
        role: Role::Write,
    })?;
    Ok(Some(BootstrapInfo {
        username: DEFAULT_ADMIN_USERNAME.to_string(),
        created: true,
        default_password: is_default,
    }))
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
    fn password_roundtrip_is_constant_time_and_hash_is_never_plaintext() {
        let h = hash_password("s3cret-pass").expect("hash");
        assert!(h.starts_with("$argon2id$"), "PHC 串：{h}");
        assert!(!h.contains("s3cret-pass"), "哈希里不能出现明文");
        assert!(verify_password("s3cret-pass", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("s3cret-pass", "not-a-phc-string"));
        assert!(hash_password("").is_err(), "空口令拒绝");
    }

    #[test]
    fn user_crud_never_exposes_hash() {
        let (s, _d) = store();
        let rec = s
            .create_user(&NewUser {
                username: "alice".into(),
                password: "pw-alice".into(),
                role: Role::Write,
            })
            .expect("create");
        assert_eq!(rec.username, "alice");
        assert_eq!(rec.role, Role::Write);
        assert!(!rec.disabled);

        // 重复用户名
        let dup = s.create_user(&NewUser {
            username: "alice".into(),
            password: "other".into(),
            role: Role::Read,
        });
        assert!(matches!(dup, Err(AuthError::UsernameExists)));

        assert_eq!(s.count_users().expect("count"), 1);
        let listed = s.list_users().expect("list");
        assert_eq!(listed.len(), 1);
        assert!(!serde_json::to_string(&listed).expect("ser").contains("password"));

        // 登录校验
        assert_eq!(
            s.verify_credentials("alice", "pw-alice").expect("verify"),
            Some(Role::Write)
        );
        assert_eq!(s.verify_credentials("alice", "nope").expect("verify"), None);
        assert_eq!(s.verify_credentials("ghost", "pw-alice").expect("verify"), None);

        // 改密 / 改角色 / 停用
        let patched = s
            .update_user(
                "alice",
                &UserPatch {
                    password: Some("new-pw".into()),
                    role: Some(Role::Read),
                    disabled: Some(true),
                },
            )
            .expect("patch");
        assert_eq!(patched.role, Role::Read);
        assert!(patched.disabled);
        assert_eq!(s.verify_credentials("alice", "new-pw").expect("v"), None, "停用后不可登录");
        assert!(matches!(
            s.update_user("ghost", &UserPatch::default()),
            Err(AuthError::UserNotFound)
        ));
    }

    #[test]
    fn jwt_issue_verify_and_expiry() {
        let jwt = Jwt::new("unit-test-secret");
        let tok = jwt.issue("alice", Role::Write, 60).expect("issue");
        let claims = jwt.verify(&tok).expect("verify");
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.role, "write");
        assert_eq!(claims.iss, JWT_ISSUER);
        assert_eq!(claims.exp - claims.iat, 60);

        // 换密钥 → 签名不符
        let other = Jwt::new("another-secret");
        assert!(matches!(other.verify(&tok), Err(AuthError::InvalidToken)));

        // ttl=0 → 立即过期（专用错误）
        let expired = jwt.issue("alice", Role::Read, 0).expect("issue");
        assert!(matches!(jwt.verify(&expired), Err(AuthError::TokenExpired)));

        // 篡改签名段 → 校验失败（改 role 的载荷同样过不了签名）
        let mut parts: Vec<String> = tok.split('.').map(str::to_string).collect();
        let sig = parts[2].clone();
        let flipped: String = sig
            .chars()
            .enumerate()
            .map(|(i, c)| if i == 0 { if c == 'A' { 'B' } else { 'A' } } else { c })
            .collect();
        parts[2] = flipped;
        assert!(matches!(
            jwt.verify(&parts.join(".")),
            Err(AuthError::InvalidToken)
        ));

        // 缺 `sub` / `role` 的令牌同样拒绝（claims 结构不完整）
        assert!(matches!(jwt.verify("a.b.c"), Err(AuthError::InvalidToken)));
    }

    #[test]
    fn bootstrap_mode_parses_cli_values() {
        for v in ["default", "auto", "DEFAULT", "on"] {
            assert_eq!(
                BootstrapMode::parse(v).expect(v),
                BootstrapMode::Auto,
                "`{v}`"
            );
        }
        for v in ["none", "off", "NONE", "disabled"] {
            assert_eq!(
                BootstrapMode::parse(v).expect(v),
                BootstrapMode::Disabled,
                "`{v}`"
            );
        }
        assert!(BootstrapMode::parse("maybe").is_err());
    }

    #[test]
    fn bootstrap_only_creates_once_and_respects_mode() {
        let (s, _d) = store();

        // none → 不创建
        assert!(ensure_bootstrap_admin(&s, None, BootstrapMode::Disabled)
            .expect("none")
            .is_none());
        assert_eq!(s.count_users().expect("c"), 0);

        // 空库 + 默认口令
        let info = ensure_bootstrap_admin(&s, None, BootstrapMode::Auto)
            .expect("bootstrap")
            .expect("应创建");
        assert!(info.created);
        assert!(info.default_password, "内置默认口令必须标注");
        assert_eq!(info.username, DEFAULT_ADMIN_USERNAME);
        assert_eq!(
            s.verify_credentials(DEFAULT_ADMIN_USERNAME, DEFAULT_ADMIN_PASSWORD)
                .expect("v"),
            Some(Role::Write)
        );

        // 已有用户 → 不再重建（即使给了别的口令）
        let again = ensure_bootstrap_admin(&s, Some("other-pw"), BootstrapMode::Auto)
            .expect("second")
            .map(|i| i.default_password);
        assert_eq!(again, None, "表非空时永不重建");
        assert_eq!(s.count_users().expect("c"), 1);

        // 显式口令 → 非默认
        let (s2, _d2) = store();
        let info2 = ensure_bootstrap_admin(&s2, Some("from-env"), BootstrapMode::Auto)
            .expect("b")
            .expect("应创建");
        assert!(!info2.default_password, "显式口令不算默认口令");
        assert_eq!(
            s2.verify_credentials(DEFAULT_ADMIN_USERNAME, "from-env").expect("v"),
            Some(Role::Write)
        );
    }
}
