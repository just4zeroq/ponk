/** vue-query 键工厂（§5.6：客户端由 @tanstack/vue-query 管理 stale / refetch）。 */
import type { Ref } from 'vue'
import type { AssetListQuery, ExchangeListQuery, InstrumentChangeListQuery, InstrumentListQuery } from '../api/catalog'
import type { AuditListQuery, InstanceListQuery, NodeListQuery, ServerListQuery } from '../api/types'

const ROOT = 'ponk'

/** 允许把 `computed` 放进 queryKey（vue-query 会深度解包 ref，键随之变化 → 自动重取）。 */
type MaybeRefDeep<T> = T | Ref<T>

export const qk = {
  healthz: () => [ROOT, 'healthz'] as const,
  overview: () => [ROOT, 'overview'] as const,
  meta: () => [ROOT, 'meta'] as const,

  servers: (query: ServerListQuery) => [ROOT, 'servers', query] as const,
  server: (host: string) => [ROOT, 'server', host] as const,

  nodes: (query: NodeListQuery) => [ROOT, 'nodes', query] as const,
  node: (name: string) => [ROOT, 'node', name] as const,
  nodePreview: (name: string) => [ROOT, 'node', name, 'preview'] as const,
  nodeRuntime: (name: string) => [ROOT, 'node', name, 'runtime'] as const,
  nodeAssignments: (name: string) => [ROOT, 'node', name, 'assignments'] as const,

  configDomains: () => [ROOT, 'configDomains'] as const,
  configDomain: (domain: string) => [ROOT, 'config', domain] as const,
  configVersions: (domain: string, offset: number) => [ROOT, 'config', domain, 'versions', offset] as const,
  configAudit: (domain: string) => [ROOT, 'config', domain, 'audit'] as const,
  configConvergence: (domain: string) => [ROOT, 'config', domain, 'convergence'] as const,

  instances: (query: InstanceListQuery) => [ROOT, 'instances', query] as const,
  convergence: () => [ROOT, 'convergence'] as const,

  audit: (query: AuditListQuery) => [ROOT, 'audit', query] as const,

  // 目录类页面（docs/14 §6 / §7）。
  // 查询条件既可传普通值，也可传 `computed` —— vue-query 会对 queryKey 里的 ref 做
  // 深度解包（`cloneDeepUnref`），于是「行详情 / 变更历史」这类随 props 变化的键会自动重取。
  exchanges: (query: MaybeRefDeep<ExchangeListQuery>) => [ROOT, 'exchanges', query] as const,
  exchange: (venue: string) => [ROOT, 'exchange', venue] as const,
  markets: (venue: string) => [ROOT, 'markets', venue] as const,
  instruments: (query: MaybeRefDeep<InstrumentListQuery>) => [ROOT, 'instruments', query] as const,
  instrument: (venue: string, market: string, native: string) =>
    [ROOT, 'instrument', venue, market, native] as const,
  instrumentChanges: (query: MaybeRefDeep<InstrumentChangeListQuery | null>) =>
    [ROOT, 'instrumentChanges', query] as const,
  assets: (query: MaybeRefDeep<AssetListQuery>) => [ROOT, 'assets', query] as const,
  asset: (code: MaybeRefDeep<string>) => [ROOT, 'asset', code] as const,
  assetContracts: (code: MaybeRefDeep<string>) => [ROOT, 'asset', code, 'contracts'] as const,
  venueAssets: (venue: MaybeRefDeep<string>, query: MaybeRefDeep<{ q?: string; limit?: number; offset?: number }>) =>
    [ROOT, 'venueAssets', venue, query] as const,

  // 同步状态（docs/14 §7.4 空态与手动同步）
  syncStatus: () => [ROOT, 'syncStatus'] as const,
  syncRuns: (limit: number) => [ROOT, 'syncRuns', limit] as const,
  offchainStatus: () => [ROOT, 'offchainStatus'] as const,
} as const
