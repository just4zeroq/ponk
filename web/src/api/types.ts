/**
 * 控制面 HTTP API 的请求 / 响应类型。
 *
 * **唯一来源**：`docs/13-控制面管理界面与API.md` §4（服务接口）+ §3.3（逐页规格字段）
 * + §5.1（鉴权）/ §5.3（并发）/ §5.4（导入导出）。
 * 字段名与 `crates/control` 中的 serde 序列化名对齐（`ServerRecord` / `NodeRecord` /
 * `ScopeRules` / `SnapshotKey` / `StoredDoc` / `AuditEntry`）。
 *
 * 文档未逐字给出结构的聚合响应（如 `/overview` 的 `domains[]`、`/assignments/*`），
 * 按 §3.3 的展示字段建模，未知项标为可选并注明；见交付报告的「偏差与待后端确认」。
 */

// ─────────────────────────────────────────────────────────────────────────────
// 通则（§4.1 / §4.2）
// ─────────────────────────────────────────────────────────────────────────────

/** 分页响应信封（§4.1：`{"items":[…],"total":N,"limit":50,"offset":0}`）。 */
export interface Page<T> {
  items: T[]
  total: number
  limit: number
  offset: number
}

/** 分页查询参数（§4.1：`limit` 默认 50、最大 500）。 */
export interface PageQuery {
  limit?: number
  offset?: number
  sort?: string
  order?: 'asc' | 'desc'
}

/** 排序查询参数。 */
export interface SortQuery extends PageQuery {
  sort?: string
}

/** §4.2 错误码。 */
export type ApiErrorCode =
  | 'unauthenticated'
  /** JWT 过期（§5.1）：清会话回登录页。 */
  | 'token_expired'
  /** 用户名或密码错误（不区分，防用户名枚举）。 */
  | 'invalid_credentials'
  /** 空库：尚无任何用户，需先 `ponk-control user add`。 */
  | 'no_users'
  /** 接口尚未实现（§4.2：与 404 区分）。 */
  | 'not_implemented'
  | 'forbidden'
  | 'not_found'
  | 'conflict'
  | 'precondition_failed'
  | 'precondition_required'
  | 'validation'
  | 'illegal_state'
  | 'busy'
  | 'internal'

/** §4.2 统一错误体。 */
export interface ApiErrorBody {
  error: {
    code: ApiErrorCode | string
    message: string
    detail?: Record<string, unknown>
  }
}

/** GET /healthz（§4.3，唯一免鉴权接口）。 */
export interface Healthz {
  status: string
  instance_id: string
  started_at: number
}

// ─────────────────────────────────────────────────────────────────────────────
// 八域（09 §2 / §2.2）
// ─────────────────────────────────────────────────────────────────────────────

/** 八域 wire 名（`ConfigDomain::as_str`）。 */
export type ConfigDomainName =
  | 'subscribe_plan'
  | 'signal_rules'
  | 'alert_rules'
  | 'factor_rules'
  | 'channels'
  | 'process_capacity'
  | 'serialization'
  | 'instrument_blocklist'

/** 传播类别（09 §2.2）：A 广播通知+定向应答，B = A + 定向推送，C 不发通知。 */
export type PushClass = 'A' | 'B' | 'C'

/** GET /meta 的域定义（§4.3）。 */
export interface DomainMeta {
  name: ConfigDomainName | string
  display: string
  consumers: string[]
  push: PushClass
}

/** GET /meta 响应。 */
export interface MetaResponse {
  domains: DomainMeta[]
}

/** 总览中的八域行（§3.3 P0 展示字段 / §4.3 `/overview` → `domains[]`）。 */
export interface OverviewDomain {
  domain: ConfigDomainName | string
  /** 中文名；后端可能省略，前端有本地兜底表。 */
  display?: string
  /** 0 = 未发布（§3.4）。 */
  current_version: number
  updated_at?: number | null
  publisher?: string | null
  consumers?: string[]
  push?: PushClass
}

/** 实例计数（§4.3 `instances{…}`）。 */
export interface OverviewInstances {
  running: number
  stopped: number
  dead: number
  converged: number
  not_converged: number
  not_converged_timeout: number
}

/** 最近发布条目 = 审计条目（§3.3 P0「audit_log 最后 10 条」）。 */
export interface RecentPublish extends AuditEntry {}

/** 同步状态（§4.3 `sync{…}`，字段见 §3.3 P0 / P8）。 */
export interface OverviewSync {
  interval_ms?: number
  last_round_ms?: number | null
  running?: boolean
  due?: boolean
  [k: string]: unknown
}

/** catalog 概览（§4.3 `catalog{…}`）。 */
export interface OverviewCatalog {
  updated_at?: number | null
  symbols?: number
  currencies?: number
  [k: string]: unknown
}

/** GET /overview 响应。 */
export interface OverviewResponse {
  domains: OverviewDomain[]
  instances: OverviewInstances
  recent_publishes: RecentPublish[]
  sync?: OverviewSync
  catalog?: OverviewCatalog
}

// ─────────────────────────────────────────────────────────────────────────────
// 服务器台账（§4.4，12 §3.5）
// ─────────────────────────────────────────────────────────────────────────────

/** 服务器状态（§3.4）。 */
export type ServerStatus = 'active' | 'maintenance' | 'retired'

/** 服务器台账记录（`ServerRecord`）。 */
export interface ServerRecord {
  id: number
  host: string
  region: string | null
  roles: string[]
  cpu_cores: number | null
  mem_gb: number | null
  disk_gb: number | null
  status: ServerStatus | string
  labels: Record<string, string>
  notes: string | null
  created_at: number
  updated_at: number
}

/** 新建 / 编辑服务器的请求体（§4.4 POST / PUT）。 */
export interface ServerInput {
  host: string
  region?: string | null
  roles?: string[]
  cpu_cores?: number | null
  mem_gb?: number | null
  disk_gb?: number | null
  labels?: Record<string, string>
  notes?: string | null
}

/** PATCH /servers/{host}/status 请求体。 */
export interface ServerStatusInput {
  status: ServerStatus
}

/** GET /servers 查询（§4.4）。 */
export interface ServerListQuery extends PageQuery {
  status?: string
  role?: string
  q?: string
}

/** GET /servers/{host}/instances 响应（§4.4）。 */
export type ServerInstancesResponse = InstanceRow[]

// ─────────────────────────────────────────────────────────────────────────────
// 采集节点（§4.5，12 §3.6）
// ─────────────────────────────────────────────────────────────────────────────

/** 节点范围规则（`ScopeRules`，12 §5）。 */
export interface ScopeRules {
  /** 空 = 全部交易所。 */
  venues: string[]
  /** 空 = 全部市场（`spot` / `swap` / `future`…）。 */
  markets: string[]
  /** 通道：`candle_1m` / `trade` / `ticker` / `l1` / `funding`（02 §2）。 */
  channels: string[]
  /** 计价币过滤，空 = 不限。 */
  quote: string[]
  /** 基础币白名单，空 = 不限。 */
  base: string[]
  /** 基础币排除。 */
  base_exclude: string[]
  /** 交易对状态过滤，空 = 不限。 */
  status: string[]
  /** 分片大小（标的数/分片，09 §5.1，必须 >= 1）。 */
  shard_size: number
}

/** 节点定义（`NodeRecord`；`name` 即 `instance_id`）。 */
export interface NodeRecord {
  id: number
  name: string
  enabled: boolean
  server_id: number | null
  scope: ScopeRules
  scope_include: string[]
  scope_exclude: string[]
  runtime_params: unknown
  note: string | null
  created_at: number
  updated_at: number
}

/** 节点新建 / 编辑请求体（§4.5 POST / PUT）。 */
export interface NodeInput {
  name: string
  enabled: boolean
  server_id?: number | null
  scope: ScopeRules
  scope_include?: string[]
  scope_exclude?: string[]
  runtime_params?: unknown
  note?: string | null
}

/** PATCH /nodes/{name}/enabled 请求体。 */
export interface NodeEnabledInput {
  enabled: boolean
}

/** 实例状态（`InstStatus`，§3.4）。 */
export type InstStatus = 'registered' | 'running' | 'stopped' | 'dead'

/** 节点种类（`NodeKind`，wire 为 snake_case）。 */
export type NodeKind =
  | 'collector'
  | 'signal'
  | 'alert'
  | 'factor'
  | 'storage'
  | 'data_svc'
  | 'control'

/** 自报承载能力（`ReportedCapacity`）。 */
export interface ReportedCapacity {
  max_shards?: number | null
  max_instruments?: number | null
  max_channels?: number | null
  venues?: string[] | null
}

/** 运行时快照键（`SnapshotKey`，12 §3.6 热表 / 09 §6.3）。 */
export interface SnapshotKey {
  instance_id: string
  node_kind: NodeKind | string
  host?: string | null
  status: InstStatus | string
  reported?: ReportedCapacity
  /** 最近一次**下发**给它的 `assignment_version`。 */
  last_assignment_version?: number | null
  /** 心跳上报的 `applied_version`（采集 = `assignment_version`）。 */
  applied_version?: number | null
  /** 逐域收敛判据 `cfg.<domain>`（09 §4.4）。 */
  counters?: Record<string, number>
  started_at: number
  last_seen_ts: number
  /** 首次「不收敛」的时刻；已收敛为 null。 */
  unconverged_since?: number | null
  /** 已按 09 §4.4 重推过的时刻。 */
  last_repush_ts?: number | null
  lost_count?: number
}

/** 一条未收敛记录（`NotConverged`）。 */
export interface NotConverged {
  instance_id: string
  /** 采集为 null：它比 `applied_version == last_assignment_version`。 */
  domain: string | null
  expected: number
  actual: number
  held_ms: number
}

/** 收敛比对结果（`Convergence`，§4.10 `/convergence`）。 */
export interface ConvergenceSnapshot {
  converged: number
  not_converged: NotConverged[]
  running?: number
  stopped?: number
  dead?: number
}

/** 未登记机器提示：`node_runtime.host` 不在 `server` 表（§3.4）。 */
export interface UnregisteredHost {
  host: string
  instances: string[]
}

/** 节点列表行 = 定义 + 运行时联表（§4.5 GET `/nodes`）。 */
export interface NodeRow extends NodeRecord {
  /** 期望机器 host（由 `server_id` → `server.host` 解析；后端联表新增）。 */
  server_host?: string | null
  /** 实际上报机器（`node_runtime.host`）。 */
  runtime_host?: string | null
  /** 运行时快照（无实例时为 null）。 */
  runtime?: SnapshotKey | null
  /** 收敛状态（采集：applied == last_assignment）。 */
  converged?: boolean | null
  /** 最新指派摘要（§4.5）。 */
  assignment?: AssignmentSummary | null
  /** 机器未登记提示（§3.4）。 */
  unregistered_host?: boolean
}

/** 节点详情（§4.5 GET `/nodes/{name}` = 定义 + 运行时块 + 最新指派摘要）。 */
export interface NodeDetail extends NodeRow {}

/** 指派摘要（§3.3 P2 详情「指派块」）。 */
export interface AssignmentSummary {
  instance_id?: string
  node_name?: string
  assignment_version?: number
  mode?: 'snapshot' | 'diff' | string
  generated_at?: number
  shard_count?: number
  instrument_count?: number
  /** 逐所配额分摊（09 §5.3）。 */
  quota_splits?: QuotaSplit[]
  [k: string]: unknown
}

/** 逐所配额分摊行（§3.3 P10）。 */
export interface QuotaSplit {
  venue: string
  [k: string]: unknown
}

/** 分片行（§3.3 P10「按实例」视图组成）。 */
export interface ShardRow {
  instance_id?: string
  node_name?: string
  assignment_version?: number
  venue: string
  market: string
  shard_idx: number
  /** 该分片标的数。 */
  instruments?: number
  instrument_count?: number
  /** 承载机器（`node_runtime.host`；未上报则 `server` 期望机器）。 */
  host?: string | null
  server_id?: number | null
  [k: string]: unknown
}

/** GET /nodes 查询（§4.5）。 */
export interface NodeListQuery extends PageQuery {
  enabled?: boolean
  server_id?: number
  status?: InstStatus | string
  converged?: boolean
  host?: string
  q?: string
}

/** 范围预览响应（§4.5 / §3.3 P2）。 */
export interface ScopePreview {
  /** 入选标的（含精度）。 */
  instruments: PreviewInstrument[]
  /** 例外条目未解析的原因（12 §5「不臆造精度、不静默丢」）。 */
  unresolved: Array<[string, string] | { entry: string; reason: string }>
  counts: ScopePreviewCounts
}

/** 预览入选标的（精度字段为可选：catalog 无事实时不臆造）。 */
export interface PreviewInstrument {
  venue: string
  market: string
  native_code: string
  base?: string
  quote?: string
  status?: string
  instrument_type?: string
  price_precision?: number | null
  qty_precision?: number | null
  tick_size?: number | null
  step_size?: number | null
}

/** 预览计数。 */
export interface ScopePreviewCounts {
  instruments?: number
  unresolved?: number
  shards?: number
  [k: string]: number | undefined
}

/** GET /nodes/{name}/runtime 响应（§4.5）。 */
export interface NodeRuntimeResponse {
  runtime: SnapshotKey | null
  converged: boolean
  not_converged?: NotConverged
  host_registered?: boolean
}

/** GET /nodes/{name}/assignments 响应（§4.5，版本降序分页）。 */
export type NodeAssignmentsResponse = Page<AssignmentSummary>

// ─────────────────────────────────────────────────────────────────────────────
// 交易所与市场（§4.6，12 §3.1，docs/14 §6）
// ─────────────────────────────────────────────────────────────────────────────

/** 资产类别（12 §3.1 `exchange.asset_class`；docs/14 §6：`stock` → 该所交易对跳过链上币信息）。 */
export type AssetClass = 'crypto' | 'stock' | 'forex' | 'prediction' | string

/** 交易所行（`exchange`）。 */
export interface ExchangeRecord {
  venue: string
  name?: string | null
  dig3_exchange_id?: string | null
  enabled?: boolean
  /**
   * 资产类别（12 §3.1 / docs/14 §4）。
   * 注意：docs/13 §4.6 的 POST/PUT body 与 docs/14 §6 的表单字段清单均**未列出**此字段，
   * 但 DDL 有列、docs/14 §6 的 stock 跳过逻辑依赖它 —— 见交付报告的规格问题一节。
   */
  asset_class?: AssetClass | null
  extra?: Record<string, unknown>
  created_at?: number
  updated_at?: number
  /** 关联市场数（§4.6「交易所行 + 市场数」）。**列表不显示平均配额**（docs/14 §6）。 */
  market_count?: number
  /** 行级并发用 ETag（§5.3）。列表行由后端 `with_etag` 一并给出。 */
  etag?: string
}

/** 市场行（`exchange_market`）。 */
export interface ExchangeMarketRecord {
  venue?: string
  market: string
  /** 02 §6 映射，如 `swap → FuturesCross`；**无默认值**（12 §3.1）。 */
  dig3_account_type: string
  enabled?: boolean
  quota_collect?: number
  quota_backfill?: number
  quota_metadata?: number
  egress_ip?: string | null
  created_at?: number
  updated_at?: number
  etag?: string
}

/** 市场写入体（§4.6：POST 与 PUT 同体）。 */
export type ExchangeMarketInput = Omit<ExchangeMarketRecord, 'venue' | 'created_at' | 'updated_at' | 'etag'>

/** 交易所写入体（§4.6）。 */
export interface ExchangeInput {
  venue: string
  name?: string
  dig3_exchange_id?: string
  enabled?: boolean
  asset_class?: AssetClass
  extra?: Record<string, unknown>
}

// ─────────────────────────────────────────────────────────────────────────────
// 交易对 / 变更历史（§4.7，12 §3.3 / §3.4，docs/14 §7.2）
// ─────────────────────────────────────────────────────────────────────────────

/** 交易对行（§3.3 P4；canonical 三段 = venue/market/native_code）。 */
export interface InstrumentRow {
  venue: string
  market: string
  native_code: string
  base?: string | null
  quote?: string | null
  status: string
  instrument_type?: string | null
  price_precision?: number | null
  qty_precision?: number | null
  /** **十进制字符串**（12 §3.3 / 03 §5.5 保真）—— 不要当 number 用。 */
  tick_size?: string | null
  /** 同上，十进制字符串。 */
  step_size?: string | null
  /** 同上，十进制字符串。 */
  min_notional?: string | null
  listed_at?: number | null
  delisted_at?: number | null
  first_seen_at?: number | null
  last_synced_at?: number | null
  row_hash?: string
  extra?: Record<string, unknown>
  etag?: string
}

/** 交易对变更行（§3.3 P4b；列名以 DDL 为准，见 §7.2 R7）。 */
export interface InstrumentChange {
  id: number
  venue?: string
  market?: string
  native_code?: string
  change_kind: 'new' | 'delisted' | 'status' | 'precision' | 'type' | 'relisted' | string
  before?: unknown
  after?: unknown
  /** DDL 实际列名（§7.2 R7）。 */
  before_v?: unknown
  after_v?: unknown
  detected_at: number
  sync_run_id?: number | null
  etag?: string
}

/** 变更历史取值：`before` / `after` 是接口别名，`before_v` / `after_v` 是 DDL 列名。 */
export function changePayload(c: InstrumentChange, which: 'before' | 'after'): unknown {
  const alias = which === 'before' ? c.before : c.after
  const ddl = which === 'before' ? c.before_v : c.after_v
  return alias !== undefined && alias !== null ? alias : ddl
}

/** 变更类型（12 §3.3 `instrument_change.change_kind`）。 */
export type InstrumentChangeKind = 'new' | 'delisted' | 'status' | 'precision' | 'type' | 'relisted' | string

/** docs/14 §7.2 列出的全部变更类型（抽屉筛选下拉用）。 */
export const INSTRUMENT_CHANGE_KINDS: InstrumentChangeKind[] = [
  'new',
  'delisted',
  'status',
  'precision',
  'type',
  'relisted',
]

// ─────────────────────────────────────────────────────────────────────────────
// 币信息（§4.8，12 §3.2，docs/14 §7.3）
// ─────────────────────────────────────────────────────────────────────────────

/** 资产类别（12 §3.2 `asset.kind`）。 */
export type AssetKind = 'coin' | 'token' | 'stock' | 'index' | string

/** 匹配状态（docs/14 §5.2 三级：宁可「未收录」不静默错配）。 */
export type AssetMatchStatus = 'matched' | 'ambiguous' | 'not_found' | string

/** 全局层币记录（`asset`）。 */
export interface AssetRow {
  id?: number
  code: string
  name?: string | null
  kind?: AssetKind
  created_at?: number
  updated_at?: number
  /** 各所覆盖数（§3.3 P5）。 */
  venues?: number
  venue_count?: number
  // ── docs/14 §4「asset 增八列」───────────────────────────────────────────────
  /** CG coin id（如 `bitcoin`）；未匹配为 `null`。 */
  external_id?: string | null
  /** `matched` / `ambiguous` / `not_found`。 */
  match_status?: AssetMatchStatus
  twitter_url?: string | null
  /** **十进制字符串**（12 §3.2 / 03 §5.5），不是 number。 */
  total_supply?: string | null
  circulating_supply?: string | null
  /** `coingecko` / `coinmarketcap`。 */
  detail_source?: string | null
  detail_synced_at?: number | null
  /** 外部源原始响应（JSON）。 */
  extra?: Record<string, unknown> | null
  etag?: string
}

/** 各所层币记录（`venue_asset`）。 */
export interface VenueAssetRow {
  asset_id?: number
  venue: string
  code?: string
  precision?: number | null
  networks?: string[]
  deposit_enabled?: boolean
  withdraw_enabled?: boolean
  status?: string | null
  extra?: Record<string, unknown>
  row_hash?: string
  last_synced_at?: number | null
  etag?: string
}

/** PATCH /assets/{code} 请求体（docs/14 §9）。 */
export interface AssetPatch {
  kind?: AssetKind
  /** 手填 CG id 后服务端置 `match_status = matched`。 */
  external_id?: string | null
}

/** 一币多链的合约地址（12 §3.2.1 `asset_contract` 子表）。 */
export interface AssetContractRecord {
  id?: number
  asset_id?: number
  /** TRON / Ethereum / Arbitrum … */
  chain: string
  address: string
  standard?: string | null
  /** 是否主链合约。 */
  is_primary?: boolean
  /** `coingecko` / `coinmarketcap`。 */
  source?: string
  updated_at?: number
  etag?: string
}

// ─────────────────────────────────────────────────────────────────────────────
// 配置域（§4.9，12 §3.7）
// ─────────────────────────────────────────────────────────────────────────────

/** 已发布的域版本（`StoredDoc`；`content_hash` / `etag` 由 API 层组装，§6.1）。 */
export interface ConfigDoc {
  domain: ConfigDomainName | string
  version: number
  publisher: string
  ts: number
  reason: string
  prev_version: number | null
  content: unknown
  content_hash?: string
  /** 行级并发用 ETag（§5.3：`W/"<id>-<updated_at>"` 口径）。 */
  etag?: string
}

/** GET /config/domains 的八域概览（§4.9）。 */
export interface DomainOverview {
  domain: ConfigDomainName | string
  display?: string
  /** 0 = 未发布（§3.4，不计入版本历史）。 */
  current_version: number
  updated_at?: number | null
  consumers?: string[]
  push?: PushClass
  publisher?: string | null
  reason?: string | null
}

/** 版本历史行（§3.3 P6 版本历史字段）。 */
export interface VersionSummary {
  domain?: ConfigDomainName | string
  version: number
  publisher: string
  published_at?: number
  ts?: number
  reason: string
  prev_version: number | null
  content_hash?: string
  /** 与上一版的 diff 摘要（`diff_summary`）。 */
  diff?: string[]
}

/** GET /config/{domain}/versions 响应。 */
export type VersionListResponse = Page<VersionSummary>

/** 校验成功响应（§4.9）。 */
export interface ValidateOk {
  ok: true
  domain: string
}

/** 发布请求体（§4.9 / §5.2）。 */
export interface PublishRequest {
  content: unknown
  /** 读取时的 `current_version`；不符 → 409（§5.3）。 */
  base_version: number
  reason: string
}

/** PUT /config/{domain} 请求体（§4.9：与 POST /publish 同语义，另带 `If-Match`）。 */
export interface ConfigPutRequest extends PublishRequest {}

/** 回滚请求体（§4.9 / §5.2：发布为**新版本**，`current` 不倒回）。 */
export interface RollbackRequest {
  target_version: number
  base_version: number
}

/** 发布 / 回滚响应（§4.9 示例）。 */
export interface PublishResult {
  doc: ConfigDoc
  diff: string[]
  /** 出站消息条数（`Vec<Outbound>` 长度）。 */
  notified: number
  /** 本次发布是否触发指派重算（SubscribePlan / InstrumentBlocklist / ProcessCapacity）。 */
  recompute: boolean
  convergence?: ConvergenceSnapshot
}

/** 校验失败响应（§4.9：`detail{path}` 定位 JSON 路径）。 */
export interface ValidationDetail {
  path?: string
  [k: string]: unknown
}

/** GET /config/{domain}/convergence 响应（§4.9）。 */
export interface DomainConvergence {
  converged: number
  not_converged: NotConverged[]
}

/** 审计条目（`AuditEntry` + §3.3 P9 全表字段）。
 *
 * 仅 `ts` 为各接口共有的必填项：`GET /config/{domain}/audit` 返回域侧字段
 * （domain / version / prev_version / publisher / ts / reason / diff），
 * `GET /audit` 返回全表字段（id / actor / action / target / before_hash / after_hash / detail）。
 */
export interface AuditEntry {
  id?: number
  domain?: ConfigDomainName | string
  /** publish / rollback / node_create / server_update / …（§3.3 P9）。 */
  action?: string
  target?: string
  actor?: string
  publisher?: string
  version?: number
  prev_version?: number | null
  before_hash?: string | null
  after_hash?: string | null
  /** 含 `AuditEntry` 的 `diff[]`（§3.3 P9）。 */
  detail?: Record<string, unknown>
  reason?: string
  diff?: string[]
  ts: number
}

/** GET /audit 查询（§4.13）。 */
export interface AuditListQuery extends PageQuery {
  action?: string
  target?: string
  actor?: string
  since?: number
  until?: number
  q?: string
}

// ─────────────────────────────────────────────────────────────────────────────
// 运行时状态（§4.10）
// ─────────────────────────────────────────────────────────────────────────────

/** 运行时实例行 = `SnapshotKey` + 收敛判定（§4.10）。 */
export interface InstanceRow extends SnapshotKey {
  /** 是否收敛（不匹配时 `domain` / `expected` / `actual` / `held_ms` 给出依据）。 */
  converged?: boolean
  /** 不收敛详情（`domain` 采集为 null）。 */
  not_converged?: NotConverged | null
  /** 机器是否已登记（§3.4 黄标）。 */
  host_registered?: boolean
  /** 消费域（`validate::consumed_domains(kind)`），供域维度过滤。 */
  consumed_domains?: string[]
}

/** GET /runtime/instances 查询（§4.10）。 */
export interface InstanceListQuery extends PageQuery {
  node_kind?: string
  status?: InstStatus | string
  converged?: boolean
  host?: string
  domain?: string
}

/** GET /runtime/counts 响应（§4.10，供 P0）。 */
export interface RuntimeCounts extends OverviewInstances {
  total?: number
  [k: string]: number | undefined
}

// ─────────────────────────────────────────────────────────────────────────────
// 指派结果（§4.11）—— 阶段二试算，其余查询界面本次未实现页面
// ─────────────────────────────────────────────────────────────────────────────

/** GET /assignments 查询（§4.11）。 */
export interface AssignmentListQuery extends PageQuery {
  instance?: string
  venue?: string
  market?: string
  host?: string
  assignment_version?: number
  /** 视图切换：按实例 / 按机器 / 按交易所（§3.3 P10）。 */
  view?: 'instance' | 'host' | 'venue' | string
}

/** 指派分片列表响应。 */
export type AssignmentListResponse = Page<ShardRow>

/** 该实例最新计划（§4.11 `/assignments/{instance_id}`）。 */
export interface AssignmentDetail {
  instance_id: string
  assignment_version?: number
  mode?: string
  generated_at?: number
  shards?: ShardRow[]
  quota_splits?: QuotaSplit[]
  /** 完整 `SubscribePlan`（JSON，§3.3 P10）。 */
  plan?: unknown
  [k: string]: unknown
}

/** 未指派分片（§3.3 P10：`(venue, market, shard_idx, 原因)`）。 */
export interface UnassignedShard {
  venue: string
  market: string
  shard_idx: number
  reason?: string
  [k: string]: unknown
}

/** 试算请求（§4.11 POST `/assignments/dry-run`，不下发）。 */
export interface DryRunRequest {
  instance_id?: string
}

/** 试算响应：与当前下发计划的差异（结构由后端 `assign` 结果决定）。 */
export interface DryRunResult {
  [k: string]: unknown
}

// ─────────────────────────────────────────────────────────────────────────────
// 元数据同步（§4.12，12 §3.4）—— 阶段三，界面标注「未接入」
// ─────────────────────────────────────────────────────────────────────────────

/** GET /sync/status（§4.12）。 */
export interface SyncStatus {
  interval_ms: number
  last_round_ms: number | null
  running: boolean
  due: boolean
  share?: number
  next_due_at?: number | null
  catalog?: OverviewCatalog
  [k: string]: unknown
}

/** 跳过原因（§3.3 P8）。 */
export interface SyncSkipped {
  venue?: string
  market?: string
  reason: 'throttled' | 'unsupported' | 'fetch failed' | string
}

/** 单轮同步结果（`SyncOutcome`，§4.12）。 */
export interface SyncOutcome {
  ran?: boolean
  changed?: boolean
  recompute_ran?: boolean
  diff?: Record<string, unknown>
  skipped?: SyncSkipped[]
  fetched_symbols?: number
  fetched_currencies?: number
  [k: string]: unknown
}

/** 轮次记录（`sync_run`）。 */
export interface SyncRun {
  id: number
  started_at: number
  finished_at?: number | null
  outcome?: SyncOutcome | null
  error?: string | null
}

/** GET /sync/targets 条目（§4.12）。 */
export interface SyncTarget {
  venue: string
  market: string
  share?: number
  [k: string]: unknown
}

/**
 * 外部（币信息）同步轮次（12 §3.4b `offchain_sync_run`，docs/14 §9）。
 *
 * `outcome` 常见键（docs/14 §4/§11）：`fetched` / `matched` / `ambiguous` /
 * `not_found` / `detail_pulled` / `rate_limited`。
 */
export interface OffchainSyncRun {
  id?: number
  started_at?: number
  finished_at?: number | null
  outcome?: Record<string, unknown> | null
  error?: string | null
  /** 后端若在状态接口里额外给出「是否在跑」，按它优先。 */
  running?: boolean
  [k: string]: unknown
}

/**
 * GET /sync/offchain/status（docs/14 §9：上一轮结果，含 `rate_limited` / 失败原因）。
 *
 * **文档未给出确切响应结构**（docs/13 §4.8/§4.12 只有一行说明），故全部字段可选，
 * 前端按 `normalizeOffchainStatus()` 做了形状兜底。
 */
export interface OffchainSyncStatus {
  running?: boolean
  interval_ms?: number
  last_run?: OffchainSyncRun
  last?: OffchainSyncRun
  run?: OffchainSyncRun
  /** 直接平铺 `offchain_sync_run` 列时的形态。 */
  id?: number
  started_at?: number
  finished_at?: number | null
  outcome?: Record<string, unknown> | null
  error?: string | null
  [k: string]: unknown
}

// ── 认证（§5.1 / §5.1.1：用户名密码 + JWT）────────────────────────────────────

/** 登录请求。 */
export interface LoginRequest {
  username: string
  password: string
}

/** 登录成功响应（`access_token` 即 JWT）。 */
export interface LoginResponse {
  access_token: string
  token_type: string
  /** 有效期（秒）。 */
  expires_in: number
  role: 'read' | 'write'
  username: string
}

/** 当前身份（从 JWT 解析，服务端核签）。 */
export interface MeResponse {
  username: string
  role: 'read' | 'write'
  /** `exp` / `iat`（epoch 秒）。 */
  exp: number
  iat: number
}

/** 用户（**永不含密码哈希**）。 */
export interface UserRecord {
  username: string
  role: 'read' | 'write'
  disabled: boolean
  created_at?: number
  updated_at?: number
}

/** 新建用户。 */
export interface CreateUserRequest {
  username: string
  password: string
  role: 'read' | 'write'
}
