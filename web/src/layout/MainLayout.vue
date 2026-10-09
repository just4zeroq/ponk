<script setup lang="ts">
/**
 * 主布局：§3.1 导航树（冷热分区）+ 会话信息 + 只读角色横幅（§5.1）。
 * 未接入 / 未实现的页面在导航上如实标注，不假装可用（O5）。
 */
import { computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessageBox } from 'element-plus'
import { SwitchButton } from '@element-plus/icons-vue'
import { NAV } from '../router'
import { capability } from '../api/capabilities'
import { useSessionStore } from '../stores/session'
import { API_BASE } from '../api/http'

const route = useRoute()
const router = useRouter()
const session = useSessionStore()

/** 深链（`/nodes/col-binance`）时仍高亮所属分组。 */
const activePath = computed(() => {
  const p = route.path
  if (p.startsWith('/nodes')) return '/nodes'
  if (p.startsWith('/config')) return '/config'
  if (p.startsWith('/servers')) return '/servers'
  if (p.startsWith('/runtime')) return '/runtime'
  if (p.startsWith('/audit')) return '/audit'
  if (p.startsWith('/assignments')) return '/assignments'
  if (p.startsWith('/sync')) return '/sync'
  if (p.startsWith('/catalog/')) return p
  return p === '' ? '/' : p
})

async function logout(): Promise<void> {
  try {
    await ElMessageBox.confirm('确定退出登录？token 将从 sessionStorage 清除（§5.1）。', '退出登录', {
      type: 'warning',
    })
  } catch {
    return
  }
  session.logout()
  await router.replace('/login')
}
</script>

<template>
  <el-container class="layout">
    <el-aside width="232px" class="aside">
      <div class="brand">
        <div class="brand-name">PONK 控制面</div>
        <div class="brand-sub">管理界面 docs/13</div>
      </div>

      <el-menu :default-active="activePath" router class="nav">
        <template v-for="(group, gi) in NAV" :key="gi">
          <el-menu-item-group v-if="group.title" :title="group.title">
            <el-menu-item v-for="item in group.items" :key="item.to" :index="item.to">
              <span class="nav-label">{{ item.label }}</span>
              <span v-if="item.page" class="nav-page">{{ item.page }}</span>
              <el-tag v-if="item.unimplemented" size="small" type="info" effect="plain">未实现</el-tag>
              <el-tag
                v-else-if="item.capability && capability(item.capability).status === 'not_wired'"
                size="small"
                type="warning"
                effect="plain"
              >未接入</el-tag>
            </el-menu-item>
          </el-menu-item-group>
          <template v-else>
            <el-menu-item v-for="item in group.items" :key="item.to" :index="item.to">
              <span class="nav-label">{{ item.label }}</span>
              <span v-if="item.page" class="nav-page">{{ item.page }}</span>
            </el-menu-item>
          </template>
        </template>
      </el-menu>
    </el-aside>

    <el-container>
      <el-header class="header" height="52px">
        <div class="header-left">
          <span class="muted">API</span>
          <span class="mono">{{ API_BASE }}</span>
        </div>
        <div class="header-right">
          <el-tag :type="session.role === 'write' ? 'success' : 'info'" size="small">
            {{ session.role === 'write' ? '可写 write' : '只读 read' }}
          </el-tag>
          <span class="actor">{{ session.actor || '未设置 X-Actor' }}</span>
          <el-button size="small" :icon="SwitchButton" @click="logout">退出</el-button>
        </div>
      </el-header>

      <el-main class="main">
        <el-alert
          v-if="session.readOnly"
          class="readonly-banner"
          type="info"
          :closable="false"
          show-icon
          title="当前 token 角色为 read：仅可查看与导出，写操作（新建 / 编辑 / 发布 / 回滚 / 删除 / 触发）会被 403 拒绝（docs/13 §5.1）。"
        />
        <router-view />
      </el-main>
    </el-container>
  </el-container>
</template>

<style scoped>
.layout {
  height: 100%;
}

.aside {
  background: #fff;
  border-right: 1px solid var(--ponk-border);
  overflow-y: auto;
}

.brand {
  padding: 14px 16px 10px;
  border-bottom: 1px solid var(--ponk-border);
}

.brand-name {
  font-size: 15px;
  font-weight: 700;
}

.brand-sub {
  font-size: 12px;
  color: var(--ponk-text-3);
  margin-top: 2px;
}

.nav {
  border-right: none;
}

.nav-label {
  flex: 1;
}

.nav-page {
  font-size: 11px;
  color: var(--ponk-text-3);
  margin-right: 6px;
}

.header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  background: #fff;
  border-bottom: 1px solid var(--ponk-border);
  padding: 0 16px;
}

.header-left,
.header-right {
  display: flex;
  align-items: center;
  gap: 10px;
}

.actor {
  color: var(--ponk-text-2);
  font-size: 13px;
}

.main {
  padding: 0;
  overflow-y: auto;
}

.readonly-banner {
  margin: 12px 20px 0;
}
</style>
