<script setup lang="ts">
/**
 * 登录页（docs/13 §5.1）：
 * - **用户名 + 密码** → 服务端签发 **JWT**，存 `sessionStorage`（关闭标签即失效）
 * - 角色由服务端从用户表读出并写进 JWT，**前端不做角色选择**
 * - 失败按错误码内联提示：`invalid_credentials` 不区分用户不存在/密码错（防枚举）、
 *   `no_users` 提示先用 CLI 创建首个用户
 */
import { reactive, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { Lock, User } from '@element-plus/icons-vue'
import { useSessionStore } from '../stores/session'
import { ApiError } from '../api'

const route = useRoute()
const router = useRouter()
const session = useSessionStore()

const form = reactive<{ username: string; password: string }>({
  username: '',
  password: '',
})

const submitting = ref(false)
const error = ref('')

/** 错误码 → 面向操作者的中文提示（§5.1.1）。 */
function messageOf(e: unknown): string {
  if (e instanceof ApiError) {
    switch (e.code) {
      case 'invalid_credentials':
        return '用户名或密码错误'
      case 'no_users':
        return '控制面还没有任何用户：请先在服务端执行 ponk-control user add --username <u> --role <write>'
      case 'token_expired':
        return '登录已过期，请重新登录'
      case 'not_implemented':
        return '登录接口尚未实现（后端待接入，docs/13 §5.1.1）'
      case 'internal':
        return '服务端错误，请稍后重试'
      default:
        return e.message || `登录失败（${e.status}）`
    }
  }
  return e instanceof Error ? `无法连接控制面：${e.message}` : '登录失败'
}

async function submit(): Promise<void> {
  error.value = ''
  if (!form.username.trim()) {
    error.value = '请输入用户名'
    return
  }
  if (!form.password) {
    error.value = '请输入密码'
    return
  }
  submitting.value = true
  try {
    await session.login(form.username, form.password)
  } catch (e) {
    error.value = messageOf(e)
    submitting.value = false
    return
  }
  submitting.value = false
  const redirect = typeof route.query.redirect === 'string' ? route.query.redirect : '/'
  ElMessage.success(`已登录（${session.username} · ${session.role}）`)
  await router.replace(redirect)
}
</script>

<template>
  <div class="login-page">
    <el-card class="login-card">
      <template #header>
        <div class="login-head">
          <div class="title">PONK 控制面</div>
          <div class="sub">管理界面登录 · docs/13 §5.1</div>
        </div>
      </template>

      <el-form label-position="top" @submit.prevent="submit">
          <el-form-item label="用户名">
            <el-input
              v-model="form.username"
              placeholder="control_user.username"
              :prefix-icon="User"
              autocomplete="username"
            />
            <div class="hint">由 ponk-control user add 创建；用户名即 JWT 的 sub 与审计 actor。</div>
          </el-form-item>

          <el-form-item label="密码">
            <el-input
              v-model="form.password"
              type="password"
              show-password
              placeholder="密码"
              :prefix-icon="Lock"
              autocomplete="current-password"
              @keyup.enter="submit"
            />
            <div class="hint">服务端只存 argon2id 哈希，任何接口都不返回密码。</div>
          </el-form-item>

          <el-alert v-if="error" type="error" :closable="false" :title="error" class="err" />

          <el-button type="primary" native-type="submit" :loading="submitting" class="submit">登录</el-button>
        </el-form>

      <div class="foot muted">
        未接入目录类接口（交易所与市场 / 交易对 / 币信息 / 元数据同步）在导航上标注「未接入」，属阶段三（docs/13 §8）。
      </div>
    </el-card>
  </div>
</template>

<style scoped>
.login-page {
  min-height: 100%;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--ponk-bg);
  padding: 24px;
}

.login-card {
  width: 460px;
}

.login-head .title {
  font-size: 17px;
  font-weight: 700;
}

.login-head .sub {
  font-size: 12px;
  color: var(--ponk-text-3);
  margin-top: 2px;
}

.hint {
  font-size: 12px;
  color: var(--ponk-text-3);
  line-height: 1.6;
  margin-top: 4px;
}

.err {
  margin-bottom: 12px;
}

.submit {
  width: 100%;
}

.foot {
  margin-top: 14px;
  font-size: 12px;
  line-height: 1.6;
}
</style>
