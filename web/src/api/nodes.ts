/** §4.5 采集节点接口（含范围预览与 TOML 导出，§5.4）。 */
import { downloadFile, request, requestWithEtag, type EtagResult } from './http'
import type {
  AssignmentSummary,
  NodeAssignmentsResponse,
  NodeDetail,
  NodeEnabledInput,
  NodeInput,
  NodeListQuery,
  NodeRecord,
  NodeRow,
  NodeRuntimeResponse,
  Page,
  ScopePreview,
} from './types'

/** GET /nodes 节点 + 运行时联表行（§4.5）。 */
export function listNodes(query: NodeListQuery = {}, signal?: AbortSignal): Promise<Page<NodeRow>> {
  return request<Page<NodeRow>>('/nodes', { query: { ...query }, signal })
}

/** POST /nodes 新建（`create_node`，内含 `ScopeRules::validate`）。 */
export function createNode(body: NodeInput): Promise<NodeRecord> {
  return request<NodeRecord>('/nodes', { method: 'POST', body })
}

/** GET /nodes/{name} 定义 + 运行时块 + 最新指派摘要（含 `ETag`）。 */
export function getNode(name: string, signal?: AbortSignal): Promise<EtagResult<NodeDetail>> {
  return requestWithEtag<NodeDetail>(`/nodes/${encodeURIComponent(name)}`, { signal })
}

/** PUT /nodes/{name} 编辑（同 POST + `If-Match`）。 */
export function updateNode(name: string, body: NodeInput, ifMatch: string): Promise<NodeRecord> {
  return request<NodeRecord>(`/nodes/${encodeURIComponent(name)}`, { method: 'PUT', body, ifMatch })
}

/** PATCH /nodes/{name}/enabled 启停（`If-Match`）。 */
export function setNodeEnabled(name: string, enabled: boolean, ifMatch: string): Promise<NodeRecord> {
  return request<NodeRecord>(`/nodes/${encodeURIComponent(name)}/enabled`, {
    method: 'PATCH',
    body: { enabled } satisfies NodeEnabledInput,
    ifMatch,
  })
}

/** DELETE /nodes/{name}（连带删派生计划、保留运行时行，§3.3 P2）。 */
export function deleteNode(name: string, ifMatch: string): Promise<void> {
  return request<void>(`/nodes/${encodeURIComponent(name)}`, { method: 'DELETE', ifMatch })
}

/** GET /nodes/{name}/scope/preview 范围预览（不落库、不下发，§3.3 P2）。 */
export function previewNodeScope(name: string, signal?: AbortSignal): Promise<ScopePreview> {
  return request<ScopePreview>(`/nodes/${encodeURIComponent(name)}/scope/preview`, { signal })
}

/** GET /nodes/{name}/config.toml 导出配置（`text/plain` 下载，§5.4）。 */
export async function downloadNodeToml(name: string): Promise<void> {
  await downloadFile(`/nodes/${encodeURIComponent(name)}/config.toml`, `${name}.toml`, { text: false })
}

/** GET /nodes/{name}/assignments 指派历史（版本降序分页）。 */
export function getNodeAssignments(
  name: string,
  query: { limit?: number; offset?: number } = {},
  signal?: AbortSignal,
): Promise<NodeAssignmentsResponse> {
  return request<NodeAssignmentsResponse>(`/nodes/${encodeURIComponent(name)}/assignments`, {
    query: { ...query },
    signal,
  })
}

/** GET /nodes/{name}/assignments/latest 最新 `SubscribePlan` + `quota_splits`。 */
export function getNodeLatestAssignment(name: string, signal?: AbortSignal): Promise<AssignmentSummary> {
  return request<AssignmentSummary>(`/nodes/${encodeURIComponent(name)}/assignments/latest`, { signal })
}

/** GET /nodes/{name}/runtime 快照键 + 收敛判定。 */
export function getNodeRuntime(name: string, signal?: AbortSignal): Promise<NodeRuntimeResponse> {
  return request<NodeRuntimeResponse>(`/nodes/${encodeURIComponent(name)}/runtime`, { signal })
}
