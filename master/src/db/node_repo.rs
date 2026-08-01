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
        "INSERT INTO users (name, uuid, password, quota_bytes, sub_token, created_at)
         VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(&uuid)
    .bind(&password)
    .bind(quota_bytes)
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

    #[tokio::test]
    async fn list_nodes_round_trips_params_and_protocol() {
        let p = pool().await;
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
