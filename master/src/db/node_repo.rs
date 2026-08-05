//! nodes / users / user_nodes 的读写。
//!
//! **每个改变配置的写操作都必须推进 `config_revision`**(§4.1),
//! 否则 agent 握手时看到 revision 相同,就不会拉取新配置——
//! 表现为「加了节点但 agent 上没有」,且没有任何报错。
//!
//! 同理,改动 `users.enabled` 必须推进 `user_state_revision`。
//! 这两条是本文件存在的主要理由:把「写库」与「推进 revision」绑在一起,
//! 而不是留给调用方记得去做。

use crate::model::node::{InboundNode, NodeParams, Protocol};
use crate::model::user::User;
use anyhow::{Context, Result};
use sqlx::SqlitePool;

// ─────────────────────────── nodes ───────────────────────────

pub async fn list_nodes(pool: &SqlitePool) -> Result<Vec<InboundNode>> {
    let rows: Vec<(i64, i64, String, String, i64, String)> = sqlx::query_as(
        "SELECT id, agent_id, tag, protocol, listen_port, params_json
           FROM nodes ORDER BY agent_id, id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, agent_id, tag, proto, port, params)| InboundNode {
            id,
            agent_id,
            tag,
            protocol: Protocol::parse(&proto),
            // 端口在库里是 INTEGER,取回来要收窄成 u16。
            // 越界只可能来自手工改库,截断比 panic 好。
            listen_port: port.clamp(0, u16::MAX as i64) as u16,
            params: serde_json::from_str(&params).unwrap_or_default(),
        })
        .collect())
}

/// 新增节点并推进该 agent 的 `config_revision`。
///
/// 返回 `(node_id, 新的 config_revision)`。调用方拿到 revision 后可以立刻下发
/// `config.apply`;agent 离线时什么都不用做——重连握手会自动补齐(§4.1)。
pub async fn add_node(
    pool: &SqlitePool,
    agent_id: i64,
    tag: &str,
    protocol: Protocol,
    listen_port: u16,
    params: &NodeParams,
) -> Result<(i64, i64)> {
    if matches!(protocol, Protocol::Unknown) {
        anyhow::bail!("不能用 unknown 协议建节点");
    }
    let mut tx = pool.begin().await?;

    let node_id: i64 = sqlx::query_scalar(
        "INSERT INTO nodes (agent_id, tag, protocol, listen_port, params_json)
         VALUES (?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(agent_id)
    .bind(tag)
    .bind(protocol.as_str())
    .bind(listen_port as i64)
    .bind(serde_json::to_string(params)?)
    .fetch_one(&mut *tx)
    .await
    .with_context(|| format!("新增节点 {tag} 失败(tag 在同一 agent 内必须唯一)"))?;

    let rev: i64 = sqlx::query_scalar(
        "UPDATE agents SET config_revision = config_revision + 1
          WHERE id = ? RETURNING config_revision",
    )
    .bind(agent_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((node_id, rev))
}

/// 改节点的可编辑部分(端口与 `params`),推进 `config_revision`。
///
/// **tag / 协议 / 所属 agent 不在可改之列**,这不是偷懒:
///   * `tag` 是 `(用户, tag)` 记账口径的一半(§7.1)。改掉它,历史流量还挂在旧 tag 上,
///     新流量记到新 tag 上,`user_traffic` 里就出现两条谁也不认识谁的账。
///   * 协议一改,`params` 里的密钥材料整套都不对(reality 的密钥对 ≠ ss 的服务端密钥),
///     等于重建一个节点 —— 那就该走「删掉重建」,而不是伪装成一次编辑。
///
/// `params` 由调用方**在原值上改**再传回来。直接构造一个新的 `NodeParams` 会把
/// reality 密钥对 / 自签证书 / ss 服务端密钥全部清空 —— 下发之后所有客户端静默失联(§9.1)。
pub async fn update_node(
    pool: &SqlitePool,
    node_id: i64,
    listen_port: u16,
    params: &NodeParams,
) -> Result<(i64, i64)> {
    let mut tx = pool.begin().await?;

    let agent_id: i64 = sqlx::query_scalar("SELECT agent_id FROM nodes WHERE id = ?")
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("没有 id 为 {node_id} 的节点"))?;

    sqlx::query("UPDATE nodes SET listen_port = ?, params_json = ? WHERE id = ?")
        .bind(listen_port as i64)
        .bind(serde_json::to_string(params)?)
        .bind(node_id)
        .execute(&mut *tx)
        .await?;

    let rev: i64 = sqlx::query_scalar(
        "UPDATE agents SET config_revision = config_revision + 1
          WHERE id = ? RETURNING config_revision",
    )
    .bind(agent_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((agent_id, rev))
}

/// 删除节点并推进 `config_revision`。返回该节点所属的 agent 与新 revision。
pub async fn delete_node(pool: &SqlitePool, node_id: i64) -> Result<(i64, i64)> {
    let mut tx = pool.begin().await?;

    let agent_id: i64 = sqlx::query_scalar("SELECT agent_id FROM nodes WHERE id = ?")
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("没有 id 为 {node_id} 的节点"))?;

    // user_nodes / user_traffic 靠外键级联清理(§6.1)。
    sqlx::query("DELETE FROM nodes WHERE id = ?")
        .bind(node_id)
        .execute(&mut *tx)
        .await?;

    let rev: i64 = sqlx::query_scalar(
        "UPDATE agents SET config_revision = config_revision + 1
          WHERE id = ? RETURNING config_revision",
    )
    .bind(agent_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((agent_id, rev))
}

// ─────────────────────────── users ───────────────────────────

pub async fn list_users(pool: &SqlitePool) -> Result<Vec<User>> {
    Ok(sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY name")
        .fetch_all(pool)
        .await?)
}

pub async fn get_user_by_name(pool: &SqlitePool, name: &str) -> Result<Option<User>> {
    Ok(sqlx::query_as::<_, User>("SELECT * FROM users WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?)
}

/// 新用户的计费倍率。
///
/// **2.0,不是 1.0。** agent 的 tracker 记的是「客户端 ↔ 本机」那一段的
/// read + write,而代理要把同样的数据再跟目标站点跑一遍(客户端→本机→目标,
/// 目标→本机→客户端)—— 网卡上进出的量约是 tracker 数字的**两倍**,
/// 而 VPS 厂商正是按网卡计费。默认 1.0 等于「用户账单只到厂商账单的一半」,
/// 是在替管理员亏钱。
///
/// 默认值放在这里而不是 schema 的 `DEFAULT`:SQLite 改不了列默认值,只能重建
/// 整张表,而 users 被 002 / 003 各加过一批列,重建就得把当前列一个不漏地抄一遍
/// —— 抄漏一列的后果是那一列的数据在人已经在用的库上直接消失。见 006 那张迁移。
pub const DEFAULT_TRAFFIC_MULTIPLIER: f64 = 2.0;

/// 新增用户。
///
/// **不推进任何 revision**:新用户还没被分配任何节点,不影响任何 agent 的配置。
/// revision 在 `assign_node` 时才推进。
pub async fn add_user(pool: &SqlitePool, name: &str, quota_bytes: i64, now: i64) -> Result<i64> {
    let uuid = uuid::Uuid::new_v4().to_string();
    // 密码给 shadowsocks / trojan 这类用;用 token 生成器出一个高熵值。
    let password = crate::cluster::token::generate();
    let sub_token = crate::cluster::token::generate();

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO users (name, uuid, password, quota_bytes, traffic_multiplier, sub_token, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(&uuid)
    .bind(&password)
    .bind(quota_bytes)
    .bind(DEFAULT_TRAFFIC_MULTIPLIER)
    .bind(&sub_token)
    .bind(now)
    .fetch_one(pool)
    .await
    .with_context(|| format!("新增用户 {name} 失败(名字必须唯一)"))?;
    Ok(id)
}

/// 把节点分配给用户,并推进该节点所属 agent 的 `config_revision`。
///
/// 一个用户可以被分配多个节点,且这些节点可以来自不同 agent(§0.0-3)。
pub async fn assign_node(pool: &SqlitePool, user_id: i64, node_id: i64) -> Result<(i64, i64)> {
    let mut tx = pool.begin().await?;

    let agent_id: i64 = sqlx::query_scalar("SELECT agent_id FROM nodes WHERE id = ?")
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("没有 id 为 {node_id} 的节点"))?;

    // OR IGNORE:重复分配是幂等的,不该报错。
    sqlx::query("INSERT OR IGNORE INTO user_nodes (user_id, node_id) VALUES (?, ?)")
        .bind(user_id)
        .bind(node_id)
        .execute(&mut *tx)
        .await?;

    let rev: i64 = sqlx::query_scalar(
        "UPDATE agents SET config_revision = config_revision + 1
          WHERE id = ? RETURNING config_revision",
    )
    .bind(agent_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((agent_id, rev))
}

/// 解除分配,推进对应 agent 的 `config_revision`。
pub async fn unassign_node(pool: &SqlitePool, user_id: i64, node_id: i64) -> Result<(i64, i64)> {
    let mut tx = pool.begin().await?;
    let agent_id: i64 = sqlx::query_scalar("SELECT agent_id FROM nodes WHERE id = ?")
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("没有 id 为 {node_id} 的节点"))?;

    sqlx::query("DELETE FROM user_nodes WHERE user_id = ? AND node_id = ?")
        .bind(user_id)
        .bind(node_id)
        .execute(&mut *tx)
        .await?;

    let rev: i64 = sqlx::query_scalar(
        "UPDATE agents SET config_revision = config_revision + 1
          WHERE id = ? RETURNING config_revision",
    )
    .bind(agent_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok((agent_id, rev))
}

/// 改用户的计费属性:配额、倍率、到期、重置日。
///
/// **不推进任何 revision**,这一条要记牢:这四项都不进 sing-box 的配置 ——
/// inbound 里只有 uuid / password / 名字。它们只被 §6.3 的巡检读,
/// 而巡检本来就是每 30 秒跑一次的。为它们推进 `config_revision` 等于
/// 「改一次配额 = 全网重建一次 box」,把最贵的动作挂在最随手的操作上。
///
/// 一个有用的连带效果:把配额从 100G 调到 200G,原先因超额被**系统**停用的用户
/// (`auto_disabled = 1`)会在下一次巡检时自动放出来 —— 不需要在这里手动改 `enabled`。
/// 管理员手动停用的(`auto_disabled = 0`)不会被这条路径影响,那是刻意的(supervisor.rs §6.3)。
pub async fn update_user(
    pool: &SqlitePool,
    user_id: i64,
    quota_bytes: i64,
    traffic_multiplier: f64,
    expire_at: Option<i64>,
    reset_day: Option<i64>,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE users SET quota_bytes = ?, traffic_multiplier = ?, expire_at = ?, reset_day = ?
          WHERE id = ?",
    )
    .bind(quota_bytes)
    .bind(traffic_multiplier)
    .bind(expire_at)
    .bind(reset_day)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        anyhow::bail!("没有 id 为 {user_id} 的用户");
    }
    Ok(())
}

/// 把某个用户的节点分配**整体替换**成 `node_ids`,返回受影响的 `(agent_id, 新 revision)`。
///
/// 为什么不是「循环调 assign / unassign」:那样每动一个节点就是一次独立事务,
/// 中途失败会留下一半分配;而且同一台 agent 上勾了三个节点就会把它的
/// `config_revision` 推进三次,日志里看起来像发生了三件事。
///
/// 受影响的 agent 取的是**改动前后两个集合的并集所属的 agent**:
/// 只看新集合会漏掉「把某台机器上最后一个节点取消勾选」—— 那台恰恰是必须重下发的。
pub async fn set_user_nodes(
    pool: &SqlitePool,
    user_id: i64,
    node_ids: &[i64],
) -> Result<Vec<(i64, i64)>> {
    let mut tx = pool.begin().await?;

    // 改动前这个用户占用了哪几台 agent。
    let before: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT n.agent_id FROM user_nodes un
           JOIN nodes n ON n.id = un.node_id
          WHERE un.user_id = ?",
    )
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM user_nodes WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    let mut after: Vec<i64> = Vec::new();
    for nid in node_ids {
        let agent_id: i64 = sqlx::query_scalar("SELECT agent_id FROM nodes WHERE id = ?")
            .bind(nid)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| anyhow::anyhow!("没有 id 为 {nid} 的节点"))?;
        sqlx::query("INSERT OR IGNORE INTO user_nodes (user_id, node_id) VALUES (?, ?)")
            .bind(user_id)
            .bind(nid)
            .execute(&mut *tx)
            .await?;
        after.push(agent_id);
    }

    let mut affected: Vec<i64> = before.into_iter().chain(after).collect();
    affected.sort_unstable();
    affected.dedup();

    let mut out = Vec::with_capacity(affected.len());
    for agent_id in affected {
        let rev: i64 = sqlx::query_scalar(
            "UPDATE agents SET config_revision = config_revision + 1
              WHERE id = ? RETURNING config_revision",
        )
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
        out.push((agent_id, rev));
    }

    tx.commit().await?;
    Ok(out)
}

/// 把某个用户绑定的**网卡流量来源**整体替换成 `agent_ids`。
/// 手动重置用户流量。**只清零已用量,不会改动月重置日期。**
///
/// 与周期重置的区别:周期重置推进 `last_reset_ym` 并自动放人,手动重置不推。
/// 手动重置的场景是「管理员想在不改档期的情况下清一次流量」——
/// 典型情况是给用户续了费或改了配额,想立刻让新额度生效而不是等下个月。
///
/// **不推进任何 revision**:这是个纯计费数字,agent 不需要知道它。
pub async fn reset_user_traffic(pool: &SqlitePool, user_id: i64) -> Result<()> {
    // 与 supervisor::reset_user_cycles 的逻辑一致:cycle_* 清零,
    // total_* 不动(终身累计),last_up / last_down **也不能动**(它们是
    // delta 计算的基线,清零会让下一次上报被当成新纪元重复计一遍)。
    sqlx::query("UPDATE user_traffic SET cycle_up = 0, cycle_down = 0 WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    // 这个用户在所有节点上都还没产生过流量时影响 0 行也是正常的
    // (他还没跑过任何流量)。只要 user 存在,清零就算成功。
    let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await?
        .is_some();
    if !exists {
        anyhow::bail!("没有 id 为 {user_id} 的用户");
    }
    Ok(())
}

/// 撤销订阅 token 时写进 `sub_token` 的前缀。
///
/// **不能像旧项目那样写空串。** sbx 的 `users.sub_token` 是 `NOT NULL UNIQUE`
/// (001 那张表),空串在**第二个**被撤销的用户上就会撞唯一索引 ——
/// 表现是「第一个能撤,后面的报数据库错误」。
///
/// 所以撤销 = 写一个**故意不合法**的值:`!revoked:<id>`。它带 `!` 和 `:`,
/// 过不了 `sub_server::token_looks_valid` 的字符集(只准字母数字和 `-_`),
/// 于是在查库之前就被挡下、返回 404;而带上 id 又保证了每行互不相同。
pub const REVOKED_PREFIX: &str = "!revoked:";

/// 这个 token 是不是被撤销了。
pub fn is_revoked(token: &str) -> bool {
    token.starts_with(REVOKED_PREFIX)
}

/// 重新生成订阅 token。返回新 token。
///
/// **不推进任何 revision**:订阅 token 不进 sing-box 配置,agent 不需要知道它。
/// 老 URL 立刻失效 —— 这正是这个操作的目的(泄露之后换一把锁)。
pub async fn regenerate_sub_token(pool: &SqlitePool, user_id: i64) -> Result<String> {
    let token = crate::cluster::token::generate();
    let n = sqlx::query("UPDATE users SET sub_token = ? WHERE id = ?")
        .bind(&token)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        anyhow::bail!("没有 id 为 {user_id} 的用户");
    }
    Ok(token)
}

/// 撤销订阅 token:订阅地址立刻返回 404,`[g]` 重新生成可以恢复。
///
/// 与「停用用户」是两件事:停用挡的是**代理连接**(§7.5),撤销挡的是
/// **订阅下载**。一个泄露了订阅链接但还该上网的用户,要的是这个而不是停用。
pub async fn revoke_sub_token(pool: &SqlitePool, user_id: i64) -> Result<()> {
    let n = sqlx::query("UPDATE users SET sub_token = ? WHERE id = ?")
        .bind(format!("{REVOKED_PREFIX}{user_id}"))
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        anyhow::bail!("没有 id 为 {user_id} 的用户");
    }
    Ok(())
}

///
/// **不推进任何 revision。** 这张表不进 sing-box 配置 —— 绑定关系只被订阅那一条路
/// 读(`sub_server::usage_header`),agent 根本不需要知道它。为它推进 revision
/// 等于「改一个只影响响应头的东西 = 全网重建一次 box」。
///
/// 语义见 `005_user_nic_bindings.sql`:绑定之后只有订阅响应头里的流量数字变成
/// 这些机器的网卡用量之和,订阅内容与用户自己的计费流量都不受影响。
pub async fn set_user_nics(pool: &SqlitePool, user_id: i64, agent_ids: &[i64]) -> Result<usize> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM user_nic_bindings WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for aid in agent_ids {
        // 存在性显式检查:外键在 SQLite 里要 `PRAGMA foreign_keys=ON` 才生效
        // (init_pool 里开了),但报错文案是「FOREIGN KEY constraint failed」,
        // 对着那句话没人知道是哪个 id 错了。
        let ok: Option<i64> = sqlx::query_scalar("SELECT id FROM agents WHERE id = ?")
            .bind(aid)
            .fetch_optional(&mut *tx)
            .await?;
        ok.ok_or_else(|| anyhow::anyhow!("没有 id 为 {aid} 的被控服务器"))?;
        sqlx::query("INSERT OR IGNORE INTO user_nic_bindings (user_id, agent_id) VALUES (?, ?)")
            .bind(user_id)
            .bind(aid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(agent_ids.len())
}

/// 每个用户绑了哪几台机器的网卡。一次查完,给 TUI 的列表用。
pub async fn user_nic_bindings(pool: &SqlitePool) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query_as("SELECT user_id, agent_id FROM user_nic_bindings ORDER BY agent_id")
        .fetch_all(pool)
        .await?)
}

/// 手工启停用户。推进**所有** agent 的 `user_state_revision`(§6.3)。
///
/// `auto_disabled` 被显式置为 false:管理员手动的启停不该被月重置之类的
/// 自动流程覆盖掉(沿用旧项目 `auto_disabled` 的语义)。
pub async fn set_user_enabled(pool: &SqlitePool, name: &str, enabled: bool) -> Result<()> {
    let mut tx = pool.begin().await?;

    let n = sqlx::query("UPDATE users SET enabled = ?, auto_disabled = 0 WHERE name = ?")
        .bind(enabled as i64)
        .bind(name)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        anyhow::bail!("没有名为 {name} 的用户");
    }

    // 禁用是全局状态,每台 agent 都要重新同步。离线的那些靠重连握手补齐。
    sqlx::query("UPDATE agents SET user_state_revision = user_state_revision + 1")
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

/// 删除用户。推进所有 agent 的 `config_revision`——
/// 用户从 inbound 的 users 列表里消失是配置变更,需要重建 box。
pub async fn delete_user(pool: &SqlitePool, name: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query("DELETE FROM users WHERE name = ?")
        .bind(name)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        anyhow::bail!("没有名为 {name} 的用户");
    }
    sqlx::query("UPDATE agents SET config_revision = config_revision + 1")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 新用户的倍率默认 **2.0**(双向)。
    ///
    /// tracker 记的是「客户端 ↔ 本机」那一段,而代理还要跟目标站点再跑一遍同样的量;
    /// 网卡上的量约是记账数字的两倍,而 VPS 厂商按网卡收钱。默认 1.0 等于
    /// 「用户账单只到厂商账单的一半」—— 那是一个会一直亏钱、且没人会发现的默认值。
    #[tokio::test]
    async fn a_new_user_is_billed_both_directions() {
        let p = pool().await;
        let id = add_user(&p, "alice", 0, 0).await.unwrap();
        let mult: f64 = sqlx::query_scalar("SELECT traffic_multiplier FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(mult, 2.0, "新用户该按双向计费");
        assert_eq!(mult, DEFAULT_TRAFFIC_MULTIPLIER);
    }

    /// **已有用户的倍率一个字节都不能动。**
    ///
    /// 它是管理员当时定的价。批量翻倍等于在所有人不知情的情况下把配额砍一半,
    /// 那会让一批人在下一次巡检(30s)里直接被停用 —— 一个「改默认值」的动作
    /// 不该有这种后果。006 那张迁移之所以是空的,就是为了这一条。
    ///
    /// 测法是**重开同一个库文件**:`init_pool` 每次都会把迁移从头过一遍,
    /// 所以这正是「老库升级到带 006 的版本」那条路径。
    #[tokio::test]
    async fn upgrading_leaves_existing_multipliers_alone() {
        let path = std::env::temp_dir().join(format!("sbx-mult-{}.db", uuid::Uuid::new_v4()));
        let url = path.to_string_lossy().to_string();

        let p = crate::db::init_pool(&url).await.unwrap();
        let id = add_user(&p, "old", 0, 0).await.unwrap();
        sqlx::query("UPDATE users SET traffic_multiplier = 1.0 WHERE id = ?")
            .bind(id)
            .execute(&p)
            .await
            .unwrap();
        p.close().await;

        // 再开一次 = 再跑一遍迁移。
        let p = crate::db::init_pool(&url).await.unwrap();
        let mult: f64 = sqlx::query_scalar("SELECT traffic_multiplier FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(mult, 1.0, "老用户的倍率不该被升级改掉");

        // 同一个库上新建的用户仍然拿 2.0 —— 「不动老的」不等于「新的也退回去」。
        let fresh = add_user(&p, "fresh", 0, 0).await.unwrap();
        let mult: f64 = sqlx::query_scalar("SELECT traffic_multiplier FROM users WHERE id = ?")
            .bind(fresh)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(mult, 2.0);
    }

    /// 撤销之后写进库里的那个值**必须过不了订阅接口的格式校验**。
    ///
    /// 这是撤销真正生效的那一步:`sub_server::handle_sub` 在查库**之前**先看
    /// token 长得对不对,不对就 404。要是撤销写进去的东西恰好还是合法形状,
    /// 那它就变成了一个「换了个名字但依然能用」的 token —— 一个静默失效的安全操作。
    #[tokio::test]
    async fn a_revoked_token_cannot_pass_the_url_check() {
        let p = pool().await;
        let id = add_user(&p, "alice", 0, 0).await.unwrap();
        revoke_sub_token(&p, id).await.unwrap();

        let token: String = sqlx::query_scalar("SELECT sub_token FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(&p)
            .await
            .unwrap();
        assert!(is_revoked(&token), "该被认作已撤销:{token}");
        assert!(
            !crate::sub_server::token_looks_valid(&token),
            "撤销值不能是合法形状,否则订阅还能下载:{token}"
        );
    }

    /// **撤销多个用户不能撞唯一索引。**
    ///
    /// `users.sub_token` 是 `NOT NULL UNIQUE`。旧项目的撤销是写空串,
    /// 那在第二个用户上就会失败 —— 表现是「第一个能撤,后面的报数据库错误」。
    /// sbx 写的是 `!revoked:<id>`,每行天然不同。
    #[tokio::test]
    async fn revoking_two_users_does_not_collide() {
        let p = pool().await;
        let a = add_user(&p, "alice", 0, 0).await.unwrap();
        let b = add_user(&p, "bob", 0, 0).await.unwrap();
        revoke_sub_token(&p, a).await.unwrap();
        revoke_sub_token(&p, b).await.unwrap(); // 这一行是重点
        let n: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT sub_token) FROM users")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(n, 2, "两个撤销值必须互不相同");
    }

    /// 重新生成会给出一个**新**的、合法形状的 token,并且能从撤销状态恢复。
    #[tokio::test]
    async fn regenerating_replaces_the_token_and_undoes_a_revoke() {
        let p = pool().await;
        let id = add_user(&p, "alice", 0, 0).await.unwrap();
        let first: String = sqlx::query_scalar("SELECT sub_token FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(&p)
            .await
            .unwrap();

        revoke_sub_token(&p, id).await.unwrap();
        let fresh = regenerate_sub_token(&p, id).await.unwrap();

        assert!(!is_revoked(&fresh), "恢复之后不该还是撤销态");
        assert!(crate::sub_server::token_looks_valid(&fresh), "新 token 该是合法形状:{fresh}");
        assert_ne!(fresh, first, "必须换一把新锁,不能把老 token 还回来");
    }

    /// 手动重置**只清零已用量**,不动月重置日期,也不动终身累计。
    ///
    /// `last_reset_ym` 不能推进:推了的话这个月的自动重置就被跳过了,
    /// 而管理员按这个键的意思是「现在清一次」,不是「这个月不用再清了」。
    /// `last_up/last_down` 更不能动 —— 它们是 delta 的基线(§5.2),
    /// 清零会让下一次上报被当成新纪元,把一整个周期的量重复计一遍。
    #[tokio::test]
    async fn resetting_traffic_keeps_the_cycle_day_and_the_lifetime_total() {
        let p = pool().await;
        let (agent_id, _) = crate::db::agent_repo::create(&p, "tokyo", 0).await.unwrap();
        let (node_id, _) = add_node(
            &p,
            agent_id,
            "n",
            Protocol::VlessReality,
            8443,
            &NodeParams::default(),
        )
        .await
        .unwrap();
        let uid = add_user(&p, "alice", 0, 0).await.unwrap();
        set_user_nodes(&p, uid, &[node_id]).await.unwrap();
        sqlx::query(
            "INSERT INTO user_traffic
                 (user_id, node_id, cycle_up, cycle_down, total_up, total_down,
                  last_up, last_down, updated_at)
             VALUES (?, ?, 100, 200, 1000, 2000, 55, 66, 0)",
        )
        .bind(uid)
        .bind(node_id)
        .execute(&p)
        .await
        .unwrap();
        sqlx::query("UPDATE users SET reset_day = 22, last_reset_ym = '2026-07' WHERE id = ?")
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();

        reset_user_traffic(&p, uid).await.unwrap();

        let (cu, cd, tu, td, lu, ld): (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT cycle_up, cycle_down, total_up, total_down, last_up, last_down
               FROM user_traffic WHERE user_id = ?",
        )
        .bind(uid)
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!((cu, cd), (0, 0), "本周期用量该清零");
        assert_eq!((tu, td), (1000, 2000), "终身累计不该动");
        assert_eq!((lu, ld), (55, 66), "delta 基线绝不能动");

        let (day, ym): (Option<i64>, String) =
            sqlx::query_as("SELECT reset_day, last_reset_ym FROM users WHERE id = ?")
                .bind(uid)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(day, Some(22), "月重置日期不该动");
        assert_eq!(ym, "2026-07", "不该推进 last_reset_ym,否则本月的自动重置会被跳过");
    }

    /// 一次流量都没跑过的用户也能重置(不该报错),而不存在的用户要报错。
    #[tokio::test]
    async fn resetting_is_fine_with_no_traffic_but_errors_on_a_missing_user() {
        let p = pool().await;
        let id = add_user(&p, "alice", 0, 0).await.unwrap();
        reset_user_traffic(&p, id).await.unwrap();
        assert!(reset_user_traffic(&p, 9999).await.is_err(), "不存在的用户该报错");
        assert!(revoke_sub_token(&p, 9999).await.is_err(), "不存在的用户该报错");
        assert!(regenerate_sub_token(&p, 9999).await.is_err(), "不存在的用户该报错");
    }

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-noderepo-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    async fn rev(p: &SqlitePool, agent_id: i64) -> (i64, i64) {
        sqlx::query_as("SELECT config_revision, user_state_revision FROM agents WHERE id = ?")
            .bind(agent_id)
            .fetch_one(p)
            .await
            .unwrap()
    }

    /// 这是本文件的核心不变量:**加节点必须推进 config_revision**。
    /// 不推进的话,agent 握手时看到 revision 相同就不拉配置,
    /// 表现为「加了节点但 agent 上没有」且无任何报错。
    #[tokio::test]
    async fn adding_a_node_bumps_config_revision() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        assert_eq!(rev(&p, a).await, (0, 0));

        let (_, r) = add_node(&p, a, "in-1", Protocol::VlessReality, 8443, &NodeParams::default())
            .await
            .unwrap();
        assert_eq!(r, 1);
        assert_eq!(rev(&p, a).await, (1, 0), "只该推进 config,不该动 user_state");
    }

    #[tokio::test]
    async fn deleting_a_node_bumps_config_revision() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (nid, _) = add_node(&p, a, "in-1", Protocol::VlessReality, 8443, &NodeParams::default())
            .await
            .unwrap();

        let (agent, r) = delete_node(&p, nid).await.unwrap();
        assert_eq!(agent, a);
        assert_eq!(r, 2, "加一次删一次 = revision 2");
    }

    #[tokio::test]
    async fn duplicate_tag_on_same_agent_is_rejected() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        add_node(&p, a, "in-1", Protocol::VlessReality, 8443, &NodeParams::default())
            .await
            .unwrap();
        let err = add_node(&p, a, "in-1", Protocol::VlessReality, 9443, &NodeParams::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("in-1"), "错误应指出冲突的 tag: {err}");
    }

    /// 同名 tag 在**不同** agent 上是合法的(UNIQUE 是 (agent_id, tag))。
    #[tokio::test]
    async fn same_tag_on_different_agents_is_allowed() {
        let p = pool().await;
        let (a1, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (a2, _) = crate::db::agent_repo::create(&p, "b", 0).await.unwrap();
        add_node(&p, a1, "in", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        assert!(add_node(&p, a2, "in", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn unknown_protocol_cannot_create_a_node() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        assert!(add_node(&p, a, "in", Protocol::Unknown, 443, &NodeParams::default())
            .await
            .is_err());
    }

    /// 新增用户不该动任何 revision——它还没被分配节点。
    #[tokio::test]
    async fn adding_a_user_does_not_bump_revisions() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        add_user(&p, "alice", 0, 0).await.unwrap();
        assert_eq!(rev(&p, a).await, (0, 0));
    }

    #[tokio::test]
    async fn assigning_a_node_bumps_that_agents_config_revision() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (nid, _) = add_node(&p, a, "in", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        let uid = add_user(&p, "alice", 0, 0).await.unwrap();

        let (_, r) = assign_node(&p, uid, nid).await.unwrap();
        assert_eq!(r, 2, "建节点 1 次 + 分配 1 次");
    }

    /// 跨 agent 分配:一个用户拿两台机器上的节点(§0.0-3)。
    /// 每台的 revision 各自推进,互不影响。
    #[tokio::test]
    async fn cross_agent_assignment_bumps_each_agent_separately() {
        let p = pool().await;
        let (a1, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (a2, _) = crate::db::agent_repo::create(&p, "b", 0).await.unwrap();
        let (n1, _) = add_node(&p, a1, "in", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        let (n2, _) = add_node(&p, a2, "in", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        let uid = add_user(&p, "alice", 0, 0).await.unwrap();

        assign_node(&p, uid, n1).await.unwrap();
        assert_eq!(rev(&p, a1).await.0, 2);
        assert_eq!(rev(&p, a2).await.0, 1, "另一台不该被牵连");

        assign_node(&p, uid, n2).await.unwrap();
        assert_eq!(rev(&p, a2).await.0, 2);
    }

    #[tokio::test]
    async fn repeated_assignment_is_idempotent() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (nid, _) = add_node(&p, a, "in", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        let uid = add_user(&p, "alice", 0, 0).await.unwrap();

        assign_node(&p, uid, nid).await.unwrap();
        assign_node(&p, uid, nid).await.unwrap(); // 不该报错

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_nodes")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(n, 1, "重复分配不该产生第二行");
    }

    /// 启停用户推进 **user_state_revision**(不是 config)——
    /// 这条路径不重建 box(§6.3 / §7.5)。
    #[tokio::test]
    async fn toggling_a_user_bumps_user_state_not_config() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        add_user(&p, "alice", 0, 0).await.unwrap();

        set_user_enabled(&p, "alice", false).await.unwrap();
        assert_eq!(rev(&p, a).await, (0, 1), "只该动 user_state");

        let u = get_user_by_name(&p, "alice").await.unwrap().unwrap();
        assert!(!u.enabled);
        assert!(!u.auto_disabled, "手动停用不该标 auto_disabled");
    }

    /// 启停必须推进**所有** agent 的 revision,包括离线的(§6.3)。
    #[tokio::test]
    async fn toggling_bumps_every_agent() {
        let p = pool().await;
        let (a1, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (a2, _) = crate::db::agent_repo::create(&p, "b", 0).await.unwrap();
        add_user(&p, "alice", 0, 0).await.unwrap();

        set_user_enabled(&p, "alice", false).await.unwrap();
        assert_eq!(rev(&p, a1).await.1, 1);
        assert_eq!(rev(&p, a2).await.1, 1);
    }

    #[tokio::test]
    async fn toggling_a_missing_user_errors() {
        let p = pool().await;
        assert!(set_user_enabled(&p, "nobody", false).await.is_err());
    }

    /// 删用户是配置变更(要从 inbound 的 users 里移除),推进 config_revision。
    #[tokio::test]
    async fn deleting_a_user_bumps_config_revision() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        add_user(&p, "alice", 0, 0).await.unwrap();

        delete_user(&p, "alice").await.unwrap();
        assert_eq!(rev(&p, a).await.0, 1);
        assert!(get_user_by_name(&p, "alice").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn user_gets_unique_uuid_password_and_sub_token() {
        let p = pool().await;
        add_user(&p, "alice", 0, 0).await.unwrap();
        add_user(&p, "bob", 0, 0).await.unwrap();

        let a = get_user_by_name(&p, "alice").await.unwrap().unwrap();
        let b = get_user_by_name(&p, "bob").await.unwrap().unwrap();
        assert_ne!(a.uuid, b.uuid);
        assert_ne!(a.password, b.password);
        assert_ne!(a.sub_token, b.sub_token);
        assert!(!a.uuid.is_empty() && !a.password.is_empty() && !a.sub_token.is_empty());
    }

    /// 改节点也要推进 config_revision —— 与新增/删除同一个理由:
    /// 不推进的话 agent 握手看到 revision 没变,就不会拉新配置,
    /// 表现为「端口改了但客户端还得连旧端口」且没有任何报错。
    #[tokio::test]
    async fn updating_a_node_bumps_config_revision_and_keeps_key_material() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let mut params = NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
        let (nid, _) = add_node(&p, a, "in", Protocol::VlessReality, 443, &params).await.unwrap();
        let priv_key = params.private_key.clone().unwrap();

        // 只改人填的那部分,密钥材料原样带回去。
        params.server_name = Some("www.microsoft.com".into());
        params.ipv6 = true;
        let (agent, rev) = update_node(&p, nid, 8443, &params).await.unwrap();
        assert_eq!(agent, a);
        assert_eq!(rev, 2, "建一次 + 改一次");

        let n = &list_nodes(&p).await.unwrap()[0];
        assert_eq!(n.listen_port, 8443);
        assert_eq!(n.params.server_name.as_deref(), Some("www.microsoft.com"));
        assert!(n.params.ipv6);
        // 这一条是本函数存在的主要风险点:密钥换了 = 全部客户端静默失联(§9.1)。
        assert_eq!(n.params.private_key.as_deref(), Some(priv_key.as_str()));
        assert!(n.params.short_id.is_some());
    }

    #[tokio::test]
    async fn updating_a_missing_node_errors() {
        let p = pool().await;
        assert!(update_node(&p, 999, 443, &NodeParams::default()).await.is_err());
    }

    /// 改配额/倍率/到期/重置日**不该动任何 revision**:这四项不进 sing-box 配置。
    /// 推进的话「改一次配额」就等于「全网重建一次 box」。
    #[tokio::test]
    async fn updating_user_billing_fields_touches_no_revision() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let uid = add_user(&p, "alice", 0, 0).await.unwrap();

        update_user(&p, uid, 100 * 1_073_741_824, 2.0, Some(1_893_456_000), Some(22))
            .await
            .unwrap();
        assert_eq!(rev(&p, a).await, (0, 0), "计费属性不该推进任何 revision");

        let u = get_user_by_name(&p, "alice").await.unwrap().unwrap();
        assert_eq!(u.quota_bytes, 100 * 1_073_741_824);
        assert_eq!(u.traffic_multiplier, 2.0);
        assert_eq!(u.expire_at, Some(1_893_456_000));
        assert_eq!(u.reset_day, Some(22));

        // 到期与重置日都能清回「永久 / 不重置」。
        update_user(&p, uid, 0, 1.0, None, None).await.unwrap();
        let u = get_user_by_name(&p, "alice").await.unwrap().unwrap();
        assert_eq!(u.expire_at, None);
        assert_eq!(u.reset_day, None);
    }

    #[tokio::test]
    async fn updating_a_missing_user_errors() {
        let p = pool().await;
        assert!(update_user(&p, 999, 0, 1.0, None, None).await.is_err());
    }

    /// 整体替换分配:一次事务、每台 agent 只推进一次 revision。
    #[tokio::test]
    async fn set_user_nodes_replaces_the_whole_set() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (n1, _) = add_node(&p, a, "n1", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        let (n2, _) = add_node(&p, a, "n2", Protocol::VlessReality, 444, &NodeParams::default())
            .await
            .unwrap();
        let (n3, _) = add_node(&p, a, "n3", Protocol::VlessReality, 445, &NodeParams::default())
            .await
            .unwrap();
        let uid = add_user(&p, "alice", 0, 0).await.unwrap();

        let out = set_user_nodes(&p, uid, &[n1, n2]).await.unwrap();
        assert_eq!(out.len(), 1, "只碰了一台 agent,就该只有一条记录");
        assert_eq!(out[0].0, a);
        let before = rev(&p, a).await.0;

        // 换成另一组:n1 留下、n2 去掉、n3 加上。
        set_user_nodes(&p, uid, &[n1, n3]).await.unwrap();
        assert_eq!(rev(&p, a).await.0, before + 1, "同一台 agent 一次替换只推进一次");

        let ids: Vec<i64> =
            sqlx::query_scalar("SELECT node_id FROM user_nodes WHERE user_id = ? ORDER BY node_id")
                .bind(uid)
                .fetch_all(&p)
                .await
                .unwrap();
        assert_eq!(ids, vec![n1, n3]);
    }

    /// 把某台 agent 上最后一个节点取消勾选时,**那台**也必须重下发 ——
    /// 只看新集合会漏掉它,表现为「界面上取消了,机器上还在服务」。
    #[tokio::test]
    async fn set_user_nodes_bumps_agents_that_lost_their_last_node() {
        let p = pool().await;
        let (a1, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (a2, _) = crate::db::agent_repo::create(&p, "b", 0).await.unwrap();
        let (n1, _) = add_node(&p, a1, "n", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        let (n2, _) = add_node(&p, a2, "n", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        let uid = add_user(&p, "alice", 0, 0).await.unwrap();

        set_user_nodes(&p, uid, &[n1, n2]).await.unwrap();
        let (r1, r2) = (rev(&p, a1).await.0, rev(&p, a2).await.0);

        // 只留 a1 的节点 —— a2 上什么都不剩了,但它同样要收到新配置。
        let out = set_user_nodes(&p, uid, &[n1]).await.unwrap();
        assert_eq!(out.len(), 2, "两台都受影响:一台加、一台减");
        assert_eq!(rev(&p, a1).await.0, r1 + 1);
        assert_eq!(rev(&p, a2).await.0, r2 + 1, "失去最后一个节点的 agent 被漏掉了");
    }

    /// 传空列表 = 清空分配。这是「取消全部勾选」的正常路径,不该报错。
    #[tokio::test]
    async fn set_user_nodes_accepts_an_empty_set() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (n1, _) = add_node(&p, a, "n", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        let uid = add_user(&p, "alice", 0, 0).await.unwrap();
        set_user_nodes(&p, uid, &[n1]).await.unwrap();

        set_user_nodes(&p, uid, &[]).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_nodes WHERE user_id = ?")
            .bind(uid)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    /// 列表里混进一个不存在的 node_id 时,**整个替换都不能生效** ——
    /// 半套分配比报错难查得多。
    #[tokio::test]
    async fn set_user_nodes_rolls_back_on_a_bad_id() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let (n1, _) = add_node(&p, a, "n", Protocol::VlessReality, 443, &NodeParams::default())
            .await
            .unwrap();
        let uid = add_user(&p, "alice", 0, 0).await.unwrap();
        set_user_nodes(&p, uid, &[n1]).await.unwrap();
        let before = rev(&p, a).await.0;

        assert!(set_user_nodes(&p, uid, &[n1, 999]).await.is_err());

        let ids: Vec<i64> = sqlx::query_scalar("SELECT node_id FROM user_nodes WHERE user_id = ?")
            .bind(uid)
            .fetch_all(&p)
            .await
            .unwrap();
        assert_eq!(ids, vec![n1], "失败的替换不该动原有分配");
        assert_eq!(rev(&p, a).await.0, before, "失败的替换不该推进 revision");
    }

    #[tokio::test]
    async fn list_nodes_round_trips_params_and_protocol() {        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let params = NodeParams {
            server_name: Some("www.example.com".into()),
            ipv6: true,
            ..Default::default()
        };
        add_node(&p, a, "in-1", Protocol::VlessReality, 8443, &params).await.unwrap();

        let nodes = list_nodes(&p).await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].protocol, Protocol::VlessReality);
        assert_eq!(nodes[0].listen_port, 8443);
        assert_eq!(nodes[0].params.server_name.as_deref(), Some("www.example.com"));
        assert!(nodes[0].params.ipv6);
    }
}
