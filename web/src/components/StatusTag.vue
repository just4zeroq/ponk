<script setup lang="ts">
/** 状态标签：颜色语义统一取自 docs/13 §3.4（utils/status.ts）。 */
import { computed } from 'vue'
import { ElTooltip } from 'element-plus'
import type { StatusChip } from '../utils/status'

const props = defineProps<{
  chip: StatusChip
  /** 是否显示 tooltip 提示。 */
  showHint?: boolean
}>()

const type = computed(() => props.chip.type)
</script>

<template>
  <el-tooltip v-if="showHint !== false && chip.hint" :content="chip.hint" placement="top">
    <el-tag :type="type" size="small" effect="light">{{ chip.label }}</el-tag>
  </el-tooltip>
  <el-tag v-else :type="type" size="small" effect="light">{{ chip.label }}</el-tag>
</template>
