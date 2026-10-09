<script setup lang="ts">
/** 分页条（§4.1：`limit` 默认 50、最大 500；响应 `{items,total,limit,offset}`）。 */
import { computed } from 'vue'

const props = defineProps<{
  total: number
  limit: number
  offset: number
}>()

const emit = defineEmits<{ change: [next: { limit: number; offset: number }] }>()

const page = computed(() => Math.floor(props.offset / Math.max(props.limit, 1)) + 1)

function toPage(p: number): void {
  const next = (p - 1) * props.limit
  if (next !== props.offset) emit('change', { limit: props.limit, offset: next })
}

function toSize(size: number): void {
  emit('change', { limit: size, offset: 0 })
}
</script>

<template>
  <el-pagination
    background
    layout="total, sizes, prev, pager, next, jumper"
    :total="total"
    :page-size="limit"
    :current-page="page"
    :page-sizes="[20, 50, 100, 200, 500]"
    @current-change="toPage"
    @size-change="toSize"
  />
</template>
