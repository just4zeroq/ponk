<script setup lang="ts">
/**
 * P5 币信息详情抽屉（docs/14 §7.3）。
 *
 * 区块：基本 / 匹配（三级）/ 供应与社交 / **`asset_contract` 子表（链·地址·标准·主链）** /
 * 各所口径 / CG 原始 `extra`。
 *
 * 可写点只有两处（docs/14 §4 判断 3）：`kind`（人工兜底）与 `external_id`（手填 CG id）；
 * 它们**不参与** `instrument.row_hash` 比对。写入经 `emit('patch')` 交父页带 `If-Match` 提交。
 */
import { computed, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import { useQuery } from '@tanstack/vue-query'
import { Link } from '@element-plus/icons-vue'
import StatusTag from './StatusTag.vue'
import { getAssetContracts, type AssetDetail } from '../api/catalog'
import { qk } from '../query/keys'
import { useSessionStore } from '../stores/session'
import { assetKindChip, assetMatchChip } from '../utils/status'
import { fmtTs, prettyJson } from '../utils/format'
import { promptExternalId } from '../utils/asset'
import type { AssetKind, AssetPatch } from '../api/types'

const props = defineProps<{ row: AssetDetail | null }>()
const emit = defineEmits<{ close: []; patch: [body: AssetPatch] }>()
const router = useRouter()
const session = useSessionStore()

const open = computed({
  get: () => !!props.row,
  set: (v: boolean) => {
    if (!v) emit('close')
  },
})

const code = computed(() => props.row?.code ?? '')

// ── `asset_contract` 子表（docs/14 §9：GET /assets/{code}/contracts）───────────
const { data: contracts, isFetching: contractsLoading, isError: contractsError } = useQuery({
  queryKey: qk.assetContracts(code),
  queryFn: ({ signal }) => getAssetContracts(code.value, signal),
  enabled: computed(() => !!code.value),
  retry: false,
})

// ── 可写点：kind / external_id ──────────────────────────────────────────────
const kindDraft = ref<AssetKind>('coin')

watch(
  () => props.row,
  (r) => {
    kindDraft.value = r?.kind ?? 'coin'
  },
  { immediate: true },
)

function onKindChange(v: AssetKind): void {
  if (!props.row || v === props.row.kind) return
  emit('patch', { kind: v })
}

async function editExternalId(): Promise<void> {
  const r = props.row
  if (!r) return
  const next = await promptExternalId(r.code, r.external_id)
  if (next === undefined) return
  emit('patch', { external_id: next })
}
function gotoInstruments(): void {
  const c = props.row?.code
  if (!c) return
  void router.push({ path: '/catalog/instruments', query: { base: c } })
}

function extraText(row: AssetDetail): string {
  return row.extra && Object.keys(row.extra).length ? prettyJson(row.extra) : '—'
}
</script>

<template>
  <el-drawer v-model="open" :title="row ? `${row.code}${row.name ? ' · ' + row.name : ''}` : '币详情'" size="660px">
    <template v-if="row">
      <div class="block-head">
        <h4>基本</h4>
        <div class="flex-row">
          <el-button size="small" type="primary" :icon="Link" @click="gotoInstruments">在交易对中查看</el-button>
        </div>
      </div>

      <el-descriptions :column="2" size="small" border>
        <el-descriptions-item label="code"><span class="mono strong">{{ row.code }}</span></el-descriptions-item>
        <el-descriptions-item label="name"><span :class="{ muted: !row.name }">{{ row.name || '—' }}</span></el-descriptions-item>
        <el-descriptions-item label="kind">
          <div class="flex-row">
            <StatusTag :chip="assetKindChip(row.kind)" />
            <el-select
              v-if="session.canWrite"
              v-model="kindDraft"
              size="small"
              style="width: 120px"
              @change="onKindChange"
            >
              <el-option label="coin" value="coin" />
              <el-option label="token" value="token" />
              <el-option label="stock" value="stock" />
              <el-option label="index" value="index" />
            </el-select>
          </div>
        </el-descriptions-item>
        <el-descriptions-item label="各所覆盖">{{ row.venue_count ?? row.venues ?? 0 }}</el-descriptions-item>
        <el-descriptions-item label="created_at">{{ fmtTs(row.created_at) }}</el-descriptions-item>
        <el-descriptions-item label="updated_at">{{ fmtTs(row.updated_at) }}</el-descriptions-item>
      </el-descriptions>

      <h4 style="margin-top: 16px">外部匹配（docs/14 §5.2 三级，宁可「未收录」不静默错配）</h4>
      <el-descriptions :column="2" size="small" border>
        <el-descriptions-item label="match_status">
          <div class="flex-row">
            <StatusTag :chip="assetMatchChip(row.match_status)" />
            <el-button v-if="session.canWrite" link type="primary" size="small" @click="editExternalId">
              {{ row.external_id ? '改手填 CG id' : '手填 CG id' }}
            </el-button>
          </div>
        </el-descriptions-item>
        <el-descriptions-item label="external_id">
          <span class="mono" :class="{ muted: !row.external_id }">{{ row.external_id ?? 'NULL（未匹配）' }}</span>
        </el-descriptions-item>
        <el-descriptions-item label="detail_source"><span class="mono">{{ row.detail_source ?? '—' }}</span></el-descriptions-item>
        <el-descriptions-item label="detail_synced_at">{{ fmtTs(row.detail_synced_at) }}</el-descriptions-item>
        <el-descriptions-item label="total_supply">
          <span class="mono">{{ row.total_supply ?? '—' }}</span><span class="muted unit">十进制字符串</span>
        </el-descriptions-item>
        <el-descriptions-item label="circulating_supply">
          <span class="mono">{{ row.circulating_supply ?? '—' }}</span><span class="muted unit">十进制字符串</span>
        </el-descriptions-item>
        <el-descriptions-item label="twitter" :span="2">
          <a v-if="row.twitter_url" :href="row.twitter_url" target="_blank" rel="noreferrer" class="mono">{{ row.twitter_url }}</a>
          <span v-else class="muted">—</span>
        </el-descriptions-item>
      </el-descriptions>

      <h4 style="margin-top: 16px">
        asset_contract 子表（链 / 地址 / 标准 / 主链，docs/14 §4 判断 2）
      </h4>
      <el-table
        :data="contracts ?? []"
        v-loading="contractsLoading"
        size="small"
        stripe
        max-height="260"
      >
        <el-table-column label="chain" width="130">
          <template #default="{ row: c }"><span class="mono">{{ c.chain }}</span></template>
        </el-table-column>
        <el-table-column label="address" min-width="260">
          <template #default="{ row: c }"><span class="mono sub">{{ c.address }}</span></template>
        </el-table-column>
        <el-table-column label="standard" width="100">
          <template #default="{ row: c }"><span class="mono">{{ c.standard ?? '—' }}</span></template>
        </el-table-column>
        <el-table-column label="主链" width="70">
          <template #default="{ row: c }">
            <el-tag v-if="c.is_primary" type="success" size="small" effect="plain">主</el-tag>
            <span v-else class="muted">—</span>
          </template>
        </el-table-column>
      </el-table>
      <el-alert
        v-if="contractsError"
        type="warning"
        show-icon
        :closable="false"
        style="margin-top: 8px"
        title="合约地址接口不可用"
        description="GET /assets/{code}/contracts（docs/14 §9）未返回数据 —— 该表可能尚未接入，此处如实显示为空。"
      />
      <el-empty
        v-else-if="!contractsLoading && !(contracts ?? []).length"
        description="暂无链上合约地址（股票类交易对会整体跳过链上币信息，docs/14 §6）"
        :image-size="56"
      />

      <h4 style="margin-top: 16px">各所口径（venue_asset）</h4>
      <el-table :data="row.venue_assets ?? []" size="small" stripe max-height="220">
        <el-table-column label="venue" width="110">
          <template #default="{ row: v }"><span class="mono">{{ v.venue }}</span></template>
        </el-table-column>
        <el-table-column label="precision" width="90" align="right">
          <template #default="{ row: v }"><span class="mono">{{ v.precision ?? '—' }}</span></template>
        </el-table-column>
        <el-table-column label="networks" min-width="150">
          <template #default="{ row: v }">
            <el-tag v-for="n in v.networks ?? []" :key="n" size="small" effect="plain" class="net">{{ n }}</el-tag>
            <span v-if="!(v.networks ?? []).length" class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="充 / 提" width="110">
          <template #default="{ row: v }">
            <el-tag :type="v.deposit_enabled === true ? 'success' : v.deposit_enabled === false ? 'info' : 'warning'" size="small" effect="plain">
              充{{ v.deposit_enabled === true ? '✓' : v.deposit_enabled === false ? '✗' : '?' }}
            </el-tag>
            <el-tag :type="v.withdraw_enabled === true ? 'success' : v.withdraw_enabled === false ? 'info' : 'warning'" size="small" effect="plain">
              提{{ v.withdraw_enabled === true ? '✓' : v.withdraw_enabled === false ? '✗' : '?' }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="所方 status" width="110">
          <template #default="{ row: v }"><span class="mono">{{ v.status ?? '—' }}</span></template>
        </el-table-column>
      </el-table>
      <el-empty
        v-if="!(row.venue_assets ?? []).length"
        description="该币在各所层无记录 —— 交易所未提供币端点时允许为空，不阻塞交易对同步（docs/14 §11）"
        :image-size="56"
      />

      <h4 style="margin-top: 16px">CG 原始 extra（外部源加字段不必改表，12 §3.2）</h4>
      <pre class="json">{{ extraText(row) }}</pre>

      <el-alert
        v-if="session.canWrite"
        type="info"
        :closable="false"
        show-icon
        style="margin-top: 14px"
        title="可写点只有 kind 与 external_id（PATCH /assets/{code} + If-Match）。"
        description="二者不参与 instrument 的 row_hash 比对，不会被下一轮同步冲掉（docs/14 §11 人工与同步冲突）。"
      />
    </template>
    <span v-else class="muted">未选择币</span>
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
  margin: 0 0 8px;
  font-size: 13px;
}

.json {
  background: #fafafa;
  border: 1px solid var(--ponk-border);
  border-radius: 4px;
  padding: 8px 10px;
  font-size: 12px;
  line-height: 1.5;
  max-height: 260px;
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-all;
  margin: 0;
}

.strong {
  font-weight: 600;
}

.sub {
  font-size: 11px;
}

.unit {
  font-size: 11px;
  margin-left: 6px;
}

.net {
  margin-right: 4px;
}
</style>
