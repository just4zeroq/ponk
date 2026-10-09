# 13 控制面管理界面与 API

> 本文档定义控制面的**人机操作面**：一个管理界面（页面）与一套 HTTP 服务接口（API）。
> 对应 `01-architecture` §2（范围）、§4.8（M1/M2/M4）、§6（四条红线），
> 落地 `09-控制面` §1（操作者写接口 / 状态观测面）、`12-控制面数据库`（14 张表）与
> `02-采集` §7（节点生命周期）。部署编排不在范围内（01 M3 / 09 P4）。
>
> 界面是**控制面的客户端**，不是平台节点：不订阅事件中心、不发 `ops.*`、不消费 `ConfigDomain`
> （对比 11 §1.1 B3）。一切读写经 HTTP 直达控制面进程，配置下发仍只走总线（09 §4.6）。

---

## 1. 定位与边界

### 1.1 是什么 / 不是什么

| # | 判定 | 说明 |
|---|---|---|
| A1 | **是控制面两个面的呈现层** | 09 §1：配置写入面（提交域文档变更 → 版本、审计、`ConfigChanged`）与状态观测面（快照键、收敛比对、失联）各对应一组页面 |
| A2 | **是操作者的入口，不是配置事实来源** | 事实来源仍是控制面进程与其 SQLite 库（12 §1）；界面只做输入校验与呈现，不缓存权威状态 |
| A3 | **是控制面进程内的 HTTP 服务** | 与 `serve` 同进程、同事件中心会话、同 `SqliteStore`；不另起服务、不另开一份库写连接（§2.4） |
| A4 | **不是平台节点** | 不注册、不心跳、不发 `md.*`；06 监控仍自订 `ops.*`，界面不代发任何告警（09 §6.5） |
| A5 | **不是数据服务的消费方** | 行情/信号/因子历史查询属 08 与存储层，本界面不呈现；界面只呈现控制面库内的配置与目录 |

### 1.2 与桌面客户端（11）的边界

| 边界 | 桌面客户端（11） | 管理界面（本文件） |
|---|---|---|
| 产品性质 | 平台的**消费端产品**（行情展示 + 凭据管理） | 平台的**运维操作端**（配置与目录管理） |
| 用户 | 分析 / 交易使用者 | 运维与配置操作者 |
| 数据面 | 只读查询存储后端（SQL 结果集），轮询 | 读写控制面库（配置与目录），HTTP 直连控制面 |
| 配置面 | 本地偏好（布局/自选/主题），**不承载配置事实**（11 §1.1 B5） | 控制面八域与节点/服务器定义，**配置事实的唯一入口**（09 P1） |
| 凭据 | 本地凭据库，与平台零耦合 | 不涉及交易所凭据（01 §2.2 排除密钥管理） |
| 进程形态 | 桌面应用（GPUI + gpui-kit） | 浏览器访问的 Web 界面 + 控制面进程内 HTTP 服务 |
| 与总线关系 | 一期非平台节点（11 §1.1 B3） | 非平台节点；不订阅、不发布 |

**互斥关系**：八域配置、节点范围、指派结果的配置界面**只存在于本文件**（11 §1.2 N5/N8 已把它们排除在桌面客户端之外）；桌面客户端不提供任何写入口。两者不共享代码，只共享术语与色语义（§3.4）。

**组件复用判定**：不复用 gpui-kit 组件（§2.2）。桌面端的组件能力盘点（11 §2.4）与未核实项清单（11 §2.6 U1–U9）针对行情终端语境，本界面所需的表格分页、抽屉表单、审计时间轴等中后台控件属另一类需求；复用会把 11 的桌面运行时引入运维通路。

### 1.3 只读 / 读写范围

| 范围 | 对象 | 界面动作 | 依据 |
|---|---|---|---|
| **只读** | 交易对、币信息、交易所与市场的事实部分、运行时状态、指派结果、审计、同步记录 | 查看、筛选、分页、导出 | 事实由 5 分钟同步（09 §2.4）与心跳（10 §5）维护，界面不手工改写 |
| **读写** | 服务器台账、采集节点定义、八个配置域 | 新建 / 编辑 / 删除 / 校验 / 发布 / 回滚 | 09 §1 写入面；12 §3.5 / §3.6 |
| **读写（触发类）** | 同步轮次、指派重算、配置导出 | 立即触发同步、试算、导出 | 09 §2.4 / §5.4；只触发既有流程，不改事实 |
| **不可写** | 配置版本历史、快照键、`node_runtime` 的状态字段 | 无 | 09 §4.2 历史不可变；09 §6.2 状态由控制信道写入 |

**事实字段的手工覆写不开放**：`instrument.status`、`venue_asset.row_hash`、`node_runtime.*` 等由同步与心跳产生，界面提供查看与筛选，不提供编辑入口——否则 12 §4.2 的变更检测（`row_hash`）与 09 §4.4 的收敛判据会被人手写入破坏。

### 1.4 明确不做

| # | 非目标 | 依据 |
|---|---|---|
| X1 | 远程启动 / 停止 / 重启采集进程，进程编排、宿主管理 | 01 M3；09 P4；02 §7.3「控制面只生成内容，不远程部署」 |
| X2 | 配置应用失败反馈、运行时自动回滚 | 09 §7 |
| X3 | 失联驱动的自动再指派、再平衡 | 09 §7；10 §8 |
| X4 | 控制面 HA、多实例选主、只读副本 | 09 §7 |
| X5 | 走总线的界面通道（界面订阅 `md.*` / `ctl.*` / `ops.*`） | 红线 4；§5.5 |
| X6 | 多用户账号体系、角色系统、SSO、审计之外的操作留痕 | 一期最小鉴权（§5.1）；对比 11 §1.2 N7 |
| X7 | 行情 / 信号 / 因子历史查询、回放回测入口 | 04 / 05 / 08 的职责；11 §1.2 N6 |
| X8 | 交易所私有数据与凭据管理 | 01 §2.2 |

---

## 2. 技术选型

### 2.1 后端 HTTP 层

**现状**：控制面目前只有事件中心通路（`ControlPlane::poll` 同步轮询）与 CLI 五个子命令
（`serve` / `publish` / `rollback` / `audit` / `current`），**无任何 HTTP 服务**；
`crates/control` 不依赖 `tokio`（workspace 已有 `tokio`，由 `eventcenter` / `collect` 使用）。

| 项 | 选定 | 说明 |
|---|---|---|
| 框架 | **axum** | 见理由 |
| 运行时 | tokio（workspace 已有） | 控制面主循环迁入 `spawn_blocking` 或独立线程，HTTP 服务占 tokio 运行时 |
| 路由 | axum `Router`，路径参数 + JSON extractor | 与 `serde_json` 直接对接，域文档原样透传 |
| 中间件 | `tower` 层：鉴权 → 请求日志 → 限流 → CORS（仅同源） | §5.1 |
| 静态资源 | axum 内嵌或 `tower-http::ServeDir` | 界面产物随控制面二进制分发，单二进制部署 |
| SSE（可选） | axum `Sse` 流 | 收敛进度推送；一期以轮询为主（§5.6） |

**理由**：

| # | 理由 | 说明 |
|---|---|---|
| R1 | 与 workspace 一致 | `tokio` 已是工作区依赖（`eventcenter` / `collect` 在用），不引入第二套异步栈 |
| R2 | 异步 + 同步混用成本最低 | 控制面是同步阻塞型（SQLite `Mutex` + 轮询循环），axum handler 内用 `Mutex` 保护的同步调用即可，无需把 `rusqlite` 改成异步 |
| R3 | 中间件生态 | 鉴权、日志、限流、CORS 均有现成 `tower` 层，不必手写 |
| R4 | 推送能力可扩展 | 收敛进度、同步轮次变化可用 SSE/WebSocket 追加，不换框架 |
| R5 | 类型化 extractor | `Path` / `Query` / `Json` 直接落到既有结构体，域文档以 `serde_json::Value` 透传，避免重复定义结构 |

**备选对照**：

| 备选 | 优势 | 不选的理由 |
|---|---|---|
| `tiny_http` | 纯同步、零运行时改造，与当前 `ControlPlane` 形态最接近 | 路由、中间件、SSE、限流、CORS 均需手写；后续推送与鉴权演进成本高 |
| `actix-web` | 性能强、生态成熟 | 与 `tokio` 混用需额外处理运行时归属；extractor/宏体系学习成本高于收益 |
| `hyper` 裸用 | 依赖最少 | 路由与提取层全部自建，等价于自研框架 |
| 不引框架、手写 HTTP/1.1 | 依赖为零 | 无法承载分页、鉴权、SSE 的长期演进；不列入分期 |

**HTTP 层落位**：置于 `crates/control/src/` 内的 HTTP 模块（或同 crate 的子模块），**不新建独立 crate**。
理由：`SqliteStore::with_read` / `with_write` 为 `pub(crate)`，`ControlPlane::repush_plan` 等内部能力不对外暴露，
独立 crate 只能通过受限的公开 API 访问，反而需要把一批内部函数改公有。

### 2.2 前端形态

**选定：Web SPA（静态单页应用，构建产物由控制面进程托管）**。

| 方案 | 优势 | 代价 | 判定 |
|---|---|---|---|
| **Web SPA（选定）** | 浏览器即达、无需安装、内网任意机器可操作、发布与回滚可多人协作、组件生态成熟（表格分页/抽屉表单/审计时间轴） | 需自建一套界面代码，与 11 不共享组件 | **选定** |
| SSR（服务端渲染） | 无 | 控制面无 SEO 与首屏性能诉求；在全 Rust 栈中额外引入一套 Node 运行时与构建链；收益不成立 | 不选 |
| 复用 gpui-kit 桌面（与 11 同栈） | 与 11 共用主题与组件、视觉统一 | (a) 会把管理能力塞进 11，与 11 §1.1 B3/B5、§1.2 N5/N8 冲突；(b) 操作者须安装桌面端，远程可达性差，发布/回滚无法在会话现场完成；(c) gpui-kit 组件能力与未核实项（11 §2.6）面向行情终端，管理面控件适配度**未核实**；(d) Windows 侧若干未决 issue（11 §2.3） | 不选（备选，见 §2.3） |

**桌面形态的保留条件**：若后续需要「操作者本地客户端」（离线查看配置历史、无浏览器环境），
再评估 gpui-kit 复用；届时只复用**主题令牌与色语义**（§3.4），不复用页面结构。

### 2.3 前端框架

| 项 | 选定 | 备选 | 理由 |
|---|---|---|---|
| 语言 | TypeScript | — | 接口契约与响应结构可由 OpenAPI 生成类型，减少手写漂移 |
| 构建 | Vite | — | 静态产物、开发热更、无服务端渲染链 |
| 框架 | **Vue 3** | React | 与 Element Plus 同生态配套；组件族能力相当，主线已拍板 |
| 组件库 | **Element Plus** | Ant Design（React 生态）、自建 | 10 类页面所需的表格（分页/排序/固定列）、表单（校验/抽屉）、树、时间轴、步骤条、描述列表均由组件库直接覆盖；自建成本与一致性风险高 |
| 服务端状态 | **@tanstack/vue-query**（本地状态用 Pinia） | 手写 composable | 分页缓存、失效重取、写后失效（发布后刷新域列表与收敛）统封装 |
| 编辑器 | 域文档 = JSON 编辑器（带 schema 高亮与校验反馈）+ 按域的结构化表单 | — | 八域结构差异大（09 §3），全表单化成本过高、全 JSON 化可读性差；选定「默认 JSON 编辑 + 校验错误定位到 JSON 路径」，规则条目密集的域（`SignalRules` / `AlertRules` / `InstrumentBlocklist`）提供结构化列表视图 |

### 2.4 进程与部署落位

| 项 | 选定 |
|---|---|
| 运行形态 | 与 `serve` **同进程**：`ponk-control serve --http <addr>`（默认关闭，显式开启） |
| 静态资源 | 界面构建产物随二进制分发（内嵌或同目录 `web/`） |
| 数据访问 | 复用同一个 `SqliteStore` 写连接 + 只读连接（WAL 允许多读，12 §1.2）；读接口走只读连接，写接口走 `ControlPlane` |
| 共享方式 | `Arc<Mutex<ControlPlane>>`：发布 / 回滚 / 同步触发 / 指派试算需要 `&mut self`；读接口只借 `&self` 短锁 |
| 锁粒度 | 单次请求持锁；`poll` 循环与 HTTP 共用一把锁，锁等待超时返回 503 |

**不另起进程的理由**：

| # | 理由 |
|---|---|
| S1 | 发布必须经 `ControlPlane::publish_domain` 才能产出 `ConfigChanged` 与指派重算（09 §4.3 / §5.4）；另一进程无法共享本进程的事件中心会话 |
| S2 | CLI `publish` 当前形态即为独立进程：它用全新的内存事件中心发布 `ConfigChanged`，该通知不会到达正在运行的 `serve` 实例，运行中的控制面只能等心跳收敛比对才补发（09 §4.4）。界面发布走同进程，通知即时发出 |
| S3 | SQLite 单写（12 §1.2）：多进程写会把锁等待推到不可控；同进程复用一个写连接 |
| S4 | 红线 4 与 09 P2：控制面与其余节点不直连；界面是控制面的客户端，与控制面同进程即"一个中枢、一个人机入口"，不新增传输 |

---

## 3. 页面清单与信息架构

### 3.1 导航结构

```text
总览 Overview
├── 资源 Resources
│   ├── 服务器台账          P1
│   └── 采集节点            P2
├── 目录 Catalog
│   ├── 交易所与市场        P3
│   ├── 交易对              P4
│   │   └── 变更历史（子页）P4b
│   └── 币信息              P5
├── 配置 Configuration
│   ├── 八个配置域          P6
│   │   └── 版本历史 / 审计（子页）
│   └── 审计日志            P9
└── 运行时 Runtime
    ├── 实例状态            P7
    ├── 指派结果            P10
    └── 元数据同步          P8
```

| 导航原则 | 说明 |
|---|---|
| **冷热分区** | 「资源 / 目录 / 配置」= 冷数据（12 §2 定义与版本）；「运行时」= 热数据（每 5s 心跳写入）。导航上物理分组，避免高频刷新干扰配置编辑 |
| **跨页跳转** | 采集节点 → 其机器（服务器台账）/ 其运行时实例（P7）/ 其指派（P10）；交易对 → 所属交易所（P3）；配置域 → 受影响实例（P7，按消费域筛选）；指派分片 → 承载实例 |
| **深链** | 每页可直链（`/nodes/col-binance`、`/config/signal_rules`），便于审计条目中的 `target` 回跳 |

### 3.2 页面总表

| # | 页面 | 数据主表 | 只读/读写 | 主要动作 | 对应 API 分组 |
|---|---|---|---|---|---|
| P0 | 总览 | 派生 | 只读 | 跳转、筛选 | §4.3 |
| P1 | 服务器台账 | `server` | 读写 | 新建 / 编辑 / 改状态 / 删除 | §4.4 |
| P2 | 采集节点 | `collector_node` + `node_runtime` + `node_assignment` | 读写 | 新建 / 编辑 / 范围预览 / 导出配置 / 启停 / 删除 | §4.5 |
| P3 | 交易所与市场 | `exchange` + `exchange_market` | 读写 | **两步创建**（先建所再建市场）/ 编辑 / 启停 / 配额（展开可见） | §4.6 + **14 §6** |
| P4 | 交易对 | `instrument` | 只读 | 筛选 / 分页 / 导出 / **抽屉详情** | §4.7 + **14 §7.2** |
| P4b | 交易对变更历史 | `instrument_change` | 只读 | 按标的 / 时间 / 类型筛选（**并入 P4 抽屉**） | §4.7 + **14 §7.1/7.2** |
| P5 | 币信息 | `asset` + `venue_asset` + `asset_contract` | **双 Tab** + 有限写（`kind` / `external_id`） | 匹配状态 / 手填 CG id / 链与合约地址 / **立即同步** / 交叉跳转 | §4.8 + **14 §7.3** |
| P6 | 八个配置域 | `config_domain` + `config_version` + `audit_log` | 读写 | 查看 / 校验 / 发布 / 回滚 / 导出导入 / 收敛进度 | §4.9 |
| P7 | 实例状态 | `node_runtime` + 收敛比对 | 只读（触发类） | 筛选 / 重推（可选） | §4.10 |
| P8 | 元数据同步 | `sync_run` + catalog 状态 | 只读（触发类） | 立即触发 / 查看轮次与跳过原因 | §4.12 |
| P9 | 审计日志 | `audit_log` | 只读 | 按 actor / action / target / 时间筛选、导出 | §4.13 |
| P10 | 指派结果 | `node_assignment` + `node_runtime` + `server` | 只读（试算） | 三种视图切换 / 试算 | §4.11 |

### 3.3 页面规格

> 每页给出：**展示字段**、**可执行操作**、**数据表**与**现有函数**。
> 「新增」= 该能力在 `crates/control` 中尚无对应函数（映射明细见 §6）。

#### P0 总览

| 项 | 内容 |
|---|---|
| 展示字段 | 八域当前版本（`domain` / `current_version` / `updated_at` / 消费者 / 传播类别 A·B·C，09 §2.2）；实例计数（running / stopped / dead / converged / not_converged / not_converged_timeout）；未收敛清单（实例、域、期望版本、实际版本、持续时长）；最近发布（`audit_log` 最后 10 条）；catalog 状态（`updated_at`、标的数、币数）；同步状态（周期、上次轮次、是否在跑） |
| 操作 | 全部为跳转：未收敛实例 → P7；发布记录 → P6 对应域；catalog → P4 |
| 数据表 | `config_domain`、`audit_log`、`node_runtime`（派生） |
| 现有函数 | `ConfigStore::current_versions()`、`ControlPlane::convergence(now)`、`ConfigStore::audit(domain)`、`Catalog::symbol_count()` / `CatalogSync::last_round_ms()` / `is_running()` |

#### P1 服务器台账

| 项 | 内容 |
|---|---|
| 展示字段 | `id`、`host`、`region`、`roles[]`、`cpu_cores`、`mem_gb`、`disk_gb`、`status`、`labels{}`、`notes`、`created_at`、`updated_at`；派生列：登记节点数、在该机上的运行时实例数与状态 |
| 筛选 | `status`（active / maintenance / retired）、`role`、`q`（host 模糊） |
| 操作 | 新建（表单）、编辑（`If-Match`）、改状态、删除（关联节点存在时拒绝）、查看其上节点 → P2 |
| 数据表 | `server`（12 §3.5）；关联 `collector_node.server_id`、`node_runtime.host` |
| 现有函数 | `SqliteStore::upsert_server` / `list_servers` / `server_by_id` / `server_by_host` |
| 新增 | 按 `status` / `role` / 关键字的分页查询；`status` 修改（`upsert_server` 的更新分支不改 `status`）；`delete_server`（含关联校验）；`host` 关联节点查询 |

> 语义提示（12 §3.5 / 09 §2.3）：本表是**人工登记台账**；`node_runtime.host` 由 register 上报，
> 与之关联但不强校验。页面需并列显示「登记 host」与「实际上报 host」，未匹配的运行时实例标注为**未登记**（不拒绝注册，只提示）。

#### P2 采集节点

| 项 | 内容 |
|---|---|
| 展示字段（列表） | `name`、`enabled`、期望机器（`server_id` → `host`）、实际机器（`node_runtime.host`）、范围摘要（venues / markets / channels / quote / status / `shard_size`）、例外数（include / exclude）、运行状态（`status`）、`applied_version` vs `last_assignment_version`、`last_seen_ts` |
| 展示字段（详情） | 全量 `scope_rules`、`scope_include[]`、`scope_exclude[]`、`runtime_params{}`、`note`、`created_at` / `updated_at`；运行时块（`counters`、`started_at`、`lost_count`、`unconverged_since`、`last_repush_ts`）；指派块（最新 `assignment_version`、`mode`、分片数、标的数、`quota_splits`） |
| 操作 | 新建（`create_node`）、编辑（`upsert_node` + `If-Match`）、启停（`enabled`）、删除（`delete_node`，连带删派生计划、保留运行时行）、**范围预览**（规则 + 例外 → 入选标的与 `unresolved`）、**导出配置**（TOML 下载）、查看指派历史、查看运行时详情 → P7 |
| 数据表 | `collector_node`（12 §3.6 冷）、`node_assignment`（派生）、`node_runtime`（热） |
| 现有函数 | `create_node` / `upsert_node` / `node_by_name` / `list_nodes` / `delete_node` / `export_node_toml` / `server_by_id`；`scope::resolve(scope, include, exclude, catalog)`、`ScopeRules::validate()`；`SqliteStore::load_plans()`（全量计划） |
| 新增 | 定义 + 运行时的联表查询；按节点查 `node_assignment`（分页）；范围预览的 HTTP 封装（`resolve` 已实现、`plane.catalog()` 已公开） |

> 范围预览返回两段：`instruments`（入选逐条，含精度）与 `unresolved`（例外条目未解析的原因，
> 12 §5「不臆造精度、不静默丢」）。预览结果不落库、不下发。

#### P3 交易所与市场

| 项 | 内容 |
|---|---|
| 展示字段（交易所） | `venue`、`name`、`dig3_exchange_id`、`enabled`、`extra{}`、`created_at` / `updated_at` |
| 展示字段（市场） | `market`（spot / swap / future）、`dig3_account_type`（02 §6 映射，如 `swap → FuturesCross`）、`enabled`、`quota_collect` / `quota_backfill` / `quota_metadata`（02 §6：0.50 / 0.35 / 0.15）、`egress_ip` |
| 操作 | 新建 / 编辑 / 启停交易所；新增 / 编辑 / 启停市场；配额份额编辑（写入前校验三项之和与取值范围，校验规则由 02 §6 约束） |
| 数据表 | `exchange`、`exchange_market`（12 §3.1） |
| 现有函数 | 无（DDL 已在 `migrations/0001_init.sql`，读写函数未实现） |
| 新增 | 交易所与市场的 CRUD、分页与筛选、`enabled` 开关、配额字段校验 |

> 字段提示（12 §3.1）：`dig3_account_type` **不设默认值**，必须由运维显式填——写错连不上；
> 页面在市场新建表单中置为必填并给出 02 §6 映射表提示。

#### P4 交易对 / P4b 变更历史

| 项 | 内容 |
|---|---|
| 展示字段 | `venue`、`market`、`native_code`（主键即 canonical，12 §3.3）、`base` / `quote`（`asset` 关联）、`status`（trading / break / delisted）、`instrument_type`、`price_precision`、`qty_precision`、`tick_size`、`step_size`、`min_notional`、`listed_at`、`delisted_at`、`first_seen_at`、`last_synced_at`、`extra{}` |
| 筛选 | `venue`、`market`、`status`、`native_code`（前缀/包含）、`base`、`quote`、`instrument_type`、`last_synced_at` 范围、`sort`（status / last_synced_at / native_code） |
| 变更历史字段 | `id`、`change_kind`（new / delisted / status / precision / type / relisted）、`before` / `after`（JSON diff）、`detected_at`、`sync_run_id` → 跳转 P8 |
| 操作 | 只读；导出（CSV，分页外的全量导出带筛选条件）；变更历史跳转同步轮次 |
| 数据表 | `instrument`、`instrument_change`、`sync_run`（12 §3.3 / §3.4） |
| 现有函数 | 过渡期可读 `ControlPlane::catalog()`（`Catalog.symbols`） |
| 新增 | `instrument` 分页/筛选查询、单条详情、CSV 导出；`instrument_change` 分页查询与 before/after 解析 |

> 只读依据（12 §4.1）：本表由 5 分钟同步 upsert，`row_hash` 变化才写 `instrument_change`。
> 界面不提供状态改写入口；人工干预的正确路径是 `InstrumentBlocklist` 域（09 §2.1）。

#### P5 币信息

| 项 | 内容 |
|---|---|
| 展示字段（全局层 `asset`） | `id`、`code`、`name`、`kind`（coin / token / stock / index）、`created_at` / `updated_at`、各所覆盖数 |
| 展示字段（各所层 `venue_asset`） | `venue`、`precision`、`networks[]`、`deposit_enabled`、`withdraw_enabled`、`status`（所方状态串）、`extra{}`、`row_hash`、`last_synced_at` |
| 视图 | 「全局」（按 `code` 聚合、跨所对比精度与充提状态）/「按所」（`venue` 过滤） |
| 筛选 | `kind`、`q`（code / name）、`venue`、`deposit_enabled` / `withdraw_enabled` |
| 操作 | 只读；导出 |
| 数据表 | `asset`、`venue_asset`（12 §3.2） |
| 现有函数 | 过渡期可读 `ControlPlane::catalog().currencies` |
| 新增 | 两层表的查询与聚合、分所筛选、导出 |

#### P6 八个配置域

| 项 | 内容 |
|---|---|
| 列表字段 | `domain`、中文名、当前版本（`current_version`，0 = 未发布）、`updated_at`、消费者（09 §8 接口表）、传播类别（A / B / C，09 §2.2）、最近发布者与时间 |
| 详情字段 | 当前文档 `content`、`version`、`publisher`、`ts`、`reason`、`prev_version`、`content_hash`；`ETag`（并发控制用，§5.3） |
| 版本历史字段 | `version`、`publisher`、`published_at`、`reason`、`prev_version`、`content_hash`、与上一版的 `diff[]`（`diff_summary`） |
| 审计字段 | `domain`、`version`、`prev_version`、`publisher`、`ts`、`reason`、`diff[]` |
| 操作 | 查看（JSON + 结构化摘要）、**校验**（不落库）、**发布**（版本 +1）、**回滚**（发布为目标版本内容为新版本）、导出 JSON、导入 JSON（= 发布）、查看该域收敛进度（消费实例的期望/实际版本） |
| 数据表 | `config_domain`、`config_version`、`audit_log`（12 §3.7） |
| 现有函数 | `validate::validate(domain, payload, store)`；`ControlPlane::publish_domain(...)`、`ControlPlane::rollback(...)`；`ConfigStore::current` / `get` / `audit` / `current_versions`；`store::diff_summary`；`ControlPlane::convergence` |
| 新增 | 版本列表分页；发布请求的 `base_version` 基线校验；`content_hash` → ETag 组装；域级收敛过滤视图 |

> 页面需按 09 §2.2 标注传播行为：
> **A** 广播通知 + 定向应答；**B** A + 定向推送（`SubscribePlan`）；**C** 不发通知（`ProcessCapacity`，效果以 `assignment_version` 体现）。
> 「未发布」（`current_version = 0`）为独立状态，不参与版本历史。

#### P7 实例状态（运行时）

| 项 | 内容 |
|---|---|
| 展示字段 | `instance_id`、`node_kind`、`host`（上报）、`status`（registered / running / stopped / dead）、收敛（是否收敛、`domain`、`expected` / `actual`、`held_ms`）、`applied_version`、`last_assignment_version`、`started_at`、`last_seen_ts`、`lost_count`、`unconverged_since`、`last_repush_ts`、`counters{}`（含 `cfg.<domain>`、`capacity.*`）、自报承载能力（`ReportedCapacity`） |
| 筛选 | `node_kind`、`status`、`converged`（true / false）、`host`、`domain`（按消费域看未收敛） |
| 操作 | 只读；展开 `counters`；跳转到对应节点定义（P2）与指派（P10）；**手动重推**（可选，触发类） |
| 数据表 | `node_runtime`（12 §3.6 热） |
| 现有函数 | `ControlPlane::instances()`、`ControlPlane::convergence(now)`、`SqliteStore::load_keys()`、`OpsRegistry::get`、`validate::consumed_domains(kind)` |
| 新增 | 分页与筛选；域维度过滤视图；手动重推入口（`repush_plan` 为私有，公开入口需新增） |

> 收敛判据（09 §4.4）：采集比 `applied_version == last_assignment_version`；
> 其余节点比 `counters["cfg.<domain>"] == 该域当前版本`；控制面不参与比对。
> 「失联」判据 = `shutdown` 缺失 + 心跳断 3×5s（10 §5），控制面只标记、不改指派（09 §7）。

#### P8 元数据同步

| 项 | 内容 |
|---|---|
| 展示字段（状态） | 周期 `interval_ms`（默认 300000）、`last_round_ms`、是否在跑、下次到期时刻、`due`；配额（`share` 缺省 0.15、基准速率、每所令牌桶余量） |
| 展示字段（轮次） | `sync_run.id` / `started_at` / `finished_at` / `outcome{}`（逐所结果）/ `error`；本轮差异（新增/下架/变更标的与币）、`skipped[]`（原因：`throttled` / `unsupported` / `fetch failed`）、`fetched_symbols` / `fetched_currencies`、是否触发指派重算 |
| 操作 | 立即触发（绕过周期门槛）、查看轮次详情、跳转本轮产生的 `instrument_change`（P4b） |
| 数据表 | `sync_run`（12 §3.4）、catalog 内部状态（文件 `state/catalog.json`，非配置域） |
| 现有函数 | `ControlPlane::run_metadata_sync(now)`、`ControlPlane::metadata_targets()`、`CatalogSync::{catalog, last_round_ms, is_running, due, interval_ms}` |
| 新增 | `&CatalogSync` 访问器（`ControlPlane` 现只暴露 `catalog() → &Catalog`）；强制触发（`run_metadata_sync` 受 `due()` 与 `running` 双重门槛）；`sync_run` 写入与分页查询；差异视图的持久化查询 |

> 零动作约定（09 §2.4 / 12 §4.2）：与上一轮 `row_hash` 无差异 → 不 bump 版本、不发 `ConfigChanged`、不发 `md.meta`。
> 页面需明确区分「轮次执行了但零差异」与「轮次未到期/被跳过」。

#### P9 审计日志

| 项 | 内容 |
|---|---|
| 展示字段 | `id`、`actor`、`action`（publish / rollback / node_create / node_assign / …）、`target`、`before_hash`、`after_hash`、`detail{}`（含 `AuditEntry` 的 `diff[]`）、`ts` |
| 筛选 | `action`、`target`、`actor`、`ts` 区间、关键字 |
| 操作 | 只读、导出、跳转（`target` 为域 → P6；为节点 → P2） |
| 数据表 | `audit_log`（12 §3.7） |
| 现有函数 | `ConfigStore::audit(domain)`（**仅** `action IN ('publish','rollback')` 且按域过滤） |
| 新增 | 全表分页查询与多维筛选 |

#### P10 指派结果

| 视图 | 内容 | 组成 |
|---|---|---|
| 按实例 | 每实例的 `assignment_version`、`mode`、`generated_at`、分片列表（`venue` / `market` / `shard_idx` / 标的数）、`quota_splits`（逐所分摊） | `node_assignment.plan` |
| 按机器 | 分片 → 承载机器（`node_runtime.host`，未上报则显示 `server_id` 期望机器）；每机分片数与标的数 | `node_assignment` + `node_runtime` + `server` |
| 按交易所 | 每所持有该所的进程数（配额分摊分母，09 §5.3）、未指派分片及原因 | `AssignmentResult.holders` / `unassigned` |
| 未指派 | `(venue, market, shard_idx, 原因)`（容量不足 / 无候选所） | `assign::AssignmentResult::unassigned` |

| 项 | 内容 |
|---|---|
| 筛选 | `venue`、`market`、`instance`、`host`、`assignment_version` |
| 操作 | 只读；查看单实例完整 `SubscribePlan`（JSON）；**试算**（不下发：按当前域文档与活实例重新计算并展示差异） |
| 数据表 | `node_assignment`、`node_runtime`、`collector_node`、`server` |
| 现有函数 | `SqliteStore::load_plans()`（全量、按版本升序）、`ControlPlane::plan_of(instance)`、`assign::assign(AssignRequest)`、`ControlPlane::gate(instance)` |
| 新增 | 按节点/版本分页查询 `node_assignment`；按 host 聚合视图；`quota_splits` 解析列；试算入口（需从注册表构造 `Candidate` 集，`recompute_inner` 为私有） |

### 3.4 状态色与状态语义

| 语义 | 取值来源 | 色 | 备注 |
|---|---|---|---|
| 收敛 | 实例不在 `Convergence::not_converged` | 绿 | 期望值与实际值相等 |
| 未收敛（窗口内） | `held_ms < 30000`（09 §4.4 超时阈值） | 黄 | 预期下一心跳（≤5s）恢复 |
| 未收敛（超时） | `held_ms ≥ 30000` | 红 | 控制面已重推；计入 `ops.control`，告警由 06 产出 |
| 失联 / 判死 | `status = dead` | 灰底 + 红标 | 3×5s 无心跳（10 §5）；不触发再指派 |
| 正常下线 | `status = stopped` | 灰 | 收到 `shutdown` |
| 运行中 | `status = running` / `registered` | 蓝 | |
| 节点启用 | `enabled = 1` | 绿 | |
| 节点停用 | `enabled = 0` | 灰 | |
| 交易所 / 市场启用 | `enabled = 1` | 绿 | |
| 维护中 / 已退役 | `server.status = maintenance` / `retired` | 黄 / 灰 | |
| 未发布 | `current_version = 0` | 灰 | 不计入版本历史 |
| 标的状态 | `instrument.status` | 按值映射：trading 绿 / break 黄 / delisted 灰 | 所方口径原样 |
| 未解析例外 | `ScopeOutcome::unresolved` | 黄 | 12 §5：如实汇报，不静默丢 |
| 未登记机器 | `node_runtime.host` 不在 `server` 表 | 黄 | 12 §3.5：只告警，不拒绝注册 |

### 3.5 页面间跳转

| 起点 | 终点 | 触发 |
|---|---|---|
| P0 未收敛清单 | P7（按 `converged=false` 预筛选） | 点击实例 |
| P0 / P9 审计条目 | P6 对应域（定位到版本） | 点击 `target` |
| P2 节点详情 | P1（期望机器）、P7（运行时实例）、P10（指派） | 字段链接 |
| P10 分片 | P7（承载实例）、P3（交易所与市场） | 字段链接 |
| P4b 变更 | P8（`sync_run_id`）、P3（所属市场） | 字段链接 |
| P6 收敛进度 | P7（按消费域筛选） | 点击实例 |
| P8 轮次 | P4b（本轮变更） | 点击 `outcome` |

---

## 4. 服务接口

### 4.1 通则

| 项 | 约定 |
|---|---|
| 基址 | `http(s)://<control-host>:<port>/api/v1`；`<port>` 来自 `control.toml` 的 `[http].addr` |
| 鉴权 | `Authorization: Bearer <jwt>`（§5.1 登录换取）；`/healthz` 与 `/auth/login` 除外 |
| 操作者标识 | 请求头 `X-Actor`（缺省取 token 标识）；写入 `publisher` 与 `audit_log.actor`，格式 `<instance>/<actor>`（09 §3.1，如 `cp-1/alice`） |
| 内容类型 | 请求与响应均 `application/json; charset=utf-8`；文件下载接口另标 |
| 时间 | 一律 UTC 毫秒整数（03 §5.1 信封口径） |
| 分页 | `?limit=`（默认 50，最大 500）、`?offset=`；响应 `{"items":[…],"total":N,"limit":50,"offset":0}`。**仅列出 `limit=&offset=` 的端点分页**；未列出的（如 `GET /exchanges/{v}/markets`、`GET /assignments/holders`）返回**裸数组** |
| 排序 | `?sort=<field>&order=asc\|desc`；各接口列出允许字段 |
| 键编码 | 路径中的复合键用 `:` 连接并做 URL 编码，如 `/instruments/binance%3Aswap%3ABTCUSDT` 采用分段路径 `/instruments/{venue}/{market}/{native_code}` |
| 幂等 | 写接口不设幂等键；并发正确性由 §5.3 的版本基线 / `If-Match` 承担 |
| 版本演进 | 破坏性变更升 `/api/v2`；新增字段不视为破坏 |

### 4.2 错误模型

响应体统一：

```json
{ "error": { "code": "validation", "message": "shard_size 必须 >= 1（09 §5.1）", "detail": {} } }
```

| `code` | HTTP | 来源 |
|---|---|---|
| `unauthenticated` | 401 | 缺失 / 无效 token |
| `forbidden` | 403 | 角色不足（§5.1 二期） |
| `not_found` | 404 | `ControlError::NotFound`（域 / 版本 / 实例 / 节点 / 服务器） |
| `conflict` | 409 | 版本基线不符（§5.3）、同名节点已存在、删除被关联记录阻止 |
| `precondition_failed` | 412 | `If-Match` 与当前 `ETag` 不符 |
| `precondition_required` | 428 | 写接口缺少 `If-Match` / `base_version` |
| `validation` | 400 | `ControlError::Validation`（schema、跨域引用、字段校验） |
| `illegal_state` | 422 | `ControlError::Illegal`（状态不允许该操作） |
| `busy` | 503 | 控制面锁等待超时、同步轮次进行中拒绝重入 |
| `internal` | 500 | 数据库 / IO / JSON / 协议 / 事件中心错误（`ControlError::{Db,Io,Json,Protocol,Ec}`），`message` 不回显内部细节 |

映射在 HTTP 适配层完成：`ControlError` 变体 → `code` + HTTP 状态；重复节点名等本属 `Illegal`
的语义在适配层先行查询判重后返回 `conflict`。

### 4.3 概览

| Method | Path | 说明 | 响应要点 | 函数 |
|---|---|---|---|---|
| GET | `/healthz` | 存活探针（无鉴权） | `{status:"ok",instance_id,started_at}` | `ControlPlane::config()` |
| GET | `/overview` | 总览聚合 | `domains[]`、`instances{running,stopped,dead,converged,not_converged,not_converged_timeout}`、`recent_publishes[]`、`sync{…}`、`catalog{updated_at,symbols,currencies}` | `current_versions` + `convergence` + `audit` + `catalog()` |
| GET | `/meta` | 八域定义与消费关系（前端渲染用） | `domains[{name,display,consumers[],push:"A"\|"B"\|"C"}]` | `validate::consumed_domains` + 常量表 |

### 4.4 服务器台账

| Method | Path | 请求 | 响应 | 函数 |
|---|---|---|---|---|
| GET | `/servers?status=&role=&q=&limit=&offset=&sort=&order=` | — | `ServerRecord[]` | `list_servers` + 分页包装**（分页新增）** |
| POST | `/servers` | `{host,region,roles[],cpu_cores,mem_gb,disk_gb,labels{},notes}` | 201 `ServerRecord` | `upsert_server` |
| GET | `/servers/{host}` | — | 200 `ServerRecord` | `server_by_host` |
| PUT | `/servers/{host}` | 同 POST + `If-Match` | 200 `ServerRecord` | `upsert_server`**（status 单独接口）** |
| PATCH | `/servers/{host}/status` | `{status}` + `If-Match` | 200 `ServerRecord` | **新增** |
| DELETE | `/servers/{host}` | `If-Match` | 204；有关联节点 → 409 | **新增** |
| GET | `/servers/{host}/nodes?limit=&offset=` | — | 该机节点（期望） | **新增**（按 `server_id` 过滤 `list_nodes`） |
| GET | `/servers/{host}/instances` | — | 该机运行时实例 | **新增**（按 `host` 过滤 `instances()`） |

### 4.5 采集节点

| Method | Path | 请求 | 响应 | 函数 |
|---|---|---|---|---|
| GET | `/nodes?enabled=&server_id=&status=&converged=&host=&q=&limit=&offset=` | — | 节点 + 运行时联表行 | `list_nodes` + `instances()`**（联表新增）** |
| POST | `/nodes` | `{name,enabled,server_id,scope{…},scope_include[],scope_exclude[],runtime_params{},note}` | 201 `NodeRecord` | `create_node`（内部调 `ScopeRules::validate`） |
| GET | `/nodes/{name}` | — | `NodeRecord` + 运行时块 + 最新指派摘要 | `node_by_name` + `registry.get`**（聚合新增）** |
| PUT | `/nodes/{name}` | 同 POST + `If-Match` | 200 `NodeRecord` | `upsert_node` |
| PATCH | `/nodes/{name}/enabled` | `{enabled}` + `If-Match` | 200 `NodeRecord` | `node_by_name` → `upsert_node`（读改写组合） |
| DELETE | `/nodes/{name}` | `If-Match` | 204 | `delete_node` |
| GET | `/nodes/{name}/scope/preview` | — | `{instruments[],unresolved[[entry,reason]],counts{}}` | `scope::resolve` + `ControlPlane::catalog()`**（封装新增）** |
| GET | `/nodes/{name}/config.toml` | — | `text/plain` 下载 | `export_node_toml` |
| GET | `/nodes/{name}/assignments?limit=&offset=` | — | 指派历史（版本降序） | **新增**（`load_plans` 全量过滤不适用于分页） |
| GET | `/nodes/{name}/assignments/latest` | — | 最新 `SubscribePlan` + `quota_splits` | `plan_of` / `load_plans`**（按节点封装新增）** |
| GET | `/nodes/{name}/runtime` | — | `SnapshotKey` + 收敛判定 | `registry.get` + `convergence()`**（封装新增）** |

### 4.6 交易所与市场

> 本组接口的数据表（`exchange` / `exchange_market`）DDL 已就绪，**读写函数全部新增**。

| Method | Path | 请求 | 响应 |
|---|---|---|---|
| GET | `/exchanges?enabled=&q=&limit=&offset=` | — | 交易所行 + 市场数 |
| POST | `/exchanges` | `{venue,name,dig3_exchange_id,enabled,extra{}}` | 201 |
| GET | `/exchanges/{venue}` | — | 交易所行 + 市场列表 |
| PUT | `/exchanges/{venue}` | 同 POST + `If-Match` | 200 |
| PATCH | `/exchanges/{venue}/enabled` | `{enabled}` + `If-Match` | 200 |
| DELETE | `/exchanges/{venue}` | `If-Match` | 204（级联删除市场与交易对 → 需二次确认参数） |
| GET | `/exchanges/{venue}/markets` | — | 市场行 |
| POST | `/exchanges/{venue}/markets` | `{market,dig3_account_type,enabled,quota_collect,quota_backfill,quota_metadata,egress_ip}` | 201 |
| PUT | `/exchanges/{venue}/markets/{market}` | 同上 + `If-Match` | 200 |
| PATCH | `/exchanges/{venue}/markets/{market}/enabled` | `{enabled}` + `If-Match` | 200 |

字段校验：`venue` / `market` 非空；`dig3_account_type` 必填（12 §3.1）；三项配额 ∈ [0,1]（02 §6）。

### 4.7 交易对

| Method | Path | 说明 | 函数 |
|---|---|---|---|
| GET | `/instruments?venue=&market=&status=&native_code=&base=&quote=&instrument_type=&synced_after=&synced_before=&limit=&offset=&sort=&order=` | 分页筛选；`sort` 允许 `native_code` / `status` / `last_synced_at` / `listed_at` | **新增**（过渡期可读 `ControlPlane::catalog()`） |
| GET | `/instruments/{venue}/{market}/{native_code}` | 单条详情（canonical 三段路径） | **新增** |
| GET | `/instruments/export?…同筛选` | CSV 流式导出（`Content-Disposition`） | **新增** |
| GET | `/instruments/changes?venue=&market=&native_code=&change_kind=&since=&until=&sync_run_id=&limit=&offset=` | 变更历史分页 | **新增** |
| GET | `/instruments/changes/{id}` | 单条变更（`before` / `after` JSON） | **新增** |

响应行字段见 §3.3 P4；`base` / `quote` 返回 `asset.code`（经 `base_asset_id` / `quote_asset_id` 关联）。

### 4.8 币信息

| Method | Path | 说明 | 函数 |
|---|---|---|---|
| GET | `/assets?kind=&q=&venue=&deposit_enabled=&withdraw_enabled=&limit=&offset=` | 全局层 + 可选按所过滤 | **新增** |
| GET | `/assets/{code}` | 全局记录 + 各所 `venue_asset[]` | **新增** |
| GET | `/venues/{venue}/assets?q=&limit=&offset=` | 按所视图 | **新增** |
| GET | `/assets/export?…` | CSV 导出 | **新增** |
| PATCH | `/assets/{code}` | `{kind?, external_id?}` + `If-Match`；手填 CG id 后服务端置 `match_status = matched` | **新增**（`docs/14 §9`） |
| GET | `/assets/{code}/contracts` | 链与合约地址列表（`asset_contract` 子表） | **新增**（`docs/14 §9`） |

### 4.9 配置域

| Method | Path | 请求 | 响应 | 函数 |
|---|---|---|---|---|
| GET | `/config/domains` | — | 八域概览（版本、更新时间、消费者、传播类别、最近发布） | `current_versions` + `audit`**（聚合新增）** |
| GET | `/config/{domain}` | — | `{domain,version,content,publisher,ts,reason,prev_version,content_hash,etag}` + `ETag` 头 | `ConfigStore::current` |
| PUT | `/config/{domain}` | `{content,base_version,reason}` + `If-Match` | 201 = 发布（同 POST） | 基线校验**新增** + `publish_domain` |
| GET | `/config/{domain}/versions?limit=&offset=` | — | 版本历史（含 `diff[]`） | **新增分页**（逐版本可读 `get`） |
| GET | `/config/{domain}/versions/{version}` | — | 指定版本文档 | `ConfigStore::get` |
| GET | `/config/{domain}/versions/{version}/diff` | — | 与相邻版本的结构 diff | `store::diff_summary`**（封装新增）** |
| GET | `/config/{domain}/audit` | — | 该域审计 | `ConfigStore::audit` |
| POST | `/config/{domain}/validate` | `{content}` | 200 `{ok:true,domain}` / 400 `{code:"validation",message,detail{path}}` | `validate::validate` |
| POST | `/config/{domain}/publish` | `{content,base_version,reason}` | 201 `{doc,diff[],notified,recompute,convergence}` | 基线校验**新增** + `ControlPlane::publish_domain` + `publish_all` |
| POST | `/config/{domain}/rollback` | `{target_version,base_version}` | 201 `{doc,diff[],notified,recompute,convergence}`（`reason = "rollback to vN"`） | 基线校验**新增** + `ControlPlane::rollback` + `publish_all` |
| GET | `/config/{domain}/export` | — | 文档 JSON 下载（含 `version` / `publisher` 元数据信封） | `current`**（封装新增）** |
| GET | `/config/{domain}/convergence` | — | 该域消费实例的 `expected` / `actual` / `held_ms` | `ControlPlane::convergence`**（域过滤新增）** |

**发布响应示例**

```json
{
  "doc": { "domain": "signal_rules", "version": 8, "publisher": "cp-1/alice",
           "ts": 1791000000000, "reason": "调整 RSI 阈值", "prev_version": 7 },
  "diff": ["~ rules[0].emit.value", "+ rules[1]"],
  "notified": 3,
  "recompute": false,
  "convergence": { "converged": 4, "not_converged": [
    { "instance_id": "sig-1", "domain": "signal_rules", "expected": 8, "actual": 7, "held_ms": 0 } ] }
}
```

`notified` = 出站消息条数（`publish_domain` 返回的 `Vec<Outbound>` 长度）；`recompute` 表示本次发布
是否触发指派重算（`SubscribePlan` / `InstrumentBlocklist` / `ProcessCapacity` 三域，09 §5.4）；
`ProcessCapacity` 不发 `ConfigChanged`（09 §2.2 C），`notified` 中不含变更通知。

### 4.10 运行时状态

| Method | Path | 说明 | 函数 |
|---|---|---|---|
| GET | `/runtime/instances?node_kind=&status=&converged=&host=&domain=&limit=&offset=` | 实例列表 + 收敛状态 | `ControlPlane::instances()` + `convergence()`**（联表新增）** |
| GET | `/runtime/instances/{instance_id}` | 详情（`counters`、自报能力、门状态） | `registry.get` + `convergence()`**（封装新增）** |
| GET | `/convergence?domain=&timeout_ms=` | 全量收敛比对快照 | `ControlPlane::convergence(now)` |
| POST | `/runtime/instances/{instance_id}/repush` | 手动重推（可选，触发类） | **新增**（`repush_plan` 为私有） |
| GET | `/runtime/counts` | 计数器（供 P0） | `convergence()`**（封装新增）** |

### 4.11 指派结果

| Method | Path | 说明 | 函数 |
|---|---|---|---|
| GET | `/assignments?instance=&venue=&market=&host=&assignment_version=&limit=&offset=` | 分片行（视图：按实例 / 按机器 / 按交易所由 `view` 参数切换） | **新增**（`load_plans` 无过滤与分页） |
| GET | `/assignments/{instance_id}` | 该实例最新 `SubscribePlan` + `quota_splits` | `ControlPlane::plan_of`**（封装新增）** |
| GET | `/assignments/{instance_id}/versions/{version}` | 指定版本快照 | **新增**（按 `(node_id, assignment_version)` 查） |
| GET | `/assignments/holders` | 每所持有进程数（配额分摊分母） | **新增**（由 `load_plans` 聚合） |
| GET | `/assignments/unassigned` | 未指派分片与原因 | **新增**。实现为**试算**（与 dry-run 同源，只读不下发）——落存需改 `recompute_inner`（私有）；与「最近一次实际下发的 unassigned」在存在未下发变更时会不同 |
| POST | `/assignments/dry-run` | `{instance_id?}` 试算：按当前域文档与活实例重算并返回差异，**不下发** | `assign::assign`**（包装新增）** |

### 4.12 元数据同步

| Method | Path | 说明 | 函数 |
|---|---|---|---|
| GET | `/sync/status` | 周期、上次轮次、是否在跑、`due`、配额、catalog 概览 | `CatalogSync::{interval_ms,last_round_ms,is_running,due}` + `catalog()`**（访问器与聚合新增）** |
| POST | `/sync/runs` | **立即触发**；返回 202 + 本轮 `SyncOutcome`（`ran` / `changed` / `recompute_ran` / `diff` / `skipped` / 计数） | **新增**（`run_metadata_sync` 受 `due()` 与 `running` 门槛） |
| GET | `/sync/runs?limit=&offset=` | 轮次历史 | **新增**（`sync_run` 无读写函数） |
| GET | `/sync/runs/{id}` | 单轮详情（`started_at` / `finished_at` / `outcome{}` / `error`） | **新增** |
| GET | `/sync/targets` | 本轮覆盖的 `(venue, market)` 集合与份额 | `ControlPlane::metadata_targets()`**（封装新增）** |
| POST | `/sync/offchain/runs` | 立即触发**外部币信息同步**（202）；独立记录 `offchain_sync_run` | **新增**（`docs/14 §9`） |
| GET | `/sync/offchain/status` | 上一轮结果：`{running, last_run, detail_synced_at}`；`outcome.rate_limited` 为**计数（0/1）**、`outcome.detail_failed` 为失败币数 | **新增**（`docs/14 §9`） |

并发语义：一轮未完成不重入（09 §2.3）；重复触发返回 409 / 503。

### 4.13 审计日志

| Method | Path | 说明 | 函数 |
|---|---|---|---|
| GET | `/audit?action=&target=&actor=&since=&until=&q=&limit=&offset=&sort=ts&order=desc` | 全表分页（`idx_audit_ts`） | **新增**（`audit(domain)` 仅覆盖 publish/rollback 且按域过滤） |
| GET | `/audit/{id}` | 单条（含 `detail` 全文与 `diff[]`） | **新增** |
| GET | `/audit/export?…同筛选` | CSV 导出 | **新增** |

---

## 5. 关键设计点

### 5.1 鉴权：用户名密码登录 + JWT

**选定：`POST /auth/login`（用户名 + 密码）签发 JWT，后续请求带 `Authorization: Bearer <jwt>`。**

| 方案 | 评价 | 判定 |
|---|---|---|
| **用户名密码 + JWT（选定）** | 有真实身份（`sub`）、有角色、有到期时间；不需分发静态密钥；过期即失效；审计 `actor` 取自 `sub`，满足 09 §4.5「谁改的」 | **选定** |
| 静态 Bearer token（原方案） | 无身份粒度、泄露即长期有效、无到期；多操作者只能靠分发多个 token | 弃用（原 `--token` 已移除） |
| Basic | 需 TLS 否则明文；浏览器重发行为不可控；无到期 | 不选 |
| 内网免密 | 写操作无归属，审计 `actor` 退化为统一值 | 可作为二层中的第一层，不单独使用 |

| 项 | 约定 |
|---|---|
| 登录端点 | `POST /api/v1/auth/login`，**免鉴权**；成功 `200`，凭证错误 **`401 invalid_credentials`**（不区分「用户不存在/密码错误」，防用户名枚举） |
| 令牌 | **JWT（HS256）**；claims：`sub`（用户名）、`role`（`read`\|`write`）、`iat`、`exp`、`iss = ponk-control` |
| 有效期 | 默认 **8h**（可配 `--jwt-ttl`）；到期返回 `401 token_expired`（前端清会话跳登录） |
| 密钥 | **配置文件 `control.toml` 的 `[auth].jwt_secret`**（或 `--jwt-secret` 临时覆盖）；**缺失则 HTTP 服务拒绝启动**；`--config` 指定路径（默认 `./control.toml`），优先级 **CLI > 文件 > 默认**，**不读环境变量**（不得自动生成：重启会使令牌全部失效，多实例也不一致） |
| 存储与端口 | 同在 `control.toml`：`[store].path`（SQLite 目录）、`[http].addr`（有值即开启）。样例见 `crates/control/control.example.toml`；查找顺序 `--config <path>` > `./control.toml` > `crates/control/control.toml` |
| 文件权限 | 该文件含密钥与首启口令：`chmod 600 control.toml`，**勿提交进版本库** |
| 密码 | 仅存 **argon2id 哈希**（`control_user.password_hash`），**任何接口都不返回**；比对恒定时间；**不进八域、不进日志** |
| 用户 | 用户存 `control_user` 表（`docs/12 §3.8`）；初始用户用 CLI 创建：`ponk-control user add --username <u> --role <read\|write>` |
| 角色 | `read` = 全部 GET；`write` = 写接口与触发类。角色取自**用户表**（JWT 里的 `role` 仅作回退），不再由 token 自声明 |
| **吊销** | 中间件**每次请求回查 `control_user`**：账号不存在 / 已停用 → `401 unauthenticated`；角色以**库**为准 ⇒ **降权与停用对已签发令牌立即生效**，不等 `exp`（代价 = 每请求一次本地 SQLite 读，WAL 下亚毫秒；回查失败**闭合**，回 500 不放行）。不引入 `jti` 吊销列表或短 TTL 缓存 |
| 操作者 | 默认 `publisher = <instance>/<sub>`；请求可用 `X-Actor` 覆盖 `sub`（仍受限于角色） |
| 界面侧 | 登录页提交用户名密码 → 存 JWT 于 `sessionStorage`（关闭标签即失效，不写 `localStorage`）；401 → 清会话跳登录；角色读 JWT |
| 作用域 | 仅覆盖本 HTTP 面；CLI 与总线不新增认证面（CLI 是本机进程，09 §1 引导配置例外） |

**公开端点**（免 JWT）：`/healthz`（含 `/api/v1/healthz`）、`/api/v1/auth/login`。
  5 个认证端点**同时注册裸路径**（`/auth/login` 等）作为别名，两者均免鉴权——统一以
  **`/api/v1/...`** 为准（前端 `VITE_API_BASE=/api/v1` 拼的就是它）。其余一律需 JWT，否则 `401 unauthenticated`。

**防爆破**：`control_user` **不设** `failed_attempts` / `locked_until` / `last_login_at`
（`docs/12 §3.8` 照此 DDL）——账号锁定属于**反向代理层**的限流职责（本面已要求内网 + TLS 终止）；
若后续需要应用层锁定，再加代次字段与 `jti` 吊销一并评估。

**首启引导（默认账号）**：`control_user` **为空**时，HTTP 面启动即创建内置管理员：

| 项 | 值 |
|---|---|
| 用户名 | `admin` |
| 密码 | **配置文件 `[auth].admin_password`**（或 `--admin-password` 临时覆盖）优先；**未设置则用内置默认 `admin123`**。**不提供随机生成**（`--admin-password auto` 已删）：它要求把口令打进日志，与「口令不得出现在日志」直接冲突 |
| 角色 | `write` |
| 触发条件 | **仅当用户表为空**；之后永不重建、永不动已有用户（改密走 `PATCH /auth/users/{u}`） |
| 提示 | 启动日志与登录响应均标注 `default_password: true`，前端登录后给出"正在使用默认密码，请修改"提示 |

**理由与风险**：内置默认是公开已知凭据，故**只在空库首次创建** + **强制提示** + **生产必须用环境变量覆盖**
（HTTP 面本就要求内网部署 + 反代终止 TLS）。这样既保留"开箱可登录"，又不会在长期运行的部署里留下固定口令。

替代方案（已否决）：完全无默认、要求先跑 `ponk-control user add` —— 运维首次启动会卡在
"没有账号、进不去页面"；随机密码打印到日志也保留为可选（`--admin-password auto`）。

### 5.1.1 接口：认证

| 方法 | 路径 | 请求 | 响应 | 说明 |
|---|---|---|---|---|
| POST | `/auth/login` | `{username, password}` | `200 {access_token, token_type:"Bearer", expires_in, role, username}` | 免鉴权；失败 `401 invalid_credentials`；无用户 `403 no_users` |
| GET | `/auth/me` | — | `200 {username, role, exp, iat}` | 从 JWT 解析身份；用于前端恢复会话与角色 |
| GET | `/auth/users` | — | `200 {items[]}` | 列表**不含**密码哈希；`write` 才可读 |
| POST | `/auth/users` | `{username, password, role}` | `201 {username, role}` | 建用户（`write`）；用户名重复 `409` |
| PATCH | `/auth/users/{username}` | `{password?, role?, disabled?}` | `200` + `UserRecord` | 改密 / 改角色 / 停用（`write`）；响应含更新后的记录 |
| — | **CLI 恢复路径** | `ponk-control user set-password --username <u> [--password <p>]` | — | **忘记口令时的唯一路径**：`PATCH` 需要 write 令牌，而手边恰好没有有效令牌 |

### 5.2 配置发布流程

**流程（09 §4.3 的界面实现）**：

```text
① 编辑器载入 GET /config/{domain}（取 content + etag + base_version）
② 用户编辑 → POST /config/{domain}/validate   → 失败：错误定位到 JSON 路径，不落库
③ 用户提交 → POST /config/{domain}/publish {content, base_version, reason}
      ├─ 基线校验：current_version != base_version → 409（返回当前版本与 diff 摘要）
      ├─ ControlPlane::publish_domain
      │     ① validate::validate（schema + 跨域引用）
      │     ② ConfigStore::publish：版本 = 当前 + 1，内容入历史，current 前移，写审计
      │     ③ make_changed → ConfigChanged（ProcessCapacity 跳过，09 §2.2 C）
      │     ④ 三域触发 recompute（09 §5.4）→ 逐实例 SubscribePlan 下发
      └─ publish_all(outbound) → 事件中心
④ 响应返回 doc / diff / notified / recompute / convergence 快照
⑤ 页面进入「收敛进度」视图：轮询 GET /config/{domain}/convergence
      ├─ 每实例：expected（当前域版本）vs actual（心跳 counters["cfg.<domain>"]）
      ├─ ≤30s 未收敛（黄）→ 控制面自动重推（09 §4.4）
      └─ >30s 未收敛（红）→ 计入 ops.control，告警由 06 产出（09 §6.5）
⑥ 全部收敛（绿）→ 结束；任一实例持续红 → 页面给出跳转 P7
```

**回滚 = 发布新版本（09 §4.5）**：

| 项 | 约定 |
|---|---|
| 请求 | `POST /config/{domain}/rollback {target_version, base_version}` |
| 语义 | 取 `target_version` 内容重新发布为**新版本号**，`current` 不倒回；`reason = "rollback to vN"` |
| 界面 | 版本历史列表上「回滚到此版本」按钮；二次确认展示「将发布为 vN+1」而非「恢复为 vN」 |
| 审计 | 产生 `action = rollback` 的新审计条目，与普通发布同流程 |
| 校验 | 回滚同样走 `validate`（引用可能已变化，例如告警引用的 signal_id 已被后续版本移除） |

**界面必须明确展示的语义**：

| 语义 | 展示 |
|---|---|
| 版本只前进 | 版本历史按 `version` 降序，回滚产生新行，不出现「版本倒流」 |
| 通知不等于生效 | 「已发布」≠「已收敛」；两段状态分色显示（§3.4） |
| 域是唯一版本单位 | 同域内单条规则未变也随域发布获得新版本（09 §4.1），diff 视图标注「本次未改动的规则」 |
| `ProcessCapacity` 无通知 | 页面标注「本域不发变更通知，效果以 `assignment_version` 变化体现」 |

### 5.3 并发控制

**选定：配置域用 `base_version` 版本基线（409 冲突）；行级记录用 `ETag` / `If-Match`（412）；不采用 last-write-wins。**

| 对象 | 机制 | 冲突响应 | 理由 |
|---|---|---|---|
| 八个配置域 | 提交体携带 `base_version`（读取时的 `current_version`）；服务端比对当前版本 | 409 + 当前版本 + `diff[]`（他人已发布的变更摘要） | 域版本是天然的乐观锁（09 §4.1）；`ConfigStore::publish` 只保证「版本 = 当前 + 1」，不检查提交方基线，基线校验必须在 API 层完成 |
| 服务器 / 节点定义 | `ETag = W/"<id>-<updated_at>"`；写请求必须带 `If-Match` | 412（不匹配）/ 428（缺失） | 行级无版本列，`updated_at` 可作代用版本；丢失更新会直接覆盖他人的范围规则 |
| 目录行（交易所 / 市场 / 交易对 / 币） | `ETag = W/"<内容哈希>"`（`row_etag_of`）；同样带响应头 + body `etag` 双通道 | 412 / 428 | 目录行没有稳定的 `id`+`updated_at` 代用版本，改用内容哈希；**客户端一律以服务端返回值为准，不要自行拼装 ETag** |
| 纯注释字段（`notes` / `note`） | 可降级为 last-write-wins | 覆盖不报错 | 只影响说明文字，冲突成本为零；其余字段一律走 `If-Match` |
| 同步触发 / 指派试算 | 单飞（`running` 门槛） | 409 / 503 | 一轮未完成不重入（09 §2.3） |
| 删除操作 | `If-Match` + 关联校验 | 412 / 409 | `delete_node` 已连带删派生计划；服务器删除需检查关联节点 |

**不采用 last-write-wins 的原因**：配置发布是不可变历史写入，无「覆盖」语义，只有「在他人版本之上再发一版」；
用 `base_version` 能把隐式覆盖变成显式冲突，操作者看到 diff 后再决定是否继续发布。

**锁与阻塞**：发布需 `&mut ControlPlane`，与 `poll` 循环共锁。单次请求持锁时间 = 一次 SQLite 事务 +
一次指派重算（低 QPS，12 §1.2）；HTTP 侧设锁等待上限，超时返回 503 `busy`，界面提示重试。

### 5.4 导入与导出

| 方向 | 对象 | 接口 | 函数 | 说明 |
|---|---|---|---|---|
| 导出 | 节点配置文件 | `GET /nodes/{name}/config.toml` | `SqliteStore::export_node_toml` | 12 §6 步骤 5 的「文件退化为导出」；控制面只生成内容，不远程部署（01 M3） |
| 导出 | 单域文档 | `GET /config/{domain}/export` | `ConfigStore::current` | 信封含 `domain` / `version` / `publisher` / `ts` / `content` |
| 导出 | 版本快照 | `GET /config/{domain}/versions/{v}` | `ConfigStore::get` | 历史不可变（09 §4.2） |
| 导出 | 交易对 / 币 / 审计 | `GET …/export`（CSV） | **新增** | 筛选条件随导出 |
| 导入 | 单域文档 | `POST /config/{domain}/publish`（body = 文档） | `ControlPlane::publish_domain` | 导入即发布：同样走校验、版本 +1、通知与审计 |
| 导入 | 节点定义 | `POST /nodes`（body = 定义 JSON） | `create_node` / `upsert_node` | 批量导入 = 循环调用，接口不设批量端点 |
| 导入（迁移用，不开放界面） | 文件树历史版本 | — | `SqliteStore::import_version` | 12 §6 步骤 2 的迁移路径，属 CLI/脚本职责，不进 UI |
| 导入 | 节点 TOML 反解 | — | — | **不设**：TOML 导出是单向产物，反解会引入与 `CollectorConfig` 的字段契约维护成本 |

导出文件的命名与校验：`<domain>-v<N>.json`、`<node>.toml`；JSON 导出附 `content_hash` 便于落盘后比对。

### 5.5 与事件中心的关系

**选定：UI 走 HTTP 直连控制面，不走总线。**

| 项 | 约定 |
|---|---|
| 落位 | 浏览器 → HTTP → **控制面进程内 HTTP 层** → `ControlPlane`（写入面）+ `SqliteStore`（读面） |
| 红线 4 的判定 | 红线要求**节点之间不直连、事件中心是唯一交换枢纽**；界面不是平台节点（§1.1 A4），它与控制面是「客户端 / 服务」关系，与 09 §1 图中的「操作者 / 自动化 ──写接口──► 控制面」同一条边，不构成节点间第二条传输 |
| 不走总线的理由 | ① 事件中心不提供请求/响应与 K/V 查找（03 §3），分页、筛选、错误码、鉴权在总线上无处安放；② 09 §4.6 明确的是**实例拉取配置**不得改走 HTTP——本接口服务操作者，不服务实例拉取，两者不冲突；③ 界面是浏览器侧，不能假定其具备总线客户端凭据与消费组语义 |
| 配置下发仍只走总线 | 发布产生的 `ConfigChanged`、`ConfigSnapshot`、`SubscribePlan` 一律经事件中心（09 §4.3）；HTTP 响应只回给操作者，不替代下发 |
| 状态观测 | 界面读控制面库与收敛比对结果，**不订阅 `ops.*`**；06 监控仍直接订阅同一主题（10 §5），两者互不代发 |
| `md.meta` | 界面不消费；其去向是存储层（09 §2.3 产物第 3 路），查询属 08 |
| 单一传输的完整性 | 平台内（节点 ↔ 节点）仍为事件中心单一传输；HTTP 仅存在于「人 ↔ 控制面」这一条边界 |

### 5.6 实时性与轮询

| 项 | 约定 |
|---|---|
| 一期 | 纯轮询；心跳周期 5s（10 §5），故运行时页轮询间隔取 **5s**，收敛进度视图取 **2s**（发布后 60s 内），配置列表取 **30s** |
| 缓存 | 服务端不加缓存层（SQLite 读成本低，12 §1.2）；客户端由 @tanstack/vue-query 管理 stale / refetch |
| 二期（可选） | `GET /api/v1/events/stream`（SSE）推送：发布完成、收敛状态翻转、同步轮次结束、实例判死；仍走同一 token 与同一进程 |
| 不采用 | WebSocket 双向通道——界面无上行推送需求，SSE 单向足够 |

---

## 6. 与现有代码的映射表

> 「已有」= `crates/control` 中存在可直接调用的公开函数；「已有·封装」= 函数存在但需 HTTP 包装 / 过滤 / 分页；
> 「新增」= 无对应实现。**本文件不改动任何代码，此表仅为实现范围清单。**

### 6.1 API → 函数

| API | 函数 | 状态 |
|---|---|---|
| `GET /healthz` | `ControlPlane::config` / `status` | 已有·封装 |
| `GET /overview` | `ConfigStore::current_versions`、`ControlPlane::convergence`、`ConfigStore::audit`、`ControlPlane::catalog`、`CatalogSync::{last_round_ms,is_running}` | 已有·封装 |
| `GET /meta` | `validate::consumed_domains` | 已有·封装 |
| `GET /servers` | `SqliteStore::list_servers` | 已有·封装（分页新增） |
| `POST/PUT /servers` | `SqliteStore::upsert_server` | 已有 |
| `GET /servers/{host}` | `SqliteStore::server_by_host` / `server_by_id` | 已有 |
| `PATCH /servers/{host}/status` | — | **新增**（`upsert_server` 更新分支不改 `status`） |
| `DELETE /servers/{host}` | — | **新增** |
| `GET /servers/{host}/nodes` | `list_nodes` + 过滤 | 已有·封装 |
| `GET /servers/{host}/instances` | `ControlPlane::instances` + `host` 过滤 | 已有·封装 |
| `GET /nodes` | `SqliteStore::list_nodes` + `ControlPlane::instances` | 已有·封装（联表新增） |
| `POST /nodes` | `SqliteStore::create_node`（内含 `ScopeRules::validate`） | 已有 |
| `GET/PUT/DELETE /nodes/{name}` | `node_by_name` / `upsert_node` / `delete_node` | 已有 |
| `GET /nodes/{name}/scope/preview` | `scope::resolve` + `ControlPlane::catalog` | 已有·封装 |
| `GET /nodes/{name}/config.toml` | `SqliteStore::export_node_toml` | 已有 |
| `GET /nodes/{name}/assignments*` | `SqliteStore::load_plans` / `ControlPlane::plan_of` | 已有·封装（按节点分页**新增**） |
| `GET /exchanges*` / `GET /exchanges/{venue}/markets*` | — | **新增**（DDL 就绪，无读写函数） |
| `GET /instruments*` | 过渡期 `ControlPlane::catalog`；目标 `instrument` 表 | **新增**（表未接线） |
| `GET /instruments/changes*` | — | **新增** |
| `GET /assets*` | 过渡期 `ControlPlane::catalog().currencies` | **新增**（表未接线） |
| `GET /config/domains` | `ConfigStore::current_versions` + `validate::consumed_domains` | 已有·封装 |
| `GET /config/{domain}` | `ConfigStore::current` | 已有·封装（ETag 组装新增） |
| `GET /config/{domain}/versions` | `ConfigStore::get`（逐版本） | 已有·封装（分页新增） |
| `GET /config/{domain}/audit` | `ConfigStore::audit` | 已有 |
| `POST /config/{domain}/validate` | `validate::validate` | 已有 |
| `POST /config/{domain}/publish` | `ControlPlane::publish_domain` + `publish_all` | 已有（`base_version` 基线校验**新增**） |
| `POST /config/{domain}/rollback` | `ControlPlane::rollback` + `publish_all` | 已有（基线校验**新增**） |
| `GET /config/{domain}/versions/{v}/diff` | `store::diff_summary` | 已有·封装 |
| `GET /config/{domain}/convergence` | `ControlPlane::convergence` | 已有·封装（域过滤新增） |
| `GET /runtime/instances*` | `ControlPlane::instances` / `OpsRegistry::get` / `SqliteStore::load_keys` | 已有·封装 |
| `POST /runtime/instances/{id}/repush` | `repush_plan`（私有） | **新增**（公开入口） |
| `GET /convergence` | `ControlPlane::convergence` | 已有 |
| `GET /assignments*` | `SqliteStore::load_plans`、`ControlPlane::plan_of` | 已有·封装（过滤/分页/聚合**新增**） |
| `POST /assignments/dry-run` | `assign::assign` | 已有·封装（构造 `Candidate` 集**新增**） |
| `GET /sync/status` | `CatalogSync::{interval_ms,last_round_ms,is_running,due}`、`Catalog::symbol_count` | **新增**（`ControlPlane` 未暴露 `&CatalogSync`，仅有 `catalog()` → `&Catalog`，需补访问器） |
| `POST /sync/runs` | `ControlPlane::run_metadata_sync` | **新增**（强制触发绕过 `due()` / `running` 门槛） |
| `GET /sync/runs*` | — | **新增**（`sync_run` 无读写函数） |
| `GET /audit*` | `ConfigStore::audit`（按域、仅 publish/rollback） | **新增**（全表分页与筛选） |

### 6.2 表 → 函数现状

| 表（12 §3） | 读写函数 | 状态 |
|---|---|---|
| `exchange` / `exchange_market` | — | **新增** |
| `asset` / `venue_asset` | — | **新增** |
| `instrument` | — | **新增** |
| `instrument_change` | — | **新增** |
| `sync_run` | — | **新增** |
| `server` | `upsert_server` / `list_servers` / `server_by_host` / `server_by_id` | 部分（改状态、删除、分页**新增**） |
| `collector_node` | `create_node` / `upsert_node` / `node_by_name` / `list_nodes` / `delete_node` / `export_node_toml` | 完整 |
| `node_assignment` | `save_plan` / `load_plans` | 部分（按节点/版本分页**新增**） |
| `node_runtime` | `save_key` / `load_keys` / `runtime_count` | 部分（分页筛选**新增**） |
| `config_domain` / `config_version` | `ConfigStore::{current_version,current,get,publish,rollback}`（SQLite 实现） | 完整（版本列表分页**新增**） |
| `audit_log` | `publish` 写入、`ConfigStore::audit` 读 | 部分（全局查询**新增**；节点类 action 未写入，见 §7 R6） |

### 6.3 CLI 与 HTTP 的能力对照

| CLI 子命令 | HTTP 对应 | 关系 |
|---|---|---|
| `serve` | `serve --http <addr>` | 同进程新增监听 |
| `publish <domain> <file>` | `POST /config/{domain}/publish` | 同一 `publish_domain` 语义；CLI 为独立进程，其 `ConfigChanged` 发往自身内存事件中心，不达运行实例（§2.4 S2），**自动化走 HTTP** |
| `rollback <domain> <version>` | `POST /config/{domain}/rollback` | 同上 |
| `audit <domain>` | `GET /config/{domain}/audit` | 同一 `ConfigStore::audit` |
| `current <domain>` | `GET /config/{domain}` | 同一 `ConfigStore::current` |
| （无） | 节点 / 服务器 / 目录 / 运行时 / 指派 / 同步 / 全局审计 | HTTP 为首个入口；CLI 不新增对应子命令，避免两套写入口语义分叉 |

---

## 7. 非目标与已知风险

### 7.1 非目标

见 §1.4（X1–X8）；补充：

| # | 非目标 | 依据 |
|---|---|---|
| X9 | 界面直连存储层或数据服务 | 红线 4；08 与 04 的职责 |
| X10 | 界面内嵌总线客户端 | §5.5 |
| X11 | 批量配置发布（一次发布多域） | 09 §4.1「域是唯一版本单位」，一次发布对应一个域版本 |
| X12 | 配置草稿区 / 审批流 / 多级发布 | 一期最小写入面；由 `reason`（工单号）承载流程信息 |

### 7.2 已知风险

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| R1 | `exchange` / `exchange_market` / `asset` / `venue_asset` / `instrument` / `instrument_change` / `sync_run` 七表**有 DDL 无读写函数**，catalog 目前落 `state/catalog.json` 文件 | P3 / P4 / P5 / P8 四组页面无数据源 | 分两步：过渡期读 `ControlPlane::catalog()`；目标按 12 §4.1 落库后切表 |
| R2 | `scope::resolve` 已实现但**未接入** `ControlPlane::recompute_inner`（指派直接取 `SubscribePlan` 域文档） | P2 的范围预览结果与 P10 实际下发计划可能不一致 | 界面在两者未接线前明确标注「预览 ≠ 已下发」；接线列入实现事项 |
| R3 | ~~`collector_node.enabled` 与 `server.status` 未参与指派候选过滤~~ **已按 O8 接线**：`recompute_inner` 先取已停用实例集合，从候选中剔除 | 启停开关对指派无实际效果，界面易产生错误预期 | **已解决**；注意停用只是「不再接收新计划」，实例上一次的计划仍在（不自动撤回，09 §7 同口径） |
| R4 | 控制面单实例 + 共享 `Mutex`：发布 / 同步触发会阻塞 `poll` 循环 | 长耗时操作期间心跳消费延迟，可能推高未收敛计数 | 锁等待上限 + 503；同步触发放在后台任务；指派试算限流 |
| R5 | HTTP 面首次引入鉴权；当前控制面无任何认证 | 一旦绑定非回环地址即为内网暴露面 | 鉴权与监听地址同时上线；默认绑定 `127.0.0.1` |
| R6 | `audit_log` 仅在 `publish` / `rollback` / `import_version` 时写入；节点与服务器变更**不写审计** | P9 无法回答「谁改了节点范围」 | 节点 / 服务器写接口落库时同步写 `audit_log`（`action = node_create` / `server_update` 等），与 12 §3.7 的 `action` 取值对齐 |
| R7 | ~~`instrument_change` 列名文档与 DDL 不一致~~ **已对齐**：`docs/12 §3.3` 改为 `before_v`/`after_v`（接口仍同时给 `before`/`after` 别名，前端两种都声明了） | 文档与 DDL 不一致，接口字段命名需择一 | **已解决** |
| R8 | 域文档以 JSON 原样透传，八域结构差异大 | 纯 JSON 编辑器对规则密集域可读性差 | §2.3：默认 JSON + 校验路径定位；密集域提供结构化列表视图 |
| R9 | 交易对量级（单所千级、全量数万） | offset 分页在深页变慢 | `idx_instrument_status` / `idx_instrument_synced` 支撑常规筛选；深页翻页改用 keyset 游标（后续） |
| R10 | 界面与桌面客户端（11）双端并存 | 术语与状态色漂移 | 共享 §3.4 的状态语义表；11 侧不引入配置页面（11 §1.2 N5/N8） |

---

## 8. 实现事项

> 分阶段，中性排布；阶段边界不含验收承诺，仅表示依赖顺序。

### 阶段一：HTTP 面与只读闭环

1. `ponk-control serve` 增加 HTTP 监听（axum + tokio），`ControlPlane` 提升为 `Arc<Mutex<_>>`，`poll` 循环迁入独立任务
2. 鉴权中间件（Bearer token + 角色 + `X-Actor`）、统一错误模型（`ControlError` → HTTP 映射）、分页与排序封装
3. 只读接口：概览、服务器、节点（定义 + 运行时联表）、配置域读取与版本历史、运行时与收敛、指派结果、全局审计
4. 触发类接口：域校验、范围预览、配置与 TOML 导出
5. 前端骨架：Vite + TypeScript + **Vue 3 + Element Plus**，导航树与状态色（§3.4），登录与 401 处理

### 阶段二：写入闭环

6. 节点与服务器 CRUD（`If-Match` / ETag、关联校验、`audit_log` 写入）
7. 配置发布与回滚（`base_version` 基线校验、校验错误路径定位、收敛进度视图与轮询）
8. 配置导入导出（单域 JSON、节点 TOML、CSV 导出）
9. 指派试算与手动重推（触发类，需公开 `repush_plan` 入口）

### 阶段三：目录与同步落库

10. `exchange` / `exchange_market` 读写函数与 P3 页面
11. `instrument` / `venue_asset` / `asset` 落库路径（12 §4.1）与 P4 / P5 页面、变更历史
12. `sync_run` 写入与查询、同步强制触发，P8 页面
13. catalog 由文件切换为库内事实（12 §6 步骤 4–5 影子比对后切换）

### 阶段四：接线与加固

14. `scope::resolve` 接入 `recompute_inner`（使节点范围规则真正进入指派）
15. ~~`collector_node.enabled` / `server.status` 与候选集的关系~~ **按 O8 落地：参与候选过滤**（停用节点与非 active 机器不进候选集）
16. SSE 推送（可选）、角色细分、只读连接池与读写分离
17. 界面产物打包与静态资源托管、部署说明（反向代理与 TLS）

---

## 附：需主线拍板的开放项

| # | 开放项 | 本文件的选定 / 备选 |
|---|---|---|
| O1 | 前端框架 | **已定：Vue 3 + TypeScript + Vite + Element Plus**；备选 React + Ant Design |
| O2 | 是否复用 gpui-kit 桌面栈 | 选定不复用（§2.2）；备选：仅复用主题令牌 |
| O3 | 鉴权方案 | **已定：Bearer token + read/write 角色 + 内网边界**；备选（不采纳）内网免密、Basic、二期 JWT |
| O4 | 后端框架 | 选定 axum；备选 tiny_http / actix-web |
| O5 | 首批接口范围 | 阶段一只读 + 校验/导出；阶段二写入与回滚 |
| O6 | 目录页面数据源时序 | 过渡期读 `catalog()`，目标切 `instrument` 等表（阶段三） |
| O7 | 实时性 | 一期轮询；二期 SSE |
| O8 | `enabled` / `server.status` 是否参与指派候选 | **已定：参与**——停用节点 / 非 active 机器不再进候选集；否则页面上的启停开关形同虚设 |
