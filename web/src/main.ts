import { createApp } from 'vue'
import { createPinia } from 'pinia'
import { VueQueryPlugin } from '@tanstack/vue-query'
import ElementPlus from 'element-plus'
import zhCn from 'element-plus/es/locale/lang/zh-cn'
import 'element-plus/dist/index.css'
import App from './App.vue'
import { router } from './router'
import { createAppQueryClient } from './query/client'
import { onUnauthorized } from './api/http'
import { useSessionStore } from './stores/session'
import './style.css'

const app = createApp(App)

app.use(createPinia())
app.use(ElementPlus, { locale: zhCn })
app.use(VueQueryPlugin, { queryClient: createAppQueryClient() })
app.use(router)

// 401 → 清会话并跳登录（docs/13 §5.1）。
onUnauthorized(() => {
  const session = useSessionStore()
  session.invalidate()
  const current = router.currentRoute.value
  if (current.name !== 'login') {
    void router.replace({ path: '/login', query: { redirect: current.fullPath } })
  }
})

router.afterEach((to) => {
  const title = (to.meta as { title?: string }).title
  document.title = title ? `${title} · PONK 控制面` : 'PONK 控制面管理界面'
})

app.mount('#app')
