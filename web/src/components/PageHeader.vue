<script setup lang="ts">
/** 页面标题条：标题 + §3.2 页面编号 + 右侧操作。 */
defineProps<{
  title: string
  /** §3.2 页面编号，如 `P2`。 */
  page?: string
  desc?: string
}>()
</script>

<template>
  <div class="page-header">
    <div class="page-title">
      <h2>{{ title }}</h2>
      <span v-if="page" class="page-code">{{ page }}</span>
      <slot name="extra" />
    </div>
    <div class="flex-row">
      <slot name="actions" />
    </div>
  </div>
  <p v-if="desc" class="page-desc">{{ desc }}</p>
  <slot />
</template>
