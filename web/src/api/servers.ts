/** §4.4 服务器台账接口。 */
import { request, requestWithEtag, type EtagResult } from './http'
import type {
  InstanceRow,
  NodeRecord,
  Page,
  ServerInput,
  ServerListQuery,
  ServerRecord,
  ServerStatus,
} from './types'

/** GET /servers 分页列表（§4.4）。 */
export function listServers(query: ServerListQuery = {}, signal?: AbortSignal): Promise<Page<ServerRecord>> {
  return request<Page<ServerRecord>>('/servers', { query: { ...query }, signal })
}

/** POST /servers 新建（201 → `ServerRecord`）。 */
export function createServer(body: ServerInput): Promise<ServerRecord> {
  return request<ServerRecord>('/servers', { method: 'POST', body })
}

/** GET /servers/{host} 单条（含 `ETag`，§5.3）。 */
export function getServer(host: string, signal?: AbortSignal): Promise<EtagResult<ServerRecord>> {
  return requestWithEtag<ServerRecord>(`/servers/${encodeURIComponent(host)}`, { signal })
}

/** PUT /servers/{host} 编辑（同 POST + `If-Match`）。 */
export function updateServer(host: string, body: ServerInput, ifMatch: string): Promise<ServerRecord> {
  return request<ServerRecord>(`/servers/${encodeURIComponent(host)}`, {
    method: 'PUT',
    body,
    ifMatch,
  })
}

/** PATCH /servers/{host}/status 改状态（`If-Match`，§4.4 新增）。 */
export function updateServerStatus(host: string, status: ServerStatus, ifMatch: string): Promise<ServerRecord> {
  return request<ServerRecord>(`/servers/${encodeURIComponent(host)}/status`, {
    method: 'PATCH',
    body: { status },
    ifMatch,
  })
}

/** DELETE /servers/{host}（`If-Match`；有关联节点 → 409，§4.4）。 */
export function deleteServer(host: string, ifMatch: string): Promise<void> {
  return request<void>(`/servers/${encodeURIComponent(host)}`, { method: 'DELETE', ifMatch })
}

/** GET /servers/{host}/nodes 该机登记的节点（期望，按 `server_id` 过滤 `list_nodes`）。 */
export function getServerNodes(
  host: string,
  query: { limit?: number; offset?: number } = {},
  signal?: AbortSignal,
): Promise<Page<NodeRecord>> {
  return request<Page<NodeRecord>>(`/servers/${encodeURIComponent(host)}/nodes`, { query: { ...query }, signal })
}

/** GET /servers/{host}/instances 该机运行时实例（按 `host` 过滤 `instances()`）。 */
export function getServerInstances(host: string, signal?: AbortSignal): Promise<InstanceRow[]> {
  return request<InstanceRow[]>(`/servers/${encodeURIComponent(host)}/instances`, { signal })
}

// 注：§4.4 未列服务器台账的导出端点（§5.4 导出对象为节点 TOML / 单域 JSON / 交易对·币·审计 CSV），
// 故本组**不提供**导出函数——不臆造接口。
