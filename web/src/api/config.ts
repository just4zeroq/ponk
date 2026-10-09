/**
 * §4.9 配置域接口（含 §5.2 发布流程与 §5.4 导入导出）。
 *
 * 并发（§5.3）：读取拿 `etag` / `version` → 发布带 `base_version`（+ `If-Match`）→
 * 基线不符 409、`If-Match` 不符 412、缺失 428。
 */
import { downloadFile, request, requestWithEtag, type EtagResult } from './http'
import type {
  AuditEntry,
  ConfigDoc,
  ConfigPutRequest,
  DomainConvergence,
  DomainOverview,
  PublishRequest,
  PublishResult,
  RollbackRequest,
  ValidateOk,
  VersionListResponse,
} from './types'

/** GET /config/domains 八域概览（§4.9）。 */
export function listConfigDomains(signal?: AbortSignal): Promise<DomainOverview[]> {
  return request<DomainOverview[]>('/config/domains', { signal })
}

/** GET /config/{domain} 当前文档（响应体含 `etag`，并同时取 `ETag` 头）。 */
export function getConfigDomain(domain: string, signal?: AbortSignal): Promise<EtagResult<ConfigDoc>> {
  return requestWithEtag<ConfigDoc>(`/config/${encodeURIComponent(domain)}`, { signal })
}

/** PUT /config/{domain} 发布（= POST /publish，另带 `If-Match`，§4.9）。 */
export function putConfigDomain(domain: string, body: ConfigPutRequest, ifMatch: string): Promise<PublishResult> {
  return request<PublishResult>(`/config/${encodeURIComponent(domain)}`, {
    method: 'PUT',
    body,
    ifMatch,
  })
}

/** GET /config/{domain}/versions 版本历史（含 `diff[]`，分页）。 */
export function listConfigVersions(
  domain: string,
  query: { limit?: number; offset?: number } = {},
  signal?: AbortSignal,
): Promise<VersionListResponse> {
  return request<VersionListResponse>(`/config/${encodeURIComponent(domain)}/versions`, {
    query: { ...query },
    signal,
  })
}

/** GET /config/{domain}/versions/{version} 指定版本文档（历史不可变，§5.4）。 */
export function getConfigVersion(domain: string, version: number, signal?: AbortSignal): Promise<ConfigDoc> {
  return request<ConfigDoc>(`/config/${encodeURIComponent(domain)}/versions/${version}`, { signal })
}

/** GET /config/{domain}/versions/{version}/diff 与相邻版本的结构 diff。 */
export function getConfigVersionDiff(domain: string, version: number, signal?: AbortSignal): Promise<string[]> {
  return request<string[]>(`/config/${encodeURIComponent(domain)}/versions/${version}/diff`, { signal })
}

/** GET /config/{domain}/audit 该域审计（仅 publish / rollback，§6.1）。 */
export function getDomainAudit(domain: string, signal?: AbortSignal): Promise<AuditEntry[]> {
  return request<AuditEntry[]>(`/config/${encodeURIComponent(domain)}/audit`, { signal })
}

/** POST /config/{domain}/validate 校验（不落库）；失败抛 400 `detail.path`。 */
export function validateDomain(domain: string, content: unknown): Promise<ValidateOk> {
  return request<ValidateOk>(`/config/${encodeURIComponent(domain)}/validate`, {
    method: 'POST',
    body: { content },
  })
}

/** POST /config/{domain}/publish 发布（版本 +1；`base_version` 基线校验 → 409）。 */
export function publishDomain(domain: string, body: PublishRequest): Promise<PublishResult> {
  return request<PublishResult>(`/config/${encodeURIComponent(domain)}/publish`, { method: 'POST', body })
}

/** POST /config/{domain}/rollback 回滚 = 发布为目标版本内容为新版本（§5.2）。 */
export function rollbackDomain(domain: string, body: RollbackRequest): Promise<PublishResult> {
  return request<PublishResult>(`/config/${encodeURIComponent(domain)}/rollback`, { method: 'POST', body })
}

/** GET /config/{domain}/export 文档 JSON 下载（信封含 version/publisher/ts，§5.4）。 */
export async function exportDomainJson(domain: string): Promise<void> {
  await downloadFile(`/config/${encodeURIComponent(domain)}/export`, `${domain}.json`)
}

/** GET /config/{domain}/convergence 该域消费实例收敛进度（§4.9）。 */
export function getDomainConvergence(domain: string, signal?: AbortSignal): Promise<DomainConvergence> {
  return request<DomainConvergence>(`/config/${encodeURIComponent(domain)}/convergence`, { signal })
}
