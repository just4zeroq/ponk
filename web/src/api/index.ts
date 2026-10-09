/**
 * 类型化 API 客户端入口（`docs/13 §4`）。
 *
 * 分组：auth(§5.1.1) · overview(§4.3) · servers(§4.4) · nodes(§4.5) · catalog(§4.6–4.8)
 * · config(§4.9) · runtime(§4.10) · assignments(§4.11) · sync(§4.12) · audit(§4.13)
 */
export * from './types'
export * from './http'
export * from './session'
export * from './capabilities'
export * as authApi from './auth'
export * as overviewApi from './overview'
export * as serversApi from './servers'
export * as nodesApi from './nodes'
export * as catalogApi from './catalog'
export * as configApi from './config'
export * as runtimeApi from './runtime'
export * as assignmentsApi from './assignments'
export * as syncApi from './sync'
export * as auditApi from './audit'
