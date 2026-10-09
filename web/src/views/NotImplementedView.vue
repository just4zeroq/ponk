<script setup lang="ts">
/**
 * 「本次交付未实现的页面」（P9 审计日志 / P10 指派结果）。
 *
 * 与「接口未接入」（NotWiredView）的区别：
 * - 这里的接口**在 docs/13 §4 中已定义且属阶段一/二范围**，但本交付的页面清单
 *   （总览 / 采集节点 / 配置域 / 服务器台账 / 节点运行时 / 登录）不含该页；
 * - 类型化客户端已写好（src/api/audit.ts、src/api/assignments.ts），页面留待后续。
 */
import { computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ArrowLeft } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'

const route = useRoute()
const router = useRouter()

const page = computed(() => String((route.meta as { page?: string }).page ?? ''))
const title = computed(() => String((route.meta as { title?: string }).title ?? ''))

const ENDPOINTS: Record<string, string[]> = {
  P9: ['GET /audit', 'GET /audit/{id}', 'GET /audit/export'],
  P10: [
    'GET /assignments',
    'GET /assignments/{instance_id}',
    'GET /assignments/{instance_id}/versions/{version}',
    'GET /assignments/holders',
    'GET /assignments/unassigned',
    'POST /assignments/dry-run',
  ],
}

const endpoints = computed(() => ENDPOINTS[page.value] ?? [])
</script>

<template>
  <div class="page">
    <PageHeader :title="title" :page="page">
      <template #actions>
        <el-button :icon="ArrowLeft" @click="router.push('/')">返回总览</el-button>
      </template>
    </PageHeader>

    <el-result icon="info" :title="`${page} ${title}：本页未实现`" sub-title="该页面不在本次交付的页面清单内；接口已按 docs/13 §4 写成类型化客户端，页面留待后续实现。">
      <template #extra>
        <div class="box">
          <div class="row"><span class="muted">交付范围</span><span>总览 P0 · 服务器台账 P1 · 采集节点 P2 · 配置域 P6 · 实例状态 P7 · 登录</span></div>
          <div class="row"><span class="muted">客户端代码</span><span class="mono">{{ page === 'P9' ? 'src/api/audit.ts' : 'src/api/assignments.ts' }}</span></div>
        </div>

        <h4>该页对应的接口（docs/13 §4）</h4>
        <ul class="eps">
          <li v-for="e in endpoints" :key="e" class="mono">{{ e }}</li>
        </ul>

        <el-alert
          v-if="page === 'P9'"
          type="info"
          :closable="false"
          show-icon
          title="域级审计可在配置域详情的「审计」标签页查看（GET /config/{domain}/audit，仅 publish / rollback）。"
        />
        <el-alert
          v-if="page === 'P10'"
          type="info"
          :closable="false"
          show-icon
          style="margin-top: 10px"
          title="节点详情页展示最新指派摘要与 quota_splits（GET /nodes/{name}/assignments/latest）。"
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
