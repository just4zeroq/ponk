/** §4.13 审计日志接口（全表分页，只读）。 */
import { downloadFile, request } from './http'
import type { AuditEntry, AuditListQuery, Page } from './types'

/** GET /audit 全表分页（`idx_audit_ts`）。 */
export function listAudit(query: AuditListQuery = {}, signal?: AbortSignal): Promise<Page<AuditEntry>> {
  return request<Page<AuditEntry>>('/audit', { query: { ...query }, signal })
}

/** GET /audit/{id} 单条（含 `detail` 全文与 `diff[]`）。 */
export function getAudit(id: number, signal?: AbortSignal): Promise<AuditEntry> {
  return request<AuditEntry>(`/audit/${id}`, { signal })
}

/** GET /audit/export CSV 导出（筛选条件随导出，§5.4）。 */
export async function exportAudit(query: AuditListQuery = {}): Promise<void> {
  await downloadFile('/audit/export', 'audit.csv', { query: { ...query } })
}
