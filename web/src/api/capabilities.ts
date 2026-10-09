/**
 * 接口接入状态登记（O5 已定范围）。
 *
 * 依据 `docs/13 §6.1` 的「已有 / 已有·封装 / 新增」与 `§8` 的阶段划分：
 * - **阶段一**（只读 + 校验/导出）与**阶段二**（写入与回滚）覆盖的接口 → 本次可用；
 * - 目录三组（`exchanges` / `instruments` / `assets`）按 `docs/14 §9` **已翻转为 ready**；
 * - 其余阶段三接口（`sync` = P8 元数据同步页、`assignments` = P10）仍标注**未接入**，
 *   界面如实显示，不假装可用。
 *
 * 说明：这是**前端按文档标注的能力状态**，不代表后端实际实现进度——
 * 后端 `crates/control/src/http.rs` 由另一路实现，若实际接入情况不同，以此文件为准调整。
 */

/** 分组的接入状态。 */
export type CapabilityStatus = 'ready' | 'not_wired'

export interface Capability {
  status: CapabilityStatus
  /** 文档依据。 */
  ref: string
  /** 界面文案。 */
  note: string
}

const NOT_WIRED_NOTE = '未接入：属阶段三（docs/13 §8-10/11/12），后端读写函数尚未提供。'

/** §4 分组 → 接入状态。 */
export const CAPABILITIES: Record<string, Capability> = {
  overview: { status: 'ready', ref: 'docs/13 §4.3 / §8-3', note: '只读聚合' },
  servers: { status: 'ready', ref: 'docs/13 §4.4 / §8-6', note: '读写（含 ETag / 关联校验）' },
  nodes: { status: 'ready', ref: 'docs/13 §4.5 / §8-4/6', note: '读写 + 范围预览 + TOML 导出' },
  config: { status: 'ready', ref: 'docs/13 §4.9 / §8-7/8', note: '读写（校验 / 发布 / 回滚 / 导入导出）' },
  runtime: { status: 'ready', ref: 'docs/13 §4.10 / §8-3/9', note: '只读 + 手动重推（触发类）' },
  audit: { status: 'ready', ref: 'docs/13 §4.13 / §8-3', note: '只读全表分页' },
  // docs/14 §9「能力状态翻转」：三页已接真实视图。
  exchanges: { status: 'ready', ref: 'docs/13 §4.6 / docs/14 §6', note: '读写（两步创建 / 编辑 If-Match / 启停 / 删除二次确认）' },
  instruments: { status: 'ready', ref: 'docs/13 §4.7 / docs/14 §7.2', note: '只读（筛选 / 抽屉详情 + 变更历史 / CSV 导出）' },
  assets: { status: 'ready', ref: 'docs/13 §4.8 / docs/14 §7.3', note: '双 Tab + 有限写（kind / external_id）+ 立即同步币信息' },
  // §9 的两条 offchain 接口（POST /sync/offchain/runs、GET /sync/offchain/status）由后端另行接入，
  // P8 页面本体仍未交付 → 保持 not_wired。
  sync: { status: 'not_wired', ref: 'docs/13 §6.1 / §8-12', note: NOT_WIRED_NOTE },
  assignments: { status: 'not_wired', ref: 'docs/13 §6.1 / §8-9', note: '除 plan_of 外均为新增；本次未实现 P10 页面。' },
}

/** 查询某分组的接入状态。 */
export function capability(group: keyof typeof CAPABILITIES | string): Capability {
  return CAPABILITIES[group] ?? { status: 'not_wired', ref: 'docs/13 §4', note: '未登记的能力状态。' }
}

/** 本次交付已实现的页面（§3.2 的 P 编号）。 */
export const IMPLEMENTED_PAGES = ['P0', 'P1', 'P2', 'P3', 'P4', 'P5', 'P6', 'P7'] as const
