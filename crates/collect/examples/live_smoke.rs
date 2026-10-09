//! 联网 smoke（**默认不跑**）：连真实交易所、订阅、打印前 N 条事件。
//!
//! 本机访问不了 binance/okx（被墙）、也没有本地 Redis，因此：
//!
//! * **单元测试 / 集成测试一律离线**（`MockSource` + `RecordingSink`）；
//! * 真实联网验证只放在本 example 后面，并用环境变量门控：
//!
//! ```bash
//! PONK_LIVE_SMOKE=1 \
//! PONK_SMOKE_CONFIG=crates/collect/config/collector.example.toml \
//! PONK_SMOKE_EVENTS=5 \
//! cargo run -p ponk-collect --example live_smoke
//! ```
//!
//! 可选：`PONK_SMOKE_VENUE=binance` `PONK_SMOKE_MARKET=spot` 覆盖配置里的第一个连接，
//! `PONK_SMOKE_TIMEOUT_SECS=30` 控制等待时长。
//!
//! 输出的是**信封 JSON**（`event_id` / `shard` / `x_version` 可直接肉眼核对）。
//! 事件中心 backend 尚未落地（03 §9-3），故本 example 不经 `EventCenter::publish`，
//! 而是用 [`ponk_collect::sink::RecordingSink`] 收下后打印——backend 落地后
//! 换成 `CenterSink` 即可。

use std::sync::Arc;
use std::time::Duration;

use ponk_collect::config::CollectorConfig;
use ponk_collect::sink::RecordingSink;
use ponk_collect::Collector;

fn enabled() -> bool {
    matches!(
        std::env::var("PONK_LIVE_SMOKE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    if !enabled() {
        eprintln!(
            "live_smoke 需要显式开启（联网、真实交易所）：\n  \
             PONK_LIVE_SMOKE=1 cargo run -p ponk-collect --example live_smoke\n\n\
             离线验证请用：\n  cargo test -p ponk-collect"
        );
        std::process::exit(2);
    }

    if let Err(e) = run().await {
        eprintln!("live_smoke failed: {e}");
        std::process::exit(1);
    }
}

async fn run() -> ponk_collect::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ponk_collect=info")),
        )
        .init();

    let config_path = std::env::var("PONK_SMOKE_CONFIG")
        .unwrap_or_else(|_| "crates/collect/config/collector.example.toml".to_string());
    let want_events = env_u64("PONK_SMOKE_EVENTS", 5);
    let timeout = Duration::from_secs(env_u64("PONK_SMOKE_TIMEOUT_SECS", 30));

    let mut cfg = CollectorConfig::load(&config_path)?;
    // 可选：只跑一个连接，减少联网面。
    if let (Ok(venue), Ok(market)) = (std::env::var("PONK_SMOKE_VENUE"), std::env::var("PONK_SMOKE_MARKET")) {
        cfg.shards.retain(|s| s.venue.eq_ignore_ascii_case(&venue) && s.market.eq_ignore_ascii_case(&market));
    }
    cfg.instance_id = format!("{}-smoke", cfg.instance_id);

    let sink = Arc::new(RecordingSink::new());
    let mut collector = Collector::new(cfg, sink.clone())?;
    collector.build_dig3_sources().await?;

    let counters = collector.counters();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(collector.run(shutdown_rx));

    // 只数**行情**事件：同一个 sink 里还有 ops 心跳，用 `count()` 会提前满足条件。
    let market_events = || {
        sink.pairs()
            .iter()
            .filter(|(_, e)| e.event_type == ponk_protocol::EventType::Market)
            .count()
    };
    let deadline = tokio::time::Instant::now() + timeout;
    while market_events() < want_events as usize {
        if tokio::time::Instant::now() >= deadline {
            let snap = counters.snapshot();
            eprintln!(
                "timeout after {:?}: got {} event(s); counters = {:#?}",
                timeout,
                sink.count(),
                snap
            );
            break;
        }
        if handle.is_finished() {
            eprintln!("collector exited early");
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let _ = shutdown_tx.send(true);
    handle
        .await
        .map_err(|e| ponk_collect::Error::EventCenter(format!("join: {e}")))??;

    for env in sink.envelopes() {
        let json = serde_json::to_string(&env).map_err(|e| ponk_collect::Error::Envelope(e.to_string()))?;
        println!("{json}");
    }

    println!("---");
    println!("market   : {}", market_events());
    println!("total    : {}", sink.count());
    for (k, v) in counters.snapshot() {
        println!("{:<26} {}", k, v);
    }
    Ok(())
}
