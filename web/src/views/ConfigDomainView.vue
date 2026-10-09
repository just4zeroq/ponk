<script setup lang="ts">
/**
 * P6 配置域详情（docs/13 §3.3 P6 + §4.9 + §5.2 发布流程 + §5.3 并发 + §5.4 导入导出）。
 *
 * 四个标签页：
 * - 编辑与发布：JSON 编辑器 → `validate`（错误定位 JSON 路径）→ `publish {content, base_version, reason}`
 * - 版本历史：版本降序 + diff；「回滚到此版本」= 发布为目标内容为**新版本**（二次确认展示 vN+1）
 * - 审计：该域 `audit_log` 时间轴
 * - 收敛进度：发布后 2s 轮询 `GET /config/{domain}/convergence`，全部收敛结束（§5.6）
 */
import { computed, ref, watch } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/vue-query'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Download, Upload, ArrowLeft } from '@element-plus/icons-vue'
import PageHeader from '../components/PageHeader.vue'
import StatusTag from '../components/StatusTag.vue'
import JsonEditor from '../components/JsonEditor.vue'
import PagerBar from '../components/PagerBar.vue'
import {
  exportDomainJson,
  getConfigDomain,
  getDomainAudit,
  getDomainConvergence,
  listConfigVersions,
  publishDomain,
  rollbackDomain,
  validateDomain,
} from '../api/config'
import { qk } from '../query/keys'
import { useSessionStore } from '../stores/session'
import { PUSH_TEXT, domainLocal } from '../utils/domains'
import { fmtDuration, fmtTs, prettyJson } from '../utils/format'
import { convergenceChip, domainVersionChip } from '../utils/status'
import { ApiError } from '../api/http'
import type { PublishResult } from '../api/types'

const route = useRoute()
const router = useRouter()
const session = useSessionStore()
const qc = useQueryClient()

const domain = computed(() => String(route.params.domain ?? ''))
const local = computed(() => domainLocal(domain.value))

/** 标签页（支持 `?tab=versions|audit|convergence` 深链）。 */
const tab = ref<string>(String(route.query.tab ?? 'edit'))
watch(
  () => route.query.tab,
  (v) => {
    if (v && typeof v === 'string') tab.value = v
  },
)
function setTab(v: string): void {
  tab.value = v
  void router.replace({ query: { ...route.query, tab: v === 'edit' ? undefined : v } })
}

// ── 当前文档 ────────────────────────────────────────────────────────────────
const { data: current, isFetching, isError: docError, error: docErr } = useQuery({
  queryKey: qk.configDomain(domain.value),
  queryFn: ({ signal }) => getConfigDomain(domain.value, signal),
  refetchInterval: 30_000,
})

const doc = computed(() => current.value?.data ?? null)
const etag = computed(() => current.value?.etag ?? doc.value?.etag ?? null)
/** 导出信封校验用的 content_hash 前 12 位（§5.4）。 */
const hashShort = computed(() => (doc.value?.content_hash ? doc.value.content_hash.slice(0, 12) : ''))
const baseVersion = computed(() => doc.value?.version ?? 0)

/** 后端校验错误（`POST /validate` → `detail.path` 定位 JSON 路径）。 */
const serverError = ref<{ path?: string; message: string } | null>(null)

// 路由参数变化（/config/a → /config/b）复用同一组件：重置编辑态并重取。
watch(domain, async () => {
  dirty.value = false
  validated.value = false
  serverError.value = null
  conflictNote.value = ''
  content.value = ''
  versionPage.value = { limit: 50, offset: 0 }
  await qc.invalidateQueries({ queryKey: ['ponk', 'config'] })
})

// ── 编辑器状态 ──────────────────────────────────────────────────────────────
const content = ref('')
const reason = ref('')
const dirty = ref(false)
const validated = ref(false)

watch(
  doc,
  (d) => {
    if (d && !dirty.value) {
      content.value = prettyJson(d.content)
      validated.value = false
    }
  },
  { immediate: true },
)

function onContentChange(v: string): void {
  content.value = v
  dirty.value = true
  validated.value = false
}

/** JSON 是否可解析。 */
const parseFailed = computed(() => {
  try {
    JSON.parse(content.value || '{}')
    return false
  } catch {
    return true
  }
})

function parsedContent(): unknown {
  return JSON.parse(content.value || '{}')
}

// ── 校验 ────────────────────────────────────────────────────────────────────
const validate = useMutation({
  mutationFn: () => validateDomain(domain.value, parsedContent()),
  onSuccess: () => {
    validated.value = true
    serverError.value = null
    ElMessage.success('校验通过（未落库）')
  },
  onError: (e) => {
    validated.value = false
    const detail = (e as ApiError).detail as { path?: string } | undefined
    serverError.value = { path: detail?.path, message: e.message }
  },
})

function runValidate(): void {
  serverError.value = null
  if (parseFailed.value) {
    serverError.value = { message: 'JSON 语法错误，请先修正' }
    return
  }
  validate.mutate()
}

// ── 发布 ────────────────────────────────────────────────────────────────────
const publishResult = ref<PublishResult | null>(null)
const resultVisible = ref(false)
const conflictNote = ref('')
const publishedAt = ref(0)

const publish = useMutation({
  mutationFn: () =>
    publishDomain(domain.value, {
      content: parsedContent(),
      base_version: baseVersion.value,
      reason: reason.value.trim() || (baseVersion.value === 0 ? 'initial publish' : ''),
    }),
  onSuccess: async (res) => {
    publishResult.value = res
    resultVisible.value = true
    conflictNote.value = ''
    dirty.value = false
    validated.value = false
    publishedAt.value = Date.now()
    reason.value = ''
    ElMessage.success(`已发布 v${res.doc.version}（notified ${res.notified}）`)
    await qc.invalidateQueries({ queryKey: ['ponk', 'config'] })
    await qc.invalidateQueries({ queryKey: ['ponk', 'configDomains'] })
    await qc.invalidateQueries({ queryKey: ['ponk', 'overview'] })
    await qc.invalidateQueries({ queryKey: ['ponk', 'audit'] })
    setTab('convergence')
  },
  onError: (e) => {
    const err = e as ApiError
    if (err.isConflict) {
      conflictNote.value = `版本基线冲突（409）：${err.message}。你基于 base_version=${baseVersion.value} 提交，服务端当前版本已变化——已重新拉取最新文档，请比对后决定是否重新编辑发布（§5.3）。`
      // 重取当前文档，让 base_version 与 ETag 对齐服务端最新版本
      void qc.invalidateQueries({ queryKey: qk.configDomain(domain.value) })
    }
  },
})

function runPublish(): void {
  if (parseFailed.value) {
    ElMessage.error('JSON 语法错误，无法发布')
    return
  }
  void publish.mutate()
}

// ── 版本历史 ────────────────────────────────────────────────────────────────
const versionPage = ref({ limit: 50, offset: 0 })

const { data: versions, isFetching: loadingVersions, refetch: refetchVersions } = useQuery({
  queryKey: qk.configVersions(domain.value, versionPage.value.offset),
  queryFn: ({ signal }) =>
    listConfigVersions(domain.value, { limit: versionPage.value.limit, offset: versionPage.value.offset }, signal),
  enabled: computed(() => tab.value === 'versions'),
  retry: false,
})

const rollbackBusy = ref(false)

async function runRollback(target: number): Promise<void> {
  try {
    await ElMessageBox.confirm(
      `回滚到 v${target}：将以 v${target} 的内容**发布为 v${baseVersion.value + 1}**（版本只前进，current 不倒回，§5.2）。reason = "rollback to v${target}"。`,
      `回滚 → 将发布为 v${baseVersion.value + 1}`,
      { type: 'warning', confirmButtonText: '确认回滚', confirmButtonClass: 'el-button--warning' },
    )
  } catch {
    return
  }
  rollbackBusy.value = true
  try {
    const res = await rollbackDomain(domain.value, { target_version: target, base_version: baseVersion.value })
    publishResult.value = res
    resultVisible.value = true
    publishedAt.value = Date.now()
    ElMessage.success(`已回滚：发布为 v${res.doc.version}`)
    await qc.invalidateQueries({ queryKey: ['ponk', 'config'] })
    await qc.invalidateQueries({ queryKey: ['ponk', 'configDomains'] })
    await qc.invalidateQueries({ queryKey: ['ponk', 'audit'] })
    setTab('convergence')
  } catch (e) {
    const err = e as ApiError
    if (err.isConflict) ElMessage.error(`基线冲突（409）：${err.message}`)
  } finally {
    rollbackBusy.value = false
  }
}

function onVersionPage(n: { limit: number; offset: number }): void {
  versionPage.value = n
  void refetchVersions()
}

// ── 审计 ────────────────────────────────────────────────────────────────────
const { data: audit, isFetching: loadingAudit } = useQuery({
  queryKey: qk.configAudit(domain.value),
  queryFn: ({ signal }) => getDomainAudit(domain.value, signal),
  enabled: computed(() => tab.value === 'audit'),
  retry: false,
})

// ── 收敛进度 ────────────────────────────────────────────────────────────────
// §5.6：发布后 60s 内 2s 轮询；平时 5s。
const { data: convergence, isFetching: loadingConvergence } = useQuery({
  queryKey: qk.configConvergence(domain.value),
  queryFn: ({ signal }) => getDomainConvergence(domain.value, signal),
  enabled: computed(() => tab.value === 'convergence'),
  refetchInterval: () => (Date.now() - publishedAt.value < 60_000 ? 2_000 : 5_000),
  retry: false,
})

const allConverged = computed(
  () => !!convergence.value && (convergence.value.not_converged?.length ?? 0) === 0,
)

// ── 导出 / 导入 ─────────────────────────────────────────────────────────────
async function exportJson(): Promise<void> {
  await exportDomainJson(domain.value)
  ElMessage.success(`已导出 ${domain.value}-v${baseVersion.value}.json（§5.4 信封含 content_hash）`)
}

const fileInput = ref<HTMLInputElement>()
function pickImportFile(): void {
  fileInput.value?.click()
}
function onImportFile(e: Event): void {
  const input = e.target as HTMLInputElement
  const file = input.files?.[0]
  if (!file) return
  const reader = new FileReader()
  reader.onload = () => {
    const text = String(reader.result ?? '')
    // 支持导出信封 {domain,version,publisher,ts,content} 或裸文档
    let payload: unknown = text
    try {
      const parsed = JSON.parse(text)
      if (parsed && typeof parsed === 'object' && 'content' in (parsed as object)) {
        payload = (parsed as { content: unknown }).content
      }
    } catch {
      /* 原样交给编辑器显示语法错误 */
    }
    content.value = typeof payload === 'string' ? payload : prettyJson(payload)
    dirty.value = true
    validated.value = false
    ElMessage.success('已载入编辑器；导入即发布需点击「发布」（§5.4）')
  }
  reader.readAsText(file)
  input.value = ''
}
</script>

<template>
  <div class="page">
    <PageHeader :title="`配置域 · ${domain}`" page="P6">
      <template #extra>
        <StatusTag v-if="doc" :chip="domainVersionChip(doc.version)" />
        <el-tag v-if="local" size="small" effect="plain" type="info">{{ local.display }}</el-tag>
      </template>
      <template #actions>
        <el-button :icon="ArrowLeft" @click="router.push('/config')">返回八域</el-button>
        <el-button :icon="Download" @click="exportJson">导出 JSON</el-button>
        <el-button v-if="session.canWrite" :icon="Upload" @click="pickImportFile">导入 JSON</el-button>
        <input ref="fileInput" type="file" accept="application/json,.json" style="display: none" @change="onImportFile" />
      </template>
    </PageHeader>

    <div v-if="local" class="flex-row meta-row">
      <el-tag size="small" :type="local.push === 'C' ? 'info' : local.push === 'B' ? 'warning' : 'primary'">
        传播 {{ local.push }}
      </el-tag>
      <span class="muted">{{ PUSH_TEXT[local.push] }}</span>
      <span class="muted">· 消费者：{{ local.consumers.join('；') }}</span>
      <span v-if="local.recompute" class="muted">· 发布触发指派重算（09 §5.4）</span>
      <RouterLink :to="`/runtime?domain=${domain}`" class="muted">· 查看消费实例 → P7</RouterLink>
    </div>

    <el-alert
      v-if="domain === 'process_capacity'"
      type="info"
      :closable="false"
      show-icon
      style="margin-bottom: 12px"
      title="本域不发变更通知，效果以 assignment_version 变化体现（09 §2.2 C）。"
    />

    <el-alert
      v-if="conflictNote"
      type="warning"
      :closable="true"
      show-icon
      style="margin-bottom: 12px"
      :title="conflictNote"
      @close="conflictNote = ''"
    />

    <el-alert
      v-if="docError"
      type="error"
      show-icon
      :closable="false"
      style="margin-bottom: 12px"
      title="当前域文档加载失败"
      :description="(docErr as Error)?.message ?? '请求失败'"
    />

    <el-tabs v-model="tab" class="tabs" @tab-change="setTab">
      <!-- 编辑与发布 -->
      <el-tab-pane label="编辑与发布" name="edit">
        <section class="panel" v-loading="isFetching">
          <div class="flex-row between">
            <div class="muted">
              当前 <span class="mono">v{{ doc?.version ?? '—' }}</span>
              · base_version <span class="mono">{{ baseVersion }}</span>
              · publisher <span class="mono">{{ doc?.publisher || '—' }}</span>
              · ts {{ fmtTs(doc?.ts) }}
              <template v-if="hashShort"> · hash <span class="mono">{{ hashShort }}</span></template>
              <template v-if="etag"> · ETag <span class="mono">{{ etag }}</span></template>
            </div>
            <div class="flex-row">
              <el-button size="small" :loading="validate.isPending.value" @click="runValidate">校验（不落库）</el-button>
              <el-button
                v-if="session.canWrite"
                size="small"
                type="primary"
                :loading="publish.isPending.value"
                :disabled="parseFailed || !dirty"
                @click="runPublish"
              >
                发布（版本 +1）
              </el-button>
            </div>
          </div>

          <div v-if="!dirty && doc" class="muted" style="margin: 8px 0">
            已加载当前文档；修改后可校验并发布。域是唯一版本单位：同域内单条规则未变也随域发布获得新版本（09 §4.1）。
          </div>

          <JsonEditor
            :model-value="content"
            :error-path="serverError?.path ?? null"
            :error-message="serverError?.message ?? null"
            :readonly="!session.canWrite"
            @update:model-value="onContentChange"
          />

          <div class="flex-row" style="margin-top: 12px">
            <el-input
              v-model="reason"
              placeholder="原因 / 工单号（09 §4.5；回滚会自动写 rollback to vN）"
              style="flex: 1"
              :disabled="!session.canWrite"
            />
            <el-tag v-if="validated && dirty === false" type="success" size="small">已校验</el-tag>
            <span class="muted">base_version = {{ baseVersion }}</span>
          </div>
        </section>
      </el-tab-pane>

      <!-- 版本历史 -->
      <el-tab-pane label="版本历史" name="versions">
        <section class="panel" v-loading="loadingVersions">
          <el-table :data="versions?.items ?? []" size="small">
            <el-table-column label="version" width="90" prop="version" />
            <el-table-column label="published_at" width="160">
              <template #default="{ row }">{{ fmtTs(row.published_at ?? row.ts) }}</template>
            </el-table-column>
            <el-table-column label="publisher" min-width="140" prop="publisher" />
            <el-table-column label="prev" width="70">
              <template #default="{ row }"><span class="mono">{{ row.prev_version ?? '—' }}</span></template>
            </el-table-column>
            <el-table-column label="reason" min-width="160">
              <template #default="{ row }"><span class="muted">{{ row.reason || '—' }}</span></template>
            </el-table-column>
            <el-table-column label="content_hash" width="130">
              <template #default="{ row }">
                <span class="mono muted">{{ row.content_hash ? row.content_hash.slice(0, 10) : '—' }}</span>
              </template>
            </el-table-column>
            <el-table-column label="diff" min-width="260">
              <template #default="{ row }">
                <div v-if="row.diff?.length" class="muted diff">
                  <div v-for="(d, i) in row.diff.slice(0, 6)" :key="i" class="mono">{{ d }}</div>
                  <div v-if="row.diff.length > 6" class="muted">… 共 {{ row.diff.length }} 项</div>
                </div>
                <span v-else class="muted">—</span>
              </template>
            </el-table-column>
            <el-table-column label="操作" width="140" fixed="right">
              <template #default="{ row }">
                <el-button
                  v-if="session.canWrite && row.version !== doc?.version"
                  link
                  type="warning"
                  size="small"
                  :loading="rollbackBusy"
                  @click="runRollback(row.version)"
                >回滚到此版本</el-button>
              </template>
            </el-table-column>
          </el-table>

          <div v-if="doc?.version === 0" class="muted" style="margin-top: 10px">
            该域未发布（current_version = 0），不计入版本历史（§3.4）。
          </div>

          <div class="table-footer">
            <PagerBar
              :total="versions?.total ?? 0"
              :limit="versionPage.limit"
              :offset="versionPage.offset"
              @change="onVersionPage"
            />
          </div>
        </section>
      </el-tab-pane>

      <!-- 审计 -->
      <el-tab-pane label="审计" name="audit">
        <section class="panel" v-loading="loadingAudit">
          <el-timeline v-if="audit?.length">
            <el-timeline-item
              v-for="(a, i) in audit"
              :key="a.id ?? i"
              :timestamp="fmtTs(a.ts)"
              :type="a.action === 'rollback' ? 'warning' : 'primary'"
              placement="top"
            >
              <div class="flex-row">
                <el-tag size="small" :type="a.action === 'rollback' ? 'warning' : 'primary'">
                  {{ a.action ?? 'publish' }}
                </el-tag>
                <span class="mono">v{{ a.version }}</span>
                <span class="muted">← {{ a.prev_version ?? '—' }}</span>
                <span class="mono">{{ a.publisher }}</span>
              </div>
              <div class="muted">{{ a.reason || '—' }}</div>
              <div v-if="a.diff?.length" class="diff mono muted">
                <div v-for="(d, j) in a.diff.slice(0, 8)" :key="j">{{ d }}</div>
              </div>
            </el-timeline-item>
          </el-timeline>
          <el-empty v-else-if="!loadingAudit" description="该域暂无审计条目" :image-size="70" />
          <div class="muted" style="margin-top: 10px">
            本页为域级审计（<span class="mono">GET /config/{domain}/audit</span>，仅 publish / rollback，§6.1）；
            全表审计见 P9（本次未实现）。
          </div>
        </section>
      </el-tab-pane>

      <!-- 收敛进度 -->
      <el-tab-pane label="收敛进度" name="convergence">
        <section class="panel" v-loading="loadingConvergence">
          <div class="flex-row" style="justify-content: space-between; margin-bottom: 10px">
            <div class="flex-row">
              <el-tag v-if="allConverged" type="success" size="small">全部收敛</el-tag>
              <el-tag v-else type="warning" size="small">收敛中</el-tag>
              <span class="muted">已收敛 {{ convergence?.converged ?? 0 }} · 未收敛 {{ convergence?.not_converged?.length ?? 0 }}</span>
            </div>
            <span class="muted">发布后 60s 内 2s 轮询，平时 5s（§5.6）</span>
          </div>

          <el-table :data="convergence?.not_converged ?? []" size="small" empty-text="全部实例已收敛">
            <el-table-column label="实例" min-width="150">
              <template #default="{ row }">
                <RouterLink :to="`/runtime?instance=${encodeURIComponent(row.instance_id)}`">{{ row.instance_id }}</RouterLink>
              </template>
            </el-table-column>
            <el-table-column label="域" width="160">
              <template #default="{ row }"><span class="mono muted">{{ row.domain ?? '指派版本' }}</span></template>
            </el-table-column>
            <el-table-column label="expected" width="100">
              <template #default="{ row }"><span class="mono">{{ row.expected }}</span></template>
            </el-table-column>
            <el-table-column label="actual" width="100">
              <template #default="{ row }"><span class="mono">{{ row.actual }}</span></template>
            </el-table-column>
            <el-table-column label="held" width="160">
              <template #default="{ row }">
                <StatusTag :chip="convergenceChip(false, row.held_ms)" />
                <span class="muted"> {{ fmtDuration(row.held_ms) }}</span>
              </template>
            </el-table-column>
          </el-table>

          <el-alert
            v-if="convergence && convergence.not_converged?.some((n) => n.held_ms >= 30000)"
            type="error"
            :closable="false"
            show-icon
            style="margin-top: 12px"
            title="存在持续 ≥30s 的未收敛实例：控制面已重推并计入 ops.control，告警由 06 产出（09 §4.4 / §6.5）。"
          />
          <div class="muted" style="margin-top: 10px">
            <RouterLink :to="`/runtime?domain=${domain}`">查看该域全部消费实例 → 实例状态（P7）</RouterLink>
          </div>
        </section>
      </el-tab-pane>
    </el-tabs>

    <!-- 发布结果 -->
    <el-dialog v-model="resultVisible" title="发布结果" width="640px">
      <template v-if="publishResult">
        <el-descriptions :column="1" size="small" border>
          <el-descriptions-item label="新版本">v{{ publishResult.doc.version }}（prev {{ publishResult.doc.prev_version ?? '—' }}）</el-descriptions-item>
          <el-descriptions-item label="publisher">{{ publishResult.doc.publisher }}</el-descriptions-item>
          <el-descriptions-item label="reason">{{ publishResult.doc.reason || '—' }}</el-descriptions-item>
          <el-descriptions-item label="notified">{{ publishResult.notified }}（出站消息条数）</el-descriptions-item>
          <el-descriptions-item label="recompute">{{ publishResult.recompute ? '是' : '否' }}</el-descriptions-item>
        </el-descriptions>
        <h4>diff</h4>
        <div class="diff mono">
          <div v-for="(d, i) in publishResult.diff ?? []" :key="i">{{ d }}</div>
          <div v-if="!publishResult.diff?.length" class="muted">（无结构变化，仍产生新版本）</div>
        </div>
        <el-alert
          type="info"
          :closable="false"
          show-icon
          style="margin-top: 12px"
          title="已发布 ≠ 已收敛：请在「收敛进度」标签页等待实例心跳比对完成（§5.2）。"
        />
      </template>
      <template #footer>
        <el-button @click="resultVisible = false">关闭</el-button>
        <el-button type="primary" @click="((resultVisible = false), setTab('convergence'))">查看收敛进度</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.meta-row {
  margin-bottom: 12px;
  font-size: 13px;
}

.between {
  justify-content: space-between;
}

h4 {
  margin: 12px 0 6px;
  font-size: 13px;
}

.diff {
  font-size: 12px;
  line-height: 1.7;
  color: var(--ponk-text-2);
}

.diff div {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
