//! 10 §4.3 命令信道语义：**旧版丢弃 / 新版整包应用**。
//!
//! 必测（交付要求）：`assignment_version` 旧版丢弃 / 新版整包应用的定序逻辑。

use ponk_control::{next_assignment_version, OrderDecision, VersionGate};
use ponk_protocol::{InstrumentMeta, PlanMode, ShardPlan, SubscribePlan, Venue, Market};

fn plan(version: u64, shard_idx: u32) -> SubscribePlan {
    SubscribePlan {
        assignment_version: version,
        mode: PlanMode::Snapshot,
        shards: vec![ShardPlan {
            venue: Venue::binance(),
            market: Market::swap(),
            shard_idx,
            instruments: vec![InstrumentMeta {
                native_code: "BTCUSDT".into(),
                price_precision: 1,
                qty_precision: 0,
                channels: vec![ponk_protocol::Channel::new("md.candle.1m")],
                enabled: true,
                meta: Default::default(),
            }],
        }],
        quota_share: None,
    }
}

#[test]
fn older_or_duplicate_version_is_dropped() {
    let mut gate = VersionGate::new(0);

    assert_eq!(gate.accept(1), OrderDecision::Applied, "新版 → 应用");
    assert_eq!(gate.applied(), 1);

    assert_eq!(gate.accept(1), OrderDecision::Dropped, "重复版 → 丢弃");
    assert_eq!(gate.applied(), 1, "丢弃不得回退已应用版本");

    assert_eq!(gate.accept(0), OrderDecision::Dropped, "旧版 → 丢弃");
    assert_eq!(gate.applied(), 1);

    assert_eq!(gate.accept(2), OrderDecision::Applied);
    assert_eq!(gate.applied(), 2);
}

#[test]
fn out_of_order_delivery_keeps_latest_package() {
    let mut gate = VersionGate::new(0);

    // 线上乱序到达：5 → 3 → 4 → 6 → 2
    assert_eq!(gate.accept(5), OrderDecision::Applied);
    assert_eq!(gate.accept(3), OrderDecision::Dropped);
    assert_eq!(gate.accept(4), OrderDecision::Dropped);
    assert_eq!(gate.accept(6), OrderDecision::Applied);
    assert_eq!(gate.accept(2), OrderDecision::Dropped);
    assert_eq!(gate.applied(), 6, "收敛判据 = 最新一次下发");
}

#[test]
fn new_version_is_applied_whole_not_merged() {
    let mut gate = VersionGate::new(0);

    // v1：两个分片
    let mut v1 = plan(1, 0);
    v1.shards.push(ShardPlan {
        venue: Venue::binance(),
        market: Market::swap(),
        shard_idx: 1,
        instruments: vec![],
    });
    let applied = gate
        .accept_package(v1.clone())
        .expect("新版整包应用");
    assert_eq!(applied.shards.len(), 2);
    assert_eq!(applied.assignment_version, 1);

    // v2：只留一个分片 —— 必须**整包覆盖**，不得与 v1 合并
    let v2 = plan(2, 7);
    let applied = gate.accept_package(v2.clone()).expect("新版整包应用");
    assert_eq!(
        applied.shards.len(),
        1,
        "整包应用：v1 的分片不应残留"
    );
    assert_eq!(applied.shards[0].shard_idx, 7);
    assert_eq!(applied.assignment_version, 2);

    // 重推的旧命令到达 → 整条丢弃（连计划本体都不取出）
    assert!(gate.accept_package(plan(1, 99)).is_none(), "旧版整条丢弃");
    assert!(gate.accept_package(plan(2, 99)).is_none(), "重复版整条丢弃");
    assert_eq!(gate.applied(), 2);

    // 更新的版本 → 再次整包应用
    let v3 = plan(3, 42);
    let applied = gate.accept_package(v3).expect("新版整包应用");
    assert_eq!(applied.assignment_version, 3);
    assert_eq!(applied.shards[0].shard_idx, 42);
    assert_eq!(gate.applied(), 3);
}

#[test]
fn gate_starts_at_zero_so_first_delivery_applies() {
    let mut gate = VersionGate::default();
    assert_eq!(gate.applied(), 0);
    assert_eq!(gate.accept(1), OrderDecision::Applied);

    // 控制面侧的版本号生成：每次下发 +1（09 §4.1）
    assert_eq!(next_assignment_version(None), 1);
    assert_eq!(next_assignment_version(Some(1)), 2);
    assert_eq!(next_assignment_version(Some(41)), 42);
}

#[test]
fn gate_survives_restore_from_persisted_version() {
    // 控制面重启后从派生物快照恢复（09 §4.2 / §5.4），门必须从已存版本起步
    let mut gate = VersionGate::new(17);
    assert_eq!(gate.applied(), 17);
    assert_eq!(gate.accept(17), OrderDecision::Dropped, "已应用的版本重推不重复应用");
    assert_eq!(gate.accept(16), OrderDecision::Dropped);
    assert_eq!(gate.accept(18), OrderDecision::Applied);
}
