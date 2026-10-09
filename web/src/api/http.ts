/**
 * 统一 HTTP 客户端（`docs/13 §4.1` 通则 / `§4.2` 错误模型 / `§5.1` 鉴权）。
 *
 * 统一处理：
 * - `Authorization: Bearer <token>` + `X-Actor`（§4.1）
 * - 401 → 清会话并跳登录（§5.1「登录页仅在 401 时出现」）
 * - 403 → 只读提示（§5.1 角色）
 * - 409 → 冲突提示（§5.3 `base_version` 基线不符 / 同名节点 / 删除被关联阻止）
 * - 412 / 428 → 版本过期 / 缺少 `If-Match` 提示（§5.3）
 * - 400 / 422 / 503 / 500 → 按 §4.2 的 `code` 给出中文提示
 */
import { ElNotification } from 'element-plus'
import type { ApiErrorCode, ApiErrorBody } from './types'
import { clearSession, readSession } from './session'

/** API 基址：`VITE_API_BASE`，缺省 `/api/v1`（§4.1）。 */
export const API_BASE = (import.meta.env.VITE_API_BASE ?? '/api/v1').replace(/\/+$/, '')

/** 401 时的跳转回调（由应用启动时注册，避免 HTTP 层依赖 router）。 */
let unauthorizedHandler: (() => void) | null = null

/** 注册 401 处理器（跳登录页）。 */
export function onUnauthorized(handler: () => void): void {
  unauthorizedHandler = handler
}

/** 统一错误对象。 */
export class ApiError extends Error {
  readonly status: number
  readonly code: ApiErrorCode | string
  readonly detail?: Record<string, unknown>

  constructor(status: number, code: ApiErrorCode | string, message: string, detail?: Record<string, unknown>) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.detail = detail
  }

  /** 是否为「版本基线冲突」（409，§5.3）。 */
  get isConflict(): boolean {
    return this.status === 409 || this.code === 'conflict'
  }

  /** 是否为 ETag 不匹配 / 缺失（412 / 428）。 */
  get isPrecondition(): boolean {
    return this.status === 412 || this.status === 428
  }

  /** 是否为鉴权失败（401）。 */
  get isUnauthenticated(): boolean {
    return this.status === 401
  }

  /** 是否为权限不足（403，只读角色）。 */
  get isForbidden(): boolean {
    return this.status === 403
  }
}

export type QueryValue = string | number | boolean | null | undefined
export type QueryParams = Record<string, QueryValue>

export interface RequestOptions {
  method?: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE'
  /** JSON 请求体（自动序列化）。 */
  body?: unknown
  /** 查询参数；`null` / `undefined` / `''` 被忽略。 */
  query?: QueryParams
  /** 行级并发 `If-Match`（§5.3）。 */
  ifMatch?: string
  /** 响应为文本（CSV / TOML / 明文）。 */
  text?: boolean
  /** 响应为 Blob（文件下载）。 */
  blob?: boolean
  /** 已自行处理错误（不弹全局提示）。 */
  silent?: boolean
  signal?: AbortSignal
}

function buildQuery(query?: QueryParams): string {
  if (!query) return ''
  const sp = new URLSearchParams()
  for (const [k, v] of Object.entries(query)) {
    if (v === undefined || v === null || v === '') continue
    sp.append(k, String(v))
  }
  const s = sp.toString()
  return s ? `?${s}` : ''
}

/** 组装完整 URL。 */
export function apiUrl(path: string, query?: QueryParams): string {
  const p = path.startsWith('/') ? path : `/${path}`
  return `${API_BASE}${p}${buildQuery(query)}`
}

/** 取 ETag 头（大小写不敏感）。 */
function headerOf(res: Response, name: string): string | null {
  return res.headers.get(name) || res.headers.get(name.toLowerCase())
}

/** 把后端错误体转成 `ApiError`。 */
async function toApiError(res: Response): Promise<ApiError> {
  let body: unknown = null
  try {
    body = await res.json()
  } catch {
    /* 非 JSON 错误体 */
  }
  const err = (body as ApiErrorBody | null)?.error
  return new ApiError(res.status, err?.code ?? String(res.status), err?.message ?? res.statusText, err?.detail)
}

/**
 * 按状态码分发全局提示（401 / 403 / 409 / 412 / 428）。
 * 401 由 `onUnauthorized` 跳登录。
 */
function notify(err: ApiError): void {
  switch (err.status) {
    case 401:
      clearSession()
      unauthorizedHandler?.()
      return
    case 403:
      ElNotification.warning({
        title: '权限不足（只读）',
        message: `${err.message}。当前 token 角色为 read，仅可访问 GET 接口（§5.1）。`,
        duration: 6000,
      })
      return
    case 409:
      ElNotification.error({
        title: '冲突（409）',
        message: `${err.message}。配置域请用读取到的 base_version 重新提交；行记录请重新读取 ETag 后再写（§5.3）。`,
        duration: 8000,
      })
      return
    case 412:
      ElNotification.error({
        title: '版本过期（412）',
        message: `${err.message}。If-Match 与当前 ETag 不符，数据已被他人修改，请刷新后重试（§5.3）。`,
        duration: 8000,
      })
      return
    case 428:
      ElNotification.error({
        title: '缺少版本基线（428）',
        message: `${err.message}。写接口必须携带 If-Match（行记录）或 base_version（配置域）（§5.3）。`,
        duration: 8000,
      })
      return
    case 400:
    case 422:
      ElNotification.error({ title: err.code === 'validation' ? '校验失败' : '操作不被允许', message: err.message, duration: 8000 })
      return
    case 503:
      ElNotification.warning({ title: '控制面忙（503）', message: `${err.message}。锁等待超时或同步轮次进行中，请稍后重试。`, duration: 6000 })
      return
    case 404:
      ElNotification.error({ title: '未找到（404）', message: err.message, duration: 5000 })
      return
    default:
      if (err.status >= 500) {
        ElNotification.error({ title: '服务端错误', message: err.message, duration: 8000 })
      }
  }
}

/** 执行一次请求，返回 `Response`（不解析体）。 */
async function rawRequest(path: string, opts: RequestOptions = {}): Promise<Response> {
  const session = readSession()
  const headers: Record<string, string> = { Accept: 'application/json' }
  if (session?.token) headers['Authorization'] = `Bearer ${session.token}`
  if (session?.actor) headers['X-Actor'] = session.actor
  if (opts.ifMatch) headers['If-Match'] = opts.ifMatch

  const init: RequestInit = {
    method: opts.method ?? 'GET',
    headers,
    signal: opts.signal,
  }
  if (opts.body !== undefined) {
    headers['Content-Type'] = 'application/json; charset=utf-8'
    init.body = JSON.stringify(opts.body)
  }

  let res: Response
  try {
    res = await fetch(apiUrl(path, opts.query), init)
  } catch (e) {
    const err = new ApiError(0, 'internal', `无法连接控制面：${e instanceof Error ? e.message : String(e)}`)
    if (!opts.silent) notify(err)
    throw err
  }

  if (!res.ok) {
    const err = await toApiError(res)
    if (!opts.silent) notify(err)
    throw err
  }
  return res
}

/**
 * 发起请求并按类型解析响应。
 * `204` / 空体返回 `null as T`。
 */
export async function request<T>(path: string, opts: RequestOptions = {}): Promise<T> {
  const res = await rawRequest(path, opts)
  if (res.status === 204) return null as T
  if (opts.blob) return (await res.blob()) as T
  if (opts.text) return (await res.text()) as T
  const text = await res.text()
  if (!text) return null as T
  try {
    return JSON.parse(text) as T
  } catch (e) {
    const err = new ApiError(res.status, 'internal', `响应不是合法 JSON：${e instanceof Error ? e.message : String(e)}`)
    if (!opts.silent) notify(err)
    throw err
  }
}

/** 带 `ETag` 的请求（读取行级并发用的 ETag，§5.3）。 */
export interface EtagResult<T> {
  data: T
  /** 后端返回的 `ETag` 头；无则为 null（此时按 §5.3 的 `W/"<id>-<updated_at>"` 口径在前端代算）。 */
  etag: string | null
}

/** 发起请求并同时返回响应体与 `ETag` 头。 */
export async function requestWithEtag<T>(path: string, opts: RequestOptions = {}): Promise<EtagResult<T>> {
  const res = await rawRequest(path, opts)
  const etag = headerOf(res, 'etag')
  if (res.status === 204) return { data: null as T, etag }
  const text = await res.text()
  const data = text ? (JSON.parse(text) as T) : (null as T)
  return { data, etag }
}

/** 按 §5.3 口径代算 ETag（后端未回传 `ETag` 头时的兜底）。 */
export function fallbackEtag(id: number | string, updatedAt: number): string {
  return `W/"${id}-${updatedAt}"`
}

/**
 * 文件下载（§5.4 导出）：带 Bearer 头取回后走 Blob，避免直接开窗丢失鉴权头。
 */
export async function downloadFile(path: string, filename: string, opts: RequestOptions = {}): Promise<void> {
  const res = await rawRequest(path, opts)
  const blob = await res.blob()
  const cd = headerOf(res, 'content-disposition')
  let name = filename
  const star = /filename\*=UTF-8''([^;]+)/i.exec(cd ?? '')
  const plain = /filename="?([^";]+)"?/i.exec(cd ?? '')
  if (star) name = decodeURIComponent(star[1])
  else if (plain) name = plain[1]

  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = name
  document.body.appendChild(a)
  a.click()
  a.remove()
  URL.revokeObjectURL(url)
}
