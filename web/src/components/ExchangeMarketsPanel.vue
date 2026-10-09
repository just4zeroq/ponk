<script setup lang="ts">
/**
 * P3 展开区：某交易所下的市场台账（docs/14 §6 两步创建的第二步）。
 *
 * - 配额**按市场逐条展开**（列表页不显示"平均配额"——无物理意义，docs/14 §6）
 * - `dig3_account_type` **必填且无默认值**（12 §3.1：写错连不上），给出 02 §6 映射提示
 * - 三项配额 ∈ [0,1] 且之和 ≤ 1（02 §6：0.50 / 0.35 / 0.15 按 (venue, 出口IP) 切）
 * - 编辑 / 启停带 `If-Match`（§5.3，412 / 428）
 */
import { computed, reactive, ref } from 'vue'
import { useRouter } from 'vue-router'
import { useQuery, useQueryClient } from '@tanstack/vue-query'
import { ElMessage, type FormInstance } from 'element-plus'
import { Plus } from '@element-plus/icons-vue'
import StatusTag from './StatusTag.vue'
import { createMarket, listMarkets, setMarketEnabled, updateMarket } from '../api/catalog'
import { qk } from '../query/keys'
import { useSessionStore } from '../stores/session'
import { venueEnabledChip } from '../utils/status'
import { fmtTs } from '../utils/format'
import type { ExchangeMarketInput, ExchangeMarketRecord } from '../api/types'

const props = defineProps<{ venue: string }>()

const session = useSessionStore()
const router = useRouter()
const qc = useQueryClient()

const { data, isFetching, isError } = useQuery({
  queryKey: qk.markets(props.venue),
  queryFn: ({ signal }) => listMarkets(props.venue, signal),
  // §4.1：本行无 limit/offset → 裸数组
  retry: false,
})

const rows = computed(() => data.value ?? [])

/** 02 §6 的 `Market → dig3 AccountType` 硬映射（写错连不上，故给预设）。 */
const ACCOUNT_TYPES: Array<{ market: string; account: string }> = [
  { market: 'spot', account: 'Spot' },
  { market: 'swap', account: 'FuturesCross' },
  { market: 'future', account: 'FuturesCross' },
]

const dialogVisible = ref(false)
const editing = ref<ExchangeMarketRecord | null>(null)
const formRef = ref<FormInstance>()
const saving = ref(false)

const form = reactive<ExchangeMarketInput>({
  market: '',
  dig3_account_type: '',
  enabled: true,
  quota_collect: 0.5,
  quota_backfill: 0.35,
  quota_metadata: 0.15,
  egress_ip: '',
})

const quotaSum = computed(
  () => (form.quota_collect ?? 0) + (form.quota_backfill ?? 0) + (form.quota_metadata ?? 0),
)

const rules = {
  market: [{ required: true, message: '市场必填（spot / swap / future，02 §3）', trigger: 'blur' }],
  dig3_account_type: [
    { required: true, message: 'dig3_account_type 必填，无默认值（12 §3.1：写错连不上）', trigger: 'change' },
  ],
  quota_collect: [{ validator: quotaRule, trigger: 'change' }],
  quota_backfill: [{ validator: quotaRule, trigger: 'change' }],
  quota_metadata: [{ validator: quotaRule, trigger: 'change' }],
}

function quotaRule(_rule: unknown, value: number, callback: (e?: Error) => void): void {
  if (value === undefined || value === null || Number.isNaN(value)) {
    callback(new Error('配额份额必填'))
    return
  }
  if (value < 0 || value > 1) callback(new Error('三项配额各自 ∈ [0,1]（02 §6）'))
  else if (quotaSum.value > 1 + 1e-9)
    callback(new Error(`三项之和 ${quotaSum.value.toFixed(2)} > 1：同一 (venue, 出口IP) 的份额切分之和不得超过 1（02 §6）`))
  else callback()
}

function applyAccountType(market: string): void {
  const hit = ACCOUNT_TYPES.find((a) => a.market === market)
  if (hit) form.dig3_account_type = hit.account
}

function openCreate(): void {
  editing.value = null
  form.market = ''
  form.dig3_account_type = ''
  form.enabled = true
  form.quota_collect = 0.5
  form.quota_backfill = 0.35
  form.quota_metadata = 0.15
  form.egress_ip = ''
  dialogVisible.value = true
}

function openEdit(row: ExchangeMarketRecord): void {
  editing.value = row
  form.market = row.market
  form.dig3_account_type = row.dig3_account_type
  form.enabled = row.enabled ?? true
  form.quota_collect = row.quota_collect ?? 0.5
  form.quota_backfill = row.quota_backfill ?? 0.35
  form.quota_metadata = row.quota_metadata ?? 0.15
  form.egress_ip = row.egress_ip ?? ''
  dialogVisible.value = true
}

function toBody(): ExchangeMarketInput {
  return {
    market: form.market.trim(),
    dig3_account_type: form.dig3_account_type.trim(),
    enabled: form.enabled ?? true,
    quota_collect: form.quota_collect,
    quota_backfill: form.quota_backfill,
    quota_metadata: form.quota_metadata,
    egress_ip: form.egress_ip?.trim() || null,
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
      // §5.3：行记录写必须带 If-Match（列表行由后端 with_etag 给出 etag）
      await updateMarket(props.venue, editing.value.market, body, editing.value.etag ?? '*')
      ElMessage.success('市场已更新')
    } else {
      await createMarket(props.venue, body)
      ElMessage.success(`市场 ${body.market} 已创建`)
    }
    dialogVisible.value = false
    await qc.invalidateQueries({ queryKey: ['ponk', 'markets', props.venue] })
    await qc.invalidateQueries({ queryKey: ['ponk', 'exchanges'] })
  } finally {
    saving.value = false
  }
}

async function toggleEnabled(row: ExchangeMarketRecord): Promise<void> {
  await setMarketEnabled(props.venue, row.market, !(row.enabled ?? true), row.etag ?? '*')
  ElMessage.success(`市场 ${row.market} 已${row.enabled ? '停用' : '启用'}`)
  await qc.invalidateQueries({ queryKey: ['ponk', 'markets', props.venue] })
  await qc.invalidateQueries({ queryKey: ['ponk', 'exchanges'] })
}

function viewInstruments(row: ExchangeMarketRecord): void {
  void router.push({ path: '/catalog/instruments', query: { venue: props.venue, market: row.market } })
}

function quota(v?: number | null): string {
  return v === undefined || v === null ? '—' : v.toFixed(2)
}
</script>

<template>
  <div class="markets">
    <div class="head">
      <span class="muted">市场台账（配额按市场逐条，docs/14 §6：列表不显示平均配额）</span>
      <el-button v-if="session.canWrite" size="small" type="primary" :icon="Plus" @click="openCreate">新增市场</el-button>
    </div>

    <el-alert v-if="isError" type="error" show-icon :closable="false" title="市场列表加载失败" style="margin-bottom: 8px" />

    <el-table :data="rows" v-loading="isFetching" size="small" stripe>
      <el-table-column label="market" width="90">
        <template #default="{ row }"><span class="mono">{{ row.market }}</span></template>
      </el-table-column>
      <el-table-column label="dig3_account_type" width="150">
        <template #default="{ row }">
          <span class="mono">{{ row.dig3_account_type }}</span>
        </template>
      </el-table-column>
      <el-table-column label="enabled" width="80">
        <template #default="{ row }"><StatusTag :chip="venueEnabledChip(row.enabled)" /></template>
      </el-table-column>
      <el-table-column label="配额 采集/回填/元数据（02 §6）" min-width="220">
        <template #default="{ row }">
          <span class="mono quota">{{ quota(row.quota_collect) }} / {{ quota(row.quota_backfill) }} / {{ quota(row.quota_metadata) }}</span>
        </template>
      </el-table-column>
      <el-table-column label="egress_ip" width="130">
        <template #default="{ row }">
          <span :class="{ muted: !row.egress_ip }" class="mono">{{ row.egress_ip || '—' }}</span>
        </template>
      </el-table-column>
      <el-table-column label="updated_at" width="145">
        <template #default="{ row }">{{ fmtTs(row.updated_at) }}</template>
      </el-table-column>
      <el-table-column label="操作" width="180" fixed="right">
        <template #default="{ row }">
          <el-button link type="primary" size="small" @click="viewInstruments(row)">交易对</el-button>
          <el-button v-if="session.canWrite" link type="primary" size="small" @click="openEdit(row)">编辑</el-button>
          <el-button v-if="session.canWrite" link type="warning" size="small" @click="toggleEnabled(row)">
            {{ row.enabled ? '停用' : '启用' }}
          </el-button>
        </template>
      </el-table-column>
    </el-table>
    <el-empty v-if="!isFetching && !rows.length" description="该所尚未登记市场 —— 两步创建的第二步：新建市场" :image-size="56" />

    <el-dialog
      v-model="dialogVisible"
      :title="editing ? `编辑市场 ${props.venue}/${editing.market}` : `在 ${props.venue} 下新建市场`"
      width="560px"
      destroy-on-close
    >
      <el-form ref="formRef" :model="form" :rules="rules" label-width="150px">
        <el-form-item label="market" prop="market">
          <el-select
            v-model="form.market"
            filterable
            allow-create
            default-first-option
            placeholder="spot / swap / future（02 §3）"
            :disabled="!!editing"
            style="width: 100%"
            @change="applyAccountType"
          >
            <el-option v-for="a in ACCOUNT_TYPES" :key="a.market" :label="a.market" :value="a.market" />
          </el-select>
          <div class="hint">选择后自动带出 02 §6 的 dig3 AccountType 映射，可手工改。</div>
        </el-form-item>

        <el-form-item label="dig3_account_type" prop="dig3_account_type">
          <el-select
            v-model="form.dig3_account_type"
            filterable
            allow-create
            default-first-option
            placeholder="必填，无默认值"
            style="width: 100%"
          >
            <el-option label="Spot" value="Spot" />
            <el-option label="FuturesCross" value="FuturesCross" />
          </el-select>
          <div class="hint">
            02 §6 映射：<code>spot → Spot</code>、<code>swap → FuturesCross</code>、<code>future → FuturesCross</code>。
            <strong>写错连不上</strong>（FuturesIsolated 永不使用）。
          </div>
        </el-form-item>

        <el-form-item label="enabled">
          <el-switch v-model="form.enabled" />
        </el-form-item>

        <el-form-item label="quota_collect 采集" prop="quota_collect">
          <el-input-number v-model="form.quota_collect" :min="0" :max="1" :step="0.05" :precision="2" />
        </el-form-item>
        <el-form-item label="quota_backfill 回填" prop="quota_backfill">
          <el-input-number v-model="form.quota_backfill" :min="0" :max="1" :step="0.05" :precision="2" />
        </el-form-item>
        <el-form-item label="quota_metadata 元数据" prop="quota_metadata">
          <el-input-number v-model="form.quota_metadata" :min="0" :max="1" :step="0.05" :precision="2" />
        </el-form-item>
        <el-form-item label="三项之和">
          <el-tag :type="quotaSum > 1 + 1e-9 ? 'danger' : 'success'" size="small" class="mono">
            {{ quotaSum.toFixed(2) }}
          </el-tag>
          <span class="hint">02 §6 缺省 0.50 / 0.35 / 0.15；按 (venue, 出口 IP) 切分，之和不得超过 1。</span>
        </el-form-item>

        <el-form-item label="egress_ip">
          <el-input v-model="form.egress_ip" placeholder="出口 IP（配额按 venue × 出口 IP 切）" />
        </el-form-item>

        <el-alert
          v-if="editing"
          type="info"
          :closable="false"
          show-icon
          title="保存将带 If-Match（该行 ETag）；他人已修改会返回 412，需刷新后重试（§5.3）。"
        />
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="submit">{{ editing ? '保存' : '创建市场' }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.markets {
  padding: 4px 6px 6px;
}

.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  margin-bottom: 8px;
  font-size: 12px;
}

.quota {
  font-size: 12px;
}

.hint {
  color: var(--ponk-text-3);
  font-size: 12px;
  line-height: 1.6;
  margin-top: 2px;
}
</style>
