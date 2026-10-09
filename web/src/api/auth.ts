/**
 * 认证接口（`docs/13 §5.1 / §5.1.1`）：用户名 + 密码 → JWT。
 *
 * - 登录**免鉴权**；`silent: true` 让调用方自己展示错误（避免全局 401 提示与表单提示打架）；
 * - `role` 由服务端从用户表读出并写进 JWT，前端**不做**角色选择；
 * - 登录成功后由 store 落 `sessionStorage`（§5.1），后续请求自动带 `Authorization: Bearer <jwt>`。
 */
import { request } from './http'
import type { CreateUserRequest, LoginRequest, LoginResponse, MeResponse, UserRecord } from './types'

/** POST /auth/login：凭证正确返回 JWT（§5.1.1）。 */
export function login(body: LoginRequest): Promise<LoginResponse> {
  return request<LoginResponse>('/auth/login', { method: 'POST', body, silent: true })
}

/** GET /auth/me：校验 JWT 并取身份（会话恢复、角色刷新）。 */
export function me(signal?: AbortSignal): Promise<MeResponse> {
  return request<MeResponse>('/auth/me', { signal })
}

/** GET /auth/users：用户列表（`write`；响应不含密码哈希）。 */
export function listUsers(signal?: AbortSignal): Promise<{ items: UserRecord[] } | UserRecord[]> {
  return request('/auth/users', { signal })
}

/** POST /auth/users：新建用户（`write`；用户名重复 → 409）。 */
export function createUser(body: CreateUserRequest): Promise<UserRecord> {
  return request<UserRecord>('/auth/users', { method: 'POST', body })
}
