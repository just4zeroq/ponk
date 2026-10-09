<script setup lang="ts">
/**
 * P2 采集节点详情（docs/13 §3.3 P2 详情字段 + §4.5）。
 *
 * - 全量 `scope_rules` / 例外清单 / `runtime_params` / `note`
 * - 运行时块（counters、started_at、lost_count、unconverged_since、last_repush_ts）
 * - 指派块（assignment_version、mode、分片数、标的数、quota_splits）
 * - **范围预览**（规则 + 例外 → 入选标的与 unresolved，不落库不下发）
 * - **导出 TOML**（§5.4）
 * - 跨页跳转（§3.5）：期望机器 → P1、运行时实例 → P7、指派 → P10
 */
import { computed, ref } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { useQuery, useQueryClient } from '@tanstack/vue-query'
import { ElMessage, ElMessageBox } from 'element-plus'
import { ArrowLeft, Download, Delete, Edit, Refresh } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import NodeFormDialog from '../components/NodeFormDialog.vue'
import {
  deleteNode,
  downloadNodeToml,
  getNode,
  getNodeLatestAssignment,
  getNodeRuntime,
  previewNodeScope,
  setNodeEnabled,
} from '../api/nodes'
import { qk } from '../query/keys'
import { useSessionStore } from '../stores/session'
import { fmtAgo, fmtTs, unresolvedParts } from '../utils/format'
import { convergenceChip, enabledChip, instanceStatusChip, UNREGISTERED_HOST_CHIP } from '../utils/status'
import type { ScopePreview } from '../api/types'

const route = useRoute()
const router = useRouter()
const session = useSessionStore()
const qc = useQueryClient()

const name = computed(() => String(route.params.name ?? ''))

const { data: detail, isFetching, isError: detailError, error: detailErr } = useQuery({
  queryKey: qk.node(name.value),
  queryFn: ({ signal }) => getNode(name.value, signal),
  refetchInterval: 30_000,
})

const { data: runtime } = useQuery({
  queryKey: qk.nodeRuntime(name.value),
  queryFn: ({ signal }) => getNodeRuntime(name.value, signal),
  refetchInterval: 5_000, // §5.6：运行时 5s
})

const { data: assignment } = useQuery({
  queryKey: qk.nodeAssignments(name.value),
  queryFn: ({ signal }) => getNodeLatestAssignment(name.value, signal),
  retry: false,
})

const node = computed(() => detail.value?.data ?? null)
const etag = computed(() => detail.value?.etag ?? null)

// ── 范围预览 ────────────────────────────────────────────────────────────────
const preview = ref<ScopePreview | null>(null)
const previewing = ref(false)
const previewError = ref('')

async function runPreview(): Promise<void> {
  previewing.value = true
  previewError.value = ''
  try {
    preview.value = await previewNodeScope(name.value)
  } catch (e) {
    previewError.value = e instanceof Error ? e.message : String(e)
    preview.value = null
  } finally {
    previewing.value = false
  }
}

const previewPage = ref(1)

function setPreviewPage(p: number): void {
  previewPage.value = p
}
const PREVIEW_PAGE_SIZE = 20
const previewSlice = computed(() => {
  const items = preview.value?.instruments ?? []
  const start = (previewPage.value - 1) * PREVIEW_PAGE_SIZE
  return items.slice(start, start + PREVIEW_PAGE_SIZE)
})
const unresolvedRows = computed(() => (preview.value?.unresolved ?? []).map(unresolvedParts))

// ── 操作 ────────────────────────────────────────────────────────────────────
const dialogVisible = ref(false)

async function toggleEnabled(): Promise<void> {
  if (!node.value) return
  const fresh = await getNode(node.value.name)
  await setNodeEnabled(node.value.name, !node.value.enabled, fresh.etag ?? '')
  ElMessage.success(node.value.enabled ? '已停用' : '已启用')
  await qc.invalidateQueries({ queryKey: ['ponk', 'node'] })
  await qc.invalidateQueries({ queryKey: ['ponk', 'nodes'] })
}

async function remove(): Promise<void> {
  if (!node.value) return
  try {
    await ElMessageBox.confirm(
      `确认删除节点 ${node.value.name}？连带删除派生计划，运行时行保留。`,
      '删除节点',
      { type: 'warning', confirmButtonText: '删除', confirmButtonClass: 'el-button--danger' },
    )
  } catch {
    return
  }
  await deleteNode(node.value.name, etag.value ?? '')
  ElMessage.success('节点已删除')
  await qc.invalidateQueries({ queryKey: ['ponk', 'nodes'] })
  await router.replace('/nodes')
}

async function exportToml(): Promise<void> {
  await downloadNodeToml(name.value)
  ElMessage.success(`已导出 ${name.value}.toml`)
}
</script>

<template>
  <div class="page" v-loading="isFetching">
    <PageHeader :title="`节点 ${name}`" page="P2">
      <template #extra>
        <StatusTag v-if="node" :chip="enabledChip(node.enabled)" />
      </template>
      <template #actions>
        <el-button :icon="ArrowLeft" @click="router.push('/nodes')">返回列表</el-button>
        <el-button :icon="Download" @click="exportToml">导出 TOML</el-button>
        <el-button v-if="session.canWrite" :icon="Refresh" @click="toggleEnabled">
          {{ node?.enabled ? '停用' : '启用' }}
        </el-button>
        <el-button v-if="session.canWrite" type="primary" :icon="Edit" @click="dialogVisible = true">编辑</el-button>
        <el-button v-if="session.canWrite" type="danger" :icon="Delete" @click="remove">删除</el-button>
      </template>
    </PageHeader>

    <el-alert
      v-if="detailError"
      type="error"
      show-icon
      :closable="false"
      style="margin-bottom: 12px"
      title="节点详情加载失败"
      :description="(detailErr as Error)?.message ?? '请求失败'"
    />

    <template v-if="node">
      <div class="grid grid-2">
        <!-- 定义 -->
        <section class="panel">
          <h3>节点定义（collector_node，12 §3.6 冷）</h3>
          <el-descriptions :column="2" size="small" border>
            <el-descriptions-item label="name">{{ node.name }}</el-descriptions-item>
            <el-descriptions-item label="id">{{ node.id }}</el-descriptions-item>
            <el-descriptions-item label="server_id / 期望机器">
              <RouterLink v-if="node.server_id" :to="`/servers`">#{{ node.server_id }}</RouterLink>
              <span v-else class="muted">未指定</span>
            </el-descriptions-item>
            <el-descriptions-item label="ETag">{{ etag ?? '（响应未返回）' }}</el-descriptions-item>
            <el-descriptions-item label="created_at">{{ fmtTs(node.created_at) }}</el-descriptions-item>
            <el-descriptions-item label="updated_at">{{ fmtTs(node.updated_at) }}</el-descriptions-item>
            <el-descriptions-item label="note" :span="2">
              <span :class="{ muted: !node.note }">{{ node.note || '—' }}</span>
            </el-descriptions-item>
          </el-descriptions>

          <h4>范围规则 scope_rules（12 §5）</h4>
          <div class="grid grid-2">
            <div class="kv">
              <div class="kv-row"><span class="muted">venues</span><span class="mono">{{ node.scope.venues?.length ? node.scope.venues.join(', ') : '全部' }}</span></div>
              <div class="kv-row"><span class="muted">markets</span><span class="mono">{{ node.scope.markets?.length ? node.scope.markets.join(', ') : '全部' }}</span></div>
              <div class="kv-row"><span class="muted">channels</span><span class="mono">{{ node.scope.channels?.join(', ') || '—' }}</span></div>
              <div class="kv-row"><span class="muted">quote</span><span class="mono">{{ node.scope.quote?.join(', ') || '不限' }}</span></div>
            </div>
            <div class="kv">
              <div class="kv-row"><span class="muted">base</span><span class="mono">{{ node.scope.base?.join(', ') || '不限' }}</span></div>
              <div class="kv-row"><span class="muted">base_exclude</span><span class="mono">{{ node.scope.base_exclude?.join(', ') || '—' }}</span></div>
              <div class="kv-row"><span class="muted">status</span><span class="mono">{{ node.scope.status?.join(', ') || '不限' }}</span></div>
              <div class="kv-row"><span class="muted">shard_size</span><span class="mono">{{ node.scope.shard_size }}</span></div>
            </div>
          </div>

          <h4>例外清单</h4>
          <div class="grid grid-2">
            <div>
              <div class="muted">scope_include（强制纳入）</div>
              <div v-if="node.scope_include?.length" class="mono list">
                <div v-for="(e, i) in node.scope_include" :key="i">{{ e }}</div>
              </div>
              <div v-else class="muted">—</div>
            </div>
            <div>
              <div class="muted">scope_exclude（排除）</div>
              <div v-if="node.scope_exclude?.length" class="mono list">
                <div v-for="(e, i) in node.scope_exclude" :key="i">{{ e }}</div>
              </div>
              <div v-else class="muted">—</div>
            </div>
          </div>

          <h4>runtime_params</h4>
          <pre class="code">{{ JSON.stringify(node.runtime_params ?? {}, null, 2) }}</pre>
        </section>

        <!-- 运行时 + 指派 -->
        <div>
          <section class="panel">
            <h3>运行时（node_runtime 热，5s 轮询）</h3>
            <template v-if="runtime?.runtime">
              <div class="flex-row" style="margin-bottom: 8px">
                <StatusTag :chip="instanceStatusChip(runtime.runtime.status)" />
                <StatusTag :chip="convergenceChip(runtime.converged, runtime.not_converged?.held_ms)" />
                <StatusTag v-if="runtime.host_registered === false" :chip="UNREGISTERED_HOST_CHIP" />
              </div>
              <div class="kv">
                <div class="kv-row"><span class="muted">instance_id</span><span class="mono">{{ runtime.runtime.instance_id }}</span></div>
                <div class="kv-row"><span class="muted">node_kind</span><span class="mono">{{ runtime.runtime.node_kind }}</span></div>
                <div class="kv-row"><span class="muted">实际上报 host</span>
                  <span class="mono">{{ runtime.runtime.host ?? '—' }}
                    <RouterLink v-if="runtime.runtime.host" :to="`/runtime?host=${encodeURIComponent(runtime.runtime.host)}`">→ P7</RouterLink>
                  </span>
                </div>
                <div class="kv-row"><span class="muted">applied_version</span><span class="mono">{{ runtime.runtime.applied_version ?? '—' }}</span></div>
                <div class="kv-row"><span class="muted">last_assignment_version</span>
                  <span class="mono">{{ runtime.runtime.last_assignment_version ?? '—' }}
                    <RouterLink :to="`/assignments?instance=${encodeURIComponent(runtime.runtime.instance_id)}`">→ P10</RouterLink>
                  </span>
                </div>
                <div class="kv-row"><span class="muted">started_at</span><span>{{ fmtTs(runtime.runtime.started_at) }}</span></div>
                <div class="kv-row"><span class="muted">last_seen_ts</span><span>{{ fmtAgo(runtime.runtime.last_seen_ts) }}</span></div>
                <div class="kv-row"><span class="muted">lost_count</span><span>{{ runtime.runtime.lost_count ?? 0 }}</span></div>
                <div class="kv-row"><span class="muted">unconverged_since</span><span>{{ fmtTs(runtime.runtime.unconverged_since) }}</span></div>
                <div class="kv-row"><span class="muted">last_repush_ts</span><span>{{ fmtTs(runtime.runtime.last_repush_ts) }}</span></div>
              </div>
              <h4>counters</h4>
              <pre class="code">{{ JSON.stringify(runtime.runtime.counters ?? {}, null, 2) }}</pre>
            </template>
            <el-empty v-else description="尚无运行时实例（该节点未 register 或已被清理）" :image-size="70" />
          </section>

          <section class="panel">
            <h3>最新指派（node_assignment 派生）</h3>
            <template v-if="assignment">
              <div class="kv">
                <div class="kv-row"><span class="muted">assignment_version</span><span class="mono">{{ assignment.assignment_version ?? '—' }}</span></div>
                <div class="kv-row"><span class="muted">mode</span><span class="mono">{{ assignment.mode ?? '—' }}</span></div>
                <div class="kv-row"><span class="muted">generated_at</span><span>{{ fmtTs(assignment.generated_at) }}</span></div>
                <div class="kv-row"><span class="muted">分片数 / 标的数</span>
                  <span class="mono">{{ assignment.shard_count ?? '—' }} / {{ assignment.instrument_count ?? '—' }}</span>
                </div>
              </div>
              <div v-if="assignment.quota_splits?.length">
                <h4>quota_splits（逐所分摊）</h4>
                <pre class="code">{{ JSON.stringify(assignment.quota_splits, null, 2) }}</pre>
              </div>
            </template>
            <el-empty v-else description="暂无指派记录" :image-size="70" />
          </section>
        </div>
      </div>

      <!-- 范围预览 -->
      <section class="panel">
        <div class="flex-row" style="justify-content: space-between">
          <h3 style="margin: 0">范围预览（§4.5 scope/preview）</h3>
          <el-button type="primary" :loading="previewing" @click="runPreview">计算预览</el-button>
        </div>
        <el-alert
          type="warning"
          :closable="false"
          show-icon
          style="margin: 10px 0"
          title="预览 ≠ 已下发（docs/13 §7.2 R2）：scope::resolve 尚未接入 recompute_inner，实际下发计划以 SubscribePlan 为准。"
        />
        <div v-if="previewError" class="json-error">{{ previewError }}</div>

        <template v-if="preview">
          <div class="grid grid-4" style="margin-bottom: 12px">
            <div class="stat">
              <div class="stat-label">入选标的</div>
              <div class="stat-value">{{ preview.counts?.instruments ?? preview.instruments.length }}</div>
            </div>
            <div class="stat">
              <div class="stat-label">分片（估算）</div>
              <div class="stat-value">{{ preview.counts?.shards ?? '—' }}</div>
            </div>
            <div class="stat">
              <div class="stat-label">未解析例外</div>
              <div class="stat-value" :class="{ warn: unresolvedRows.length }">{{ unresolvedRows.length }}</div>
            </div>
            <div class="stat">
              <div class="stat-label">说明</div>
              <div class="stat-sub" style="padding-top: 6px">预览不落库、不下发（§3.3 P2）</div>
            </div>
          </div>

          <div v-if="unresolvedRows.length" class="panel warn-panel">
            <h3>未解析例外（黄：如实汇报，不静默丢 · 12 §5）</h3>
            <el-table :data="unresolvedRows" size="small">
              <el-table-column label="条目" prop="entry" min-width="240" />
              <el-table-column label="原因" prop="reason" min-width="260" />
            </el-table>
          </div>

          <h4>入选标的（前 {{ preview.instruments.length }} 条）</h4>
          <el-table :data="previewSlice" size="small" max-height="360">
            <el-table-column label="canonical" min-width="220">
              <template #default="{ row }">
                <span class="mono">{{ row.venue }}:{{ row.market }}:{{ row.native_code }}</span>
              </template>
            </el-table-column>
            <el-table-column label="status" width="100">
              <template #default="{ row }"><span class="mono">{{ row.status ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="price_prec" width="100">
              <template #default="{ row }"><span class="mono">{{ row.price_precision ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="qty_prec" width="100">
              <template #default="{ row }"><span class="mono">{{ row.qty_precision ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="tick / step" width="170">
              <template #default="{ row }">
                <span class="mono">{{ row.tick_size ?? '—' }} / {{ row.step_size ?? '—' }}</span>
              </template>
            </el-table-column>
          </el-table>
          <div class="table-footer">
            <el-pagination
              v-if="preview.instruments.length > PREVIEW_PAGE_SIZE"
              background
              layout="prev, pager, next"
              :total="preview.instruments.length"
              :page-size="PREVIEW_PAGE_SIZE"
              :current-page="previewPage"
              @current-change="setPreviewPage"
            />
          </div>
        </template>
      </section>
    </template>

    <NodeFormDialog v-model:visible="dialogVisible" :node="node" />
  </div>
</template>

<style scoped>
h4 {
  margin: 14px 0 6px;
  font-size: 13px;
  font-weight: 600;
}

.kv-row {
  display: flex;
  justify-content: space-between;
  gap: 10px;
  padding: 5px 0;
  border-bottom: 1px dashed var(--ponk-border);
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
  max-height: 260px;
  margin: 6px 0 0;
}

.list {
  font-size: 12px;
  line-height: 1.7;
  max-height: 140px;
  overflow: auto;
  border: 1px solid var(--ponk-border);
  border-radius: 4px;
  padding: 6px 8px;
}

.warn-panel {
  border-color: var(--el-color-warning-light-5);
  background: #fffbf5;
}
</style>
