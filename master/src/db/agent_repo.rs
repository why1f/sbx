//! agents 表的读写。
//!
//! token 校验路径在这里(§8.1):**按 prefix 索引定位 → sha256 → 恒定时间比较**。

use crate::model::agent::Agent;
use anyhow::Result;
use sqlx::SqlitePool;

/// 取全部 agent,按名字排序(TUI 列表用,需要稳定顺序)。
pub async fn list(pool: &SqlitePool) -> Result<Vec<Agent>> {
    Ok(sqlx::query_as::<_, Agent>("SELECT * FROM agents ORDER BY name")
        .fetch_all(pool)
        .await?)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Agent>> {
    Ok(sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// 按 token 明文查 agent。**这是未认证输入的入口,注意两件事:**
///
/// 1. 用 `token_prefix` 收窄候选集,避免全表扫描(也就避免了慢 KDF 那种 DoS 面,§8.1);
/// 2. 命中候选后用**恒定时间**比较 hash,不用 `==` 提前返回。
///
/// 返回 `None` 时调用方应记 `agent_events` 但**不向连接方回显原因**(避免枚举,§8.1)。
pub async fn find_by_token(pool: &SqlitePool, token: &str) -> Result<Option<Agent>> {
    let prefix = crate::cluster::token::prefix_of(token);
    let want = crate::cluster::token::hash(token);

    let candidates = sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE token_prefix = ?")
        .bind(&prefix)
        .fetch_all(pool)
        .await?;

    // 逐个恒定时间比较。候选通常只有 1 条(prefix 是 8 字节 base64url,碰撞概率极低),
    // 但**不能因此写成「取第一条再比」**——那样 prefix 碰撞时会误判为认证失败。
    Ok(candidates
        .into_iter()
        .find(|a| crate::cluster::token::verify(&want, &a.token_hash)))
}

/// 新增 agent。返回 `(agent_id, token 明文)` —— **明文只在这里出现一次**,
/// 之后库里只有 hash 与 prefix(§8.1)。调用方负责显示给用户,不要落日志。
pub async fn create(pool: &SqlitePool, name: &str, now: i64) -> Result<(i64, String)> {
    let token = crate::cluster::token::generate();
    let id = sqlx::query(
        "INSERT INTO agents (name, token_hash, token_prefix, status, created_at)
         VALUES (?, ?, ?, 'never', ?)",
    )
    .bind(name)
    .bind(crate::cluster::token::hash(&token))
    .bind(crate::cluster::token::prefix_of(&token))
    .bind(now)
    .execute(pool)
    .await?
    .last_insert_rowid();
    Ok((id, token))
}

/// 轮换 token。新 token 使旧的立即失效;**在线连接不立刻踢**,下次重连时生效(§8.1)。
pub async fn rotate_token(pool: &SqlitePool, id: i64) -> Result<String> {
    let token = crate::cluster::token::generate();
    sqlx::query("UPDATE agents SET token_hash = ?, token_prefix = ? WHERE id = ?")
        .bind(crate::cluster::token::hash(&token))
        .bind(crate::cluster::token::prefix_of(&token))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(token)
}

/// 删除 agent。`nodes` 与 `user_nodes` / `user_traffic` 靠外键级联清理(§6.1)。
///
/// **调用前应提示会影响的用户数**(§8.1),那是 UI 的职责,不在这里。
pub async fn delete(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM agents WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 会受某 agent 删除影响的用户数(去重)——给删除确认弹窗用。
pub async fn affected_user_count(pool: &SqlitePool, id: i64) -> Result<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT un.user_id)
           FROM user_nodes un JOIN nodes n ON n.id = un.node_id
          WHERE n.agent_id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// 记一条审计事件(§5.4 的 epoch 变更、§8.1 的认证失败都走这里)。
pub async fn log_event(pool: &SqlitePool, agent_id: Option<i64>, kind: &str, message: &str, at: i64) -> Result<()> {
    sqlx::query("INSERT INTO agent_events (agent_id, kind, message, at) VALUES (?, ?, ?, ?)")
        .bind(agent_id)
        .bind(kind)
        .bind(message)
        .bind(at)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-agentrepo-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    #[tokio::test]
    async fn created_token_authenticates_and_plaintext_is_not_stored() {
        let p = pool().await;
        let (id, token) = create(&p, "agent-a", 0).await.unwrap();

        let found = find_by_token(&p, &token).await.unwrap();
        assert_eq!(found.map(|a| a.id), Some(id), "刚生成的 token 应能认证");

        // 库里不该有明文
        let stored: String = sqlx::query_scalar("SELECT token_hash FROM agents WHERE id = ?")
            .bind(id)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_ne!(stored, token, "库里存的必须是 hash 不是明文");
        assert_eq!(stored.len(), 64, "sha256 十六进制是 64 字符");
    }

    #[tokio::test]
    async fn wrong_token_does_not_authenticate() {
        let p = pool().await;
        create(&p, "agent-a", 0).await.unwrap();
        assert!(find_by_token(&p, "definitely-not-the-token").await.unwrap().is_none());
        assert!(find_by_token(&p, "").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rotate_invalidates_the_old_token() {
        let p = pool().await;
        let (id, old) = create(&p, "agent-a", 0).await.unwrap();
        let new = rotate_token(&p, id).await.unwrap();

        assert!(find_by_token(&p, &old).await.unwrap().is_none(), "旧 token 应立即失效");
        assert_eq!(find_by_token(&p, &new).await.unwrap().map(|a| a.id), Some(id));
    }

    #[tokio::test]
    async fn affected_user_count_dedupes_across_nodes() {
        let p = pool().await;
        let (aid, _) = create(&p, "agent-a", 0).await.unwrap();
        for (nid, tag) in [(1, "in1"), (2, "in2")] {
            sqlx::query(
                "INSERT INTO nodes (id, agent_id, tag, protocol, listen_port, params_json)
                 VALUES (?, ?, ?, 'trojan', 443, '{}')",
            )
            .bind(nid)
            .bind(aid)
            .bind(tag)
            .execute(&p)
            .await
            .unwrap();
        }
        sqlx::query("INSERT INTO users (id, name, uuid, password, sub_token, created_at) VALUES (1,'alice','u','p','t',0)")
            .execute(&p).await.unwrap();
        // 同一用户占了这台 agent 上的两个节点 —— 应算 1 个受影响用户,不是 2
        sqlx::query("INSERT INTO user_nodes (user_id, node_id) VALUES (1,1),(1,2)")
            .execute(&p)
            .await
            .unwrap();

        assert_eq!(affected_user_count(&p, aid).await.unwrap(), 1);
    }
}
