<script setup lang="ts">
/**
 * P2 采集节点列表（docs/13 §3.3 P2 / §4.5）。
 *
 * 展示：name / enabled / 期望机器 / 实际机器 / 范围摘要 / 例外数 / 运行状态 /
 *       `applied_version` vs `last_assignment_version` / `last_seen_ts`
 * 操作：新建、编辑、启停、删除确认、范围预览、导出 TOML（后两者在节点详情页）
 * 轮询：5s（§5.6 运行时页），列表 30s。
 */
import { computed, reactive, ref, onMounted } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useQuery, useQueryClient } from '@tanstack/vue-query'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Plus, Refresh, Search, Download } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import PagerBar from '../components/PagerBar.vue'
import NodeFormDialog from '../components/NodeFormDialog.vue'
import { deleteNode, downloadNodeToml, getNode, listNodes, setNodeEnabled } from '../api/nodes'
import { listServers } from '../api/servers'
import { qk } from '../query/keys'
import { useSessionStore } from '../stores/session'
import { fmtAgo, fmtTs } from '../utils/format'
import { enabledChip, instanceStatusChip, UNREGISTERED_HOST_CHIP } from '../utils/status'
import type { NodeListQuery, NodeRow } from '../api/types'

const session = useSessionStore()
const route = useRoute()
const router = useRouter()
const qc = useQueryClient()

const filters = reactive<{
  enabled: string
  server_id: string
  status: string
  converged: string
  host: string
  q: string
  limit: number
  offset: number
}>({
  enabled: '',
  server_id: '',
  status: '',
  converged: '',
  host: '',
  q: '',
  limit: 50,
  offset: 0,
})

onMounted(() => {
  // 支持 P1「其上节点」跳转带入筛选（§3.5 跨页跳转）
  const q = route.query
  if (q.server_id) filters.server_id = String(q.server_id)
  if (q.host) filters.host = String(q.host)
  if (q.q) filters.q = String(q.q)
  if (q.converged) filters.converged = String(q.converged)
  if (q.status) filters.status = String(q.status)
})

const queryParams = computed<NodeListQuery>(() => ({
  enabled: filters.enabled === '' ? undefined : filters.enabled === 'true',
  server_id: filters.server_id ? Number(filters.server_id) : undefined,
  status: filters.status || undefined,
  converged: filters.converged === '' ? undefined : filters.converged === 'true',
  host: filters.host || undefined,
  q: filters.q || undefined,
  limit: filters.limit,
  offset: filters.offset,
}))

const { data, isFetching, refetch, isError: listError, error: listErr } = useQuery({
  queryKey: qk.nodes(queryParams.value),
  queryFn: ({ signal }) => listNodes(queryParams.value, signal),
  refetchInterval: 30_000,
})

function applyFilters(): void {
  filters.offset = 0
  void refetch()
}

function resetFilters(): void {
  filters.enabled = ''
  filters.server_id = ''
  filters.status = ''
  filters.converged = ''
  filters.host = ''
  filters.q = ''
  filters.offset = 0
  void refetch()
}

function onPage(next: { limit: number; offset: number }): void {
  filters.limit = next.limit
  filters.offset = next.offset
  void refetch()
}

const rows = computed(() => data.value?.items ?? [])
const total = computed(() => data.value?.total ?? rows.value.length)

const { data: servers } = useQuery({
  queryKey: qk.servers({ limit: 500, offset: 0 }),
  queryFn: ({ signal }) => listServers({ limit: 500, offset: 0 }, signal),
  staleTime: 60_000,
  retry: false,
})

/** 期望机器 host（`server_id` → `server.host`）。 */
function serverHost(serverId?: number | null): string {
  if (!serverId) return '—'
  const s = servers.value?.items.find((x) => x.id === serverId)
  return s ? s.host : `#${serverId}`
}

/** 范围摘要（§3.3 P2 列表字段）。 */
function scopeSummary(row: NodeRow): string {
  const s = row.scope
  if (!s) return '—'
  const parts: string[] = []
  parts.push(s.venues?.length ? s.venues.join('/') : '全交易所')
  parts.push(s.markets?.length ? s.markets.join('/') : '全市场')
  parts.push(s.channels?.length ? `${s.channels.length} 通道` : '无通道')
  if (s.quote?.length) parts.push(s.quote.join('/'))
  if (s.status?.length) parts.push(s.status.join('/'))
  parts.push(`shard ${s.shard_size ?? '—'}`)
  return parts.join(' · ')
}

/**
 * 未登记机器（§3.4 黄标）：实际上报 host 有值，但不在 `server` 台账中。
 * 只告警，不拒绝注册（12 §3.5）。
 */
function isUnregistered(row: NodeRow): boolean {
  const host = row.runtime_host ?? row.runtime?.host ?? null
  if (!host || !servers.value) return false
  return !servers.value.items.some((s) => s.host === host)
}

// ── 启停 / 删除 / 导出 ───────────────────────────────────────────────────────
async function toggleEnabled(row: NodeRow): Promise<void> {
  const { etag } = await getNode(row.name)
  await setNodeEnabled(row.name, !row.enabled, etag ?? '')
  ElMessage.success(row.enabled ? `节点 ${row.name} 已停用` : `节点 ${row.name} 已启用`)
  await qc.invalidateQueries({ queryKey: ['ponk', 'nodes'] })
}

async function remove(row: NodeRow): Promise<void> {
  try {
    await ElMessageBox.confirm(
      `确认删除节点 ${row.name}？将连带删除派生计划，运行时行保留（§3.3 P2）。`,
      '删除节点',
      { type: 'warning', confirmButtonText: '删除', confirmButtonClass: 'el-button--danger' },
    )
  } catch {
    return
  }
  const { etag } = await getNode(row.name)
  await deleteNode(row.name, etag ?? '')
  ElMessage.success('节点已删除')
  await qc.invalidateQueries({ queryKey: ['ponk', 'nodes'] })
}

async function exportToml(row: NodeRow): Promise<void> {
  await downloadNodeToml(row.name)
  ElMessage.success(`已导出 ${row.name}.toml（§5.4：控制面只生成内容，不远程部署）`)
}

function openDetail(row: NodeRow): void {
  void router.push(`/nodes/${encodeURIComponent(row.name)}`)
}

// ── 新建 / 编辑 ──────────────────────────────────────────────────────────────
const dialogVisible = ref(false)
const editingNode = ref<NodeRow | null>(null)

function openCreate(): void {
  editingNode.value = null
  dialogVisible.value = true
}

function openEdit(row: NodeRow): void {
  editingNode.value = row
  dialogVisible.value = true
}
</script>

<template>
  <div class="page">
    <PageHeader
      title="采集节点"
      page="P2"
      desc="节点定义 = 操作者输入；订阅计划是它的派生产物（09 §2.3）。名称即 instance_id。"
    >
      <template #actions>
        <el-button :icon="Refresh" @click="() => refetch()">刷新</el-button>
        <el-button v-if="session.canWrite" type="primary" :icon="Plus" @click="openCreate">新建节点</el-button>
      </template>
    </PageHeader>

    <el-alert
      v-if="listError"
      type="error"
      show-icon
      :closable="false"
      style="margin-bottom: 12px"
      title="节点列表加载失败"
      :description="(listErr as Error)?.message ?? '请求失败'"
    />

    <div class="filters">
      <el-select v-model="filters.enabled" placeholder="启用状态" clearable style="width: 130px" @change="applyFilters">
        <el-option label="启用" value="true" />
        <el-option label="停用" value="false" />
      </el-select>
      <el-select v-model="filters.server_id" placeholder="期望机器" clearable filterable style="width: 190px" @change="applyFilters">
        <el-option v-for="s in servers?.items ?? []" :key="s.id" :label="s.host" :value="String(s.id)" />
      </el-select>
      <el-select v-model="filters.status" placeholder="运行状态" clearable style="width: 140px" @change="applyFilters">
        <el-option label="registered" value="registered" />
        <el-option label="running" value="running" />
        <el-option label="stopped" value="stopped" />
        <el-option label="dead" value="dead" />
      </el-select>
      <el-select v-model="filters.converged" placeholder="收敛" clearable style="width: 130px" @change="applyFilters">
        <el-option label="已收敛" value="true" />
        <el-option label="未收敛" value="false" />
      </el-select>
      <el-input v-model="filters.host" placeholder="实际上报 host" clearable style="width: 170px" @change="applyFilters" />
      <el-input
        v-model="filters.q"
        placeholder="节点名关键字"
        clearable
        :prefix-icon="Search"
        style="width: 200px"
        @change="applyFilters"
        @clear="resetFilters"
      />
      <el-button @click="resetFilters">重置</el-button>
      <span class="muted">共 {{ total }} 条</span>
    </div>

    <div class="panel">
      <el-table :data="rows" v-loading="isFetching" size="small" stripe>
        <el-table-column label="name" min-width="160" fixed="left">
          <template #default="{ row }">
            <a class="mono" href="javascript:;" @click="openDetail(row)">{{ row.name }}</a>
            <div class="muted sub">server_id {{ row.server_id ?? '—' }}</div>
          </template>
        </el-table-column>

        <el-table-column label="enabled" width="86">
          <template #default="{ row }">
            <StatusTag :chip="enabledChip(row.enabled)" />
          </template>
        </el-table-column>

        <el-table-column label="期望机器" min-width="130">
          <template #default="{ row }">
            <span class="mono">{{ serverHost(row.server_id) }}</span>
          </template>
        </el-table-column>

        <el-table-column label="实际机器" min-width="150">
          <template #default="{ row }">
            <div class="flex-row">
              <span class="mono">{{ row.runtime_host ?? row.runtime?.host ?? '—' }}</span>
              <StatusTag v-if="isUnregistered(row)" :chip="UNREGISTERED_HOST_CHIP" />
            </div>
          </template>
        </el-table-column>

        <el-table-column label="范围摘要" min-width="260">
          <template #default="{ row }">
            <span class="muted">{{ scopeSummary(row) }}</span>
            <div class="muted sub">
              例外 include {{ row.scope_include?.length ?? 0 }} / exclude {{ row.scope_exclude?.length ?? 0 }}
            </div>
          </template>
        </el-table-column>

        <el-table-column label="运行状态" width="110">
          <template #default="{ row }">
            <StatusTag :chip="instanceStatusChip(row.runtime?.status ?? null)" />
          </template>
        </el-table-column>

        <el-table-column label="applied / assignment" width="150">
          <template #default="{ row }">
            <span class="mono">
              {{ row.runtime?.applied_version ?? '—' }} / {{ row.runtime?.last_assignment_version ?? '—' }}
            </span>
          </template>
        </el-table-column>

        <el-table-column label="last_seen" width="120">
          <template #default="{ row }">
            <span :class="{ muted: !row.runtime?.last_seen_ts }">
              {{ row.runtime?.last_seen_ts ? fmtAgo(row.runtime.last_seen_ts) : '—' }}
            </span>
          </template>
        </el-table-column>

        <el-table-column label="updated_at" width="150">
          <template #default="{ row }">{{ fmtTs(row.updated_at) }}</template>
        </el-table-column>

        <el-table-column label="操作" width="240" fixed="right">
          <template #default="{ row }">
            <el-button link type="primary" size="small" @click="openDetail(row)">详情 / 预览</el-button>
            <el-button v-if="session.canWrite" link type="primary" size="small" @click="openEdit(row)">编辑</el-button>
            <el-button v-if="session.canWrite" link type="warning" size="small" @click="toggleEnabled(row)">
              {{ row.enabled ? '停用' : '启用' }}
            </el-button>
            <el-button link type="primary" size="small" :icon="Download" @click="exportToml(row)">TOML</el-button>
            <el-button v-if="session.canWrite" link type="danger" size="small" @click="remove(row)">删除</el-button>
          </template>
        </el-table-column>
      </el-table>

      <div class="table-footer">
        <PagerBar :total="total" :limit="filters.limit" :offset="filters.offset" @change="onPage" />
      </div>
    </div>

    <NodeFormDialog v-model:visible="dialogVisible" :node="editingNode" />
  </div>
</template>

<style scoped>
.sub {
  font-size: 12px;
}
</style>
