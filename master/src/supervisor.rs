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
    //
    // **用户周期用主控时区,网卡周期按每台 agent 自己的时区。** 用户不绑定某台机器,
    // 没有「他那边的月份」这回事;而网卡配额要对齐的是各家 VPS 厂商的账单边界,
    // 而厂商按各自机房的本地日界翻月(§6.4)。所以两条路径从这里就分叉。
    if let Some(date) = local_date(now) {
        let ym = format!("{:04}-{:02}", date.year(), date.month());
        let last_day = last_day_of_month(date);
        sum.users_reset = reset_user_cycles(pool, date.day(), last_day, &ym, now).await?;
    } else {
        tracing::warn!(now, "无法解析时间戳,本轮跳过用户周期重置");
    }
    sum.nics_reset = reset_nic_cycles(pool, now).await?;

    let (disabled, reenabled) = evaluate_quotas(pool, now).await?;
    sum.disabled = disabled;
    sum.reenabled = reenabled;

    if sum.changed() {
        // 一轮里所有变化共用一次 revision 推进,这样各 agent 拿到的是同一个值。
        sqlx::query("UPDATE agents SET user_state_revision = user_state_revision + 1")
            .execute(pool)
            .await?;
    }

    // 下发**无条件跑**,不看 `sum.changed()`。那个标志只反映本轮配额巡检的结果,
    // 而 revision 也会被 TUI/CLI 的手动启停推进(node_repo::set_user_enabled) ——
    // 由 `changed()` 把门时,手动停用一个没超额的用户压根发不出去。
    // 真正的「要不要发」由 push_* 里的 revision 比较回答。
    push_user_state(pool, registry, rpc).await;

    // 配置推送。**在 `tick` 里面而不是调用它的循环里** —— 循环没有测试覆盖,
    // 把接线放在那儿的话,「函数写好了但没接上」这种错没人拦得住,
    // 而这恰恰就是它当初坏掉的形状(config.apply 只有握手一条路径)。
    //
    // 与上面的配额巡检无关:建节点、改出站策略、调整分配都只动 `config_revision`,
    // `sum.changed()` 是 false 也照样要推。
    push_config(pool, registry, rpc).await;

    Ok(sum)
}

/// 时间戳在某个 UTC 偏移下落在哪一天。
///
/// 网卡重置要按每台 agent 自己的偏移算,所以日期计算必须能换时区 ——
/// `local_date` 只是它在主控偏移上的特例。写法与 `tui/forms.rs::today_day` 一致。
fn date_in(ts: i64, off: chrono::FixedOffset) -> Option<chrono::NaiveDate> {
    chrono::DateTime::from_timestamp(ts, 0).map(|dt| dt.with_timezone(&off).date_naive())
}

fn local_date(ts: i64) -> Option<chrono::NaiveDate> {
    chrono::Local.timestamp_opt(ts, 0).single().map(|dt| dt.date_naive())
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

        sqlx::query(
            "INSERT INTO agent_events (agent_id, kind, message, at) VALUES (NULL, ?, ?, ?)",
        )
        .bind("user_cycle_reset")
        .bind(format!("用户 {name} 的流量周期已重置({ym})"))
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(candidates.len())
}

/// 网卡流量的月重置(§6.4)。与用户流量走完全独立的一套,而且**边界按每台 agent
/// 自己的时区算** —— 各家 VPS 厂商按自己机房的本地日界翻月。
///
/// 日期判定从 SQL 搬到了 Rust。三个量都必须用**这台自己的**偏移算,少一个就是 bug:
///
///   1. `date` —— 今天几号,决定够不够到重置日;
///   2. `ym` —— 一月只清一次的闸门(`last_reset_ym`)。跨月的那几个小时里,agent
///      本地的月份可能和主控不是同一个月;用主控的 `ym` 会把闸门开在错误的月份上,
///      表现是「这个月清了两次」或者「整个月没清」,而且要等一个月才复现;
///   3. `last_day_of_month` —— 短月夹取的上界,同样取决于 agent 本地落在哪个月。
///
/// 两条原有性质保持不变:`>=` 而不是 `==`(主控在重置日当天停机,第二天起来能补上),
/// `last_reset_ym != ym`(一个月只清一次)。
async fn reset_nic_cycles(pool: &SqlitePool, now: i64) -> Result<usize> {
    /// `(id, nic_reset_day, nic_reset_offset_secs, reported_utc_offset_secs, last_reset_ym)`
    type Row = (i64, i64, Option<i64>, Option<i64>, String);

    // SQL 只做「设过重置日吗」这一层粗筛,日期比较交给 Rust —— 每台的时区不同,
    // 一条 SQL 里没法用一个标量 day 同时代表所有机器。
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT a.id, a.nic_reset_day, a.nic_reset_offset_secs,
                a.reported_utc_offset_secs, t.last_reset_ym
           FROM agents a
           JOIN agent_nic_traffic t ON t.agent_id = a.id
          WHERE a.nic_reset_day IS NOT NULL AND a.nic_reset_day > 0",
    )
    .fetch_all(pool)
    .await?;

    let mut due: Vec<(i64, String)> = Vec::new();
    for (agent_id, reset_day, manual, reported, last_reset_ym) in rows {
        let (off, _) = crate::model::agent::reset_offset(manual, reported, now);
        let Some(date) = date_in(now, off) else {
            tracing::warn!(agent_id, now, "无法解析时间戳,本轮跳过这台的网卡重置");
            continue;
        };
        let ym = format!("{:04}-{:02}", date.year(), date.month());
        if last_reset_ym == ym {
            continue; // 这个月已经清过
        }
        let clamped = reset_day.min(last_day_of_month(date) as i64);
        if (date.day() as i64) < clamped {
            continue; // 本地还没到重置日
        }
        due.push((agent_id, ym));
    }

    if due.is_empty() {
        return Ok(0);
    }

    let mut tx = pool.begin().await?;
    for (agent_id, ym) in &due {
        // 同上:last_rx / last_tx 是 delta 基线,不能清。
        // 绑定的 `ym` 是**这台自己**算出来的,不是全局值。
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

    Ok(due.len())
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
        sqlx::query(
            "INSERT INTO agent_events (agent_id, kind, message, at) VALUES (NULL, ?, ?, ?)",
        )
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
        sqlx::query(
            "INSERT INTO agent_events (agent_id, kind, message, at) VALUES (NULL, ?, ?, ?)",
        )
        .bind("user_auto_enabled")
        .bind(format!("用户 {name} 已自动恢复"))
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok((to_disable.len(), to_enable.len()))
}

/// 把配置推给**库里比它新**的那些在线 agent。
///
/// ## 这个函数是补上的一条断链
///
/// 早先 `config.apply` 只有一条发送路径:握手时的 `catch_up`。于是主控这边
/// 建节点、改出站策略、调整分配之后,`config_revision` 在库里加了一,
/// 而那条**已经连着**的 WebSocket 收不到任何东西 —— 界面上写着
/// 「在线的机器会重建 box」,实际要等到 agent 重启重连才生效。
/// 现场表现就是「每次改完都得去被控机 systemctl restart」。
///
/// `user.state` 没有这个毛病,因为 `push_user_state` 每轮无条件推;
/// 配置这一路缺的正是这个对应物。
///
/// ## 为什么要记「已下发到哪一版」
///
/// 不能每轮无脑下发:`config.apply` 会让 agent **重建整个 box**,
/// 连接会断一瞬。每 30 秒重建一次,等于把服务打成筛子。
/// 所以按连接记住已下发的 revision(`Registry::sent_config_rev`),
/// 只有库里更新时才发。
///
/// 记在**连接**上而不是库里:断线重连之后一切从握手的 catch_up 重新对齐,
/// 这个值跟着连接一起消失才是对的。
///
/// 失败不重试也不回滚已记的值 —— 下一轮巡检自然重来,而握手是天然的兜底。
async fn push_config(pool: &SqlitePool, registry: &Arc<Mutex<Registry>>, rpc: &Arc<Rpc>) {
    let online = registry.lock().await.online_ids();
    for agent_id in online {
        let master_rev: i64 =
            match sqlx::query_scalar("SELECT config_revision FROM agents WHERE id = ?")
                .bind(agent_id)
                .fetch_one(pool)
                .await
            {
                Ok(r) => r,
                // 查不到通常是这台刚被删掉(连接还没断)。跳过就好,
                // 上报路径上的存在性检查会把那条连接收掉。
                Err(e) => {
                    tracing::debug!(agent_id, error = %e, "读取 config_revision 失败,本轮跳过");
                    continue;
                }
            };

        // `None` = 握手的 catch_up 还没跑完(它是并行 spawn 的)。
        // 这时候不抢着发:catch_up 正要处理这件事,两边都发就是重复下发。
        let Some(sent) = registry.lock().await.sent_config_rev(agent_id) else {
            continue;
        };
        if sent >= master_rev {
            continue;
        }

        let options = match crate::service::build_agent_config(pool, agent_id).await {
            Ok(o) => o,
            Err(e) => {
                // 组装就失败(节点缺必填参数、协议读不懂),根本没发出去。
                // 记一条审计:界面上要能解释「为什么这台一直没跟上」。
                tracing::error!(agent_id, error = %e, "组装配置失败,未下发");
                let now = chrono::Local::now().timestamp();
                let _ = crate::db::agent_repo::log_event(
                    pool,
                    Some(agent_id),
                    "config_build_failed",
                    &e.to_string(),
                    now,
                )
                .await;
                continue;
            }
        };

        let payload = serde_json::json!({ "revision": master_rev, "options": options });
        match rpc.call_default(registry, agent_id, method::CONFIG_APPLY, payload).await {
            Ok(_) => {
                registry.lock().await.mark_config_sent(agent_id, master_rev);
                tracing::info!(agent_id, rev = master_rev, "config.apply 已下发并生效");
            }
            Err(e) => {
                // agent 回的 error 里带着 box.New 的失败原文(§4.2),值得留痕。
                tracing::error!(agent_id, error = %e, "config.apply 失败(下一轮重试)");
                let now = chrono::Local::now().timestamp();
                let _ = crate::db::agent_repo::log_event(
                    pool,
                    Some(agent_id),
                    "config_apply_failed",
                    &e.to_string(),
                    now,
                )
                .await;
            }
        }
    }
}

/// 向**库里比它新**的那些在线 agent 下发禁用名单。
///
/// 和 `push_config` 是同一个形状,原因也一样:早先这里由 `tick` 的
/// `sum.changed()` 把门 —— 而那个标志只反映**配额巡检**的结果。
/// 管理员在 TUI 里手动停用一个没超额的用户时,`user_state_revision` 加了一,
/// 但 `changed()` 是 false,于是名单压根不下发,得等那台 agent 重连才生效。
///
/// 离线的不管——它们重连握手时会按 revision 自动补齐(§4.1)。
/// 这就是为什么这里失败也不需要重试:下次握手是天然的重试点。
async fn push_user_state(pool: &SqlitePool, registry: &Arc<Mutex<Registry>>, rpc: &Arc<Rpc>) {
    let online = registry.lock().await.online_ids();
    if online.is_empty() {
        return;
    }

    // 名单是全局的,查一次给所有人用。
    let disabled = match crate::service::disabled_users(pool).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "查询禁用名单失败,本轮不下发");
            return;
        }
    };

    for agent_id in online {
        let master_rev: i64 =
            match sqlx::query_scalar("SELECT user_state_revision FROM agents WHERE id = ?")
                .bind(agent_id)
                .fetch_one(pool)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(agent_id, error = %e, "读取 user_state_revision 失败,本轮跳过");
                    continue;
                }
            };

        // `None` = 握手的 catch_up 还没跑完(并行 spawn 的),它正要处理这件事。
        let Some(sent) = registry.lock().await.sent_user_rev(agent_id) else {
            continue;
        };
        if sent >= master_rev {
            continue;
        }

        let payload = serde_json::json!({
            "user_state_revision": master_rev,
            "disabled": disabled,
        });
        match rpc.call_default(registry, agent_id, method::USER_STATE, payload).await {
            Ok(_) => {
                registry.lock().await.mark_user_sent(agent_id, master_rev);
                tracing::info!(
                    agent_id,
                    rev = master_rev,
                    count = disabled.len(),
                    "user.state 已下发"
                );
            }
            Err(e) => tracing::warn!(agent_id, error = %e, "user.state 下发失败(下一轮重试)"),
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
            // 只让 agent 用真 sing-box `box.New()` 试建一次就 `Close()`,**不接管当前实例**
            // (`boxctl.Check`)。拿到的错误就是 sing-box 自己的原文,包括拼错的字段名
            // (`json: unknown field "..."`) —— 那是主控自己永远给不出的信息:
            // 主控里没有 sing-box(§0.3 结论一),它只能看出 JSON 语法错。
            "config_check" => sbx_shared::method::CONFIG_CHECK,
            other => {
                // 不认识的指令要**记下来**再丢掉。静默丢弃会让人对着一个
                // 永远「待办」的队列发懵。
                let msg = format!("不认识的指令类型:{other}");
                tracing::warn!(id = cmd.id, kind = other, "丢弃不认识的指令");
                let _ = crate::db::command_repo::finish(pool, cmd.id, Some(&msg), now).await;
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

/// 库有没有被**别的进程**改过。
///
/// `PRAGMA data_version` 的语义正好是这个:它在其他连接提交写事务后变化,
/// 而**自己**这条连接的写不会让它变。TUI 与 daemon 是两个进程、只共享
/// 这个库(§8.0),这就是「TUI 刚改了东西」唯一不需要额外信号通道的信号源。
///
/// 读失败时返回上一次的值(视作没变)—— 一次读不到不该触发一轮空推送。
async fn data_version(pool: &SqlitePool, last: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("PRAGMA data_version").fetch_one(pool).await.unwrap_or(last)
}

/// 醒来的两种理由。
enum Wake {
    /// 到了固定周期(月重置、配额判定这些**跟时间走**的事靠它)。
    Tick,
    /// 库被别的进程改了(TUI/CLI 刚写完)。只做下发,不做时间相关的判定。
    DbChanged,
}

pub fn spawn(
    state: crate::cluster::ServerState,
    interval_secs: u64,
    tg: Option<tokio::sync::mpsc::Sender<crate::tg::Event>>,
) {
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
        // 默认的 Burst 行为会在卡顿后连补几拍,对巡检没意义(它是幂等的,补拍只是浪费)。
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // 盯库变更的快轮询。1 秒是**给人看的延迟**:TUI 里改完配置,
        // 状态栏说「已下发」,那就该在一次眨眼之内真的发出去,而不是等 30 秒。
        //
        // 代价只有一句 `PRAGMA data_version` —— 它读的是内存里的页头计数器,
        // 不碰磁盘、不加锁,比这个循环里任何一次真正的查询都便宜。
        let mut watch = tokio::time::interval(std::time::Duration::from_secs(1));
        watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut seen_version = data_version(&state.pool, 0).await;

        loop {
            let wake = tokio::select! {
                _ = ticker.tick() => Wake::Tick,
                _ = watch.tick() => {
                    let v = data_version(&state.pool, seen_version).await;
                    if v == seen_version {
                        continue;
                    }
                    seen_version = v;
                    Wake::DbChanged
                }
            };

            let now = chrono::Local::now().timestamp();

            // 时间相关的判定(月重置、到期、配额)只在固定周期做。
            // 库一变就跑一遍的话,TUI 上每敲一个字都会触发一次全表扫描 ——
            // 而这些判定本来就没有秒级需求。
            if let Wake::Tick = wake {
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
                // `tick` 里已经推过一轮,这里不重复。
            } else {
                // 库变了:只把该下发的发出去。两个 push 内部都按 revision 比对,
                // 没变化的 agent 一条消息都不会发,所以这里可以放心每次都调。
                push_user_state(&state.pool, &state.registry, &state.rpc).await;
                push_config(&state.pool, &state.registry, &state.rpc).await;
            }

            // TUI 排进来的一次性指令(升级)。与配额巡检分开做:
            // 它失败了不该把巡检那一半也带下水。
            //
            // 两种唤醒都要取:按 [u] 之后等 30 秒才开始升级,人会以为没按上。
            match drain_commands(&state.pool, &state.registry, &state.rpc, now).await {
                Ok(n) if n > 0 => tracing::info!(count = n, "已下发排队的指令"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "取排队指令失败"),
            }

            if let Wake::Tick = wake {
                if let Some(tx) = &tg {
                    if let Err(e) = report_usage(&state.pool, tx).await {
                        tracing::warn!(error = %e, "推送用量给 Telegram 失败");
                    }
                }
            }

            // 自己刚才那些写(下发成功后没有写库,但 drain_commands 会写 finish)
            // 不该在下一拍被当成「外部改动」再触发一次。重新取一次基线。
            seen_version = data_version(&state.pool, seen_version).await;
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
        if tx.try_send(crate::tg::Event::QuotaAlert { username: name, percent }).is_err() {
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
        chrono::Local.with_ymd_and_hms(y, m, d, 12, 0, 0).single().unwrap().timestamp()
    }

    /// 一个**确定的 UTC 时刻**。
    ///
    /// 跨时区的边界测试不能用 `ts()` —— 那个给的是主控本机中午,值跟着 CI 机器的
    /// `TZ` 走,而这些用例要断言的正是「谁在哪一拍翻月」。正午对 |偏移| < 12h
    /// 不跨日,所以旧用例继续用 `ts()` 没问题;新用例必须锚在 UTC 上。
    fn utc_ts(y: i32, m: u32, d: u32, h: u32) -> i64 {
        chrono::Utc.with_ymd_and_hms(y, m, d, h, 0, 0).single().unwrap().timestamp()
    }

    /// 造一台带网卡用量的 agent,可选地设重置日与重置时区偏移。
    async fn nic_agent(
        p: &SqlitePool,
        name: &str,
        reset_day: i64,
        manual_offset: Option<i64>,
        reported_offset: Option<i64>,
    ) -> i64 {
        let (id, _) = crate::db::agent_repo::create(p, name, 0).await.unwrap();
        sqlx::query(
            "UPDATE agents SET nic_reset_day = ?, nic_reset_offset_secs = ?,
                               reported_utc_offset_secs = ? WHERE id = ?",
        )
        .bind(reset_day)
        .bind(manual_offset)
        .bind(reported_offset)
        .bind(id)
        .execute(p)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_nic_traffic
               (agent_id, boot_id, last_rx, last_tx, cycle_rx, cycle_tx, cycle_start, updated_at)
             VALUES (?, 'b', 9000, 9000, 500, 600, 1, 1)",
        )
        .bind(id)
        .execute(p)
        .await
        .unwrap();
        id
    }

    /// 这台机器本周期用量与闸门月份。
    async fn nic_state(p: &SqlitePool, id: i64) -> (i64, i64, String) {
        sqlx::query_as(
            "SELECT cycle_rx, cycle_tx, last_reset_ym FROM agent_nic_traffic WHERE agent_id = ?",
        )
        .bind(id)
        .fetch_one(p)
        .await
        .unwrap()
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

    /// **本次故障的正面复现。** 两台机器同一个重置日 22,一台 UTC-7 一台 UTC:
    /// 它们必须在**不同的拍**上翻月。这条挂了,就是又回到了「全网按主控时区翻月」。
    #[tokio::test]
    async fn a_minus_seven_agent_and_a_utc_agent_reset_on_different_ticks() {
        let p = pool().await;
        let dmit = nic_agent(&p, "dmit", 22, Some(-7 * 3600), None).await;
        let utc = nic_agent(&p, "utc", 22, Some(0), None).await;

        // 8/22 00:30 UTC —— UTC 那台已经进 22 号,dmit 当地还是 21 号 17:30。
        assert_eq!(reset_nic_cycles(&p, utc_ts(2026, 8, 22, 0)).await.unwrap(), 1);
        assert_eq!(nic_state(&p, utc).await, (0, 0, "2026-08".into()), "UTC 那台该清了");
        let (rx, tx, ym) = nic_state(&p, dmit).await;
        assert_eq!((rx, tx, ym.as_str()), (500, 600, ""), "dmit 当地还没到 22 号,不该动");

        // 8/22 08:00 UTC = dmit 当地 01:00,过了它自己的零点。
        assert_eq!(reset_nic_cycles(&p, utc_ts(2026, 8, 22, 8)).await.unwrap(), 1);
        assert_eq!(nic_state(&p, dmit).await, (0, 0, "2026-08".into()), "这一拍才轮到 dmit");
    }

    /// 跨月:主控已经进 9 月,而 UTC-7 的机器当地还是 8/31。它这个月清过了,
    /// **不能再清一次**。闸门用主控的 `ym` 的话这里会多清一次 —— 而且要等一个月才复现。
    #[tokio::test]
    async fn an_agent_still_in_last_month_locally_does_not_reset_again() {
        let p = pool().await;
        let id = nic_agent(&p, "dmit", 22, Some(-7 * 3600), None).await;
        sqlx::query("UPDATE agent_nic_traffic SET last_reset_ym = '2026-08' WHERE agent_id = ?")
            .bind(id)
            .execute(&p)
            .await
            .unwrap();

        // 9/1 03:00 UTC = 当地 8/31 20:00。日子够(31 >= 22),但月份还是 8。
        assert_eq!(reset_nic_cycles(&p, utc_ts(2026, 9, 1, 3)).await.unwrap(), 0);
        assert_eq!(nic_state(&p, id).await, (500, 600, "2026-08".into()), "不该被清第二次");
    }

    /// 反方向:UTC+14 的机器当地已经进下个月,该清、而且**只清一次**。
    /// 闸门写成主控的月份(还是上个月)会让它在主控跨月时再清一次。
    #[tokio::test]
    async fn an_agent_already_in_next_month_locally_resets_once_not_twice() {
        let p = pool().await;
        let id = nic_agent(&p, "kiritimati", 1, Some(14 * 3600), None).await;

        // 8/31 15:00 UTC = 当地 9/1 05:00。
        assert_eq!(reset_nic_cycles(&p, utc_ts(2026, 8, 31, 15)).await.unwrap(), 1);
        assert_eq!(nic_state(&p, id).await.2, "2026-09", "闸门要写当地的月份");
        // 主控随后也跨进 9 月 —— 不能因此再清一次。
        assert_eq!(reset_nic_cycles(&p, utc_ts(2026, 9, 1, 2)).await.unwrap(), 0);
    }

    /// 短月夹取按 **agent 当地的月份**算。同一时刻两台机器落在不同的月里:
    /// UTC-7 的当地是 2/28(2 月只有 28 天,`min(31,28)` → 该清),
    /// UTC+14 的当地已是 3/1(`min(31,31)` → 1 号还不该清)。
    #[tokio::test]
    async fn a_short_month_clamps_in_the_agents_own_calendar() {
        let p = pool().await;
        let west = nic_agent(&p, "west", 31, Some(-7 * 3600), None).await;
        let east = nic_agent(&p, "east", 31, Some(14 * 3600), None).await;

        // 2026-02-28 20:00 UTC → west 当地 2/28 13:00;east 当地 3/1 10:00。
        assert_eq!(reset_nic_cycles(&p, utc_ts(2026, 2, 28, 20)).await.unwrap(), 1);
        assert_eq!(nic_state(&p, west).await, (0, 0, "2026-02".into()), "2 月最后一天要补上");
        let (rx, _, ym) = nic_state(&p, east).await;
        assert_eq!((rx, ym.as_str()), (500, ""), "当地才 3/1,离 31 号还远");
    }

    /// 手工覆盖压过 agent 上报的值 —— agent 本机时区不等于厂商计费时区时,
    /// 人说的必须算。同时也验证候选查询真的把两列都取回来了。
    #[tokio::test]
    async fn the_manual_override_beats_what_the_agent_reported() {
        let p = pool().await;
        // agent 报自己在 UTC-7,但人手工钉成 UTC。
        let id = nic_agent(&p, "a", 22, Some(0), Some(-7 * 3600)).await;
        assert_eq!(
            reset_nic_cycles(&p, utc_ts(2026, 8, 22, 0)).await.unwrap(),
            1,
            "该按手工的 UTC 翻"
        );
        assert_eq!(nic_state(&p, id).await, (0, 0, "2026-08".into()));
    }

    /// 只有上报值时用上报值。
    #[tokio::test]
    async fn the_reported_offset_is_used_when_nobody_overrode_it() {
        let p = pool().await;
        let id = nic_agent(&p, "a", 22, None, Some(-7 * 3600)).await;
        assert_eq!(
            reset_nic_cycles(&p, utc_ts(2026, 8, 22, 0)).await.unwrap(),
            0,
            "当地还是 21 号"
        );
        assert_eq!(reset_nic_cycles(&p, utc_ts(2026, 8, 22, 8)).await.unwrap(), 1);
        assert_eq!(nic_state(&p, id).await.2, "2026-08");
    }

    /// **老 agent 护栏。** 两列都空时必须与升级前逐字节一致 ——
    /// 判定基准就是当年那个 `local_date(now)`,所以这条不看 CI 机器的时区。
    #[tokio::test]
    async fn an_agent_that_reports_nothing_keeps_the_master_timezone_boundary() {
        let p = pool().await;
        let now = ts(2026, 3, 12);
        let today = local_date(now).unwrap().day() as i64;
        // 重置日正好是主控的今天:偏移只要错一天,这条就挂。
        let id = nic_agent(&p, "legacy", today, None, None).await;
        assert_eq!(reset_nic_cycles(&p, now).await.unwrap(), 1, "两列都空 → 走主控时区");
        assert_eq!(nic_state(&p, id).await.0, 0);
    }

    /// 库里躺着一个离谱的偏移(手改过、或者以后哪个 agent 报错了)时回落,
    /// 而不是 panic、也不是把整轮巡检连带跳过 —— 那会停掉**所有**机器的重置。
    #[tokio::test]
    async fn an_absurd_offset_falls_back_instead_of_breaking_the_whole_tick() {
        let p = pool().await;
        let now = ts(2026, 3, 12);
        let today = local_date(now).unwrap().day() as i64;
        let broken = nic_agent(&p, "broken", today, Some(99 * 3600), None).await;
        let ok = nic_agent(&p, "ok", today, None, None).await;
        assert_eq!(reset_nic_cycles(&p, now).await.unwrap(), 2, "坏的那台回落,好的那台照常");
        assert_eq!(nic_state(&p, broken).await.0, 0);
        assert_eq!(nic_state(&p, ok).await.0, 0);
    }

    /// 用户周期与网卡周期用**两套日历**:用户跟主控,网卡跟 agent(§6.4)。
    #[tokio::test]
    async fn user_and_nic_cycles_use_different_calendars() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        let uid = user_with_usage(&p, "alice", 0, GB, 0).await;
        sqlx::query("UPDATE users SET reset_day = 22 WHERE id = ?")
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();
        // 这台 agent 被钉在 UTC+14,当地已经进了下一天/下个月边界之外。
        let aid = nic_agent(&p, "east", 22, Some(14 * 3600), None).await;

        let s = tick(&p, &reg, &rpc, ts(2026, 8, 22)).await.unwrap();
        assert_eq!(s.users_reset, 1, "用户按主控日历该清");
        assert_eq!(s.nics_reset, 1, "这台 agent 当地也过了 22 号");
        // 关键是两者各自记的月份来自各自的日历,互不干扰。
        let uym: String = sqlx::query_scalar("SELECT last_reset_ym FROM users WHERE id = ?")
            .bind(uid)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(uym, format!("{:04}-{:02}", 2026, 8));
        assert_eq!(nic_state(&p, aid).await.2, "2026-08");
    }

    /// 没有 reset_day 的用户永不重置(NULL = 无需重置)。
    #[tokio::test]
    async fn null_reset_day_never_resets() {
        let p = pool().await;
        let (reg, rpc) = empty_state();
        user_with_usage(&p, "alice", 0, GB, GB).await; // reset_day 默认 NULL

        for day in [1, 15, 28] {
            assert_eq!(tick(&p, &reg, &rpc, ts(2026, 3, day)).await.unwrap().users_reset, 0);
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

    /// `PRAGMA data_version` 的语义必须**真的**是「别的连接写了」。
    ///
    /// 整个「改完 1 秒内下发」都建在这一条上,而它是我从文档读来的 ——
    /// 所以实测一遍:自己写不变、别的连接写要变。这两条哪条不成立,
    /// 唤醒机制要么永不触发,要么每拍都触发(退化成 1 秒一次全量推送)。
    #[tokio::test]
    async fn data_version_tracks_other_connections_only() {
        let path = std::env::temp_dir().join(format!("sbx-dv-{}.db", uuid::Uuid::new_v4()));
        let p1 = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();
        let p2 = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();

        // 同一个池上取两次:没人写过,值不该变。
        let base = data_version(&p1, 0).await;
        assert_eq!(data_version(&p1, base).await, base, "没人写就不该变");

        // **另一个连接**写 —— 这是 TUI 改库那一幕。
        crate::db::agent_repo::create(&p2, "from-tui", 0).await.unwrap();
        let after = data_version(&p1, base).await;
        assert_ne!(after, base, "别的连接写完之后必须变,否则永远不会被唤醒");

        // 再读一次,没有新的写:要稳定下来,不能每拍都在跳。
        assert_eq!(data_version(&p1, after).await, after, "没有新写入就该稳住");
    }

    /// 造一台在线 agent:注册一条假连接,并把「已下发版本」对齐到当前库值,
    /// 模拟握手的 catch_up 刚跑完的状态。
    async fn online_agent(
        p: &SqlitePool,
        name: &str,
    ) -> (i64, Arc<Mutex<Registry>>, tokio::sync::mpsc::UnboundedReceiver<sbx_shared::Envelope>)
    {
        let (id, _) = crate::db::agent_repo::create(p, name, 0).await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut r = Registry::new();
        r.register(id, tx);
        // **两个 revision 都要对齐。** 只标其中一个的话,另一条路会被
        // 「`None` = catch_up 还没跑完」那条规则跳过,测试就测了个寂寞。
        let (cfg_rev, user_rev): (i64, i64) =
            sqlx::query_as("SELECT config_revision, user_state_revision FROM agents WHERE id = ?")
                .bind(id)
                .fetch_one(p)
                .await
                .unwrap();
        r.mark_config_sent(id, cfg_rev);
        r.mark_user_sent(id, user_rev);
        (id, Arc::new(Mutex::new(r)), rx)
    }

    /// **`config_check` 要真的发得出去,结果要真的写回来。**
    ///
    /// 这条守的是一整条跨进程通路:TUI 入队 → daemon 取走 → 发给 agent
    /// → 结果写回 `agent_commands`。中间断一环,界面上的表现都是同一个:
    /// 一个永远「校验中」的状态行。
    ///
    /// 这里直接调 `drain_commands`,而不是像 `push_config` 的测试那样走 `tick`:
    /// `drain_commands` 的调用点在 `spawn` 出去的主循环体里,不在 `tick` 里。
    /// 于是「它有没有被接进循环」这一段这两条测试盖不到 ——
    /// `upgrade` 当初也是同一个状况,不为了可测性把循环体拆了。
    #[tokio::test]
    async fn a_config_check_command_reaches_the_agent_and_reports_back() {
        let p = pool().await;
        let (id, reg, mut rx) = online_agent(&p, "tokyo").await;
        let rpc = Arc::new(Rpc::new());

        let cmd = crate::db::command_repo::enqueue(
            &p,
            id,
            "config_check",
            &serde_json::json!({ "options": { "log": { "level": "warn" } } }),
            0,
        )
        .await
        .unwrap();

        let rpc2 = rpc.clone();
        let responder = tokio::spawn(async move {
            let env = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("5 秒内没等到下发 —— config_check 没接进 drain_commands?")
                .expect("通道被关了");
            assert_eq!(env.method, method::CONFIG_CHECK, "该是 config.check");
            // payload 是入队方拼好的,daemon 只转发 —— 不能在中间重组。
            assert!(env.payload.get("options").is_some(), "options 丢了:{:?}", env.payload);
            rpc2.resolve(sbx_shared::Envelope::resp_ok(
                env.id.clone().unwrap(),
                method::CONFIG_CHECK,
                serde_json::json!({}),
            ))
            .await;
        });

        drain_commands(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        responder.await.unwrap();

        let o = crate::db::command_repo::outcome(&p, cmd).await.unwrap().expect("指令该还在");
        assert!(o.taken, "该被取走了");
        assert!(o.done, "该做完了");
        assert_eq!(o.error, None, "成功时不该有错误");
    }

    /// **agent 报的错要原文落到 `error` 列里。**
    ///
    /// 那句原文就是这个功能的全部产出 —— 主控里没有 sing-box(§0.3 结论一),
    /// 字段名拼错这类错只有 agent 能报。把它换成自己的概括词等于丢掉唯一的线索。
    #[tokio::test]
    async fn a_failed_config_check_keeps_the_singbox_error_verbatim() {
        let p = pool().await;
        let (id, reg, mut rx) = online_agent(&p, "tokyo").await;
        let rpc = Arc::new(Rpc::new());
        let cmd = crate::db::command_repo::enqueue(
            &p,
            id,
            "config_check",
            &serde_json::json!({ "options": {} }),
            0,
        )
        .await
        .unwrap();

        let verbatim = r#"解析配置: json: unknown field "outbonds""#;
        let rpc2 = rpc.clone();
        let responder = tokio::spawn(async move {
            let env = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("5 秒内没等到下发")
                .expect("通道被关了");
            rpc2.resolve(sbx_shared::Envelope::resp_err(
                env.id.clone().unwrap(),
                method::CONFIG_CHECK,
                verbatim,
            ))
            .await;
        });

        drain_commands(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        responder.await.unwrap();

        let o = crate::db::command_repo::outcome(&p, cmd).await.unwrap().expect("指令该还在");
        assert!(o.done, "失败也算做完了");
        let err = o.error.expect("该有错误文本");
        assert!(err.contains("unknown field"), "sing-box 的原文丢了:{err}");
        assert!(err.contains("outbonds"), "拼错的字段名必须在 —— 人靠它定位:{err}");
    }

    /// **改了配置就要推给在线的机器,不该等它重启。**
    ///
    /// 这是 v0.4.10 修的那个 bug 的回归锚点:`config.apply` 早先只有一条
    /// 发送路径 —— 握手时的 `catch_up`。于是主控建节点、改出站策略之后,
    /// `config_revision` 在库里加了一,而已经连着的那条 WebSocket 收不到
    /// 任何东西。现场表现是「每次改完都得去被控机 systemctl restart」。
    #[tokio::test]
    async fn a_config_change_is_pushed_to_an_already_connected_agent() {
        let p = pool().await;
        let (id, reg, mut rx) = online_agent(&p, "tokyo").await;
        let rpc = Arc::new(Rpc::new());

        // 没有变化时不该发 —— config.apply 会让 agent 重建 box,
        // 每 30 秒白重建一次等于把服务打成筛子。
        tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert!(rx.try_recv().is_err(), "没改动就不该下发");

        // 模拟「建了个节点」:推进 config_revision。
        sqlx::query("UPDATE agents SET config_revision = config_revision + 1 WHERE id = ?")
            .bind(id)
            .execute(&p)
            .await
            .unwrap();

        // push_config 会等 agent 回 resp,这里起个任务扮演 agent。
        //
        // **必须带超时。** 直接 `rx.recv().await` 的话,一旦接线断了
        // (正是这条测试要防的事)它会**永久阻塞**,表现是测试挂死而不是失败 ——
        // 挂死比失败糟:CI 上看到的是超时,而不是「哪条断言没过」。
        let rpc2 = rpc.clone();
        let responder = tokio::spawn(async move {
            let env = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("5 秒内没等到下发 —— push_config 没接进 tick?")
                .expect("通道被关了");
            assert_eq!(env.method, method::CONFIG_APPLY, "该是 config.apply");
            let rev = env.payload.get("revision").and_then(|v| v.as_i64());
            assert_eq!(rev, Some(1), "带的该是新版本号");
            rpc2.resolve(sbx_shared::Envelope::resp_ok(
                env.id.clone().unwrap(),
                method::CONFIG_APPLY,
                serde_json::json!({}),
            ))
            .await;
        });

        // **走 `tick` 而不是直接调 `push_config`** —— 要覆盖的正是「接线在不在」:
        // 函数写好了却没接进巡检,就是这个 bug 原本的形状。
        tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        responder.await.unwrap();

        // 下发成功后要记账,否则下一轮会重发同一版。
        assert_eq!(reg.lock().await.sent_config_rev(id), Some(1));
    }

    /// 下发成功之后**不能再重复发**。
    ///
    /// 少了这条约束,巡检每 30 秒就让每台在线机器重建一次 box ——
    /// 而重建期间连接是断的,等于按巡检周期定时掉线。
    #[tokio::test]
    async fn a_pushed_revision_is_not_sent_twice() {
        let p = pool().await;
        let (id, reg, mut rx) = online_agent(&p, "tokyo").await;
        let rpc = Arc::new(Rpc::new());

        sqlx::query("UPDATE agents SET config_revision = 5 WHERE id = ?")
            .bind(id)
            .execute(&p)
            .await
            .unwrap();

        let rpc2 = rpc.clone();
        let responder = tokio::spawn(async move {
            let env = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("5 秒内没等到下发")
                .expect("通道被关了");
            rpc2.resolve(sbx_shared::Envelope::resp_ok(
                env.id.clone().unwrap(),
                method::CONFIG_APPLY,
                serde_json::json!({}),
            ))
            .await;
            // 第二轮不该再来 —— 再收到就是重复下发。
            rx
        });
        push_config(&p, &reg, &rpc).await;
        let mut rx = responder.await.unwrap();

        push_config(&p, &reg, &rpc).await;
        assert!(rx.try_recv().is_err(), "同一个 revision 不该发第二次");
    }

    /// 下发失败时**不记账**,下一轮要重试。
    ///
    /// 记了的话这台机器就永远停在旧配置上,而界面上看不出任何异常。
    #[tokio::test]
    async fn a_failed_push_is_retried_next_tick() {
        let p = pool().await;
        let (id, reg, mut rx) = online_agent(&p, "tokyo").await;
        let rpc = Arc::new(Rpc::new());

        sqlx::query("UPDATE agents SET config_revision = 3 WHERE id = ?")
            .bind(id)
            .execute(&p)
            .await
            .unwrap();

        let rpc2 = rpc.clone();
        let responder = tokio::spawn(async move {
            let env = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("5 秒内没等到下发")
                .expect("通道被关了");
            rpc2.resolve(sbx_shared::Envelope::resp_err(
                env.id.clone().unwrap(),
                method::CONFIG_APPLY,
                "端口冲突",
            ))
            .await;
        });
        push_config(&p, &reg, &rpc).await;
        responder.await.unwrap();

        assert_eq!(reg.lock().await.sent_config_rev(id), Some(0), "失败不该记成已下发");
    }

    /// **手动停用一个没超额的用户,也要立刻推给在线的机器。**
    ///
    /// 这是同一个 bug 的另一半:`push_user_state` 早先由 `sum.changed()` 把门,
    /// 而那个标志只反映**配额巡检**的结果。管理员按 `[t]` 停用一个用量正常的
    /// 用户时,`user_state_revision` 加了一但 `changed()` 是 false ——
    /// 名单压根发不出去,要等那台 agent 重连。
    #[tokio::test]
    async fn a_manual_disable_is_pushed_without_a_quota_change() {
        let p = pool().await;
        let (id, reg, mut rx) = online_agent(&p, "tokyo").await;
        let rpc = Arc::new(Rpc::new());

        // 一个用量为 0、不会触发任何配额判定的用户。
        user_with_usage(&p, "alice", 100 * GB, 0, 0).await;
        // 手动停用 —— 走的是 TUI/CLI 那条路,只推进 user_state_revision。
        crate::db::node_repo::set_user_enabled(&p, "alice", false).await.unwrap();

        let rpc2 = rpc.clone();
        let responder = tokio::spawn(async move {
            let env = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("5 秒内没等到 user.state —— 被 sum.changed() 挡住了?")
                .expect("通道被关了");
            assert_eq!(env.method, method::USER_STATE, "该是 user.state");
            let names: Vec<&str> = env.payload["disabled"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert!(names.contains(&"alice"), "名单里该有 alice:{names:?}");
            rpc2.resolve(sbx_shared::Envelope::resp_ok(
                env.id.clone().unwrap(),
                method::USER_STATE,
                serde_json::json!({}),
            ))
            .await;
        });

        // 这一轮配额巡检什么都不会变(用量是 0),全靠 revision 比较把它发出去。
        let s = tick(&p, &reg, &rpc, ts(2026, 3, 15)).await.unwrap();
        assert_eq!(s.disabled, 0, "这条测试的前提:配额巡检本身没有变化");
        responder.await.unwrap();

        assert!(reg.lock().await.sent_user_rev(id).unwrap() >= 1, "下发后要记账");
    }
}
