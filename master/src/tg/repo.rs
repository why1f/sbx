//! Telegram 相关的库访问(DESIGN.md §9.1)。
//!
//! 单独一个文件而不是塞进 `db/node_repo.rs`:这些列全是 bot 的私有状态,
//! 与「主控是配置唯一真源」那条主线无关 —— 删掉整个 tg 模块的话,
//! 剩下的部分一行都不用改。

use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::BTreeMap;

/// bot 视角下的一个用户。
///
/// 用 `FromRow` 按**列名**映射,不用元组:字段有 19 个,
/// 而 sqlx 的元组 `FromRow` 只到 16 个;更要紧的是,19 个位置里
/// 有四个相邻的 bool,按位置解构时写反了不会报错,只会让通知开关串位。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TgUser {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub quota_bytes: i64,
    pub traffic_multiplier: f64,
    pub expire_at: Option<i64>,
    pub reset_day: Option<i64>,
    pub sub_token: String,
    pub cycle_up: i64,
    pub cycle_down: i64,

    pub chat_id: i64,
    pub notify_80: bool,
    pub notify_90: bool,
    pub notify_100: bool,
    pub schedule_enabled: bool,
    pub schedule_times_json: String,
    pub last_quota_level: i64,
    pub last_schedule_dates_json: String,
}

impl TgUser {
    pub fn is_bound(&self) -> bool {
        self.chat_id != 0
    }

    /// 计费口径的已用量(含倍率)。与 §6.3 的配额判定、TUI、统计页同一个口径。
    pub fn used(&self) -> i64 {
        let raw = self.cycle_up.saturating_add(self.cycle_down);
        (raw.max(0) as f64 * self.traffic_multiplier.max(0.0)) as i64
    }

    pub fn percent(&self) -> f64 {
        if self.quota_bytes <= 0 {
            return 0.0;
        }
        (self.used() as f64 / self.quota_bytes as f64 * 100.0).max(0.0)
    }

    pub fn remaining(&self) -> Option<i64> {
        (self.quota_bytes > 0).then(|| (self.quota_bytes - self.used()).max(0))
    }

    /// 解析失败一律当空。库里这两列是 bot 自己写的 JSON,
    /// 手改坏了不该让整轮播报炸掉 —— 当成「还没设置过」重来即可。
    pub fn schedule_times(&self) -> Vec<String> {
        serde_json::from_str(&self.schedule_times_json).unwrap_or_default()
    }

    pub fn last_schedule_dates(&self) -> BTreeMap<String, String> {
        serde_json::from_str(&self.last_schedule_dates_json).unwrap_or_default()
    }

    pub fn threshold_enabled(&self, level: u8) -> bool {
        match level {
            80 => self.notify_80,
            90 => self.notify_90,
            100 => self.notify_100,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AdminPrefs {
    pub chat_id: i64,
    pub notify_quota: bool,
    pub schedule_enabled: bool,
    pub schedule_times_json: String,
    pub last_schedule_dates_json: String,
}

impl AdminPrefs {
    pub fn schedule_times(&self) -> Vec<String> {
        serde_json::from_str(&self.schedule_times_json).unwrap_or_default()
    }
    pub fn last_schedule_dates(&self) -> BTreeMap<String, String> {
        serde_json::from_str(&self.last_schedule_dates_json).unwrap_or_default()
    }
}

/// SELECT 列表。别名必须与 `TgUser` 的字段名逐一对应 —— `FromRow` 按名字取值,
/// 对不上会在运行时报「column not found」,而不是编译期。
const USER_COLUMNS: &str = "u.id, u.name, u.enabled, u.quota_bytes, u.traffic_multiplier,
        u.expire_at, u.reset_day, u.sub_token,
        COALESCE(t.cycle_up, 0)   AS cycle_up,
        COALESCE(t.cycle_down, 0) AS cycle_down,
        u.tg_chat_id             AS chat_id,
        u.tg_notify_quota_80     AS notify_80,
        u.tg_notify_quota_90     AS notify_90,
        u.tg_notify_quota_100    AS notify_100,
        u.tg_schedule_enabled    AS schedule_enabled,
        u.tg_schedule_times      AS schedule_times_json,
        u.tg_last_quota_level    AS last_quota_level,
        u.tg_last_schedule_dates AS last_schedule_dates_json";

pub async fn list_users(pool: &SqlitePool) -> Result<Vec<TgUser>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {USER_COLUMNS}
           FROM users u LEFT JOIN user_traffic_total t ON t.user_id = u.id
          ORDER BY u.id"
    ))
    .fetch_all(pool)
    .await?)
}

pub async fn get_user(pool: &SqlitePool, name: &str) -> Result<Option<TgUser>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {USER_COLUMNS}
           FROM users u LEFT JOIN user_traffic_total t ON t.user_id = u.id
          WHERE u.name = ?"
    ))
    .bind(name)
    .fetch_optional(pool)
    .await?)
}

pub async fn user_by_chat(pool: &SqlitePool, chat_id: i64) -> Result<Option<TgUser>> {
    if chat_id == 0 {
        return Ok(None); // 0 是「未绑定」的哨兵,不该匹配到任何人
    }
    Ok(sqlx::query_as(&format!(
        "SELECT {USER_COLUMNS}
           FROM users u LEFT JOIN user_traffic_total t ON t.user_id = u.id
          WHERE u.tg_chat_id = ?"
    ))
    .bind(chat_id)
    .fetch_optional(pool)
    .await?)
}

/// 用绑定码绑定。成功后**清空绑定码**,防止同一个码被重复使用。
///
/// 一个 chat 只能绑一个用户:绑新的之前先把这个 chat 从别处解绑,
/// 否则 `user_by_chat` 会随机返回其中一个。
pub async fn bind(pool: &SqlitePool, code: &str, chat_id: i64) -> Result<Option<String>> {
    let code = code.trim();
    if code.is_empty() {
        return Ok(None);
    }
    let mut tx = pool.begin().await?;
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM users WHERE tg_bind_token = ? AND tg_bind_token <> ''")
            .bind(code)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(name) = name else {
        tx.rollback().await?;
        return Ok(None);
    };
    sqlx::query("UPDATE users SET tg_chat_id = 0 WHERE tg_chat_id = ?")
        .bind(chat_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET tg_chat_id = ?, tg_bind_token = '' WHERE name = ?")
        .bind(chat_id)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(name))
}

pub async fn unbind(pool: &SqlitePool, name: &str) -> Result<()> {
    sqlx::query("UPDATE users SET tg_chat_id = 0 WHERE name = ?")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

/// 生成(或重置)绑定码。返回新码。
pub async fn regenerate_bind_token(pool: &SqlitePool, name: &str) -> Result<String> {
    // 12 字节 base64url ≈ 16 字符。这不是长期凭据,用完即弃,
    // 但仍然走 OsRng —— 可猜的绑定码等于把账号送给猜中的人。
    use base64::Engine as _;
    let mut buf = [0u8; 12];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf);
    sqlx::query("UPDATE users SET tg_bind_token = ? WHERE name = ?")
        .bind(&token)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(token)
}

pub async fn set_notify_settings(
    pool: &SqlitePool,
    name: &str,
    n80: bool,
    n90: bool,
    n100: bool,
    schedule_enabled: bool,
    schedule_times_json: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE users SET tg_notify_quota_80 = ?, tg_notify_quota_90 = ?, tg_notify_quota_100 = ?,
                tg_schedule_enabled = ?, tg_schedule_times = ? WHERE name = ?",
    )
    .bind(n80 as i64)
    .bind(n90 as i64)
    .bind(n100 as i64)
    .bind(schedule_enabled as i64)
    .bind(schedule_times_json)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_last_quota_level(pool: &SqlitePool, name: &str, level: i64) -> Result<()> {
    sqlx::query("UPDATE users SET tg_last_quota_level = ? WHERE name = ?")
        .bind(level)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_last_schedule_dates(pool: &SqlitePool, name: &str, json: &str) -> Result<()> {
    sqlx::query("UPDATE users SET tg_last_schedule_dates = ? WHERE name = ?")
        .bind(json)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

// ─────────────────────────── 管理员偏好 ───────────────────────────

pub async fn ensure_admin_prefs(
    pool: &SqlitePool,
    chat_id: i64,
    notify_quota: bool,
    schedule_enabled: bool,
    schedule_times_json: &str,
) -> Result<()> {
    // INSERT OR IGNORE:已经存在的行保留管理员自己在 bot 里改过的设置,
    // 不该被配置文件里的默认值每次启动都冲掉。
    sqlx::query(
        "INSERT OR IGNORE INTO tg_admin_prefs
            (chat_id, notify_quota, schedule_enabled, schedule_times, last_schedule_dates)
         VALUES (?, ?, ?, ?, '{}')",
    )
    .bind(chat_id)
    .bind(notify_quota as i64)
    .bind(schedule_enabled as i64)
    .bind(schedule_times_json)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_admin_prefs(pool: &SqlitePool, chat_ids: &[i64]) -> Result<Vec<AdminPrefs>> {
    let mut out = Vec::with_capacity(chat_ids.len());
    for id in chat_ids {
        // 逐个查而不是 IN (...):chat_ids 来自配置,通常只有一两个,
        // 而拼 IN 列表要么动态拼 SQL、要么引 QueryBuilder,两者都不值当。
        let row: Option<(i64, bool, bool, String, String)> = sqlx::query_as(
            "SELECT chat_id, notify_quota, schedule_enabled, schedule_times, last_schedule_dates
               FROM tg_admin_prefs WHERE chat_id = ?",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?;
        if let Some(r) = row {
            out.push(AdminPrefs {
                chat_id: r.0,
                notify_quota: r.1,
                schedule_enabled: r.2,
                schedule_times_json: r.3,
                last_schedule_dates_json: r.4,
            });
        }
    }
    Ok(out)
}

pub async fn set_admin_prefs(
    pool: &SqlitePool,
    chat_id: i64,
    notify_quota: bool,
    schedule_enabled: bool,
    schedule_times_json: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE tg_admin_prefs SET notify_quota = ?, schedule_enabled = ?, schedule_times = ?
          WHERE chat_id = ?",
    )
    .bind(notify_quota as i64)
    .bind(schedule_enabled as i64)
    .bind(schedule_times_json)
    .bind(chat_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_admin_last_schedule_dates(pool: &SqlitePool, chat_id: i64, json: &str) -> Result<()> {
    sqlx::query("UPDATE tg_admin_prefs SET last_schedule_dates = ? WHERE chat_id = ?")
        .bind(json)
        .bind(chat_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ─────────────────────────── 单实例租约 ───────────────────────────

/// 抢租约。没人持有、或持有者心跳已过期时成功。
///
/// 用一条 `INSERT … ON CONFLICT … WHERE` 完成「读-判断-写」,而不是先 SELECT
/// 再 UPDATE:后者在两个进程同时启动时会双双通过判断,于是两边都跑 bot ——
/// 正是这张表要避免的情况。
pub async fn try_acquire_lease(pool: &SqlitePool, owner: &str, stale_secs: i64, now: i64) -> Result<bool> {
    let affected = sqlx::query(
        "INSERT INTO tg_bot_lease (id, owner, heartbeat) VALUES (1, ?, ?)
         ON CONFLICT(id) DO UPDATE SET owner = excluded.owner, heartbeat = excluded.heartbeat
          WHERE tg_bot_lease.owner = excluded.owner
             OR tg_bot_lease.heartbeat < ?",
    )
    .bind(owner)
    .bind(now)
    .bind(now - stale_secs)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// 续租。返回 false 表示租约已被别人接管 —— 调用方必须停下来。
pub async fn renew_lease(pool: &SqlitePool, owner: &str, now: i64) -> Result<bool> {
    let affected = sqlx::query("UPDATE tg_bot_lease SET heartbeat = ? WHERE id = 1 AND owner = ?")
        .bind(now)
        .bind(owner)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

pub async fn lease_holder(pool: &SqlitePool) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT owner FROM tg_bot_lease WHERE id = 1")
        .fetch_optional(pool)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-tg-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    async fn add_user(p: &SqlitePool, name: &str, quota_gb: f64) -> i64 {
        crate::db::node_repo::add_user(p, name, (quota_gb * 1_073_741_824.0) as i64, 0)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn new_users_start_unbound_with_defaults() {
        let p = pool().await;
        add_user(&p, "alice", 100.0).await;
        let u = get_user(&p, "alice").await.unwrap().unwrap();
        assert!(!u.is_bound());
        assert_eq!(u.last_quota_level, 0);
        assert!(u.notify_80 && u.notify_90 && u.notify_100, "三档默认全开");
        assert!(u.schedule_enabled);
        assert!(u.schedule_times().is_empty(), "空表示用配置里的默认时间");
    }

    #[tokio::test]
    async fn binding_consumes_the_code() {
        let p = pool().await;
        add_user(&p, "alice", 100.0).await;
        let code = regenerate_bind_token(&p, "alice").await.unwrap();

        assert_eq!(bind(&p, &code, 555).await.unwrap().as_deref(), Some("alice"));
        let u = get_user(&p, "alice").await.unwrap().unwrap();
        assert_eq!(u.chat_id, 555);
        // 用过的码不能再用一次 —— 否则截获过一次绑定消息的人可以随时接管。
        assert_eq!(bind(&p, &code, 666).await.unwrap(), None);
        assert_eq!(get_user(&p, "alice").await.unwrap().unwrap().chat_id, 555);
    }

    #[tokio::test]
    async fn wrong_or_empty_codes_bind_nothing() {
        let p = pool().await;
        add_user(&p, "alice", 100.0).await;
        regenerate_bind_token(&p, "alice").await.unwrap();
        assert_eq!(bind(&p, "not-a-code", 1).await.unwrap(), None);
        // 空串不能匹配到那些 tg_bind_token = '' 的用户。
        assert_eq!(bind(&p, "", 1).await.unwrap(), None);
        assert_eq!(bind(&p, "   ", 1).await.unwrap(), None);
    }

    /// 一个 chat 只能绑一个用户。绑新的会自动解绑旧的,
    /// 否则 user_by_chat 会在两行之间随机返回。
    #[tokio::test]
    async fn rebinding_a_chat_releases_the_previous_user() {
        let p = pool().await;
        add_user(&p, "alice", 100.0).await;
        add_user(&p, "bob", 100.0).await;
        let a = regenerate_bind_token(&p, "alice").await.unwrap();
        let b = regenerate_bind_token(&p, "bob").await.unwrap();

        bind(&p, &a, 777).await.unwrap();
        bind(&p, &b, 777).await.unwrap();

        assert_eq!(get_user(&p, "alice").await.unwrap().unwrap().chat_id, 0);
        assert_eq!(user_by_chat(&p, 777).await.unwrap().unwrap().name, "bob");
    }

    /// chat_id = 0 是「未绑定」的哨兵,不能匹配到一堆未绑定的用户。
    #[tokio::test]
    async fn chat_id_zero_matches_nobody() {
        let p = pool().await;
        add_user(&p, "alice", 100.0).await;
        assert!(user_by_chat(&p, 0).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn usage_applies_the_multiplier() {
        let p = pool().await;
        let uid = add_user(&p, "alice", 100.0).await;
        sqlx::query("UPDATE users SET traffic_multiplier = 2.0 WHERE id = ?")
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();
        let (agent_id, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let mut params = crate::model::node::NodeParams::default();
        crate::secrets::fill(crate::model::node::Protocol::VlessReality, &mut params).unwrap();
        let (node_id, _) = crate::db::node_repo::add_node(
            &p, agent_id, "n", crate::model::node::Protocol::VlessReality, 1, &params,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO user_traffic (user_id, node_id, cycle_up, cycle_down, updated_at)
             VALUES (?, ?, ?, ?, 0)",
        )
        .bind(uid)
        .bind(node_id)
        .bind(10 * 1_073_741_824i64)
        .bind(15 * 1_073_741_824i64)
        .execute(&p)
        .await
        .unwrap();

        let u = get_user(&p, "alice").await.unwrap().unwrap();
        assert_eq!(u.used(), 50 * 1_073_741_824, "(10+15) × 2");
        assert!((u.percent() - 50.0).abs() < 0.01);
        assert_eq!(u.remaining(), Some(50 * 1_073_741_824));
    }

    #[tokio::test]
    async fn unlimited_users_have_no_percentage() {
        let p = pool().await;
        add_user(&p, "alice", 0.0).await;
        let u = get_user(&p, "alice").await.unwrap().unwrap();
        assert_eq!(u.percent(), 0.0);
        assert_eq!(u.remaining(), None, "不限流量时「剩余」没有意义");
    }

    /// 手改坏的 JSON 不该让整轮播报炸掉。
    #[tokio::test]
    async fn corrupt_json_columns_degrade_to_empty() {
        let p = pool().await;
        add_user(&p, "alice", 100.0).await;
        sqlx::query("UPDATE users SET tg_schedule_times = '{oops', tg_last_schedule_dates = 'nope'")
            .execute(&p)
            .await
            .unwrap();
        let u = get_user(&p, "alice").await.unwrap().unwrap();
        assert!(u.schedule_times().is_empty());
        assert!(u.last_schedule_dates().is_empty());
    }

    // ─────────── 租约 ───────────

    #[tokio::test]
    async fn only_one_instance_gets_the_lease() {
        let p = pool().await;
        assert!(try_acquire_lease(&p, "a@daemon", 90, 1000).await.unwrap());
        assert!(!try_acquire_lease(&p, "b@tui", 90, 1000).await.unwrap(), "第二个必须抢不到");
        assert_eq!(lease_holder(&p).await.unwrap().as_deref(), Some("a@daemon"));
    }

    /// 持有者自己重复获取是幂等的(比如进程重启后同名再来一次)。
    #[tokio::test]
    async fn the_holder_can_reacquire_its_own_lease() {
        let p = pool().await;
        assert!(try_acquire_lease(&p, "a@daemon", 90, 1000).await.unwrap());
        assert!(try_acquire_lease(&p, "a@daemon", 90, 1010).await.unwrap());
    }

    /// 持有者被 kill 之后心跳停了,超时即可被接管 —— 否则一次崩溃会让
    /// bot 永久停摆,而且没有任何提示。
    #[tokio::test]
    async fn a_stale_lease_can_be_taken_over() {
        let p = pool().await;
        try_acquire_lease(&p, "dead@daemon", 90, 1000).await.unwrap();
        assert!(!try_acquire_lease(&p, "new@tui", 90, 1050).await.unwrap(), "还没过期");
        assert!(try_acquire_lease(&p, "new@tui", 90, 1200).await.unwrap(), "过期后可接管");
        assert_eq!(lease_holder(&p).await.unwrap().as_deref(), Some("new@tui"));
    }

    /// 续租失败 = 租约已被接管。调用方据此停下来,避免两个实例同时推消息。
    #[tokio::test]
    async fn renewing_a_stolen_lease_fails() {
        let p = pool().await;
        try_acquire_lease(&p, "a@daemon", 90, 1000).await.unwrap();
        assert!(renew_lease(&p, "a@daemon", 1025).await.unwrap());

        try_acquire_lease(&p, "b@tui", 90, 1200).await.unwrap();
        assert!(!renew_lease(&p, "a@daemon", 1225).await.unwrap(), "被抢走后不该续上");
    }
}
