//! 配额与到期的自动化巡检(DESIGN.md §6.3)。
//!
//! 这是**全系统触发最频繁的自动化路径**:默认每 30s 跑一次。
//! 它做三件事,顺序不能变:
//!
//!   1. **月重置** —— 到了重置日就清零 `cycle_*`,并把**因超额被自动禁用**的用户放出来。
//!   2. **配额/到期判定** —— 超额或过期的用户置 `enabled = 0`。
//!   3. **下发 `user.state`** —— 只在有变化时发,且只发给在线 agent。
//!
//! 顺序理由:先重置再判定,这样「重置后仍然该禁用」(例如已过期)的用户会在**同一轮**
//! 内被重新禁用,不会出现「放出来一整轮再关回去」的抖动。
//!
//! 关键语义(沿用旧项目 `auto_disabled`):
//! **只有系统自己禁用的用户才会被自动放出来。** 管理员手动停用的(`auto_disabled = 0`)
//! 任何自动流程都不碰——否则管理员封的人会在下个重置日自己复活。
//!
//! 这条路径**不重建 box**(§7.5):它只翻 agent 侧 tracker 的内存标记。
//! 这就是 §1.2「配置权威归主控」的直接收益——禁用一个跨 3 台服务器的用户
//! 是一次数据库写 + N 次轻量下发,且对离线节点天然收敛(重连握手时补齐)。

use crate::cluster::{Registry, Rpc};
use anyhow::Result;
use chrono::{Datelike, TimeZone};
use sbx_shared::method;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 一轮巡检做了什么。返回它而不是只记日志,是为了让测试能断言行为。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickSummary {
    /// 触发月重置的用户数。
    pub users_reset: usize,
    /// 触发月重置的 agent 网卡计数数。
    pub nics_reset: usize,
    /// 本轮被自动禁用的用户数。
    pub disabled: usize,
    /// 本轮被自动放出来的用户数(配额被调高、到期被延长、或月重置)。
    pub reenabled: usize,
}

impl TickSummary {
    fn changed(&self) -> bool {
        self.users_reset > 0 || self.disabled > 0 || self.reenabled > 0
    }
}

/// 跑一轮巡检。
///
/// `now` 由调用方传入(unix 秒)而不是在这里读时钟——否则「到期判定」和
/// 「重置日判定」这两块最需要测的逻辑都没法测。
pub async fn tick(
    pool: &SqlitePool,
    registry: &Arc<Mutex<Registry>>,
    rpc: &Arc<Rpc>,
    now: i64,
) -> Result<TickSummary> {
    let mut sum = TickSummary::default();

    // 时间戳转本地日期。转不出来(时间戳损坏)时跳过重置但仍做配额判定——
    // 宁可少做一件事,也不要 panic 掉整个巡检循环。
    if let Some(date) = local_date(now) {
        let ym = format!("{:04}-{:02}", date.year(), date.month());
        let last_day = last_day_of_month(date);
        sum.users_reset = reset_user_cycles(pool, date.day(), last_day, &ym, now).await?;
        sum.nics_reset = reset_nic_cycles(pool, date.day(), last_day, &ym, now).await?;
    } else {
        tracing::warn!(now, "无法解析时间戳,本轮跳过月重置");
    }

    let (disabled, reenabled) = evaluate_quotas(pool, now).await?;
    sum.disabled = disabled;
    sum.reenabled = reenabled;

    if sum.changed() {
        // 一轮里所有变化共用一次 revision 推进,这样各 agent 拿到的是同一个值。
        sqlx::query("UPDATE agents SET user_state_revision = user_state_revision + 1")
            .execute(pool)
            .await?;
        push_user_state(pool, registry, rpc).await;
    }

    Ok(sum)
}

fn local_date(ts: i64) -> Option<chrono::NaiveDate> {
    chrono::Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.date_naive())
}

/// 本月最后一天(28..=31)。
///
/// 用来把 `reset_day` 夹进本月的有效范围:`reset_day = 31` 在 2 月要当成 28
/// (闰年 29),否则该用户整个 2 月都等不到重置。
fn last_day_of_month(date: chrono::NaiveDate) -> u32 {
    (28..=31)
        .rev()
        .find(|d| chrono::NaiveDate::from_ymd_opt(date.year(), date.month(), *d).is_some())
        .unwrap_or(28)
}

/// 月重置:清零 `cycle_*`,保留 `total_*`。
///
/// 判定条件是 `今天 >= MIN(reset_day, 本月最后一天)` 且 `last_reset_ym != 本月`。
///
/// **两处都必要,少一个就出 bug:**
///   * `MIN(reset_day, 本月最后一天)` 处理**短月**——`reset_day = 31` 在 2 月
///     要当成 28(闰年 29),否则该用户整个 2 月都等不到重置。
///   * `>=` 而不是 `==` 处理**漏拍**——主控在重置日当天停机,第二天起来仍能补上。
///
/// 顺带把因超额被自动禁用的用户放出来——但**过期的不放**(放了下一步又会关回去)。
async fn reset_user_cycles(
    pool: &SqlitePool,
    day: u32,
    last_day: u32,
    ym: &str,
    now: i64,
) -> Result<usize> {
    let candidates: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, name FROM users
          WHERE reset_day IS NOT NULL AND reset_day > 0
            AND ? >= MIN(reset_day, ?)
            AND last_reset_ym != ?",
    )
    .bind(day as i64)
    .bind(last_day as i64)
    .bind(ym)
    .fetch_all(pool)
    .await?;

    if candidates.is_empty() {
        return Ok(0);
    }

    let mut tx = pool.begin().await?;
    for (user_id, name) in &candidates {
        // cycle_* 清零,total_* 不动(它是终身累计)。
        // last_up / last_down **也不能动**:它们是 delta 计算的基线(§5.2),
        // 清零会让下一次上报被当成新纪元,把一整个周期的量重复计一次。
        sqlx::query("UPDATE user_traffic SET cycle_up = 0, cycle_down = 0 WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;

        // 只放系统自己关的(auto_disabled = 1),且没过期的。
        sqlx::query(
            "UPDATE users SET
               last_reset_ym = ?,
               enabled = CASE
                 WHEN auto_disabled = 1 AND (expire_at IS NULL OR expire_at > ?) THEN 1
                 ELSE enabled END,
               auto_disabled = CASE
                 WHEN auto_disabled = 1 AND (expire_at IS NULL OR expire_at > ?) THEN 0
                 ELSE auto_disabled END
             WHERE id = ?",
        )
        .bind(ym)
        .bind(now)
        .bind(now)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query("INSERT INTO agent_events (agent_id, kind, message, at) VALUES (NULL, ?, ?, ?)")
            .bind("user_cycle_reset")
            .bind(format!("用户 {name} 的流量周期已重置({ym})"))
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    Ok(candidates.len())
}

/// 网卡流量的月重置(§6.4)。与用户流量走完全独立的一套。
async fn reset_nic_cycles(
    pool: &SqlitePool,
    day: u32,
    last_day: u32,
    ym: &str,
    now: i64,
) -> Result<usize> {
    let candidates: Vec<i64> = sqlx::query_scalar(
        "SELECT a.id FROM agents a
           JOIN agent_nic_traffic t ON t.agent_id = a.id
          WHERE a.nic_reset_day IS NOT NULL AND a.nic_reset_day > 0
            AND ? >= MIN(a.nic_reset_day, ?)
            AND t.last_reset_ym != ?",
    )
    .bind(day as i64)
    .bind(last_day as i64)
    .bind(ym)
    .fetch_all(pool)
    .await?;

    if candidates.is_empty() {
        return Ok(0);
    }

    let mut tx = pool.begin().await?;
    for agent_id in &candidates {
        // 同上:last_rx / last_tx 是 delta 基线,不能清。
        sqlx::query(
            "UPDATE agent_nic_traffic
                SET cycle_rx = 0, cycle_tx = 0, cycle_start = ?, last_reset_ym = ?
              WHERE agent_id = ?",
        )
        .bind(now)
        .bind(ym)
        .bind(agent_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(candidates.len())
}

/// `evaluate_quotas` 的一行:
/// `(user_id, name, quota_bytes, traffic_multiplier, expire_at, enabled, auto_disabled, cycle_up, cycle_down)`。
///
/// 起别名不只是为了让 clippy 闭嘴 —— 九元组按位置解构时,
/// `enabled` 和 `auto_disabled` 这两个相邻的 bool 一旦写反,巡检会把管理员手动停用的
/// 用户自动放行,而且不报错。别名让签名里至少能看到一次字段顺序。
type QuotaRow = (i64, String, i64, f64, Option<i64>, bool, bool, i64, i64);

/// 配额与到期判定。返回 `(禁用数, 放行数)`。
///
/// 放行的条件是「`auto_disabled = 1` 且现在已经不该禁用了」——覆盖两种情况:
/// 管理员把配额调高了、或者把到期时间延后了。两者都该立刻生效,不必等下个重置日。
async fn evaluate_quotas(pool: &SqlitePool, now: i64) -> Result<(usize, usize)> {
    // 用量取自跨 agent 求和的视图(§6.2)。LEFT JOIN 保证没有流量记录的用户也在结果里。
    let rows: Vec<QuotaRow> = sqlx::query_as(
        "SELECT u.id, u.name, u.quota_bytes, u.traffic_multiplier, u.expire_at,
                u.enabled, u.auto_disabled,
                COALESCE(t.cycle_up, 0), COALESCE(t.cycle_down, 0)
           FROM users u
           LEFT JOIN user_traffic_total t ON t.user_id = u.id",
    )
    .fetch_all(pool)
    .await?;

    let mut to_disable: Vec<(i64, String, &'static str)> = Vec::new();
    let mut to_enable: Vec<(i64, String)> = Vec::new();

    for (id, name, quota, mult, expire_at, enabled, auto_disabled, cyc_up, cyc_down) in rows {
        // 判定规则本体在 model::user —— 这里不重写,否则边界条件会与 TUI 侧漂开。
        let expired = crate::model::user::expired(expire_at, now);
        let used = ((cyc_up + cyc_down) as f64 * mult) as i64;
        let over = crate::model::user::over_quota(quota, used);

        if (over || expired) && enabled {
            to_disable.push((id, name, if expired { "已到期" } else { "配额用尽" }));
        } else if !over && !expired && !enabled && auto_disabled {
            // 系统关的,现在条件不成立了 → 放行。管理员手动关的(auto_disabled=0)不碰。
            to_enable.push((id, name));
        }
    }

    if to_disable.is_empty() && to_enable.is_empty() {
        return Ok((0, 0));
    }

    let mut tx = pool.begin().await?;
    for (id, name, why) in &to_disable {
        // auto_disabled = 1 标记「这是系统关的」,月重置时才敢放。
        sqlx::query("UPDATE users SET enabled = 0, auto_disabled = 1 WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO agent_events (agent_id, kind, message, at) VALUES (NULL, ?, ?, ?)")
            .bind("user_auto_disabled")
            .bind(format!("用户 {name} 已自动停用:{why}"))
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    for (id, name) in &to_enable {
        sqlx::query("UPDATE users SET enabled = 1, auto_disabled = 0 WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO agent_events (agent_id, kind, message, at) VALUES (NULL, ?, ?, ?)")
            .bind("user_auto_enabled")
            .bind(format!("用户 {name} 已自动恢复"))
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    Ok((to_disable.len(), to_enable.len()))
}

/// 向所有在线 agent 下发全量禁用名单。
///
/// 离线的不管——它们重连握手时会按 revision 自动补齐(§4.1)。
/// 这就是为什么这里失败也不需要重试:下次握手是天然的重试点。
async fn push_user_state(pool: &SqlitePool, registry: &Arc<Mutex<Registry>>, rpc: &Arc<Rpc>) {
    let disabled = match crate::service::disabled_users(pool).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "查询禁用名单失败,本轮不下发");
            return;
        }
    };

    let online = registry.lock().await.online_ids();
    for agent_id in online {
        let rev: i64 = match sqlx::query_scalar("SELECT user_state_revision FROM agents WHERE id = ?")
            .bind(agent_id)
            .fetch_one(pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(agent_id, error = %e, "读取 revision 失败");
                continue;
            }
        };

        let payload = serde_json::json!({
            "user_state_revision": rev,
            "disabled": disabled,
        });
        match rpc.call_default(registry, agent_id, method::USER_STATE, payload).await {
            Ok(_) => tracing::debug!(agent_id, rev, count = disabled.len(), "user.state 已下发"),
            Err(e) => tracing::warn!(agent_id, error = %e, "user.state 下发失败(重连时会补齐)"),
        }
    }
}

/// 启动巡检循环。
///
/// 首轮**立刻**跑一次而不是等一个周期:主控刚起来时库里的 enabled 状态
/// 可能已经落后了(比如停机期间有人到期),先对齐再进入常规节奏。
///
/// `tg` 是可选的 Telegram 事件通道。每轮把各用户的用量百分比丢过去,
/// **去重(哪一档已经通知过)是 bot 那边的事** —— 巡检只负责报现状,
/// 让它同时管「判定」和「通知节流」会把两件独立的策略缠在一起。
/// 取走 TUI 排进来的一次性指令并下发(目前只有 agent 升级)。
///
/// 放在巡检循环里而不是另起一个任务:它要的东西(registry、rpc、pool)
/// 这里全都现成,而且这类指令本来就不追求秒级延迟 —— 排进去之后
/// 下一拍(默认 30 秒)发出去即可。
///
/// **一条失败不影响其余。** 十台里有一台取不到产物,另外九台照升;
/// 失败原因写回那一行,界面上能看到「为什么这台还是旧版本」。
async fn drain_commands(
    pool: &SqlitePool,
    registry: &std::sync::Arc<tokio::sync::Mutex<crate::cluster::Registry>>,
    rpc: &crate::cluster::Rpc,
    now: i64,
) -> Result<usize> {
    let pending = crate::db::command_repo::take_pending(pool, now).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let mut done = 0usize;
    for cmd in pending {
        let method = match cmd.kind.as_str() {
            "upgrade" => sbx_shared::method::AGENT_UPGRADE,
            other => {
                // 不认识的指令要**记下来**再丢掉。静默丢弃会让人对着一个
                // 永远「待办」的队列发懵。
                let msg = format!("不认识的指令类型:{other}");
                tracing::warn!(id = cmd.id, kind = other, "丢弃不认识的指令");
                let _ =
                    crate::db::command_repo::finish(pool, cmd.id, Some(&msg), now).await;
                continue;
            }
        };
        let payload: serde_json::Value =
            serde_json::from_str(&cmd.payload_json).unwrap_or(serde_json::Value::Null);
        let result = rpc.call_default(registry, cmd.agent_id, method, payload).await;
        let err = match &result {
            Ok(_) => {
                done += 1;
                tracing::info!(agent_id = cmd.agent_id, kind = %cmd.kind, "指令已下发");
                None
            }
            Err(e) => {
                tracing::warn!(agent_id = cmd.agent_id, kind = %cmd.kind, error = %e, "指令下发失败");
                Some(e.to_string())
            }
        };
        crate::db::command_repo::finish(pool, cmd.id, err.as_deref(), now).await?;
    }
    Ok(done)
}

pub fn spawn(
    state: crate::cluster::ServerState,
    interval_secs: u64,
    tg: Option<tokio::sync::mpsc::Sender<crate::tg::Event>>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
        // 默认的 Burst 行为会在卡顿后连补几拍,对巡检没意义(它是幂等的,补拍只是浪费)。
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let now = chrono::Local::now().timestamp();
            match tick(&state.pool, &state.registry, &state.rpc, now).await {
                Ok(s) if s.changed() => tracing::info!(
                    users_reset = s.users_reset,
                    nics_reset = s.nics_reset,
                    disabled = s.disabled,
                    reenabled = s.reenabled,
                    "配额巡检有变更"
                ),
                Ok(_) => tracing::trace!("配额巡检无变更"),
                // 单轮失败不该终止循环:下一轮 30s 后照常跑。
                Err(e) => tracing::error!(error = %e, "配额巡检失败"),
            }
            // TUI 排进来的一次性指令(升级)。与配额巡检分开做:
            // 它失败了不该把巡检那一半也带下水。
            match drain_commands(&state.pool, &state.registry, &state.rpc, now).await {
                Ok(n) if n > 0 => tracing::info!(count = n, "已下发排队的指令"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "取排队指令失败"),
            }

            if let Some(tx) = &tg {
                if let Err(e) = report_usage(&state.pool, tx).await {
                    tracing::warn!(error = %e, "推送用量给 Telegram 失败");
                }
            }
        }
    });
}

/// 把当前各用户的用量百分比发给 bot。
///
/// 用 `try_send` 而不是 `send`:通道满了说明 bot 那边卡住了(网络烂、
/// Telegram 限流),这时候**丢掉这一轮**是对的 —— 巡检 30 秒后还会再报一次,
/// 而阻塞在这里会把整个巡检循环拖停,连带影响配额判定这件更要紧的事。
async fn report_usage(
    pool: &SqlitePool,
    tx: &tokio::sync::mpsc::Sender<crate::tg::Event>,
) -> Result<()> {
    let rows: Vec<(String, i64, f64, i64, i64)> = sqlx::query_as(
        "SELECT u.name, u.quota_bytes, u.traffic_multiplier,
                COALESCE(t.cycle_up, 0), COALESCE(t.cycle_down, 0)
           FROM users u LEFT JOIN user_traffic_total t ON t.user_id = u.id
          WHERE u.quota_bytes > 0",
    )
    .fetch_all(pool)
    .await?;

    for (name, quota, mult, up, down) in rows {
        let used = ((up + down).max(0) as f64 * mult.max(0.0)) as i64;
        let percent = used as f64 / quota as f64 * 100.0;
        if tx
            .try_send(crate::tg::Event::QuotaAlert { username: name, percent })
            .is_err()
        {
            tracing::debug!("Telegram 事件通道已满,本轮用量推送丢弃");
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: i64 = 1_073_741_824;

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-sup-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    fn empty_state() -> (Arc<Mutex<Registry>>, Arc<Rpc>) {
        (Arc::new(Mutex::new(Registry::new())), Arc::new(Rpc::new()))
    }

    /// 2026-03-15 12:00 本地时间的时间戳。
    fn ts(y: i32, m: u32, d: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(y, m, d, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp()
    }

    /// 造一个用户,可选地给它一个节点和一份用量。
    async fn user_with_usage(
        p: &SqlitePool,
        name: &str,
        quota_bytes: i64,
        cycle_up: i64,
        cycle_down: i64,
    ) -> i64 {
        let (agent_id, _) = match crate::db::agent_repo::list(p).await.unwrap().first() {
            Some(a) => (a.id, String::new()),
            None => crate::db::agent_repo::create(p, "a", 0).await.unwrap(),
        };
        let node_id: i64 = sqlx::query_scalar(
            "INSERT INTO nodes (agent_id, tag, protocol, listen_port, params_json)
             VALUES (?, ?, 'vless-reality', 443, '{}') RETURNING id",
        )
        .bind(agent_id)
        .bind(format!("in-{name}"))
        .fetch_one(p)
        .await
        .unwrap();

        let uid: i64 = sqlx::query_scalar(
            "INSERT INTO users (name, uuid, password, quota_bytes, sub_token, created_at)
             VALUES (?, ?, 'pw', ?, ?, 0) RETURNING id",
        )
        .bind(name)
        .bind(format!("uuid-{name}"))
        .bind(quota_bytes)
        .bind(format!("tok-{name}"))
        .fetch_one(p)
        .await
        .unwrap();

        sqlx::query("INSERT INTO user_nodes (user_id, node_id) VALUES (?, ?)")
            .bind(uid)
            .bind(node_id)
            .execute(p)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO user_traffic (user_id, node_id, cycle_up, cycle_down,
                                       total_up, total_down, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 0)",
        )
        .bind(uid)
        .bind(node_id)
        .bind(cycle_up)
        .bind(cycle_down)
        .bind(cycle_up)
        .bind(cycle_down)
        .execute(p)
        .await
        .unwrap();
        uid
    }

    async fn state_of(p: &SqlitePool, name: &str) -> (bool, bool) {
        sqlx::query_as("SELECT enabled, auto_disabled FROM users WHERE name = ?")
            .bind(name)
            .fetch_one(p)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn over_quota_user_is_disabled_and_marked_auto() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        user_with_usage(&p, "alice", 10 * GB, 6 * GB, 5 * GB).await; // 11 GB > 10 GB

        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.disabled, 1);
        assert_eq!(state_of(&p, "alice").await, (false, true), "应被关且标记为系统关的");
    }

    #[tokio::test]
    async fn under_quota_user_is_untouched() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        user_with_usage(&p, "alice", 10 * GB, GB, GB).await;

        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s, TickSummary::default(), "没有变化时不该动任何东西");
        assert_eq!(state_of(&p, "alice").await, (true, false));
    }

    #[tokio::test]
    async fn unlimited_quota_is_never_over() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        user_with_usage(&p, "alice", 0, 900 * GB, 900 * GB).await;

        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.disabled, 0, "quota_bytes = 0 表示不限");
    }

    #[tokio::test]
    async fn traffic_multiplier_is_applied_to_the_quota_check() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let uid = user_with_usage(&p, "alice", 10 * GB, 3 * GB, 2 * GB).await; // 原始 5 GB
        sqlx::query("UPDATE users SET traffic_multiplier = 2.0 WHERE id = ?")
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();

        // 5 GB × 2.0 = 10 GB,刚好打满
        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.disabled, 1, "倍率后达到配额应算超额");
    }

    #[tokio::test]
    async fn expired_user_is_disabled() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let uid = user_with_usage(&p, "alice", 0, 0, 0).await;
        let expire = ts(2026, 3, 10);
        sqlx::query("UPDATE users SET expire_at = ? WHERE id = ?")
            .bind(expire)
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();

        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.disabled, 1, "已过期应被关");

        // 审计记录应说明原因是到期而不是配额
        let msg: String = sqlx::query_scalar(
            "SELECT message FROM agent_events WHERE kind = 'user_auto_disabled'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert!(msg.contains("已到期"), "得到: {msg}");
    }

    /// 提高配额后应**立刻**放行,不必等下个重置日。
    #[tokio::test]
    async fn raising_the_quota_reenables_an_auto_disabled_user() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let uid = user_with_usage(&p, "alice", 10 * GB, 6 * GB, 5 * GB).await;

        tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(state_of(&p, "alice").await, (false, true));

        sqlx::query("UPDATE users SET quota_bytes = ? WHERE id = ?")
            .bind(100 * GB)
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();

        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.reenabled, 1);
        assert_eq!(state_of(&p, "alice").await, (true, false));
    }

    /// **管理员手动停用的用户,自动流程永不放行。**
    /// 这是 `auto_disabled` 这个字段存在的全部理由。
    #[tokio::test]
    async fn manually_disabled_user_is_never_reenabled() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        user_with_usage(&p, "alice", 0, 0, 0).await; // 不限量、不过期
        crate::db::node_repo::set_user_enabled(&p, "alice", false).await.unwrap();
        assert_eq!(state_of(&p, "alice").await, (false, false), "手动停用不标 auto");

        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.reenabled, 0, "手动停用的不该被自动放行");
        assert_eq!(state_of(&p, "alice").await, (false, false));
    }

    /// 月重置清零 cycle_*、保留 total_*,并放出因超额被关的用户。
    #[tokio::test]
    async fn monthly_reset_clears_cycle_keeps_total_and_reenables() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let uid = user_with_usage(&p, "alice", 10 * GB, 6 * GB, 5 * GB).await;
        sqlx::query("UPDATE users SET reset_day = 15 WHERE id = ?")
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();

        // 14 号:还没到重置日,先被关掉
        let s = tick(&p, &reg, &rpc, ts(2026, 3, 14)).await.unwrap();
        assert_eq!((s.users_reset, s.disabled), (0, 1));

        // 15 号:重置 + 放行
        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.users_reset, 1);
        assert_eq!(state_of(&p, "alice").await, (true, false), "应被放出来");

        let (cu, cd, tu, td): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT cycle_up, cycle_down, total_up, total_down FROM user_traffic WHERE user_id = ?",
        )
        .bind(uid)
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!((cu, cd), (0, 0), "cycle 应清零");
        assert_eq!((tu, td), (6 * GB, 5 * GB), "total 是终身累计,不该清");
    }

    /// 同一个月内只重置一次——否则每 30s 的巡检会反复清零。
    #[tokio::test]
    async fn reset_happens_once_per_month() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let uid = user_with_usage(&p, "alice", 0, GB, GB).await;
        sqlx::query("UPDATE users SET reset_day = 1 WHERE id = ?")
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();

        assert_eq!(tick(&p, &reg, &rpc, ts(2026, 3, 5)).await.unwrap().users_reset, 1);
        assert_eq!(
            tick(&p, &reg, &rpc, ts(2026, 3, 20)).await.unwrap().users_reset,
            0,
            "同月第二次不该再重置"
        );
        // 下个月应当再次重置
        assert_eq!(tick(&p, &reg, &rpc, ts(2026, 4, 2)).await.unwrap().users_reset, 1);
    }

    /// 短月:`reset_day = 31` 在 2 月也必须触发(用 `>=` 而不是 `==`),
    /// 否则 2 月整月不重置。
    #[tokio::test]
    async fn reset_day_31_still_fires_in_february() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let uid = user_with_usage(&p, "alice", 0, GB, 0).await;
        sqlx::query("UPDATE users SET reset_day = 31 WHERE id = ?")
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();

        // 2026 年 2 月只有 28 天
        assert_eq!(
            tick(&p, &reg, &rpc, ts(2026, 2, 28)).await.unwrap().users_reset,
            1,
            "短月的最后一天应补上重置"
        );
    }

    /// 过期的用户即使遇到重置日也不该被放出来——
    /// 否则会出现「放出来又立刻关回去」的抖动,审计日志里全是噪声。
    #[tokio::test]
    async fn reset_does_not_reenable_an_expired_user() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let uid = user_with_usage(&p, "alice", 10 * GB, 6 * GB, 5 * GB).await;
        sqlx::query("UPDATE users SET reset_day = 15, expire_at = ? WHERE id = ?")
            .bind(ts(2026, 3, 1))
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();

        tick(&p, &reg, &rpc, ts(2026, 3, 14)).await.unwrap(); // 先被关
        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.users_reset, 1, "重置仍然发生(cycle 要清)");
        assert_eq!(state_of(&p, "alice").await, (false, true), "过期的不该放出来");
        assert_eq!(s.reenabled, 0);
    }

    /// 有变化时推进 revision,没变化时**不推进**——
    /// 否则每 30s 一次的空巡检会让 agent 反复收到无意义的 user.state。
    #[tokio::test]
    async fn revision_advances_only_when_something_changed() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let (agent_id, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        user_with_usage(&p, "alice", 10 * GB, 6 * GB, 5 * GB).await;

        let rev = |p: SqlitePool| async move {
            sqlx::query_scalar::<_, i64>("SELECT user_state_revision FROM agents WHERE id = ?")
                .bind(agent_id)
                .fetch_one(&p)
                .await
                .unwrap()
        };

        let before = rev(p.clone()).await;
        tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap(); // 关掉 alice
        let after_change = rev(p.clone()).await;
        assert_eq!(after_change, before + 1, "有变化应推进一次");

        tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap(); // 无变化
        assert_eq!(rev(p.clone()).await, after_change, "无变化不该推进");
    }

    /// 网卡流量的重置与用户流量互不相干(§6.4)。
    #[tokio::test]
    async fn nic_reset_zeroes_cycle_and_moves_cycle_start() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let (agent_id, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        sqlx::query("UPDATE agents SET nic_reset_day = 10 WHERE id = ?")
            .bind(agent_id)
            .execute(&p)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO agent_nic_traffic
               (agent_id, boot_id, last_rx, last_tx, cycle_rx, cycle_tx, cycle_start, updated_at)
             VALUES (?, 'b', 5000, 6000, 500, 600, 1, 1)",
        )
        .bind(agent_id)
        .execute(&p)
        .await
        .unwrap();

        let when = ts(2026, 3, 12);
        let s = tick(&p, &reg, &rpc, when).await.unwrap();
        assert_eq!(s.nics_reset, 1);

        let (crx, ctx, start, lrx): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT cycle_rx, cycle_tx, cycle_start, last_rx FROM agent_nic_traffic WHERE agent_id = ?",
        )
        .bind(agent_id)
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!((crx, ctx), (0, 0), "cycle 应清零");
        assert_eq!(start, when, "cycle_start 应更新到重置时刻");
        assert_eq!(lrx, 5000, "last_rx 是 delta 基线(§5.2),绝不能清零");
    }

    /// 没有 reset_day 的用户永不重置(NULL = 无需重置)。
    #[tokio::test]
    async fn null_reset_day_never_resets() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        user_with_usage(&p, "alice", 0, GB, GB).await; // reset_day 默认 NULL

        for day in [1, 15, 28] {
            assert_eq!(
                tick(&p, &reg, &rpc, ts(2026, 3, day)).await.unwrap().users_reset,
                0
            );
        }
    }

    /// 用量为「跨 agent 求和」时也要能正确触发配额(§0.0-3 与 §6.3 的交汇点)。
    #[tokio::test]
    async fn quota_triggers_on_cross_agent_sum() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        // 两台 agent,各一个节点,单看都没超,合起来超了
        let (a1, _) = crate::db::agent_repo::create(&p, "a1", 0).await.unwrap();
        let (a2, _) = crate::db::agent_repo::create(&p, "a2", 0).await.unwrap();
        let uid: i64 = sqlx::query_scalar(
            "INSERT INTO users (name, uuid, password, quota_bytes, sub_token, created_at)
             VALUES ('alice', 'u', 'p', ?, 't', 0) RETURNING id",
        )
        .bind(10 * GB)
        .fetch_one(&p)
        .await
        .unwrap();

        for (i, agent) in [a1, a2].iter().enumerate() {
            let nid: i64 = sqlx::query_scalar(
                "INSERT INTO nodes (agent_id, tag, protocol, listen_port, params_json)
                 VALUES (?, ?, 'vless-reality', 443, '{}') RETURNING id",
            )
            .bind(agent)
            .bind(format!("in-{i}"))
            .fetch_one(&p)
            .await
            .unwrap();
            sqlx::query("INSERT INTO user_nodes (user_id, node_id) VALUES (?, ?)")
                .bind(uid)
                .bind(nid)
                .execute(&p)
                .await
                .unwrap();
            // 每个节点 6 GB → 合计 12 GB > 10 GB
            sqlx::query(
                "INSERT INTO user_traffic (user_id, node_id, cycle_up, cycle_down,
                                           total_up, total_down, updated_at)
                 VALUES (?, ?, ?, 0, ?, 0, 0)",
            )
            .bind(uid)
            .bind(nid)
            .bind(6 * GB)
            .bind(6 * GB)
            .execute(&p)
            .await
            .unwrap();
        }

        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.disabled, 1, "配额判定必须基于跨 agent 求和");
    }
}
