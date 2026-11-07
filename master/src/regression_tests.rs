use crate::{cluster, config, db, model, sub, supervisor};
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::Mutex;

async fn pool() -> SqlitePool {
    let p = std::env::temp_dir().join(format!("sbx-review-{}.db", uuid::Uuid::new_v4()));
    db::init_pool(p.to_str().unwrap()).await.unwrap()
}

async fn fixture(p: &SqlitePool) -> (i64, i64, i64) {
    let (a, _) = db::agent_repo::create(p, "a", 0).await.unwrap();
    let mut params = model::node::NodeParams::default();
    crate::secrets::fill(model::node::Protocol::VlessWs, &mut params).unwrap();
    let (n, _) =
        db::node_repo::add_node(p, a, "ws", model::node::Protocol::VlessWs, 10000, &params)
            .await
            .unwrap();
    let u = db::node_repo::add_user(p, "alice", 1000, 0).await.unwrap();
    db::node_repo::assign_node(p, u, n).await.unwrap();
    (a, n, u)
}

#[tokio::test]
async fn recreated_user_must_not_inherit_old_traffic() {
    let p = pool().await;
    let (a, n, original_uid) = fixture(&p).await;
    let old_name: String = sqlx::query_scalar("SELECT traffic_name FROM users WHERE id = ?")
        .bind(original_uid)
        .fetch_one(&p)
        .await
        .unwrap();
    let report = sbx_shared::StatsReport {
        counter_epoch: "same-agent-process".into(),
        users: vec![sbx_shared::UserCounter {
            name: old_name,
            tag: "ws".into(),
            up: 100,
            down: 200,
        }],
    };
    cluster::ingest::ingest_stats(&p, a, &report, 1).await.unwrap();
    db::node_repo::add_user(&p, "keep-id-from-being-reused", 0, 1).await.unwrap();
    db::node_repo::delete_user(&p, "alice").await.unwrap();
    let u = db::node_repo::add_user(&p, "alice", 1000, 2).await.unwrap();
    assert_ne!(original_uid, u, "this probe also excludes SQLite id reuse as the cause");
    db::node_repo::assign_node(&p, u, n).await.unwrap();
    cluster::ingest::ingest_stats(&p, a, &report, 3).await.unwrap();
    let total: i64 = sqlx::query_scalar(
        "SELECT cycle_up + cycle_down FROM user_traffic_total WHERE user_id = ?",
    )
    .bind(u)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(total, 0, "new user must not inherit retired counters");
    let new_name: String = sqlx::query_scalar("SELECT traffic_name FROM users WHERE id = ?")
        .bind(u)
        .fetch_one(&p)
        .await
        .unwrap();
    let report = sbx_shared::StatsReport {
        counter_epoch: report.counter_epoch,
        users: vec![sbx_shared::UserCounter { name: new_name, tag: "ws".into(), up: 10, down: 20 }],
    };
    cluster::ingest::ingest_stats(&p, a, &report, 4).await.unwrap();
    let total: i64 = sqlx::query_scalar(
        "SELECT cycle_up + cycle_down FROM user_traffic_total WHERE user_id = ?",
    )
    .bind(u)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(total, 30, "new identity still accounts its own traffic");
}

#[tokio::test]
async fn quota_failure_must_not_lose_revision_forever() {
    let p = pool().await;
    let (a, _, u) = fixture(&p).await;
    sqlx::query("UPDATE users SET expire_at = 1 WHERE id = ?").bind(u).execute(&p).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_revision BEFORE UPDATE OF user_state_revision ON agents BEGIN SELECT RAISE(FAIL, 'injected revision write failure'); END").execute(&p).await.unwrap();
    let registry = Arc::new(Mutex::new(cluster::Registry::new()));
    let rpc = Arc::new(cluster::Rpc::new());
    assert!(supervisor::tick(&p, &registry, &rpc, 100).await.is_err());
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM users WHERE id = ?")
        .bind(u)
        .fetch_one(&p)
        .await
        .unwrap();
    assert!(enabled, "failed revision write must roll back the disable");
    sqlx::query("DROP TRIGGER fail_revision").execute(&p).await.unwrap();
    let summary = supervisor::tick(&p, &registry, &rpc, 101).await.unwrap();
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM users WHERE id = ?")
        .bind(u)
        .fetch_one(&p)
        .await
        .unwrap();
    let revision: i64 = sqlx::query_scalar("SELECT user_state_revision FROM agents WHERE id = ?")
        .bind(a)
        .fetch_one(&p)
        .await
        .unwrap();
    assert!(!enabled);
    assert!(
        revision > 0,
        "disable committed, revision stayed zero, and next tick does not recover: {summary:?}"
    );
}

fn export(tag: &str, agent: &str) -> sub::ExportNode {
    sub::ExportNode {
        tag: tag.into(),
        agent_name: agent.into(),
        protocol: model::node::Protocol::VlessWs,
        listen_port: 1234,
        params: model::node::NodeParams::default(),
        agent_ipv4: Some("192.0.2.1".into()),
        agent_ipv6: None,
    }
}

#[test]
fn clash_names_must_be_unique_after_disambiguation() {
    let user = sub::SubUser {
        name: "alice".into(),
        uuid: uuid::Uuid::new_v4().to_string(),
        password: "pw".into(),
    };
    let yaml = sub::generate_clash_yaml(
        &user,
        &[
            export("edge", "a"),
            export("edge", "b"),
            export("edge [a]", "c"),
            export("DIRECT", "d"),
            export("节点选择", "e"),
            export("自动选择", "f"),
            export("edge [a] (2)", "g"),
        ],
        &sub::ExportOptions::default(),
    );
    let names: Vec<_> = yaml
        .split("proxy-groups:")
        .next()
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("  - name: "))
        .collect();
    let unique: std::collections::HashSet<_> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "generated duplicate proxy names: {names:?}");
}

#[test]
fn toml_settings_must_preserve_valid_commented_sections() {
    let before = "[telegram] # notifications\nenabled = false\n";
    config::Config::parse(before).unwrap();
    let after = config::replace_in_toml(before, "telegram", "enabled", "true").unwrap();
    assert!(config::Config::parse(&after).is_ok(), "settings update corrupted valid TOML: {after}");
}

#[test]
fn toml_edits_handle_multiline_arrays_and_refuse_invalid_types() {
    let before =
        "[\"telegram\"] # keep me\nadmin_chat_ids = [\n  123,\n  456,\n]\nenabled = false\n";
    let after = config::replace_in_toml(before, "telegram", "admin_chat_ids", "[789]").unwrap();
    assert!(after.contains("# keep me"));
    assert_eq!(config::Config::parse(&after).unwrap().telegram.admin_chat_ids, vec![789]);
    let path =
        std::env::temp_dir().join(format!("sbx-config-regression-{}.toml", uuid::Uuid::new_v4()));
    std::fs::write(&path, before).unwrap();
    assert!(
        config::set_value(path.to_str().unwrap(), "telegram", "enabled", "\"not a bool\"").is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn sysinfo_refreshes_addresses_and_clears_only_confirmed_removals() {
    let p = pool().await;
    let (a, _, _) = fixture(&p).await;
    sqlx::query("UPDATE agents SET ipv4 = '198.51.100.1', ipv6 = '2600:1700::1' WHERE id = ?")
        .bind(a)
        .execute(&p)
        .await
        .unwrap();
    let revision: i64 = sqlx::query_scalar("SELECT config_revision FROM agents WHERE id = ?")
        .bind(a)
        .fetch_one(&p)
        .await
        .unwrap();
    let raw = serde_json::json!({"boot_id":"b", "nic":{"rx":0,"tx":0}, "cpu_pct":0, "mem_used":0, "mem_total":0, "load1":0, "uptime_secs":0});
    let mut report: sbx_shared::SysinfoReport = serde_json::from_value(raw).unwrap();
    // Old agents omit the extension entirely.
    cluster::ingest::ingest_sysinfo(&p, a, &report, 1).await.unwrap();
    let cases = [
        (
            serde_json::json!({"ipv6_local":["2600:1700:0:0:0:0:0:1"]}),
            Some("198.51.100.1"),
            Some("2600:1700::1"),
        ),
        (
            serde_json::json!({"ipv4":"198.51.100.2", "ipv6":"2a06:9801::1", "ipv6_local":["2a06:9801::1"]}),
            Some("198.51.100.2"),
            Some("2a06:9801::1"),
        ),
        (serde_json::json!({}), Some("198.51.100.2"), Some("2a06:9801::1")),
        (
            serde_json::json!({"ipv6_local":["bad inventory"]}),
            Some("198.51.100.2"),
            Some("2a06:9801::1"),
        ),
        (serde_json::json!({"ipv6_local":["2600:1700::2"]}), Some("198.51.100.2"), None),
        (serde_json::json!({"ipv4":"", "ipv6_local":[]}), None, None),
    ];
    for (update, v4, v6) in cases {
        report.addresses = Some(serde_json::from_value(update).unwrap());
        cluster::ingest::ingest_sysinfo(&p, a, &report, 2).await.unwrap();
        let got: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT ipv4, ipv6 FROM agents WHERE id = ?")
                .bind(a)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!((got.0.as_deref(), got.1.as_deref()), (v4, v6));
    }
    let after: i64 = sqlx::query_scalar("SELECT config_revision FROM agents WHERE id = ?")
        .bind(a)
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(after, revision, "address changes must not rebuild sing-box");
}

#[tokio::test]
async fn monthly_reset_and_revision_roll_back_together() {
    let p = pool().await;
    let (a, n, u) = fixture(&p).await;
    sqlx::query("UPDATE users SET enabled = 0, auto_disabled = 1, reset_day = 1 WHERE id = ?")
        .bind(u)
        .execute(&p)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_traffic (user_id, node_id, cycle_up, updated_at) VALUES (?, ?, 300, 0)",
    )
    .bind(u)
    .bind(n)
    .execute(&p)
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER fail_revision BEFORE UPDATE OF user_state_revision ON agents BEGIN SELECT RAISE(FAIL, 'injected failure'); END").execute(&p).await.unwrap();
    let registry = Arc::new(Mutex::new(cluster::Registry::new()));
    let rpc = Arc::new(cluster::Rpc::new());
    assert!(supervisor::tick(&p, &registry, &rpc, 1_700_000_000).await.is_err());
    let state: (bool, String, i64) = sqlx::query_as("SELECT u.enabled, u.last_reset_ym, t.cycle_up FROM users u JOIN user_traffic t ON t.user_id = u.id WHERE u.id = ?").bind(u).fetch_one(&p).await.unwrap();
    assert_eq!(state, (false, String::new(), 300));
    let revision: i64 = sqlx::query_scalar("SELECT user_state_revision FROM agents WHERE id = ?")
        .bind(a)
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(revision, 0);
}

#[tokio::test]
async fn legacy_user_counters_keep_their_baseline_until_user_is_deleted() {
    let p = pool().await;
    let (a, n, u) = fixture(&p).await;
    sqlx::query("UPDATE users SET traffic_name = NULL WHERE id = ?")
        .bind(u)
        .execute(&p)
        .await
        .unwrap();
    let mut report = sbx_shared::StatsReport {
        counter_epoch: "legacy".into(),
        users: vec![sbx_shared::UserCounter {
            name: "alice".into(),
            tag: "ws".into(),
            up: 100,
            down: 0,
        }],
    };
    cluster::ingest::ingest_stats(&p, a, &report, 1).await.unwrap();
    report.users[0].up = 150;
    cluster::ingest::ingest_stats(&p, a, &report, 2).await.unwrap();
    let total: i64 =
        sqlx::query_scalar("SELECT cycle_up FROM user_traffic_total WHERE user_id = ?")
            .bind(u)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(total, 150);
    db::node_repo::delete_user(&p, "alice").await.unwrap();
    let new = db::node_repo::add_user(&p, "alice", 0, 3).await.unwrap();
    db::node_repo::assign_node(&p, new, n).await.unwrap();
    assert_eq!(cluster::ingest::ingest_stats(&p, a, &report, 4).await.unwrap().unknown, 1);
    let total: i64 =
        sqlx::query_scalar("SELECT cycle_up FROM user_traffic_total WHERE user_id = ?")
            .bind(new)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(total, 0);
}

#[tokio::test]
async fn migration_keeps_existing_config_and_traffic() {
    let path =
        std::env::temp_dir().join(format!("sbx-migration-identity-{}.db", uuid::Uuid::new_v4()));
    let p = db::init_pool(path.to_str().unwrap()).await.unwrap();
    let (a, n, u) = fixture(&p).await;
    sqlx::query("UPDATE users SET traffic_name = NULL WHERE id = ?")
        .bind(u)
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_traffic (user_id, node_id, counter_epoch, last_up, cycle_up, total_up, updated_at) VALUES (?, ?, 'old', 150, 150, 150, 0)").bind(u).bind(n).execute(&p).await.unwrap();
    let before = crate::service::build_agent_config(&p, a).await.unwrap();
    sqlx::query("DROP INDEX idx_users_traffic_name").execute(&p).await.unwrap();
    sqlx::query("ALTER TABLE users DROP COLUMN traffic_name").execute(&p).await.unwrap();
    sqlx::query("PRAGMA user_version = 14").execute(&p).await.unwrap();
    p.close().await;
    let p = db::init_pool(path.to_str().unwrap()).await.unwrap();
    assert_eq!(crate::service::build_agent_config(&p, a).await.unwrap(), before);
    let revision: i64 = sqlx::query_scalar("SELECT user_state_revision FROM agents WHERE id = ?")
        .bind(a)
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(revision, 1);
    let report = sbx_shared::StatsReport {
        counter_epoch: "old".into(),
        users: vec![sbx_shared::UserCounter {
            name: "alice".into(),
            tag: "ws".into(),
            up: 200,
            down: 0,
        }],
    };
    cluster::ingest::ingest_stats(&p, a, &report, 1).await.unwrap();
    let total: i64 =
        sqlx::query_scalar("SELECT total_up FROM user_traffic_total WHERE user_id = ?")
            .bind(u)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(total, 200, "migration must preserve the previous 150-byte baseline");
}
