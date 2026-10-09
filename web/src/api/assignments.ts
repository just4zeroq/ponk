/**
 * §4.11 指派结果接口。
 *
 * > 状态：§6.1 标注本组除 `plan_of` 外**均为新增**（过滤 / 分页 / 聚合）；
 * > 界面侧本次未实现 P10 页面，仅提供类型化客户端。
 */
import { request } from './http'
import type {
  AssignmentDetail,
  AssignmentListQuery,
  AssignmentListResponse,
  DryRunRequest,
  DryRunResult,
  UnassignedShard,
} from './types'

/** GET /assignments 分片行（`view` 切换按实例 / 按机器 / 按交易所）。 */
export function listAssignments(query: AssignmentListQuery = {}, signal?: AbortSignal): Promise<AssignmentListResponse> {
  return request<AssignmentListResponse>('/assignments', { query: { ...query }, signal })
}

/** GET /assignments/{instance_id} 该实例最新 `SubscribePlan` + `quota_splits`。 */
export function getAssignment(instanceId: string, signal?: AbortSignal): Promise<AssignmentDetail> {
  return request<AssignmentDetail>(`/assignments/${encodeURIComponent(instanceId)}`, { signal })
}

/** GET /assignments/{instance_id}/versions/{version} 指定版本快照。 */
export function getAssignmentVersion(instanceId: string, version: number, signal?: AbortSignal): Promise<AssignmentDetail> {
  return request<AssignmentDetail>(
    `/assignments/${encodeURIComponent(instanceId)}/versions/${version}`,
    { signal },
  )
}

/** GET /assignments/holders 每所持有进程数（配额分摊分母）。 */
export function getAssignmentHolders(signal?: AbortSignal): Promise<Record<string, number> | Array<{ venue: string; holders: number }>> {
  return request<Record<string, number> | Array<{ venue: string; holders: number }>>('/assignments/holders', { signal })
}

/** GET /assignments/unassigned 未指派分片与原因。 */
export function getUnassigned(query: { limit?: number; offset?: number } = {}, signal?: AbortSignal): Promise<UnassignedShard[] | AssignmentListResponse> {
  return request<UnassignedShard[] | AssignmentListResponse>('/assignments/unassigned', { query: { ...query }, signal })
}

/** POST /assignments/dry-run 试算（不下发，§4.11 / 阶段二第 9 项）。 */
export function dryRunAssignments(body: DryRunRequest = {}): Promise<DryRunResult> {
  return request<DryRunResult>('/assignments/dry-run', { method: 'POST', body })
}
