/**
 * 路由（`docs/13 §3.1` 导航树 / §3.5 页面跳转；深链如 `/nodes/col-binance`、`/config/signal_rules`）。
 *
 * 默认 history 模式：控制面托管静态资源时需提供 SPA fallback（未匹配路径回 `index.html`）。
 * 若托管方不提供 fallback，可用 `VITE_ROUTER_MODE=hash` 构建（见 `main.ts`）。
 */
import { createRouter, createWebHistory, createWebHashHistory, type RouteRecordRaw } from 'vue-router'
import { readSession } from '../api/session'

const mode = import.meta.env.VITE_ROUTER_MODE === 'hash' ? createWebHashHistory() : createWebHistory()

/** 页面元信息（P 编号来自 §3.2 页面总表）。 */
export interface PageMeta {
  title: string
  /** §3.2 的页面编号。 */
  page?: string
  /** 免鉴权页（登录）。 */
  public?: boolean
  /** 关联的能力分组（未接入时界面显示「未接入」）。 */
  capability?: string
}

const routes: RouteRecordRaw[] = [
  {
    path: '/login',
    name: 'login',
    component: () => import('../views/LoginView.vue'),
    meta: { title: '登录', public: true } satisfies PageMeta,
  },
  {
    path: '/',
    component: () => import('../layout/MainLayout.vue'),
    children: [
      { path: '', name: 'overview', component: () => import('../views/OverviewView.vue'), meta: { title: '总览', page: 'P0' } satisfies PageMeta },
      // 资源 Resources（§3.1）
      { path: 'servers', name: 'servers', component: () => import('../views/ServersView.vue'), meta: { title: '服务器台账', page: 'P1' } satisfies PageMeta },
      { path: 'nodes', name: 'nodes', component: () => import('../views/NodesView.vue'), meta: { title: '采集节点', page: 'P2' } satisfies PageMeta },
      { path: 'nodes/:name', name: 'node-detail', component: () => import('../views/NodeDetailView.vue'), meta: { title: '节点详情', page: 'P2' } satisfies PageMeta },
      // 目录 Catalog（docs/14 §6 / §7：能力状态已翻转为 ready）
      { path: 'catalog/exchanges', name: 'exchanges', component: () => import('../views/CatalogExchangesView.vue'), meta: { title: '交易所与市场', page: 'P3', capability: 'exchanges' } satisfies PageMeta },
      { path: 'catalog/instruments', name: 'instruments', component: () => import('../views/CatalogInstrumentsView.vue'), meta: { title: '交易对', page: 'P4', capability: 'instruments' } satisfies PageMeta },
      { path: 'catalog/assets', name: 'assets', component: () => import('../views/CatalogAssetsView.vue'), meta: { title: '币信息', page: 'P5', capability: 'assets' } satisfies PageMeta },
      // 配置 Configuration
      { path: 'config', name: 'config-domains', component: () => import('../views/ConfigDomainsView.vue'), meta: { title: '八个配置域', page: 'P6' } satisfies PageMeta },
      { path: 'config/:domain', name: 'config-domain', component: () => import('../views/ConfigDomainView.vue'), meta: { title: '配置域详情', page: 'P6' } satisfies PageMeta },
      { path: 'audit', name: 'audit', component: () => import('../views/NotImplementedView.vue'), meta: { title: '审计日志', page: 'P9' } satisfies PageMeta },
      // 运行时 Runtime
      { path: 'runtime', name: 'runtime', component: () => import('../views/RuntimeView.vue'), meta: { title: '实例状态', page: 'P7' } satisfies PageMeta },
      { path: 'sync', name: 'sync', component: () => import('../views/NotWiredView.vue'), meta: { title: '元数据同步', page: 'P8', capability: 'sync' } satisfies PageMeta },
      { path: 'assignments', name: 'assignments', component: () => import('../views/NotImplementedView.vue'), meta: { title: '指派结果', page: 'P10' } satisfies PageMeta },
      { path: ':pathMatch(.*)*', name: 'not-found', component: () => import('../views/NotFoundView.vue'), meta: { title: '页面不存在' } satisfies PageMeta },
    ],
  },
]

export const router = createRouter({
  history: mode,
  routes,
  scrollBehavior: () => ({ top: 0 }),
})

/** 导航树（§3.1：冷热分区 + 跨页跳转）。 */
export interface NavGroup {
  title: string
  items: Array<{
    to: string
    label: string
    page?: string
    capability?: string
    icon?: string
    /** 未实现的页面（本次交付范围外）。 */
    unimplemented?: boolean
  }>
}

export const NAV: NavGroup[] = [
  { title: '', items: [{ to: '/', label: '总览', page: 'P0' }] },
  {
    title: '资源 Resources',
    items: [
      { to: '/servers', label: '服务器台账', page: 'P1' },
      { to: '/nodes', label: '采集节点', page: 'P2' },
    ],
  },
  {
    title: '目录 Catalog',
    items: [
      { to: '/catalog/exchanges', label: '交易所与市场', page: 'P3', capability: 'exchanges' },
      { to: '/catalog/instruments', label: '交易对', page: 'P4', capability: 'instruments' },
      { to: '/catalog/assets', label: '币信息', page: 'P5', capability: 'assets' },
    ],
  },
  {
    title: '配置 Configuration',
    items: [
      { to: '/config', label: '八个配置域', page: 'P6' },
      { to: '/audit', label: '审计日志', page: 'P9', unimplemented: true },
    ],
  },
  {
    title: '运行时 Runtime',
    items: [
      { to: '/runtime', label: '实例状态', page: 'P7' },
      { to: '/assignments', label: '指派结果', page: 'P10', unimplemented: true },
      { to: '/sync', label: '元数据同步', page: 'P8', capability: 'sync' },
    ],
  },
]

/** 鉴权守卫：无 token → 登录页（§5.1「登录页仅在 401 时出现」）。 */
router.beforeEach((to) => {
  if (to.meta.public) return true
  if (!readSession()) {
    return { path: '/login', query: to.fullPath !== '/' ? { redirect: to.fullPath } : undefined }
  }
  return true
})
