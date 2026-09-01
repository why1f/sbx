//! TUI → daemon 的一次性指令队列(见 `007_agent_commands.sql`)。
//!
//! 存在的理由只有一条:**TUI 和 daemon 是两个进程**。WS 连接活在 daemon 里,
//! TUI 手上没有 registry 也没有 rpc,没法直接给 agent 发 `agent.upgrade`。
//! 于是 TUI 入队,daemon 的巡检循环取走执行。

use anyhow::Result;
use sqlx::SqlitePool;

/// 一条待执行的指令。
#[derive(Debug, Clone)]
pub struct Pending {
    pub id: i64,
    pub agent_id: i64,
    pub kind: String,
    pub payload_json: String,
}

/// 入队。`payload` 由调用方拼好 —— 版本、产物地址、校验和都是主控侧的知识,
/// daemon 只负责转发。
pub async fn enqueue(
    pool: &SqlitePool,
    agent_id: i64,
    kind: &str,
    payload: &serde_json::Value,
    now: i64,
) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO agent_commands (agent_id, kind, payload_json, created_at)
         VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(agent_id)
    .bind(kind)
    .bind(payload.to_string())
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// 取走所有待办并**当场标记为已取**。
///
/// 标记和读取在同一个事务里:分开做的话,两轮巡检挨得近就会把同一条指令
/// 下发两遍 —— 而「升级」下发两遍意味着那台机器连着重启两次。
pub async fn take_pending(pool: &SqlitePool, now: i64) -> Result<Vec<Pending>> {
    let mut tx = pool.begin().await?;
    let rows: Vec<(i64, i64, String, String)> = sqlx::query_as(
        "SELECT id, agent_id, kind, payload_json FROM agent_commands
          WHERE taken_at IS NULL ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    if !rows.is_empty() {
        sqlx::query("UPDATE agent_commands SET taken_at = ? WHERE taken_at IS NULL")
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|(id, agent_id, kind, payload_json)| Pending { id, agent_id, kind, payload_json })
        .collect())
}

/// 记结果。`error` 为 `None` 表示成功。
pub async fn finish(pool: &SqlitePool, id: i64, error: Option<&str>, now: i64) -> Result<()> {
    sqlx::query("UPDATE agent_commands SET done_at = ?, error = ? WHERE id = ?")
        .bind(now)
        .bind(error)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 还没做完的条数,给界面显示「升级下发中」。
pub async fn pending_count(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM agent_commands WHERE done_at IS NULL")
        .fetch_one(pool)
        .await?)
}

/// 一条指令现在跑到哪了。
///
/// `upgrade` 不需要这个 —— 它的结果看版本号变没变就行。但 `config_check` 的
/// **结果本身就是全部产出**:界面上发起一次校验,必须能把结论取回来,
/// 否则就只是把一条记录扒进了库里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// daemon 已经取走。一直是 `false` 意味着 **daemon 没在跑**——
    /// 那是一种很容易让人对着转圈发呆的故障,界面得能区分它和「还在跑」。
    pub taken: bool,
    pub done: bool,
    /// `None` + `done` = 成功。
    pub error: Option<String>,
}

pub async fn outcome(pool: &SqlitePool, id: i64) -> Result<Option<Outcome>> {
    let row: Option<(Option<i64>, Option<i64>, Option<String>)> =
        sqlx::query_as("SELECT taken_at, done_at, error FROM agent_commands WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(taken_at, done_at, error)| Outcome {
        taken: taken_at.is_some(),
        done: done_at.is_some(),
        error,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-cmd-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    async fn agent(p: &SqlitePool, name: &str) -> i64 {
        crate::db::agent_repo::create(p, name, 0).await.unwrap().0
    }

    /// **取走一次就不该再取到。** 分两步做(先读后标)的话,两轮巡检挨得近
    /// 就会把同一条升级下发两遍 —— 那台机器会连着重启两次。
    #[tokio::test]
    async fn taking_is_exactly_once() {
        let p = pool().await;
        let a = agent(&p, "tokyo").await;
        enqueue(&p, a, "upgrade", &serde_json::json!({"url": "u"}), 0).await.unwrap();

        let first = take_pending(&p, 1).await.unwrap();
        assert_eq!(first.len(), 1, "第一次该取到");
        let second = take_pending(&p, 2).await.unwrap();
        assert!(second.is_empty(), "第二次不该再取到同一条");
    }

    /// 失败原因要留在库里 —— 它是唯一能解释「为什么那台还是旧版本」的东西。
    #[tokio::test]
    async fn the_failure_reason_survives() {
        let p = pool().await;
        let a = agent(&p, "tokyo").await;
        let id = enqueue(&p, a, "upgrade", &serde_json::json!({}), 0).await.unwrap();
        take_pending(&p, 1).await.unwrap();

        assert_eq!(pending_count(&p).await.unwrap(), 1, "还没 finish 就还算待办");
        finish(&p, id, Some("agent 那边校验没过"), 2).await.unwrap();
        assert_eq!(pending_count(&p).await.unwrap(), 0);

        let err: Option<String> =
            sqlx::query_scalar("SELECT error FROM agent_commands WHERE id = ?")
                .bind(id)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(err.as_deref(), Some("agent 那边校验没过"));
    }

    /// 删掉 agent 要把它的待办一起带走(外键级联),
    /// 否则 daemon 会对着一个不存在的 agent 反复下发。
    #[tokio::test]
    async fn deleting_an_agent_drops_its_commands() {
        let p = pool().await;
        let a = agent(&p, "tokyo").await;
        enqueue(&p, a, "upgrade", &serde_json::json!({}), 0).await.unwrap();
        crate::db::agent_repo::delete(&p, a).await.unwrap();
        assert!(take_pending(&p, 1).await.unwrap().is_empty(), "指令该随 agent 一起没了");
    }

    /// **`outcome` 要能分清「还没人取」「在跑」「成了」「败了」「没了」。**
    ///
    /// `config_check` 靠它把结论送回界面。分不清前两种的后果最隐蔽:
    /// daemon 没在跑时指令永远躺在队列里,而界面会一直显示「校验中」——
    /// 人对着一个不动的进度无从下手,而真正该做的是去看 daemon 死没死。
    #[tokio::test]
    async fn outcome_tells_the_four_stages_apart() {
        let p = pool().await;
        let a = agent(&p, "tokyo").await;
        let id = enqueue(&p, a, "config_check", &serde_json::json!({}), 0).await.unwrap();

        let o = outcome(&p, id).await.unwrap().expect("刚入队,该查得到");
        assert!(!o.taken && !o.done, "刚入队:没人取、没做完");

        take_pending(&p, 10).await.unwrap();
        let o = outcome(&p, id).await.unwrap().unwrap();
        assert!(o.taken && !o.done, "取走了但还没做完");

        finish(&p, id, None, 20).await.unwrap();
        let o = outcome(&p, id).await.unwrap().unwrap();
        assert!(o.done && o.error.is_none(), "成功 = done 且无错误");

        let id2 = enqueue(&p, a, "config_check", &serde_json::json!({}), 0).await.unwrap();
        take_pending(&p, 10).await.unwrap();
        finish(&p, id2, Some("json: unknown field \"outbonds\""), 20).await.unwrap();
        let o = outcome(&p, id2).await.unwrap().unwrap();
        assert_eq!(o.error.as_deref(), Some("json: unknown field \"outbonds\""), "错误要原样留着");

        // agent 被删 → 指令级联删掉。界面上要能说「指令不见了」而不是永远等着。
        crate::db::agent_repo::delete(&p, a).await.unwrap();
        assert!(outcome(&p, id).await.unwrap().is_none(), "指令没了就该是 None");
    }
}
