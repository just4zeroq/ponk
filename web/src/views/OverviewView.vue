<script setup lang="ts">
/**
 * P0 总览（docs/13 §3.3 P0 / §4.3）。
 *
 * 展示：八域当前版本 · 实例计数 · 未收敛清单 · 最近发布 · catalog 与同步状态。
 * 操作全部为跳转：未收敛实例 → P7；发布记录 → P6 对应域；catalog → P4（目录接口未接入，仅提示）。
 * 轮询：配置类聚合 30s（§5.6）。
 */
import { computed } from 'vue'
import { RouterLink } from 'vue-router'
import { useQuery } from '@tanstack/vue-query'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import { getMeta, getOverview } from '../api/overview'
import { getConvergence } from '../api/runtime'
import { qk } from '../query/keys'
import { DOMAINS, PUSH_TEXT, domainLocal } from '../utils/domains'
import { fmtAgo, fmtNum, fmtTs } from '../utils/format'
import { convergenceChip, domainVersionChip } from '../utils/status'
import type { OverviewDomain, PushClass } from '../api/types'

/** 传播类别 tooltip 文案（插槽中 `row.push` 类型可能为 any，统一走此函数）。 */
function pushText(push?: string): string {
  if (!push) return ''
  return PUSH_TEXT[String(push) as PushClass] ?? ''
}

const { data: overview, isError: overviewError, error: overviewErr } = useQuery({
  queryKey: qk.overview(),
  queryFn: ({ signal }) => getOverview(signal),
  refetchInterval: 30_000,
})

// 未收敛清单取自 GET /convergence（§4.10）：§4.3 的 /overview 响应未列该字段，
// 但 §3.3 P0 要求展示「实例、域、期望、实际、持续时长」——见交付报告的接口缺口清单。
const { data: convergence } = useQuery({
  queryKey: qk.convergence(),
  queryFn: ({ signal }) => getConvergence(undefined, signal),
  refetchInterval: 5_000,
})

const { data: meta } = useQuery({
  queryKey: qk.meta(),
  queryFn: ({ signal }) => getMeta(signal),
  staleTime: 5 * 60_000,
})

/** 八域行：以 `/meta` 与本地兜底表补全中文名 / 消费者 / 传播类别。 */
const domainRows = computed<OverviewDomain[]>(() => {
  const items = overview.value?.domains ?? []
  if (items.length) {
    return items.map((d) => {
      const metaRow = meta.value?.domains.find((m) => m.name === d.domain)
      const local = domainLocal(String(d.domain))
      return {
        ...d,
        display: d.display ?? metaRow?.display ?? local?.display ?? String(d.domain),
        consumers: d.consumers ?? metaRow?.consumers ?? local?.consumers,
        push: d.push ?? metaRow?.push ?? local?.push,
      }
    })
  }
  // 后端未返回 domains[] 时，用本地八域表兜底（版本显示为未知）。
  return DOMAINS.map((d) => ({ domain: d.name, display: d.display, current_version: 0, consumers: d.consumers, push: d.push }))
})

const counts = computed(() =>
  overview.value?.instances ?? { running: 0, stopped: 0, dead: 0, converged: 0, not_converged: 0, not_converged_timeout: 0 },
)

/** 未收敛清单（§3.3 P0）。 */
const notConverged = computed(() => convergence.value?.not_converged ?? [])
</script>

<template>
  <div class="page">
    <PageHeader
      title="总览"
      page="P0"
      desc="八域版本 · 实例收敛 · 最近发布 · catalog 与同步状态（docs/13 §3.3 P0）。全部操作为跳转。"
    />

    <el-alert
      v-if="overviewError"
      type="error"
      show-icon
      :closable="false"
      style="margin-bottom: 12px"
      title="总览加载失败"
      :description="(overviewErr as Error)?.message ?? '请求失败'"
    />

    <!-- 实例计数 -->
    <div class="grid grid-4 stats">
      <div class="stat">
        <div class="stat-label">运行中 running</div>
        <div class="stat-value">{{ counts.running }}</div>
        <div class="stat-sub">已注册 / 运行中的实例</div>
      </div>
      <div class="stat">
        <div class="stat-label">已收敛 converged</div>
        <div class="stat-value ok">{{ counts.converged }}</div>
        <div class="stat-sub">期望值与实际值相等</div>
      </div>
      <div class="stat">
        <div class="stat-label">未收敛 not_converged</div>
        <div class="stat-value warn">{{ counts.not_converged }}</div>
        <div class="stat-sub">其中超时（≥30s）：{{ counts.not_converged_timeout }}</div>
      </div>
      <div class="stat">
        <div class="stat-label">下线 / 判死</div>
        <div class="stat-value dim">{{ counts.stopped }} <span class="unit">stopped</span></div>
        <div class="stat-sub">dead {{ counts.dead }}（3×5s 无心跳）</div>
      </div>
    </div>

    <div class="grid grid-2">
      <!-- 八域当前版本 -->
      <section class="panel">
        <h3>八个配置域当前版本</h3>
        <el-table :data="domainRows" size="small" v-loading="!overview">
          <el-table-column label="域" min-width="150">
            <template #default="{ row }">
              <RouterLink :to="`/config/${row.domain}`" class="mono">{{ row.domain }}</RouterLink>
              <div class="muted sub">{{ row.display }}</div>
            </template>
          </el-table-column>
          <el-table-column label="版本" width="92">
            <template #default="{ row }">
              <StatusTag :chip="domainVersionChip(row.current_version)" />
            </template>
          </el-table-column>
          <el-table-column label="更新时间" width="150">
            <template #default="{ row }">
              <span :class="{ muted: !row.updated_at }">{{ row.updated_at ? fmtTs(row.updated_at) : '—' }}</span>
            </template>
          </el-table-column>
          <el-table-column label="传播" width="70">
            <template #default="{ row }">
              <el-tooltip v-if="row.push" :content="pushText(row.push)" placement="top">
                <el-tag size="small" effect="plain" :type="row.push === 'C' ? 'info' : row.push === 'B' ? 'warning' : 'primary'">
                  {{ row.push }}
                </el-tag>
              </el-tooltip>
              <span v-else class="muted">—</span>
            </template>
          </el-table-column>
          <el-table-column label="消费者" min-width="160">
            <template #default="{ row }">
              <div v-if="row.consumers?.length" class="muted consumers">
                <div v-for="(c, i) in row.consumers" :key="i">{{ c }}</div>
              </div>
              <span v-else class="muted">—</span>
            </template>
          </el-table-column>
        </el-table>
        <div class="foot muted">
          版本 0 = 未发布（§3.4）；<span class="mono">ProcessCapacity</span> 不发变更通知（09 §2.2 C）。
        </div>
      </section>

      <!-- 最近发布 -->
      <section class="panel">
        <h3>最近发布（audit_log 最后 10 条）</h3>
        <el-table
          v-if="overview?.recent_publishes?.length"
          :data="overview.recent_publishes"
          size="small"
        >
          <el-table-column label="时间" width="150">
            <template #default="{ row }">{{ fmtTs(row.ts) }}</template>
          </el-table-column>
          <el-table-column label="域 / 目标" min-width="150">
            <template #default="{ row }">
              <RouterLink v-if="row.domain" :to="`/config/${row.domain}`" class="mono">{{ row.domain }}</RouterLink>
              <span v-else class="mono">{{ row.target ?? '—' }}</span>
              <span v-if="row.version" class="muted"> v{{ row.version }}</span>
            </template>
          </el-table-column>
          <el-table-column label="操作" width="90">
            <template #default="{ row }">
              <el-tag size="small" effect="plain" :type="row.action === 'rollback' ? 'warning' : 'primary'">
                {{ row.action ?? 'publish' }}
              </el-tag>
            </template>
          </el-table-column>
          <el-table-column label="发布者" min-width="130">
            <template #default="{ row }">{{ row.publisher ?? row.actor ?? '—' }}</template>
          </el-table-column>
          <el-table-column label="原因" min-width="130">
            <template #default="{ row }"><span class="muted">{{ row.reason || '—' }}</span></template>
          </el-table-column>
        </el-table>
        <el-empty v-else-if="overview" description="暂无发布记录" :image-size="60" />
        <div v-else class="muted" v-loading="true" style="height: 80px" />
        <div class="foot">
          <RouterLink to="/audit">查看完整审计日志（P9，本次未实现）→</RouterLink>
        </div>
      </section>
    </div>

    <!-- 未收敛清单 + catalog/同步 -->
    <div class="grid grid-2">
      <section class="panel">
        <h3>未收敛清单</h3>
        <el-table v-if="convergence" :data="notConverged" size="small" empty-text="当前无未收敛实例">
          <el-table-column label="实例" min-width="140">
            <template #default="{ row }">
              <RouterLink :to="`/runtime?instance=${encodeURIComponent(row.instance_id)}&converged=false`">
                {{ row.instance_id }}
              </RouterLink>
            </template>
          </el-table-column>
          <el-table-column label="域" width="150">
            <template #default="{ row }">
              <RouterLink v-if="row.domain" :to="`/config/${row.domain}`" class="mono">{{ row.domain }}</RouterLink>
              <span v-else class="muted">指派版本（采集）</span>
            </template>
          </el-table-column>
          <el-table-column label="期望" width="70" prop="expected" />
          <el-table-column label="实际" width="70" prop="actual" />
          <el-table-column label="持续" width="120">
            <template #default="{ row }">
              <StatusTag :chip="convergenceChip(false, row.held_ms)" />
            </template>
          </el-table-column>
        </el-table>
        <div v-else class="muted" v-loading="true" style="height: 60px" />
        <div class="foot">
          <RouterLink to="/runtime?converged=false">全部未收敛实例 → 实例状态（P7）</RouterLink>
        </div>
      </section>

      <section class="panel">
        <h3>catalog 与元数据同步</h3>
        <div class="grid grid-2">
          <div class="kv">
            <div class="kv-row"><span class="muted">catalog 更新</span><span>{{ fmtTs(overview?.catalog?.updated_at ?? null) }}</span></div>
            <div class="kv-row"><span class="muted">标的数 symbols</span><span>{{ fmtNum(overview?.catalog?.symbols) }}</span></div>
            <div class="kv-row"><span class="muted">币数 currencies</span><span>{{ fmtNum(overview?.catalog?.currencies) }}</span></div>
          </div>
          <div class="kv">
            <div class="kv-row"><span class="muted">同步周期</span><span>{{ overview?.sync?.interval_ms ? `${Math.round(Number(overview.sync.interval_ms) / 1000)}s` : '—' }}</span></div>
            <div class="kv-row"><span class="muted">上次轮次</span><span>{{ overview?.sync?.last_round_ms ? fmtAgo(Number(overview.sync.last_round_ms)) : '—' }}</span></div>
            <div class="kv-row"><span class="muted">是否在跑</span><span>{{ overview?.sync?.running ? '是' : overview?.sync ? '否' : '—' }}</span></div>
          </div>
        </div>
        <div class="foot">
          目录页面（P3 交易所与市场 / P4 交易对 / P5 币信息）与 P8 元数据同步的读写函数属阶段三，界面标注
          <el-tag size="small" type="warning" effect="plain">未接入</el-tag>
          （docs/13 §6.1 / §7.2 R1），暂不可用。
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
.stats {
  margin-bottom: 14px;
}

.stat-value.ok {
  color: var(--el-color-success);
}

.stat-value.warn {
  color: var(--el-color-warning);
}

.stat-value.dim {
  color: var(--ponk-text-3);
}

.stat-value .unit {
  font-size: 13px;
  font-weight: 400;
}

.sub {
  font-size: 12px;
}

.consumers {
  font-size: 12px;
  line-height: 1.5;
}

.foot {
  margin-top: 10px;
  font-size: 12px;
  line-height: 1.7;
}

.kv-row {
  display: flex;
  justify-content: space-between;
  gap: 10px;
  padding: 5px 0;
  border-bottom: 1px dashed var(--ponk-border);
  font-size: 13px;
}
</style>
