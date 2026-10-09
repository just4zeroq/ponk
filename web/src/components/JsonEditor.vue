<script setup lang="ts">
/**
 * JSON 编辑器（docs/13 §2.3 编辑器选型：默认 JSON 编辑 + 校验错误定位到 JSON 路径）。
 *
 * 结构：带行号槽的等宽文本区 + 实时 `JSON.parse` 语法检查 + 后端 `detail.path` 校验错误。
 * 规则密集域的结构化列表视图属 §2.3 的后续增强，本次未实现（见交付报告）。
 */
import { computed, ref, watch } from 'vue'
import { jsonParseError } from '../utils/format'

const props = withDefaults(
  defineProps<{
    modelValue: string
    /** 后端校验错误的 JSON 路径（`POST /config/{domain}/validate` → `detail.path`）。 */
    errorPath?: string | null
    /** 后端校验错误文案。 */
    errorMessage?: string | null
    /** 只读（只读角色或版本历史查看）。 */
    readonly?: boolean
    rows?: number
  }>(),
  { errorPath: null, errorMessage: null, readonly: false, rows: 18 },
)

const emit = defineEmits<{ 'update:modelValue': [value: string] }>()

const text = ref(props.modelValue)
watch(
  () => props.modelValue,
  (v) => {
    if (v !== text.value) text.value = v
  },
)
watch(text, (v) => emit('update:modelValue', v))

const parseError = computed(() => (text.value.trim() ? jsonParseError(text.value) : null))
const valid = computed(() => !parseError.value)
const lines = computed(() => text.value.split('\n').length)

function onInput(e: Event): void {
  text.value = (e.target as HTMLTextAreaElement).value
}
</script>

<template>
  <div>
    <div class="json-editor" :class="{ invalid: !valid }">
      <div class="gutter" aria-hidden="true">
        <div v-for="n in lines" :key="n">{{ n }}</div>
      </div>
      <textarea
        :value="text"
        :readonly="readonly"
        :rows="rows"
        spellcheck="false"
        @input="onInput"
      />
    </div>
    <div v-if="parseError" class="json-error">
      JSON 语法错误：{{ parseError.message }}
      <template v-if="parseError.line">（第 {{ parseError.line }} 行，第 {{ parseError.column }} 列）</template>
    </div>
    <div v-else-if="errorMessage" class="json-error">
      校验失败{{ errorPath ? `（路径 ${errorPath}）` : '' }}：{{ errorMessage }}
    </div>
  </div>
</template>
