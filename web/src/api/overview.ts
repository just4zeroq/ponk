/** §4.3 概览接口：`/healthz`、`/overview`、`/meta`。 */
import { request } from './http'
import type { Healthz, MetaResponse, OverviewResponse } from './types'

/** GET /healthz（免鉴权，§4.3）。 */
export function getHealthz(signal?: AbortSignal): Promise<Healthz> {
  return request<Healthz>('/healthz', { signal, silent: true })
}

/** GET /overview 总览聚合（§4.3）。 */
export function getOverview(signal?: AbortSignal): Promise<OverviewResponse> {
  return request<OverviewResponse>('/overview', { signal })
}

/** GET /meta 八域定义与消费关系（§4.3，前端渲染用）。 */
export function getMeta(signal?: AbortSignal): Promise<MetaResponse> {
  return request<MetaResponse>('/meta', { signal })
}
