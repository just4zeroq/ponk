/**
 * §4.12 元数据同步接口 + docs/14 §9 的两条外部（币信息）同步接口。
 *
 * 节拍分离（docs/14 §3）：
 * - **交易所目录**：5 分钟一轮，记录 `sync_run`，手动触发 `POST /sync/runs`
 * - **外部币信息**：1 小时一轮，记录 `offchain_sync_run`（独立表），
 *   手动触发 `POST /sync/offchain/runs`，上一轮结果 `GET /sync/offchain/status`
 */
import { request } from './http'
import type { OffchainSyncRun, OffchainSyncStatus, Page, SyncOutcome, SyncRun, SyncStatus, SyncTarget } from './types'

/** GET /sync/status 周期、上次轮次、是否在跑、`due`、配额、catalog 概览。 */
export function getSyncStatus(signal?: AbortSignal): Promise<SyncStatus> {
  return request<SyncStatus>('/sync/status', { signal })
}

/** POST /sync/runs 立即触发（202 + 本轮 `SyncOutcome`；重复触发 409/503）。 */
export function triggerSyncRun(): Promise<SyncOutcome> {
  return request<SyncOutcome>('/sync/runs', { method: 'POST' })
}

/** GET /sync/runs 轮次历史（分页）。 */
export function listSyncRuns(query: { limit?: number; offset?: number } = {}, signal?: AbortSignal): Promise<Page<SyncRun>> {
  return request<Page<SyncRun>>('/sync/runs', { query: { ...query }, signal })
}

/** GET /sync/runs/{id} 单轮详情。 */
export function getSyncRun(id: number, signal?: AbortSignal): Promise<SyncRun> {
  return request<SyncRun>(`/sync/runs/${id}`, { signal })
}

/** GET /sync/targets 本轮覆盖的 `(venue, market)` 集合与份额。 */
export function getSyncTargets(signal?: AbortSignal): Promise<SyncTarget[]> {
  return request<SyncTarget[]>('/sync/targets', { signal })
}

// ── docs/14 §9：外部（币信息）同步（与交易所目录节拍分离）─────────────────────

/**
 * POST /sync/offchain/runs 立即触发外部同步（202，复用 forced 模式）。
 * 响应体文档未定义 → 按空体处理。
 */
export function triggerOffchainSyncRun(): Promise<unknown> {
  return request<unknown>('/sync/offchain/runs', { method: 'POST' })
}

/**
 * GET /sync/offchain/status 上一轮 `offchain_sync_run` 结果（含 `rate_limited` / 失败原因）。
 * 响应结构文档未给出 → 返回原始对象，由 `normalizeOffchainStatus` 归一。
 */
export function getOffchainSyncStatus(signal?: AbortSignal): Promise<OffchainSyncStatus> {
  return request<OffchainSyncStatus>('/sync/offchain/status', { signal })
}

/**
 * 把 `GET /sync/offchain/status` 的多种可能形状归一成「最近一轮 + 是否在跑」。
 *
 * 文档（docs/14 §9 / docs/13 §4.12）只说明"上一轮结果"，未给结构，故兜底：
 * `{last_run|last|run:{…}}` / 平铺 `offchain_sync_run` 列 / `{running, …}`。
 */
export function normalizeOffchainStatus(raw: OffchainSyncStatus | null | undefined): {
  running: boolean
  last: OffchainSyncRun | null
} {
  if (!raw) return { running: false, last: null }
  const nested = raw.last_run ?? raw.last ?? raw.run ?? null
  const last: OffchainSyncRun | null =
    nested && typeof nested === 'object'
      ? nested
      : raw.started_at !== undefined
        ? {
            id: raw.id,
            started_at: raw.started_at,
            finished_at: raw.finished_at,
            outcome: raw.outcome,
            error: raw.error,
          }
        : null
  const started = last?.started_at ?? 0
  const finished = last?.finished_at
  // 「已结束」判据：`finished_at` 有值，或已记录错误（12 §3.4b 两者都由同步任务收尾时写）。
  const finishedLike = (finished !== null && finished !== undefined) || !!last?.error
  const running = raw.running ?? (started > 0 && !finishedLike)
  return { running, last }
}
