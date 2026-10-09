/** §4.10 运行时状态接口（只读 + 手动重推触发类）。 */
import { request } from './http'
import type {
  ConvergenceSnapshot,
  InstanceListQuery,
  InstanceRow,
  Page,
  RuntimeCounts,
} from './types'

/** GET /runtime/instances 实例列表 + 收敛状态。 */
export function listInstances(query: InstanceListQuery = {}, signal?: AbortSignal): Promise<Page<InstanceRow>> {
  return request<Page<InstanceRow>>('/runtime/instances', { query: { ...query }, signal })
}

/** GET /runtime/instances/{instance_id} 详情（counters、自报能力、门状态）。 */
export function getInstance(instanceId: string, signal?: AbortSignal): Promise<InstanceRow> {
  return request<InstanceRow>(`/runtime/instances/${encodeURIComponent(instanceId)}`, { signal })
}

/** GET /convergence 全量收敛比对快照。 */
export function getConvergence(
  query: { domain?: string; timeout_ms?: number } = {},
  signal?: AbortSignal,
): Promise<ConvergenceSnapshot> {
  return request<ConvergenceSnapshot>('/convergence', { query: { ...query }, signal })
}

/** POST /runtime/instances/{id}/repush 手动重推（可选，触发类；阶段二）。 */
export function repushInstance(instanceId: string): Promise<unknown> {
  return request<unknown>(`/runtime/instances/${encodeURIComponent(instanceId)}/repush`, { method: 'POST' })
}

/** GET /runtime/counts 计数器（供 P0）。 */
export function getRuntimeCounts(signal?: AbortSignal): Promise<RuntimeCounts> {
  return request<RuntimeCounts>('/runtime/counts', { signal })
}
