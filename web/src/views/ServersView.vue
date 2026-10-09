<script setup lang="ts">
/**
 * P1 服务器台账（docs/13 §3.3 P1 / §4.4）。
 *
 * - 筛选：status（active / maintenance / retired）、role、q（host 模糊）
 * - 操作：新建、编辑（If-Match）、改状态（PATCH /status）、删除（关联节点 → 409）、查看其上节点 → P2
 * - 派生信息（登记节点数 / 该机运行时实例）由 `GET /servers/{host}/nodes|instances` 按需取（§4.4 末两行）
 * - 语义提示：`node_runtime.host` 与登记 host 关联但不强校验；未匹配的运行时实例标注「未登记」
 */
import { computed, reactive, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useQuery, useQueryClient } from '@tanstack/vue-query'
import { ElMessage, ElMessageBox, type FormInstance } from 'element-plus'
import { Plus, Search, Refresh } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import PagerBar from '../components/PagerBar.vue'
import {
  createServer,
  deleteServer,
  getServer,
  getServerInstances,
  getServerNodes,
  listServers,
  updateServer,
  updateServerStatus,
} from '../api/servers'
import { qk } from '../query/keys'
import { useSessionStore } from '../stores/session'
import { fmtTs } from '../utils/format'
import { enabledChip, instanceStatusChip, serverStatusChip } from '../utils/status'
import type { ServerInput, ServerRecord, ServerStatus } from '../api/types'

const session = useSessionStore()
const router = useRouter()
const qc = useQueryClient()

// ── 列表与筛选 ───────────────────────────────────────────────────────────────
const filters = reactive<{ status: string; role: string; q: string; limit: number; offset: number }>({
  status: '',
  role: '',
  q: '',
  limit: 50,
  offset: 0,
})

const queryParams = computed(() => ({
  status: filters.status || undefined,
  role: filters.role || undefined,
  q: filters.q || undefined,
  limit: filters.limit,
  offset: filters.offset,
}))

const { data, isFetching, refetch, isError: listError, error: listErr } = useQuery({
  queryKey: qk.servers(queryParams.value),
  queryFn: ({ signal }) => listServers(queryParams.value, signal),
  refetchInterval: 30_000,
})

function resetFilters(): void {
  filters.status = ''
  filters.role = ''
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

// ── 新建 / 编辑 ──────────────────────────────────────────────────────────────
/** 预设角色 = 协议 `NodeKind`（`docs/12 §3.5`：这台机器跑哪些节点）。
 *  非节点类角色（如 `web`、`broker`、`nginx`）用 `allow-create` 直接输入即可。 */
const ROLE_PRESETS = [
  'collector',
  'signal',
  'alert',
  'factor',
  'storage',
  'data_svc',
  'control',
] as const

interface LabelRow {
  key: string
  value: string
}

const dialogVisible = ref(false)
const editing = ref<ServerRecord | null>(null)
const formRef = ref<FormInstance>()
const saving = ref(false)

const form = reactive<ServerInput & { roles: string[]; labelRows: LabelRow[] }>({
  host: '',
  region: '',
  roles: [],
  cpu_cores: null,
  mem_gb: null,
  disk_gb: null,
  labelRows: [],
  notes: '',
})

const rules = {
  host: [{ required: true, message: 'host 必填（台账唯一键）', trigger: 'blur' }],
}

function openCreate(): void {
  editing.value = null
  form.host = ''
  form.region = ''
  form.roles = []
  form.cpu_cores = null
  form.mem_gb = null
  form.disk_gb = null
  form.labelRows = []
  form.notes = ''
  dialogVisible.value = true
}

function openEdit(row: ServerRecord): void {
  editing.value = row
  form.host = row.host
  form.region = row.region ?? ''
  form.roles = [...(row.roles ?? [])]
  form.cpu_cores = row.cpu_cores
  form.mem_gb = row.mem_gb
  form.disk_gb = row.disk_gb
  form.labelRows = Object.entries(row.labels ?? {}).map(([key, value]) => ({ key, value }))
  form.notes = row.notes ?? ''
  dialogVisible.value = true
}

function toBody(): ServerInput {
  const labels: Record<string, string> = {}
  for (const r of form.labelRows) {
    if (r.key.trim()) labels[r.key.trim()] = r.value
  }
  return {
    host: form.host.trim(),
    region: form.region?.trim() || null,
    roles: form.roles.map((r) => r.trim()).filter(Boolean),
    cpu_cores: form.cpu_cores ?? null,
    mem_gb: form.mem_gb ?? null,
    disk_gb: form.disk_gb ?? null,
    labels,
    notes: form.notes?.trim() || null,
  }
}

async function submit(): Promise<void> {
  try {
    await formRef.value?.validate()
  } catch {
    return
  }
  saving.value = true
  try {
    const body = toBody()
    if (editing.value) {
      // §5.3：行记录写必须带 If-Match，先取最新 ETag。
      const { etag } = await getServer(editing.value.host)
      await updateServer(editing.value.host, body, etag ?? '')
      ElMessage.success('服务器已更新')
    } else {
      await createServer(body)
      ElMessage.success('服务器已登记')
    }
    dialogVisible.value = false
    await qc.invalidateQueries({ queryKey: ['ponk', 'servers'] })
  } finally {
    saving.value = false
  }
}

// ── 改状态 / 删除 ────────────────────────────────────────────────────────────
async function changeStatus(row: ServerRecord, status: ServerStatus): Promise<void> {
  if (status === row.status) return
  const { etag } = await getServer(row.host)
  await updateServerStatus(row.host, status, etag ?? '')
  ElMessage.success(`状态已改为 ${status}`)
  await qc.invalidateQueries({ queryKey: ['ponk', 'servers'] })
}

async function remove(row: ServerRecord): Promise<void> {
  try {
    await ElMessageBox.confirm(
      `确认删除服务器 ${row.host}？有关联节点时将被拒绝（409）。`,
      '删除服务器',
      { type: 'warning', confirmButtonText: '删除', confirmButtonClass: 'el-button--danger' },
    )
  } catch {
    return
  }
  const { etag } = await getServer(row.host)
  await deleteServer(row.host, etag ?? '')
  ElMessage.success('已删除')
  await qc.invalidateQueries({ queryKey: ['ponk', 'servers'] })
}

// ── 派生信息（详情抽屉） ─────────────────────────────────────────────────────
const detailHost = ref<string | null>(null)
const detailOpen = computed(() => !!detailHost.value)

const { data: detailNodes } = useQuery({
  queryKey: ['ponk', 'serverDetail', 'nodes', detailHost],
  queryFn: ({ signal }) => getServerNodes(detailHost.value ?? '', { limit: 100 }, signal),
  enabled: computed(() => !!detailHost.value),
  retry: false,
})

const { data: detailInstances } = useQuery({
  queryKey: ['ponk', 'serverDetail', 'instances', detailHost],
  queryFn: ({ signal }) => getServerInstances(detailHost.value ?? '', signal),
  enabled: computed(() => !!detailHost.value),
  retry: false,
})

function openDetail(host: string): void {
  detailHost.value = host
}

function viewNodes(row: ServerRecord): void {
  void router.push({ path: '/nodes', query: { server_id: String(row.id), server_host: row.host } })
}
</script>

<template>
  <div class="page">
    <PageHeader title="服务器台账" page="P1" desc="人工登记的机器台账（12 §3.5）。register 上报的 host 与之关联但不强校验——未登记只提示、不拒绝注册。">
      <template #actions>
        <el-button :icon="Refresh" @click="() => refetch()">刷新</el-button>
        <el-button v-if="session.canWrite" type="primary" :icon="Plus" @click="openCreate">新建服务器</el-button>
      </template>
    </PageHeader>

    <el-alert
      v-if="listError"
      type="error"
      show-icon
      :closable="false"
      style="margin-bottom: 12px"
      title="服务器台账加载失败"
      :description="(listErr as Error)?.message ?? '请求失败'"
    />

    <div class="filters">
      <el-select v-model="filters.status" placeholder="全部状态" clearable style="width: 150px" @change="() => refetch()">
        <el-option label="active 在用" value="active" />
        <el-option label="maintenance 维护中" value="maintenance" />
        <el-option label="retired 已退役" value="retired" />
      </el-select>
      <el-input v-model="filters.role" placeholder="按角色筛选，如 collector" clearable style="width: 180px" @change="() => refetch()" />
      <el-input
        v-model="filters.q"
        placeholder="host 关键字"
        clearable
        :prefix-icon="Search"
        style="width: 220px"
        @change="() => refetch()"
        @clear="resetFilters"
      />
      <el-button @click="resetFilters">重置</el-button>
      <span class="muted">共 {{ total }} 条</span>
    </div>

    <div class="panel">
      <el-table :data="rows" v-loading="isFetching" size="small" stripe>
        <el-table-column label="host" min-width="170" fixed="left">
          <template #default="{ row }">
            <a class="mono" href="javascript:;" @click="openDetail(row.host)">{{ row.host }}</a>
          </template>
        </el-table-column>
        <el-table-column label="region" width="110">
          <template #default="{ row }"><span :class="{ muted: !row.region }">{{ row.region || '—' }}</span></template>
        </el-table-column>
        <el-table-column label="roles" min-width="140">
          <template #default="{ row }">
            <div class="flex-row">
              <el-tag v-for="r in row.roles ?? []" :key="r" size="small" effect="plain">{{ r }}</el-tag>
              <span v-if="!row.roles?.length" class="muted">—</span>
            </div>
          </template>
        </el-table-column>
        <el-table-column label="CPU / 内存 / 磁盘" width="170">
          <template #default="{ row }">
            <span class="mono">{{ row.cpu_cores ?? '—' }}C / {{ row.mem_gb ?? '—' }}G / {{ row.disk_gb ?? '—' }}G</span>
          </template>
        </el-table-column>
        <el-table-column label="状态" width="120">
          <template #default="{ row }">
            <el-select
              v-if="session.canWrite"
              :model-value="row.status"
              size="small"
              @change="(v: string) => changeStatus(row, v as ServerStatus)"
            >
              <el-option label="active" value="active" />
              <el-option label="maintenance" value="maintenance" />
              <el-option label="retired" value="retired" />
            </el-select>
            <StatusTag v-else :chip="serverStatusChip(row.status)" />
          </template>
        </el-table-column>
        <el-table-column label="labels" min-width="150">
          <template #default="{ row }">
            <div class="flex-row">
              <el-tag v-for="(v, k) in row.labels ?? {}" :key="k" size="small" type="info" effect="plain">
                {{ k }}={{ v }}
              </el-tag>
              <span v-if="!Object.keys(row.labels ?? {}).length" class="muted">—</span>
            </div>
          </template>
        </el-table-column>
        <el-table-column label="登记节点 / 实例" width="150">
          <template #default="{ row }">
            <el-button link type="primary" size="small" @click="openDetail(row.host)">查看</el-button>
          </template>
        </el-table-column>
        <el-table-column label="notes" min-width="140">
          <template #default="{ row }"><span class="muted">{{ row.notes || '—' }}</span></template>
        </el-table-column>
        <el-table-column label="更新时间" width="150">
          <template #default="{ row }">{{ fmtTs(row.updated_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="170" fixed="right">
          <template #default="{ row }">
            <el-button v-if="session.canWrite" link type="primary" size="small" @click="openEdit(row)">编辑</el-button>
            <el-button link type="primary" size="small" @click="viewNodes(row)">其上节点</el-button>
            <el-button v-if="session.canWrite" link type="danger" size="small" @click="remove(row)">删除</el-button>
          </template>
        </el-table-column>
      </el-table>

      <div class="table-footer">
        <PagerBar :total="total" :limit="filters.limit" :offset="filters.offset" @change="onPage" />
      </div>
    </div>

    <!-- 新建 / 编辑 -->
    <el-dialog
      v-model="dialogVisible"
      :title="editing ? `编辑 ${editing.host}` : '新建服务器'"
      width="640px"
      destroy-on-close
    >
      <el-form ref="formRef" :model="form" :rules="rules" label-width="110px">
        <el-form-item label="host" prop="host">
          <el-input v-model="form.host" placeholder="10.0.0.1 或主机名，台账唯一" :disabled="!!editing" />
        </el-form-item>
        <el-form-item label="region">
          <el-input v-model="form.region" placeholder="如 cn-east-1" />
        </el-form-item>
        <el-form-item label="roles">
          <el-select
            v-model="form.roles"
            multiple
            filterable
            allow-create
            default-first-option
            placeholder="选择角色（可多选，也可直接输入新值）"
            class="roles-select"
          >
            <el-option v-for="r in ROLE_PRESETS" :key="r" :label="r" :value="r" />
          </el-select>
          <div class="hint">
            预设 = 协议 <code>NodeKind</code>（docs/12 §3.5）；非节点类角色（web / broker 等）可直接输入。
          </div>
        </el-form-item>
        <el-form-item label="CPU 核数">
          <el-input-number v-model="form.cpu_cores" :min="0" :max="1024" />
        </el-form-item>
        <el-form-item label="内存 GB">
          <el-input-number v-model="form.mem_gb" :min="0" :max="4096" />
        </el-form-item>
        <el-form-item label="磁盘 GB">
          <el-input-number v-model="form.disk_gb" :min="0" :max="102400" />
        </el-form-item>
        <el-form-item label="labels">
          <div style="width: 100%">
            <div v-for="(r, i) in form.labelRows" :key="i" class="flex-row" style="margin-bottom: 6px">
              <el-input v-model="r.key" placeholder="key" style="width: 40%" />
              <el-input v-model="r.value" placeholder="value" style="width: 40%" />
              <el-button link type="danger" @click="form.labelRows.splice(i, 1)">移除</el-button>
            </div>
            <el-button size="small" @click="form.labelRows.push({ key: '', value: '' })">添加标签</el-button>
          </div>
        </el-form-item>
        <el-form-item label="notes">
          <el-input v-model="form.notes" type="textarea" :rows="2" placeholder="说明文字（§5.3：纯注释字段可降级为 last-write-wins）" />
        </el-form-item>
        <el-alert v-if="editing" type="info" :closable="false" show-icon
          title="保存将带 If-Match（当前行的 ETag）；他人已修改会返回 412，需刷新后重试（§5.3）。" />
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="submit">{{ editing ? '保存' : '登记' }}</el-button>
      </template>
    </el-dialog>

    <!-- 派生信息 -->
    <el-drawer v-model="detailOpen" :title="`服务器 ${detailHost}`" size="520px">
      <h4>登记节点（期望，GET /servers/{host}/nodes）</h4>
      <el-table v-if="detailNodes?.items?.length" :data="detailNodes.items" size="small">
        <el-table-column label="name" min-width="140">
          <template #default="{ row }">
            <RouterLink :to="`/nodes/${encodeURIComponent(row.name)}`">{{ row.name }}</RouterLink>
          </template>
        </el-table-column>
        <el-table-column label="enabled" width="90">
          <template #default="{ row }">
            <StatusTag :chip="enabledChip(row.enabled)" />
          </template>
        </el-table-column>
      </el-table>
      <el-empty v-else description="该机无登记节点" :image-size="60" />

      <h4 style="margin-top: 18px">运行时实例（GET /servers/{host}/instances）</h4>
      <el-table v-if="detailInstances?.length" :data="detailInstances" size="small">
        <el-table-column label="instance_id" min-width="140" prop="instance_id" />
        <el-table-column label="status" width="110">
          <template #default="{ row }">
            <StatusTag :chip="instanceStatusChip(row.status)" />
          </template>
        </el-table-column>
        <el-table-column label="上报 host" min-width="140">
          <template #default="{ row }">
            <span class="mono">{{ row.host ?? '—' }}</span>
          </template>
        </el-table-column>
      </el-table>
      <el-empty v-else description="该机无运行时实例" :image-size="60" />
      <el-alert type="warning" :closable="false" show-icon style="margin-top: 12px"
        title="登记 host 与实际上报 host 关联但不强校验；不匹配的运行时实例标注为「未登记」（12 §3.5）。" />
    </el-drawer>
  </div>
</template>

<style scoped>
h4 {
  margin: 0 0 8px;
  font-size: 13px;
}
</style>
