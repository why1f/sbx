//! TUI 的数据层:把库里的行读成视图模型,并在内存里算网速(DESIGN.md §8.2)。
//!
//! **网速为什么在这里算。** daemon 进程里 `ServerState.speed` 有一份采样,
//! 但 TUI 是**另一个进程**(`sbx tui` 与 `sbx daemon` 分开跑),读不到那块内存。
//! 好在算它需要的东西全在库里:`agent_nic_traffic` 的 `last_rx / last_tx / updated_at`。
//! 于是 TUI 自己按刷新节奏采样、在自己内存里做差 —— 与 §8.2 的口径一致
//! (相邻两次 `sysinfo.report` 的差除以间隔),而且**同样不落库**。
//!
//! 口径提醒照旧:上报周期 30s,所以这是 **30 秒平均值**,不是瞬时速率。
//! 它回答的是「这台机器现在忙不忙」,不是抓包级的速率监控。

use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct AgentRow {
    pub id: i64,
    pub name: String,
    pub token_prefix: String,
    pub status: String,
    pub agent_version: Option<String>,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
    pub nic_quota_bytes: Option<i64>,
    pub nic_reset_day: Option<i64>,
    pub cycle_rx: i64,
    pub cycle_tx: i64,
    /// None = 还没有两次可比的采样(刚打开 TUI,或 boot_id 刚变)。
    pub up_per_sec: Option<f64>,
    pub down_per_sec: Option<f64>,
    pub node_count: i64,
}

impl AgentRow {
    pub fn used(&self) -> i64 {
        self.cycle_rx.saturating_add(self.cycle_tx)
    }

    /// 用量占配额的比例。无配额返回 `None` —— 调用方**必须**显式处理这一态,
    /// 否则很容易画出一个永远 0% 的进度条(model/agent.rs 同样的理由)。
    pub fn quota_ratio(&self) -> Option<f64> {
        let q = self.nic_quota_bytes.filter(|q| *q > 0)?;
        Some((self.used() as f64 / q as f64).clamp(0.0, 1.0))
    }
}

#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: i64,
    pub agent_id: i64,
    pub agent_name: String,
    pub tag: String,
    pub protocol: String,
    pub listen_port: i64,
    pub user_count: i64,
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub auto_disabled: bool,
    pub quota_bytes: i64,
    pub cycle_up: i64,
    pub cycle_down: i64,
    pub traffic_multiplier: f64,
    pub expire_at: Option<i64>,
    pub node_count: i64,
    pub sub_token: String,
}

impl UserRow {
    /// 计费口径的已用量:含倍率。与 §6.3 的配额判定一致 ——
    /// 两处口径不同会让「列表显示 80%」和「已经被停用」同时出现。
    pub fn used(&self) -> i64 {
        let raw = self.cycle_up.saturating_add(self.cycle_down);
        (raw as f64 * self.traffic_multiplier.max(0.0)) as i64
    }

    pub fn quota_ratio(&self) -> Option<f64> {
        let q = if self.quota_bytes > 0 { self.quota_bytes } else { return None };
        Some((self.used() as f64 / q as f64).clamp(0.0, 1.0))
    }
}

/// 上一次看到的网卡累计值。做差用。
#[derive(Debug, Clone)]
struct NicSample {
    rx: i64,
    tx: i64,
    at: i64,
    /// 机器重启会让 `/proc/net/dev` 的计数器归零。boot_id 变了就丢弃上一次采样,
    /// 否则会算出一个巨大的负数或(取绝对值后)一个爆炸的速率(§5.2 同一个道理)。
    boot_id: Option<String>,
}

/// 跨刷新保留的采样表。**只在内存里。**
#[derive(Default)]
pub struct SpeedTracker {
    last: HashMap<i64, NicSample>,
}

impl SpeedTracker {
    /// 喂一次新采样,返回 `(上行速率, 下行速率)`。
    ///
    /// 返回 `None` 的三种情况,都该在界面上显示 `--` 而不是 0:
    ///   * 第一次见到这台 agent(没有基准);
    ///   * `updated_at` 没动(agent 还没上报新的,做差会得到一个假的 0);
    ///   * boot_id 变了(计数器归零)。
    fn feed(&mut self, agent_id: i64, s: NicSample) -> (Option<f64>, Option<f64>) {
        let prev = self.last.insert(agent_id, s.clone());
        let Some(prev) = prev else { return (None, None) };

        if prev.boot_id != s.boot_id {
            return (None, None); // 机器重启,计数器归零
        }
        let dt = s.at - prev.at;
        if dt <= 0 {
            return (None, None); // 还没有新的上报
        }
        // 计数器只增不减。出现回退说明上报乱序或库被手改过,当作没有可比数据。
        if s.rx < prev.rx || s.tx < prev.tx {
            return (None, None);
        }
        let up = (s.rx - prev.rx) as f64 / dt as f64;
        let down = (s.tx - prev.tx) as f64 / dt as f64;
        (Some(up), Some(down))
    }
}

/// `agents` + `agent_nic_traffic` 的一行。
type AgentQueryRow = (
    i64,            // id
    String,         // name
    String,         // token_prefix
    String,         // status
    Option<String>, // agent_version
    Option<String>, // ipv4
    Option<String>, // ipv6
    Option<i64>,    // nic_quota_bytes
    Option<i64>,    // nic_reset_day
    Option<String>, // boot_id
    i64,            // last_rx
    i64,            // last_tx
    i64,            // cycle_rx
    i64,            // cycle_tx
    i64,            // updated_at
    i64,            // node_count
);

pub async fn load_agents(pool: &SqlitePool, speed: &mut SpeedTracker) -> Result<Vec<AgentRow>> {
    // LEFT JOIN:从没上报过的 agent 也要出现在列表里(值为 0),
    // 否则「加了但还没连上」的那台会直接看不见,而那正是最需要排查的一台。
    let rows: Vec<AgentQueryRow> = sqlx::query_as(
        "SELECT a.id, a.name, a.token_prefix, a.status, a.agent_version, a.ipv4, a.ipv6,
                a.nic_quota_bytes, a.nic_reset_day,
                t.boot_id,
                COALESCE(t.last_rx, 0), COALESCE(t.last_tx, 0),
                COALESCE(t.cycle_rx, 0), COALESCE(t.cycle_tx, 0),
                COALESCE(t.updated_at, 0),
                (SELECT COUNT(*) FROM nodes n WHERE n.agent_id = a.id)
           FROM agents a
           LEFT JOIN agent_nic_traffic t ON t.agent_id = a.id
          ORDER BY a.id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let (up, down) = speed.feed(
                r.0,
                NicSample { rx: r.10, tx: r.11, at: r.14, boot_id: r.9.clone() },
            );
            AgentRow {
                id: r.0,
                name: r.1,
                token_prefix: r.2,
                status: r.3,
                agent_version: r.4,
                ipv4: r.5,
                ipv6: r.6,
                nic_quota_bytes: r.7,
                nic_reset_day: r.8,
                cycle_rx: r.12,
                cycle_tx: r.13,
                up_per_sec: up,
                down_per_sec: down,
                node_count: r.15,
            }
        })
        .collect())
}

pub async fn load_nodes(pool: &SqlitePool) -> Result<Vec<NodeRow>> {
    let rows: Vec<(i64, i64, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT n.id, n.agent_id, a.name, n.tag, n.protocol, n.listen_port,
                (SELECT COUNT(*) FROM user_nodes un WHERE un.node_id = n.id)
           FROM nodes n JOIN agents a ON a.id = n.agent_id
          ORDER BY n.agent_id, n.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, agent_id, agent_name, tag, protocol, listen_port, user_count)| NodeRow {
            id,
            agent_id,
            agent_name,
            tag,
            protocol,
            listen_port,
            user_count,
        })
        .collect())
}

type UserQueryRow = (i64, String, bool, bool, i64, f64, Option<i64>, String, i64, i64, i64);

pub async fn load_users(pool: &SqlitePool) -> Result<Vec<UserRow>> {
    let rows: Vec<UserQueryRow> = sqlx::query_as(
        "SELECT u.id, u.name, u.enabled, u.auto_disabled, u.quota_bytes,
                u.traffic_multiplier, u.expire_at, u.sub_token,
                COALESCE(t.cycle_up, 0), COALESCE(t.cycle_down, 0),
                (SELECT COUNT(*) FROM user_nodes un WHERE un.user_id = u.id)
           FROM users u
           LEFT JOIN user_traffic_total t ON t.user_id = u.id
          ORDER BY u.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| UserRow {
            id: r.0,
            name: r.1,
            enabled: r.2,
            auto_disabled: r.3,
            quota_bytes: r.4,
            traffic_multiplier: r.5,
            expire_at: r.6,
            sub_token: r.7,
            cycle_up: r.8,
            cycle_down: r.9,
            node_count: r.10,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(rx: i64, tx: i64, at: i64, boot: &str) -> NicSample {
        NicSample { rx, tx, at, boot_id: Some(boot.into()) }
    }

    /// 第一次见到某台 agent 时没有基准,必须是 `None`(界面显示 `--`)。
    /// 显示 0 会让人以为这台机器闲着,而实际上只是还没有第二个采样点。
    #[test]
    fn first_sample_has_no_speed() {
        let mut t = SpeedTracker::default();
        assert_eq!(t.feed(1, sample(0, 0, 100, "b")), (None, None));
    }

    #[test]
    fn speed_is_delta_over_elapsed_seconds() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(1_000, 2_000, 100, "b"));
        let (up, down) = t.feed(1, sample(4_000, 5_000, 130, "b"));
        // 30 秒里涨了 3000 / 3000 字节 → 100 B/s
        assert_eq!(up, Some(100.0));
        assert_eq!(down, Some(100.0));
    }

    /// 机器重启(boot_id 变)时计数器归零。做差会得到负数 ——
    /// 必须报 `None`,不能报一个爆炸的数字(§8.2)。
    #[test]
    fn boot_id_change_suppresses_the_reading() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(9_000_000, 9_000_000, 100, "old-boot"));
        assert_eq!(t.feed(1, sample(10, 10, 130, "new-boot")), (None, None));
        // 重启之后仍然能重新建立基准。
        let (up, _) = t.feed(1, sample(310, 10, 160, "new-boot"));
        assert_eq!(up, Some(10.0));
    }

    /// agent 还没上报新数据时 updated_at 不动,做差会得到一个假的 0。
    #[test]
    fn identical_timestamp_yields_no_reading() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(1_000, 1_000, 100, "b"));
        assert_eq!(t.feed(1, sample(1_000, 1_000, 100, "b")), (None, None));
    }

    /// 计数器回退(乱序上报 / 手改库)时不该算出负速率。
    #[test]
    fn counter_going_backwards_yields_no_reading() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(5_000, 5_000, 100, "b"));
        assert_eq!(t.feed(1, sample(1_000, 5_000, 130, "b")), (None, None));
    }

    /// 每台 agent 各自一条基准线,不能互相串。
    #[test]
    fn agents_are_tracked_independently() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(0, 0, 100, "b1"));
        t.feed(2, sample(0, 0, 100, "b2"));
        assert_eq!(t.feed(1, sample(300, 0, 130, "b1")).0, Some(10.0));
        assert_eq!(t.feed(2, sample(600, 0, 130, "b2")).0, Some(20.0));
    }

    #[test]
    fn quota_ratio_is_none_without_a_quota() {
        let mut a = AgentRow {
            id: 1,
            name: "a".into(),
            token_prefix: "abcd1234".into(),
            status: "online".into(),
            agent_version: None,
            ipv4: None,
            ipv6: None,
            nic_quota_bytes: None,
            nic_reset_day: None,
            cycle_rx: 50,
            cycle_tx: 50,
            up_per_sec: None,
            down_per_sec: None,
            node_count: 0,
        };
        assert_eq!(a.quota_ratio(), None, "不限流量时不该有比例(否则会画出 0% 的条)");
        a.nic_quota_bytes = Some(0);
        assert_eq!(a.quota_ratio(), None, "配额 0 也是不限");
        a.nic_quota_bytes = Some(200);
        assert_eq!(a.quota_ratio(), Some(0.5));
        // 超额时夹到 1.0,不画出界。
        a.nic_quota_bytes = Some(10);
        assert_eq!(a.quota_ratio(), Some(1.0));
    }

    /// 用户用量要乘倍率 —— 与 §6.3 的判定同一个口径。
    #[test]
    fn user_usage_applies_the_traffic_multiplier() {
        let u = UserRow {
            id: 1,
            name: "alice".into(),
            enabled: true,
            auto_disabled: false,
            quota_bytes: 1000,
            cycle_up: 100,
            cycle_down: 150,
            traffic_multiplier: 2.0,
            expire_at: None,
            node_count: 1,
            sub_token: "t".into(),
        };
        assert_eq!(u.used(), 500);
        assert_eq!(u.quota_ratio(), Some(0.5));
    }
}
