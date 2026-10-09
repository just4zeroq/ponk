<script setup lang="ts">
/**
 * 「未接入」页面（P8 元数据同步）。
 *
 * 依据 docs/13 §6.1 与 §7.2 R1：`sync` / `assignments` 仍属阶段三（§8-9/12）。
 * 目录三页（P3 / P4 / P5）已按 docs/14 §9 翻转为 ready，不再走本页。
 * 界面如实标注「未接入」，不展示假数据、不提供假装可用的操作（O5）。
 */
import { computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ArrowLeft } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'
import { capability } from '../api/capabilities'

const route = useRoute()
const router = useRouter()

const page = computed(() => String((route.meta as { page?: string }).page ?? ''))
const title = computed(() => String((route.meta as { title?: string }).title ?? '未接入'))
const capKey = computed(() => String((route.meta as { capability?: string }).capability ?? ''))
const cap = computed(() => capability(capKey.value))

/** 该分组在 §4 中定义的端点（列出来是为了让操作者知道「等什么」）。 */
const ENDPOINTS: Record<string, string[]> = {
  // 目录三组（exchanges / instruments / assets）已接线，见 views/Catalog*.vue
  sync: ['GET /sync/status', 'POST /sync/runs', 'GET /sync/runs', 'GET /sync/runs/{id}', 'GET /sync/targets'],
}

const endpoints = computed(() => ENDPOINTS[capKey.value] ?? [])
</script>

<template>
  <div class="page">
    <PageHeader :title="title" :page="page">
      <template #actions>
        <el-button :icon="ArrowLeft" @click="router.push('/')">返回总览</el-button>
      </template>
    </PageHeader>

    <el-result icon="warning" :title="`${title}：接口未接入`" :sub-title="cap.note">
      <template #extra>
        <div class="box">
          <div class="row"><span class="muted">状态</span><el-tag type="warning" size="small">未接入</el-tag></div>
          <div class="row"><span class="muted">文档依据</span><span class="mono">{{ cap.ref }}</span></div>
          <div class="row"><span class="muted">阶段</span><span>阶段三：目录与同步落库（docs/13 §8-10 ~ 12）</span></div>
        </div>

        <h4>本页对应的接口（已按 docs/13 §4 写好类型化客户端，后端接入后即可用）</h4>
        <ul class="eps">
          <li v-for="e in endpoints" :key="e" class="mono">{{ e }}</li>
        </ul>

        <el-alert
          type="info"
          :closable="false"
          show-icon
          title="过渡期数据源：§7.2 R1 建议先读 ControlPlane::catalog()，待 12 §4.1 落库后切表；界面在接线前不呈现这些数据。"
        />
      </template>
    </el-result>
  </div>
</template>

<style scoped>
.box {
  display: inline-block;
  text-align: left;
  background: #fff;
  border: 1px solid var(--ponk-border);
  border-radius: 6px;
  padding: 12px 16px;
  margin-bottom: 16px;
}

.row {
  display: flex;
  gap: 12px;
  padding: 4px 0;
  font-size: 13px;
}

h4 {
  font-size: 13px;
  margin: 0 0 6px;
}

.eps {
  list-style: none;
  padding: 0;
  margin: 0 auto 16px;
  display: inline-block;
  text-align: left;
  font-size: 12px;
  line-height: 1.9;
  color: var(--ponk-text-2);
}
</style>
