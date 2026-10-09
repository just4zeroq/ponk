//! `ponk-control` 二进制：启动时**从本地文件存储加载八域配置**并提供
//! 09 §4 发布 / §4.6 拉取应答 / §6 控制信道消费 / §6.5 自身心跳服务。
//!
//! 不依赖数据库服务——存储后端是本地目录（09 §4.2「版本化文件树」）。
//!
//! ```text
//! ponk-control serve  [--store DIR] [--instance cp-1] [--publisher cp-1/api]
//!                     [--http 127.0.0.1:8080] --jwt-secret <s> [--jwt-ttl 8h]
//!                     [--admin-password <p>] [--bootstrap default|none]
//! ponk-control publish <domain> <file.json> [--store DIR] [--publisher P] [--reason R]
//! ponk-control rollback <domain> <version>  [--store DIR] [--publisher P]
//! ponk-control audit <domain> [--store DIR]
//! ponk-control current <domain> [--store DIR]
//! ponk-control user add --username <u> --role <read|write> [--password <p>]
//! ```
//!
//! **鉴权（`docs/13 §5.1`）**：开 `--http` 时**必须**给出 JWT 密钥
//! （配置文件 `[auth].jwt_secret` 或 `--jwt-secret`），否则拒绝启动；不开 `--http`
//! 则不需要密钥（没有 HTTP 鉴权面，也就没有首启引导）。
//!
//! `serve` 的事件中心后端为**进程内内存 backend**（03 §7，完整契约层语义）：
//! 控制面自身不持有入站连接、不直连任何节点（01 §6 红线 4 星型拓扑）。
//! 接真实总线（Redis Streams 等）属 `ponk-eventcenter` 的 backend 选择，
//! 与本二进制的存储后端无关。

use ponk_control::http;
use ponk_control::http::parse_domain;
use ponk_control::{
    AnyConfigStore, BootstrapMode, ConfigStore, ControlConfig, ControlPlane, NewUser, Role,
    SqliteStore,
};
use ponk_eventcenter::MemoryEventCenter;
use ponk_protocol::ConfigDomain;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

fn main() {
    if let Err(e) = run() {
        eprintln!("ponk-control: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        "serve" => serve(&args[1..]),
        "publish" => publish(&args[1..]),
        "rollback" => rollback(&args[1..]),
        "audit" => audit(&args[1..]),
        "current" => current(&args[1..]),
        "user" => user(&args[1..]),
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        other => Err(format!("未知子命令 `{other}`（见 `ponk-control help`）").into()),
    }
}

fn print_help() {
    println!(
        "\
ponk-control —— PONK 控制面（docs/09）

子命令：
  serve      启动控制面服务（加载八域配置 + 发布/拉取/控制信道/心跳）
  publish    发布一个配置域（域版本 +1 → 发 ConfigChanged）
  rollback   回滚：取历史版本内容**重新发布为新版本号**（09 §4.5）
  audit      打印该域的审计历史（JSONL 逐行）
  current    打印该域当前版本与内容
  user       用户管理（`user add` 建用户；改密走 PATCH /auth/users/<u>）

serve 专用参数（docs/13 §2.4 / §5.1）：
  --http <addr>       开启进程内 HTTP 服务（默认关闭；如 127.0.0.1:8080）
  -c, --config <path> 配置文件（默认 ./control.toml）——**密钥/端口/存储的首选来源**，
                      优先级：CLI 参数 > 配置文件 > 内置默认；不再读环境变量
  --jwt-secret <s>    JWT（HS256）签名密钥（覆盖配置文件 [auth].jwt_secret）
                      **开 --http 则必须给**，缺失即拒绝启动（docs/13 §5.1：
                      不自动生成随机密钥，否则重启后所有令牌失效）
  --jwt-ttl <dur>     令牌有效期，默认 8h；可写 8h / 30m / 900 / 45s
  --admin-password <p> 首启引导的管理员口令（覆盖 [auth].admin_password）
                      （未设置则用内置默认 admin123，登录响应与启动日志
                      会标 default_password: true）
  --bootstrap <mode>  default（缺省）| none —— none 则空库不建内置管理员，
                      此时登录返回 403 no_users

user add 专用参数：
  --username <u>      用户名（必填，唯一）
  --role <read|write> 角色（必填）
  --password <p>      口令；未给则读环境变量 PONK_USER_PASSWORD（非交互式）

通用参数：
  --store <dir>       配置存储根目录（默认 ./control-store）
  --instance <id>     控制面实例 id（默认 cp-1）
  --publisher <p>     发布者标识（默认 <instance>/api）
  --reason <r>        发布原因 / 工单号（publish / rollback）
  --domain <d>        配置域（publish/audit/current 的第一个位置参数亦可）
"
    );
}

// ── 参数解析（不引第三方 CLI 依赖）──────────────────────────────────────────

#[derive(Default)]
struct Opts {
    /// 配置文件路径（缺省 `./control.toml`；`docs/13 §5.1`：密钥/端口/存储的首选来源）。
    config: Option<PathBuf>,
    store: Option<PathBuf>,
    instance: Option<String>,
    publisher: Option<String>,
    reason: Option<String>,
    /// `serve --http <addr>`（docs/13 §2.4：默认关闭，显式开启）。
    http: Option<String>,
    /// `serve --jwt-secret <s>`（docs/13 §5.1；缺省取配置文件 `[auth].jwt_secret`）。
    jwt_secret: Option<String>,
    /// `serve --jwt-ttl <dur>`（默认 8h）。
    jwt_ttl: Option<String>,
    /// `serve --admin-password <p>`（缺省取配置文件 `[auth].admin_password`）。
    admin_password: Option<String>,
    /// `serve --bootstrap default|none`。
    bootstrap: Option<String>,
    /// `user add --username <u>`。
    username: Option<String>,
    /// `user add --role <read|write>`。
    role: Option<String>,
    /// `user add --password <p>`（缺省读 PONK_USER_PASSWORD）。
    password: Option<String>,
    /// `[providers]` 段（`docs/14 §8`）——**不是 CLI 参数**，只从配置文件来
    /// （与密钥同一处理：凭据与调优参数不进八域、不进环境变量）。
    providers: Option<ProvidersSection>,
    positional: Vec<String>,
}

/// 参数解析（含配置文件合并）：**CLI > 配置文件 > 内置默认**，失败 = 用法错误。
///
/// 这里是唯一入口——6 个子命令都走它，保证 `--config` 对全部子命令生效。
fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = parse_opts_raw(args)?;
    if let Some(fc) = load_file_config(&o)? {
        merge_file_config(&mut o, &fc);
    }
    Ok(o)
}

/// 纯参数解析（不含配置文件），由 [`parse_opts`] 包装。
fn parse_opts_raw(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} 缺少取值"))
        };
        match a.as_str() {
            "--config" | "-c" => o.config = Some(PathBuf::from(val("--config")?)),
            "--store" => o.store = Some(PathBuf::from(val("--store")?)),
            "--instance" => o.instance = Some(val("--instance")?),
            "--publisher" => o.publisher = Some(val("--publisher")?),
            "--reason" => o.reason = Some(val("--reason")?),
            "--http" => o.http = Some(val("--http")?),
            "--jwt-secret" => o.jwt_secret = Some(val("--jwt-secret")?),
            "--jwt-ttl" => o.jwt_ttl = Some(val("--jwt-ttl")?),
            "--admin-password" => o.admin_password = Some(val("--admin-password")?),
            "--bootstrap" => o.bootstrap = Some(val("--bootstrap")?),
            "--username" => o.username = Some(val("--username")?),
            "--role" => o.role = Some(val("--role")?),
            "--password" => o.password = Some(val("--password")?),
            // 静态 token 方案已按 docs/13 §5.1 弃用——给可执行的迁移提示
            "--token" | "--tokens" => {
                return Err(
                    "静态 token 方案已移除（docs/13 §5.1 弃用原 `--token`）：
  开 HTTP 请在配置文件 [auth].jwt_secret 写入（或临时用 --jwt-secret <s>），
  用户与角色用 `ponk-control user add --username <u> --role <read|write>` 创建。"
                        .to_string(),
                )
            }
            other if other.starts_with("--") => {
                return Err(format!("未知参数 `{other}`"))
            }
            other => o.positional.push(other.to_string()),
        }
    }
    Ok(o)
}

// ── 配置文件（docs/13 §5.1：密钥 / 端口 / 存储的首选来源）───────────────────
//
// 优先级：**CLI 参数 > 配置文件 > 内置默认**；**不再读取环境变量**（那类值改由文件保管，
// 避免密钥散落在进程环境里被误打印）。文件缺省 `./control.toml`，不存在则只用 CLI/默认。

/// `control.toml` 结构（全部字段可省略）。
#[derive(Debug, Default, serde::Deserialize)]
struct FileConfig {
    instance: Option<String>,
    publisher: Option<String>,
    #[serde(default)]
    store: StoreSection,
    #[serde(default)]
    http: HttpSection,
    #[serde(default)]
    auth: AuthSection,
    /// 外部数据源（`docs/14 §8` 新增段）。
    #[serde(default)]
    providers: ProvidersSection,
}

#[derive(Debug, Default, serde::Deserialize)]
struct StoreSection {
    /// SQLite 目录（控制面配置与状态落这里，12 §1.2）。
    path: Option<PathBuf>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct HttpSection {
    /// 监听地址（如 `127.0.0.1:8080`）；**有值即开启 HTTP**（等价于旧的 `--http`）。
    addr: Option<String>,
    /// 显式关闭：`enabled = false` 时即使写了 `addr` 也不开。
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AuthSection {
    /// JWT（HS256）密钥：开 HTTP 必须有；缺失拒绝启动（不自动生成）。
    jwt_secret: Option<String>,
    /// 令牌有效期，`8h` / `30m` / `900` / `45s`。
    jwt_ttl: Option<String>,
    /// 首启引导：`default`（缺省）| `none`。
    bootstrap: Option<String>,
    /// 首启管理员口令（只在空库用一次）；缺省内置 `admin123`。
    admin_password: Option<String>,
}

/// `control.toml [providers]`（`docs/14 §8`）：外部数据源与调优参数。
///
/// **不进八域**：与密钥同一处理——它们是连接凭据与调优参数，不是平台配置。
#[derive(Debug, Default, Clone, serde::Deserialize)]
struct ProvidersSection {
    /// `coingecko`（主）| `coinmarketcap`（备选骨架）。
    coin: Option<String>,
    /// 可选 API key（CG Demo 档无需；CMC 必填）。
    coin_api_key: Option<String>,
    /// 外部同步节拍，如 `"1h"`（与 `jwt_ttl` 同口径：`1h` / `30m` / `45s` / 裸秒数）。
    offchain_interval: Option<String>,
    /// `/coins/markets` 分页大小（CoinGecko 上限 250）。
    coin_market_page_size: Option<u32>,
    /// 每轮轮转拉取的详情币数。
    coin_detail_batch: Option<usize>,
    /// 详情拉取重试次数。
    coin_detail_max_retries: Option<u32>,
    /// 单次请求超时（ms）。
    coin_timeout_ms: Option<u64>,
    /// ── 交易所取数（统一经 dig3 连接器层，`docs/09 §2.4`）────────────────
    /// 单次请求超时（ms）——覆盖 dig3 内置的 30s。
    exchange_timeout_ms: Option<u64>,
    /// 连接失败冷却（ms）：期间跳过该 base，避免 5 分钟节拍被超时吃满。
    exchange_cooldown_ms: Option<u64>,
    /// base 候选表：`"<venue>.<market>"` → **origin 列表**（按序 failover，
    /// 首个成功的胜出）。值**只到 origin，不含 path/query** —— 路径由 dig3 的
    /// 端点定义自己拼。**按键覆盖**内置默认，未出现的键沿用默认。
    #[serde(default)]
    dig3_rest_base: std::collections::BTreeMap<String, Vec<String>>,
}

impl ProvidersSection {
    /// 折算成 [`ponk_control::OffchainConfig`]（缺省即 `docs/14 §8` 的样例值）。
    fn to_offchain_config(&self) -> Result<ponk_control::OffchainConfig, String> {
        let mut c = ponk_control::OffchainConfig::default();
        if let Some(v) = &self.coin {
            c.coin = v.trim().to_string();
        }
        c.coin_api_key = self
            .coin_api_key
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(v) = &self.offchain_interval {
            c.offchain_interval_ms = ponk_control::parse_duration_ms(v)
                .map_err(|e| format!("[providers].offchain_interval {e}"))?;
        }
        if let Some(v) = self.coin_market_page_size {
            c.coin_market_page_size = v.clamp(1, 250);
        }
        if let Some(v) = self.coin_detail_batch {
            c.coin_detail_batch = v;
        }
        if let Some(v) = self.coin_detail_max_retries {
            c.coin_detail_max_retries = v;
        }
        if let Some(v) = self.coin_timeout_ms {
            c.coin_timeout_ms = v.max(1_000);
        }
        Ok(c)
    }

    /// 折算成 [`ponk_control::ExchangeFetcherConfig`]（dig3 连接器的 base 与超时）。
    fn to_exchange_config(&self) -> ponk_control::ExchangeFetcherConfig {
        let mut c = if self.dig3_rest_base.is_empty() {
            ponk_control::ExchangeFetcherConfig::default()
        } else {
            ponk_control::ExchangeFetcherConfig::merged(self.dig3_rest_base.clone())
        };
        if let Some(v) = self.exchange_timeout_ms {
            c.timeout_ms = v.max(1_000);
        }
        if let Some(v) = self.exchange_cooldown_ms {
            c.cooldown_ms = v.max(1_000);
        }
        c
    }
}

/// 读取配置文件。查找顺序：
///
/// 1. `--config <path>`（显式指定，不存在 = 报错）
/// 2. `./control.toml`（在哪个目录启动就用哪个）
/// 3. `crates/control/control.toml`（**本仓库的约定位置**，让从仓库根启动也能直接找到）
///
/// 都找不到 = **不算错**（纯 CLI 方式仍可用）；存在但语法错误 = 直接失败，
/// 免得"以为配了其实没生效"。
fn load_file_config(o: &Opts) -> Result<Option<FileConfig>, String> {
    let path = match &o.config {
        Some(p) => {
            if !p.exists() {
                return Err(format!("指定的配置文件不存在：{}", p.display()));
            }
            p.clone()
        }
        None => {
            let cwd = PathBuf::from("./control.toml");
            if cwd.exists() {
                cwd
            } else {
                PathBuf::from("crates/control/control.toml")
            }
        }
    };
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取配置文件 {} 失败: {e}", path.display()))?;
    let fc: FileConfig = toml::from_str(&text)
        .map_err(|e| format!("解析配置文件 {} 失败: {e}", path.display()))?;
    Ok(Some(fc))
}

/// 把配置文件的取值**回填**到 `Opts`（仅填 CLI 未给的，故 CLI 优先）。
fn merge_file_config(o: &mut Opts, fc: &FileConfig) {
    if o.store.is_none() {
        if let Some(p) = &fc.store.path {
            // 相对路径以**配置文件所在目录**为基准，否则从仓库根启动时会与配置文件错位
            o.store = Some(if p.is_relative() {
                o.config
                    .clone()
                    .and_then(|c| c.parent().map(|d| d.join(p)))
                    .unwrap_or_else(|| p.clone())
            } else {
                p.clone()
            });
        }
    }
    if o.instance.is_none() {
        if let Some(v) = &fc.instance {
            o.instance = Some(v.clone());
        }
    }
    if o.publisher.is_none() {
        if let Some(v) = &fc.publisher {
            o.publisher = Some(v.clone());
        }
    }
    if o.http.is_none()
        && fc.http.enabled != Some(false)
        && fc.http.addr.as_deref().map(|a| !a.trim().is_empty()) == Some(true)
    {
        o.http = fc.http.addr.clone();
    }
    if o.jwt_secret.is_none() {
        o.jwt_secret = fc.auth.jwt_secret.clone();
    }
    if o.jwt_ttl.is_none() {
        o.jwt_ttl = fc.auth.jwt_ttl.clone();
    }
    if o.admin_password.is_none() {
        o.admin_password = fc.auth.admin_password.clone();
    }
    if o.bootstrap.is_none() {
        o.bootstrap = fc.auth.bootstrap.clone();
    }
    // `[providers]` 没有 CLI 对应项（凭据与调优参数只从文件来，docs/14 §8）
    o.providers = Some(fc.providers.clone());
}

fn config_from(o: &Opts) -> ControlConfig {
    let instance = o
        .instance
        .clone()
        .unwrap_or_else(|| "cp-1".to_string());
    ControlConfig {
        publisher: o
            .publisher
            .clone()
            .unwrap_or_else(|| format!("{instance}/api")),
        instance_id: instance,
        store_root: o
            .store
            .clone()
            .unwrap_or_else(|| PathBuf::from("./control-store")),
        ..ControlConfig::default()
    }
}

// ── serve ───────────────────────────────────────────────────────────────────

/// 控制面实例（serve 的事件中心后端 = 进程内内存 backend，03 §7）。
type Plane = ControlPlane<MemoryEventCenter>;
/// 与 HTTP 共享的同一把锁（docs/13 §2.4：`poll` 循环与 HTTP 共用一把锁）。
type Shared = Arc<Mutex<Plane>>;

/// 控制面主循环（09 §4 / §6.5）。
///
/// 每轮**持锁执行**一次 `poll` + 心跳 + 判死 + 元数据同步，随后**放锁**并短暂
/// 休眠——放锁窗口就是 HTTP 请求拿锁的机会（docs/13 §2.4）。
/// 休眠是本轮新增的：原循环在无会话时 `poll` 立即返回、忙等空转，
/// 会把锁吃死，HTTP 侧只能一直 503。
fn control_loop(shared: &Shared) -> Result<(), Box<dyn std::error::Error>> {
    let heartbeat_every = {
        let g = shared.blocking_lock();
        Duration::from_millis(g.config().liveness.heartbeat_interval_ms)
    };
    let mut last_hb = Instant::now();
    let mut last_sweep = Instant::now();
    let mut last_meta: Option<Instant> = None;
    let mut last_offchain: Option<Instant> = None;
    let mut status_started = false;

    loop {
        let n = {
            let mut g = shared.blocking_lock();

            let n = g.poll(Duration::from_millis(200))?;
            if n > 0 {
                println!("[control] 处理入站消息 {n} 条");
            }

            if last_hb.elapsed() >= heartbeat_every {
                if !status_started {
                    g.set_status(ponk_protocol::Status::Running);
                    status_started = true;
                }
                let now = ponk_control::now_ms();
                let _ = g.publish_heartbeat(now)?;
                last_hb = Instant::now();
            }

            if last_sweep.elapsed() >= Duration::from_millis(1_000) {
                let now = ponk_control::now_ms();
                let dead = g.sweep(now)?;
                if !dead.is_empty() {
                    // 09 §6.4：只标记 + 计数，**不触发任何自动指派变更**
                    println!("[control] 判死（3×5s 无心跳）：{}", dead.join(", "));
                }
                let conv = g.convergence(now);
                if !conv.not_converged.is_empty() {
                    let ids: Vec<&str> = conv
                        .not_converged
                        .iter()
                        .map(|nc| nc.instance_id.as_str())
                        .collect();
                    println!(
                        "[control] 未收敛 {} 个：{}",
                        conv.not_converged.len(),
                        ids.join(", ")
                    );
                }
                last_sweep = Instant::now();
            }

            // 09 §2.3 定时任务：每 5 分钟同步一次交易对与币信息（内部按 due 自节流）
            if last_meta.map_or(true, |t| t.elapsed() >= Duration::from_secs(30)) {
                let now = ponk_control::now_ms();
                let outcome = g.run_metadata_sync(now)?;
                if outcome.changed {
                    println!(
                        "[control] catalog 变更：{}；md.meta {} 条；重算={}",
                        outcome.diff.summary(),
                        outcome.md_meta.len(),
                        outcome.recompute_ran
                    );
                }
                for sk in outcome.skipped.iter().take(3) {
                    println!(
                        "[control] 元数据跳过 {}/{}：{}",
                        sk.venue,
                        sk.market.as_deref().unwrap_or("*"),
                        sk.reason
                    );
                }
                last_meta = Some(Instant::now());
            }

            // docs/14 §3：外部（币）同步——**独立节拍**（默认 1 小时，内部按 due 自节流）。
            // 与交易所同步分离：CG 限速/失败只记 `offchain_sync_run`，**不碰 `sync_run`**。
            if last_offchain.map_or(true, |t| t.elapsed() >= Duration::from_secs(60)) {
                let now = ponk_control::now_ms();
                let out = g.run_offchain_sync(now);
                if out.ran
                    && (out.rate_limited || out.error.is_some() || out.detail_pulled > 0 || out.contracts > 0)
                {
                    let tag = if out.rate_limited {
                        "被限速"
                    } else if out.error.is_some() {
                        "有错误"
                    } else {
                        "完成"
                    };
                    println!("[control] 外部币同步（{tag}）：{}", out.summary());
                    if let Some(e) = &out.error {
                        println!("[control]   原因：{e}");
                    }
                }
                last_offchain = Some(Instant::now());
            }
            n
        };
        if n == 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let o = parse_opts(args)?;
    let cfg = config_from(&o);
    let root = cfg.store_root.clone();
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg)?;

    // 启动：加载八域配置（09 交付物 7）
    let domains = plane.load_domains()?;
    println!(
        "[control] store={} 实例={} 已加载配置域 {}/8",
        root.display(),
        plane.config().instance_id,
        domains.len()
    );
    for d in ConfigDomain::ALL {
        match domains.get(&d) {
            Some(doc) => println!(
                "  - {d} v{} ({} bytes, publisher={}, {})",
                doc.version,
                doc.content.to_string().len(),
                doc.publisher,
                doc.reason
            ),
            None => println!("  - {d} <未发布>"),
        }
    }
    let restored = plane.attach_known();
    println!("[control] 恢复快照键并重建实例会话：{restored}");

    // 外部（币）同步：`[providers]` 段（docs/14 §8）——独立节拍，失败不污染 sync_run
    let offchain_cfg = o
        .providers
        .clone()
        .unwrap_or_default()
        .to_offchain_config()?;
    plane.set_offchain_config(offchain_cfg.clone());
    match ponk_control::build_provider(&offchain_cfg) {
        Ok(p) => {
            println!(
                "[control] 外部币源 = {}，节拍 = {} ms，分页 = {}，详情轮转 = {} 个/轮",
                offchain_cfg.coin,
                offchain_cfg.offchain_interval_ms,
                offchain_cfg.coin_market_page_size,
                offchain_cfg.coin_detail_batch
            );
            plane.set_offchain_provider(p);
        }
        Err(e) => eprintln!(
            "[control] 外部币源构造失败（外部同步将以 unsupported 记进 offchain_sync_run）：{e}"
        ),
    }

    // 交易所取数拉取器（09 §2.4）：**必须接线**，否则 catalog 永远为空、页面点
    // 「立即同步」永远 changed=false（症状 =「同步有返回但没有数据」）。
    //
    // 09 §2.4 的口径是统一经 dig3 连接器层 —— transport 归 dig3，本仓只做平台语义
    // 归一化（见 `fetcher.rs::normalize`）。构造不联网、不建 Runtime，故不会失败。
    let ex_cfg = o
        .providers
        .clone()
        .unwrap_or_default()
        .to_exchange_config();
    {
        let mut keys: Vec<&String> = ex_cfg.rest_bases.keys().collect();
        keys.sort();
        println!(
            "[sync] 交易所取数 = Dig3Fetcher（经 dig3 连接器；{} 个 base 键，超时 {}ms，失败冷却 {}ms）",
            ex_cfg.endpoint_count(),
            ex_cfg.timeout_ms,
            ex_cfg.cooldown_ms
        );
        for k in keys {
            let list = &ex_cfg.rest_bases[k];
            println!("  - {k} → {} 个候选 base（按序 failover，各建独立 hub）", list.len());
            for b in list {
                println!("      · {b}");
            }
        }
        plane.set_fetcher(std::sync::Arc::new(ponk_control::Dig3Fetcher::new(ex_cfg)));
    }

    // 鉴权（docs/13 §5.1）：**开 HTTP 必须给出 JWT 密钥**，缺失即拒绝启动
    // （不自动生成随机密钥）；未开 HTTP 则没有鉴权面，也不做首启引导。
    let auth = if o.http.is_some() {
        // 密钥与首启口令：**配置文件 > CLI**；不再读环境变量（docs/13 §5.1）
        let secret = o.jwt_secret.clone();
        let admin_password = o.admin_password.clone();
        let ttl = match o.jwt_ttl.as_deref() {
            Some(t) => Some(parse_ttl(t)?),
            None => None,
        };
        let bootstrap = match o.bootstrap.as_deref() {
            Some(m) => BootstrapMode::parse(m)?,
            None => BootstrapMode::default(),
        };
        http::AuthConfig::new(secret, ttl, admin_password, bootstrap)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
    } else {
        http::AuthConfig::offline()
    };

    let (state, shared) = http::AppState::build(
        plane,
        auth,
        Duration::from_millis(http::DEFAULT_LOCK_WAIT_MS),
    )
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    // docs/13 §5.1 首启引导：空库 → 自动创建内置管理员（仅一次），日志强制标注
    if let Some(info) = state.bootstrap() {
        println!(
            "[control] 首启引导：已创建内置管理员 {}（role=write，default_password={}）",
            info.username, info.default_password
        );
        if info.default_password {
            println!(
                "[control] 警告：该账号仍使用内置默认口令——生产请在 control.toml 的 [auth].admin_password 覆盖，或登录后 PATCH /auth/users/{} 改密",
                info.username
            );
        }
    }

    let Some(addr) = o.http.clone() else {
        // 未开 HTTP：与改造前一致，控制循环独占主线程
        return control_loop(&shared);
    };

    let loop_shared = shared.clone();
    // 控制循环挪到独立 OS 线程（阻塞语义不变），HTTP 跑在 tokio runtime 上
    std::thread::spawn(move || {
        if let Err(e) = control_loop(&loop_shared) {
            eprintln!("ponk-control: 控制循环退出：{e}");
            std::process::exit(1);
        }
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(http::serve(&addr, state))
}

// ── publish / rollback / audit / current ────────────────────────────────────

fn publish(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let o = parse_opts(args)?;
    let domain = o
        .positional
        .first()
        .ok_or("publish 需要 <domain> <file.json>")?;
    let file = o
        .positional
        .get(1)
        .ok_or("publish 需要 <domain> <file.json>")?;
    let cfg = config_from(&o);
    let content: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file)?)?;

    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg.clone())?;
    let (doc, out) = plane.publish_domain(
        parse_domain(domain)?,
        content,
        &cfg.publisher,
        o.reason.as_deref().unwrap_or("publish"),
    )?;
    plane.publish_all(&out)?;
    println!(
        "{} v{} 已发布（publisher={}, diff={:?}），总线出站 {} 条",
        doc.domain,
        doc.version,
        doc.publisher,
        plane
            .store()
            .audit(doc.domain)?
            .last()
            .map(|a| a.diff.clone())
            .unwrap_or_default(),
        out.len()
    );
    Ok(())
}

fn rollback(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let o = parse_opts(args)?;
    let domain = o.positional.first().ok_or("rollback 需要 <domain> <version>")?;
    let ver: u64 = o
        .positional
        .get(1)
        .ok_or("rollback 需要 <domain> <version>")?
        .parse()?;
    let cfg = config_from(&o);
    let mut plane = ControlPlane::open(MemoryEventCenter::new(), cfg.clone())?;
    let (doc, out) = plane.rollback(parse_domain(domain)?, ver, &cfg.publisher)?;
    plane.publish_all(&out)?;
    println!(
        "回滚 {} → 以 v{ver} 的内容发布为 **新版本 v{}**（current 未倒回旧号，09 §4.5）",
        doc.domain, doc.version
    );
    Ok(())
}

fn audit(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let o = parse_opts(args)?;
    let domain = o.positional.first().ok_or("audit 需要 <domain>")?;
    let cfg = config_from(&o);
    let store = AnyConfigStore::open(&cfg.store_root)?;
    for e in store.audit(parse_domain(domain)?)? {
        println!("{}", serde_json::to_string(&e)?);
    }
    Ok(())
}

fn current(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let o = parse_opts(args)?;
    let domain = o.positional.first().ok_or("current 需要 <domain>")?;
    let cfg = config_from(&o);
    let store = AnyConfigStore::open(&cfg.store_root)?;
    let d = parse_domain(domain)?;
    match store.current(d)? {
        Some(doc) => println!("{}", serde_json::to_string_pretty(&doc)?),
        None => println!("{d}: <未发布>"),
    }
    Ok(())
}

// ── user（docs/13 §5.1「初始用户用 CLI 创建」）───────────────────────────────

/// `ponk-control user <add>`：非交互式建用户（适合脚本 / 首次部署）。
fn user(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let o = parse_opts(args)?;
    match o.positional.first().map(|s| s.as_str()) {
        Some("add") => user_add(&o),
        Some("set-password") => user_set_password(&o),
        other => Err(format!(
            "未知 user 子命令 {other:?}（支持：`user add --username <u> --role <read|write> [--password <p>]`、             `user set-password --username <u> [--password <p>]`；口令缺省读环境变量 PONK_USER_PASSWORD）"
        )
        .into()),
    }
}

/// `user add`：创建用户，口令只进 argon2id 哈希——**不回显、不进日志**。
fn user_add(o: &Opts) -> Result<(), Box<dyn std::error::Error>> {
    let username = o.username.clone().unwrap_or_default();
    if username.trim().is_empty() {
        return Err("user add 需要 --username <u>".into());
    }
    let role_raw = o.role.as_deref().unwrap_or_default();
    let role = Role::parse(role_raw)
        .ok_or_else(|| format!("--role 必须是 read / write，得到 `{role_raw}`"))?;

    // 非交互式：--password > 环境变量 PONK_USER_PASSWORD > 报错（不读 stdin）
    let password = o
        .password
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| std::env::var("PONK_USER_PASSWORD").ok().filter(|p| !p.is_empty()))
        .ok_or(
            "缺少口令：传 --password <p> 或设置环境变量 PONK_USER_PASSWORD（非交互式，适合脚本）",
        )?;

    let cfg = config_from(o);
    let store = SqliteStore::open(&cfg.store_root)?;
    let rec = store.create_user(&NewUser {
        username: username.trim().to_string(),
        password,
        role,
    })?;
    println!("已创建用户 {}（role={}）", rec.username, rec.role);
    Ok(())
}

/// `user set-password`：改密，口令只进 argon2id 哈希——**不回显、不进日志**。
///
/// 存在理由：`PATCH /auth/users/{u}` 需要 write 令牌，而**忘记口令时手上恰好没有有效令牌**
/// （`docs/13` §5 规格缺口 #2）——CLI 是唯一不依赖令牌的恢复路径。
fn user_set_password(o: &Opts) -> Result<(), Box<dyn std::error::Error>> {
    let username = o.username.clone().unwrap_or_default();
    if username.trim().is_empty() {
        return Err("user set-password 需要 --username <u>".into());
    }
    // 非交互式：--password > 环境变量 PONK_USER_PASSWORD（不读 stdin）
    let password = o
        .password
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| std::env::var("PONK_USER_PASSWORD").ok().filter(|p| !p.is_empty()))
        .ok_or(
            "缺少新口令：传 --password <p> 或设置环境变量 PONK_USER_PASSWORD（非交互式）",
        )?;

    let cfg = config_from(o);
    let store = SqliteStore::open(&cfg.store_root)?;
    if store.count_users()? == 0 {
        return Err("用户表为空：先用 `user add` 创建首个用户".into());
    }
    let rec = store.update_user(
        username.trim(),
        &ponk_control::UserPatch {
            password: Some(password),
            role: None,
            disabled: None,
        },
    )?;
    // 只回显身份与角色；口令不回显、不进日志
    println!("已更新 {} 的口令（role={}）", rec.username, rec.role);
    Ok(())
}

// ── --jwt-ttl 解析（8h / 30m / 900 / 45s，缺省单位 = 秒）────────────────────

fn parse_ttl(raw: &str) -> Result<u64, String> {
    let t = raw.trim();
    let (num, mult) = match t.strip_suffix('h') {
        Some(v) => (v, 3_600u64),
        None => match t.strip_suffix('m') {
            Some(v) => (v, 60),
            None => match t.strip_suffix('s') {
                Some(v) => (v, 1),
                None => (t, 1),
            },
        },
    };
    let n: u64 = num.trim().parse().map_err(|_| {
        format!("--jwt-ttl 取值不合法：`{raw}`（可用 8h / 30m / 900 / 45s）")
    })?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("--jwt-ttl 过大：`{raw}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ttl_accepts_suffixes_and_rejects_garbage() {
        assert_eq!(parse_ttl("8h").expect("8h"), 8 * 3600);
        assert_eq!(parse_ttl("30m").expect("30m"), 1800);
        assert_eq!(parse_ttl("45s").expect("45s"), 45);
        assert_eq!(parse_ttl("900").expect("900"), 900);
        assert_eq!(parse_ttl(" 2h ").expect("带空白"), 7200);
        assert_eq!(parse_ttl("0").expect("0"), 0, "0 允许：测试用极短 TTL");
        assert!(parse_ttl("").is_err());
        assert!(parse_ttl("abc").is_err());
        assert!(parse_ttl("8h30m").is_err());
        assert!(parse_ttl("-1").is_err());
    }
}

#[cfg(test)]
mod file_config_tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn write_config(dir: &std::path::Path, text: &str) -> PathBuf {
        let p = dir.join("control.toml");
        std::fs::write(&p, text).expect("write config");
        p
    }

    #[test]
    fn file_provides_secret_port_and_store_and_cli_still_wins() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = write_config(
            dir.path(),
            r#"
instance = "cp-9"
[store]
path = "./my-store"
[http]
addr = "127.0.0.1:18080"
[auth]
jwt_secret = "file-secret-value"
jwt_ttl = "30m"
"#,
        );

        // ① 只给 --config：三项全部来自文件
        let o = parse_opts(&args(&["--config", path.to_str().unwrap()])).expect("parse");
        // 相对存储路径按**配置文件所在目录**解析（否则从仓库根启动会与配置错位）
        assert_eq!(
            o.store,
            Some(dir.path().join("my-store")),
            "存储来自文件，且相对路径以配置文件目录为基准"
        );
        assert_eq!(o.http.as_deref(), Some("127.0.0.1:18080"), "端口来自文件");
        assert_eq!(o.jwt_secret.as_deref(), Some("file-secret-value"), "密钥来自文件");
        assert_eq!(o.jwt_ttl.as_deref(), Some("30m"));
        assert_eq!(o.instance.as_deref(), Some("cp-9"));

        // ② CLI 显式给出时**覆盖**文件（优先级：CLI > 文件）
        let o2 = parse_opts(&args(&[
            "--config",
            path.to_str().unwrap(),
            "--store",
            "./cli-store",
            "--http",
            "127.0.0.1:19999",
        ]))
        .expect("parse");
        // CLI 给的相对路径**保持原样**（它以进程工作目录为基准，由使用者负责）
        assert_eq!(o2.store, Some(PathBuf::from("./cli-store")), "CLI 覆盖文件");
        assert_eq!(o2.http.as_deref(), Some("127.0.0.1:19999"));
        assert_eq!(o2.jwt_secret.as_deref(), Some("file-secret-value"), "未覆盖的仍取文件");
    }

    #[test]
    fn http_can_be_disabled_even_when_addr_present() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = write_config(dir.path(), "[http]
addr = \"127.0.0.1:18080\"
enabled = false
");
        let o = parse_opts(&args(&["--config", path.to_str().unwrap()])).expect("parse");
        assert_eq!(o.http, None, "enabled=false 时即使有 addr 也不开 HTTP");

        let path2 = write_config(dir.path(), "[http]
addr = \"127.0.0.1:18080\"
");
        let o2 = parse_opts(&args(&["--config", path2.to_str().unwrap()])).expect("parse");
        assert_eq!(o2.http.as_deref(), Some("127.0.0.1:18080"));
    }

    #[test]
    fn explicit_missing_config_is_an_error_but_omitted_is_ok() {
        let dir = tempfile::tempdir().expect("tmpdir");
        // 显式 `--config` 指向不存在的文件 = 配置写错，必须**报错**而非静默回退
        let missing = dir.path().join("nope.toml");
        let res = parse_opts(&args(&["--config", missing.to_str().unwrap()]));
        assert!(res.is_err(), "显式指定的文件不存在要报错（防止误以为配置已生效）");
        let err = res.err().unwrap_or_default();
        assert!(err.contains("nope.toml") || err.contains("不存在"), "{err}");

        // 不给 --config 时按查找顺序回退（./control.toml → crates/control/control.toml），
        // 两者都没有 = 不算错，走 CLI/默认
        let o = parse_opts(&args(&[])).expect("未指定配置文件不算错");
        assert!(o.config.is_none() || o.config.is_some());

        let bad = write_config(dir.path(), "[auth
jwt_secret = \"x\"
");
        let res = parse_opts(&args(&["--config", bad.to_str().unwrap()]));
        // 不用 expect_err：那要求 Opts: Debug（Opts 只有 Option 字段，没派生 Debug）
        assert!(res.is_err(), "语法错误必须报错（免得以为配了其实没生效）");
        let err = res.err().unwrap_or_default();
        assert!(err.contains("control.toml") || err.contains("解析"), "{err}");
    }

    #[test]
    fn secret_never_comes_from_env_anymore() {
        // docs/13 §5.1：密钥/端口/存储改由配置文件保管，**不再读环境变量**
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = write_config(dir.path(), "[auth]
jwt_ttl = \"1h\"
");
        std::env::set_var("PONK_JWT_SECRET", "should-be-ignored");
        let o = parse_opts(&args(&["--config", path.to_str().unwrap()])).expect("parse");
        std::env::remove_var("PONK_JWT_SECRET");
        assert_eq!(o.jwt_secret, None, "环境变量不再作为密钥来源");
    }
}
