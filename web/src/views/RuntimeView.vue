<script setup lang="ts">
/**
 * P7 实例状态（运行时）（docs/13 §3.3 P7 / §4.10）。
 *
 * - 展示：`instance_id` / `node_kind` / `host` / `status` / 收敛（domain·expected·actual·held_ms）/
 *         `applied_version` vs `last_assignment_version` / `started_at` / `last_seen_ts` /
 *         `lost_count` / `unconverged_since` / `last_repush_ts` / `counters` / 自报承载能力
 * - 筛选：`node_kind`、`status`、`converged`、`host`、`domain`（支持 `?instance=` 深链）
 * - 操作：只读；展开 counters；跳转节点定义（P2）与指派（P10）；手动重推（触发类）
 * - 轮询 5s（§5.6，心跳周期 5s）
 *
 * 收敛判据（09 §4.4）：采集比 `applied_version == last_assignment_version`；
 * 其余节点比 `counters["cfg.<domain>"] == 该域当前版本`；控制面不参与比对。
 */
import { computed, onMounted, reactive } from 'vue'
import { RouterLink, useRoute } from 'vue-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/vue-query'
import { ElMessage } from 'element-plus'
import { Refresh } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import PagerBar from '../components/PagerBar.vue'
import { listInstances, repushInstance } from '../api/runtime'
import { qk } from '../query/keys'
import { useSessionStore } from '../stores/session'
import { fmtAgo, fmtTs, prettyJson } from '../utils/format'
import { convergenceChip, instanceStatusChip, UNREGISTERED_HOST_CHIP } from '../utils/status'
import type { InstanceListQuery, InstanceRow } from '../api/types'

const session = useSessionStore()
const route = useRoute()
const qc = useQueryClient()

const filters = reactive<{
  node_kind: string
  status: string
  converged: string
  host: string
  domain: string
  instance: string
  limit: number
  offset: number
}>({
  node_kind: '',
  status: '',
  converged: '',
  host: '',
  domain: '',
  instance: '',
  limit: 50,
  offset: 0,
})

onMounted(() => {
  const q = route.query
  if (q.node_kind) filters.node_kind = String(q.node_kind)
  if (q.status) filters.status = String(q.status)
  if (q.converged) filters.converged = String(q.converged)
  if (q.host) filters.host = String(q.host)
  if (q.domain) filters.domain = String(q.domain)
  if (q.instance) filters.instance = String(q.instance)
})

const queryParams = computed<InstanceListQuery>(() => ({
  node_kind: filters.node_kind || undefined,
  status: filters.status || undefined,
  converged: filters.converged === '' ? undefined : filters.converged === 'true',
  host: filters.host || undefined,
  domain: filters.domain || undefined,
  limit: filters.limit,
  offset: filters.offset,
}))

const { data, isFetching, refetch, isError: listError, error: listErr } = useQuery({
  queryKey: qk.instances(queryParams.value),
  queryFn: ({ signal }) => listInstances(queryParams.value, signal),
  refetchInterval: 5_000, // §5.6：运行时 5s
})

/** `?instance=` 深链（P0 未收敛清单 / P6 收敛进度跳转）在前端二次过滤。 */
const rows = computed(() => {
  const items = data.value?.items ?? []
  if (!filters.instance) return items
  const kw = filters.instance.toLowerCase()
  return items.filter((r) => r.instance_id.toLowerCase().includes(kw))
})

const total = computed(() => (filters.instance ? rows.value.length : (data.value?.total ?? rows.value.length)))

function applyFilters(): void {
  filters.offset = 0
  void refetch()
}

function resetFilters(): void {
  filters.node_kind = ''
  filters.status = ''
  filters.converged = ''
  filters.host = ''
  filters.domain = ''
  filters.instance = ''
  filters.offset = 0
  void refetch()
}

function onPage(next: { limit: number; offset: number }): void {
  filters.limit = next.limit
  filters.offset = next.offset
  void refetch()
}

/**
 * 收敛判定（09 §4.4）：优先取服务端 `converged`；
 * 采集节点若服务端未返回，则按 `applied_version == last_assignment_version` 前端推断。
 */
function convergedOf(row: InstanceRow): boolean | null | undefined {
  if (typeof row.converged === 'boolean') return row.converged
  if (row.node_kind === 'collector') {
    if (row.applied_version === undefined || row.applied_version === null) return undefined
    if (row.last_assignment_version === undefined || row.last_assignment_version === null) return undefined
    return row.applied_version === row.last_assignment_version
  }
  return undefined
}

/** 收敛持续时长（未收敛记录来自 `not_converged`）。 */
function heldOf(row: InstanceRow): number | null {
  if (row.converged === false && row.not_converged) return row.not_converged.held_ms
  if (row.unconverged_since) return Date.now() - row.unconverged_since
  return null
}

// ── 手动重推（触发类，§4.10 / 阶段二第 9 项） ───────────────────────────────
const repush = useMutation({
  mutationFn: (id: string) => repushInstance(id),
  onSuccess: () => {
    ElMessage.success('已触发重推')
    void qc.invalidateQueries({ queryKey: ['ponk', 'instances'] })
    void qc.invalidateQueries({ queryKey: ['ponk', 'convergence'] })
  },
})

/** 采集节点定义同名跳转（`instance_id` = 节点 `name`）。 */
function nodeLink(row: InstanceRow): string | null {
  return row.node_kind === 'collector' ? `/nodes/${encodeURIComponent(row.instance_id)}` : null
}
</script>

<template>
  <div class="page">
    <PageHeader
      title="实例状态"
      page="P7"
      desc="node_runtime 热数据（每 5s 心跳写入）· 轮询 5s（§5.6）· 状态字段由控制信道写入，界面不提供编辑入口（§1.3）。"
    >
      <template #actions>
        <el-button :icon="Refresh" @click="() => refetch()">刷新</el-button>
      </template>
    </PageHeader>

    <el-alert
      v-if="listError"
      type="error"
      show-icon
      :closable="false"
      style="margin-bottom: 12px"
      title="实例状态加载失败"
      :description="(listErr as Error)?.message ?? '请求失败'"
    />

    <div class="filters">
      <el-select v-model="filters.node_kind" placeholder="node_kind" clearable style="width: 130px" @change="applyFilters">
        <el-option label="collector" value="collector" />
        <el-option label="signal" value="signal" />
        <el-option label="alert" value="alert" />
        <el-option label="factor" value="factor" />
        <el-option label="storage" value="storage" />
        <el-option label="data_svc" value="data_svc" />
        <el-option label="control" value="control" />
      </el-select>
      <el-select v-model="filters.status" placeholder="status" clearable style="width: 130px" @change="applyFilters">
        <el-option label="registered" value="registered" />
        <el-option label="running" value="running" />
        <el-option label="stopped" value="stopped" />
        <el-option label="dead" value="dead" />
      </el-select>
      <el-select v-model="filters.converged" placeholder="收敛" clearable style="width: 130px" @change="applyFilters">
        <el-option label="已收敛" value="true" />
        <el-option label="未收敛" value="false" />
      </el-select>
      <el-select v-model="filters.domain" placeholder="按消费域" clearable style="width: 200px" @change="applyFilters">
        <el-option v-for="d in ['subscribe_plan', 'signal_rules', 'alert_rules', 'factor_rules', 'channels', 'process_capacity', 'serialization', 'instrument_blocklist']" :key="d" :label="d" :value="d" />
      </el-select>
      <el-input v-model="filters.host" placeholder="host" clearable style="width: 150px" @change="applyFilters" />
      <el-input v-model="filters.instance" placeholder="instance_id" clearable style="width: 170px" @change="applyFilters" @clear="resetFilters" />
      <el-button @click="resetFilters">重置</el-button>
      <span class="muted">共 {{ total }} 条</span>
    </div>

    <div class="panel">
      <el-table
        :data="rows"
        v-loading="isFetching"
        size="small"
        stripe
        row-key="instance_id"
      >
        <el-table-column type="expand">
          <template #default="{ row }">
            <div class="expand">
              <div class="grid grid-2">
                <div>
                  <h4>counters（含 cfg.&lt;domain&gt;、capacity.*）</h4>
                  <pre class="code">{{ prettyJson(row.counters ?? {}) }}</pre>
                </div>
                <div>
                  <h4>自报承载能力 ReportedCapacity</h4>
                  <pre class="code">{{ prettyJson(row.reported ?? {}) }}</pre>
                  <div class="muted">
                    消费域：{{ (row.consumed_domains ?? []).join(', ') || '—' }}
                  </div>
                </div>
              </div>
            </div>
          </template>
        </el-table-column>

        <el-table-column label="instance_id" min-width="170" fixed="left">
          <template #default="{ row }">
            <RouterLink v-if="nodeLink(row)" :to="nodeLink(row)!" class="mono">{{ row.instance_id }}</RouterLink>
            <span v-else class="mono">{{ row.instance_id }}</span>
            <div class="muted sub">
              <RouterLink v-if="nodeLink(row)" :to="`/assignments?instance=${encodeURIComponent(row.instance_id)}`">指派 → P10</RouterLink>
            </div>
          </template>
        </el-table-column>

        <el-table-column label="kind" width="96">
          <template #default="{ row }">
            <el-tag size="small" effect="plain" :type="row.node_kind === 'collector' ? 'primary' : 'info'">
              {{ row.node_kind }}
            </el-tag>
          </template>
        </el-table-column>

        <el-table-column label="host" min-width="150">
          <template #default="{ row }">
            <div class="flex-row">
              <span class="mono">{{ row.host ?? '—' }}</span>
              <StatusTag v-if="row.host && row.host_registered === false" :chip="UNREGISTERED_HOST_CHIP" />
            </div>
          </template>
        </el-table-column>

        <el-table-column label="status" width="120">
          <template #default="{ row }">
            <StatusTag :chip="instanceStatusChip(row.status)" />
          </template>
        </el-table-column>

        <el-table-column label="收敛" min-width="180">
          <template #default="{ row }">
            <StatusTag :chip="convergenceChip(convergedOf(row), heldOf(row))" />
            <div v-if="row.not_converged" class="muted sub">
              {{ row.not_converged.domain ?? '指派版本' }}：{{ row.not_converged.expected }} vs {{ row.not_converged.actual }}
            </div>
          </template>
        </el-table-column>

        <el-table-column label="applied / assignment" width="160">
          <template #default="{ row }">
            <span class="mono">{{ row.applied_version ?? '—' }} / {{ row.last_assignment_version ?? '—' }}</span>
          </template>
        </el-table-column>

        <el-table-column label="started_at" width="150">
          <template #default="{ row }">{{ fmtTs(row.started_at) }}</template>
        </el-table-column>

        <el-table-column label="last_seen" width="120">
          <template #default="{ row }">{{ fmtAgo(row.last_seen_ts) }}</template>
        </el-table-column>

        <el-table-column label="lost" width="70">
          <template #default="{ row }">
            <span :class="{ warn: row.lost_count }">{{ row.lost_count ?? 0 }}</span>
          </template>
        </el-table-column>

        <el-table-column label="unconverged_since" width="150">
          <template #default="{ row }">
            <span :class="{ muted: !row.unconverged_since }">{{ fmtTs(row.unconverged_since) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="last_repush" width="150">
          <template #default="{ row }">
            <span :class="{ muted: !row.last_repush_ts }">{{ fmtTs(row.last_repush_ts) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="操作" width="110" fixed="right">
          <template #default="{ row }">
            <el-tooltip
              content="手动重推（触发类）：§6.1 标注 repush_plan 为私有，公开入口需新增——若后端未接入将返回 404/405。"
              placement="left"
            >
              <el-button
                v-if="session.canWrite"
                link
                type="warning"
                size="small"
                :loading="repush.isPending.value"
                @click="repush.mutate(row.instance_id)"
              >重推</el-button>
            </el-tooltip>
          </template>
        </el-table-column>
      </el-table>

      <div class="table-footer">
        <PagerBar :total="total" :limit="filters.limit" :offset="filters.offset" @change="onPage" />
      </div>
    </div>

    <section class="panel">
      <h3>判据说明</h3>
      <ul class="notes">
        <li>采集：比较 <span class="mono">applied_version == last_assignment_version</span>；其余节点比较 <span class="mono">counters["cfg.&lt;domain&gt;"] == 该域当前版本</span>（09 §4.4）。</li>
        <li>失联判据：<span class="mono">shutdown</span> 缺失 + 心跳断 3×5s（10 §5）；控制面只标记、不改指派（09 §7）。</li>
        <li>未收敛 &lt;30s 黄（预期下一心跳恢复），≥30s 红（控制面已重推，计入 <span class="mono">ops.control</span>）。</li>
        <li>状态字段不可手工覆写（§1.3），否则 12 §4.2 的 <span class="mono">row_hash</span> 变更检测与收敛判据会被人手写入破坏。</li>
      </ul>
    </section>
  </div>
</template>

<style scoped>
.sub {
  font-size: 12px;
}

.warn {
  color: var(--el-color-warning);
}

.expand {
  padding: 8px 16px;
}

h4 {
  margin: 0 0 6px;
  font-size: 13px;
}

.code {
  background: #f8f9fb;
  border: 1px solid var(--ponk-border);
  border-radius: 4px;
  padding: 8px 10px;
  font-size: 12px;
  line-height: 1.6;
  overflow: auto;
  max-height: 220px;
  margin: 0 0 8px;
}

.notes {
  margin: 0;
  padding-left: 18px;
  font-size: 13px;
  line-height: 1.9;
  color: var(--ponk-text-2);
}
</style>
