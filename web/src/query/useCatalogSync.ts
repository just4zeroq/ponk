/**
 * 目录三页对称的「手动同步 + 上次同步结果 + running 态」（docs/14 §7.4）。
 *
 * - **交易所目录**（P3 / P4）：`GET /sync/status` + `GET /sync/runs?limit=1` 读 `sync_run`，
 *   手动触发 `POST /sync/runs`（§4.12，forced 模式）。
 * - **币信息**（P5）：`GET /sync/offchain/status` 读 `offchain_sync_run`，
 *   手动触发 `POST /sync/offchain/runs`（docs/14 §9）。
 *
 * 硬规则：**完成前不谎报「已同步」** —— 只有 `finished_at` 有值（或已记录错误）才判为结束；
 * 触发后轮询到本轮结束、或 180s 超时如实报「仍在后台运行」。
 */
import { computed, onScopeDispose, reactive, ref, watchEffect } from 'vue'
import { useQuery, useQueryClient } from '@tanstack/vue-query'
import { qk } from './keys'
import {
  getOffchainSyncStatus,
  getSyncStatus,
  listSyncRuns,
  normalizeOffchainStatus,
  triggerOffchainSyncRun,
  triggerSyncRun,
} from '../api/sync'

/** 同步目标：`catalog` = 交易所目录（5 分钟）；`offchain` = 外部币信息（1 小时）。 */
export type CatalogSyncKind = 'catalog' | 'offchain'

/** 上次同步结果（`sync_run` / `offchain_sync_run` 一行）。 */
export interface SyncLastRun {
  id?: number
  startedAt: number | null
  finishedAt: number | null
  error: string | null
  outcome: Record<string, unknown> | null
}

/** 传给展示组件的状态（`reactive` 包装 → 模板里直接读标量）。 */
export interface CatalogSyncState {
  kind: CatalogSyncKind
  /** 本轮是否在跑（`running`）。 */
  running: boolean
  /** 手动触发后正在等待本轮结束（按钮 loading）。 */
  syncing: boolean
  last: SyncLastRun | null
  /** 状态接口本身不可用时的说明（不假装有结果）。 */
  statusError: string | null
  /** 手动同步按钮文案（docs/14 §7.3「立即同步币信息」）。 */
  label: string
  refresh: () => Promise<void>
  trigger: () => Promise<void>
}

function errText(e: unknown): string {
  if (e && typeof e === 'object' && 'message' in e) return String((e as Error).message)
  return String(e)
}

/** 轮询节拍：手动触发后每 2s 读一次轮次状态。 */
const POLL_MS = 2_000
/** 等待本轮结束的上限；超时如实报「仍在后台运行」，不谎报完成。 */
const POLL_TIMEOUT_MS = 180_000

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

export function useCatalogSync(kind: CatalogSyncKind): CatalogSyncState {
  const qc = useQueryClient()
  let disposed = false
  onScopeDispose(() => {
    disposed = true
  })

  const localError = ref<string | null>(null)

  // ── 交易所目录：状态 + 最近一轮 ───────────────────────────────────────────
  const statusQ = useQuery({
    queryKey: qk.syncStatus(),
    queryFn: ({ signal }) => getSyncStatus(signal),
    staleTime: 3_000,
    retry: false,
    enabled: kind === 'catalog',
  })
  const runsQ = useQuery({
    queryKey: qk.syncRuns(1),
    queryFn: ({ signal }) => listSyncRuns({ limit: 1 }, signal),
    staleTime: 3_000,
    retry: false,
    enabled: kind === 'catalog',
  })

  // ── 外部币信息：单一状态端点 ───────────────────────────────────────────────
  const offchainQ = useQuery({
    queryKey: qk.offchainStatus(),
    queryFn: ({ signal }) => getOffchainSyncStatus(signal),
    staleTime: 3_000,
    retry: false,
    enabled: kind === 'offchain',
  })

  const last = computed<SyncLastRun | null>(() => {
    if (kind === 'offchain') {
      const n = normalizeOffchainStatus(offchainQ.data.value)
      const r = n.last
      if (!r) return null
      return {
        id: r.id,
        startedAt: r.started_at ?? null,
        finishedAt: r.finished_at ?? null,
        error: r.error ?? null,
        outcome: (r.outcome as Record<string, unknown> | null | undefined) ?? null,
      }
    }
    const r = runsQ.data.value?.items?.[0]
    if (!r) return null
    return {
      id: r.id,
      startedAt: r.started_at ?? null,
      finishedAt: r.finished_at ?? null,
      error: r.error ?? null,
      outcome: (r.outcome as unknown as Record<string, unknown> | null | undefined) ?? null,
    }
  })

  const running = computed(() => {
    if (kind === 'offchain') return normalizeOffchainStatus(offchainQ.data.value).running
    if (statusQ.data.value?.running === true) return true
    const l = last.value
    // started_at 有值而 finished_at 为空 → 本轮未结束（docs/14 §7.4）
    return !!l && !!l.startedAt && !l.finishedAt && !l.error
  })

  const statusError = computed<string | null>(() => {
    if (localError.value) return localError.value
    if (kind === 'offchain') {
      return offchainQ.isError.value ? errText(offchainQ.error.value) : null
    }
    if (statusQ.isError.value) return errText(statusQ.error.value)
    if (runsQ.isError.value) return errText(runsQ.error.value)
    return null
  })

  async function refresh(): Promise<void> {
    if (kind === 'offchain') {
      await offchainQ.refetch()
      return
    }
    await Promise.all([statusQ.refetch(), runsQ.refetch()])
  }

  async function invalidate(): Promise<void> {
    await Promise.all([
      qc.invalidateQueries({ queryKey: ['ponk', 'exchanges'] }),
      qc.invalidateQueries({ queryKey: ['ponk', 'instruments'] }),
      qc.invalidateQueries({ queryKey: ['ponk', 'instrumentChanges'] }),
      qc.invalidateQueries({ queryKey: ['ponk', 'assets'] }),
      qc.invalidateQueries({ queryKey: ['ponk', 'venueAssets'] }),
      qc.invalidateQueries({ queryKey: ['ponk', 'syncStatus'] }),
      qc.invalidateQueries({ queryKey: ['ponk', 'syncRuns'] }),
      qc.invalidateQueries({ queryKey: ['ponk', 'offchainStatus'] }),
    ])
  }

  async function trigger(): Promise<void> {
    if (state.syncing) return
    state.syncing = true
    localError.value = null

    // 触发前的轮次基线：用于识别「新一轮已落库」（强制触发可能快到看不见 running）。
    const baseline = last.value?.startedAt ?? 0

    try {
      if (kind === 'offchain') await triggerOffchainSyncRun()
      else await triggerSyncRun()
    } catch (e) {
      // http.ts 已按 §4.2 弹全局提示；这里只把原因留在状态条上，不谎报成功。
      localError.value = `触发失败：${errText(e)}`
      state.syncing = false
      return
    }

    let done = false
    let sawRunning = false
    const deadline = Date.now() + POLL_TIMEOUT_MS
    while (Date.now() < deadline) {
      if (disposed) {
        state.syncing = false
        return
      }
      await sleep(POLL_MS)
      await refresh()
      if (running.value) {
        sawRunning = true
        continue
      }
      if (sawRunning) {
        done = true
        break
      }
      const cur = last.value
      if (cur && (cur.startedAt ?? 0) > baseline && cur.finishedAt) {
        done = true
        break
      }
    }

    state.syncing = false
    if (!done && !disposed) {
      localError.value = `等待本轮同步结束超过 ${POLL_TIMEOUT_MS / 1000}s，可能仍在后台运行；请稍后刷新查看结果。`
    }
    await invalidate()
  }

  const state = reactive<CatalogSyncState>({
    kind,
    running: false,
    syncing: false,
    last: null,
    statusError: null,
    label: kind === 'offchain' ? '立即同步币信息' : '立即同步',
    refresh,
    trigger,
  })

  // computed → reactive 标量（模板里直接读 `state.running`）。
  watchEffect(() => {
    state.running = running.value
    state.last = last.value
    state.statusError = statusError.value
  })

  return state
}
