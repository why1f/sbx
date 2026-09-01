//! agents 表的读写。
//!
//! token 校验路径在这里(§8.1):**按 prefix 索引定位 → sha256 → 恒定时间比较**。

use crate::model::agent::Agent;
use anyhow::{Context, Result};
use sqlx::SqlitePool;

/// 取全部 agent,按名字排序(TUI 列表用,需要稳定顺序)。
pub async fn list(pool: &SqlitePool) -> Result<Vec<Agent>> {
    Ok(sqlx::query_as::<_, Agent>("SELECT * FROM agents ORDER BY name").fetch_all(pool).await?)
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
    Ok(candidates.into_iter().find(|a| crate::cluster::token::verify(&want, &a.token_hash)))
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
    sqlx::query("DELETE FROM agents WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

/// 改 agent 的人工设置:名称、网卡月配额、网卡重置日和网卡记账口径。
///
/// **不推进任何 revision**:这些项都不进下发给 agent 的配置 —— 网卡配额和口径是主控侧
/// 用来算「这台机器烧了多少」的规则,agent 自己根本不需要知道。
/// 为它推进 `config_revision` 等于「改一次配额 = 那台机器重建一次 box」。
///
/// `ipv4` / `ipv6` **不在这里改**:它们每次 `sysinfo.report` 都会被 agent 自探的值
/// 覆盖(§7.3),在这里手工填一个值只会在下一个上报周期悄悄变回去 ——
/// 一个「改了但过一会儿自己变回来」的输入框比没有这个输入框更糟。
/// 改出站地址族策略,并**推进 `config_revision`**。
///
/// 与 `update_settings` 里那几项(名称、网卡配额、重置日)不同:那些只是主控
/// 自己的记账,agent 根本不需要知道。而出站策略会进 sing-box 配置 ——
/// 不推进 revision 的话,agent 握手时看到版本相同就不会拉新配置,
/// 表现是「界面上改了,机器上没变」,而且**没有任何报错**(§4.1)。
///
/// 返回新的 `config_revision`,调用方可以立刻下发 `config.apply`。
pub async fn set_outbound_strategy(
    pool: &SqlitePool,
    id: i64,
    strategy: crate::model::outbound::OutboundStrategy,
) -> Result<i64> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query("UPDATE agents SET outbound_strategy = ? WHERE id = ?")
        .bind(strategy.key())
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        anyhow::bail!("没有 id 为 {id} 的被控服务器");
    }
    let rev: i64 = sqlx::query_scalar(
        "UPDATE agents SET config_revision = config_revision + 1
          WHERE id = ? RETURNING config_revision",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rev)
}

pub async fn update_settings(
    pool: &SqlitePool,
    id: i64,
    name: &str,
    nic_quota_bytes: Option<i64>,
    nic_reset_day: Option<i64>,
    nic_accounting_mode: crate::model::agent::NicAccountingMode,
    // **放在 mode 后面,而不是紧挨着 nic_reset_day。** 两个 `Option<i64>` 挨在一起时
    // 调用方写反了编译器一句话都不会说,而后果是重置边界悄悄挪几个小时 ——
    // 隔一个月才复现。中间垫一个不同类型的参数,写反就编译不过。
    nic_reset_offset_secs: Option<i64>,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE agents SET name = ?, nic_quota_bytes = ?, nic_reset_day = ?,
                           nic_accounting_mode = ?, nic_reset_offset_secs = ?
         WHERE id = ?",
    )
    .bind(name)
    .bind(nic_quota_bytes)
    .bind(nic_reset_day)
    .bind(nic_accounting_mode.key())
    .bind(nic_reset_offset_secs)
    .bind(id)
    .execute(pool)
    .await
    .with_context(|| format!("改 agent #{id} 失败(名称 {name} 必须唯一)"))?
    .rows_affected();
    if n == 0 {
        anyhow::bail!("没有 id 为 {id} 的被控服务器");
    }
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
pub async fn log_event(
    pool: &SqlitePool,
    agent_id: Option<i64>,
    kind: &str,
    message: &str,
    at: i64,
) -> Result<()> {
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

    /// **改出站策略必须推进 `config_revision`。**
    ///
    /// 它进 sing-box 配置。不推的话 agent 握手时看到版本相同就不拉新配置 ——
    /// 表现是「界面上改了,机器上没变」,而且没有任何报错(§4.1)。
    /// 这是这一整类 bug 里最难查的形状,所以专门钉住。
    #[tokio::test]
    async fn changing_the_outbound_strategy_bumps_config_revision() {
        use crate::model::outbound::OutboundStrategy;
        let p = pool().await;
        let (id, _) = create(&p, "tokyo", 0).await.unwrap();

        let before: i64 = sqlx::query_scalar("SELECT config_revision FROM agents WHERE id = ?")
            .bind(id)
            .fetch_one(&p)
            .await
            .unwrap();

        let rev = set_outbound_strategy(&p, id, OutboundStrategy::Ipv6Only).await.unwrap();
        assert_eq!(rev, before + 1, "该推进一格");

        let (stored, now): (String, i64) =
            sqlx::query_as("SELECT outbound_strategy, config_revision FROM agents WHERE id = ?")
                .bind(id)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(stored, "v6only", "存的是短名");
        assert_eq!(now, rev);
    }

    /// 新建的 agent 默认 auto —— 升级到带这一列的版本时,行为不该变。
    #[tokio::test]
    async fn a_new_agent_defaults_to_auto() {
        use crate::model::outbound::OutboundStrategy;
        let p = pool().await;
        let (id, _) = create(&p, "tokyo", 0).await.unwrap();
        let stored: String =
            sqlx::query_scalar("SELECT outbound_strategy FROM agents WHERE id = ?")
                .bind(id)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(OutboundStrategy::parse(&stored), OutboundStrategy::Auto);
    }

    /// 不存在的 agent 要报错,而不是静默成功。
    #[tokio::test]
    async fn setting_the_strategy_on_a_missing_agent_errors() {
        use crate::model::outbound::OutboundStrategy;
        let p = pool().await;
        assert!(set_outbound_strategy(&p, 9999, OutboundStrategy::Ipv4Only).await.is_err());
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

    /// 改名与配额都要落库,且**不能碰任何 revision** ——
    /// 网卡配额是主控自己算账用的口径,agent 根本不需要知道(§6.4)。
    #[tokio::test]
    async fn update_settings_persists_without_touching_revisions() {
        let p = pool().await;
        let (id, _) = create(&p, "tokyo-1", 0).await.unwrap();
        sqlx::query(
            "INSERT INTO agent_nic_traffic
               (agent_id, boot_id, last_rx, last_tx, cycle_rx, cycle_tx,
                cycle_start, last_reset_ym, updated_at)
             VALUES (?, 'boot', 1000, 2000, 300, 200, 10, '2026-08', 20)",
        )
        .bind(id)
        .execute(&p)
        .await
        .unwrap();

        update_settings(
            &p,
            id,
            "tokyo-1a",
            Some(500 * 1_073_741_824),
            Some(22),
            crate::model::agent::NicAccountingMode::Sum,
            Some(-7 * 3600),
        )
        .await
        .unwrap();
        let a = get(&p, id).await.unwrap().unwrap();
        assert_eq!(a.name, "tokyo-1a");
        assert_eq!(a.nic_quota_bytes, Some(500 * 1_073_741_824));
        assert_eq!(a.nic_reset_day, Some(22));
        assert_eq!(a.nic_accounting_mode, "sum");
        assert_eq!(a.nic_reset_offset_secs, Some(-25200), "重置时区要往返");
        assert_eq!(a.reported_utc_offset_secs, None, "手工覆盖不该动 agent 上报那一列");

        let revs: (i64, i64) =
            sqlx::query_as("SELECT config_revision, user_state_revision FROM agents WHERE id = ?")
                .bind(id)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(revs, (0, 0), "改设置不该让 agent 重建 box");

        // 都能清回「不限 / 不重置」。
        update_settings(
            &p,
            id,
            "tokyo-1a",
            None,
            None,
            crate::model::agent::NicAccountingMode::Max,
            None,
        )
        .await
        .unwrap();
        let a = get(&p, id).await.unwrap().unwrap();
        assert_eq!(a.nic_quota_bytes, None);
        assert_eq!(a.nic_reset_day, None);
        assert_eq!(a.nic_accounting_mode, "max");
        assert_eq!(a.nic_reset_offset_secs, None, "也能清回「跟随 agent」");
        let raw: (String, i64, i64, i64, i64, i64, String, i64) = sqlx::query_as(
            "SELECT boot_id, last_rx, last_tx, cycle_rx, cycle_tx, cycle_start,
                    last_reset_ym, updated_at
               FROM agent_nic_traffic WHERE agent_id = ?",
        )
        .bind(id)
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(raw, ("boot".into(), 1000, 2000, 300, 200, 10, "2026-08".into(), 20));
    }

    /// 名字撞了要给一句认得出的话,不是 sqlx 的原始错误。
    #[tokio::test]
    async fn update_settings_reports_a_name_clash() {
        let p = pool().await;
        let (id, _) = create(&p, "a", 0).await.unwrap();
        create(&p, "b", 0).await.unwrap();
        let err = update_settings(
            &p,
            id,
            "b",
            None,
            None,
            crate::model::agent::NicAccountingMode::Sum,
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains('b'), "错误里要指出冲突的名字: {err}");
    }

    #[tokio::test]
    async fn update_settings_on_a_missing_agent_errors() {
        let p = pool().await;
        assert!(update_settings(
            &p,
            999,
            "x",
            None,
            None,
            crate::model::agent::NicAccountingMode::Sum,
            None,
        )
        .await
        .is_err());
    }
}
