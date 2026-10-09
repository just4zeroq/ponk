<script setup lang="ts">
/**
 * P6 八个配置域列表（docs/13 §3.3 P6 / §4.9 `GET /config/domains`）。
 *
 * 字段：domain、中文名、当前版本（0 = 未发布）、updated_at、消费者、传播类别（A/B/C）、
 *       最近发布者与时间。
 * 轮询 30s（§5.6 配置列表）。
 */
import { computed } from 'vue'
import { RouterLink } from 'vue-router'
import { useQuery } from '@tanstack/vue-query'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import { listConfigDomains } from '../api/config'
import { getMeta } from '../api/overview'
import { qk } from '../query/keys'
import { PUSH_TEXT, domainLocal } from '../utils/domains'
import { fmtTs } from '../utils/format'
import { domainVersionChip } from '../utils/status'
import type { DomainOverview, PushClass } from '../api/types'

const { data, isFetching, isError, error: queryErr } = useQuery({
  queryKey: qk.configDomains(),
  queryFn: ({ signal }) => listConfigDomains(signal),
  refetchInterval: 30_000,
})

const { data: meta } = useQuery({
  queryKey: qk.meta(),
  queryFn: ({ signal }) => getMeta(signal),
  staleTime: 5 * 60_000,
})

/** 传播类别短说明（插槽中 `row.push` 类型可能为 any，统一走此函数）。 */
function pushText(push?: string): string {
  if (!push) return ''
  const key = String(push) as PushClass
  return PUSH_TEXT[key]?.split('：')[1] ?? ''
}

const rows = computed<DomainOverview[]>(() => {
  const items = data.value ?? []
  if (items.length) {
    return items.map((d) => {
      const m = meta.value?.domains.find((x) => x.name === d.domain)
      const local = domainLocal(String(d.domain))
      return {
        ...d,
        display: d.display ?? m?.display ?? local?.display ?? String(d.domain),
        consumers: d.consumers ?? m?.consumers ?? local?.consumers,
        push: d.push ?? m?.push ?? local?.push,
      }
    })
  }
  return []
})
</script>

<template>
  <div class="page">
    <PageHeader
      title="八个配置域"
      page="P6"
      desc="域即版本单位：一域一文档，发布即该域版本 +1（09 §4.1）。回滚 = 发布为目标版本内容为新版本（§5.2）。"
    />

    <el-alert
      v-if="isError"
      type="error"
      show-icon
      :closable="false"
      style="margin-bottom: 12px"
      title="配置域列表加载失败"
      :description="(queryErr as Error)?.message ?? '请求失败'"
    />

    <el-alert
      type="info"
      :closable="false"
      show-icon
      style="margin-bottom: 12px"
      title="「已发布」≠「已收敛」：发布只把版本推前并通知，实例收敛看心跳比对（§5.2）。"
    />

    <div class="panel" v-loading="isFetching">
      <el-table :data="rows" size="small" stripe>
        <el-table-column label="域" min-width="180">
          <template #default="{ row }">
            <RouterLink :to="`/config/${row.domain}`" class="mono">{{ row.domain }}</RouterLink>
            <div class="muted sub">{{ row.display }}</div>
          </template>
        </el-table-column>

        <el-table-column label="当前版本" width="110">
          <template #default="{ row }">
            <StatusTag :chip="domainVersionChip(row.current_version)" />
          </template>
        </el-table-column>

        <el-table-column label="updated_at" width="160">
          <template #default="{ row }">
            <span :class="{ muted: !row.updated_at }">{{ row.updated_at ? fmtTs(row.updated_at) : '—' }}</span>
          </template>
        </el-table-column>

        <el-table-column label="传播类别" width="150">
          <template #default="{ row }">
            <div class="flex-row">
              <el-tag
                v-if="row.push"
                size="small"
                effect="plain"
                :type="row.push === 'C' ? 'info' : row.push === 'B' ? 'warning' : 'primary'"
              >{{ row.push }}</el-tag>
              <span v-else class="muted">—</span>
              <span class="muted sub" v-if="row.push">{{ pushText(row.push) }}</span>
            </div>
          </template>
        </el-table-column>

        <el-table-column label="消费者" min-width="220">
          <template #default="{ row }">
            <div v-if="row.consumers?.length" class="muted consumers">
              <div v-for="(c, i) in row.consumers" :key="i">{{ c }}</div>
            </div>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>

        <el-table-column label="最近发布" min-width="180">
          <template #default="{ row }">
            <span class="mono">{{ row.publisher || '—' }}</span>
            <div class="muted sub">{{ row.reason || '' }}</div>
          </template>
        </el-table-column>

        <el-table-column label="操作" width="220" fixed="right">
          <template #default="{ row }">
            <RouterLink :to="`/config/${row.domain}`">
              <el-button link type="primary" size="small">查看 / 编辑</el-button>
            </RouterLink>
            <RouterLink :to="`/config/${row.domain}?tab=versions`">
              <el-button link type="primary" size="small">版本历史</el-button>
            </RouterLink>
            <RouterLink :to="`/config/${row.domain}?tab=audit`">
              <el-button link type="primary" size="small">审计</el-button>
            </RouterLink>
          </template>
        </el-table-column>
      </el-table>
    </div>

    <section class="panel">
      <h3>传播行为说明（09 §2.2）</h3>
      <ul class="notes">
        <li><b>A</b>：广播 <span class="mono">ConfigChanged</span> 通知 + 实例拉取应答 <span class="mono">ConfigSnapshot</span>。</li>
        <li><b>B</b>：A + 定向推送 <span class="mono">SubscribePlan</span>（拉是正确性保证，推只是加速）。</li>
        <li><b>C</b>：<span class="mono">ProcessCapacity</span> 不发通知，效果以 <span class="mono">assignment_version</span> 体现。</li>
        <li>触发指派重算的三域：<span class="mono">SubscribePlan</span> / <span class="mono">InstrumentBlocklist</span> / <span class="mono">ProcessCapacity</span>（09 §5.4）。</li>
      </ul>
    </section>
  </div>
</template>

<style scoped>
.sub {
  font-size: 12px;
}

.consumers {
  font-size: 12px;
  line-height: 1.5;
}

.notes {
  margin: 0;
  padding-left: 18px;
  font-size: 13px;
  line-height: 1.9;
  color: var(--ponk-text-2);
}
</style>
