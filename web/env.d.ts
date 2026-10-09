/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** API 基址，缺省 `/api/v1`（docs/13 §4.1）。 */
  readonly VITE_API_BASE?: string
  /** 路由模式：`history`（缺省，需 SPA fallback）或 `hash`。 */
  readonly VITE_ROUTER_MODE?: string
  /** 开发代理目标。 */
  readonly VITE_PROXY_TARGET?: string
}

interface ImportMeta {
  readonly env: ImportMetaEnv
}

declare module '*.vue' {
  import type { DefineComponent } from 'vue'
  const component: DefineComponent<Record<string, never>, Record<string, never>, unknown>
  export default component
}
