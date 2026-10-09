/**
 * 会话：**JWT** 存 `sessionStorage`（§5.1「关闭标签即失效，不写 localStorage」）。
 *
 * 登录改为**用户名 + 密码**换 JWT（§5.1.1）：`token` 即 `access_token`，
 * `role` 取自登录响应（服务端从用户表读出，不由前端选择）。
 *
 * 纯函数模块，不依赖 Pinia / Vue，供 HTTP 层直接读取（避免循环引用）。
 */

const KEY_TOKEN = 'ponk.token'
const KEY_ACTOR = 'ponk.actor'
const KEY_ROLE = 'ponk.role'
const KEY_USER = 'ponk.user'
const KEY_EXP = 'ponk.exp'

/** 角色（§5.1 一期两类：read = 全部 GET，write = 写接口与触发类）。 */
export type Role = 'read' | 'write'

export interface Session {
  /** JWT（`access_token`）。 */
  token: string
  /** `X-Actor`：写入 `publisher` 与 `audit_log.actor`；缺省取登录用户名。 */
  actor: string
  role: Role
  /** 登录用户名（与 JWT `sub` 一致）。 */
  username: string
  /** JWT `exp`（epoch 秒）；缺失表示服务端未给，不做过期判定。 */
  expiresAt?: number
}

function safeGet(key: string): string | null {
  try {
    return sessionStorage.getItem(key)
  } catch {
    return null
  }
}

function safeSet(key: string, value: string): void {
  try {
    sessionStorage.setItem(key, value)
  } catch {
    /* 私密模式等场景静默失败 */
  }
}

function safeRemove(key: string): void {
  try {
    sessionStorage.removeItem(key)
  } catch {
    /* ignore */
  }
}

/** 读取当前会话；未登录**或已过期**返回 null（过期即视为未登录，直接走登录页）。 */
export function readSession(): Session | null {
  const token = safeGet(KEY_TOKEN)
  if (!token) return null
  const expRaw = safeGet(KEY_EXP)
  const expiresAt = expRaw ? Number(expRaw) : undefined
  if (expiresAt && Number.isFinite(expiresAt) && expiresAt * 1000 <= Date.now()) {
    clearSession() // 本地先清，避免带着过期 JWT 反复吃 401
    return null
  }
  const roleRaw = safeGet(KEY_ROLE)
  const role: Role = roleRaw === 'read' ? 'read' : 'write'
  const username = safeGet(KEY_USER) ?? ''
  return {
    token,
    actor: safeGet(KEY_ACTOR) || username,
    role,
    username,
    expiresAt,
  }
}

/** 写入会话（登录接口成功后调用）。 */
export function writeSession(session: Session): void {
  safeSet(KEY_TOKEN, session.token)
  safeSet(KEY_ACTOR, session.actor)
  safeSet(KEY_ROLE, session.role)
  safeSet(KEY_USER, session.username)
  if (session.expiresAt !== undefined) safeSet(KEY_EXP, String(session.expiresAt))
  else safeRemove(KEY_EXP)
}

/** 清除会话（登出 / 401）。 */
export function clearSession(): void {
  safeRemove(KEY_TOKEN)
  safeRemove(KEY_ACTOR)
  safeRemove(KEY_ROLE)
  safeRemove(KEY_USER)
  safeRemove(KEY_EXP)
}

/** 是否可写（写接口与触发类）。 */
export function canWrite(): boolean {
  return readSession()?.role === 'write'
}
