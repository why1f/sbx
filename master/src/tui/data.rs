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
    /// `runtime.GOARCH`。升级时靠它挑对产物 —— release 只出 amd64 / arm64。
    pub arch: Option<String>,
    /// 出站地址族策略。进 sing-box 配置,改它要推进 config_revision。
    pub outbound: crate::model::outbound::OutboundStrategy,
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
    /// 这个节点上**所有用户**本周期用量之和,**已含各自的计费倍率**。
    ///
    /// 倍率在 SQL 里就乘进去了(`SUM(t.cycle_up * u.traffic_multiplier)`),
    /// 因为一个节点上的用户各有各的倍率 —— 先求和再乘一个「平均倍率」是算不出来的。
    ///
    /// 与 `agents` 上的网卡数字仍是两个口径:这里是 sing-box 记的账 × 倍率,
    /// 网卡是整机进出(§6.4 / §7.2)。两个数对不上是正常的,不是 bug。
    pub cycle_up: i64,
    pub cycle_down: i64,
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
    /// 订阅响应头按这几台机器的网卡用量报(§10.3)。空 = 按用户自己的用量。
    pub nic_agent_ids: Vec<i64>,
    pub sub_token: String,
}

impl UserRow {
    pub fn node_count(&self) -> usize {
        self.node_ids.len()
    }

    /// 用户名后面跟的那个倍率标记(`[2.0x]` / `[1.0x]` / `[1.5x]`)。
    ///
    /// **不省略 `[1.0x]`。** 省了的话「没标记」有两种含义 —— 单倍、或者这一版
    /// 还没做倍率显示 —— 而人分不出来。一律标出来,一列里全是 `[1.0x]` 也无妨,
    /// 那本身就是「这些人都是单倍」的信息。
    pub fn mult_tag(&self) -> String {
        mult_tag(self.traffic_multiplier)
    }
}

/// 倍率标记的统一写法:`[2.0x]`。
///
/// **一律带一位小数**,`[2.0x]` 而不是 `[2x]` —— 宽度恒定 6 列,
/// 于是它右边那一栏在每一行都从同一个位置起,不会随倍率是不是整数而抖动。
/// 1.5 这样的倍率也不用特殊处理。
///
/// 数字在前、`x` 在后,和口语「两倍」同序;方括号是为了让它读起来像个标签,
/// 而不是黏在用户名尾巴上的一截乱码(`alice2.0x` 会被当成名字的一部分)。
pub fn mult_tag(m: f64) -> String {
    format!("[{:.1}x]", m.max(0.0))
}

/// `mult_tag` 的显示宽度。恒定 6 列(`[9.9x]` 到 `[2.0x]` 都是)——
/// 布局按它算预留,别在渲染处再写一遍字面量 6。
pub const MULT_TAG_COLS: usize = 6;

/// 原始字节数 × 倍率。负数(不该出现,但库里可能有)当 0 —— 一个负的
/// 用量会让进度条和百分比一起变成负数,而那比显示 0 更难看懂。
fn billed(raw: i64, mult: f64) -> i64 {
    (raw.max(0) as f64 * mult.max(0.0)) as i64
}

impl UserRow {
    /// 计费口径的上行/下行。**界面上一律显示这两个**,不显示 `cycle_up/cycle_down`。
    ///
    /// 原始字段留着不动:它们是库里的真值,也是订阅头、明细对账要回溯的东西。
    /// 但摆在人眼前的数字必须和配额判定同一个口径 —— 早先上下行是单倍、
    /// 总量是倍率后的,于是「↑ 2.63 MB ↓ 132.03 MB」加起来对不上旁边的「累计 269 MB」,
    /// 看着像统计坏了,实际上只是两栏在用两套账。
    pub fn billed_up(&self) -> i64 {
        billed(self.cycle_up, self.traffic_multiplier)
    }

    pub fn billed_down(&self) -> i64 {
        billed(self.cycle_down, self.traffic_multiplier)
    }

    /// 计费口径的已用量:含倍率。与 §6.3 的配额判定一致 ——
    /// 两处口径不同会让「列表显示 80%」和「已经被停用」同时出现。
    ///
    /// 分别乘再相加,**不是**相加再乘:后者会和上下行两栏差最多 1 字节,
    /// 而这一栏就摆在那两栏旁边,差一个字节也是肉眼可见的对不上。
    pub fn used(&self) -> i64 {
        self.billed_up().saturating_add(self.billed_down())
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
    /// 名字后面那个灰色的倍率标记(`[2.0x]`)。**节点视图才有** ——
    /// 那一维每行是一个用户,各有各的倍率;用户视图整张表同一个倍率,
    /// 逐行重复只是噪声(它写在标题里)。
    ///
    /// 与 `label` 分开存,是因为两者**颜色不同**:名字跟着主色,标记是灰的。
    /// 拼成一个字符串就只能整体一个颜色了。
    pub mult: Option<String>,
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

/// `user_nodes` × `users` × `user_traffic` 的一行(节点视图)。
type NodeUserRow = (String, bool, bool, f64, i64, i64, i64, i64);

/// 某个节点上,各用户的用量。**数字含各用户自己的计费倍率**,名字后面带 `x2` 标记。
///
/// `LEFT JOIN`:分配了但还没跑过流量的用户也要出现(全 0)——
/// 「分配了没生效」正是最需要看见的一种情况,内连接会把它整行藏起来。
pub async fn node_breakdown(pool: &SqlitePool, node_id: i64) -> Result<Vec<BreakdownRow>> {
    // 倍率取出来在 Rust 里乘,不在 SQL 里:这一行只关联一个用户,
    // 而标记 `x2` 也要用到这个值,查一次比查两次好。
    let rows: Vec<NodeUserRow> = sqlx::query_as(
        "SELECT u.name, u.enabled, u.auto_disabled, u.traffic_multiplier,
                COALESCE(t.cycle_up, 0), COALESCE(t.cycle_down, 0),
                COALESCE(t.total_up, 0), COALESCE(t.total_down, 0)
           FROM user_nodes un
           JOIN users u ON u.id = un.user_id
           LEFT JOIN user_traffic t ON t.user_id = un.user_id AND t.node_id = un.node_id
          WHERE un.node_id = ?
          ORDER BY (COALESCE(t.cycle_up, 0) + COALESCE(t.cycle_down, 0))
                     * u.traffic_multiplier DESC, u.name",
    )
    .bind(node_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, enabled, auto, mult, cu, cd, tu, td)| BreakdownRow {
            label: name,
            // 倍率标记跟在名字后面 —— 这一行的四个数字都乘过它,
            // 不标的话看到的人无从知道 `↓ 132 MB` 已经是 ×2 之后的值。
            // 单独一个字段是为了让它渲染成灰色,和仪表盘用户视图一致。
            mult: Some(mult_tag(mult)),
            note: match (enabled, auto) {
                (true, _) => "启用".into(),
                (false, true) => "自动停用".into(),
                (false, false) => "手动停用".into(),
            },
            cycle_up: billed(cu, mult),
            cycle_down: billed(cd, mult),
            total_up: billed(tu, mult),
            total_down: billed(td, mult),
        })
        .collect())
}

/// `user_nodes` × `nodes` × `agents` × `user_traffic` 的一行。
type UserNodeRow = (String, String, String, i64, f64, i64, i64, i64, i64);

/// 某个用户在各节点上的用量。**数字含这个用户的计费倍率。**
///
/// 这里的行不带 `x2` 标记:整张表都是同一个用户,标记会在每一行重复一遍 ——
/// 它属于标题(调用方会写进 `head`),不属于行。
pub async fn user_breakdown(pool: &SqlitePool, user_id: i64) -> Result<Vec<BreakdownRow>> {
    let rows: Vec<UserNodeRow> = sqlx::query_as(
        "SELECT n.tag, a.name, n.protocol, n.listen_port, u.traffic_multiplier,
                COALESCE(t.cycle_up, 0), COALESCE(t.cycle_down, 0),
                COALESCE(t.total_up, 0), COALESCE(t.total_down, 0)
           FROM user_nodes un
           JOIN users u  ON u.id = un.user_id
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
        .map(|(tag, agent, proto, port, mult, cu, cd, tu, td)| BreakdownRow {
            // tag 在不同机器上可以重名,所以标签里必须带机器名 ——
            // 否则两行长得一模一样,只能靠顺序猜是哪一台。
            label: format!("{tag} @ {agent}"),
            // 整张表同一个用户,倍率写在标题里(见 `ShowUserNodes`)。
            mult: None,
            note: format!("{proto} · :{port}"),
            cycle_up: billed(cu, mult),
            cycle_down: billed(cd, mult),
            total_up: billed(tu, mult),
            total_down: billed(td, mult),
        })
        .collect())
}

/// `nodes` × `user_traffic` 聚合后的一行。
type AgentNodeRow = (String, String, i64, i64, i64, i64, i64, i64);

/// 某台被控机上,各节点的用量。**节点那几个数含各用户自己的计费倍率**,
/// 与页头几行的网卡口径**不是**一回事 —— 这正是这一页要并排给出两者的原因。
///
/// 这是「网卡明细」二级页面的主体:先看整机网卡烧了多少(在 head 那几行),
/// 再看这台机器上是哪个节点在跑量。两个口径不同,**必须并排看**才有意义 ——
/// 网卡是整机进出(含系统更新、别的服务),节点是 sing-box 记的账 × 倍率(§6.4)。
///
/// `LEFT JOIN`:建了但一次没跑过的节点也要出现(全 0)。
/// 「配了没生效」正是最需要看见的情况,内连接会把它整行藏起来。
pub async fn agent_breakdown(pool: &SqlitePool, agent_id: i64) -> Result<Vec<BreakdownRow>> {
    // 一个节点上的用户各有各的倍率,所以要逐用户乘完再 SUM(理由同 `load_nodes`)。
    // `users` 走 LEFT JOIN 是为了保住上面那条「没跑过的节点也要出现」——
    // 内连接会在 `user_traffic` 为空时把整行滤掉。
    let rows: Vec<AgentNodeRow> = sqlx::query_as(
        "SELECT n.tag, n.protocol, n.listen_port,
                (SELECT COUNT(*) FROM user_nodes un WHERE un.node_id = n.id),
                COALESCE(CAST(SUM(t.cycle_up   * u.traffic_multiplier) AS INTEGER), 0),
                COALESCE(CAST(SUM(t.cycle_down * u.traffic_multiplier) AS INTEGER), 0),
                COALESCE(CAST(SUM(t.total_up   * u.traffic_multiplier) AS INTEGER), 0),
                COALESCE(CAST(SUM(t.total_down * u.traffic_multiplier) AS INTEGER), 0)
           FROM nodes n
           LEFT JOIN user_traffic t ON t.node_id = n.id
           LEFT JOIN users u        ON u.id = t.user_id
          WHERE n.agent_id = ?
          GROUP BY n.id
          ORDER BY COALESCE(SUM((t.cycle_up + t.cycle_down) * u.traffic_multiplier), 0) DESC, n.id",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(tag, proto, _port, users, cu, cd, tu, td)| BreakdownRow {
            label: tag,
            // 一行是一个节点,上面混着多个倍率不同的用户 —— 没有单一倍率可标。
            mult: None,
            // 人数排在最前面:这一页问的是「量堆在哪」,而「几个人在用」是
            // 紧接着的下一个问题。端口不写 —— 它是节点页的事,和流量对账无关,
            // 而这一栏窄到装不下所有东西时,先被截掉的正是最右边那一段。
            note: format!("{users} 人 · {proto}"),
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
    /// 见过的最新一次上报时间。用来判断「这一轮刷新有没有新数据」——
    /// 没有新数据就不该往图上加点(否则同一个读数会被复制成一条直线)。
    last_report_at: i64,
}

/// 速率读数保留多久。上报周期 30s,给三倍余量:
/// 真的掉线了要变回 `--`,而不是永远挂着最后一个数字。
const RATE_STALE_AFTER: i64 = 90;

/// 折线图留多少个点。**一个点 = 一次新上报**,不是一次刷新。
///
/// 60 而不是 120,和 sb-manager 一样:点数少一半,每格就宽一倍,
/// 于是每来一个新读数、曲线往左挪的那一下**看得见**。120 格时一次位移
/// 只有半个字符宽,盯着看都未必察觉。
///
/// 覆盖多长时间取决于 `cluster.report_interval_secs`:
/// 1 秒上报 = 一分钟(与 sb-manager 逐格对应),5 秒 = 五分钟,30 秒 = 半小时。
pub const HISTORY_LEN: usize = 60;

impl SpeedTracker {
    /// 全集群速率历史,给仪表盘的折线图用。最旧的在前。
    pub fn history(&self) -> &VecDeque<(f64, f64)> {
        &self.history
    }

    /// 记一个历史点。**只在真的来了新读数时记**(`report_at` 前进了)。
    ///
    /// 这一条来回改过两次,把两次的教训都记下来:
    ///
    /// * 按**刷新**记(每秒一个点)的话,主控在两次上报之间根本没有新信息,
    ///   于是同一个数字被复制 N 遍 —— 图上是一条长长的直线,只在上报到达的
    ///   那一格跳一下。画得再密也变不出不存在的数据。
    /// * 按**上报**记,每个点都是一次真实测量,没有重复段。
    ///   曲线走多快由 `cluster.report_interval_secs` 决定:调到 1 秒就和
    ///   sb-manager 逐格对应(它是本机每秒直接读 `/proc/net/dev`,没有 agent)。
    ///
    /// `known` 是**有速率读数的 agent 个数**。为 0 时不记点:那不是
    /// 「全网速率是 0」,是「还没有任何可比的采样」。少了这条判断,刚开界面
    /// 第一帧会记进一个 `(0, 0)`,下一轮记真实速率 —— 历史里只有两个点,
    /// 而 ratatui 会把它们拉满整幅宽度,画出一条从左下到右上的假斜线。
    fn observe(&mut self, report_at: i64, up: f64, down: f64, known: usize) {
        if known == 0 || report_at <= self.last_report_at {
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
                        // **rx 是下行,tx 是上行。** 站在这台被控机上看:
                        // `/proc/net/dev` 的 Receive 是它**收**进来的字节(下载),
                        // Transmit 是它**发**出去的(上传)。早先这里接反了,
                        // 表现是 agent 自升级(纯下载)时界面上涨的是 ↑。
                        up: (s.tx - p.tx) as f64 / dt as f64,
                        down: (s.rx - p.rx) as f64 / dt as f64,
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
    arch: Option<String>,
    outbound_strategy: String,
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
    last_seen: Option<i64>,
    boot_id: Option<String>,
    last_rx: i64,
    last_tx: i64,
    cycle_rx: i64,
    cycle_tx: i64,
    updated_at: i64,
    node_count: i64,
}

/// 库里写着 online,但已经太久没听见动静 —— 一律按 offline 显示。
///
/// **这是显示侧的兜底,不是主要防线。** 主要防线在 daemon 里:`recv_loop` 的
/// 读超时会把半开连接判掉并 `mark_offline`(`cluster::server::idle_limit`)。
/// 但那条路要求 **daemon 还活着**。daemon 崩了或者被 kill 之后,库里的
/// `status` 就永远停在最后一刻的值 —— 整屏绿灯,而一台都没连着。
/// TUI 是另一个进程,照着 `status` 列直读就会跟着一起骗人。
///
/// `stale_after` 取得比 daemon 的判定窗口**更宽**(见调用方):这样两者永不打架
/// —— daemon 还在的时候,它总是先一步把状态改对,这里什么都不会做;
/// 只有 daemon 失灵、状态烂在库里时,这一条才生效。
///
/// `last_seen` 为 NULL = 从来没连上过。那种情况保持库里的值(通常就是 offline),
/// 不在这里改写:「从没连过」和「连过又断了」是两回事,§7.2 的三态要分得开。
fn derive_status(status: &str, last_seen: Option<i64>, now: i64, stale_after: i64) -> String {
    match last_seen {
        Some(t) if status == "online" && now - t > stale_after => "offline".into(),
        _ => status.into(),
    }
}

pub async fn load_agents(
    pool: &SqlitePool,
    speed: &mut SpeedTracker,
    now: i64,
    stale_after: i64,
) -> Result<Vec<AgentRow>> {
    // LEFT JOIN:从没上报过的 agent 也要出现在列表里(值为 0),
    // 否则「加了但还没连上」的那台会直接看不见,而那正是最需要排查的一台。
    let rows: Vec<AgentQueryRow> = sqlx::query_as(
        "SELECT a.id, a.name, a.token_prefix, a.status, a.agent_version, a.arch, a.outbound_strategy, a.ipv4, a.ipv6,
                a.nic_quota_bytes, a.nic_reset_day,
                a.cpu_pct, a.mem_used, a.mem_total, a.load1, a.uptime_secs, a.sysinfo_at,
                a.last_seen,
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

    // 先把所有 agent 的上报时间捞出来 —— 下面 into_iter 之后就取不到了。
    let newest_report = rows.iter().map(|r| r.updated_at).max().unwrap_or(0);

    let out: Vec<AgentRow> = rows
        .into_iter()
        .map(|r| {
            let (up, down) = speed.feed(
                r.id,
                NicSample { rx: r.last_rx, tx: r.last_tx, at: r.updated_at, boot_id: r.boot_id },
                now,
            );
            AgentRow {
                id: r.id,
                name: r.name,
                token_prefix: r.token_prefix,
                status: derive_status(&r.status, r.last_seen, now, stale_after),
                agent_version: r.agent_version,
                arch: r.arch,
                outbound: crate::model::outbound::OutboundStrategy::parse(&r.outbound_strategy),
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
    //
    // **只在至少有一个读数时才记点**(count > 0)。否则第一帧推进 `(0, 0)`,
    // 下一轮上报推进真实速率,历史里就只有两个点 —— ratatui 把两个点拉满整个宽度,
    // 画出一条从左下到右上的斜线,看起来像「流量在一小时里稳步爬升」,
    // 实际上只是插值。
    // 任何一台 agent 报上来新数据,合计就变了:`newest_report` 一前进
    // 就说明这一帧真有新东西可画。
    let known = out.iter().filter(|a| a.up_per_sec.is_some()).count();
    let up: f64 = out.iter().filter_map(|a| a.up_per_sec).sum();
    let down: f64 = out.iter().filter_map(|a| a.down_per_sec).sum();
    speed.observe(newest_report, up, down, known);

    Ok(out)
}

/// `nodes` + 所属 agent 名 + 用户数 + 原始 `params_json`。
type NodeQueryRow = (i64, i64, String, String, String, i64, i64, i64, i64, String);

pub async fn load_nodes(pool: &SqlitePool) -> Result<Vec<NodeRow>> {
    // 倍率要**逐用户**乘进去再求和:一个节点上的用户各有各的倍率,
    // 先 SUM 再乘一个数是算不出来的。`CAST(... AS INTEGER)` 不能省 ——
    // `traffic_multiplier` 是 REAL,乘出来的和也是 REAL,而这一列要解成 i64,
    // 少了 CAST 会在解码时报「invalid type」而不是给出一个近似值。
    let rows: Vec<NodeQueryRow> = sqlx::query_as(
        "SELECT n.id, n.agent_id, a.name, n.tag, n.protocol, n.listen_port,
                (SELECT COUNT(*) FROM user_nodes un WHERE un.node_id = n.id),
                COALESCE((SELECT CAST(SUM(t.cycle_up   * u.traffic_multiplier) AS INTEGER)
                            FROM user_traffic t JOIN users u ON u.id = t.user_id
                           WHERE t.node_id = n.id), 0),
                COALESCE((SELECT CAST(SUM(t.cycle_down * u.traffic_multiplier) AS INTEGER)
                            FROM user_traffic t JOIN users u ON u.id = t.user_id
                           WHERE t.node_id = n.id), 0),
                n.params_json
           FROM nodes n JOIN agents a ON a.id = n.agent_id
          ORDER BY n.agent_id, n.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                agent_id,
                agent_name,
                tag,
                protocol,
                listen_port,
                user_count,
                cycle_up,
                cycle_down,
                params,
            )| NodeRow {
                id,
                agent_id,
                agent_name,
                tag,
                protocol,
                listen_port,
                user_count,
                cycle_up,
                cycle_down,
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

    let mut nics: HashMap<i64, Vec<i64>> = HashMap::new();
    for (uid, aid) in crate::db::node_repo::user_nic_bindings(pool).await? {
        nics.entry(uid).or_default().push(aid);
    }

    Ok(rows
        .into_iter()
        .map(|r| UserRow {
            node_ids: by_user.remove(&r.0).unwrap_or_default(),
            nic_agent_ids: nics.remove(&r.0).unwrap_or_default(),
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
        // 重启之后仍然能重新建立基准。tx 涨了 300 → 上行 10 B/s。
        let (up, _) = t.feed(1, sample(10, 310, 190, "new-boot"), 190);
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
        // `sample(rx, tx, …)` —— 动的是 tx,所以看上行。
        assert_eq!(t.feed(1, sample(0, 300, 130, "b1"), 130).0, Some(10.0));
        assert_eq!(t.feed(2, sample(0, 600, 130, "b2"), 130).0, Some(20.0));
    }

    /// **rx 是下行,tx 是上行。** 站在被控机上看:`/proc/net/dev` 的 Receive
    /// 是它收进来的字节(下载),Transmit 是它发出去的(上传)。
    ///
    /// 这条是 v0.4.8 的回归锚点。接反了平时看不出来 —— 代理流量两个方向都有,
    /// 数字都在涨。只有 agent 自升级这种**纯下载**的时刻才露馅:
    /// 界面上涨的是 ↑,而机器明明在下载。
    #[test]
    fn receive_is_download_and_transmit_is_upload() {
        let mut t = SpeedTracker::default();
        t.feed(1, sample(0, 0, 100, "b"), 100);
        // 30 秒里 rx 涨 3000、tx 涨 300 —— 就是「在下载」的样子。
        let (up, down) = t.feed(1, sample(3_000, 300, 130, "b"), 130);
        assert_eq!(down, Some(100.0), "rx 的增量该算成**下行**");
        assert_eq!(up, Some(10.0), "tx 的增量该算成**上行**");
        assert!(down > up, "纯下载时下行必须大于上行,反了就是这个 bug");
    }

    /// 折线图**只在真的来了新读数时**才推进一格。
    ///
    /// 主控在两次上报之间没有任何新信息。按刷新记的话,同一个数字会被复制
    /// 一大串 —— 图上是一条长直线,只在上报到达的那一格跳一下。
    /// 每个点都该是一次真实测量。
    #[test]
    fn history_advances_only_on_a_new_report() {
        let mut t = SpeedTracker::default();
        t.observe(100, 1.0, 2.0, 1);
        // 同一轮上报,界面刷新了两次 —— 不该多出点来。
        t.observe(100, 1.0, 2.0, 1);
        t.observe(100, 1.0, 2.0, 1);
        assert_eq!(t.history().len(), 1, "没有新上报就不该加点");

        // 更旧的时间戳(乱序/时钟回拨)同样忽略。
        t.observe(90, 9.0, 9.0, 1);
        assert_eq!(t.history().len(), 1);

        // 新上报到了。
        t.observe(130, 3.0, 4.0, 1);
        assert_eq!(t.history().len(), 2);
        assert_eq!(t.history()[1], (3.0, 4.0));
    }

    /// 图铺满要多久,由**上报间隔**决定,不由刷新频率决定。
    ///
    /// `HISTORY_LEN` 个上报铺满整幅图:1 秒上报 = 一分钟(与 sb-manager 对应),
    /// 5 秒 = 五分钟,30 秒 = 半小时。
    #[test]
    fn one_report_advances_exactly_one_point() {
        let mut t = SpeedTracker::default();
        for i in 0..super::HISTORY_LEN {
            t.observe(i as i64 + 1, i as f64, i as f64, 1);
        }
        assert_eq!(t.history().len(), super::HISTORY_LEN, "每个上报占一格,该正好铺满");
    }

    /// 60 格而不是 120:每格宽一倍,一次左移才看得见。
    ///
    /// 120 格时一次位移只有半个字符宽,盯着看都未必察觉 —— 用户就是这么报的。
    #[test]
    fn the_window_matches_sb_manager() {
        assert_eq!(super::HISTORY_LEN, 60, "与 sb-manager 同宽,位移才明显");
    }

    /// 历史是有界的,否则一个开着几天的 TUI 会一直吃内存。
    /// 刚打开界面时还没有任何速率读数,**不该**往图上记 0 点。
    ///
    /// 记了的话,开头那几帧会是一串 `(0, 0)` —— 把「还没有可比的采样」
    /// 画成了「速率真的是 0」。这与 `up_per_sec` 用 `Option` 而不是 `0.0`
    /// 是同一条理由。
    #[test]
    fn a_frame_with_no_readings_records_nothing() {
        let mut t = SpeedTracker::default();
        // 第一帧:只有一台 agent,它上报过一次,但 TUI 还没有第二个采样点可比。
        let (up, down) = t.feed(1, NicSample { rx: 1000, tx: 2000, at: 100, boot_id: Some("b".into()) }, 100);
        assert_eq!((up, down), (None, None), "第一次见到就该是 None");
        // load_agents 把所有有读数的 agent 的速率加起来,没有读数的不参与:
        // 0 个有读数 => sum 还是 0.0,但那不是「有 0.0 速率的读数」,是「没有任何读数」。
        t.observe(100, 0.0, 0.0, 0);
        assert!(t.history().is_empty(), "没有任何读数的那一帧不该记点");
    }


    #[test]
    fn history_is_bounded() {

        let mut t = SpeedTracker::default();
        for i in 0..(HISTORY_LEN as i64 * 3) {
            t.observe(i + 1, i as f64, 0.0, 1);
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
            arch: Some("amd64".into()),
            outbound: Default::default(),
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
            nic_agent_ids: vec![],
            sub_token: "t".into(),
        };
        assert_eq!(u.used(), 500);
        assert_eq!(u.quota_ratio(), Some(0.5));
        assert_eq!(u.node_count(), 1);
    }

    // ── agent_breakdown / load_nodes 的库测 ────────────────────────────────

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-tuidata-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    /// 造一台机器 + 一个节点 + 一个分到该节点的用户,返回 (agent_id, node_id, user_id)。
    async fn fixture(p: &SqlitePool, tag: &str) -> (i64, i64, i64) {
        let (agent_id, _) = crate::db::agent_repo::create(p, tag, 0).await.unwrap();
        let (node_id, _) = crate::db::node_repo::add_node(
            p,
            agent_id,
            tag,
            crate::model::node::Protocol::VlessReality,
            8443,
            &crate::model::node::NodeParams::default(),
        )
        .await
        .unwrap();
        let user_id = crate::db::node_repo::add_user(p, "alice", 0, 0).await.unwrap();
        crate::db::node_repo::set_user_nodes(p, user_id, &[node_id]).await.unwrap();
        (agent_id, node_id, user_id)
    }

    async fn traffic(p: &SqlitePool, user_id: i64, node_id: i64, up: i64, down: i64) {
        sqlx::query(
            "INSERT INTO user_traffic
                 (user_id, node_id, cycle_up, cycle_down, total_up, total_down, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 0)",
        )
        .bind(user_id)
        .bind(node_id)
        .bind(up)
        .bind(down)
        .bind(up * 2)
        .bind(down * 2)
        .execute(p)
        .await
        .unwrap();
    }

    /// 建好但一次没跑过流量的节点**必须出现**(全 0)。
    ///
    /// 「配了没生效」正是打开这一页最想看见的一种情况 —— 内连接会把它整行藏起来,
    /// 界面上表现为「这个节点不存在」,而人刚刚才建过它。
    #[tokio::test]
    async fn agent_breakdown_lists_nodes_that_never_carried_traffic() {
        let p = pool().await;
        let (agent_id, _, _) = fixture(&p, "tokyo").await;
        crate::db::node_repo::add_node(
            &p,
            agent_id,
            "idle",
            crate::model::node::Protocol::VlessWs,
            2053,
            &crate::model::node::NodeParams::default(),
        )
        .await
        .unwrap();

        let rows = agent_breakdown(&p, agent_id).await.unwrap();
        assert_eq!(rows.len(), 2, "两个节点都要在:{rows:?}");
        let idle = rows.iter().find(|r| r.label == "idle").unwrap();
        assert_eq!(idle.cycle(), 0);
        assert!(idle.note.contains("0 人"), "没人用的节点要写明白:{}", idle.note);
    }

    /// 一个节点上多个用户的量要**加起来**,而且是**各乘完自己的倍率再加**。
    ///
    /// 不聚合的话同一个 tag 会出现好几行,看起来像建重了。
    /// 而倍率必须逐用户乘 —— 见下面那条专门盯着混合倍率的测试。
    #[tokio::test]
    async fn agent_breakdown_sums_every_user_on_the_node() {
        let p = pool().await;
        let (agent_id, node_id, alice) = fixture(&p, "tokyo").await;
        let bob = crate::db::node_repo::add_user(&p, "bob", 0, 0).await.unwrap();
        crate::db::node_repo::set_user_nodes(&p, bob, &[node_id]).await.unwrap();
        set_mult(&p, alice, 1.0).await;
        set_mult(&p, bob, 1.0).await;
        traffic(&p, alice, node_id, 100, 200).await;
        traffic(&p, bob, node_id, 30, 70).await;

        let rows = agent_breakdown(&p, agent_id).await.unwrap();
        assert_eq!(rows.len(), 1, "一个节点一行:{rows:?}");
        assert_eq!(rows[0].cycle_up, 130);
        assert_eq!(rows[0].cycle_down, 270);
        assert!(rows[0].note.contains("2 人"), "{}", rows[0].note);
    }

    /// 倍率要**逐用户**乘。
    ///
    /// 这条是整轮改动里最容易写错的一处:先 SUM 再乘一个数,
    /// 在同一个节点上混着 x1 和 x2 的用户时怎么都算不对 ——
    /// 而算错的表现只是「一个略微偏大的数字」,没人看得出来。
    #[tokio::test]
    async fn a_node_mixing_multipliers_bills_each_user_at_their_own_rate() {
        let p = pool().await;
        let (agent_id, node_id, single) = fixture(&p, "tokyo").await;
        let double = crate::db::node_repo::add_user(&p, "double", 0, 0).await.unwrap();
        crate::db::node_repo::set_user_nodes(&p, double, &[node_id]).await.unwrap();
        set_mult(&p, single, 1.0).await;
        set_mult(&p, double, 2.0).await;
        traffic(&p, single, node_id, 100, 300).await;
        traffic(&p, double, node_id, 100, 300).await;

        // 100×1 + 100×2 = 300。先加再乘的话:200×(某个平均倍率),怎么取都不是 300。
        let rows = agent_breakdown(&p, agent_id).await.unwrap();
        assert_eq!(rows[0].cycle_up, 300, "上行该是 100×1 + 100×2");
        assert_eq!(rows[0].cycle_down, 900, "下行该是 300×1 + 300×2");

        let nodes = load_nodes(&p).await.unwrap();
        assert_eq!(nodes[0].cycle_up, 300, "load_nodes 要和 agent_breakdown 同一个口径");
        assert_eq!(nodes[0].cycle_down, 900);

        // 节点二级页面:每行是一个用户,各自带自己的标记和乘过的数字。
        // 标记与名字**分开存**(渲染时颜色不同),所以这里分开断言。
        let by_user = node_breakdown(&p, node_id).await.unwrap();
        let d = by_user.iter().find(|r| r.label == "double").unwrap();
        assert_eq!(d.mult.as_deref(), Some("[2.0x]"), "名字后面要带倍率标记");
        assert_eq!((d.cycle_up, d.cycle_down), (200, 600));
        let s = by_user.iter().find(|r| r.label == "alice").unwrap();
        assert_eq!(s.mult.as_deref(), Some("[1.0x]"), "单倍也要标出来");
        assert_eq!((s.cycle_up, s.cycle_down), (100, 300));

        // 用户方向整张表同一个倍率,标记在标题里,行上不带。
        let by_node = user_breakdown(&p, double).await.unwrap();
        assert!(by_node.iter().all(|r| r.mult.is_none()), "用户视图的行不该逐行重复倍率");
    }

    async fn set_mult(p: &SqlitePool, user_id: i64, mult: f64) {
        sqlx::query("UPDATE users SET traffic_multiplier = ? WHERE id = ?")
            .bind(mult)
            .bind(user_id)
            .execute(p)
            .await
            .unwrap();
    }

    /// 别台机器的节点不能混进来。
    #[tokio::test]
    async fn agent_breakdown_only_covers_its_own_machine() {
        let p = pool().await;
        let (tokyo, _, _) = fixture(&p, "tokyo").await;
        fixture_second_machine(&p).await;
        let rows = agent_breakdown(&p, tokyo).await.unwrap();
        assert_eq!(rows.len(), 1, "只该有 tokyo 自己的节点:{rows:?}");
        assert_eq!(rows[0].label, "tokyo");
    }

    async fn fixture_second_machine(p: &SqlitePool) {
        let (agent_id, _) = crate::db::agent_repo::create(p, "osaka", 0).await.unwrap();
        crate::db::node_repo::add_node(
            p,
            agent_id,
            "osaka",
            crate::model::node::Protocol::Hysteria2,
            8444,
            &crate::model::node::NodeParams::default(),
        )
        .await
        .unwrap();
    }

    /// 仪表盘的节点视图靠 `load_nodes` 带回来的 cycle_up/down 排序。
    /// 这两列一旦忘了填,整栏会全是 0 —— 而那看起来像「真的没流量」。
    ///
    /// fixture 的新用户默认倍率 2.0,所以 111 字节的原始量到这里是 222:
    /// 这一列给界面的是**计费口径**,不是原始量(原始量那列不喂排序)。
    #[tokio::test]
    async fn load_nodes_carries_per_node_cycle_traffic() {
        let p = pool().await;
        let (_, node_id, alice) = fixture(&p, "tokyo").await;
        traffic(&p, alice, node_id, 111, 222).await;

        let rows = load_nodes(&p).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cycle_up, 222, "111 原始 × 2.0 倍率");
        assert_eq!(rows[0].cycle_down, 444, "222 原始 × 2.0 倍率");
    }

    /// **daemon 崩了之后,那盏绿灯也必须灭掉。**
    ///
    /// 主要防线在 daemon 里(`cluster::server` 的读超时)。但它要求 daemon 还活着 ——
    /// daemon 崩了或被 kill 之后,库里的 `status` 就冻在最后一刻,整屏绿灯而
    /// 一台都没连着。TUI 是另一个进程,照着 `status` 列直读会跟着一起骗人。
    #[test]
    fn a_stale_online_row_is_shown_as_offline() {
        const NOW: i64 = 1_700_000_000;
        // 心跳还新鲜 —— 保持 online。
        assert_eq!(derive_status("online", Some(NOW - 10), NOW, 90), "online");
        // 正好卡在窗口上不算过期(> 才算,别在边界上抖)。
        assert_eq!(derive_status("online", Some(NOW - 90), NOW, 90), "online");
        // 超过窗口 —— 灭灯。
        assert_eq!(derive_status("online", Some(NOW - 91), NOW, 90), "offline");
        assert_eq!(derive_status("online", Some(NOW - 86400), NOW, 90), "offline");
    }

    /// 这条规则**只会把 online 改成 offline**,不会反过来。
    ///
    /// 反过来的话,一台刚删掉又重建、last_seen 还是 NULL 的机器会被点亮,
    /// 而它根本没连上来过。
    #[test]
    fn the_staleness_rule_never_lights_a_lamp_up() {
        const NOW: i64 = 1_700_000_000;
        assert_eq!(derive_status("offline", Some(NOW), NOW, 90), "offline", "新鲜也不该点亮");
        assert_eq!(
            derive_status("offline", None, NOW, 90),
            "offline",
            "从没连上过的保持原样"
        );
        // last_seen 为 NULL 且写着 online:不改写 —— 「从没连过」与「连过又断了」
        // 是两回事,这里没有依据去判,交给 daemon(它重启时会统一重置)。
        assert_eq!(derive_status("online", None, NOW, 90), "online");
    }

    /// 时钟回拨(NTP 校时、虚拟机快照恢复)不该把在线的机器判掉。
    #[test]
    fn a_clock_going_backwards_does_not_kick_anyone_offline() {
        const NOW: i64 = 1_700_000_000;
        // last_seen 在“未来”:now - t 是负数,不该大于窗口。
        assert_eq!(derive_status("online", Some(NOW + 3600), NOW, 90), "online");
    }
}
