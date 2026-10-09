<script setup lang="ts">
/**
 * 同步状态条（docs/14 §7.4 / §7.3）：上次同步结果 + running 态 + 「立即同步」按钮。
 *
 * 硬规则：**完成前不谎报「已同步」** —— 只有 `finished_at` 有值才显示「已完成」；
 * 状态接口不可用时如实显示原因，不显示假的轮次结果。
 */
import { computed } from 'vue'
import { Refresh } from '@element-plus/icons-vue'
import StatusTag from './StatusTag.vue'
import type { CatalogSyncState } from '../query/useCatalogSync'
import type { StatusChip } from '../utils/status'
import { useSessionStore } from '../stores/session'
import { syncRoundChip } from '../utils/status'
import { fmtAgo, fmtTs } from '../utils/format'

const props = defineProps<{
  state: CatalogSyncState
  /** 空态时由 `CatalogEmptyState` 提供 §7.4 的按钮，状态条不再重复给一个。 */
  hideTrigger?: boolean
}>()
const emit = defineEmits<{ sync: [] }>()
const session = useSessionStore()

/**
 * 本轮的「跳过原因」（`outcome.skipped[]`）。
 *
 * **这是排查「点了同步没数据」的第一现场**：后端 202 且 `changed=false` 时，
 * 真正的原因全部在 `skipped` 里（端点未接线 / 网络不可达 / 令牌桶节流）。
 * 它是数组，标量过滤抓不到，必须单独上屏。
 */
const skipped = computed(() => {
  const raw = (props.state.last?.outcome as Record<string, unknown> | null)?.skipped
  return Array.isArray(raw) ? (raw as { venue?: string; market?: string; reason?: string }[]) : []
})

/** 全部目标都被跳过 → 本轮实际零产出，不能报「已完成」。 */
const allSkipped = computed(() => {
  const o = props.state.last?.outcome as Record<string, unknown> | undefined
  if (!o || skipped.value.length === 0) return false
  const fetched = Number(o.fetched_symbols ?? 0) + Number(o.fetched_currencies ?? 0)
  return fetched === 0
})

/** 跳过原因去重后的展示行（`reason` 才是人要看的，venue/market 只是前缀）。 */
const skippedLines = computed(() => {
  const seen = new Set<string>()
  const out: string[] = []
  for (const s of skipped.value) {
    const where = [s.venue, s.market].filter(Boolean).join('/')
    const line = where ? `${where} — ${s.reason ?? '未知原因'}` : (s.reason ?? '未知原因')
    if (seen.has(line)) continue
    seen.add(line)
    out.push(line)
  }
  return out
})

const chip = computed<StatusChip>(() => {
  if (props.state.running) return syncRoundChip(true, null, props.state.last?.startedAt ?? null)
  if (allSkipped.value) {
    return {
      type: 'warning',
      label: `已跳过 ${skipped.value.length} 项`,
      hint: skippedLines.value[0] ?? '本轮全部目标被跳过，没有产出',
    }
  }
  return syncRoundChip(false, props.state.last?.error ?? null, props.state.last?.startedAt ?? null)
})

/** `outcome` 里的标量摘要（逐所 / 扁平两种形状都只取标量，避免整块 JSON 上屏）。 */
const outcomeText = computed(() => {
  const o = props.state.last?.outcome
  if (!o) return ''
  return Object.entries(o)
    .filter(([, v]) => ['string', 'number', 'boolean'].includes(typeof v))
    .slice(0, 6)
    .map(([k, v]) => `${k}=${String(v)}`)
    .join(' · ')
})

const finishedAt = computed(() => props.state.last?.finishedAt ?? null)
</script>

<template>
  <div class="sync-bar">
    <StatusTag :chip="chip" />
    <span class="muted">
      <template v-if="state.last?.startedAt">
        上次同步 <span class="mono">{{ fmtTs(state.last.startedAt) }}</span>
        （{{ fmtAgo(state.last.startedAt) }}）
        <template v-if="finishedAt">→ 结束 <span class="mono">{{ fmtTs(finishedAt) }}</span></template>
        <template v-else-if="state.running"> → 进行中</template>
      </template>
      <template v-else>尚无同步轮次记录</template>
    </span>
    <span v-if="outcomeText" class="mono muted outcome">{{ outcomeText }}</span>

    <span class="grow" />

    <el-button size="small" :icon="Refresh" :loading="state.syncing" @click="() => state.refresh()">刷新状态</el-button>
    <el-button
      v-if="session.canWrite && !hideTrigger"
      size="small"
      type="primary"
      :loading="state.syncing"
      :disabled="state.running"
      @click="emit('sync')"
    >{{ state.label }}</el-button>
  </div>

  <!-- 跳过原因：「点了同步没数据」的第一现场（端点未接线 / 网络不可达 / 节流） -->
  <el-alert
    v-if="skippedLines.length && !state.statusError"
    :type="allSkipped ? 'error' : 'warning'"
    show-icon
    :closable="false"
    style="margin-top: 8px"
    :title="
      allSkipped
        ? `本轮 ${skippedLines.length} 项目标全部被跳过 —— 没有拉到任何数据（原因如下）`
        : `本轮有 ${skippedLines.length} 项目标被跳过（未拉到该部分数据）`
    "
  >
    <template #description>
      <div class="skip-line" v-for="line in skippedLines" :key="line">{{ line }}</div>
    </template>
  </el-alert>

  <el-alert
    v-if="state.statusError"
    type="warning"
    show-icon
    :closable="false"
    style="margin-top: 8px"
    title="同步状态不可用 / 上次同步未成功"
    :description="state.statusError"
  />
</template>

<style scoped>
.sync-bar {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  background: var(--ponk-panel);
  border: 1px solid var(--ponk-border);
  border-radius: 6px;
  padding: 9px 12px;
  margin-bottom: 12px;
  font-size: 13px;
}

.grow {
  flex: 1;
}

.outcome {
  font-size: 12px;
}

.skip-line {
  font-family: var(--el-font-family-mono, monospace);
  font-size: 12px;
  line-height: 1.6;
  word-break: break-all;
}
</style>
