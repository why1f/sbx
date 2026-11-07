use crate::{cluster, db, model, supervisor};
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::Mutex;

async fn fixture() -> (SqlitePool, i64, i64, i64, model::node::NodeParams) {
    let path = std::env::temp_dir().join(format!("sbx-review-probe-{}.db", uuid::Uuid::new_v4()));
    let p = db::init_pool(path.to_str().unwrap()).await.unwrap();
    let (a, _) = db::agent_repo::create(&p, "a", 0).await.unwrap();
    let mut params = model::node::NodeParams::default();
    crate::secrets::fill(model::node::Protocol::VlessWs, &mut params).unwrap();
    let (n, _) =
        db::node_repo::add_node(&p, a, "ws", model::node::Protocol::VlessWs, 10000, &params)
            .await
            .unwrap();
    let u = db::node_repo::add_user(&p, "alice", 100, 0).await.unwrap();
    db::node_repo::assign_node(&p, u, n).await.unwrap();
    (p, a, n, u, params)
}

async fn report(p: &SqlitePool, u: i64) -> sbx_shared::StatsReport {
    let name = sqlx::query_scalar("SELECT traffic_name FROM users WHERE id = ?")
        .bind(u)
        .fetch_one(p)
        .await
        .unwrap();
    sbx_shared::StatsReport {
        counter_epoch: "same-process".into(),
        users: vec![sbx_shared::UserCounter { name, tag: "ws".into(), up: 100, down: 0 }],
    }
}

#[tokio::test]
async fn regression_recreated_node_must_not_resurrect_reset_traffic() {
    let (p, a, n, u, params) = fixture().await;
    let r = report(&p, u).await;
    cluster::ingest::ingest_stats(&p, a, &r, 1).await.unwrap();
    db::node_repo::reset_user_traffic(&p, u).await.unwrap();
    db::node_repo::add_node(&p, a, "keep-id", model::node::Protocol::VlessWs, 10001, &params)
        .await
        .unwrap();
    db::node_repo::delete_node(&p, n).await.unwrap();
    let (new, _) =
        db::node_repo::add_node(&p, a, "ws", model::node::Protocol::VlessWs, 10000, &params)
            .await
            .unwrap();
    assert_ne!(n, new);
    db::node_repo::assign_node(&p, u, new).await.unwrap();
    cluster::ingest::ingest_stats(&p, a, &r, 2).await.unwrap();
    let used: i64 = sqlx::query_scalar("SELECT cycle_up FROM user_traffic_total WHERE user_id=?")
        .bind(u)
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(
        used, 0,
        "no new traffic occurred; retired node counters must not return after a cycle reset"
    );
}

#[tokio::test]
async fn regression_deleting_node_must_not_refund_consumed_quota() {
    let (p, a, n, u, _) = fixture().await;
    let r = report(&p, u).await;
    cluster::ingest::ingest_stats(&p, a, &r, 1).await.unwrap();
    let reg = Arc::new(Mutex::new(cluster::Registry::new()));
    let rpc = Arc::new(cluster::Rpc::new());
    supervisor::tick(&p, &reg, &rpc, 10).await.unwrap();
    assert!(!db::node_repo::get_user_by_name(&p, "alice").await.unwrap().unwrap().enabled);
    db::node_repo::delete_node(&p, n).await.unwrap();
    supervisor::tick(&p, &reg, &rpc, 11).await.unwrap();
    assert!(
        !db::node_repo::get_user_by_name(&p, "alice").await.unwrap().unwrap().enabled,
        "deleting infrastructure must not refund already consumed user quota"
    );
}

#[tokio::test]
async fn regression_claimed_commands_must_recover_after_restart() {
    let (p, a, _, _, _) = fixture().await;
    let id =
        db::command_repo::enqueue(&p, a, "config_check", &serde_json::json!({}), 1).await.unwrap();
    assert_eq!(db::command_repo::take_pending(&p, 2).await.unwrap().len(), 1);
    let later = db::command_repo::take_pending(&p, 86400).await.unwrap();
    let outcome = db::command_repo::outcome(&p, id).await.unwrap().unwrap();
    assert!(
        outcome.done || !later.is_empty(),
        "a day later: taken={}, done={}, pending={}; command never recovers",
        outcome.taken,
        outcome.done,
        db::command_repo::pending_count(&p).await.unwrap()
    );
}

#[tokio::test]
async fn retired_traffic_keeps_history_across_repeated_deletes_and_resets() {
    let (p, a, mut n, u, params) = fixture().await;
    let mut r = report(&p, u).await;
    for amount in [100, 150, 200] {
        r.users[0].up = amount;
        cluster::ingest::ingest_stats(&p, a, &r, 1).await.unwrap();
        db::node_repo::delete_node(&p, n).await.unwrap();
        let total: i64 =
            sqlx::query_scalar("SELECT total_up FROM user_traffic_total WHERE user_id=?")
                .bind(u)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(total, amount);
        (n, _) =
            db::node_repo::add_node(&p, a, "ws", model::node::Protocol::VlessWs, 10000, &params)
                .await
                .unwrap();
        db::node_repo::assign_node(&p, u, n).await.unwrap();
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retired_user_traffic")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(rows, 1, "repeated deletions must not grow history indefinitely");
    db::node_repo::reset_user_traffic(&p, u).await.unwrap();
    r.users[0].up = 210;
    cluster::ingest::ingest_stats(&p, a, &r, 2).await.unwrap();
    db::agent_repo::delete(&p, a).await.unwrap();
    let totals: (i64, i64) =
        sqlx::query_as("SELECT cycle_up,total_up FROM user_traffic_total WHERE user_id=?")
            .bind(u)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(totals, (10, 210));
    sqlx::query("UPDATE users SET reset_day=1").execute(&p).await.unwrap();
    supervisor::tick(
        &p,
        &Arc::new(Mutex::new(cluster::Registry::new())),
        &Arc::new(cluster::Rpc::new()),
        1_700_000_000,
    )
    .await
    .unwrap();
    let totals: (i64, i64) =
        sqlx::query_as("SELECT cycle_up,total_up FROM user_traffic_total WHERE user_id=?")
            .bind(u)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(totals, (0, 210));
    db::node_repo::delete_user(&p, "alice").await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retired_user_traffic")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn failed_node_deletion_rolls_back_archiving() {
    let (p, a, n, u, _) = fixture().await;
    cluster::ingest::ingest_stats(&p, a, &report(&p, u).await, 1).await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_node_delete BEFORE DELETE ON nodes BEGIN SELECT RAISE(FAIL, 'injected'); END")
        .execute(&p).await.unwrap();
    assert!(db::node_repo::delete_node(&p, n).await.is_err());
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retired_user_traffic")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    let total: i64 = sqlx::query_scalar("SELECT total_up FROM user_traffic_total WHERE user_id=?")
        .bind(u)
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(total, 100);
}

#[tokio::test]
async fn new_counter_epoch_after_recreating_node_still_counts_new_traffic() {
    let (p, a, n, u, params) = fixture().await;
    let mut r = report(&p, u).await;
    cluster::ingest::ingest_stats(&p, a, &r, 1).await.unwrap();
    db::node_repo::delete_node(&p, n).await.unwrap();
    let (new, _) =
        db::node_repo::add_node(&p, a, "ws", model::node::Protocol::VlessWs, 10000, &params)
            .await
            .unwrap();
    db::node_repo::assign_node(&p, u, new).await.unwrap();
    r.counter_epoch = "restarted-process".into();
    r.users[0].up = 20;
    cluster::ingest::ingest_stats(&p, a, &r, 2).await.unwrap();
    let total: i64 = sqlx::query_scalar("SELECT total_up FROM user_traffic_total WHERE user_id=?")
        .bind(u)
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(total, 120);
}
