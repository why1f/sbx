//! 线性迁移 + `PRAGMA user_version`,框架照搬旧项目 `src/db/mod.rs`(DESIGN.md §6 / §9.1)。
//!
//! 两条不能改的做法(旧项目已经踩过坑):
//!   1. 迁移必须**在建池之前**用一条独立连接跑完;
//!   2. 每个版本的所有语句在**一个事务**里跑,不会停在「加了一半列」的中间态。

pub mod command_repo;
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
    include_str!("migrations/006_default_multiplier_two.sql"),
    include_str!("migrations/007_agent_commands.sql"),
    include_str!("migrations/008_agent_outbound_strategy.sql"),
    include_str!("migrations/009_agents_autoincrement.sql"),
    include_str!("migrations/010_agent_nic_accounting_mode.sql"),
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

    // 连接级 pragma **必须挂在 ConnectOptions 上**,不能建完池再
    // `execute(&pool)` 发一遍 —— 那样只会作用在池随手给出的**那一条**连接上,
    // 其余连接各自保持默认值。
    //
    // `foreign_keys` 默认是 **OFF**,而这个库有四张表靠 `ON DELETE CASCADE`
    // 清理(agent_nic_traffic / nodes / user_nic_bindings / agent_commands)。
    // 早先那种写法的后果:删 agent 时若这条 DELETE 落在没开外键的连接上,
    // 级联不发生,残留数据留在库里 —— 而 SQLite 的 `INTEGER PRIMARY KEY`
    // 没有 AUTOINCREMENT 会**复用被删掉的 id**,于是下一台新加的机器
    // 一上来就顶着上一台的流量和节点(v0.4.9 修的就是这个)。
    //
    // `journal_mode` / `synchronous` 是库级的(写进文件头),挂在这里也无妨,
    // 顺带省掉建池后那几条语句。
    let opts = SqliteConnectOptions::from_str(&url)?
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .with_context(|| format!("打开数据库 {} 失败", db_path))?;
    Ok(pool)
}

/// 线性迁移:每个版本的所有语句在一个事务里跑,要么整版生效要么整版回滚。
/// ALTER TABLE 的重复列错误单独放行,兼容历史上被手工 ALTER 过的库。
///
/// **迁移连接上外键是关的。** 这是重建表(建新表、搬数据、丢旧表、改名)
/// 唯一可行的前提:外键开着时 `DROP TABLE agents` 会先做一次隐式
/// DELETE,顺着 `ON DELETE CASCADE` 把 nodes / agent_nic_traffic / user_traffic
/// 全部清空 —— 一次改属性的迁移会把所有节点和流量删干净(009 就是这种迁移)。
///
/// 在这里关而不在 SQL 里关,是因为 `PRAGMA foreign_keys` **在事务内是空操作**,
/// 而每一版都跑在事务里;写在 SQL 里只会让人以为关掉了。
///
/// 只影响这条临时连接,业务池那边照常开着(见 `init_pool`)。
async fn migrate(url: &str) -> Result<()> {
    let mut conn = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .foreign_keys(false)
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

    /// 009 重建 `agents` 表 —— 重建后的列必须与「从零建库」**完全一致**。
    ///
    /// 重建表的经典事故就是漏列:009 的 CREATE 是照着 001 抄的,而 004 后来
    /// 又 ALTER 加了六个主机指标列。漏掉它们时,库能建起来、迁移也不报错,
    /// 只有跑到那条 SQL 才炸「no such column: cpu_pct」。
    ///
    /// 所以这里不逐个点名列,而是拿**升级上来的库**和**全新建的库**对比整份
    /// 列清单 —— 以后再往 agents 上加列,忘了同步 009 的话这条会直接失败。
    #[tokio::test]
    async fn the_rebuilt_agents_table_matches_a_fresh_one() {
        async fn columns(pool: &SqlitePool) -> Vec<(String, String)> {
            let rows = sqlx::query("PRAGMA table_info(agents)").fetch_all(pool).await.unwrap();
            let mut cols: Vec<(String, String)> =
                rows.iter().map(|r| (r.get::<String, _>(1), r.get::<String, _>(2))).collect();
            cols.sort();
            cols
        }

        // 一路迁上来的库(009 重建过)。
        let upgraded = init_pool(&tmp_db()).await.unwrap();
        // 全新建的库 —— 同样跑完 009,但表里没有历史数据。
        let fresh = init_pool(&tmp_db()).await.unwrap();

        assert_eq!(columns(&upgraded).await, columns(&fresh).await);
        // 再钉一遍那几个最容易漏的:光比「两边一致」的话,两边一起漏了也发现不了。
        let names: Vec<String> = columns(&fresh).await.into_iter().map(|(n, _)| n).collect();
        for must in
            [
                "cpu_pct",
                "mem_used",
                "mem_total",
                "load1",
                "uptime_secs",
                "sysinfo_at",
                "outbound_strategy",
                "nic_accounting_mode",
            ]
        {
            assert!(names.contains(&must.to_string()), "重建后的 agents 缺列 {must}:{names:?}");
        }
    }

    /// 回归(v0.4.9):`agents.id` **不能复用**。
    ///
    /// 现场表现是「TUI 里删掉一台机器、重新添加,新机器还没装 agent,
    /// 页面上流量就已经在涨」—— 因为被删的 id 被重发给了新机器,而 daemon
    /// 里那条旧 WebSocket 还活着,继续往这个 id 上报。
    #[tokio::test]
    async fn a_deleted_agent_id_is_never_handed_out_again() {
        let pool = init_pool(&tmp_db()).await.unwrap();
        let (first, _) = agent_repo::create(&pool, "old", 0).await.unwrap();
        agent_repo::delete(&pool, first).await.unwrap();

        let (second, _) = agent_repo::create(&pool, "new", 0).await.unwrap();
        assert_ne!(second, first, "删掉的 id 又发出去了 —— 新机器会顶着旧机器的流量");
    }

    /// 009 重建 `agents` 表。**外键必须在迁移连接上关掉** ——
    /// 开着的话 `DROP TABLE agents` 会顺着 `ON DELETE CASCADE` 做一次隐式 DELETE,
    /// 把 nodes / 流量全部清空:一次「改 id 属性」的迁移会删掉用户所有数据。
    ///
    /// 这里造出 009 之前的状态(带数据),跑完整套迁移,再确认数据还在。
    #[tokio::test]
    async fn rebuilding_agents_keeps_nodes_and_traffic() {
        let path = tmp_db();
        let url = format!("sqlite://{}?mode=rwc", path);
        {
            // 先迁到 008 —— 也就是 009 要重建的那个 agents 表。
            let pool = init_pool(&path).await.unwrap();
            let (agent, _) = agent_repo::create(&pool, "keep-me", 0).await.unwrap();
            sqlx::query(
                "INSERT INTO nodes (agent_id, tag, protocol, listen_port, params_json)
                 VALUES (?, 'n1', 'vless', 443, '{}')",
            )
            .bind(agent)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO agent_nic_traffic
                   (agent_id, boot_id, cycle_rx, cycle_tx, cycle_start, updated_at)
                 VALUES (?, 'b', 111, 222, 0, 1)",
            )
            .bind(agent)
            .execute(&pool)
            .await
            .unwrap();
            // 把版本退回 008,让 009 在有数据的库上重跑一次。
            sqlx::query("PRAGMA user_version = 8").execute(&pool).await.unwrap();
            pool.close().await;
        }

        migrate(&url).await.unwrap();

        let pool = init_pool(&path).await.unwrap();
        let nodes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes").fetch_one(&pool).await.unwrap();
        assert_eq!(nodes, 1, "重建 agents 表把节点一起删了");
        let rx: i64 = sqlx::query_scalar("SELECT cycle_rx FROM agent_nic_traffic")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rx, 111, "重建 agents 表把流量一起删了");
        let agents: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents").fetch_one(&pool).await.unwrap();
        assert_eq!(agents, 1, "agent 自己也得留着");
        let mode: String =
            sqlx::query_scalar("SELECT nic_accounting_mode FROM agents WHERE name = 'keep-me'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(mode, "sum", "迁移 010 要给旧 agent 保持原来的 RX+TX 口径");
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

    /// 探针:把池里**每一条**连接上的 `PRAGMA foreign_keys` 实际值查出来。
    ///
    /// 留着它是因为这个开关是连接级的,而排查时唯一可信的是实测值 ——
    /// 靠读文档猜 sqlx 的默认行为容易猜错。
    #[tokio::test]
    async fn foreign_keys_is_on_for_every_connection_in_the_pool() {
        let pool = init_pool(&tmp_db()).await.unwrap();
        // 同时占住 4 条连接(= max_connections),逐条问它自己的 pragma。
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(pool.acquire().await.unwrap());
        }
        for (i, conn) in held.iter_mut().enumerate() {
            let on: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            assert_eq!(on, 1, "第 {i} 条连接上的 foreign_keys 是关的");
        }
    }

    /// **池里的每一条连接都要开着外键。**
    ///
    /// `foreign_keys` 是**连接级**开关且默认 OFF。早先的写法是建完池再
    /// `PRAGMA foreign_keys=ON` `execute(&pool)` 发一遍 —— 那只作用在池
    /// 随手给出的那一条连接上,其余三条仍是关的。
    ///
    /// 后果不是报错,是**静默不级联**:删 agent 时若那条 DELETE 落在没开外键的
    /// 连接上,`agent_nic_traffic` 等四张表的行会留下来。而 SQLite 的
    /// `INTEGER PRIMARY KEY` 不带 AUTOINCREMENT 会**复用被删掉的 id**,
    /// 于是下一台新加的机器一上来就顶着上一台的流量(v0.4.9 修的就是这个:
    /// 「刚加的 agent 还没装就在涨流量」)。
    ///
    /// 这条测试**并发**发起远多于池容量的查询,逼 sqlx 把每条连接都用上;
    /// 只要有一条没开外键,插入孤儿行就会成功,断言随即失败。
    #[tokio::test]
    async fn every_pooled_connection_enforces_foreign_keys() {
        let path =
            std::env::temp_dir().join(format!("sbx-fk-all-conns-{}.db", uuid::Uuid::new_v4()));
        let pool = init_pool(path.to_string_lossy().as_ref()).await.unwrap();

        // 32 个并发探针 ≫ max_connections(4),每条连接都会被轮到。
        // 每个探针插一行指向不存在的 agent —— 外键开着就必须失败。
        let probes = (0..32).map(|i| {
            let pool = pool.clone();
            tokio::spawn(async move {
                sqlx::query(
                    "INSERT INTO nodes (agent_id, tag, protocol, listen_port, params_json)
                     VALUES (99999, ?, 'trojan', 443, '{}')",
                )
                .bind(format!("orphan-{i}"))
                .execute(&pool)
                .await
                .is_ok()
            })
        });
        let accepted = futures_util::future::join_all(probes)
            .await
            .into_iter()
            .filter(|r| *r.as_ref().unwrap_or(&false))
            .count();
        assert_eq!(accepted, 0, "有连接没开外键:{accepted} 个孤儿节点被插进去了");

        // 反过来确认这个探针本身是有效的 —— agent 存在时同样的插入要成功,
        // 否则上面的「全部失败」可能只是因为 SQL 写错了。
        sqlx::query(
            "INSERT INTO agents (id, name, token_hash, token_prefix, status, created_at)
             VALUES (1, 'a', 'h', 'p', 'never', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO nodes (agent_id, tag, protocol, listen_port, params_json)
             VALUES (1, 'real', 'trojan', 443, '{}')",
        )
        .execute(&pool)
        .await
        .expect("agent 存在时这一插入该成功,否则上面的断言是假阳性");
    }

    /// 删 agent 要把**网卡流量**也带走。
    ///
    /// 漏掉它的表现最迷惑:因为 SQLite 会复用被删掉的 id,下一台新加的机器
    /// 会继承这一行,于是「还没装 agent,服务管理页上就开始涨流量」。
    #[tokio::test]
    async fn deleting_an_agent_clears_its_nic_traffic() {
        let path = std::env::temp_dir().join(format!("sbx-fk-nic-{}.db", uuid::Uuid::new_v4()));
        let pool = init_pool(path.to_string_lossy().as_ref()).await.unwrap();

        sqlx::query(
            "INSERT INTO agents (id, name, token_hash, token_prefix, status, created_at)
             VALUES (1, 'azure', 'h', 'p', 'online', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_nic_traffic
               (agent_id, boot_id, last_rx, last_tx, cycle_rx, cycle_tx, cycle_start, updated_at)
             VALUES (1, 'b', 1, 2, 111, 222, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        crate::db::agent_repo::delete(&pool, 1).await.unwrap();

        let n: i64 = sqlx::query("SELECT COUNT(*) FROM agent_nic_traffic")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 0, "删除 agent 后网卡流量必须一起清掉");

        // 复用 id=1 建一台新的:它必须是干净的。
        let (id, _) = crate::db::agent_repo::create(&pool, "new-box", 0).await.unwrap();
        let rev: i64 = sqlx::query_scalar("SELECT config_revision FROM agents WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rev, 0, "新建 agent 的 config_revision 该从 0 起");
        let n: i64 = sqlx::query("SELECT COUNT(*) FROM agent_nic_traffic WHERE agent_id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(n, 0, "新 agent 不该继承上一台的流量");
    }
}
