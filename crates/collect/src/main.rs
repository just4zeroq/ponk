//! 采集进程入口。
//!
//! ```text
//! ponk-collect --config crates/collect/config/collector.example.toml
//! ```
//!
//! 启动时序见 10 §7。**TODO(控制面)**：当前第 2/3 步（`ctl.request.<instance>` 拉取
//! 与 `ctl.collector.<instance>` 接收 snapshot）不执行——控制面尚未实现，
//! 订阅计划只从本地文件读（[`ponk_collect::CollectorConfig::load`]）。
//! 第 1 步（`ops.collector.<instance>` register = `Startup` 心跳）与
//! 第 5 步（`md.*` 发数据、每 5s 心跳）已实现。

use std::path::PathBuf;
use std::sync::Arc;

use ponk_collect::command::ControlLink;
use ponk_collect::config::CollectorConfig;
use ponk_collect::sink::EventSink;
use ponk_collect::{Collector, Error, Result};

fn usage() -> &'static str {
    "usage: ponk-collect [--config <path>] [--eventcenter | --stdout]\n\
     \n\
     --config <path>   订阅计划（TOML 或 JSON），默认 ./collector.toml\n\
     --stdout          把事件按 JSON 行打到 stdout，不经总线（调试用）\n\
     --eventcenter     经 EventCenter::publish 发布（默认；backend = memory）\n\
     \n\
     env: RUST_LOG=ponk_collect=debug,dig3=info 控制日志"
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("ponk-collect: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config_path = PathBuf::from("collector.toml");
    let mut use_stdout = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let p = args.next().ok_or_else(|| Error::Config("--config 缺少路径".into()))?;
                config_path = PathBuf::from(p);
            }
            "--eventcenter" => use_stdout = false,
            "--stdout" => use_stdout = true,
            "--help" | "-h" => {
                println!("{}", usage());
                return Ok(());
            }
            other => {
                return Err(Error::Config(format!("未知参数 `{other}`\n{}", usage())));
            }
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ponk_collect=info")),
        )
        .init();

    let cfg = CollectorConfig::load(&config_path)?;
    let plan = cfg.resolve()?;
    tracing::info!(
        target: "ponk::collect",
        config = %config_path.display(),
        instance = %cfg.instance_id,
        assignment_version = plan.assignment_version,
        connections = plan.subscriptions.len(),
        "loaded local subscribe plan"
    );
    if plan.subscriptions.is_empty() {
        return Err(Error::Config("订阅计划为空".into()));
    }

    let (sink, center) = build_sink(use_stdout)?;

    let mut collector = Collector::new(cfg, sink.clone())?;
    collector.build_dig3_sources().await?;

    // 控制面接线（10 §7）：共享句柄必须在 `run()` 消费 collector 之前取出。
    let local_version = collector.plan().assignment_version;
    let instance_id = collector.instance_id().to_string();
    let applied = collector.applied_handle();
    let plan_state = collector.plan_state();
    let reload_tx = collector.reload_sender();
    let counters = collector.counters();
    let control_cfg = collector.config().control.clone();
    let cfg_host = collector.config().host_name();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let run = tokio::spawn(collector.run(shutdown_rx.clone()));

    let mut control_tasks = Vec::new();
    if let Some(center) = center {
        let link = ControlLink::new(
            instance_id.clone(),
            cfg_host.clone(),
            ponk_collect::heartbeat::now_ms(),
            applied.clone(),
            counters,
            sink.clone(),
            control_cfg.session_timeout_ms,
        );
        // 10 §7 第 2 步：启动即拉（拉是正确性保证；推送只是加速）
        if control_cfg.pull_on_start {
            control_tasks.push(tokio::spawn({
                let link = link.clone();
                let sd = shutdown_rx.clone();
                async move {
                    link.pull_until_applied(
                        local_version,
                        control_cfg.pull_retry_ms,
                        control_cfg.max_pull_retries,
                        sd,
                    )
                    .await;
                }
            }));
        }
        // 10 §7 第 3 步：订阅 ctl.collector.<instance>，收到计划即应用
        control_tasks.push(tokio::spawn({
            let link = link.clone();
            let center = center.clone();
            let sd = shutdown_rx.clone();
            async move {
                if let Err(e) = link
                    .run(&*center, plan_state, reload_tx, sd)
                    .await
                {
                    tracing::warn!(target: "ponk::collect", "control link stopped: {e}");
                }
            }
        }));
        tracing::info!(
            target: "ponk::collect",
            instance = %instance_id,
            local_version,
            "control link started (ctl.request / ctl.collector)"
        );
    }

    // Ctrl-C / SIGTERM → 正常下线（10 §5：shutdown 心跳缺失即失联判定）。
    wait_for_signal().await;
    tracing::info!(target: "ponk::collect", "shutdown requested");
    let _ = shutdown_tx.send(true);
    for t in control_tasks {
        let _ = t.await;
    }
    run.await
        .map_err(|e| Error::EventCenter(format!("join: {e}")))??;
    tracing::info!(target: "ponk::collect", "stopped cleanly");
    Ok(())
}

/// 选择事件出口。
///
/// 默认走**真正的** `EventCenter::publish`（03 §4.1），backend 为
/// `MemoryEventCenter`（03 §7.1 的内存 backend，契约层语义完整）。
///
/// TODO(部署)：生产要换成 Redis Streams backend —— `ponk-eventcenter` 的
/// `redis` feature 是编译期开关，本 crate 不替部署方决定；backend 换掉后
/// 只需把 `MemoryEventCenter::new()` 换成 `RedisEventCenter::new(...)`，
/// 采集器侧代码不变（03 §2「上层不应感知 backend 差异」）。
///
/// `--stdout` 用于调试：直接打印信封 JSON 行，不经总线。
fn build_sink(
    use_stdout: bool,
) -> Result<(
    Arc<dyn EventSink>,
    Option<Arc<ponk_eventcenter::MemoryEventCenter>>,
)> {
    if use_stdout {
        tracing::warn!(
            target: "ponk::collect",
            "--stdout: events are printed as JSON lines, NOT published to the event center"
        );
        return Ok((Arc::new(ponk_collect::sink::StdoutSink::new()), None));
    }
    let center = ponk_eventcenter::MemoryEventCenter::new();
    tracing::info!(
        target: "ponk::collect",
        "EventCenter backend = memory (in-process, 03 §7.1); Redis Streams backend 由 ponk-eventcenter 的 `redis` feature 提供"
    );
    // 同一个 center 既要发布（sink）也要开订阅会话（控制面客户端）
    // MemoryEventCenter 是 Clone 的（内部 Arc 共享），克隆一份分别给发布与订阅
    let sink: Arc<dyn EventSink> = Arc::new(ponk_collect::sink::CenterSink::new(center.clone()));
    Ok((sink, Some(Arc::new(center))))
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
