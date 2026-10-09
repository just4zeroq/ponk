/**
 * §4.6 交易所与市场 / §4.7 交易对 / §4.8 币信息（docs/14 §6 / §7）。
 *
 * 分页口径（§4.1）：**只有列出 `limit=&offset=` 的端点返回分页信封**；
 * `GET /exchanges/{venue}/markets` 与 `GET /assets/{code}/contracts` 返回**裸数组**。
 */
import { downloadFile, request, requestWithEtag, type EtagResult } from './http'
import type {
  AssetContractRecord,
  AssetPatch,
  AssetRow,
  ExchangeInput,
  ExchangeMarketInput,
  ExchangeMarketRecord,
  ExchangeRecord,
  InstrumentChange,
  InstrumentRow,
  Page,
  PageQuery,
  VenueAssetRow,
} from './types'

// ── §4.6 交易所与市场 ────────────────────────────────────────────────────────

export interface ExchangeListQuery extends PageQuery {
  enabled?: boolean
  q?: string
}

/** GET /exchanges 交易所行 + 市场数。 */
export function listExchanges(query: ExchangeListQuery = {}, signal?: AbortSignal): Promise<Page<ExchangeRecord>> {
  return request<Page<ExchangeRecord>>('/exchanges', { query: { ...query }, signal })
}

/** POST /exchanges 新建（201）。 */
export function createExchange(body: ExchangeInput): Promise<ExchangeRecord> {
  return request<ExchangeRecord>('/exchanges', { method: 'POST', body })
}

/** GET /exchanges/{venue} 交易所行 + 市场列表。 */
export function getExchange(venue: string, signal?: AbortSignal): Promise<EtagResult<ExchangeRecord & { markets?: ExchangeMarketRecord[] }>> {
  return requestWithEtag<ExchangeRecord & { markets?: ExchangeMarketRecord[] }>(
    `/exchanges/${encodeURIComponent(venue)}`,
    { signal },
  )
}

/** PUT /exchanges/{venue}（同 POST + `If-Match`）。 */
export function updateExchange(venue: string, body: ExchangeInput, ifMatch: string): Promise<ExchangeRecord> {
  return request<ExchangeRecord>(`/exchanges/${encodeURIComponent(venue)}`, { method: 'PUT', body, ifMatch })
}

/** PATCH /exchanges/{venue}/enabled 启停。 */
export function setExchangeEnabled(venue: string, enabled: boolean, ifMatch: string): Promise<ExchangeRecord> {
  return request<ExchangeRecord>(`/exchanges/${encodeURIComponent(venue)}/enabled`, {
    method: 'PATCH',
    body: { enabled },
    ifMatch,
  })
}

/**
 * DELETE /exchanges/{venue}（级联删除市场与交易对 → 需二次确认参数）。
 *
 * docs/14 §6：有关联市场时后端返回 `400 + detail.confirm = <venue>`，
 * 调用方据此弹二次确认后带 `?confirm=<venue>` 重试。
 * @param silent 由调用方接管 400 提示（避免确认前先弹一条全局报错）。
 */
export function deleteExchange(
  venue: string,
  ifMatch: string,
  confirmCascade?: string,
  silent = false,
): Promise<void> {
  return request<void>(`/exchanges/${encodeURIComponent(venue)}`, {
    method: 'DELETE',
    ifMatch,
    query: confirmCascade ? { confirm: confirmCascade } : undefined,
    silent,
  })
}

/** GET /exchanges/{venue}/markets 市场行。 */
export function listMarkets(venue: string, signal?: AbortSignal): Promise<ExchangeMarketRecord[]> {
  return request<ExchangeMarketRecord[]>(`/exchanges/${encodeURIComponent(venue)}/markets`, { signal })
}

/** POST /exchanges/{venue}/markets 新建市场（`dig3_account_type` 必填，12 §3.1）。 */
export function createMarket(venue: string, body: ExchangeMarketInput): Promise<ExchangeMarketRecord> {
  return request<ExchangeMarketRecord>(`/exchanges/${encodeURIComponent(venue)}/markets`, { method: 'POST', body })
}

/** PUT /exchanges/{venue}/markets/{market}（同上 + `If-Match`）。 */
export function updateMarket(
  venue: string,
  market: string,
  body: ExchangeMarketInput,
  ifMatch: string,
): Promise<ExchangeMarketRecord> {
  return request<ExchangeMarketRecord>(
    `/exchanges/${encodeURIComponent(venue)}/markets/${encodeURIComponent(market)}`,
    { method: 'PUT', body, ifMatch },
  )
}

/** PATCH /exchanges/{venue}/markets/{market}/enabled 启停。 */
export function setMarketEnabled(venue: string, market: string, enabled: boolean, ifMatch: string): Promise<ExchangeMarketRecord> {
  return request<ExchangeMarketRecord>(
    `/exchanges/${encodeURIComponent(venue)}/markets/${encodeURIComponent(market)}/enabled`,
    { method: 'PATCH', body: { enabled }, ifMatch },
  )
}

// ── §4.7 交易对 ──────────────────────────────────────────────────────────────

export interface InstrumentListQuery extends PageQuery {
  venue?: string
  market?: string
  status?: string
  native_code?: string
  base?: string
  quote?: string
  instrument_type?: string
  synced_after?: number
  synced_before?: number
}

/** GET /instruments 分页筛选（`sort` 允许 native_code/status/last_synced_at/listed_at）。 */
export function listInstruments(query: InstrumentListQuery = {}, signal?: AbortSignal): Promise<Page<InstrumentRow>> {
  return request<Page<InstrumentRow>>('/instruments', { query: { ...query }, signal })
}

/** GET /instruments/{venue}/{market}/{canonical} 单条详情（三段路径）。 */
export function getInstrument(venue: string, market: string, nativeCode: string, signal?: AbortSignal): Promise<InstrumentRow> {
  return request<InstrumentRow>(
    `/instruments/${encodeURIComponent(venue)}/${encodeURIComponent(market)}/${encodeURIComponent(nativeCode)}`,
    { signal },
  )
}

/** GET /instruments/export CSV 流式导出（筛选条件随导出，§5.4）。 */
export async function exportInstruments(query: InstrumentListQuery = {}): Promise<void> {
  await downloadFile('/instruments/export', 'instruments.csv', { query: { ...query } })
}

export interface InstrumentChangeListQuery extends PageQuery {
  venue?: string
  market?: string
  native_code?: string
  change_kind?: string
  since?: number
  until?: number
  sync_run_id?: number
}

/** GET /instruments/changes 变更历史分页。 */
export function listInstrumentChanges(
  query: InstrumentChangeListQuery = {},
  signal?: AbortSignal,
): Promise<Page<InstrumentChange>> {
  return request<Page<InstrumentChange>>('/instruments/changes', { query: { ...query }, signal })
}

/** GET /instruments/changes/{id} 单条变更（`before` / `after` JSON）。 */
export function getInstrumentChange(id: number, signal?: AbortSignal): Promise<InstrumentChange> {
  return request<InstrumentChange>(`/instruments/changes/${id}`, { signal })
}

// ── §4.8 币信息 ──────────────────────────────────────────────────────────────

export interface AssetListQuery extends PageQuery {
  kind?: string
  q?: string
  venue?: string
  deposit_enabled?: boolean
  withdraw_enabled?: boolean
}

/** GET /assets 全局层 + 可选按所过滤。 */
export function listAssets(query: AssetListQuery = {}, signal?: AbortSignal): Promise<Page<AssetRow>> {
  return request<Page<AssetRow>>('/assets', { query: { ...query }, signal })
}

/** 资产详情 = 全局记录 + 各所 `venue_asset[]`。 */
export type AssetDetail = AssetRow & { venue_assets?: VenueAssetRow[] }

/** GET /assets/{code} 全局记录 + 各所 `venue_asset[]`。 */
export function getAsset(code: string, signal?: AbortSignal): Promise<AssetDetail> {
  return request<AssetDetail>(`/assets/${encodeURIComponent(code)}`, { signal })
}

/** GET /assets/{code}（带 `ETag` 头）：写前取最新行级 ETag（§5.3）。 */
export function getAssetWithEtag(code: string, signal?: AbortSignal): Promise<EtagResult<AssetDetail>> {
  return requestWithEtag<AssetDetail>(`/assets/${encodeURIComponent(code)}`, { signal })
}

/**
 * PATCH /assets/{code}：改 `kind` / `external_id` + `If-Match`（docs/14 §9 / docs/13 §4.8）。
 * 手填 CG id 后服务端置 `match_status = matched`。
 */
export function patchAsset(code: string, body: AssetPatch, ifMatch: string): Promise<AssetRow> {
  return request<AssetRow>(`/assets/${encodeURIComponent(code)}`, { method: 'PATCH', body, ifMatch })
}

/**
 * GET /assets/{code}/contracts：链 + 合约地址列表（12 §3.2.1 子表，docs/14 §9）。
 *
 * 该端点在 §4.8 中**未列 `limit=&offset=`** → 按 §4.1 通则返回**裸数组**；
 * 文档未给出响应体示例，故对 `{items:[…]}` 信封也做了兜底。
 */
export function getAssetContracts(code: string, signal?: AbortSignal): Promise<AssetContractRecord[]> {
  return request<AssetContractRecord[] | { items?: AssetContractRecord[] }>(
    `/assets/${encodeURIComponent(code)}/contracts`,
    { signal },
  ).then((r) => (Array.isArray(r) ? r : (r?.items ?? [])))
}

/** GET /venues/{venue}/assets 按所视图。 */
export function listVenueAssets(
  venue: string,
  query: { q?: string; limit?: number; offset?: number } = {},
  signal?: AbortSignal,
): Promise<Page<VenueAssetRow>> {
  return request<Page<VenueAssetRow>>(`/venues/${encodeURIComponent(venue)}/assets`, { query: { ...query }, signal })
}

/** GET /assets/export CSV 导出（筛选条件随导出，§5.4）。 */
export async function exportAssets(query: AssetListQuery = {}): Promise<void> {
  await downloadFile('/assets/export', 'assets.csv', { query: { ...query } })
}
