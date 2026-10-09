<script setup lang="ts">
/**
 * P3 交易所与市场（docs/14 §6 / docs/13 §3.3 P3 / §4.6）。
 *
 * - **两步创建**（方案 A）：①「新建交易所」→ ② 展开行里逐个建市场
 * - **列表不显示"平均配额"**（无物理意义）：显示**市场数**，配额在展开区按市场逐条列出
 * - 编辑带 `If-Match`（§5.3：412 / 428）；启停走 `PATCH .../enabled`
 * - 删除有关联市场时 `?confirm=<venue>`（后端 `400 + detail.confirm`，docs/14 §6）
 * - 形态：**台账用展开行**，不用抽屉（docs/14 §7.1）
 */
import { computed, reactive, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useQuery, useQueryClient } from '@tanstack/vue-query'
import { ElMessage, ElMessageBox, type FormInstance } from 'element-plus'
import { Plus, Refresh, Search } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import PagerBar from '../components/PagerBar.vue'
import CatalogSyncBar from '../components/CatalogSyncBar.vue'
import CatalogEmptyState from '../components/CatalogEmptyState.vue'
import ExchangeMarketsPanel from '../components/ExchangeMarketsPanel.vue'
import { createExchange, deleteExchange, listExchanges, setExchangeEnabled, updateExchange } from '../api/catalog'
import { ApiError } from '../api/http'
import { qk } from '../query/keys'
import { useCatalogSync } from '../query/useCatalogSync'
import { useSessionStore } from '../stores/session'
import { fmtTs, prettyJson } from '../utils/format'
import { venueEnabledChip } from '../utils/status'
import type { ExchangeInput, ExchangeRecord } from '../api/types'

const session = useSessionStore()
const route = useRoute()
const router = useRouter()
const qc = useQueryClient()

const sync = useCatalogSync('catalog')

// ── 列表与筛选 ───────────────────────────────────────────────────────────────
const filters = reactive<{ enabled: string; q: string; limit: number; offset: number }>({
  enabled: '',
  q: '',
  limit: 50,
  offset: 0,
})

// 深链（docs/13 §3.5：P10 分片 / P4 交易对 → P3）：`?venue=binance` 预筛关键字。
const fromRoute = String(route.query.venue ?? route.query.q ?? '')
if (fromRoute) filters.q = fromRoute

const queryParams = computed(() => ({
  enabled: filters.enabled === '' ? undefined : filters.enabled === 'true',
  q: filters.q || undefined,
  limit: filters.limit,
  offset: filters.offset,
}))

const { data, isFetching, refetch, isError: listError, error: listErr } = useQuery({
  queryKey: qk.exchanges(queryParams),
  queryFn: ({ signal }) => listExchanges(queryParams.value, signal),
  refetchInterval: 60_000,
})

const rows = computed(() => data.value?.items ?? [])
const total = computed(() => data.value?.total ?? rows.value.length)
const loading = computed(() => isFetching.value && !data.value)
const isEmpty = computed(() => !loading.value && total.value === 0)
const hasFilters = computed(() => !!(filters.q || filters.enabled))

/** 展开行（台账 + 市场）。row-key 保证刷新后展开状态不丢。 */
const expanded = ref<string[]>([])

function applyFilters(): void {
  filters.offset = 0
  void refetch()
}

function resetFilters(): void {
  filters.enabled = ''
  filters.q = ''
  filters.offset = 0
  void refetch()
}

function onPage(next: { limit: number; offset: number }): void {
  filters.limit = next.limit
  filters.offset = next.offset
  void refetch()
}

/** 展开某所的市场台账（单开，避免堆叠多行）。 */
function expandOnly(venue: string): void {
  expanded.value = [venue]
}

async function onSync(): Promise<void> {
  await sync.trigger()
  void refetch()
}

// ── 新建 / 编辑交易所 ────────────────────────────────────────────────────────
const dialogVisible = ref(false)
const editing = ref<ExchangeRecord | null>(null)
const formRef = ref<FormInstance>()
const saving = ref(false)

const form = reactive<ExchangeInput>({
  venue: '',
  name: '',
  dig3_exchange_id: '',
  enabled: true,
  asset_class: 'crypto',
  extra: {},
})
/** `extra{}` 的 JSON 文本（展示字段，docs/13 §3.3 P3）。 */
const extraText = ref('{}')

const rules = {
  venue: [{ required: true, message: 'venue 必填（库内主键）', trigger: 'blur' }],
  name: [{ required: true, message: 'name 必填（12 §3.1 NOT NULL）', trigger: 'blur' }],
  dig3_exchange_id: [
    { required: true, message: 'dig3_exchange_id 必填（必须能映射到 dig3 ExchangeId，否则采集起不来）', trigger: 'blur' },
  ],
}

function openCreate(): void {
  editing.value = null
  form.venue = ''
  form.name = ''
  form.dig3_exchange_id = ''
  form.enabled = true
  form.asset_class = 'crypto'
  extraText.value = '{}'
  dialogVisible.value = true
}

function openEdit(row: ExchangeRecord): void {
  editing.value = row
  form.venue = row.venue
  form.name = row.name ?? ''
  form.dig3_exchange_id = row.dig3_exchange_id ?? ''
  form.enabled = row.enabled ?? true
  form.asset_class = row.asset_class ?? 'crypto'
  extraText.value = prettyJson(row.extra ?? {})
  dialogVisible.value = true
}

function toBody(): ExchangeInput | null {
  let extra: Record<string, unknown> = {}
  const raw = extraText.value.trim()
  if (raw) {
    try {
      const parsed: unknown = JSON.parse(raw)
      if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) extra = parsed as Record<string, unknown>
      else {
        ElMessage.error('extra 必须是 JSON 对象')
        return null
      }
    } catch {
      ElMessage.error('extra 不是合法 JSON')
      return null
    }
  }
  return {
    venue: form.venue.trim(),
    name: (form.name ?? '').trim(),
    dig3_exchange_id: (form.dig3_exchange_id ?? '').trim(),
    enabled: form.enabled ?? true,
    asset_class: form.asset_class || 'crypto',
    extra,
  }
}

async function submit(): Promise<void> {
  try {
    await formRef.value?.validate()
  } catch {
    return
  }
  const body = toBody()
  if (!body) return
  saving.value = true
  try {
    if (editing.value) {
      // §5.3：行记录写必须带 If-Match（列表行由后端 with_etag 给出 etag）
      const updated = await updateExchange(editing.value.venue, body, editing.value.etag ?? '*')
      ElMessage.success(`交易所 ${updated.venue} 已更新`)
    } else {
      const created = await createExchange(body)
      ElMessage.success(`交易所 ${created.venue} 已创建 —— 第二步：在展开区创建市场（docs/14 §6 两步创建）`)
      expanded.value = [created.venue]
    }
    dialogVisible.value = false
    await qc.invalidateQueries({ queryKey: ['ponk', 'exchanges'] })
  } finally {
    saving.value = false
  }
}

// ── 启停 / 删除 ──────────────────────────────────────────────────────────────
async function toggleEnabled(row: ExchangeRecord): Promise<void> {
  await setExchangeEnabled(row.venue, !(row.enabled ?? true), row.etag ?? '*')
  ElMessage.success(`交易所 ${row.venue} 已${row.enabled ? '停用' : '启用'}`)
  await qc.invalidateQueries({ queryKey: ['ponk', 'exchanges'] })
}

async function remove(row: ExchangeRecord): Promise<void> {
  const markets = row.market_count ?? 0
  try {
    await ElMessageBox.confirm(
      markets > 0
        ? `确认删除交易所 ${row.venue}？该所下有 ${markets} 个市场及关联交易对会被**级联删除**（docs/14 §6：需 ?confirm=<venue>）。`
        : `确认删除交易所 ${row.venue}？`,
      '删除交易所',
      { type: 'warning', confirmButtonText: '删除', confirmButtonClass: 'el-button--danger' },
    )
  } catch {
    return
  }

  const ifMatch = row.etag ?? '*'
  try {
    // 有关联市场 → 直接带 ?confirm=<venue>
    await deleteExchange(row.venue, ifMatch, markets > 0 ? row.venue : undefined)
  } catch (e) {
    // 后端仍要求二次确认（如期间新增了市场）：400 + detail.confirm（docs/14 §6）
    const confirm = e instanceof ApiError ? (e.detail?.confirm as string | undefined) : undefined
    if (!(e instanceof ApiError) || e.status !== 400 || !confirm) throw e
    try {
      await ElMessageBox.confirm(
        `该交易所下仍有市场会被级联删除，请再次确认（?confirm=${confirm}）。`,
        '二次确认',
        { type: 'warning', confirmButtonText: '确认删除', confirmButtonClass: 'el-button--danger' },
      )
    } catch {
      return
    }
    await deleteExchange(row.venue, ifMatch, confirm)
  }
  ElMessage.success(`交易所 ${row.venue} 已删除`)
  await qc.invalidateQueries({ queryKey: ['ponk', 'exchanges'] })
}

function viewInstruments(row: ExchangeRecord): void {
  void router.push({ path: '/catalog/instruments', query: { venue: row.venue } })
}

function parseExtra(v?: Record<string, unknown> | null): string {
  return v && Object.keys(v).length ? prettyJson(v) : ''
}
</script>

<template>
  <div class="page">
    <PageHeader
      title="交易所与市场"
      page="P3"
      desc="人工台账（docs/14 §2）：写入方是本页，不是同步任务。配额与出口 IP 每市场独立 —— 市场在展开区逐条列出，列表只显示市场数（docs/14 §6）。"
    >
      <template #actions>
        <el-button :icon="Refresh" @click="() => refetch()">刷新</el-button>
        <el-button v-if="session.canWrite" type="primary" :icon="Plus" @click="openCreate">新建交易所</el-button>
      </template>
    </PageHeader>

    <CatalogSyncBar
      v-if="!isEmpty"
      :state="sync"
      @sync="onSync"
    />

    <el-alert
      v-if="listError"
      type="error"
      show-icon
      :closable="false"
      style="margin-bottom: 12px"
      title="交易所列表加载失败"
      :description="(listErr as Error)?.message ?? '请求失败'"
    />

    <div class="filters">
      <el-select v-model="filters.enabled" placeholder="启用状态" clearable style="width: 130px" @change="applyFilters">
        <el-option label="启用" value="true" />
        <el-option label="停用" value="false" />
      </el-select>
      <el-input
        v-model="filters.q"
        placeholder="venue / name / dig3_exchange_id 关键字"
        clearable
        :prefix-icon="Search"
        style="width: 280px"
        @change="applyFilters"
        @clear="resetFilters"
      />
      <el-button @click="resetFilters">重置</el-button>
      <span class="muted">共 {{ total }} 条</span>
    </div>

    <!-- §7.4 空态 -->
    <CatalogEmptyState
      v-if="isEmpty"
      :state="sync"
      interval-text="交易对目录每 5 分钟自动同步一次"
      title="尚无数据 —— 交易所与市场是人工台账（docs/14 §2），需本页登记"
      @sync="onSync"
    >
      <template #extra>
        <el-button v-if="session.canWrite" type="primary" :icon="Plus" @click="openCreate">新建交易所（第一步）</el-button>
        <el-button @click="resetFilters" v-if="hasFilters">清除筛选</el-button>
      </template>
    </CatalogEmptyState>

    <template v-else>
      <div class="panel">
        <el-table
          :data="rows"
          v-loading="isFetching"
          size="small"
          stripe
          row-key="venue"
          :expand-row-keys="expanded"
        >
          <el-table-column type="expand">
            <template #default="{ row }">
              <ExchangeMarketsPanel :venue="row.venue" />
            </template>
          </el-table-column>

          <el-table-column label="venue" min-width="130" fixed="left">
            <template #default="{ row }"><span class="mono strong">{{ row.venue }}</span></template>
          </el-table-column>
          <el-table-column label="name" min-width="150">
            <template #default="{ row }"><span :class="{ muted: !row.name }">{{ row.name || '—' }}</span></template>
          </el-table-column>
          <el-table-column label="dig3_exchange_id" width="150">
            <template #default="{ row }"><span class="mono">{{ row.dig3_exchange_id || '—' }}</span></template>
          </el-table-column>
          <el-table-column label="asset_class" width="110">
            <template #default="{ row }">
              <el-tag size="small" effect="plain" :type="row.asset_class === 'stock' ? 'warning' : 'info'">
                {{ row.asset_class || 'crypto' }}
              </el-tag>
            </template>
          </el-table-column>
          <el-table-column label="enabled" width="86">
            <template #default="{ row }"><StatusTag :chip="venueEnabledChip(row.enabled)" /></template>
          </el-table-column>
          <el-table-column label="市场数" width="90" prop="market_count">
            <template #default="{ row }">
              <el-button link type="primary" size="small" @click="expandOnly(row.venue)">
                {{ row.market_count ?? 0 }}
              </el-button>
            </template>
          </el-table-column>
          <el-table-column label="extra{}" min-width="150">
            <template #default="{ row }">
              <span class="mono muted sub">{{ parseExtra(row.extra) || '—' }}</span>
            </template>
          </el-table-column>
          <el-table-column label="created_at" width="145">
            <template #default="{ row }">{{ fmtTs(row.created_at) }}</template>
          </el-table-column>
          <el-table-column label="updated_at" width="145">
            <template #default="{ row }">{{ fmtTs(row.updated_at) }}</template>
          </el-table-column>
          <el-table-column label="操作" width="230" fixed="right">
            <template #default="{ row }">
              <el-button link type="primary" size="small" @click="expandOnly(row.venue)">市场</el-button>
              <el-button link type="primary" size="small" @click="viewInstruments(row)">交易对</el-button>
              <el-button v-if="session.canWrite" link type="primary" size="small" @click="openEdit(row)">编辑</el-button>
              <el-button v-if="session.canWrite" link type="warning" size="small" @click="toggleEnabled(row)">
                {{ row.enabled ? '停用' : '启用' }}
              </el-button>
              <el-button v-if="session.canWrite" link type="danger" size="small" @click="remove(row)">删除</el-button>
            </template>
          </el-table-column>
        </el-table>

        <div class="table-footer">
          <PagerBar :total="total" :limit="filters.limit" :offset="filters.offset" @change="onPage" />
        </div>
      </div>

      <p class="muted note">
        展开行内可新增 / 编辑 / 启停市场，配额三项（采集 / 回填 / 元数据）按市场独立填写（02 §6：0.50 / 0.35 / 0.15）。
        <code>dig3_account_type</code> 无默认值，必须显式填（12 §3.1：写错连不上）。
      </p>
    </template>

    <!-- 新建 / 编辑交易所（两步创建的第一步） -->
    <el-dialog
      v-model="dialogVisible"
      :title="editing ? `编辑 ${editing.venue}` : '新建交易所（第一步）'"
      width="600px"
      destroy-on-close
    >
      <el-form ref="formRef" :model="form" :rules="rules" label-width="150px">
        <el-form-item label="venue" prop="venue">
          <el-input v-model="form.venue" placeholder="binance / okx / bitget / hyperliquid（库内主键）" :disabled="!!editing" />
        </el-form-item>
        <el-form-item label="name" prop="name">
          <el-input v-model="form.name" placeholder="展示名" />
        </el-form-item>
        <el-form-item label="dig3_exchange_id" prop="dig3_exchange_id">
          <el-input v-model="form.dig3_exchange_id" placeholder="必须能映射到 dig3 ExchangeId，否则采集起不来" />
        </el-form-item>
        <el-form-item label="enabled">
          <el-switch v-model="form.enabled" />
          <span class="hint">停用的所停止同步，省配额与存储（docs/14 §2）</span>
        </el-form-item>
        <el-form-item label="asset_class">
          <el-select v-model="form.asset_class" style="width: 220px">
            <el-option label="crypto" value="crypto" />
            <el-option label="stock" value="stock" />
            <el-option label="forex" value="forex" />
            <el-option label="prediction" value="prediction" />
          </el-select>
          <span class="hint">12 §3.1；<code>stock</code> → 该所交易对跳过链上币信息（docs/14 §6）。</span>
        </el-form-item>
        <el-form-item label="extra{}">
          <el-input v-model="extraText" type="textarea" :rows="4" class="mono" placeholder='{"…": "所方开放字段，JSON 对象"}' />
        </el-form-item>

        <el-alert
          v-if="editing"
          type="info"
          :closable="false"
          show-icon
          title="保存将带 If-Match（该行 ETag）；他人已修改会返回 412，需刷新后重试（docs/13 §5.3）。"
        />
        <el-alert
          v-else
          type="info"
          :closable="false"
          show-icon
          style="margin-top: 8px"
          title="创建后在列表展开区做第二步：逐个创建市场（market / dig3_account_type / 配额三项 / egress_ip）。"
        />
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="submit">{{ editing ? '保存' : '创建交易所' }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.sub {
  font-size: 11px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 260px;
}

.strong {
  font-weight: 600;
}

.hint {
  color: var(--ponk-text-3);
  font-size: 12px;
  margin-left: 8px;
}

.note {
  font-size: 12px;
  margin: 0;
}
</style>
