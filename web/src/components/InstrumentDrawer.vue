<script setup lang="ts">
/**
 * P4 交易对详情抽屉（docs/14 §7.1「浏览用抽屉」/ §7.2「抽屉两个区块」）。
 *
 * 区块一「标的详情」：全量字段 + `extra` 原始 JSON。
 * 区块二「变更历史」：`before_v` / `after_v`、时间倒序、按 `change_kind` 筛。
 *
 * 本抽屉**只读** —— 交易对由 5 分钟同步单写入（docs/14 §1 单写入方）；
 * 「排除某标的」跳黑名单配置域（P6），本页不提供写入口。
 */
import { computed, reactive, watch } from 'vue'
import { useRouter } from 'vue-router'
import { useQuery } from '@tanstack/vue-query'
import { Download, Link } from '@element-plus/icons-vue'
import StatusTag from './StatusTag.vue'
import PagerBar from './PagerBar.vue'
import { exportInstruments, listInstrumentChanges, type InstrumentListQuery } from '../api/catalog'
import { qk } from '../query/keys'
import { changeKindChip, instrumentStatusChip } from '../utils/status'
import { fmtTs, prettyJson } from '../utils/format'
import { INSTRUMENT_CHANGE_KINDS, changePayload, type InstrumentChange, type InstrumentRow } from '../api/types'

const props = defineProps<{ row: InstrumentRow | null }>()
const emit = defineEmits<{ close: [] }>()
const router = useRouter()

const open = computed({
  get: () => !!props.row,
  set: (v: boolean) => {
    if (!v) emit('close')
  },
})

const changeFilter = reactive<{ change_kind: string; limit: number; offset: number }>({
  change_kind: '',
  limit: 20,
  offset: 0,
})

// 打开抽屉时重置变更历史筛选
watch(
  () => props.row?.native_code,
  () => {
    changeFilter.change_kind = ''
    changeFilter.offset = 0
  },
)

const changeQuery = computed(() => {
  const r = props.row
  if (!r) return null
  return {
    venue: r.venue,
    market: r.market,
    native_code: r.native_code,
    change_kind: changeFilter.change_kind || undefined,
    limit: changeFilter.limit,
    offset: changeFilter.offset,
  }
})

const { data: changes, isFetching: changesLoading, refetch: refetchChanges } = useQuery({
  // 键里放 computed：换标的 / 换筛选时 vue-query 会重取（无需手动 refetch）。
  queryKey: qk.instrumentChanges(changeQuery),
  queryFn: ({ signal }) => listInstrumentChanges(changeQuery.value ?? {}, signal),
  enabled: computed(() => !!props.row),
  retry: false,
})

const changeRows = computed(() => changes.value?.items ?? [])
const changeTotal = computed(() => changes.value?.total ?? changeRows.value.length)

function onChangeKind(v: string): void {
  changeFilter.change_kind = v
  changeFilter.offset = 0
  void refetchChanges()
}

function onChangePage(next: { limit: number; offset: number }): void {
  changeFilter.limit = next.limit
  changeFilter.offset = next.offset
  void refetchChanges()
}

// ── 跨页跳转（docs/13 §3.5）──────────────────────────────────────────────────
function gotoVenue(venue: string): void {
  void router.push({ path: '/catalog/exchanges', query: { venue } })
}

function gotoAsset(code: string): void {
  void router.push({ path: '/catalog/assets', query: { code } })
}

/** 排除某标的 → 黑名单配置域（09 §2.1 / docs/14 §7.2：本页无写入口）。 */
function gotoBlocklist(): void {
  void router.push({ path: '/config/instrument_blocklist' })
}

async function exportCurrent(): Promise<void> {
  const r = props.row
  if (!r) return
  const q: InstrumentListQuery = { venue: r.venue, market: r.market, native_code: r.native_code }
  await exportInstruments(q)
}

/** `before_v` / `after_v`（接口同时给 `before` / `after` 别名）。 */
function payload(c: InstrumentChange, which: 'before' | 'after'): string {
  const v = changePayload(c, which)
  return v === undefined || v === null ? '—' : prettyJson(v)
}

function extraText(row: InstrumentRow): string {
  return row.extra && Object.keys(row.extra).length ? prettyJson(row.extra) : '—'
}
</script>

<template>
  <el-drawer v-model="open" :title="row ? `${row.venue} / ${row.market} / ${row.native_code}` : '标的详情'" size="660px">
    <template v-if="row">
      <!-- 区块一：标的详情 -->
      <div class="block-head">
        <h4>标的详情</h4>
        <div class="flex-row">
          <el-button size="small" :icon="Download" @click="exportCurrent">导出本条 CSV</el-button>
          <el-button size="small" :icon="Link" @click="gotoBlocklist">排除标的（黑名单域）</el-button>
        </div>
      </div>

      <el-descriptions :column="2" size="small" border>
        <el-descriptions-item label="venue">
          <a href="javascript:;" class="mono" @click="gotoVenue(row.venue)">{{ row.venue }}</a>
        </el-descriptions-item>
        <el-descriptions-item label="market"><span class="mono">{{ row.market }}</span></el-descriptions-item>
        <el-descriptions-item label="native_code" :span="2"><span class="mono strong">{{ row.native_code }}</span></el-descriptions-item>
        <el-descriptions-item label="base">
          <a v-if="row.base" href="javascript:;" class="mono" @click="gotoAsset(row.base)">{{ row.base }}</a>
          <span v-else class="muted">—</span>
        </el-descriptions-item>
        <el-descriptions-item label="quote">
          <a v-if="row.quote" href="javascript:;" class="mono" @click="gotoAsset(row.quote)">{{ row.quote }}</a>
          <span v-else class="muted">—</span>
        </el-descriptions-item>
        <el-descriptions-item label="status"><StatusTag :chip="instrumentStatusChip(row.status)" /></el-descriptions-item>
        <el-descriptions-item label="instrument_type">
          <span class="mono">{{ row.instrument_type ?? '—' }}</span>
        </el-descriptions-item>
        <el-descriptions-item label="price_precision"><span class="mono">{{ row.price_precision ?? '—' }}</span></el-descriptions-item>
        <el-descriptions-item label="qty_precision"><span class="mono">{{ row.qty_precision ?? '—' }}</span></el-descriptions-item>
        <el-descriptions-item label="tick_size">
          <span class="mono">{{ row.tick_size ?? '—' }}</span>
          <span class="muted unit">十进制字符串</span>
        </el-descriptions-item>
        <el-descriptions-item label="step_size">
          <span class="mono">{{ row.step_size ?? '—' }}</span>
          <span class="muted unit">十进制字符串</span>
        </el-descriptions-item>
        <el-descriptions-item label="min_notional">
          <span class="mono">{{ row.min_notional ?? '—' }}</span>
        </el-descriptions-item>
        <el-descriptions-item label="row_hash"><span class="mono sub">{{ row.row_hash ?? '—' }}</span></el-descriptions-item>
        <el-descriptions-item label="listed_at">{{ fmtTs(row.listed_at) }}</el-descriptions-item>
        <el-descriptions-item label="delisted_at">{{ fmtTs(row.delisted_at) }}</el-descriptions-item>
        <el-descriptions-item label="first_seen_at">{{ fmtTs(row.first_seen_at) }}</el-descriptions-item>
        <el-descriptions-item label="last_synced_at">{{ fmtTs(row.last_synced_at) }}</el-descriptions-item>
      </el-descriptions>

      <h4 style="margin-top: 16px">extra 原始 JSON（所方原样，12 §3.3）</h4>
      <pre class="json">{{ extraText(row) }}</pre>

      <!-- 区块二：变更历史 -->
      <div class="block-head" style="margin-top: 18px">
        <h4>变更历史（instrument_change，时间倒序）</h4>
        <el-select
          :model-value="changeFilter.change_kind"
          placeholder="全部类型"
          clearable
          size="small"
          style="width: 150px"
          @change="onChangeKind"
        >
          <el-option v-for="k in INSTRUMENT_CHANGE_KINDS" :key="k" :label="k" :value="k" />
        </el-select>
      </div>

      <el-table :data="changeRows" v-loading="changesLoading" size="small" stripe>
        <el-table-column type="expand">
          <template #default="{ row: c }">
            <div class="diff">
              <div class="diff-col">
                <div class="diff-title">before_v（变更前）</div>
                <pre class="json">{{ payload(c, 'before') }}</pre>
              </div>
              <div class="diff-col">
                <div class="diff-title">after_v（变更后）</div>
                <pre class="json">{{ payload(c, 'after') }}</pre>
              </div>
            </div>
          </template>
        </el-table-column>
        <el-table-column label="detected_at" width="145">
          <template #default="{ row: c }">{{ fmtTs(c.detected_at) }}</template>
        </el-table-column>
        <el-table-column label="change_kind" width="110">
          <template #default="{ row: c }"><StatusTag :chip="changeKindChip(c.change_kind)" /></template>
        </el-table-column>
        <el-table-column label="id" width="70">
          <template #default="{ row: c }"><span class="mono">#{{ c.id }}</span></template>
        </el-table-column>
        <el-table-column label="sync_run_id" width="120">
          <template #default="{ row: c }">
            <RouterLink v-if="c.sync_run_id" :to="{ path: '/sync' }" class="mono">#{{ c.sync_run_id }}</RouterLink>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
      </el-table>
      <el-empty v-if="!changesLoading && !changeRows.length" description="该标的暂无变更记录" :image-size="56" />
      <div class="table-footer">
        <PagerBar :total="changeTotal" :limit="changeFilter.limit" :offset="changeFilter.offset" @change="onChangePage" />
      </div>

      <el-alert
        type="info"
        :closable="false"
        show-icon
        style="margin-top: 14px"
        title="本页只读：交易对由 5 分钟同步按 row_hash 单写入（docs/14 §1）。"
        description="要排除某标的，走黑名单配置域 InstrumentBlocklist（09 §2.1），此处只提供跳转。"
      />
    </template>
    <span v-else class="muted">未选择标的</span>
  </el-drawer>
</template>

<style scoped>
.block-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  margin-bottom: 8px;
}

h4 {
  margin: 0;
  font-size: 13px;
}

.json {
  background: #fafafa;
  border: 1px solid var(--ponk-border);
  border-radius: 4px;
  padding: 8px 10px;
  font-size: 12px;
  line-height: 1.5;
  max-height: 240px;
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-all;
  margin: 0;
}

.diff {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 10px;
  padding: 6px 8px;
}

.diff-title {
  font-size: 12px;
  color: var(--ponk-text-3);
  margin-bottom: 4px;
}

.unit {
  font-size: 11px;
  margin-left: 6px;
}

.strong {
  font-weight: 600;
}

.sub {
  font-size: 11px;
}

</style>
