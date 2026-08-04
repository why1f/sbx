//! 线性迁移 + `PRAGMA user_version`,框架照搬旧项目 `src/db/mod.rs`(DESIGN.md §6 / §9.1)。
//!
//! 两条不能改的做法(旧项目已经踩过坑):
//!   1. 迁移必须**在建池之前**用一条独立连接跑完;
//!   2. 每个版本的所有语句在**一个事务**里跑,不会停在「加了一半列」的中间态。

pub mod agent_repo;
pub mod node_repo;

use anyhow::{Context, Result};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    ConnectOptions, Connection, Row, SqlitePool,
};
use std::str::FromStr;

/// 迁移脚本按 user_version 顺序执行;索引 i 对应目标版本 i+1。
///
/// **只追加,不修改已发布的脚本。** 改动已发布脚本对已经迁移过的库不会重跑,
/// 只会让新库与旧库的 schema 悄悄分叉。
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_init.sql"),
    include_str!("migrations/002_reset_cycle.sql"),
    include_str!("migrations/003_telegram.sql"),
    include_str!("migrations/004_agent_host_metrics.sql"),
    include_str!("migrations/005_user_nic_bindings.sql"),
];

/// 当前程序期望的 schema 版本(= 迁移脚本数量),供 doctor 比对实际库版本。
pub fn schema_version() -> i64 {
    MIGRATIONS.len() as i64
}

pub async fn init_pool(db_path: &str) -> Result<SqlitePool> {
    let url = format!("sqlite://{}?mode=rwc", db_path);

    // 迁移在**建池之前**、用一条独立连接跑完。
    // 若放在池上跑,先建好的连接可能缓存了 ALTER TABLE 之前的表结构,
    // 后续 `SELECT *` 会拿到列数与新结构不符的行(表现为解码时下标越界)。
    migrate(&url)
        .await
        .with_context(|| format!("迁移数据库 {} 失败", db_path))?;

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .with_context(|| format!("打开数据库 {} 失败", db_path))?;
    for pragma in [
        "PRAGMA journal_mode=WAL",
        "PRAGMA synchronous=NORMAL",
        "PRAGMA busy_timeout=5000",
        "PRAGMA foreign_keys=ON",
    ] {
        sqlx::query(pragma).execute(&pool).await?;
    }
    Ok(pool)
}

/// 线性迁移:每个版本的所有语句在一个事务里跑,要么整版生效要么整版回滚。
/// ALTER TABLE 的重复列错误单独放行,兼容历史上被手工 ALTER 过的库。
async fn migrate(url: &str) -> Result<()> {
    let mut conn = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .disable_statement_logging()
        .connect()
        .await?;
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&mut conn)
        .await?;
    sqlx::query("PRAGMA busy_timeout=5000")
        .execute(&mut conn)
        .await?;

    let current: i64 = sqlx::query("PRAGMA user_version")
        .fetch_one(&mut conn)
        .await?
        .try_get(0)
        .unwrap_or(0);

    for (idx, script) in MIGRATIONS.iter().enumerate() {
        let target = idx as i64 + 1;
        if current >= target {
            continue;
        }
        let mut tx = conn.begin().await?;
        for stmt in split_sql(script) {
            if let Err(e) = sqlx::query(&stmt).execute(&mut *tx).await {
                if is_duplicate_column(&e) {
                    continue;
                }
                return Err(anyhow::Error::new(e).context(format!("迁移 v{} 失败: {}", target, stmt)));
            }
        }
        // PRAGMA user_version 不接受绑定参数,target 来自枚举下标,非外部输入。
        sqlx::query(&format!("PRAGMA user_version = {}", target))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    // 迁移本身已提交,关闭连接失败无关紧要(连接 drop 时也会释放),
    // 不该因此让整个 init_pool 失败。
    if let Err(e) = conn.close().await {
        tracing::warn!(error = %e, "关闭迁移连接失败(迁移已提交,可忽略)");
    }
    Ok(())
}

/// 旧库可能已被手工 ALTER 过同名列,这类错误视为幂等成功。
fn is_duplicate_column(e: &sqlx::Error) -> bool {
    e.to_string().contains("duplicate column")
}

/// 按 `;` 切分成单条语句,先剥掉 `--` 行注释。
///
/// 旧项目的版本是 `src.split(';')` 再丢掉纯注释块,那有一个隐藏的坑:
/// **注释里只要出现一个 `;`,语句就会被切在中间**,报出来的错是
/// `incomplete input` —— 与真正的语法错误长得一样,排查方向完全被带偏。
/// 中文注释里写分号是很自然的事(「NULL = 无需重置;否则 1..31」),
/// 所以这里改成先剥注释再切,而不是把这条约定留给写迁移的人去记。
///
/// 单引号字符串内的 `--` 与 `;` 不被当作注释/分隔符,`''` 按 SQL 的转义规则处理。
fn split_sql(src: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut cur = String::new();
    let mut it = src.chars().peekable();
    let mut in_string = false;

    while let Some(c) = it.next() {
        if in_string {
            cur.push(c);
            if c == '\'' {
                // SQL 里 '' 表示一个字面单引号,不是字符串结束。
                if it.peek() == Some(&'\'') {
                    cur.push(it.next().unwrap());
                } else {
                    in_string = false;
                }
            }
            continue;
        }
        match c {
            '\'' => {
                in_string = true;
                cur.push(c);
            }
            // `--` 行注释:丢弃到行尾(保留换行,免得把两行代码粘成一行)
            '-' if it.peek() == Some(&'-') => {
                for n in it.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                cur.push('\n');
            }
            ';' => flush(&mut stmts, &mut cur),
            _ => cur.push(c),
        }
    }
    flush(&mut stmts, &mut cur);
    stmts
}

fn flush(stmts: &mut Vec<String>, cur: &mut String) {
    let s = cur.trim();
    if !s.is_empty() {
        stmts.push(s.to_string());
    }
    cur.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> String {
        std::env::temp_dir()
            .join(format!("sbx-test-{}.db", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn split_sql_drops_comment_only_chunks() {
        let src = "-- 注释\nCREATE TABLE t (a INTEGER);\n-- 尾部注释\n";
        let stmts = split_sql(src);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("CREATE TABLE"));
    }

    /// 回归:注释里的分号**不能**把语句切成两半。
    /// 踩过一次——中文注释「NULL = 无需重置;否则 1..31」让整个 001_init 报
    /// `incomplete input`,而那个错误看起来像语法错误,排查方向完全被带偏。
    #[test]
    fn split_sql_ignores_semicolons_inside_comments() {
        let src = "CREATE TABLE t (\n  a INTEGER,  -- 可空;否则 1..31\n  b TEXT\n);";
        let stmts = split_sql(src);
        assert_eq!(stmts.len(), 1, "得到: {stmts:?}");
        assert!(stmts[0].contains("a INTEGER"));
        assert!(stmts[0].contains("b TEXT"));
        assert!(!stmts[0].contains("否则"), "注释应被剥掉");
    }

    /// 字符串字面量里的 `--` 和 `;` 不是注释/分隔符。
    #[test]
    fn split_sql_respects_string_literals() {
        let src = "INSERT INTO t VALUES ('a--b;c');\nINSERT INTO t VALUES ('x');";
        let stmts = split_sql(src);
        assert_eq!(stmts.len(), 2, "得到: {stmts:?}");
        assert!(stmts[0].contains("'a--b;c'"), "字符串内容不该被改动: {}", stmts[0]);
    }

    /// SQL 的 `''` 转义:不该被当成字符串结束。
    #[test]
    fn split_sql_handles_escaped_quotes() {
        let src = "INSERT INTO t VALUES ('it''s; fine');\nSELECT 1;";
        let stmts = split_sql(src);
        assert_eq!(stmts.len(), 2, "得到: {stmts:?}");
        assert!(stmts[0].contains("'it''s; fine'"));
    }

    #[tokio::test]
    async fn migrations_run_to_latest_and_are_idempotent() {
        let path = tmp_db();
        let url = format!("sqlite://{}?mode=rwc", path);
        let pool = init_pool(&path).await.unwrap();
        let version: i64 = sqlx::query("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get(0)
            .unwrap();
        assert_eq!(version, schema_version());
        // 再跑一次不应报错(幂等)
        migrate(&url).await.unwrap();
    }

    /// 跨 agent 求和视图(§13.1 第二条)。
    ///
    /// 造 2 个 agent × 3 个节点,验证 `user_traffic_total` 等于逐节点手算之和,
    /// **且无流量记录的用户也出现(值为 0)**——后者是 §6.2 里 LEFT JOIN 的存在理由。
    #[tokio::test]
    async fn user_traffic_total_sums_across_agents_and_keeps_users_with_no_traffic() {
        let pool = init_pool(&tmp_db()).await.unwrap();

        for (id, name) in [(1, "agent-a"), (2, "agent-b")] {
            sqlx::query(
                "INSERT INTO agents (id, name, token_hash, token_prefix, created_at)
                 VALUES (?, ?, 'h', 'pfx', 0)",
            )
            .bind(id)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
        }
        // agent-a 上两个节点,agent-b 上一个 —— 覆盖「同一 agent 多节点」这个
        // 只报 name 不报 tag 就会塌掉的场景(§4.3)
        for (id, agent, tag) in [(1, 1, "vless-in"), (2, 1, "trojan-in"), (3, 2, "vless-in")] {
            sqlx::query(
                "INSERT INTO nodes (id, agent_id, tag, protocol, listen_port, params_json)
                 VALUES (?, ?, ?, 'vless-reality', 443, '{}')",
            )
            .bind(id)
            .bind(agent)
            .bind(tag)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (id, name, tok) in [(1, "alice", "t1"), (2, "bob", "t2")] {
            sqlx::query(
                "INSERT INTO users (id, name, uuid, password, sub_token, created_at)
                 VALUES (?, ?, 'u', 'p', ?, 0)",
            )
            .bind(id)
            .bind(name)
            .bind(tok)
            .execute(&pool)
            .await
            .unwrap();
        }
        // alice 拿到三个节点的流量;bob 一条记录都没有
        for (node, up, down) in [(1, 100, 200), (2, 30, 40), (3, 1, 2)] {
            sqlx::query(
                "INSERT INTO user_traffic
                   (user_id, node_id, cycle_up, cycle_down, total_up, total_down, updated_at)
                 VALUES (1, ?, ?, ?, ?, ?, 0)",
            )
            .bind(node)
            .bind(up)
            .bind(down)
            .bind(up)
            .bind(down)
            .execute(&pool)
            .await
            .unwrap();
        }

        let rows = sqlx::query("SELECT name, cycle_up, cycle_down FROM user_traffic_total ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "无流量的用户也必须出现在视图里");

        let alice = &rows[0];
        assert_eq!(alice.get::<String, _>("name"), "alice");
        assert_eq!(alice.get::<i64, _>("cycle_up"), 131, "100+30+1,跨 agent 求和");
        assert_eq!(alice.get::<i64, _>("cycle_down"), 242, "200+40+2");

        let bob = &rows[1];
        assert_eq!(bob.get::<String, _>("name"), "bob");
        assert_eq!(bob.get::<i64, _>("cycle_up"), 0, "无记录应为 0,不是 NULL,也不能漏行");
    }

    /// 删除 agent 应级联删掉它的节点,并顺带清掉 user_nodes / user_traffic 的行(§8.1)。
    #[tokio::test]
    async fn deleting_agent_cascades_to_nodes_and_assignments() {
        let pool = init_pool(&tmp_db()).await.unwrap();
        sqlx::query(
            "INSERT INTO agents (id, name, token_hash, token_prefix, created_at)
             VALUES (1, 'a', 'h', 'p', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO nodes (id, agent_id, tag, protocol, listen_port, params_json)
             VALUES (1, 1, 'in', 'trojan', 443, '{}')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO users (id, name, uuid, password, sub_token, created_at)
             VALUES (1, 'alice', 'u', 'p', 't', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO user_nodes (user_id, node_id) VALUES (1, 1)")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("DELETE FROM agents WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();

        for (table, what) in [("nodes", "节点"), ("user_nodes", "用户-节点分配")] {
            let n: i64 = sqlx::query(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap()
                .get(0);
            assert_eq!(n, 0, "删除 agent 后应级联清掉{what}");
        }
        // 用户本身不该被删
        let n: i64 = sqlx::query("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 1, "删除 agent 不应删掉用户");
    }
}
