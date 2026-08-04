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
use std::collections::{HashMap, VecDeque};

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
    /// 主机指标(§7.2 的 `sysinfo.report`)。**全都可空** ——
    /// 从没连上过的 agent 这几项就该显示 `--`,而不是一个看起来像真的 0。
    pub cpu_pct: Option<f64>,
    pub mem_used: Option<i64>,
    pub mem_total: Option<i64>,
    pub load1: Option<f64>,
    pub uptime_secs: Option<i64>,
    /// 这批指标是哪一刻的。少了它,一台离线三天的机器会一直显示三天前那个
    /// CPU 数字,看起来和在线的机器毫无区别。
    pub sysinfo_at: Option<i64>,
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

    /// 内存占用比例。`mem_total` 为 0 或缺失时是 `None`。
    pub fn mem_ratio(&self) -> Option<f64> {
        let total = self.mem_total.filter(|t| *t > 0)?;
        let used = self.mem_used?;
        Some((used as f64 / total as f64).clamp(0.0, 1.0))
    }

    /// 主机指标是不是还新鲜。超过 `stale_after` 秒就当作过期,界面上显示 `--`。
    ///
    /// 上报周期是 30s,所以门槛要留出余量 —— 卡在 30s 上会让正常的抖动
    /// (网络慢了两秒)表现成「指标一闪一闪地消失」。
    pub fn host_metrics_fresh(&self, now: i64, stale_after: i64) -> bool {
        matches!(self.sysinfo_at, Some(at) if now - at <= stale_after)
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
    /// 节点参数原样带回来,给「编辑节点」做预填。
    ///
    /// **这里面有凭据**(reality 私钥、自签证书私钥、ss 服务端密钥,见 model/node.rs)。
    /// 页面上只准渲染 `server_name` / `path` / `ipv6` / `relay` 这几项;
    /// 编辑时也只改这几项,其余原样写回 —— 换一套密钥等于客户端静默全部失联(§9.1)。
    pub params: crate::model::node::NodeParams,
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
    pub reset_day: Option<i64>,
    /// 已分配的节点 id。分配框要靠它把已选项打上勾 ——
    /// 只有个数的话,打开分配框会是一片空白,人以为原来什么都没选。
    pub node_ids: Vec<i64>,
    pub sub_token: String,
}

impl UserRow {
    pub fn node_count(&self) -> usize {
        self.node_ids.len()
    }

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

/// 「某个节点上各用户用了多少」/「某个用户在各节点上用了多少」的一行。
///
/// 两个方向共用一个结构:它们查的是 `user_traffic` 的同一张表,
/// 只是分组的那一维不同 —— 拆成两个类型只会让渲染也要写两份。
#[derive(Debug, Clone)]
pub struct BreakdownRow {
    /// 对面那一维的名字:节点视图里是用户名,用户视图里是「tag(所属机器)」。
    pub label: String,
    /// 灰色副标题:节点视图里是用户状态,用户视图里是协议与端口。
    pub note: String,
    pub cycle_up: i64,
    pub cycle_down: i64,
    pub total_up: i64,
    pub total_down: i64,
}

impl BreakdownRow {
    pub fn cycle(&self) -> i64 {
        self.cycle_up.saturating_add(self.cycle_down)
    }
}

/// 某个节点上,各用户的用量。
///
/// `LEFT JOIN`:分配了但还没跑过流量的用户也要出现(全 0)——
/// 「分配了没生效」正是最需要看见的一种情况,内连接会把它整行藏起来。
pub async fn node_breakdown(pool: &SqlitePool, node_id: i64) -> Result<Vec<BreakdownRow>> {
    let rows: Vec<(String, bool, bool, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT u.name, u.enabled, u.auto_disabled,
                COALESCE(t.cycle_up, 0), COALESCE(t.cycle_down, 0),
                COALESCE(t.total_up, 0), COALESCE(t.total_down, 0)
           FROM user_nodes un
           JOIN users u ON u.id = un.user_id
           LEFT JOIN user_traffic t ON t.user_id = un.user_id AND t.node_id = un.node_id
          WHERE un.node_id = ?
          ORDER BY (COALESCE(t.cycle_up, 0) + COALESCE(t.cycle_down, 0)) DESC, u.name",
    )
    .bind(node_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, enabled, auto, cu, cd, tu, td)| BreakdownRow {
            label: name,
            note: match (enabled, auto) {
                (true, _) => "启用".into(),
                (false, true) => "自动停用".into(),
                (false, false) => "手动停用".into(),
            },
            cycle_up: cu,
            cycle_down: cd,
            total_up: tu,
            total_down: td,
        })
        .collect())
}

/// `user_nodes` × `nodes` × `agents` × `user_traffic` 的一行。
type UserNodeRow = (String, String, String, i64, i64, i64, i64, i64);

/// 某个用户在各节点上的用量。
pub async fn user_breakdown(pool: &SqlitePool, user_id: i64) -> Result<Vec<BreakdownRow>> {
    let rows: Vec<UserNodeRow> = sqlx::query_as(
        "SELECT n.tag, a.name, n.protocol, n.listen_port,
                COALESCE(t.cycle_up, 0), COALESCE(t.cycle_down, 0),
                COALESCE(t.total_up, 0), COALESCE(t.total_down, 0)
           FROM user_nodes un
           JOIN nodes n  ON n.id = un.node_id
           JOIN agents a ON a.id = n.agent_id
           LEFT JOIN user_traffic t ON t.user_id = un.user_id AND t.node_id = un.node_id
          WHERE un.user_id = ?
          ORDER BY (COALESCE(t.cycle_up, 0) + COALESCE(t.cycle_down, 0)) DESC, n.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(tag, agent, proto, port, cu, cd, tu, td)| BreakdownRow {
            // tag 在不同机器上可以重名,所以标签里必须带机器名 ——
            // 否则两行长得一模一样,只能靠顺序猜是哪一台。
            label: format!("{tag} @ {agent}"),
            note: format!("{proto} · :{port}"),
            cycle_up: cu,
            cycle_down: cd,
            total_up: tu,
            total_down: td,
        })
        .collect())
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

/// 一次算出来的速率,连同它是**哪一次上报**算出来的。
#[derive(Debug, Clone, Copy)]
struct Rate {
    up: f64,
    down: f64,
    /// 对应 `agent_nic_traffic.updated_at`。用来判断这个读数还新不新鲜。
    at: i64,
}

/// 跨刷新保留的采样表。**只在内存里。**
#[derive(Default)]
pub struct SpeedTracker {
    last: HashMap<i64, NicSample>,
    /// 上一次算出来的速率。
    ///
    /// **必须留住它。** TUI 每秒刷新一次,而 agent 每 30 秒才上报一次 ——
    /// 两次上报之间的 29 次刷新都做不出差(`updated_at` 没动)。早先那一版
    /// 在做不出差时直接给 `None`,于是界面上的网速只在上报到达后的**那一帧**
    /// 闪一下,其余时间全是 `--`。这不是「没有数据」,是「数据还是上一次那个」——
    /// 本来这个数字就是 30 秒平均值(§8.2),它在一个周期内保持不变才是对的。
    rates: HashMap<i64, Rate>,
    /// 全集群速率历史,给仪表盘的折线图用。
    history: VecDeque<(f64, f64)>,
    /// 见过的最新一次上报时间。用来判断「这一轮刷新有没有新数据」。
    last_report_at: i64,
}

/// 速率读数保留多久。上报周期 30s,给三倍余量:
/// 真的掉线了要变回 `--`,而不是永远挂着最后一个数字。
const RATE_STALE_AFTER: i64 = 90;

/// 折线图留多少个点。一个点 = 一轮上报(30s),120 个点 ≈ 一小时。
pub const HISTORY_LEN: usize = 120;

impl SpeedTracker {
    /// 全集群速率历史,给仪表盘的折线图用。最旧的在前。
    pub fn history(&self) -> &VecDeque<(f64, f64)> {
        &self.history
    }

    /// 记一个历史点。
    ///
    /// **只在真的来了新上报时记**(`report_at` 前进了)。TUI 每秒刷新一次,
    /// 每次都记的话,一轮上报会被复制成 30 个一模一样的点 —— 图上是一条条平台,
    /// 横轴的含义也变成了「刷新次数」而不是时间。
    fn observe(&mut self, report_at: i64, up: f64, down: f64) {
        if report_at <= self.last_report_at {
            return;
        }
        self.last_report_at = report_at;
        if self.history.len() >= HISTORY_LEN {
            self.history.pop_front();
        }
        self.history.push_back((up, down));
    }

    /// 喂一次新采样,返回 `(上行速率, 下行速率)`。
    ///
    /// 返回 `None` 的情况都该在界面上显示 `--` 而不是 0:
    ///   * 第一次见到这台 agent(还没有第二个采样点);
    ///   * boot_id 变了(计数器归零,上一次的基准作废);
    ///   * 上一个读数已经过期(agent 掉线了)。
    fn feed(&mut self, agent_id: i64, s: NicSample, now: i64) -> (Option<f64>, Option<f64>) {
        let prev = self.last.insert(agent_id, s.clone());

        let fresh = match prev {
            None => None,                                 // 没有基准
            Some(p) if p.boot_id != s.boot_id => {
                // 机器重启,计数器归零。旧读数一并作废,否则会一直显示重启前的速率。
                self.rates.remove(&agent_id);
                None
            }
            Some(p) => {
                let dt = s.at - p.at;
                // 计数器只增不减。出现回退说明上报乱序或库被手改过,当作没有可比数据。
                if dt <= 0 || s.rx < p.rx || s.tx < p.tx {
                    None
                } else {
                    Some(Rate {
                        up: (s.rx - p.rx) as f64 / dt as f64,
                        down: (s.tx - p.tx) as f64 / dt as f64,
                        at: s.at,
                    })
                }
            }
        };

        if let Some(r) = fresh {
            self.rates.insert(agent_id, r);
        }
        match self.rates.get(&agent_id) {
            // `s.at` 是这台 agent 最后一次上报的时间。用它而不是墙上时间来判过期:
            // TUI 与 daemon 是两个进程,库里的时间戳才是唯一共同的参照。
            Some(r) if now - r.at <= RATE_STALE_AFTER => (Some(r.up), Some(r.down)),
            _ => (None, None),
        }
    }
}

/// `agents` + `agent_nic_traffic` 的一行。
///
/// 用具名结构体而不是元组:sqlx 的 `FromRow` 对元组只实现到 16 元,
/// 而这里已经贴着上限了 —— 再加一列就会撞上一个「trait 未实现」的天书报错。
/// 具名结构体按**列名**取值,顺带把「SELECT 的列序和这里对不上」这类错也消掉了。
#[derive(sqlx::FromRow)]
struct AgentQueryRow {
    id: i64,
    name: String,
    token_prefix: String,
    status: String,
    agent_version: Option<String>,
    ipv4: Option<String>,
    ipv6: Option<String>,
    nic_quota_bytes: Option<i64>,
    nic_reset_day: Option<i64>,
    cpu_pct: Option<f64>,
    mem_used: Option<i64>,
    mem_total: Option<i64>,
    load1: Option<f64>,
    uptime_secs: Option<i64>,
    sysinfo_at: Option<i64>,
    boot_id: Option<String>,
    last_rx: i64,
    last_tx: i64,
    cycle_rx: i64,
    cycle_tx: i64,
    updated_at: i64,
    node_count: i64,
}

pub async fn load_agents(
    pool: &SqlitePool,
    speed: &mut SpeedTracker,
    now: i64,
) -> Result<Vec<AgentRow>> {
    // LEFT JOIN:从没上报过的 agent 也要出现在列表里(值为 0),
    // 否则「加了但还没连上」的那台会直接看不见,而那正是最需要排查的一台。
    let rows: Vec<AgentQueryRow> = sqlx::query_as(
        "SELECT a.id, a.name, a.token_prefix, a.status, a.agent_version, a.ipv4, a.ipv6,
                a.nic_quota_bytes, a.nic_reset_day,
                a.cpu_pct, a.mem_used, a.mem_total, a.load1, a.uptime_secs, a.sysinfo_at,
                t.boot_id,
                COALESCE(t.last_rx, 0)    AS last_rx,
                COALESCE(t.last_tx, 0)    AS last_tx,
                COALESCE(t.cycle_rx, 0)   AS cycle_rx,
                COALESCE(t.cycle_tx, 0)   AS cycle_tx,
                COALESCE(t.updated_at, 0) AS updated_at,
                (SELECT COUNT(*) FROM nodes n WHERE n.agent_id = a.id) AS node_count
           FROM agents a
           LEFT JOIN agent_nic_traffic t ON t.agent_id = a.id
          ORDER BY a.id",
    )
    .fetch_all(pool)
    .await?;

    let mut newest_report = 0i64;
    let out: Vec<AgentRow> = rows
        .into_iter()
        .map(|r| {
            newest_report = newest_report.max(r.updated_at);
            let (up, down) = speed.feed(
                r.id,
                NicSample { rx: r.last_rx, tx: r.last_tx, at: r.updated_at, boot_id: r.boot_id },
                now,
            );
            AgentRow {
                id: r.id,
                name: r.name,
                token_prefix: r.token_prefix,
                status: r.status,
                agent_version: r.agent_version,
                ipv4: r.ipv4,
                ipv6: r.ipv6,
                nic_quota_bytes: r.nic_quota_bytes,
                nic_reset_day: r.nic_reset_day,
                cycle_rx: r.cycle_rx,
                cycle_tx: r.cycle_tx,
                up_per_sec: up,
                down_per_sec: down,
                node_count: r.node_count,
                cpu_pct: r.cpu_pct,
                mem_used: r.mem_used,
                mem_total: r.mem_total,
                load1: r.load1,
                uptime_secs: r.uptime_secs,
                sysinfo_at: r.sysinfo_at,
            }
        })
        .collect();

    // 全集群合计,给折线图用。**只算有读数的那些** —— 把 None 当 0 会让
    // 「刚打开界面」和「全网都闲着」画出同一条线。
    let up: f64 = out.iter().filter_map(|a| a.up_per_sec).sum();
    let down: f64 = out.iter().filter_map(|a| a.down_per_sec).sum();
    speed.observe(newest_report, up, down);

    Ok(out)
}

/// `nodes` + 所属 agent 名 + 用户数 + 原始 `params_json`。
type NodeQueryRow = (i64, i64, String, String, String, i64, i64, String);

pub async fn load_nodes(pool: &SqlitePool) -> Result<Vec<NodeRow>> {
    let rows: Vec<NodeQueryRow> = sqlx::query_as(
        "SELECT n.id, n.agent_id, a.name, n.tag, n.protocol, n.listen_port,
                (SELECT COUNT(*) FROM user_nodes un WHERE un.node_id = n.id),
                n.params_json
           FROM nodes n JOIN agents a ON a.id = n.agent_id
          ORDER BY n.agent_id, n.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, agent_id, agent_name, tag, protocol, listen_port, user_count, params)| NodeRow {
                id,
                agent_id,
                agent_name,
                tag,
                protocol,
                listen_port,
                user_count,
                // 解不出来就当空参数:一行坏 JSON 不该让整个节点页读不出来。
                params: serde_json::from_str(&params).unwrap_or_default(),
            },
        )
        .collect())
}

type UserQueryRow = (i64, String, bool, bool, i64, f64, Option<i64>, Option<i64>, String, i64, i64);

pub async fn load_users(pool: &SqlitePool) -> Result<Vec<UserRow>> {
    let rows: Vec<UserQueryRow> = sqlx::query_as(
        "SELECT u.id, u.name, u.enabled, u.auto_disabled, u.quota_bytes,
                u.traffic_multiplier, u.expire_at, u.reset_day, u.sub_token,
                COALESCE(t.cycle_up, 0), COALESCE(t.cycle_down, 0)
           FROM users u
           LEFT JOIN user_traffic_total t ON t.user_id = u.id
          ORDER BY u.id",
    )
    .fetch_all(pool)
    .await?;

    // 分配关系一次查完再分发,而不是每个用户一条子查询:
    // 用户数上百时那是上百次查询,而 TUI 每秒刷新一次。
    let pairs: Vec<(i64, i64)> =
        sqlx::query_as("SELECT user_id, node_id FROM user_nodes ORDER BY node_id")
            .fetch_all(pool)
            .await?;
    let mut by_user: HashMap<i64, Vec<i64>> = HashMap::new();
    for (uid, nid) in pairs {
        by_user.entry(uid).or_default().push(nid);
    }

    Ok(rows
        .into_iter()
        .map(|r| UserRow {
            node_ids: by_user.remove(&r.0).unwrap_or_default(),
            id: r.0,
            name: r.1,
            enabled: r.2,
            auto_disabled: r.3,
            quota_bytes: r.4,
            traffic_multiplier: r.5,
            expire_at: r.6,
            reset_day: r.7,
            sub_token: r.8,
            cycle_up: r.9,
            cycle_down: r.10,
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
        assert_eq!(t.feed(1, sample(0, 0, 100, "b"), 100), (None, None));
    }

    #[test]
    fn speed_is_delta_over_elapsed_seconds() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(1_000, 2_000, 100, "b"), 100);
        let (up, down) = t.feed(1, sample(4_000, 5_000, 130, "b"), 130);
        // 30 秒里涨了 3000 / 3000 字节 → 100 B/s
        assert_eq!(up, Some(100.0));
        assert_eq!(down, Some(100.0));
    }

    /// **两次上报之间的刷新要留住上一个读数。**
    ///
    /// 这是 v0.2.2 里那个「网速显示不了、偶尔闪一下」的回归锚点:
    /// TUI 每秒刷新,agent 每 30 秒上报,中间 29 次刷新都做不出差。
    /// 早先那一版在做不出差时给 `None`,于是网速只在上报到达的**那一帧**出现。
    /// 这个数字本来就是 30 秒平均值,一个周期内保持不变才是对的。
    #[test]
    fn the_reading_persists_between_reports() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(1_000, 1_000, 100, "b"), 100);
        assert_eq!(t.feed(1, sample(4_000, 4_000, 130, "b"), 130).0, Some(100.0));

        // 之后 29 次刷新,库里的 updated_at 一直是 130 —— 读数要保持住。
        for now in 131..=159 {
            assert_eq!(
                t.feed(1, sample(4_000, 4_000, 130, "b"), now).0,
                Some(100.0),
                "第 {now} 秒时读数不该消失"
            );
        }
    }

    /// 但**真的掉线了**要变回 `--`,不能永远挂着最后一个数字。
    #[test]
    fn a_stale_reading_eventually_disappears() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(1_000, 1_000, 100, "b"), 100);
        t.feed(1, sample(4_000, 4_000, 130, "b"), 130);
        // 90 秒是门槛(上报周期的三倍)。
        assert_eq!(t.feed(1, sample(4_000, 4_000, 130, "b"), 130 + 90).0, Some(100.0));
        assert_eq!(t.feed(1, sample(4_000, 4_000, 130, "b"), 130 + 91), (None, None));
    }

    /// 机器重启(boot_id 变)时计数器归零。做差会得到负数 ——
    /// 必须报 `None`,不能报一个爆炸的数字(§8.2),**也不能留住重启前那个读数**。
    #[test]
    fn boot_id_change_suppresses_the_reading() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(9_000_000, 9_000_000, 100, "old-boot"), 100);
        t.feed(1, sample(9_003_000, 9_003_000, 130, "old-boot"), 130);
        assert_eq!(t.feed(1, sample(10, 10, 160, "new-boot"), 160), (None, None));
        // 重启之后仍然能重新建立基准。
        let (up, _) = t.feed(1, sample(310, 10, 190, "new-boot"), 190);
        assert_eq!(up, Some(10.0));
    }

    /// 计数器回退(乱序上报 / 手改库)时不该算出负速率。
    #[test]
    fn counter_going_backwards_yields_no_reading() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(5_000, 5_000, 100, "b"), 100);
        // 没有可比数据,又没有旧读数可留 → None。
        assert_eq!(t.feed(1, sample(1_000, 5_000, 130, "b"), 130), (None, None));
    }

    /// 每台 agent 各自一条基准线,不能互相串。
    #[test]
    fn agents_are_tracked_independently() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(0, 0, 100, "b1"), 100);
        t.feed(2, sample(0, 0, 100, "b2"), 100);
        assert_eq!(t.feed(1, sample(300, 0, 130, "b1"), 130).0, Some(10.0));
        assert_eq!(t.feed(2, sample(600, 0, 130, "b2"), 130).0, Some(20.0));
    }

    /// 折线图的点**只在真的来了新上报时**才记。
    /// 每次刷新都记的话,一轮上报会被复制成 30 个一样的点 ——
    /// 横轴的含义就从「时间」变成了「刷新次数」。
    #[test]
    fn history_advances_only_on_a_new_report() {
        let mut t = SpeedTracker::default();
        t.observe(100, 1.0, 2.0);
        t.observe(100, 9.0, 9.0); // 同一轮上报,忽略
        t.observe(90, 9.0, 9.0); // 更旧的时间戳,忽略
        assert_eq!(t.history().len(), 1);
        assert_eq!(t.history()[0], (1.0, 2.0));

        t.observe(130, 3.0, 4.0);
        assert_eq!(t.history().len(), 2);
        assert_eq!(t.history()[1], (3.0, 4.0));
    }

    /// 历史是有界的,否则一个开着几天的 TUI 会一直吃内存。
    #[test]
    fn history_is_bounded() {
        let mut t = SpeedTracker::default();
        for i in 0..(HISTORY_LEN as i64 * 3) {
            t.observe(i + 1, i as f64, 0.0);
        }
        assert_eq!(t.history().len(), HISTORY_LEN);
        // 留下的是**最新**的那一段。
        assert_eq!(t.history()[HISTORY_LEN - 1].0, (HISTORY_LEN as f64) * 3.0 - 1.0);
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
            cpu_pct: None,
            mem_used: None,
            mem_total: None,
            load1: None,
            uptime_secs: None,
            sysinfo_at: None,
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
            reset_day: None,
            node_ids: vec![3],
            sub_token: "t".into(),
        };
        assert_eq!(u.used(), 500);
        assert_eq!(u.quota_ratio(), Some(0.5));
        assert_eq!(u.node_count(), 1);
    }
}
