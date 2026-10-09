<script setup lang="ts">
/**
 * P5 币信息（docs/14 §7.3 / docs/13 §3.3 P5 / §4.8）。
 *
 * - **双 Tab**：「全局资产」（`asset`，一币一行去重）/「各所口径」（`venue_asset`，一币×一所）
 * - 全局层显示**匹配状态**（`matched` / `ambiguous` / `not_found`）+ **手填 CG id** + **改 `kind`**
 * - 详情抽屉：`asset_contract` 子表 + CG 原始 `extra` + 「在交易对中查看」
 * - 头部「**立即同步币信息**」→ `POST /sync/offchain/runs`；旁显 `detail_synced_at` 与上轮结果
 * - 空态（§7.4）：外部源每 1 小时一轮；全局资产本身由交易对派生，故给一条「先同步交易所目录」的出路
 */
import { computed, reactive, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useQuery, useQueryClient } from '@tanstack/vue-query'
import { ElMessage } from 'element-plus'
import { Download, Refresh, Search } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import PagerBar from '../components/PagerBar.vue'
import CatalogSyncBar from '../components/CatalogSyncBar.vue'
import CatalogEmptyState from '../components/CatalogEmptyState.vue'
import AssetDrawer from '../components/AssetDrawer.vue'
import {
  exportAssets,
  getAssetWithEtag,
  listAssets,
  listExchanges,
  listVenueAssets,
  patchAsset,
  type AssetListQuery,
} from '../api/catalog'
import { qk } from '../query/keys'
import { useCatalogSync } from '../query/useCatalogSync'
import { useSessionStore } from '../stores/session'
import { fmtTs } from '../utils/format'
import { promptExternalId } from '../utils/asset'
import { assetKindChip, assetMatchChip } from '../utils/status'
import type { AssetPatch, AssetRow } from '../api/types'

const session = useSessionStore()
const route = useRoute()
const router = useRouter()
const qc = useQueryClient()

/** 外部币信息同步（docs/14 §9 两条 offchain 接口）。 */
const offchainSync = useCatalogSync('offchain')
/** 交易所目录同步：空态里给一条出路（全局资产由 base/quote 派生）。 */
const catalogSync = useCatalogSync('catalog')

const tab = ref('global')

// ── 交易所列表（venue 下拉 + 各所 Tab 的默认值）──────────────────────────────
const { data: exchanges } = useQuery({
  queryKey: qk.exchanges({ limit: 500, offset: 0 }),
  queryFn: ({ signal }) => listExchanges({ limit: 500, offset: 0 }, signal),
  staleTime: 120_000,
  retry: false,
})
const venues = computed(() => (exchanges.value?.items ?? []).map((e) => e.venue))

// ── Tab 1：全局资产 ─────────────────────────────────────────────────────────
const g = reactive<{ kind: string; q: string; venue: string; deposit: string; withdraw: string; limit: number; offset: number }>({
  kind: '',
  q: '',
  venue: '',
  deposit: '',
  withdraw: '',
  limit: 50,
  offset: 0,
})

// 深链：交易对页 base/quote → 本页按 code 预筛（接口只提供 q 模糊，docs/13 §4.8）
const fromCode = route.query.code ?? route.query.q
if (fromCode) g.q = String(fromCode)

const gParams = computed<AssetListQuery>(() => ({
  kind: g.kind || undefined,
  q: g.q || undefined,
  venue: g.venue || undefined,
  deposit_enabled: g.deposit === '' ? undefined : g.deposit === 'true',
  withdraw_enabled: g.withdraw === '' ? undefined : g.withdraw === 'true',
  limit: g.limit,
  offset: g.offset,
}))

const { data: gData, isFetching: gFetching, refetch: gRefetch, isError: gError, error: gErr } = useQuery({
  queryKey: qk.assets(gParams),
  queryFn: ({ signal }) => listAssets(gParams.value, signal),
  refetchInterval: 60_000,
})

const gRows = computed(() => gData.value?.items ?? [])
const gTotal = computed(() => gData.value?.total ?? gRows.value.length)
const gLoading = computed(() => gFetching.value && !gData.value)
const gEmpty = computed(() => !gLoading.value && gTotal.value === 0)
const gHasFilters = computed(() => !!(g.kind || g.q || g.venue || g.deposit || g.withdraw))

function resetGlobal(): void {
  g.kind = ''
  g.q = ''
  g.venue = ''
  g.deposit = ''
  g.withdraw = ''
  g.offset = 0
}

function onGlobalPage(next: { limit: number; offset: number }): void {
  g.limit = next.limit
  g.offset = next.offset
}

// ── Tab 2：各所口径 ─────────────────────────────────────────────────────────
const v = reactive<{ venue: string; q: string; limit: number; offset: number }>({
  venue: '',
  q: '',
  limit: 50,
  offset: 0,
})

// 默认选中第一个已登记交易所，避免一进 Tab 就空手（交易所列表是异步来的，故用 watch）
watch(venues, (list) => {
  if (!v.venue && list.length) v.venue = list[0] ?? ''
}, { immediate: true })

/** 查询用 venue：未显式选择时回落到第一个已登记交易所。 */
const venueKey = computed(() => v.venue)

const vQuery = computed(() => ({ q: v.q || undefined, limit: v.limit, offset: v.offset }))

const {
  data: vData,
  isFetching: vFetching,
  refetch: vRefetch,
  isError: vError,
  error: vErr,
} = useQuery({
  queryKey: qk.venueAssets(venueKey, vQuery),
  queryFn: ({ signal }) => listVenueAssets(v.venue, vQuery.value, signal),
  enabled: computed(() => !!v.venue),
  retry: false,
})

const vRows = computed(() => vData.value?.items ?? [])
const vTotal = computed(() => vData.value?.total ?? vRows.value.length)

function resetVenue(): void {
  v.q = ''
  v.offset = 0
}

function onVenuePage(next: { limit: number; offset: number }): void {
  v.limit = next.limit
  v.offset = next.offset
}

// ── 详情抽屉 + 可写点（PATCH /assets/{code} + If-Match）──────────────────────
const selected = ref<AssetRow | null>(null)

function openDrawer(row: AssetRow): void {
  selected.value = row
}

async function patch(row: AssetRow, body: AssetPatch): Promise<void> {
  // §5.3：行记录写必须带 If-Match —— 先取该行最新 ETag，取不到再用列表行自带的 etag 兜底。
  const { etag } = await getAssetWithEtag(row.code)
  const updated = await patchAsset(row.code, body, etag ?? row.etag ?? '*')
  ElMessage.success(`币 ${row.code} 已更新（kind / external_id 不参与 row_hash 比对，docs/14 §11）`)
  if (selected.value?.code === row.code) selected.value = { ...selected.value, ...updated }
  await qc.invalidateQueries({ queryKey: ['ponk', 'assets'] })
  await qc.invalidateQueries({ queryKey: ['ponk', 'asset'] })
}

async function changeKind(row: AssetRow, k: unknown): Promise<void> {
  await patch(row, { kind: String(k) })
}

async function editExternalIdInline(row: AssetRow): Promise<void> {
  const next = await promptExternalId(row.code, row.external_id)
  if (next === undefined) return
  await patch(row, { external_id: next })
}

async function patchSelected(body: AssetPatch): Promise<void> {
  if (selected.value) await patch(selected.value, body)
}

// ── 同步 / 导出 ─────────────────────────────────────────────────────────────
async function onSync(): Promise<void> {
  await offchainSync.trigger()
}

async function onCatalogSync(): Promise<void> {
  await catalogSync.trigger()
}

async function exportCsv(): Promise<void> {
  await exportAssets(gParams.value)
}

function gotoInstruments(code: string): void {
  void router.push({ path: '/catalog/instruments', query: { base: code } })
}
</script>

<template>
  <div class="page">
    <PageHeader
      title="币信息"
      page="P5"
      desc="两层：全局 `asset`（一币一行，跨所对齐的锚）与各所 `venue_asset`（精度 / 网络 / 可提可充按所存，12 §3.2）。可写点只有 kind 与 external_id（docs/14 §7.3）。"
    >
      <template #actions>
        <el-button :icon="Refresh" @click="() => { gRefetch(); if (v.venue) vRefetch() }">刷新</el-button>
        <el-button :icon="Download" @click="exportCsv">导出 CSV</el-button>
        <el-button
          v-if="session.canWrite"
          type="primary"
          :loading="offchainSync.syncing"
          :disabled="offchainSync.running"
          @click="onSync"
        >立即同步币信息</el-button>
      </template>
    </PageHeader>

    <CatalogSyncBar :state="offchainSync" :hide-trigger="gEmpty && tab === 'global'" @sync="onSync" />

    <el-tabs v-model="tab" class="tabs">
      <!-- ── 全局资产 ─────────────────────────────────────────────────── -->
      <el-tab-pane label="全局资产" name="global">
        <el-alert
          v-if="gError"
          type="error"
          show-icon
          :closable="false"
          style="margin-bottom: 12px"
          title="全局资产列表加载失败"
          :description="(gErr as Error)?.message ?? '请求失败'"
        />

        <div class="filters">
          <el-select v-model="g.kind" placeholder="kind" clearable style="width: 130px">
            <el-option label="coin" value="coin" />
            <el-option label="token" value="token" />
            <el-option label="stock" value="stock" />
            <el-option label="index" value="index" />
          </el-select>
          <el-input
            v-model="g.q"
            placeholder="code / name 关键字"
            clearable
            :prefix-icon="Search"
            style="width: 220px"
          />
          <el-select v-model="g.venue" placeholder="按所过滤" clearable filterable style="width: 150px">
            <el-option v-for="x in venues" :key="x" :label="x" :value="x" />
          </el-select>
          <el-select v-model="g.deposit" placeholder="可充值" clearable style="width: 120px">
            <el-option label="可充值" value="true" />
            <el-option label="不可充" value="false" />
          </el-select>
          <el-select v-model="g.withdraw" placeholder="可提币" clearable style="width: 120px">
            <el-option label="可提币" value="true" />
            <el-option label="不可提" value="false" />
          </el-select>
          <el-button @click="resetGlobal">重置</el-button>
          <span class="muted">共 {{ gTotal }} 条</span>
        </div>

        <!-- §7.4 空态（币） -->
        <CatalogEmptyState
          v-if="gEmpty"
          :state="offchainSync"
          interval-text="交易对每 5 分钟自动同步一次（全局资产由 base/quote 派生），币信息外部源每 1 小时同步一次"
          @sync="onSync"
        >
          <template #extra>
            <el-button
              v-if="session.canWrite"
              :loading="catalogSync.syncing"
              :disabled="catalogSync.running"
              @click="onCatalogSync"
            >先同步交易所目录（POST /sync/runs）</el-button>
            <el-button v-if="gHasFilters" @click="resetGlobal">清除筛选</el-button>
          </template>
        </CatalogEmptyState>

        <div v-else class="panel">
          <el-table :data="gRows" v-loading="gFetching" size="small" stripe>
            <el-table-column label="code" width="110" fixed="left">
              <template #default="{ row }">
                <a href="javascript:;" class="mono strong" @click="openDrawer(row)">{{ row.code }}</a>
              </template>
            </el-table-column>
            <el-table-column label="name" min-width="150">
              <template #default="{ row }"><span :class="{ muted: !row.name }">{{ row.name || '—' }}</span></template>
            </el-table-column>
            <el-table-column label="kind" width="140">
              <template #default="{ row }">
                <div class="flex-row">
                  <StatusTag :chip="assetKindChip(row.kind)" />
                  <el-select
                    v-if="session.canWrite"
                    :model-value="row.kind"
                    size="small"
                    style="width: 90px"
                    @change="(k: unknown) => changeKind(row, k)"
                  >
                    <el-option label="coin" value="coin" />
                    <el-option label="token" value="token" />
                    <el-option label="stock" value="stock" />
                    <el-option label="index" value="index" />
                  </el-select>
                </div>
              </template>
            </el-table-column>
            <el-table-column label="匹配状态" width="180">
              <template #default="{ row }">
                <div class="flex-row">
                  <StatusTag :chip="assetMatchChip(row.match_status)" />
                  <span class="mono sub" :class="{ muted: !row.external_id }">{{ row.external_id ?? '未收录' }}</span>
                </div>
              </template>
            </el-table-column>
            <el-table-column label="total_supply" width="150" align="right">
              <template #default="{ row }"><span class="mono">{{ row.total_supply ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="circulating_supply" width="150" align="right">
              <template #default="{ row }"><span class="mono">{{ row.circulating_supply ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="twitter" width="110">
              <template #default="{ row }">
                <a v-if="row.twitter_url" :href="row.twitter_url" target="_blank" rel="noreferrer">链接</a>
                <span v-else class="muted">—</span>
              </template>
            </el-table-column>
            <el-table-column label="来源" width="110">
              <template #default="{ row }"><span class="mono">{{ row.detail_source ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="detail_synced_at" width="145">
              <template #default="{ row }">{{ fmtTs(row.detail_synced_at) }}</template>
            </el-table-column>
            <el-table-column label="各所" width="70" align="right">
              <template #default="{ row }">{{ row.venue_count ?? row.venues ?? 0 }}</template>
            </el-table-column>
            <el-table-column label="操作" width="160" fixed="right">
              <template #default="{ row }">
                <el-button link type="primary" size="small" @click="openDrawer(row)">详情</el-button>
                <el-button v-if="session.canWrite" link type="primary" size="small" @click="editExternalIdInline(row)">
                  手填 CG id
                </el-button>
                <el-button link type="primary" size="small" @click="gotoInstruments(String(row.code))">交易对</el-button>
              </template>
            </el-table-column>
          </el-table>

          <div class="table-footer">
            <PagerBar :total="gTotal" :limit="g.limit" :offset="g.offset" @change="onGlobalPage" />
          </div>
        </div>
      </el-tab-pane>

      <!-- ── 各所口径 ─────────────────────────────────────────────────── -->
      <el-tab-pane label="各所口径" name="venue">
        <el-alert
          v-if="vError"
          type="error"
          show-icon
          :closable="false"
          style="margin-bottom: 12px"
          title="各所口径加载失败"
          :description="(vErr as Error)?.message ?? '请求失败'"
        />

        <div class="filters">
          <el-select v-model="v.venue" placeholder="选择交易所（必选）" filterable style="width: 190px">
            <el-option v-for="x in venues" :key="x" :label="x" :value="x" />
          </el-select>
          <el-input
            v-model="v.q"
            placeholder="code 关键字"
            clearable
            :prefix-icon="Search"
            style="width: 200px"
            @clear="resetVenue"
          />
          <el-button @click="resetVenue">重置</el-button>
          <span class="muted">共 {{ vTotal }} 条</span>
        </div>

        <el-empty v-if="!v.venue" description="请先选择一个交易所（各所口径按 venue 过滤，docs/13 §4.8）" :image-size="60" />

        <div v-else class="panel">
          <el-table :data="vRows" v-loading="vFetching" size="small" stripe>
            <el-table-column label="venue" width="110">
              <template #default="{ row }"><span class="mono strong">{{ row.venue }}</span></template>
            </el-table-column>
            <el-table-column label="code" width="110">
              <template #default="{ row }"><span class="mono">{{ row.code ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="precision" width="90" align="right">
              <template #default="{ row }"><span class="mono">{{ row.precision ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="networks" min-width="170">
              <template #default="{ row }">
                <el-tag v-for="n in row.networks ?? []" :key="n" size="small" effect="plain" class="net">{{ n }}</el-tag>
                <span v-if="!(row.networks ?? []).length" class="muted">—</span>
              </template>
            </el-table-column>
            <el-table-column label="可充" width="80">
              <template #default="{ row }">
                <el-tag :type="row.deposit_enabled === true ? 'success' : row.deposit_enabled === false ? 'info' : 'warning'" size="small" effect="plain">
                  {{ row.deposit_enabled === true ? '✓' : row.deposit_enabled === false ? '✗' : '未知' }}
                </el-tag>
              </template>
            </el-table-column>
            <el-table-column label="可提" width="80">
              <template #default="{ row }">
                <el-tag :type="row.withdraw_enabled === true ? 'success' : row.withdraw_enabled === false ? 'info' : 'warning'" size="small" effect="plain">
                  {{ row.withdraw_enabled === true ? '✓' : row.withdraw_enabled === false ? '✗' : '未知' }}
                </el-tag>
              </template>
            </el-table-column>
            <el-table-column label="所方 status" width="120">
              <template #default="{ row }"><span class="mono">{{ row.status ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="row_hash" min-width="150">
              <template #default="{ row }"><span class="mono sub muted">{{ row.row_hash ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="last_synced_at" width="145">
              <template #default="{ row }">{{ fmtTs(row.last_synced_at) }}</template>
            </el-table-column>
          </el-table>

          <div class="table-footer">
            <PagerBar :total="vTotal" :limit="v.limit" :offset="v.offset" @change="onVenuePage" />
          </div>
        </div>
      </el-tab-pane>
    </el-tabs>

    <AssetDrawer :row="selected" @close="selected = null" @patch="patchSelected" />
  </div>
</template>

<style scoped>
.strong {
  font-weight: 600;
}

.sub {
  font-size: 11px;
}

.net {
  margin-right: 4px;
}

.tabs :deep(.el-tabs__content) {
  padding-top: 4px;
}
</style>
