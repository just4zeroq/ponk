//! 09 §4.2 存储与审计 / §4.5 回滚硬约束。
//!
//! 必测（交付要求）：**域版本递增、审计记录、回滚产生新版本号**。

mod common;

use common::{temp_store, Cleanup};
use ponk_control::{ConfigStore, ControlError, FileConfigStore};
use ponk_protocol::ConfigDomain;
use serde_json::json;

fn sig_rules(marker: &str) -> serde_json::Value {
    json!({
        "rules": [{
            "signal_id": "btc_4h_momentum",
            "signal_name": "BTC 4h 动量",
            "timeframe": "4h",
            "scope": ["binance:swap:*"],
            "inputs": ["md.candle.1m"],
            "graph": [
                {"id": "close", "op": "pick", "args": {"input": "md.candle.1m", "field": "close"}},
                {"id": "rsi", "op": "rsi", "args": {"period": 14}, "in": ["close"]}
            ],
            "emit": {
                "value": "rsi",
                "payload": ["rsi"],
                "direction": {"long": {"field": "rsi", "op": ">", "value": 70}, "default": "Flat"}
            }
        }],
        "marker": marker
    })
}

#[test]
fn domain_version_increments_and_audit_is_appended() {
    let dir = temp_store("store");
    let _c = Cleanup::new(dir.clone());
    let store = FileConfigStore::open(&dir).unwrap();

    assert_eq!(
        store.current_version(ConfigDomain::SignalRules).unwrap(),
        None,
        "空存储无 current 指针"
    );

    let v1 = store
        .publish(
            ConfigDomain::SignalRules,
            sig_rules("first"),
            "cp-1/alice",
            "init",
        )
        .unwrap();
    assert_eq!(v1.version, 1);
    assert_eq!(v1.prev_version, None);

    let v2 = store
        .publish(
            ConfigDomain::SignalRules,
            sig_rules("second"),
            "cp-1/bob",
            "tune rsi period",
        )
        .unwrap();
    assert_eq!(v2.version, 2, "域版本 +1");
    assert_eq!(v2.prev_version, Some(1));
    assert_eq!(store.current_version(ConfigDomain::SignalRules).unwrap(), Some(2));

    let v3 = store
        .publish(ConfigDomain::SignalRules, sig_rules("third"), "cp-1/api", "add scope")
        .unwrap();
    assert_eq!(v3.version, 3);

    // 历史内容不可变（09 §4.2）
    let h1 = store.get(ConfigDomain::SignalRules, 1).unwrap().unwrap();
    assert_eq!(h1.version, 1);
    assert_eq!(h1.content["marker"], "first", "v1 内容不被后续发布改写");

    // 审计条目：域、版本、发布者、时间、diff、原因（09 §4.2）
    let audit = store.audit(ConfigDomain::SignalRules).unwrap();
    assert_eq!(audit.len(), 3);
    assert_eq!(audit[0].version, 1);
    assert_eq!(audit[0].publisher, "cp-1/alice");
    assert_eq!(audit[0].reason, "init");
    assert_eq!(audit[1].publisher, "cp-1/bob");
    assert_eq!(audit[1].reason, "tune rsi period");
    assert_eq!(audit[2].reason, "add scope");
    assert!(
        audit[1].diff.iter().any(|d| d.contains("marker")),
        "第二版的 diff 应标出 marker 变更，实际 {:?}",
        audit[1].diff
    );
    assert!(audit[1].ts > 0);
}

#[test]
fn rollback_publishes_new_version_number_and_never_rewinds_current() {
    let dir = temp_store("rollback");
    let _c = Cleanup::new(dir.clone());
    let store = FileConfigStore::open(&dir).unwrap();

    store
        .publish(ConfigDomain::SignalRules, sig_rules("v1"), "cp-1/a", "r1")
        .unwrap();
    store
        .publish(ConfigDomain::SignalRules, sig_rules("v2"), "cp-1/b", "r2")
        .unwrap();
    store
        .publish(ConfigDomain::SignalRules, sig_rules("v3"), "cp-1/c", "r3")
        .unwrap();
    assert_eq!(store.current_version(ConfigDomain::SignalRules).unwrap(), Some(3));

    // 09 §4.5：回滚 = 取历史版本内容**重新发布为新版本号**
    let rolled = store
        .rollback(ConfigDomain::SignalRules, 1, "cp-1/alice")
        .unwrap();

    assert_eq!(
        rolled.version, 4,
        "回滚必须产生新版本号（3 → 4），不能把 current 倒回 1"
    );
    assert_eq!(rolled.prev_version, Some(3));
    assert_eq!(
        store.current_version(ConfigDomain::SignalRules).unwrap(),
        Some(4),
        "current 指针只前移"
    );
    assert_eq!(rolled.reason, "rollback to v1");
    assert_eq!(
        rolled.content["marker"], "v1",
        "回滚内容 = v1 的内容"
    );
    assert_eq!(
        rolled.content["version"], 4,
        "域版本字段由控制面在发布时赋值（09 §3.2）"
    );

    // 历史条目仍然可查、v1 本体不被改写
    assert_eq!(
        store
            .get(ConfigDomain::SignalRules, 1)
            .unwrap()
            .unwrap()
            .content["marker"],
        "v1"
    );
    assert_eq!(
        store.get(ConfigDomain::SignalRules, 4).unwrap().unwrap().version,
        4
    );

    // 回滚本身也走 §4.3 全流程，产生新审计条目
    let audit = store.audit(ConfigDomain::SignalRules).unwrap();
    assert_eq!(audit.len(), 4);
    assert_eq!(audit[3].reason, "rollback to v1");
    assert_eq!(audit[3].version, 4);
    assert_eq!(audit[3].prev_version, Some(3));

    // 再回滚到上一次回滚的产物：版本号继续单调
    let again = store
        .rollback(ConfigDomain::SignalRules, 4, "cp-1/alice")
        .unwrap();
    assert_eq!(again.version, 5);
}

#[test]
fn rollback_of_missing_version_is_an_error() {
    let dir = temp_store("rollback-missing");
    let _c = Cleanup::new(dir.clone());
    let store = FileConfigStore::open(&dir).unwrap();
    store
        .publish(ConfigDomain::Channels, json!({"channels": []}), "cp-1", "x")
        .unwrap();
    let err = store.rollback(ConfigDomain::Channels, 99, "cp-1").unwrap_err();
    assert!(matches!(err, ControlError::NotFound(_)), "实际 {err:?}");
}

#[test]
fn eight_domains_have_independent_version_counters() {
    let dir = temp_store("eight");
    let _c = Cleanup::new(dir.clone());
    let store = FileConfigStore::open(&dir).unwrap();

    for d in ConfigDomain::ALL {
        let payload = default_payload(d);
        store.publish(d, payload, "cp-1/api", "bootstrap").unwrap();
        store.publish(d, default_payload(d), "cp-1/api", "tweak").unwrap();
    }

    for d in ConfigDomain::ALL {
        assert_eq!(
            store.current_version(d).unwrap(),
            Some(2),
            "域 {d} 的版本计数应独立"
        );
        assert_eq!(store.audit(d).unwrap().len(), 2, "域 {d} 的审计独立");
    }

    let versions = store.current_versions().unwrap();
    assert_eq!(versions.len(), 8);
    for d in ConfigDomain::ALL {
        assert_eq!(versions[&d.counter_key()], 2, "{}", d.counter_key());
    }
}

/// 各域一份**能通过 09 §3 schema**的最小文档。
fn default_payload(d: ConfigDomain) -> serde_json::Value {
    match d {
        ConfigDomain::SubscribePlan => serde_json::json!({
            "shard_size": 64,
            "default_channels": ["md.candle.1m"],
            "instruments": [{
                "venue": "binance", "market": "swap", "native_code": "BTCUSDT",
                "price_precision": 1, "qty_precision": 0
            }],
            "quota": [{"venue": "binance", "egress_ip": "1.2.3.4",
                       "collect": 0.5, "backfill": 0.35, "metadata": 0.15}]
        }),
        ConfigDomain::SignalRules => sig_rules("x"),
        ConfigDomain::AlertRules => serde_json::json!({
            "alerts": [{
                "rule_id": "r1", "name": "n",
                "when": {"all": [{"field": "rsi", "op": ">", "value": 70}]},
                "window": {"occurrences": 2, "within": "10m"},
                "level": "Warn",
                "route": [], "throttle": "5m"
            }]
        }),
        ConfigDomain::FactorRules => serde_json::json!({
            "factors": [{
                "factor_id": "funding_basis", "factor_name": "资金费偏离",
                "timeframe": "1m", "inputs": ["md.funding"], "window": "24h",
                "compute": [{"id": "basis", "op": "funding_basis", "args": {"annualize": true}}],
                "emit": {"value": "basis", "meta": ["funding_rate"]}
            }]
        }),
        ConfigDomain::Channels => serde_json::json!({
            "channels": [{"id": "feishu", "backend": "feishu",
                          "endpoint": "https://open.feishu.cn/open-apis/bot/v2/hook/x",
                          "retry": {"attempts": 3, "backoff": "2s"},
                          "limits": {"rate_per_min": 60}}]
        }),
        ConfigDomain::ProcessCapacity => serde_json::json!({
            "limits": {"max_shards_per_process": 12,
                       "max_instruments_per_process": 4000,
                       "max_channels_per_process": 8},
            "distribution": {"strategy": "scatter", "order": "round_robin"}
        }),
        ConfigDomain::Serialization => serde_json::json!({
            "families": {"md.*": "json", "signal.*": "json", "factor.*": "json",
                         "alarm.*": "json", "ctl.*": "json", "ops.*": "json"}
        }),
        _ => serde_json::json!({
            "entries": [{"instrument": "okx:swap:XXX-USDT",
                         "reason": "delisted", "since": "2026-10-01T00:00:00Z",
                         "note": "交易所公告下架"}]
        }),
    }
}
