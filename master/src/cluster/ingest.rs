//! 上报入库(DESIGN.md §3 `cluster/ingest.rs`)。
//!
//! 把 `stats.report` / `sysinfo.report` 落进库,epoch 与增量交给 `delta`(§5.2)处理。
//!
//! 三条必须守住的语义:
//!
//!   1. **未知条目直接丢弃**(§4.3)。tracker 的计数表只增不减,所以用户/节点在主控侧
//!      被删除后,agent 仍会继续上报它们,直到 agent 进程重启。不建行、不报错、不记 warn
//!      ——否则每 30s 刷一条日志。
//!   2. **epoch 变更必须可见**(§5.4)。写一条 `agent_events`,不静默吞掉。
//!      因为 §5.2 已把 epoch 变更压到只剩进程/机器重启,这条记录是低频且高信息量的。
//!   3. **一次上报一个事务**。半写入的状态会让 `last_*` 与 `cycle_*` 不一致,
//!      而这种不一致无法从数据本身看出来——下次上报会把它当成正常基线继续累加。

use crate::cluster::delta;
use anyhow::Result;
use sbx_shared::{StatsReport, SysinfoReport};
use sqlx::SqlitePool;

/// 一次 `stats.report` 的入库结果,供日志与 TUI 使用。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StatsOutcome {
    /// 成功入账的条目数。
    pub applied: usize,
    /// 因为找不到 `(user, tag)` 对应的行而丢弃的条目数(§4.3,正常现象)。
    pub unknown: usize,
    /// 是否发生了 epoch 变更(§5.4)。
    pub epoch_changed: bool,
}

/// 落库一次用户流量上报。
///
/// `(name, tag)` → `(user_id, node_id)` 的映射靠一次 JOIN 查出来:
/// 用户名唯一(`users.name UNIQUE`),节点在 agent 内按 tag 唯一(`UNIQUE(agent_id, tag)`),
/// 且该用户必须**确实被分配了**这个节点(`user_nodes`)——三个条件都满足才入账。
///
/// 最后一条是刻意的:agent 上报了一个「主控没给这个用户分配过」的 (user, tag),
/// 说明主控与 agent 的配置不同步(通常是 `config.apply` 还没到)。
/// 这时候建账会造出一条主控视角里不该存在的记录,所以宁可丢弃等下次。
pub async fn ingest_stats(
    pool: &SqlitePool,
    agent_id: i64,
    r: &StatsReport,
    now: i64,
) -> Result<StatsOutcome> {
    let mut out = StatsOutcome::default();

    // BEGIN IMMEDIATE 作跨进程写锁(§6.3):daemon 与 TUI 会同时在跑。
    //
    // 必须用 `begin_with`:`pool.begin()` 自己已经发了一条 `BEGIN`(DEFERRED),
    // 之后再执行 `BEGIN IMMEDIATE` 只会得到 "cannot start a transaction within a
    // transaction" —— 以前这里就是那样写的,错误被 `.ok()` 吞掉,事务实际上一直是
    // DEFERRED:先读快照、再在 UPSERT 时升级成写锁,TUI 恰好在中间提交时得到的是
    // SQLITE_BUSY_SNAPSHOT(busy_timeout 对它不生效),整条上报失败等下一轮。
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    for u in &r.users {
        let ids: Option<(i64, i64)> = sqlx::query_as(
            "SELECT u.id, n.id
               FROM users u
               JOIN user_nodes un ON un.user_id = u.id
               JOIN nodes n       ON n.id = un.node_id
              WHERE u.name = ? AND n.tag = ? AND n.agent_id = ?",
        )
        .bind(&u.name)
        .bind(&u.tag)
        .bind(agent_id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((user_id, node_id)) = ids else {
            // §4.3:安静丢弃。这是删号/删节点后的正常现象。
            out.unknown += 1;
            continue;
        };

        let prev: Option<(Option<String>, i64, i64)> = sqlx::query_as(
            "SELECT counter_epoch, last_up, last_down FROM user_traffic
              WHERE user_id = ? AND node_id = ?",
        )
        .bind(user_id)
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;

        let (last_epoch, last_up, last_down) = match &prev {
            Some((e, up, down)) => (e.as_deref(), *up, *down),
            None => (None, 0, 0),
        };

        // 计数器不可能是负的。收到负数说明 agent 有 bug 或数据被动过手脚:
        // 若原样存进 `last_*`,下一次上报一个 0 就会算出 `0 - (-X) = X` 的增量,
        // 凭空记出 X 字节。在这里钳到 0,存的和算的都是钳过的值。
        let (up, down) = (u.up.max(0), u.down.max(0));
        let d = delta::compute_pair(last_epoch, last_up, last_down, &r.counter_epoch, up, down);
        out.epoch_changed |= d.epoch_changed;

        // UPSERT:第一次见到这个 (user, node) 就建行,之后累加。
        // `last_*` 存的是**本次上报的原始值**(不是增量),下次据此算 delta。
        sqlx::query(
            "INSERT INTO user_traffic
               (user_id, node_id, counter_epoch, last_up, last_down,
                cycle_up, cycle_down, total_up, total_down, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id, node_id) DO UPDATE SET
               counter_epoch = excluded.counter_epoch,
               last_up       = excluded.last_up,
               last_down     = excluded.last_down,
               cycle_up      = user_traffic.cycle_up   + excluded.cycle_up,
               cycle_down    = user_traffic.cycle_down + excluded.cycle_down,
               total_up      = user_traffic.total_up   + excluded.total_up,
               total_down    = user_traffic.total_down + excluded.total_down,
               updated_at    = excluded.updated_at",
        )
        .bind(user_id)
        .bind(node_id)
        .bind(&r.counter_epoch)
        .bind(up)
        .bind(down)
        .bind(d.up)
        .bind(d.down)
        .bind(d.up)
        .bind(d.down)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        out.applied += 1;
    }

    // §5.4:epoch 变更必须留痕,不静默吞掉。
    if out.epoch_changed {
        sqlx::query("INSERT INTO agent_events (agent_id, kind, message, at) VALUES (?, ?, ?, ?)")
            .bind(agent_id)
            .bind("counter_reset")
            .bind(format!(
                "用户流量计数器已重置(counter_epoch={});通常意味着 agent 进程重启",
                r.counter_epoch
            ))
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(out)
}

/// 一次 `sysinfo.report` 的入库结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SysinfoOutcome {
    pub epoch_changed: bool,
    /// 本次的网卡增量,供主控在内存里算网速(§8.2,**不落库**)。
    pub rx_delta: i64,
    pub tx_delta: i64,
}

/// 落库一次网卡流量上报(§6.4)。
///
/// 与用户流量走**完全独立**的一套:epoch 是 `boot_id`,配额在 `agents.nic_quota_bytes`。
/// 口径是整机物理网卡 RX+TX,与 `user_traffic` 不是同一套数字(§7.2 的警告)。
pub async fn ingest_sysinfo(
    pool: &SqlitePool,
    agent_id: i64,
    r: &SysinfoReport,
    now: i64,
) -> Result<SysinfoOutcome> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?; // 理由见 ingest_stats

    let prev: Option<(Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT boot_id, last_rx, last_tx FROM agent_nic_traffic WHERE agent_id = ?",
    )
    .bind(agent_id)
    .fetch_optional(&mut *tx)
    .await?;

    let (last_boot, last_rx, last_tx) = match &prev {
        Some((b, rx, tx_)) => (b.as_deref(), *rx, *tx_),
        None => (None, 0, 0),
    };

    // **第一次见到这台 agent 时只建基线,不入账。**
    //
    // 通用的 epoch 规则(§5.2)是「epoch 变了 → delta = new」,那对**用户流量**是对的:
    // agent 的 tracker 从进程启动才开始数,new 就是这段时间真实发生的量。
    // 但网卡计数读的是 `/proc/net/dev`,它从**机器开机**就在数 —— 一台已经跑了
    // 三个月的机器接进来,第一次上报会把这三个月的流量整个搬进本周期用量,
    // 界面上立刻显示「1.49 GB」甚至几百 GB,而这台机器在本集群里其实一个字节都没跑。
    //
    // 注意这**不影响 boot_id 变更**那条路:机器重启后计数器确实是从 0 开始的,
    // 那时 delta = new 才是对的。区别只在「有没有基线」,不在「epoch 变没变」。
    let first_contact = prev.is_none();
    // 负数钳到 0,理由同 ingest_stats。
    let (rx, tx_) = (r.nic.rx.max(0), r.nic.tx.max(0));
    let d = if first_contact {
        delta::PairDelta { up: 0, down: 0, epoch_changed: false }
    } else {
        delta::compute_pair(last_boot, last_rx, last_tx, &r.boot_id, rx, tx_)
    };

    sqlx::query(
        "INSERT INTO agent_nic_traffic
           (agent_id, boot_id, last_rx, last_tx, cycle_rx, cycle_tx, cycle_start, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(agent_id) DO UPDATE SET
           boot_id    = excluded.boot_id,
           last_rx    = excluded.last_rx,
           last_tx    = excluded.last_tx,
           cycle_rx   = agent_nic_traffic.cycle_rx + excluded.cycle_rx,
           cycle_tx   = agent_nic_traffic.cycle_tx + excluded.cycle_tx,
           updated_at = excluded.updated_at",
    )
    .bind(agent_id)
    .bind(&r.boot_id)
    .bind(rx)
    .bind(tx_)
    .bind(d.up)
    .bind(d.down)
    .bind(now) // cycle_start 只在建行时生效(ON CONFLICT 不更新它)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    if d.epoch_changed {
        sqlx::query("INSERT INTO agent_events (agent_id, kind, message, at) VALUES (?, ?, ?, ?)")
            .bind(agent_id)
            .bind("nic_counter_reset")
            .bind(format!("网卡计数器已重置(boot_id={});通常意味着机器重启", r.boot_id))
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }

    // 主机指标存**最新一次**就够(§8.0)。它们和网卡计数不一样:
    // 网卡是累加的账,必须一笔不漏;CPU / 内存 / load 只有「现在多少」有意义,
    // 留历史等于给一张每 30 秒 × N 台 agent 增长的表,而没有任何一处会去查它。
    //
    // `sysinfo_at` 是必须的:少了它,一台离线三天的机器会一直显示三天前那个
    // CPU 数字,看起来和在线的机器毫无区别。
    sqlx::query(
        "UPDATE agents SET cpu_pct = ?, mem_used = ?, mem_total = ?, load1 = ?,
                uptime_secs = ?, sysinfo_at = ? WHERE id = ?",
    )
    .bind(r.cpu_pct)
    .bind(r.mem_used)
    .bind(r.mem_total)
    .bind(r.load1)
    .bind(r.uptime_secs)
    .bind(now)
    .bind(agent_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(SysinfoOutcome { epoch_changed: d.epoch_changed, rx_delta: d.up, tx_delta: d.down })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sbx_shared::{NicCounter, UserCounter};

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-ingest-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    /// 造一台 agent、一个用户、以及分配给它的若干节点。
    /// `nodes` 是 (node_id, tag) 列表。
    async fn fixture(p: &SqlitePool, nodes: &[(i64, &str)]) -> i64 {
        let (agent_id, _) = crate::db::agent_repo::create(p, "a", 0).await.unwrap();
        sqlx::query("INSERT INTO users (id, name, uuid, password, sub_token, created_at) VALUES (1,'alice','u','p','t',0)")
            .execute(p).await.unwrap();
        for (nid, tag) in nodes {
            sqlx::query(
                "INSERT INTO nodes (id, agent_id, tag, protocol, listen_port, params_json)
                 VALUES (?, ?, ?, 'vless-reality', 443, '{}')",
            )
            .bind(nid)
            .bind(agent_id)
            .bind(tag)
            .execute(p)
            .await
            .unwrap();
            sqlx::query("INSERT INTO user_nodes (user_id, node_id) VALUES (1, ?)")
                .bind(nid)
                .execute(p)
                .await
                .unwrap();
        }
        agent_id
    }

    fn report(epoch: &str, users: &[(&str, &str, i64, i64)]) -> StatsReport {
        StatsReport {
            counter_epoch: epoch.into(),
            users: users
                .iter()
                .map(|(n, t, up, down)| UserCounter {
                    name: (*n).into(),
                    tag: (*t).into(),
                    up: *up,
                    down: *down,
                })
                .collect(),
        }
    }

    async fn traffic(p: &SqlitePool, node_id: i64) -> (i64, i64, i64, i64) {
        sqlx::query_as(
            "SELECT cycle_up, cycle_down, total_up, total_down FROM user_traffic WHERE node_id = ?",
        )
        .bind(node_id)
        .fetch_one(p)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn first_report_creates_row_with_full_value() {
        let p = pool().await;
        let a = fixture(&p, &[(1, "vless-in")]).await;

        let out = ingest_stats(&p, a, &report("e1", &[("alice", "vless-in", 100, 200)]), 10)
            .await
            .unwrap();
        assert_eq!(out.applied, 1);
        assert_eq!(out.unknown, 0);
        assert!(!out.epoch_changed, "首次上报不该算重置(§5.2)");

        assert_eq!(traffic(&p, 1).await, (100, 200, 100, 200));
    }

    #[tokio::test]
    async fn successive_reports_accumulate_deltas_not_raw_values() {
        let p = pool().await;
        let a = fixture(&p, &[(1, "vless-in")]).await;

        ingest_stats(&p, a, &report("e1", &[("alice", "vless-in", 100, 200)]), 10).await.unwrap();
        ingest_stats(&p, a, &report("e1", &[("alice", "vless-in", 350, 500)]), 20).await.unwrap();

        // 累计值应是最后一次的原始值,而不是 100+350
        assert_eq!(traffic(&p, 1).await, (350, 500, 350, 500));
    }

    /// §0.0(3) 的核心场景:同一用户在**同一台** agent 上有两个节点。
    /// 少了 tag 这两条会塌成一个数字(§4.3),这个测试就是那条口径的回归锚点。
    #[tokio::test]
    async fn two_nodes_on_the_same_agent_are_accounted_separately() {
        let p = pool().await;
        let a = fixture(&p, &[(1, "vless-in"), (2, "trojan-in")]).await;

        let out = ingest_stats(
            &p,
            a,
            &report("e1", &[("alice", "vless-in", 100, 200), ("alice", "trojan-in", 7, 9)]),
            10,
        )
        .await
        .unwrap();
        assert_eq!(out.applied, 2, "两个 tag 应各自入账");

        assert_eq!(traffic(&p, 1).await, (100, 200, 100, 200));
        assert_eq!(traffic(&p, 2).await, (7, 9, 7, 9));

        // 视图求和应等于两者之和
        let (cu, cd): (i64, i64) = sqlx::query_as(
            "SELECT cycle_up, cycle_down FROM user_traffic_total WHERE name = 'alice'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!((cu, cd), (107, 209));
    }

    /// §4.3:未知条目安静丢弃,不建行、不报错。
    #[tokio::test]
    async fn unknown_entries_are_dropped_without_error() {
        let p = pool().await;
        let a = fixture(&p, &[(1, "vless-in")]).await;

        let out = ingest_stats(
            &p,
            a,
            &report(
                "e1",
                &[
                    ("alice", "vless-in", 10, 20),
                    ("ghost", "vless-in", 999, 999), // 用户不存在(已删号)
                    ("alice", "deleted-in", 888, 888), // 节点不存在(已删节点)
                ],
            ),
            10,
        )
        .await
        .unwrap();

        assert_eq!(out.applied, 1);
        assert_eq!(out.unknown, 2, "两条未知条目应被丢弃");

        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM user_traffic").fetch_one(&p).await.unwrap();
        assert_eq!(rows, 1, "未知条目不该建行");
    }

    /// 用户存在、节点存在,但**没有分配关系**时也应丢弃——
    /// 那说明主控与 agent 的配置不同步,建账会造出主控视角里不该有的记录。
    #[tokio::test]
    async fn unassigned_node_is_dropped() {
        let p = pool().await;
        let a = fixture(&p, &[(1, "vless-in")]).await;
        // 加一个节点但**不**分配给 alice
        sqlx::query(
            "INSERT INTO nodes (id, agent_id, tag, protocol, listen_port, params_json)
             VALUES (2, ?, 'unassigned-in', 'trojan', 443, '{}')",
        )
        .bind(a)
        .execute(&p)
        .await
        .unwrap();

        let out = ingest_stats(&p, a, &report("e1", &[("alice", "unassigned-in", 50, 50)]), 10)
            .await
            .unwrap();
        assert_eq!(out.applied, 0);
        assert_eq!(out.unknown, 1);
    }

    /// 另一台 agent 上的同名 tag 不该被算到这台头上。
    #[tokio::test]
    async fn same_tag_on_a_different_agent_is_not_matched() {
        let p = pool().await;
        let a1 = fixture(&p, &[(1, "vless-in")]).await;
        let (a2, _) = crate::db::agent_repo::create(&p, "b", 0).await.unwrap();

        // 用 agent2 的身份上报 agent1 的 tag —— 不该匹配上
        let out =
            ingest_stats(&p, a2, &report("e1", &[("alice", "vless-in", 1, 1)]), 10).await.unwrap();
        assert_eq!(out.applied, 0, "tag 必须在 agent 内解析");
        assert_eq!(out.unknown, 1);

        // agent1 上报同一条是正常的
        let out =
            ingest_stats(&p, a1, &report("e1", &[("alice", "vless-in", 1, 1)]), 10).await.unwrap();
        assert_eq!(out.applied, 1);
    }

    /// §5.4:epoch 变更必须写审计记录,不静默吞掉。
    #[tokio::test]
    async fn epoch_change_writes_an_audit_event_and_does_not_lose_prior_total() {
        let p = pool().await;
        let a = fixture(&p, &[(1, "vless-in")]).await;

        ingest_stats(&p, a, &report("e1", &[("alice", "vless-in", 500, 600)]), 10).await.unwrap();

        // agent 被 kill -9 后重启:新 epoch,计数从小值重新开始
        let out =
            ingest_stats(&p, a, &report("e2", &[("alice", "vless-in", 30, 40)]), 20).await.unwrap();
        assert!(out.epoch_changed, "epoch 变更必须被报告");

        // 重启前的 500/600 保住,重启后的 30/40 计一次(§13.3)
        assert_eq!(traffic(&p, 1).await, (530, 640, 530, 640));

        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE kind = 'counter_reset'")
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(n, 1, "epoch 变更应写一条 agent_events(§5.4)");
    }

    /// 同 epoch 内的重复上报(值没变)应产生 0 增量,不能重复计。
    #[tokio::test]
    async fn duplicate_report_adds_nothing() {
        let p = pool().await;
        let a = fixture(&p, &[(1, "vless-in")]).await;
        let r = report("e1", &[("alice", "vless-in", 100, 100)]);

        ingest_stats(&p, a, &r, 10).await.unwrap();
        ingest_stats(&p, a, &r, 20).await.unwrap();
        assert_eq!(traffic(&p, 1).await, (100, 100, 100, 100), "重复上报不该翻倍");
    }

    // ── 网卡流量 ──

    fn sysinfo(boot: &str, rx: i64, tx: i64) -> SysinfoReport {
        SysinfoReport {
            boot_id: boot.into(),
            nic: NicCounter { rx, tx },
            cpu_pct: 12.5,
            mem_used: 1 << 30,
            mem_total: 4 << 30,
            load1: 0.4,
            uptime_secs: 3600,
        }
    }

    async fn nic(p: &SqlitePool, agent_id: i64) -> (i64, i64) {
        sqlx::query_as("SELECT cycle_rx, cycle_tx FROM agent_nic_traffic WHERE agent_id = ?")
            .bind(agent_id)
            .fetch_one(p)
            .await
            .unwrap()
    }

    /// 第一次见到一台 agent 时**只建基线,不入账**。
    ///
    /// 这条守的是一个很难事后发现的错:网卡计数读的是 `/proc/net/dev`,
    /// 从**机器开机**就在数。一台已经跑了三个月的机器接进来,若照通用 epoch 规则
    /// 「epoch 变了 → delta = new」办,第一次上报就会把那三个月整个搬进本周期用量 ——
    /// 界面上凭空多出几十上百 GB,而这台机器在本集群里一个字节都还没跑。
    #[tokio::test]
    async fn the_first_report_only_records_a_baseline() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();

        // 一台开机三个月的机器:网卡上已经有 500 GB。
        let huge = 500 * 1_073_741_824;
        let o = ingest_sysinfo(&p, a, &sysinfo("boot-1", huge, huge), 1_000).await.unwrap();
        assert_eq!((o.rx_delta, o.tx_delta), (0, 0), "首次上报不该入账");
        assert!(!o.epoch_changed, "首次上报不是「计数器重置」,不该刷审计记录");
        assert_eq!(nic(&p, a).await, (0, 0), "本周期用量应当从 0 开始");

        // 之后正常做差。
        ingest_sysinfo(&p, a, &sysinfo("boot-1", huge + 300, huge + 700), 1_030).await.unwrap();
        assert_eq!(nic(&p, a).await, (300, 700));
    }

    /// 但机器**重启**时 delta = new 仍然是对的 —— 那时计数器确实从 0 开始了。
    /// 区别在「有没有基线」,不在「epoch 变没变」,这两条不能混。
    #[tokio::test]
    async fn a_reboot_still_counts_the_whole_counter() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        ingest_sysinfo(&p, a, &sysinfo("boot-1", 1_000, 1_000), 10).await.unwrap();
        ingest_sysinfo(&p, a, &sysinfo("boot-1", 1_500, 1_500), 40).await.unwrap();
        assert_eq!(nic(&p, a).await, (500, 500));

        // 重启:计数器归零后又跑了 200/300。
        let o = ingest_sysinfo(&p, a, &sysinfo("boot-2", 200, 300), 70).await.unwrap();
        assert!(o.epoch_changed, "重启要留痕(§5.4)");
        assert_eq!(nic(&p, a).await, (700, 800), "重启后的量要接着累加");
    }

    /// 主机指标要落到 `agents` 上,并且**每次覆盖**成最新的一份。    ///
    /// 这几个值以前是被直接扔掉的:`sysinfo.report` 每 30 秒把 CPU / 内存 / load
    /// 送到主控,入库时只取了 nic 那一段 —— 于是概览页想显示「哪台机器忙成什么样」
    /// 时无米下锅,而数据其实早就在线上了。
    #[tokio::test]
    async fn host_metrics_are_persisted_and_overwritten() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();

        // 还没上报过:全是 NULL,不是 0。0 会被读成「这台机器闲着」。
        let before: (Option<f64>, Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT cpu_pct, mem_used, sysinfo_at FROM agents WHERE id = ?")
                .bind(a)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(before, (None, None, None));

        ingest_sysinfo(&p, a, &sysinfo("boot-1", 1, 1), 1_000).await.unwrap();
        type Metrics =
            (Option<f64>, Option<i64>, Option<i64>, Option<f64>, Option<i64>, Option<i64>);
        let row: Metrics = sqlx::query_as(
            "SELECT cpu_pct, mem_used, mem_total, load1, uptime_secs, sysinfo_at
               FROM agents WHERE id = ?",
        )
        .bind(a)
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(
            row,
            (Some(12.5), Some(1 << 30), Some(4 << 30), Some(0.4), Some(3600), Some(1_000))
        );

        // 第二次上报覆盖,不是累加 —— 这几个是「现在多少」,不是账。
        let mut r2 = sysinfo("boot-1", 2, 2);
        r2.cpu_pct = 91.0;
        r2.uptime_secs = 7200;
        ingest_sysinfo(&p, a, &r2, 2_000).await.unwrap();
        let row: (Option<f64>, Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT cpu_pct, uptime_secs, sysinfo_at FROM agents WHERE id = ?")
                .bind(a)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(row, (Some(91.0), Some(7200), Some(2_000)), "应当是覆盖,不是累加");
    }

    #[tokio::test]
    async fn nic_traffic_accumulates_and_reports_deltas_for_speed() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();

        // 第一次只建基线(见 the_first_report_only_records_a_baseline)。
        let o1 = ingest_sysinfo(&p, a, &sysinfo("boot-1", 1_000, 2_000), 10).await.unwrap();
        assert_eq!((o1.rx_delta, o1.tx_delta), (0, 0));
        assert!(!o1.epoch_changed);

        let o2 = ingest_sysinfo(&p, a, &sysinfo("boot-1", 1_500, 2_200), 40).await.unwrap();
        // 增量供内存里算网速用(§8.2),不落库
        assert_eq!((o2.rx_delta, o2.tx_delta), (500, 200));
        assert_eq!(nic(&p, a).await, (500, 200));
    }

    /// 机器重启:boot_id 变化 + 计数器清零(§5.2 / §6.4)。
    #[tokio::test]
    async fn reboot_changes_boot_id_and_writes_audit() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();

        // 建基线,再正常跑一段:重启前本周期已经有 (400, 300)。
        ingest_sysinfo(&p, a, &sysinfo("boot-1", 900_000, 800_000), 10).await.unwrap();
        ingest_sysinfo(&p, a, &sysinfo("boot-1", 900_400, 800_300), 20).await.unwrap();
        assert_eq!(nic(&p, a).await, (400, 300));

        let o = ingest_sysinfo(&p, a, &sysinfo("boot-2", 1_024, 2_048), 30).await.unwrap();

        assert!(o.epoch_changed, "boot_id 变化必须可见");
        assert_eq!(nic(&p, a).await, (1_424, 2_348), "重启前的量不能丢");

        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE kind = 'nic_counter_reset'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    /// `cycle_start` 只在建行时写入,后续上报不该动它——
    /// 否则月重置的周期起点会被每 30s 刷新一次,重置永远不会触发。
    #[tokio::test]
    async fn cycle_start_is_not_updated_on_subsequent_reports() {
        let p = pool().await;
        let (a, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();

        ingest_sysinfo(&p, a, &sysinfo("boot-1", 10, 10), 1_000).await.unwrap();
        ingest_sysinfo(&p, a, &sysinfo("boot-1", 20, 20), 9_999).await.unwrap();

        let start: i64 =
            sqlx::query_scalar("SELECT cycle_start FROM agent_nic_traffic WHERE agent_id = ?")
                .bind(a)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(start, 1_000, "cycle_start 不该被后续上报改写");
    }
}
