-- 控制面数据库初始结构（docs/12-控制面数据库.md §3）
--
-- 约定：
--  * 时间戳一律 INTEGER（UTC 毫秒），与事件信封一致（docs/03 §5.1）
--  * STRICT 表（SQLite 3.37+）：类型不符直接报错，不静默转换
--  * 所方原始字段进 extra（TEXT/JSON），不为个所怪字段加列
--  * JSON 字段用 TEXT 存储（可用 json_* 函数查询）
--
-- 冷热分离（docs/12 §2）：定义与版本（冷，可回滚）与运行时状态（热，每 5s 写）分表。

PRAGMA foreign_keys = ON;

-- ─────────────────────────────────────────────────────────────────────────────
-- 交易所与市场
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS exchange (
  venue            TEXT PRIMARY KEY,               -- binance / okx / bitget / hyperliquid
  name             TEXT NOT NULL,
  dig3_exchange_id TEXT NOT NULL,                  -- 必须能映射到 dig3 ExchangeId，否则采集起不来
  enabled          INTEGER NOT NULL DEFAULT 1,     -- 0/1
  asset_class      TEXT NOT NULL DEFAULT 'crypto', -- crypto / stock / forex / prediction
                                                   -- stock → 该所交易对跳过链上币信息（docs/14 §6）
  extra            TEXT NOT NULL DEFAULT '{}',
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS exchange_market (
  venue             TEXT NOT NULL REFERENCES exchange(venue) ON DELETE CASCADE,
  market            TEXT NOT NULL,                 -- spot / swap / future
  dig3_account_type TEXT NOT NULL,                 -- Spot / FuturesCross …（docs/02 §6）
  enabled           INTEGER NOT NULL DEFAULT 1,
  quota_collect     REAL NOT NULL DEFAULT 0.50,    -- 02 §6：采集 ~50%
  quota_backfill    REAL NOT NULL DEFAULT 0.35,    -- 02 §6：回填 ~35%（归数据检查修复节点）
  quota_metadata    REAL NOT NULL DEFAULT 0.15,    -- 02 §6：元数据 ~15%（5 分钟同步任务）
  egress_ip         TEXT,
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  PRIMARY KEY (venue, market)
) STRICT;

-- ─────────────────────────────────────────────────────────────────────────────
-- 币信息（两层：全局锚 + 各所口径）
-- ─────────────────────────────────────────────────────────────────────────────
-- 全局资产：跨所对齐的锚（BTC 只有一个）
CREATE TABLE IF NOT EXISTS asset (
  id         INTEGER PRIMARY KEY,
  code       TEXT NOT NULL UNIQUE,                 -- BTC / USDT
  name       TEXT,
  kind       TEXT NOT NULL DEFAULT 'coin',         -- coin / token / stock / index
  -- ── 外部源（CoinGecko 为主，docs/14 §5）─────────────────────────────────
  external_id        TEXT,                         -- CG coin id（如 bitcoin）；未匹配为 NULL
  match_status       TEXT NOT NULL DEFAULT 'not_found',  -- matched / ambiguous / not_found
  twitter_url        TEXT,
  total_supply       TEXT,                         -- 十进制字符串（docs/03 §5.5 保真）
  circulating_supply TEXT,
  detail_source      TEXT,                         -- coingecko / coinmarketcap
  detail_synced_at   INTEGER,
  extra              TEXT NOT NULL DEFAULT '{}',   -- CG 原始响应（外部源加字段不必改表）
  -- `kind` / `match_status` 可由页面改（PATCH /assets/{code}）；**不参与** instrument.row_hash
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

-- 一币多链的合约地址（docs/14 §4 判断 2：**子表而非 JSON 列** —— 一对多本是关系，
-- 且"按地址反查"将来最可能用；量极小，一币 1~10 条）
CREATE TABLE IF NOT EXISTS asset_contract (
  id          INTEGER PRIMARY KEY,
  asset_id    INTEGER NOT NULL REFERENCES asset(id) ON DELETE CASCADE,
  chain       TEXT NOT NULL,                       -- TRON / Ethereum / Arbitrum ...
  address     TEXT NOT NULL,                       -- 合约地址
  standard    TEXT,                                -- TRC20 / ERC20 / BEP20 ...
  is_primary  INTEGER NOT NULL DEFAULT 0,
  source      TEXT NOT NULL DEFAULT 'coingecko',
  updated_at  INTEGER NOT NULL,
  UNIQUE(asset_id, chain, address)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_contract_address ON asset_contract(address);

CREATE TABLE IF NOT EXISTS venue_asset (
  asset_id         INTEGER NOT NULL REFERENCES asset(id) ON DELETE CASCADE,
  venue            TEXT NOT NULL REFERENCES exchange(venue) ON DELETE CASCADE,
  precision        INTEGER,
  networks         TEXT NOT NULL DEFAULT '[]',     -- ["TRC20","ERC20"]
  deposit_enabled  INTEGER,
  withdraw_enabled INTEGER,
  status           TEXT,
  extra            TEXT NOT NULL DEFAULT '{}',
  row_hash         TEXT NOT NULL,                  -- 变更检测（docs/12 §4.2）
  last_synced_at   INTEGER NOT NULL,
  PRIMARY KEY (asset_id, venue)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_venue_asset_venue ON venue_asset(venue);

-- ─────────────────────────────────────────────────────────────────────────────
-- 交易所交易对（主键 = canonical：venue:market:native_code）
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS instrument (
  venue            TEXT NOT NULL,
  market           TEXT NOT NULL,
  native_code      TEXT NOT NULL,
  base_asset_id    INTEGER REFERENCES asset(id),
  quote_asset_id   INTEGER REFERENCES asset(id),
  status           TEXT NOT NULL,                  -- trading / break / delisted
  instrument_type  TEXT,                           -- spot / perpetual / future（所方原样）
  price_precision  INTEGER,
  qty_precision    INTEGER,
  tick_size        TEXT,                           -- 十进制字符串（保真）
  step_size        TEXT,
  min_notional     TEXT,
  listed_at        INTEGER,
  delisted_at      INTEGER,
  first_seen_at    INTEGER NOT NULL,
  last_synced_at   INTEGER NOT NULL,
  row_hash         TEXT NOT NULL,                  -- 变更检测（docs/12 §4.2）
  extra            TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (venue, market, native_code),
  FOREIGN KEY (venue, market) REFERENCES exchange_market(venue, market) ON DELETE CASCADE
) STRICT;

CREATE INDEX IF NOT EXISTS idx_instrument_assets ON instrument(base_asset_id, quote_asset_id);
CREATE INDEX IF NOT EXISTS idx_instrument_status ON instrument(venue, market, status);
CREATE INDEX IF NOT EXISTS idx_instrument_synced ON instrument(last_synced_at);

-- 变更历史：审计 + "为什么重算了"的依据 + 名单决策输入
CREATE TABLE IF NOT EXISTS instrument_change (
  id          INTEGER PRIMARY KEY,
  venue       TEXT NOT NULL,
  market      TEXT NOT NULL,
  native_code TEXT NOT NULL,
  change_kind TEXT NOT NULL,      -- new / delisted / status / precision / type / relisted
  before_v    TEXT,               -- JSON（新上时为 NULL）
  after_v     TEXT,               -- JSON
  detected_at INTEGER NOT NULL,
  sync_run_id INTEGER REFERENCES sync_run(id)
) STRICT;

CREATE INDEX IF NOT EXISTS idx_change_detected ON instrument_change(detected_at);
CREATE INDEX IF NOT EXISTS idx_change_instrument ON instrument_change(venue, market, native_code);

-- 同步运行记录（09 §2.4 的 5 分钟任务：每轮一条）
CREATE TABLE IF NOT EXISTS sync_run (
  id          INTEGER PRIMARY KEY,
  started_at  INTEGER NOT NULL,
  finished_at INTEGER,
  outcome     TEXT NOT NULL DEFAULT '{}',
  error       TEXT
) STRICT;

-- 外部（币信息）同步运行记录（docs/12 §3.4b）——
-- 与 `sync_run` **分离**（docs/14 §3）：交易所目录与外部币信息是两个节拍、两种失败语义，
-- 混在一张表里会让"CG 限速"伪装成"交易所同步失败"。
CREATE TABLE IF NOT EXISTS offchain_sync_run (
  id           INTEGER PRIMARY KEY,
  started_at   INTEGER NOT NULL,
  finished_at  INTEGER,
  outcome      TEXT NOT NULL DEFAULT '{}',   -- fetched/matched/ambiguous/not_found/detail_pulled/rate_limited
  error        TEXT
) STRICT;

-- ─────────────────────────────────────────────────────────────────────────────
-- 服务器信息（人工登记的运维台账）
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS server (
  id         INTEGER PRIMARY KEY,
  host       TEXT NOT NULL UNIQUE,
  region     TEXT,
  roles      TEXT NOT NULL DEFAULT '[]',           -- ["collector","storage","control",…]
  cpu_cores  INTEGER,
  mem_gb     INTEGER,
  disk_gb    INTEGER,
  status     TEXT NOT NULL DEFAULT 'active',       -- active / maintenance / retired
  labels     TEXT NOT NULL DEFAULT '{}',
  notes      TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

-- ─────────────────────────────────────────────────────────────────────────────
-- 采集节点（定义 / 派生 / 运行时）
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS collector_node (
  id             INTEGER PRIMARY KEY,
  name           TEXT NOT NULL UNIQUE,             -- 唯一标识 = instance_id（10 §5）
  enabled        INTEGER NOT NULL DEFAULT 1,
  server_id      INTEGER REFERENCES server(id),
  scope_rules    TEXT NOT NULL,                    -- 标的选择规则（docs/12 §5）
  scope_exclude  TEXT NOT NULL DEFAULT '[]',       -- 例外：排除的 native_code
  scope_include  TEXT NOT NULL DEFAULT '[]',       -- 例外：强制纳入（仍受黑名单约束）
  runtime_params TEXT NOT NULL DEFAULT '{}',       -- 运行参数（生成配置文件时写出）
  note           TEXT,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS node_assignment (
  node_id            INTEGER NOT NULL REFERENCES collector_node(id) ON DELETE CASCADE,
  assignment_version INTEGER NOT NULL,
  mode               TEXT NOT NULL,                -- snapshot / diff
  plan               TEXT NOT NULL,                -- SubscribePlan JSON（03 §5.3）
  quota_splits       TEXT NOT NULL DEFAULT '{}',   -- 逐所分摊（线上只带一份，完整表在此）
  generated_at       INTEGER NOT NULL,
  PRIMARY KEY (node_id, assignment_version)
) STRICT;

-- 热表：运行时状态（= 兜底快照键的持久化形态，09 §6.3）
CREATE TABLE IF NOT EXISTS node_runtime (
  instance_id           TEXT PRIMARY KEY,          -- 同名重登 = 覆盖本行（docs/02 §7.4）
  node_id               INTEGER REFERENCES collector_node(id),
  node_kind             TEXT NOT NULL DEFAULT 'collector',  -- collector / signal / storage / …
  host                  TEXT,                      -- register 实际上报（09 §2.3 分散分片依据）
  status                TEXT NOT NULL,
  applied_version       INTEGER,
  last_assignment_version INTEGER,
  counters              TEXT NOT NULL DEFAULT '{}',
  started_at            INTEGER,
  last_seen_ts          INTEGER NOT NULL,
  lost_count            INTEGER NOT NULL DEFAULT 0,
  unconverged_since     INTEGER,
  last_repush_ts        INTEGER
) STRICT;

CREATE INDEX IF NOT EXISTS idx_runtime_seen ON node_runtime(last_seen_ts);
CREATE INDEX IF NOT EXISTS idx_runtime_node ON node_runtime(node_id);

-- ─────────────────────────────────────────────────────────────────────────────
-- 八域配置的版本化存储（09 §4.2）
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS config_domain (
  domain          TEXT PRIMARY KEY,
  current_version INTEGER NOT NULL DEFAULT 0,
  updated_at      INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE TABLE IF NOT EXISTS config_version (
  domain       TEXT NOT NULL REFERENCES config_domain(domain) ON DELETE CASCADE,
  version      INTEGER NOT NULL,
  content      TEXT NOT NULL,                       -- 域文档本体（JSON）
  content_hash TEXT NOT NULL,                       -- 审计与去重
  publisher    TEXT NOT NULL,
  published_at INTEGER NOT NULL,
  reason       TEXT NOT NULL DEFAULT '',
  prev_version INTEGER,
  PRIMARY KEY (domain, version)
) STRICT;

CREATE TABLE IF NOT EXISTS audit_log (
  id          INTEGER PRIMARY KEY,
  actor       TEXT NOT NULL,
  action      TEXT NOT NULL,                        -- publish / rollback / node_create / …
  target      TEXT NOT NULL,
  before_hash TEXT,
  after_hash  TEXT,
  detail      TEXT NOT NULL DEFAULT '{}',
  ts          INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log(ts);

-- ─────────────────────────────────────────────────────────────────────────────
-- HTTP 面登录用户（docs/12 §3.8；docs/13 §5.1）
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS control_user (
  id            INTEGER PRIMARY KEY,
  username      TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,                 -- argon2id；**永不返回、不进日志**
  role          TEXT NOT NULL DEFAULT 'read',  -- read / write
  disabled      INTEGER NOT NULL DEFAULT 0,    -- 0/1
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
) STRICT;
