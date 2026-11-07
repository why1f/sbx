use super::*;
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct Calls {
    fail_user: AtomicBool,
    user: AtomicUsize,
    admin: AtomicUsize,
    requests: AtomicUsize,
}

async fn fixture() -> (Ctx, Arc<Calls>, tokio::task::JoinHandle<()>) {
    let path = std::env::temp_dir().join(format!("sbx-tg-recovery-{}.db", uuid::Uuid::new_v4()));
    let pool = crate::db::init_pool(path.to_str().unwrap()).await.unwrap();
    let calls = Arc::new(Calls::default());
    let state = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().fallback(move |uri: axum::http::Uri, body: String| {
            let state = state.clone();
            async move {
                let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
                state.requests.fetch_add(1, Ordering::Relaxed);
                if uri.path().ends_with("sendMessage") {
                    match payload["chat_id"].as_i64() {
                        Some(42) => {
                            state.user.fetch_add(1, Ordering::Relaxed);
                            if state.fail_user.load(Ordering::Relaxed) {
                                return serde_json::json!({"ok": false, "error_code": 500, "description": "temporary failure"}).to_string();
                            }
                        }
                        Some(999) => { state.admin.fetch_add(1, Ordering::Relaxed); }
                        _ => {}
                    }
                }
                serde_json::json!({"ok": true, "result": []}).to_string()
            }
        })).await.unwrap();
    });
    let ctx = Ctx {
        pool,
        cfg: Arc::new(TelegramConfig { admin_chat_ids: vec![999], ..Default::default() }),
        public_base: String::new(),
        api: api::Api::with_base(base),
        offset: FixedOffset::east_opt(0).unwrap(),
        pending: Arc::new(Mutex::new(HashMap::new())),
        lease_owner: uuid::Uuid::new_v4().to_string(),
        active: Arc::new(AtomicBool::new(false)),
    };
    (ctx, calls, task)
}

#[tokio::test]
async fn restarting_bot_waits_for_lease_then_starts_and_releases_on_stop() {
    let (ctx, calls, server) = fixture().await;
    repo::try_acquire_lease(&ctx.pool, "old-process", LEASE_STALE_SECS, Utc::now().timestamp())
        .await
        .unwrap();
    let bot = launch(ctx.clone(), Duration::from_millis(20));
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(calls.requests.load(Ordering::Relaxed), 0, "standby must not contact Telegram");
    // 模拟旧进程崩溃,心跳过期;不需要再次重启新进程。
    sqlx::query("UPDATE tg_bot_lease SET heartbeat=0").execute(&ctx.pool).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while calls.requests.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(ctx.alive());
    bot.stop().await;
    assert!(!ctx.alive());
    let lease: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tg_bot_lease").fetch_one(&ctx.pool).await.unwrap();
    assert_eq!(lease, 0);
    server.abort();
}

#[tokio::test]
async fn stopping_standby_does_not_release_another_process_lease() {
    let (ctx, calls, server) = fixture().await;
    repo::try_acquire_lease(&ctx.pool, "live-process", LEASE_STALE_SECS, Utc::now().timestamp())
        .await
        .unwrap();
    let bot = launch(ctx.clone(), Duration::from_millis(20));
    bot.stop().await;
    let owner: String =
        sqlx::query_scalar("SELECT owner FROM tg_bot_lease").fetch_one(&ctx.pool).await.unwrap();
    assert_eq!(owner, "live-process");
    assert_eq!(calls.requests.load(Ordering::Relaxed), 0);
    server.abort();
}

#[tokio::test]
async fn failed_notifications_retry_only_the_unsuccessful_recipient_with_backoff() {
    let (ctx, calls, server) = fixture().await;
    crate::db::node_repo::add_user(&ctx.pool, "alice", 100, 0).await.unwrap();
    sqlx::query("UPDATE users SET tg_chat_id=42").execute(&ctx.pool).await.unwrap();
    repo::ensure_admin_prefs(&ctx.pool, 999, true, false, "[]").await.unwrap();
    calls.fail_user.store(true, Ordering::Relaxed);
    handle_quota_alert(&ctx, "alice", 85.0).await.unwrap();
    handle_quota_alert(&ctx, "alice", 85.0).await.unwrap();
    assert_eq!(calls.user.load(Ordering::Relaxed), 1, "failure must back off");
    assert_eq!(
        calls.admin.load(Ordering::Relaxed),
        1,
        "successful recipient must not receive duplicates"
    );
    assert_eq!(repo::get_user(&ctx.pool, "alice").await.unwrap().unwrap().last_quota_level, 0);
    calls.fail_user.store(false, Ordering::Relaxed);
    sqlx::query("UPDATE tg_quota_deliveries SET retry_at=0").execute(&ctx.pool).await.unwrap();
    handle_quota_alert(&ctx, "alice", 85.0).await.unwrap();
    assert_eq!(calls.user.load(Ordering::Relaxed), 2);
    assert_eq!(calls.admin.load(Ordering::Relaxed), 1);
    assert_eq!(repo::get_user(&ctx.pool, "alice").await.unwrap().unwrap().last_quota_level, 80);
    reset_fallen_levels(&ctx).await.unwrap();
    handle_quota_alert(&ctx, "alice", 85.0).await.unwrap();
    assert_eq!(calls.user.load(Ordering::Relaxed), 3, "new cycle must notify again");
    assert_eq!(calls.admin.load(Ordering::Relaxed), 2);
    server.abort();
}
