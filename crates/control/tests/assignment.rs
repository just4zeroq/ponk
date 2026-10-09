//! 09 §5 分片 → 进程指派。
//!
//! 必测（交付要求）：**承载约束 min(配置上限, 自报) 与 `scatter` 分布**。

mod common;

use ponk_control::assign::{
    assign, shard_count, shard_index, AssignRequest, AssignmentResult, Candidate, ReportedCapacity,
};
use ponk_control::schema::{
    BlockEntry, BlocklistDoc, CapacityLimits, Distribution, PlanInstrument, ProcessCapacityDoc,
    QuotaSpec, Strategy, SubscribePlanDoc,
};
use ponk_protocol::{NodeKind, PlanMode};
use std::collections::{BTreeMap, BTreeSet};

fn instruments(n: usize, venue: &str, market: &str) -> Vec<PlanInstrument> {
    (0..n)
        .map(|i| PlanInstrument {
            venue: venue.to_string(),
            market: market.to_string(),
            native_code: format!("SYM{i:04}"),
            channels: vec!["md.candle.1m".to_string(), "md.trade".to_string()],
            price_precision: 2,
            qty_precision: 3,
            meta: BTreeMap::new(),
        })
        .collect()
}

fn plan_doc(n: usize, shard_size: u64) -> SubscribePlanDoc {
    SubscribePlanDoc {
        shard_size,
        default_channels: vec!["md.candle.1m".to_string(), "md.trade".to_string()],
        instruments: instruments(n, "binance", "swap"),
        quota: vec![QuotaSpec {
            venue: "binance".to_string(),
            egress_ip: "1.2.3.4".to_string(),
            collect: 0.5,
            backfill: 0.35,
            metadata: 0.15,
        }],
    }
}

fn candidate(id: &str, reported: ReportedCapacity) -> Candidate {
    Candidate {
        host: None,
        instance_id: id.to_string(),
        node_kind: NodeKind::Collector,
        reported,
    }
}

fn request(candidates: Vec<Candidate>, doc: SubscribePlanDoc) -> AssignRequest {
    AssignRequest {
        candidates,
        plan_doc: doc,
        blocklist: BlocklistDoc::default(),
        limits: CapacityLimits {
            max_shards_per_process: 12,
            max_instruments_per_process: 4000,
            max_channels_per_process: 8,
        },
        strategy: Strategy::Scatter,
        mode: PlanMode::Snapshot,
        versions: BTreeMap::new(),
        // 默认无 scope 限制（A 方案只在存在节点定义时才写入允许集）
        allowed: BTreeMap::new(),
    }
}

fn total_shards(r: &AssignmentResult) -> usize {
    r.plans.values().map(|a| a.plan.shards.len()).sum()
}

// ── §5.1 切分片 ─────────────────────────────────────────────────────────────

#[test]
fn shard_count_is_power_of_two_per_spec() {
    // shard_count = 2^ceil(log2(ceil(n / shard_size)))
    assert_eq!(shard_count(0, 200), 1, "无标的 → 下界 1");
    assert_eq!(shard_count(200, 200), 1);
    assert_eq!(shard_count(201, 200), 2);
    assert_eq!(shard_count(400, 200), 2);
    assert_eq!(shard_count(401, 200), 4);
    assert_eq!(shard_count(1000, 200), 8, "ceil(1000/200)=5 → 8");
    assert_eq!(shard_count(1600, 200), 8);
    assert_eq!(shard_count(1601, 200), 16);
    // shard_size=0 在发布侧被 schema 拒绝（09 §5.1）；此处仅保证不 panic 且仍取 2 的幂
    assert_eq!(shard_count(10, 0), 16, "shard_size=0 夹到 1 → ceil(10/1)=10 → 16");

    for n in [0usize, 1, 7, 63, 64, 65, 999, 5000] {
        let c = shard_count(n, 100);
        assert!(c.is_power_of_two(), "n={n} → {c} 必须是 2 的幂");
    }
}

#[test]
fn shard_index_is_stable_and_spreads() {
    let a = shard_index("binance:swap:BTCUSDT", 16);
    assert_eq!(a, shard_index("binance:swap:BTCUSDT", 16), "稳定：同标的同分片");
    assert!(a < 16);
    assert_eq!(shard_index("anything", 1), 0);

    // 跨幂边界翻倍：约半数标的留在原分片（09 §5.1 稳定性）
    let codes: Vec<String> = (0..64).map(|i| format!("binance:swap:SYM{i:04}")).collect();
    let stay = codes
        .iter()
        .filter(|c| shard_index(c, 8) == shard_index(c, 16))
        .count();
    assert!(
        stay >= codes.len() / 4,
        "翻倍后应有相当比例标的留在原分片，实际 {stay}/{}",
        codes.len()
    );

    let distinct: BTreeSet<u32> = (0..200)
        .map(|i| shard_index(&format!("binance:swap:SYM{i:04}"), 8))
        .collect();
    assert_eq!(distinct.len(), 8, "200 个标的应铺满 8 个分片");
}

// ── §5.2 承载约束 ───────────────────────────────────────────────────────────

#[test]
fn reported_capacity_roundtrips_through_ops_counters() {
    let mut rc = ReportedCapacity {
        max_shards: Some(5),
        max_instruments: Some(300),
        max_channels: Some(4),
        venues: Some(BTreeSet::from(["binance".to_string()])),
    };
    rc.venues.as_mut().unwrap().insert("okx".to_string());

    let counters = rc.to_counters();
    assert_eq!(counters["capacity.shards"], 5);
    assert_eq!(counters["capacity.instruments"], 300);
    assert_eq!(counters["capacity.channels"], 4);
    assert_eq!(counters["capacity.venue.binance"], 1);
    assert_eq!(counters["capacity.venue.okx"], 1);

    let back = ReportedCapacity::from_counters(&counters);
    assert_eq!(back, rc, "register 自报能力经 counters 往返一致");
}

#[test]
fn effective_capacity_is_min_of_config_and_self_report() {
    let limits = CapacityLimits {
        max_shards_per_process: 12,
        max_instruments_per_process: 4000,
        max_channels_per_process: 8,
    };

    // 自报 > 配置 → 取配置
    let c = Candidate {
        host: None,
        instance_id: "a".into(),
        node_kind: NodeKind::Collector,
        reported: ReportedCapacity {
            max_shards: Some(999),
            max_instruments: Some(99_999),
            max_channels: Some(64),
            venues: None,
        },
    }
    .capacity(&limits);
    assert_eq!(c.max_shards, 12);
    assert_eq!(c.max_instruments, 4000);
    assert_eq!(c.max_channels, 8);

    // 自报 < 配置 → 取自报
    let c = Candidate {
        host: None,
        instance_id: "b".into(),
        node_kind: NodeKind::Collector,
        reported: ReportedCapacity {
            max_shards: Some(3),
            max_instruments: Some(10),
            max_channels: Some(2),
            venues: None,
        },
    }
    .capacity(&limits);
    assert_eq!(c.max_shards, 3, "min(配置上限, 自报)");
    assert_eq!(c.max_instruments, 10);
    assert_eq!(c.max_channels, 2);

    // 未自报 → 该项以配置上限为准
    let c = Candidate {
        host: None,
        instance_id: "c".into(),
        node_kind: NodeKind::Collector,
        reported: ReportedCapacity::default(),
    }
    .capacity(&limits);
    assert_eq!(c.max_shards, 12);
    assert_eq!(c.max_instruments, 4000);
    assert_eq!(c.max_channels, 8);
}

#[test]
fn assignment_respects_self_reported_shard_limit() {
    let doc = plan_doc(64, 4); // 64 标的 / 每片 4 → need 16 → 16 片
    let c = candidate(
        "col-1",
        ReportedCapacity {
            max_shards: Some(4),
            ..ReportedCapacity::default()
        },
    );
    let r = assign(request(vec![c], doc)).unwrap();
    assert_eq!(
        r.plans["col-1"].plan.shards.len(),
        4,
        "有效上限 = min(12, 4) = 4，只装 4 个分片"
    );
    assert!(!r.unassigned.is_empty(), "装不下的分片必须登记为未指派");
}

#[test]
fn unassigned_when_no_candidate_holds_the_venue() {
    let doc = plan_doc(32, 8);
    let c = candidate(
        "col-okx-only",
        ReportedCapacity {
            venues: Some(BTreeSet::from(["okx".to_string()])),
            ..ReportedCapacity::default()
        },
    );
    let r = assign(request(vec![c], doc)).unwrap();
    assert!(r.plans["col-okx-only"].plan.shards.is_empty());
    assert_eq!(r.unassigned.len(), 4, "binance 分片全部无人可承载");
}

// ── §5.3 scatter 分布 ───────────────────────────────────────────────────────

#[test]
fn scatter_spreads_one_venue_across_processes() {
    let doc = plan_doc(64, 4); // 16 个分片
    let candidates = vec![
        candidate("col-1", ReportedCapacity::default()),
        candidate("col-2", ReportedCapacity::default()),
        candidate("col-3", ReportedCapacity::default()),
    ];
    let mut req = request(candidates, doc);
    req.strategy = Strategy::Scatter;

    let r = assign(req).unwrap();
    assert_eq!(r.unassigned.len(), 0, "容量充足时不应有未指派分片");
    assert_eq!(total_shards(&r), 16);

    let per_proc: Vec<usize> = ["col-1", "col-2", "col-3"]
        .iter()
        .map(|id| r.plans[*id].plan.shards.len())
        .collect();
    // 同所打散：每个进程都持有该所的一部分，且大致均分
    assert!(
        per_proc.iter().all(|&n| n >= 5 && n <= 6),
        "scatter 应把同所分片轮转打散，实际 {per_proc:?}"
    );

    // 无重复指派：每个 (venue, market, shard_idx) 只属于一个进程
    let mut seen = BTreeSet::new();
    for a in r.plans.values() {
        for s in &a.plan.shards {
            let key = (s.venue.to_string(), s.market.to_string(), s.shard_idx);
            assert!(seen.insert(key.clone()), "分片 {key:?} 被重复指派");
        }
    }
    assert_eq!(seen.len(), 16);
}

#[test]
fn scatter_respects_venue_affinity_of_self_report() {
    let mut doc = plan_doc(0, 4);
    doc.instruments = instruments(32, "binance", "swap");
    doc.instruments.extend(instruments(32, "okx", "swap"));

    let binance_only = candidate(
        "col-binance",
        ReportedCapacity {
            venues: Some(BTreeSet::from(["binance".to_string()])),
            ..ReportedCapacity::default()
        },
    );
    let both = candidate("col-both", ReportedCapacity::default());

    let mut req = request(vec![binance_only, both], doc);
    req.strategy = Strategy::Scatter;
    let r = assign(req).unwrap();

    for s in &r.plans["col-binance"].plan.shards {
        assert_eq!(s.venue.as_str(), "binance", "自报只装 binance 的进程不得拿到 okx 分片");
    }
    assert!(
        r.plans["col-both"]
            .plan
            .shards
            .iter()
            .any(|s| s.venue.as_str() == "okx"),
        "okx 分片应落到可承载 okx 的进程"
    );
    assert_eq!(r.unassigned.len(), 0);
}

#[test]
fn aggregate_strategy_concentrates_one_venue() {
    let doc = plan_doc(64, 4); // 16 片
    let candidates = vec![
        candidate("col-1", ReportedCapacity::default()),
        candidate("col-2", ReportedCapacity::default()),
    ];
    let mut req = request(candidates, doc);
    req.strategy = Strategy::Aggregate;
    req.limits = CapacityLimits {
        max_shards_per_process: 12,
        max_instruments_per_process: 4000,
        max_channels_per_process: 8,
    };
    let r = assign(req).unwrap();

    assert_eq!(total_shards(&r), 16);
    let n1 = r.plans["col-1"].plan.shards.len();
    let n2 = r.plans["col-2"].plan.shards.len();
    assert!(
        n1 == 12 && n2 == 4,
        "aggregate 应把同所尽量塞进第一个实例直到装不下，实际 col-1={n1} col-2={n2}"
    );
}

// ── §5.3 quota_share 分摊 ───────────────────────────────────────────────────

#[test]
fn quota_share_is_split_by_holder_count_of_that_venue() {
    let doc = plan_doc(64, 4); // 16 片
    let candidates = vec![
        candidate("col-1", ReportedCapacity::default()),
        candidate("col-2", ReportedCapacity::default()),
    ];
    let mut req = request(candidates, doc);
    req.versions = BTreeMap::from([("col-1".to_string(), 7u64), ("col-2".to_string(), 7u64)]);
    let r = assign(req).unwrap();

    assert_eq!(r.holders.get("binance"), Some(&2), "持有 binance 的进程数 = 2");

    for id in ["col-1", "col-2"] {
        let a = &r.plans[id];
        let split = &a.quota_splits["binance"];
        assert_eq!(
            split.collect, 0.25,
            "{id}: 份额 0.5 / 2 个持有进程 = 0.25"
        );
        assert_eq!(split.backfill, 0.175);
        assert_eq!(split.metadata, 0.075);
        assert_eq!(split.egress_ip, "1.2.3.4");
        assert_eq!(split.version, 7, "配额份额随 assignment_version 下发");

        // 线上的单值 quota_share 取该实例首个分片所属的所
        let online = a.plan.quota_share.as_ref().expect("应带配额份额");
        assert_eq!(online.venue.as_str(), "binance");
        assert_eq!(online.collect, 0.25);
        assert_eq!(a.plan.assignment_version, 7);
    }
}

#[test]
fn quota_share_is_not_split_when_only_one_process_holds_the_venue() {
    let doc = plan_doc(32, 8);
    let r = assign(request(
        vec![candidate("only", ReportedCapacity::default())],
        doc,
    ))
    .unwrap();
    assert_eq!(r.holders.get("binance"), Some(&1));
    let q = r.plans["only"].quota_splits["binance"].clone();
    assert!((q.collect - 0.5).abs() < 1e-12, "唯一持有者拿全额份额");
}

// ── §2.1 黑名单 ⊂ 白名单 ───────────────────────────────────────────────────

#[test]
fn blocklist_removes_or_narrows_instruments() {
    let doc = plan_doc(16, 4);
    let full = BlocklistDoc {
        entries: vec![BlockEntry {
            instrument: "binance:swap:SYM0000".to_string(),
            channels: vec![],
            reason: Some("delisted".into()),
            since: None,
            note: None,
        }],
    };
    let mut req = request(vec![candidate("c", ReportedCapacity::default())], doc.clone());
    req.blocklist = full;
    let r = assign(req).unwrap();
    let codes: BTreeSet<String> = r.plans["c"]
        .plan
        .shards
        .iter()
        .flat_map(|s| s.instruments.iter().map(|i| i.native_code.clone()))
        .collect();
    assert!(!codes.contains("SYM0000"), "全通道黑名单标的应被剔除");
    assert_eq!(codes.len(), 15);

    // 部分通道屏蔽：剔除该通道、标的保留
    let partial = BlocklistDoc {
        entries: vec![BlockEntry {
            instrument: "binance:swap:SYM0001".to_string(),
            channels: vec!["md.trade".to_string()],
            reason: Some("suspended".into()),
            since: None,
            note: None,
        }],
    };
    let mut req = request(vec![candidate("c", ReportedCapacity::default())], doc);
    req.blocklist = partial;
    let r = assign(req).unwrap();
    let ins = r.plans["c"]
        .plan
        .shards
        .iter()
        .flat_map(|s| s.instruments.iter())
        .find(|i| i.native_code == "SYM0001")
        .expect("部分屏蔽的标的仍在白名单内");
    assert_eq!(
        ins.channels.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        vec!["md.candle.1m"],
        "被屏蔽通道已剔除"
    );
    assert!(ins.enabled, "enabled 不承担黑名单语义（09 §2.1 黑名单是独立域）");
}

// ── §5.4 指派结果的确定性 ───────────────────────────────────────────────────

#[test]
fn assignment_is_deterministic_for_same_input() {
    let build = || {
        let doc = plan_doc(97, 7);
        let candidates = vec![
            candidate("col-a", ReportedCapacity::default()),
            candidate("col-b", ReportedCapacity::default()),
        ];
        let mut versions = BTreeMap::new();
        versions.insert("col-a".to_string(), 3u64);
        versions.insert("col-b".to_string(), 3u64);
        let mut req = request(candidates, doc);
        req.versions = versions;
        assign(req).unwrap()
    };
    let a = build();
    let b = build();
    for id in ["col-a", "col-b"] {
        assert_eq!(
            serde_json::to_value(&a.plans[id].plan.shards).unwrap(),
            serde_json::to_value(&b.plans[id].plan.shards).unwrap(),
            "{id} 指派必须确定"
        );
        assert_eq!(a.plans[id].plan.assignment_version, 3);
    }
    assert_eq!(a.holders, b.holders);
    assert_eq!(a.unassigned, b.unassigned);
}

#[test]
fn process_capacity_domain_drives_strategy_and_limits() {
    let doc: ProcessCapacityDoc = serde_json::from_value(serde_json::json!({
        "limits": {"max_shards_per_process": 1, "max_instruments_per_process": 64,
                   "max_channels_per_process": 2},
        "distribution": {"strategy": "aggregate", "order": "round_robin"}
    }))
    .unwrap();
    assert_eq!(Strategy::parse(&doc.distribution.strategy).unwrap(), Strategy::Aggregate);
    assert_eq!(doc.limits.max_shards_per_process, 1);

    // 策略取值非法 → 校验拒绝（09 §3.5）
    let bad: ProcessCapacityDoc =
        serde_json::from_value(serde_json::json!({"distribution": {"strategy": "hash"}})).unwrap();
    assert!(Strategy::parse(&bad.distribution.strategy).is_err());
    assert_eq!(
        Distribution::default().strategy,
        "scatter",
        "缺省分布策略 = scatter（09 §5.3）"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// A 方案（docs/12 §5 / 模型 1）：逐实例 scope 允许集
//
// 数据量取 16 标的/所 × shard_size 4 → 每所 4 个分片，低于 request() 的
// max_shards_per_process = 12 —— 否则会撞容量上限而与 scope 无关。
// ─────────────────────────────────────────────────────────────────────────────

fn allowed_for(prefix: &str, n: usize) -> BTreeSet<String> {
    (0..n).map(|i| format!("{prefix}:swap:SYM{i:04}")).collect()
}

/// 逐实例允许集：每个实例只拿到自己声明的范围，且覆盖完整、互不重复。
#[test]
fn scope_allowed_set_gates_which_instance_gets_which_shards() {
    let mut doc = plan_doc(16, 4); // binance:swap
    doc.instruments.extend(instruments(16, "okx", "swap")); // 并集池

    let candidates = vec![
        candidate("binance-col", ReportedCapacity::default()),
        candidate("okx-col", ReportedCapacity::default()),
    ];
    let mut req = request(candidates, doc);
    req.strategy = Strategy::Scatter;
    let mut allowed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    allowed.insert("binance-col".into(), allowed_for("binance", 16));
    allowed.insert("okx-col".into(), allowed_for("okx", 16));
    req.allowed = allowed;

    let r = assign(req).unwrap();
    assert_eq!(r.unassigned.len(), 0, "两份范围都被覆盖 → 不应有未指派");

    for (id, venue) in [("binance-col", "binance"), ("okx-col", "okx")] {
        let a = r.plans.get(id).unwrap_or_else(|| panic!("{id} 应拿到计划"));
        assert!(
            a.plan.shards.iter().all(|s| s.venue.as_str() == venue),
            "{id} 只应持有 {venue} 的分片"
        );
        let codes: usize = a.plan.shards.iter().map(|s| s.instruments.len()).sum();
        assert_eq!(codes, 16, "{id} 应拿满自己范围内的 16 个标的");
    }

    // 无重复指派（模型 1：每个分片只落一个实例）
    let mut seen = BTreeSet::new();
    for a in r.plans.values() {
        for s in &a.plan.shards {
            assert!(seen.insert((s.venue.to_string(), s.market.to_string(), s.shard_idx)));
        }
    }
    assert_eq!(seen.len(), 8, "两个所各 4 个分片");
}

/// 范围收窄到一半：范围外的分片必须**如实报 scope 原因**，不能伪装成"承载不足"。
#[test]
fn shard_outside_every_scope_is_reported_as_scope_miss() {
    let doc = plan_doc(16, 4); // 全是 binance:swap
    let candidates = vec![
        candidate("binance-col", ReportedCapacity::default()),
        candidate("okx-col", ReportedCapacity::default()),
    ];
    let mut req = request(candidates, doc);
    req.strategy = Strategy::Scatter;
    let mut limited: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // 只声明前 8 个；okx-col 声明了但范围为空集
    limited.insert("binance-col".into(), allowed_for("binance", 8));
    limited.insert("okx-col".into(), BTreeSet::new());
    req.allowed = limited;

    let r = assign(req).unwrap();
    assert!(
        r.unassigned.iter().any(|u| u.3.contains("scope 未覆盖")),
        "范围外的分片必须报 scope 原因，实际：{:?}",
        r.unassigned
    );
    assert!(
        r.unassigned.iter().all(|u| !u.3.contains("承载不足")),
        "被 scope 挡下不能误报成容量不足"
    );
    let placed: usize = r
        .plans
        .get("binance-col")
        .map(|a| a.plan.shards.iter().map(|s| s.instruments.len()).sum())
        .unwrap_or(0);
    assert!(placed > 0 && placed < 16, "只应放置完全落在范围内的分片，实际 {placed}");
}
