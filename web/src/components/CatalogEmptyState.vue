<script setup lang="ts">
/**
 * 目录三页对称的空态（docs/14 §7.4）：
 * 「尚无数据 —— <节拍>，可立即触发」+ 按钮 + 上次同步结果（内嵌状态条）。
 *
 * 按钮调用 forced 接口：交易对/交易所 → `POST /sync/runs`；币 → `POST /sync/offchain/runs`。
 * 同步进行中由状态条显示 `running`（读 `started_at` / `finished_at`），**完成前不谎报「已同步」**。
 */
import { computed } from 'vue'
import { useSessionStore } from '../stores/session'
import CatalogSyncBar from './CatalogSyncBar.vue'
import type { CatalogSyncState } from '../query/useCatalogSync'

const props = defineProps<{
  state: CatalogSyncState
  /** 节拍描述，如「每 5 分钟自动同步一次」。 */
  intervalText: string
  /** 可选标题。 */
  title?: string
}>()

const emit = defineEmits<{ sync: [] }>()
const session = useSessionStore()

const line = computed(
  () => `${props.title ?? '尚无数据'} —— ${props.intervalText}，可立即触发。`,
)
</script>

<template>
  <div class="panel empty-state">
    <div class="empty-title">
      <span class="dot" />
      {{ line }}
    </div>

    <div class="flex-row actions">
      <el-button
        v-if="session.canWrite"
        type="primary"
        :loading="state.syncing"
        :disabled="state.running"
        @click="emit('sync')"
      >{{ state.label }}</el-button>
      <slot name="extra" />
      <span class="muted" v-if="!session.canWrite">当前为只读角色，无法手动触发同步（docs/13 §5.1）</span>
    </div>

    <!-- §7.4：空态必须带「上次同步结果」；状态条本身不给触发按钮，避免与上面的按钮重复。 -->
    <CatalogSyncBar :state="state" hide-trigger />

    <slot />
  </div>
</template>

<style scoped>
.empty-state {
  text-align: left;
}

.empty-title {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 14px;
  font-weight: 600;
  margin-bottom: 10px;
}

.dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--el-color-info);
  flex: 0 0 auto;
}

.actions {
  margin-bottom: 12px;
}
</style>
