/** 会话本地状态（Pinia）。服务端状态一律走 vue-query（§2.3）。 */
import { computed, ref } from 'vue'
import { defineStore } from 'pinia'
import { authApi } from '../api'
import { clearSession, readSession, writeSession, type Role, type Session } from '../api/session'

export const useSessionStore = defineStore('session', () => {
  const session = ref<Session | null>(readSession())

  const token = computed(() => session.value?.token ?? '')
  const actor = computed(() => session.value?.actor ?? '')
  const role = computed<Role>(() => session.value?.role ?? 'read')
  /** 登录用户名（JWT `sub`；用作 `X-Actor` 与审计 actor 的缺省值）。 */
  const username = computed(() => session.value?.username ?? '')
  const loggedIn = computed(() => !!session.value?.token)
  /** 只读角色（§5.1：read = 全部 GET）。 */
  const readOnly = computed(() => loggedIn.value && role.value === 'read')
  const canWrite = computed(() => loggedIn.value && role.value === 'write')

  /**
   * 登录（§5.1）：**用户名 + 密码 → JWT**。
   *
   * - `role` 取服务端返回值（由用户表决定），不接受前端自选；
   * - `actor` 缺省取用户名（§5.1：`publisher = <instance>/<sub>`）；
   * - 失败原样抛出（`invalid_credentials` / `no_users` / 网络错误），由登录页内联展示。
   */
  async function login(username: string, password: string): Promise<void> {
    const res = await authApi.login({ username: username.trim(), password })
    const s: Session = {
      token: res.access_token,
      actor: username.trim(),
      role: res.role,
      username: res.username || username.trim(),
      expiresAt: res.expires_in ? Math.floor(Date.now() / 1000) + res.expires_in : undefined,
    }
    writeSession(s)
    session.value = s
  }

  function logout(): void {
    clearSession()
    session.value = null
  }

  /** 401 处理器调用：服务端判定 token 无效时同步本地状态。 */
  function invalidate(): void {
    session.value = null
  }

  return {
    session,
    token,
    actor,
    role,
    username,
    loggedIn,
    readOnly,
    canWrite,
    login,
    logout,
    invalidate,
  }
})
