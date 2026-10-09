<script setup lang="ts">
/**
 * 节点新建 / 编辑表单（docs/13 §3.3 P2 + §4.5）。
 *
 * 范围规则（`ScopeRules`，12 §5）+ 例外清单（`scope_include` / `scope_exclude`，一行一条）
 * + `runtime_params`（JSON）+ 期望机器（`server_id`）+ 启停。
 * 写入走 `POST /nodes` / `PUT /nodes/{name}`（后者带 `If-Match`）。
 */
import { computed, reactive, ref, watch } from 'vue'
import { useMutation, useQuery, useQueryClient } from '@tanstack/vue-query'
import { ElMessage, type FormInstance } from 'element-plus'
import { createNode, getNode, updateNode } from '../api/nodes'
import { listServers } from '../api/servers'
import { qk } from '../query/keys'
import type { NodeInput, NodeRecord, ScopeRules } from '../api/types'

const props = defineProps<{
  visible: boolean
  /** 编辑目标；null = 新建。 */
  node?: NodeRecord | null
}>()

const emit = defineEmits<{
  'update:visible': [value: boolean]
  saved: [name: string]
}>()

const qc = useQueryClient()
const formRef = ref<FormInstance>()
const saving = ref(false)
const error = ref('')

const blankScope = (): ScopeRules => ({
  venues: [],
  markets: [],
  channels: ['candle_1m', 'trade', 'ticker', 'l1', 'funding'],
  quote: [],
  base: [],
  base_exclude: [],
  status: ['trading'],
  shard_size: 200,
})

const form = reactive({
  name: '',
  enabled: true,
  server_id: null as number | null,
  scope: blankScope(),
  scope_include: '',
  scope_exclude: '',
  runtime_params: '{}',
  note: '',
})

/** 编辑目标存在即为编辑态。 */
const editing = computed(() => !!props.node)

const rules = {
  name: [
    { required: true, message: 'name 必填（= instance_id，唯一标识）', trigger: 'blur' },
    { pattern: /^[A-Za-z0-9._-]+$/, message: '仅允许字母数字与 . _ -', trigger: 'blur' },
  ],
}

const { data: servers } = useQuery({
  queryKey: qk.servers({ limit: 500, offset: 0 }),
  queryFn: ({ signal }) => listServers({ limit: 500, offset: 0 }, signal),
  staleTime: 60_000,
  retry: false,
})

watch(
  () => props.visible,
  async (open) => {
    if (!open) return
    error.value = ''
    form.scope = blankScope()
    form.scope_include = ''
    form.scope_exclude = ''
    form.runtime_params = '{}'
    form.note = ''
    form.server_id = null
    form.enabled = true
    if (props.node) {
      form.name = props.node.name
      form.enabled = props.node.enabled
      form.server_id = props.node.server_id
      form.scope = { ...blankScope(), ...props.node.scope }
      form.scope_include = (props.node.scope_include ?? []).join('\n')
      form.scope_exclude = (props.node.scope_exclude ?? []).join('\n')
      form.runtime_params = JSON.stringify(props.node.runtime_params ?? {}, null, 2)
      form.note = props.node.note ?? ''
    } else {
      form.name = ''
    }
    // 编辑时以服务端最新 ETag 版本为准（列表可能滞后）
    if (props.node) {
      try {
        const { data } = await getNode(props.node.name)
        if (data) {
          form.scope = { ...blankScope(), ...data.scope }
          form.scope_include = (data.scope_include ?? []).join('\n')
          form.scope_exclude = (data.scope_exclude ?? []).join('\n')
          form.runtime_params = JSON.stringify(data.runtime_params ?? {}, null, 2)
          form.note = data.note ?? ''
          form.server_id = data.server_id
          form.enabled = data.enabled
        }
      } catch {
        /* 读取失败时用列表数据兜底 */
      }
    }
  },
)

const save = useMutation({
  mutationFn: async (_vars: { name: string }) => {
    let runtimeParams: unknown
    try {
      runtimeParams = form.runtime_params.trim() ? JSON.parse(form.runtime_params) : {}
    } catch (e) {
      throw new Error(`runtime_params 不是合法 JSON：${e instanceof Error ? e.message : String(e)}`)
    }
    const body: NodeInput = {
      name: form.name.trim(),
      enabled: form.enabled,
      server_id: form.server_id,
      scope: {
        ...form.scope,
        shard_size: Number(form.scope.shard_size) || 0,
        venues: [...form.scope.venues],
        markets: [...form.scope.markets],
        channels: [...form.scope.channels],
        quote: [...form.scope.quote],
        base: [...form.scope.base],
        base_exclude: [...form.scope.base_exclude],
        status: [...form.scope.status],
      },
      scope_include: form.scope_include.split('\n').map((s) => s.trim()).filter(Boolean),
      scope_exclude: form.scope_exclude.split('\n').map((s) => s.trim()).filter(Boolean),
      runtime_params: runtimeParams,
      note: form.note.trim() || null,
    }
    if (props.node) {
      // §5.3：先取最新 ETag 再写，避免覆盖他人的范围规则。
      const { etag } = await getNode(props.node.name)
      return updateNode(props.node.name, body, etag ?? '')
    }
    return createNode(body)
  },
  onSuccess: async (_data, vars) => {
    ElMessage.success(props.node ? '节点已更新' : '节点已创建')
    emit('update:visible', false)
    await qc.invalidateQueries({ queryKey: ['ponk', 'nodes'] })
    await qc.invalidateQueries({ queryKey: ['ponk', 'node'] })
    emit('saved', vars.name)
  },
  onError: (e) => {
    error.value = e instanceof Error ? e.message : String(e)
  },
})

async function submit(): Promise<void> {
  error.value = ''
  try {
    await formRef.value?.validate()
  } catch {
    return
  }
  if (!form.scope.shard_size || Number(form.scope.shard_size) < 1) {
    error.value = 'scope.shard_size 必须 >= 1（09 §5.1）'
    return
  }
  try {
    await save.mutateAsync({ name: form.name.trim() })
  } catch {
    /* 错误已由 onError 写入 error，同时被 HTTP 层全局提示 */
  }
}
</script>

<template>
  <el-dialog
    :model-value="visible"
    :title="editing ? `编辑节点 ${node?.name}` : '新建采集节点'"
    width="760px"
    destroy-on-close
    @update:model-value="(v: boolean) => emit('update:visible', v)"
  >
    <el-form ref="formRef" :model="form" :rules="rules" label-width="130px">
      <el-form-item label="name" prop="name">
        <el-input v-model="form.name" placeholder="= instance_id，如 col-binance" :disabled="editing" />
      </el-form-item>

      <el-form-item label="启用">
        <el-switch v-model="form.enabled" />
        <span class="hint">停用 = 不再接收新计划；已有计划不自动撤回（09 §7）。</span>
      </el-form-item>

      <el-form-item label="期望机器 server_id">
        <el-select v-model="form.server_id" clearable filterable placeholder="选择服务器台账条目" style="width: 100%">
          <el-option v-for="s in servers?.items ?? []" :key="s.id" :label="`${s.host}（#${s.id}）`" :value="s.id" />
        </el-select>
        <span class="hint">实际运行机器由 register 上报（`node_runtime.host`），与期望机器关联但不强校验。</span>
      </el-form-item>

      <el-divider content-position="left">范围规则 scope（12 §5）</el-divider>

      <el-form-item label="venues 交易所">
        <el-select v-model="form.scope.venues" multiple filterable allow-create default-first-option style="width: 100%" placeholder="空 = 全部交易所">
          <el-option v-for="v in ['binance', 'okx', 'bybit', 'bitget']" :key="v" :label="v" :value="v" />
        </el-select>
      </el-form-item>

      <el-form-item label="markets 市场">
        <el-select v-model="form.scope.markets" multiple filterable allow-create default-first-option style="width: 100%" placeholder="空 = 全部市场">
          <el-option v-for="m in ['spot', 'swap', 'future']" :key="m" :label="m" :value="m" />
        </el-select>
      </el-form-item>

      <el-form-item label="channels 通道">
        <el-select v-model="form.scope.channels" multiple filterable allow-create default-first-option style="width: 100%">
          <el-option v-for="c in ['candle_1m', 'trade', 'ticker', 'l1', 'funding']" :key="c" :label="c" :value="c" />
        </el-select>
      </el-form-item>

      <el-form-item label="quote 计价币">
        <el-select v-model="form.scope.quote" multiple filterable allow-create default-first-option style="width: 100%" placeholder="空 = 不限">
          <el-option v-for="q in ['USDT', 'USDC', 'USD']" :key="q" :label="q" :value="q" />
        </el-select>
      </el-form-item>

      <el-row :gutter="12">
        <el-col :span="12">
          <el-form-item label="base 白名单">
            <el-select v-model="form.scope.base" multiple filterable allow-create default-first-option style="width: 100%" placeholder="空 = 不限">
              <el-option v-for="b in ['BTC', 'ETH', 'SOL']" :key="b" :label="b" :value="b" />
            </el-select>
          </el-form-item>
        </el-col>
        <el-col :span="12">
          <el-form-item label="base_exclude">
            <el-select v-model="form.scope.base_exclude" multiple filterable allow-create default-first-option style="width: 100%">
              <el-option v-for="b in ['BTC', 'ETH', 'SOL']" :key="b" :label="b" :value="b" />
            </el-select>
          </el-form-item>
        </el-col>
      </el-row>

      <el-row :gutter="12">
        <el-col :span="12">
          <el-form-item label="status 状态">
            <el-select v-model="form.scope.status" multiple filterable allow-create default-first-option style="width: 100%">
              <el-option v-for="st in ['trading', 'break', 'delisted']" :key="st" :label="st" :value="st" />
            </el-select>
          </el-form-item>
        </el-col>
        <el-col :span="12">
          <el-form-item label="shard_size" required>
            <el-input-number v-model="form.scope.shard_size" :min="1" :max="100000" style="width: 100%" />
            <span class="hint">标的数/分片；必须 >= 1（09 §5.1）</span>
          </el-form-item>
        </el-col>
      </el-row>

      <el-divider content-position="left">例外清单（一行一条）</el-divider>

      <el-form-item label="scope_include">
        <el-input v-model="form.scope_include" type="textarea" :rows="3" placeholder="强制纳入的 canonical，如 binance:swap:BTCUSDT" />
        <span class="hint">catalog 无事实时不臆造精度（12 §5）；未解析的条目在范围预览中如实列出。</span>
      </el-form-item>

      <el-form-item label="scope_exclude">
        <el-input v-model="form.scope_exclude" type="textarea" :rows="3" placeholder="排除：canonical 或裸码" />
      </el-form-item>

      <el-divider content-position="left">运行参数与说明</el-divider>

      <el-form-item label="runtime_params">
        <el-input v-model="form.runtime_params" type="textarea" :rows="4" class="mono" />
        <span class="hint">生成配置文件时写出；不含计划本身（计划由控制面下发）。必须是合法 JSON。</span>
      </el-form-item>

      <el-form-item label="note">
        <el-input v-model="form.note" type="textarea" :rows="2" />
      </el-form-item>

      <el-alert v-if="error" type="error" :closable="false" :title="error" />
      <el-alert
        v-else-if="editing"
        type="info"
        :closable="false"
        show-icon
        title="保存将携带 If-Match（节点当前 ETag）；他人已改范围规则会返回 412（§5.3）。"
      />
    </el-form>

    <template #footer>
      <el-button @click="emit('update:visible', false)">取消</el-button>
      <el-button type="primary" :loading="saving || save.isPending.value" @click="submit">
        {{ editing ? '保存' : '创建' }}
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.hint {
  display: block;
  font-size: 12px;
  color: var(--ponk-text-3);
  line-height: 1.6;
  margin-top: 2px;
}
</style>
