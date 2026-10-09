/**
 * 状态色与状态语义（`docs/13 §3.4`）。
 *
 * 所有页面的标签颜色必须经本模块产出，避免与桌面端（11）色语义漂移（§7.2 R10）。
 */
import type { TagProps } from 'element-plus'

export type TagType = TagProps['type']

export interface StatusChip {
  type: TagType
  /** 面板上的文字。 */
  label: string
  /** 补充说明（tooltip / 副标题）。 */
  hint?: string
}

/** 收敛超时阈值（09 §4.4：30s）。 */
export const CONVERGE_TIMEOUT_MS = 30_000

/**
 * 收敛状态色（§3.4 前三行）。
 * @param converged 是否收敛（`instance ∉ Convergence::not_converged`）
 * @param heldMs 未收敛持续时长
 */
export function convergenceChip(converged: boolean | null | undefined, heldMs?: number | null): StatusChip {
  if (converged === null) {
    return { type: 'info', label: '无实例', hint: '该节点尚无运行时实例' }
  }
  if (converged === undefined) {
    return {
      type: 'info',
      label: '收敛未知',
      hint: '服务端未返回 converged 字段；采集可按 applied_version == last_assignment_version 判断（09 §4.4）',
    }
  }
  if (converged) return { type: 'success', label: '已收敛', hint: '期望值与实际值相等' }
  const held = heldMs ?? 0
  if (held < CONVERGE_TIMEOUT_MS) {
    return { type: 'warning', label: '未收敛', hint: `窗口内（${Math.round(held / 1000)}s < 30s），预期下一心跳恢复` }
  }
  return { type: 'danger', label: '未收敛·超时', hint: `已持续 ${Math.round(held / 1000)}s ≥ 30s，控制面已重推，计入 ops.control` }
}

/** 实例运行状态色（§3.4：running/registered 蓝、stopped 灰、dead 灰底红标）。 */
export function instanceStatusChip(status?: string | null): StatusChip {
  switch (status) {
    case 'running':
      return { type: 'primary', label: '运行中' }
    case 'registered':
      return { type: 'primary', label: '已注册' }
    case 'stopped':
      return { type: 'info', label: '正常下线', hint: '收到 shutdown' }
    case 'dead':
      return { type: 'danger', label: '失联 / 判死', hint: '3×5s 无心跳（10 §5）；控制面只标记，不改指派（09 §7）' }
    default:
      return { type: 'info', label: '未上报' }
  }
}

/** 节点启用 / 停用（§3.4）。 */
export function enabledChip(enabled: boolean): StatusChip {
  return enabled
    ? { type: 'success', label: '启用' }
    : { type: 'info', label: '停用', hint: '停用节点不再进入指派候选集（O8）；已有计划不自动撤回' }
}

/** 服务器状态（§3.4：active 绿 / maintenance 黄 / retired 灰）。 */
export function serverStatusChip(status?: string | null): StatusChip {
  switch (status) {
    case 'active':
      return { type: 'success', label: '在用' }
    case 'maintenance':
      return { type: 'warning', label: '维护中' }
    case 'retired':
      return { type: 'info', label: '已退役' }
    default:
      return { type: 'info', label: status ?? '—' }
  }
}

/** 交易所 / 市场启用（§3.4）。 */
export function venueEnabledChip(enabled?: boolean | null): StatusChip {
  if (enabled === undefined || enabled === null) return { type: 'info', label: '—' }
  return enabled ? { type: 'success', label: '启用' } : { type: 'info', label: '停用' }
}

/** 配置域版本状态（§3.4：`current_version = 0` 为未发布，灰，不计入版本历史）。 */
export function domainVersionChip(version: number): StatusChip {
  if (!version) return { type: 'info', label: '未发布', hint: 'current_version = 0，不计入版本历史' }
  return { type: 'success', label: `v${version}` }
}

/** 交易对状态（§3.4：trading 绿 / break 黄 / delisted 灰，所方口径原样）。 */
export function instrumentStatusChip(status?: string | null): StatusChip {
  switch (status) {
    case 'trading':
      return { type: 'success', label: 'trading' }
    case 'break':
      return { type: 'warning', label: 'break' }
    case 'delisted':
      return { type: 'info', label: 'delisted' }
    default:
      return { type: 'info', label: status ?? '—' }
  }
}

/** 币匹配状态（docs/14 §5.2 三级；宁可「未收录」不静默错配）。 */
export function assetMatchChip(status?: string | null): StatusChip {
  switch (status) {
    case 'matched':
      return { type: 'success', label: 'matched', hint: 'symbol 大小写不敏感精确唯一命中，已自动绑定 external_id' }
    case 'ambiguous':
      return {
        type: 'warning',
        label: 'ambiguous',
        hint: '多命中（同 symbol 多个项目）：不自动选，需人工指定 CG id（docs/14 §5.2）',
      }
    case 'not_found':
      return { type: 'info', label: 'not_found', hint: '零命中（含股票、缩放前缀如 1000PEPE）：可手填 CG id（docs/14 §5.2）' }
    default:
      return { type: 'info', label: status ?? '—' }
  }
}

/** 资产类别（12 §3.2 `asset.kind`）。 */
export function assetKindChip(kind?: string | null): StatusChip {
  switch (kind) {
    case 'coin':
      return { type: 'success', label: 'coin' }
    case 'token':
      return { type: 'primary', label: 'token' }
    case 'stock':
      return { type: 'warning', label: 'stock', hint: '股票类：该所交易对跳过链上币信息（docs/14 §6）' }
    case 'index':
      return { type: 'info', label: 'index' }
    default:
      return { type: 'info', label: kind ?? '—' }
  }
}

/** 交易对变更类型（12 §3.3 `instrument_change.change_kind`）。 */
export function changeKindChip(kind?: string | null): StatusChip {
  switch (kind) {
    case 'new':
      return { type: 'success', label: 'new' }
    case 'relisted':
      return { type: 'success', label: 'relisted' }
    case 'delisted':
      return { type: 'danger', label: 'delisted' }
    case 'status':
      return { type: 'warning', label: 'status' }
    case 'precision':
      return { type: 'warning', label: 'precision' }
    case 'type':
      return { type: 'primary', label: 'type' }
    default:
      return { type: 'info', label: kind ?? '—' }
  }
}

/**
 * 同步轮次状态（docs/14 §7.4：**完成前不谎报「已同步」**）。
 *
 * @param running 是否在跑（`sync_run.started_at` 有值而 `finished_at` 为空）
 * @param error 本轮错误
 * @param startedAt 本轮开始时间；无值 = 尚无轮次
 */
export function syncRoundChip(running: boolean, error?: string | null, startedAt?: number | null): StatusChip {
  if (running) return { type: 'primary', label: 'running', hint: '同步进行中，完成后才更新「已同步」时间' }
  if (error) return { type: 'danger', label: '失败', hint: error }
  if (startedAt) return { type: 'success', label: '已完成', hint: '本轮 finished_at 已写入' }
  return { type: 'info', label: '尚无轮次', hint: 'sync_run 表中还没有记录' }
}

/** 未解析例外（§3.4 黄：如实汇报，不静默丢）。 */
export const UNRESOLVED_CHIP: StatusChip = {
  type: 'warning',
  label: '未解析例外',
  hint: '12 §5：不臆造精度、不静默丢',
}

/** 未登记机器（§3.4 黄：只告警，不拒绝注册）。 */
export const UNREGISTERED_HOST_CHIP: StatusChip = {
  type: 'warning',
  label: '未登记机器',
  hint: 'node_runtime.host 不在 server 表（12 §3.5）',
}
