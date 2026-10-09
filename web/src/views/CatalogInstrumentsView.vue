<script setup lang="ts">
/**
 * P4 交易对 / P4b 变更历史（docs/14 §7.1「浏览用抽屉」/ §7.2 / docs/13 §3.3 P4 / §4.7）。
 *
 * - **只读**：交易对由 5 分钟同步按 `row_hash` 单写入（docs/14 §1 单写入方）
 * - 筛选：venue / market / status / native_code 模糊 / base / quote / synced_after
 * - 列含精度与 `tick_size` / `step_size`（**十进制字符串**，03 §5.5 —— 不当 number）
 * - 行点击 → 右侧抽屉（标的详情含 `extra` 原始 JSON + **变更历史**区块）
 * - base / quote 可点 → 跳币信息页；「排除标的」跳黑名单配置域（本页无写入口）
 * - 导出 CSV（筛选条件随导出，docs/13 §5.4）
 */
import { computed, reactive, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useQuery } from '@tanstack/vue-query'
import { Download, Refresh, Search } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import PagerBar from '../components/PagerBar.vue'
import CatalogSyncBar from '../components/CatalogSyncBar.vue'
import CatalogEmptyState from '../components/CatalogEmptyState.vue'
import InstrumentDrawer from '../components/InstrumentDrawer.vue'
import { exportInstruments, listExchanges, listInstruments, type InstrumentListQuery } from '../api/catalog'
import { qk } from '../query/keys'
import { useCatalogSync } from '../query/useCatalogSync'
import { fmtTs } from '../utils/format'
import { instrumentStatusChip } from '../utils/status'
import type { InstrumentRow } from '../api/types'

const route = useRoute()
const router = useRouter()

const sync = useCatalogSync('catalog')

// ── 筛选 ─────────────────────────────────────────────────────────────────────
const filters = reactive<{
  venue: string
  market: string
  status: string
  native_code: string
  base: string
  quote: string
  instrument_type: string
  synced_after: string
  sort: string
  order: string
  limit: number
  offset: number
}>({
  venue: '',
  market: '',
  status: '',
  native_code: '',
  base: '',
  quote: '',
  instrument_type: '',
  synced_after: '',
  sort: 'last_synced_at',
  order: 'desc',
  limit: 50,
  offset: 0,
})

// 深链预筛（docs/13 §3.5：P5 币信息 → P4 按 base 预筛；P3 → P4 按 venue）
for (const k of ['venue', 'market', 'status', 'native_code', 'base', 'quote'] as const) {
  const v = route.query[k]
  if (v) filters[k] = String(v)
}

const queryParams = computed<InstrumentListQuery>(() => ({
  venue: filters.venue || undefined,
  market: filters.market || undefined,
  status: filters.status || undefined,
  native_code: filters.native_code || undefined,
  base: filters.base || undefined,
  quote: filters.quote || undefined,
  instrument_type: filters.instrument_type || undefined,
  synced_after: filters.synced_after ? Number(filters.synced_after) : undefined,
  sort: filters.sort || undefined,
  order: (filters.order as 'asc' | 'desc') || undefined,
  limit: filters.limit,
  offset: filters.offset,
}))

const { data, isFetching, refetch, isError: listError, error: listErr } = useQuery({
  queryKey: qk.instruments(queryParams),
  queryFn: ({ signal }) => listInstruments(queryParams.value, signal),
  refetchInterval: 60_000,
})

const rows = computed(() => data.value?.items ?? [])
const total = computed(() => data.value?.total ?? rows.value.length)
const loading = computed(() => isFetching.value && !data.value)
const isEmpty = computed(() => !loading.value && total.value === 0)
const hasFilters = computed(() =>
  !!(filters.venue || filters.market || filters.status || filters.native_code || filters.base || filters.quote
    || filters.instrument_type || filters.synced_after),
)

// 交易所下拉（venue 选项来自 P3 台账）
const { data: exchanges } = useQuery({
  queryKey: qk.exchanges({ limit: 500, offset: 0 }),
  queryFn: ({ signal }) => listExchanges({ limit: 500, offset: 0 }, signal),
  staleTime: 120_000,
  retry: false,
})
const venues = computed(() => (exchanges.value?.items ?? []).map((e) => e.venue))

function resetFilters(): void {
  filters.venue = ''
  filters.market = ''
  filters.status = ''
  filters.native_code = ''
  filters.base = ''
  filters.quote = ''
  filters.instrument_type = ''
  filters.synced_after = ''
  filters.offset = 0
}

function onPage(next: { limit: number; offset: number }): void {
  filters.limit = next.limit
  filters.offset = next.offset
}

async function onSync(): Promise<void> {
  await sync.trigger()
}

async function exportCsv(): Promise<void> {
  await exportInstruments(queryParams.value)
}

// ── 详情抽屉（§7.1 浏览用抽屉）──────────────────────────────────────────────
const selected = ref<InstrumentRow | null>(null)

function openDrawer(row: InstrumentRow): void {
  selected.value = row
}

function onRowClick(row: InstrumentRow, column: { label?: string }): void {
  // 操作列上的按钮自行处理，避免点按钮又开抽屉
  if (column?.label === '操作') return
  openDrawer(row)
}

// ── 交叉跳转 ────────────────────────────────────────────────────────────────
function gotoAsset(code: string): void {
  void router.push({ path: '/catalog/assets', query: { code } })
}

function gotoVenue(venue: string): void {
  void router.push({ path: '/catalog/exchanges', query: { venue } })
}
</script>

<template>
  <div class="page">
    <PageHeader
      title="交易对"
      page="P4"
      desc="只读目录：由 5 分钟同步按 row_hash 单写入（docs/14 §1）。人工干预走黑名单配置域，本页不提供写入口（docs/14 §7.2）。"
    >
      <template #actions>
        <el-button :icon="Refresh" @click="() => refetch()">刷新</el-button>
        <el-button :icon="Download" @click="exportCsv">导出 CSV</el-button>
      </template>
    </PageHeader>

    <CatalogSyncBar v-if="!isEmpty" :state="sync" @sync="onSync" />

    <el-alert
      v-if="listError"
      type="error"
      show-icon
      :closable="false"
      style="margin-bottom: 12px"
      title="交易对列表加载失败"
      :description="(listErr as Error)?.message ?? '请求失败'"
    />

    <div class="filters">
      <el-select v-model="filters.venue" placeholder="venue" clearable filterable style="width: 150px">
        <el-option v-for="v in venues" :key="v" :label="v" :value="v" />
      </el-select>
      <el-select v-model="filters.market" placeholder="market" clearable style="width: 120px">
        <el-option label="spot" value="spot" />
        <el-option label="swap" value="swap" />
        <el-option label="future" value="future" />
      </el-select>
      <el-select v-model="filters.status" placeholder="status" clearable style="width: 130px">
        <el-option label="trading" value="trading" />
        <el-option label="break" value="break" />
        <el-option label="delisted" value="delisted" />
      </el-select>
      <el-select v-model="filters.instrument_type" placeholder="instrument_type" clearable style="width: 150px">
        <el-option label="spot" value="spot" />
        <el-option label="perpetual" value="perpetual" />
        <el-option label="future" value="future" />
      </el-select>
      <el-input
        v-model="filters.native_code"
        placeholder="native_code 模糊"
        clearable
        :prefix-icon="Search"
        style="width: 170px"
      />
      <el-input v-model="filters.base" placeholder="base，如 BTC" clearable style="width: 120px" />
      <el-input v-model="filters.quote" placeholder="quote，如 USDT" clearable style="width: 120px" />
      <el-date-picker
        v-model="filters.synced_after"
        type="datetime"
        placeholder="last_synced_at ≥"
        value-format="x"
        style="width: 190px"
      />
      <el-select v-model="filters.sort" style="width: 150px">
        <el-option label="last_synced_at" value="last_synced_at" />
        <el-option label="native_code" value="native_code" />
        <el-option label="status" value="status" />
        <el-option label="listed_at" value="listed_at" />
      </el-select>
      <el-select v-model="filters.order" style="width: 100px">
        <el-option label="降序" value="desc" />
        <el-option label="升序" value="asc" />
      </el-select>
      <el-button @click="resetFilters">重置</el-button>
      <span class="muted">共 {{ total }} 条</span>
    </div>

    <!-- §7.4 空态 -->
    <CatalogEmptyState
      v-if="isEmpty"
      :state="sync"
      interval-text="每 5 分钟自动同步一次"
      @sync="onSync"
    >
      <template #extra>
        <el-button v-if="hasFilters" @click="resetFilters">清除筛选</el-button>
      </template>
    </CatalogEmptyState>

    <template v-else>
      <div class="panel">
        <el-table :data="rows" v-loading="isFetching" size="small" stripe @row-click="onRowClick" class="clickable">
          <el-table-column label="venue" width="110">
            <template #default="{ row }">
              <a href="javascript:;" class="mono" @click.stop="gotoVenue(row.venue)">{{ row.venue }}</a>
            </template>
          </el-table-column>
          <el-table-column label="market" width="86">
            <template #default="{ row }"><span class="mono">{{ row.market }}</span></template>
          </el-table-column>
          <el-table-column label="native_code" min-width="150" fixed="left">
            <template #default="{ row }"><span class="mono strong">{{ row.native_code }}</span></template>
          </el-table-column>
          <el-table-column label="base / quote" width="150">
            <template #default="{ row }">
              <a v-if="row.base" href="javascript:;" class="mono" @click.stop="gotoAsset(String(row.base))">{{ row.base }}</a>
              <span v-else class="muted">—</span>
              <span class="muted"> / </span>
              <a v-if="row.quote" href="javascript:;" class="mono" @click.stop="gotoAsset(String(row.quote))">{{ row.quote }}</a>
              <span v-else class="muted">—</span>
            </template>
          </el-table-column>
          <el-table-column label="status" width="100">
            <template #default="{ row }"><StatusTag :chip="instrumentStatusChip(row.status)" /></template>
          </el-table-column>
          <el-table-column label="价格精度" width="90" align="right">
            <template #default="{ row }"><span class="mono">{{ row.price_precision ?? '—' }}</span></template>
          </el-table-column>
          <el-table-column label="数量精度" width="90" align="right">
            <template #default="{ row }"><span class="mono">{{ row.qty_precision ?? '—' }}</span></template>
          </el-table-column>
          <el-table-column label="tick_size" width="110" align="right">
            <template #default="{ row }">
              <span class="mono" :title="'十进制字符串（03 §5.5）'">{{ row.tick_size ?? '—' }}</span>
            </template>
          </el-table-column>
          <el-table-column label="step_size" width="110" align="right">
            <template #default="{ row }">
              <span class="mono" :title="'十进制字符串（03 §5.5）'">{{ row.step_size ?? '—' }}</span>
            </template>
          </el-table-column>
          <el-table-column label="last_synced_at" width="145">
            <template #default="{ row }">{{ fmtTs(row.last_synced_at) }}</template>
          </el-table-column>
          <el-table-column label="变更" width="90">
            <template #default="{ row }">
              <el-button link type="primary" size="small" @click.stop="openDrawer(row)">历史</el-button>
            </template>
          </el-table-column>
          <el-table-column label="操作" width="80" fixed="right">
            <template #default="{ row }">
              <el-button link type="primary" size="small" @click.stop="openDrawer(row)">详情</el-button>
            </template>
          </el-table-column>
        </el-table>

        <div class="table-footer">
          <PagerBar :total="total" :limit="filters.limit" :offset="filters.offset" @change="onPage" />
        </div>
      </div>

      <p class="muted note">
        点任意行打开右侧抽屉：<strong>标的详情</strong>（含 extra 原始 JSON）与<strong>变更历史</strong>
        （before_v / after_v，按 change_kind 筛，时间倒序）。精度与 tick_size / step_size 是十进制字符串（03 §5.5）。
      </p>
    </template>

    <InstrumentDrawer :row="selected" @close="selected = null" />
  </div>
</template>

<style scoped>
.strong {
  font-weight: 600;
}

.note {
  font-size: 12px;
  margin: 0;
}

:deep(.clickable tbody tr) {
  cursor: pointer;
}
</style>
