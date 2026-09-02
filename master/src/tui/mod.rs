//! TUI 主循环与应用状态(DESIGN.md §8)。
//!
//! 五个页面:仪表盘、服务管理(agents,两行式)、节点、用户、设置。
//! 页签里的序号就是**能直接按的键**:`1`-`5` 直达,Tab 循环。
//!
//! **它是一个独立进程,不是 daemon 的一部分。** `sbx tui` 与 `sbx daemon` 各跑各的,
//! 之间只通过 SQLite 交换状态。这带来一个直接后果:TUI 看不到 daemon 内存里的
//! 网速采样,所以它自己按刷新节奏从 `agent_nic_traffic` 做差(见 `data::SpeedTracker`)。
//!
//! 另一个后果是 TUI **只能改库,不能直接推送**:改完之后 revision 会推进,
//! 在线 agent 由 daemon 在库变更后 **1s 内**推送给它(v0.4.10);
//! 离线的在重连握手时补齐。所以每个写操作之后状态栏都会说明「什么时候生效」,
//! 免得人对着「改了但没变」发懵。

mod clip;
mod data;
mod forms;
mod modal;
mod pages;
mod settings;
/// 配色与自绘部件。`cols` / `pad` 那两个按显示列宽算的工具 `doctor` 也在用
/// —— CJK 宽度逻辑只该有一份。
pub mod theme;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::prelude::{CrosstermBackend, Terminal};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use sqlx::SqlitePool;
use std::time::Duration;

use crate::config::Config;
use crate::install;
use data::SpeedTracker;
use modal::{Action, Modal, Outcome};

const PAGES: [&str; 5] = ["仪表盘", "服务管理", "节点", "用户", "设置"];
/// 刷新间隔。1 秒足够跟上 30 秒一次的上报,又不会让 SQLite 被空转拖累。
const TICK: Duration = Duration::from_millis(1000);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Dashboard = 0,
    Agents = 1,
    Nodes = 2,
    Users = 3,
    Settings = 4,
}

impl Page {
    fn from_index(i: usize) -> Page {
        match i {
            1 => Page::Agents,
            2 => Page::Nodes,
            3 => Page::Users,
            4 => Page::Settings,
            _ => Page::Dashboard,
        }
    }
}

struct App {
    pool: SqlitePool,
    cfg: Config,
    /// 配置文件路径。设置页要就地改它(`config::set_value`)。
    cfg_path: String,
    page: Page,
    sel: [usize; 5],
    /// 仪表盘上的焦点:`false` = 左边的用户栏,`true` = 右边的节点栏。
    ///
    /// 仪表盘原本是纯只读的。加选中态是为了让那两张表也能按 Enter 下钻,
    /// 下钻本身复用节点页/用户页那套(`ShowUserNodes` / `ShowNodeUsers`)。
    dash_on_nodes: bool,
    /// 两栏各自的选中行。下标是**排过序之后**的行号
    /// (`pages::dashboard_*_order`),不是 `users` / `nodes` 里的位置。
    dash_sel: [usize; 2],
    agents: Vec<data::AgentRow>,
    nodes: Vec<data::NodeRow>,
    users: Vec<data::UserRow>,
    speed: SpeedTracker,
    modal: Option<Modal>,
    /// 后台探到的公网 IP。
    ///
    /// **必须后台探。** 它要打一次外网(见 `install::probe_public_ip` 的说明),
    /// 在按下 `a` 的那一刻同步去打,界面会冻住好几秒。开界面时就丢一个任务出去,
    /// 等人走到服务管理页再按 `a`,结果早就到了;没到就按 `resolve_host` 的
    /// 次序退回本机出口地址。
    probed_ip: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// 打开着的用量明细(节点 → 各用户 / 用户 → 各节点)。
    ///
    /// 与 `modal` 分开是因为它**是只读的、且数据要异步查**:走 `Modal` 的话
    /// 得让弹窗持有一个 pool,而弹窗那一层现在是纯渲染 + 纯按键,没有 IO。
    overlay: Option<Overlay>,
    /// 上一帧的终端高度。**按键那一侧要靠它算滚动**:一屏是多少行、滚到哪儿
    /// 算到底,都得按真实高度来,而 `on_key` 手里没有 frame。
    ///
    /// 存上一帧而不是现场问终端:两者只在「按键和改窗口大小同时发生」的那一拍
    /// 不同,而下一帧就会纠正回来 —— 为这点误差把 crossterm 的查询塞进按键路径
    /// 不划算。0 表示还没画过第一帧(测试里构造的 App 就是这样),
    /// 由 `overlay_view_h` 兜一个保守值。
    term_h: u16,
    /// 一次性消息(某个操作的结果)。为 `None` 时状态栏显示当前页的快捷键
    /// 与选中项摘要 —— 那是**常驻**信息,不该被上一次操作的回执长期占着。
    /// 还没下发完的升级指令条数。TUI 只入队,真正下发在 daemon 那边(隔一拍),
    /// 不显示的话人按完 [u] 会觉得什么都没发生。
    pending_cmds: i64,
    /// 正在等结果的 `config.check`。
    ///
    /// 校验是 **跨进程异步**的:TUI 入队 → daemon 取走 → 发给 agent → 结果写回
    /// `agent_commands.error`。所以按下 `K` 到看到结论之间隔着一拍,得把「在等」
    /// 这个状态显式拿着 —— 否则人会以为没反应,连按好几下。
    check_wait: Option<CheckWait>,
    /// 主循环下一拍要去跑主控自升级。见 `Action::SelfUpgrade`。
    want_self_upgrade: bool,
    /// 主循环下一拍要拉 `$EDITOR` 去改这台的自定义配置。见 `Action::EditAgentConfig`。
    ///
    /// 和自升级走同一个机制而不是在 `perform` 里就地做:要离开 alternate screen
    /// 得拿到 `Terminal`,而 `perform` 手上只有 `App`。
    want_edit_custom: Option<(i64, String)>,
    status: Option<String>,
    status_is_error: bool,
    quit: bool,
}

/// 一次正在跑的 `config.check`。
#[derive(Debug, Clone)]
struct CheckWait {
    cmd_id: i64,
    agent: String,
    /// 入队时刻。用来区分「还在跑」和「**daemon 根本没在跑**」——
    /// 后者会让指令永远蹲在队列里,而人对着一个不动的「校验中」无从下手。
    at: i64,
}

/// 等 daemon 取走指令的宽容限。巡检周期是 `report_interval_secs`(默认 30s),
/// 翻一倍再加一拍 —— 这个阀值宁可宽:误报「daemon 没跑」比多等一会儿要坏。
const CHECK_TAKEN_GRACE_SECS: i64 = 75;

impl App {
    fn new(pool: SqlitePool, cfg: Config, cfg_path: String) -> Self {
        Self {
            pool,
            cfg,
            cfg_path,
            page: Page::Dashboard,
            sel: [0; 5],
            dash_on_nodes: false,
            dash_sel: [0; 2],
            agents: Vec::new(),
            nodes: Vec::new(),
            users: Vec::new(),
            speed: SpeedTracker::default(),
            modal: None,
            overlay: None,
            term_h: 0,
            probed_ip: std::sync::Arc::new(std::sync::Mutex::new(None)),
            pending_cmds: 0,
            check_wait: None,
            want_self_upgrade: false,
            want_edit_custom: None,
            status: None,
            status_is_error: false,
            quit: false,
        }
    }

    /// 仪表盘当前的焦点,交给渲染层画高亮。两张表都空时不给焦点 ——
    /// 画一个停在空表上的光标只会让人以为界面卡住了。
    fn dash_focus(&self) -> Option<(bool, usize)> {
        if self.users.is_empty() && self.nodes.is_empty() {
            return None;
        }
        Some((!self.dash_on_nodes, self.dash_sel[usize::from(self.dash_on_nodes)]))
    }

    /// 仪表盘当前那一栏有几行。
    fn dash_len(&self) -> usize {
        if self.dash_on_nodes {
            self.nodes.len()
        } else {
            self.users.len()
        }
    }

    fn sel_mut(&mut self) -> &mut usize {
        // 仪表盘的选中态不在 `sel` 里 —— 它有**两栏**,各自一个光标。
        if self.page == Page::Dashboard {
            return &mut self.dash_sel[usize::from(self.dash_on_nodes)];
        }
        &mut self.sel[self.page as usize]
    }

    fn len(&self) -> usize {
        match self.page {
            // 仪表盘现在也有选中态(那两张表能按 Enter 下钻),
            // 上下键作用在**当前有焦点的那一栏**上。
            Page::Dashboard => self.dash_len(),
            Page::Agents => self.agents.len(),
            Page::Nodes => self.nodes.len(),
            Page::Users => self.users.len(),
            Page::Settings => settings::all(&self.cfg).len(),
        }
    }

    fn note(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
        self.status_is_error = false;
    }

    /// 把已经出结果的 `config.check` 变成一条状态行。每次 `refresh` 跑一遍。
    ///
    /// 四种落地:完成(成/败)、还在跑、**没人取**、指令没了。
    /// 第三种必须单独说:它意味着 daemon 没在跑,而不是校验失败 ——
    /// 两者的下一步完全不同(一个去 `systemctl status sbx`,一个去改配置)。
    /// 第四种是库被手清过或 agent 被删(级联删指令),不能就这么挂着。
    async fn poll_config_check(&mut self, now: i64) {
        let Some(w) = self.check_wait.clone() else {
            return;
        };
        match crate::db::command_repo::outcome(&self.pool, w.cmd_id).await {
            Ok(Some(o)) if o.done => {
                self.check_wait = None;
                match o.error {
                    None => self.note(format!("{} 的配置通过了 sing-box 校验", w.agent)),
                    Some(e) => self.fail(format!("{} 的配置没通过:{e}", w.agent)),
                }
            }
            Ok(Some(o)) if !o.taken && now - w.at > CHECK_TAKEN_GRACE_SECS => {
                self.check_wait = None;
                self.fail(format!(
                    "{} 的校验一直没人取走 —— daemon 大概没在跑(systemctl status sbx)",
                    w.agent
                ));
            }
            Ok(None) => {
                self.check_wait = None;
                self.fail(format!("{} 的校验指令不见了(agent 被删了?)", w.agent));
            }
            // 还在跑,或者查不动库 —— 都不要改状态行,下一拍再看。
            _ => {}
        }
    }

    fn fail(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
        self.status_is_error = true;
    }

    /// 最底下那条页脚:版本 + 规模 + **每页都一样**的那几个键。
    ///
    /// 通用键只写在这里一处。以前它们跟在每页的专有键前面(`common` 前缀),
    /// 于是「切页/选择/刷新/退出」在底栏里重复五遍,把真正会变的那部分
    /// ——当页能做什么——挤到了行尾,窄终端上正好被截掉。
    fn header_line(&self) -> String {
        format!(
            " sbx v{}  用户:{}  节点:{}  机器:{}  [1-5/Tab]切页  [↑↓/jk]选择  [R]刷新  [U]升级主控  [q]退出",
            env!("CARGO_PKG_VERSION"),
            self.users.len(),
            self.nodes.len(),
            self.agents.len(),
        )
    }

    /// 「操作」面板的摘要行:**当前选中的是什么**。
    ///
    /// 返回 1~2 行。节点页和用户页各有一批「列里放不下、但又必须看得见」的东西
    /// (完整 SNI、中转落点、订阅地址、网卡绑定),它们原来在一个单独的
    /// 「详情」面板里 —— 而那个面板的前半段和这里逐字重复,同一屏上把同一件事
    /// 说了两遍,还各占四行。现在只留这一处。
    ///
    /// 与下面那行按键放在同一个框里是有意的:「[d]删除」和「选中: alice」
    /// 隔开摆的话,按下去之前得先抬头去表里找光标在哪 —— 而那正是最不该
    /// 需要确认两次的时刻。
    ///
    /// **这里绝不能渲染密钥材料**(§11.3):`NodeParams` 里还躺着 reality 私钥、
    /// 证书私钥、ss 服务端密钥。只准取人自己填过的那几项。
    fn ops_lines(&self) -> Vec<String> {
        match self.page {
            // 仪表盘现在有选中态了,所以也要一行提示 —— 否则没人知道
            // ←/→ 能换栏、Enter 能下钻。
            Page::Dashboard => {
                if self.users.is_empty() && self.nodes.is_empty() {
                    return vec![];
                }
                let which = if self.dash_on_nodes { "节点用量" } else { "用量 Top" };
                vec![format!("  焦点: {which}   ←/→ 换栏   Enter 看明细")]
            }
            Page::Agents => match self.selected_agent() {
                // token_prefix 有实际用途:主控日志里认证失败只记前 8 位
                // (§8.1 不回显完整 token),对不上号时靠它把日志和某一行连起来。
                Some(a) => vec![format!(
                    "  选中: {}  token: {}…  节点: {} 个  状态: {}{}  网卡: {}  出站: {}{}{}",
                    a.name,
                    a.token_prefix,
                    a.node_count,
                    match a.status.as_str() {
                        "online" => "● 在线",
                        "offline" => "● 离线",
                        _ => "○ 从未连接",
                    },
                    offline_for(a, chrono::Local::now().timestamp()),
                    a.nic_accounting_mode.short(),
                    a.outbound.label(),
                    custom_note(a),
                    if self.pending_cmds > 0 {
                        format!("   ⋯ {} 条指令待下发", self.pending_cmds)
                    } else {
                        String::new()
                    }
                )],
                None => vec!["  (还没有被控服务器,按 [a] 加一台)".into()],
            },
            Page::Nodes => match self.selected_node() {
                Some(n) => {
                    let p = crate::model::node::Protocol::parse(&n.protocol);
                    let mut first = format!(
                        "  选中: {}  机器: {}  {}:{}  在用: {} 人",
                        n.tag, n.agent_name, n.protocol, n.listen_port, n.user_count
                    );
                    if forms::uses_sni(p) {
                        first.push_str(&format!(
                            "  SNI: {}",
                            n.params.server_name.as_deref().unwrap_or("(未设,下发时取默认)")
                        ));
                    }
                    if forms::uses_path(p) {
                        first.push_str(&format!(
                            "  path: {}",
                            n.params.path.as_deref().unwrap_or("(未设,下发时取默认)")
                        ));
                    }
                    // 订阅导出那一段:客户端到底连哪儿。中转时**不是**节点自身端口,
                    // 这一句丢了就会有人对着节点端口查为什么连不上。
                    let mut second = String::from("  订阅导出:");
                    match pages::relay_label(n) {
                        Some(l) => {
                            second.push_str(&format!(" 中转 {l}(客户端连这里,不是节点自身端口)"))
                        }
                        None => second.push_str(&format!(
                            " {} 的 {}",
                            n.agent_name,
                            if n.params.ipv6 { "IPv6" } else { "IPv4" }
                        )),
                    }
                    if n.params.port_reuse {
                        second.push_str("  · 端口复用(导出端口固定 443)");
                    }
                    vec![first, second]
                }
                None => vec!["  (还没有节点,按 [a] 建一个)".into()],
            },
            Page::Users => match self.selected_user() {
                Some(u) => {
                    let nodes = if u.node_count() == 0 {
                        "未分配".to_string()
                    } else {
                        format!("{} 个", u.node_count())
                    };
                    // 订阅地址是这一页最常要看的东西(要发给用户)。
                    // 撤销过的不拼地址 —— 那条 URL 一定 404,给出去只会让人
                    // 以为服务坏了(sub_modal 里同一个理由)。
                    let sub = if crate::db::node_repo::is_revoked(&u.sub_token) {
                        "(已撤销,按 [T] → [g] 恢复)".to_string()
                    } else {
                        let base = self.sub_base().trim().trim_end_matches('/');
                        if base.is_empty() {
                            format!("/sub/{}(未配 public_base,只能给路径)", u.sub_token)
                        } else {
                            format!("{base}/sub/{}", u.sub_token)
                        }
                    };
                    let first = format!(
                        "  选中: {}  节点: {}  倍率: {:.1}x  订阅: {}",
                        u.name, nodes, u.traffic_multiplier, sub
                    );
                    // 绑了网卡就必须说 —— 客户端里显示的流量和表里那个数不是一回事。
                    if !u.nic_agent_ids.is_empty() {
                        return vec![
                            first,
                            format!(
                                "  订阅响应头报的是 {} 台机器的网卡用量之和,不是这个用户自己的用量(§10.3)",
                                u.nic_agent_ids.len()
                            ),
                        ];
                    }
                    vec![first]
                }
                None => vec!["  (还没有用户,按 [a] 建一个)".into()],
            },
            Page::Settings => {
                vec!["  改的是配置文件本身,注释与排版都保留;改完要重启 daemon".into()]
            }
        }
    }

    /// 「操作」面板第二行:**这一页能按的键**。
    ///
    /// 通用键(切页/选择/刷新/退出)不在这里 —— 它们在最底下那条页脚里,
    /// 只写一处。混在一起会让它们在五个页面里重复五遍,把真正会变的那部分挤走。
    fn ops_keys(&self) -> &'static str {
        match self.page {
            Page::Dashboard => "  [←/→]换栏  [↑↓/jk]选择  [Enter]用量明细",
            Page::Agents => {
                "  [a]新增  [E]编辑  [Enter]网卡明细  [c]看配置  [C]改自定义  [K]校验  [o]出站策略  [i]接入命令  [u]升级  [r]轮换token  [d]删除"
            }
            Page::Nodes => "  [a]新增  [E]编辑  [Enter]用量明细  [d]删除",
            Page::Users => {
                "  [a]新增  [E]编辑  [Enter]明细  [n]分配节点  [b]绑网卡  [T]token  [r]重置流量  [t]启/停  [s]订阅  [d]删除"
            }
            Page::Settings => "  [Enter]改这一项",
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        let now = chrono::Local::now().timestamp();
        // 显示侧的「太久没动静就按离线算」窗口。
        //
        // 取 daemon 判半开连接的窗口(`idle_limit`)**再宽一倍还多**:
        // daemon 活着的时候永远轮不到这条规则 —— 它会先一步把 status 改对。
        // 只有 daemon 崩了、状态烂在库里没人更新时,这里才把绿灯灭掉。
        // 两个进程的判据不能反过来,否则会出现「TUI 说离线、daemon 还在下发」。
        let stale_after =
            crate::cluster::idle_limit(self.cfg.cluster.heartbeat_secs).as_secs() as i64 * 2 + 30;
        self.agents = data::load_agents(&self.pool, &mut self.speed, now, stale_after).await?;
        self.nodes = data::load_nodes(&self.pool).await?;
        self.users = data::load_users(&self.pool).await?;
        self.pending_cmds = crate::db::command_repo::pending_count(&self.pool).await.unwrap_or(0);
        self.poll_config_check(now).await;
        // 删掉最后一行之后光标会落在表外,下一帧渲染就会读到不存在的下标。
        let lens = [
            0,
            self.agents.len(),
            self.nodes.len(),
            self.users.len(),
            settings::all(&self.cfg).len(),
        ];
        for (i, len) in lens.iter().enumerate() {
            if self.sel[i] >= *len {
                self.sel[i] = len.saturating_sub(1);
            }
        }
        Ok(())
    }

    fn selected_agent(&self) -> Option<&data::AgentRow> {
        self.agents.get(self.sel[Page::Agents as usize])
    }
    fn selected_node(&self) -> Option<&data::NodeRow> {
        self.nodes.get(self.sel[Page::Nodes as usize])
    }
    fn selected_user(&self) -> Option<&data::UserRow> {
        self.users.get(self.sel[Page::Users as usize])
    }

    fn sub_base(&self) -> &str {
        &self.cfg.subscription.public_base
    }

    /// 主控地址。锁只在这一句里持有,别把它带进渲染。
    fn master_host(&self) -> String {
        let probed = self.probed_ip.lock().ok().and_then(|g| g.clone());
        install::resolve_host(&self.cfg, probed.as_deref())
    }

    /// 重新读一次配置文件。设置页写完之后要调 —— 否则页面上还是旧值,
    /// 人会以为没保存成功,然后再改一遍。
    fn reload_config(&mut self) {
        if let Ok(text) = std::fs::read_to_string(&self.cfg_path) {
            if let Ok(c) = Config::parse(&text) {
                self.cfg = c;
            }
        }
    }

    /// 二级页面一屏能显示几行。滚动的上界和翻页的步长都按它算。
    ///
    /// 还没画过第一帧时(`term_h == 0`)退回 20 行:**宁可少翻也不能多翻** ——
    /// 少翻一屏只是多按一次,多翻会直接跳过没看过的内容,而人不会察觉。
    fn overlay_view_h(&self) -> usize {
        match pages::config_view_h(self.term_h) {
            0 => 20,
            h => h,
        }
    }
}

/// 一个只读的二级页面。
struct Overlay {
    title: String,
    head: String,
    /// 表头下面的补充行。节点/用户明细不需要,留空;
    /// 网卡明细要靠它把「整机烧了多少」和下面「各节点跑了多少」并排摆出来 ——
    /// 这两个数字口径不同,分开看会得出错误结论(§6.4)。
    info: Vec<Line<'static>>,
    body: OverlayBody,
}

/// 二级页面的内容。用量明细是表,配置是长文本 —— 后者必须能滚动
/// (一份三节点的配置有两三百行,一屏放不下)。
enum OverlayBody {
    Table(Vec<data::BreakdownRow>),
    /// `scroll` 是**首行行号**。存行号而不是像素/百分比:
    /// 终端高度会变,按行号滚动在任何高度下都停在同一行内容上。
    Text {
        lines: Vec<String>,
        scroll: usize,
    },
}

impl Overlay {
    fn table(title: String, head: String, rows: Vec<data::BreakdownRow>) -> Self {
        Self { title, head, info: vec![], body: OverlayBody::Table(rows) }
    }

    /// 滚动。`view_h` 是当前能显示几行 —— 传进来而不是存起来,
    /// 因为它随终端尺寸变,存下来就会在改窗口大小之后失准。
    ///
    /// 上界是 `len - view_h`:滚到底时最后一行贴着底边。
    /// 不设上界的话可以一直按下去,直到整屏空白 —— 那时人会以为内容没了。
    fn scroll_by(&mut self, delta: i64, view_h: usize) {
        if let OverlayBody::Text { lines, scroll } = &mut self.body {
            let max = lines.len().saturating_sub(view_h.max(1));
            *scroll = (*scroll as i64 + delta).clamp(0, max as i64) as usize;
        }
    }
}

/// 进入 TUI。返回时终端一定已经恢复原状(正常退出、出错、panic 三条路都覆盖)。
pub async fn run(pool: SqlitePool, cfg: Config, cfg_path: String) -> Result<()> {
    // panic 时也要把终端恢复回去。少了这一步,一次 panic 会让用户的终端
    // 卡在 raw mode + alternate screen —— 看起来像整个 shell 挂了。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        default_hook(info);
    }));

    enable_raw_mode().context("进入 raw mode 失败(当前不是一个交互终端?)")?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = event_loop(&mut term, App::new(pool, cfg, cfg_path)).await;

    restore_terminal()?;
    let _ = term.show_cursor();
    result
}

/// 挂起 TUI 去跑一键安装脚本,跑完再把界面接回来。
///
/// **必须挂起**:脚本会打十几行进度(下载、校验、替换、restart),
/// 而 alternate screen + raw mode 下那些输出要么看不见、要么排版全乱。
/// 出问题时那几行恰恰是唯一的线索。
///
/// 做法照 sb-manager 的 `run_self_update`:离开 alternate screen → 跑 → 回来。
/// 无论脚本成功与否都要**把终端接回来** —— 半路 return 会把人扔在一个
/// raw mode 没关的 shell 里,那时连 Ctrl-C 都不一定好使。
///
/// 跑完**不自动重启自己**:当前进程用的还是老二进制,要重进 TUI 才会加载新的。
/// 悄悄替换掉再假装没事,比明说更糟。
fn run_self_upgrade<B: ratatui::backend::Backend>(term: &mut Terminal<B>) -> Result<String> {
    restore_terminal()?;
    let _ = term.show_cursor();

    println!();
    println!("=== 升级 sbx(当前 v{}) ===", crate::upgrade::target_version());
    println!("脚本会比对版本,已是最新就什么都不做;有新版会下载、校验 sha256、替换。");
    println!();

    let cmd = format!("curl -fsSL {} | bash", crate::install::INSTALL_URL);
    let status = std::process::Command::new("sh").arg("-c").arg(&cmd).status();

    println!();
    println!("按 Enter 回到界面…");
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);

    // 先把终端接回来,再判断脚本的结果 —— 顺序反过来的话,
    // 脚本失败时会从这里 return,把人扔在一个 raw mode 没关的 shell 里。
    enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    term.clear()?;

    match status {
        Ok(s) if s.success() => {
            Ok("升级脚本跑完了。**要按 [q] 退出再重进 TUI**,当前进程还是老二进制".into())
        }
        Ok(s) => anyhow::bail!("脚本非零退出(exit {:?}),上面那几行有原因", s.code()),
        Err(e) => anyhow::bail!("起不来安装脚本:{e}"),
    }
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

/// 拉编辑器去改一台 agent 的自定义配置,回来后校验并存库。
///
/// **为什么用外部编辑器而不是在 TUI 里写一个。** 要改的是一段 JSON,
/// 而设置页那种「一项一个输入框」的形态放不下它;ratatui 也没有现成的多行
/// 编辑组件。而人手上那个编辑器有高亮、有撤销、有他用习惯了的键位。
///
/// 挂起/接回终端的做法照 `run_self_upgrade` —— 包括那条教训:
/// **无论成败都要把终端接回来**,半路 return 会把人扒在一个 raw mode 没关的 shell 里。
async fn edit_custom_config<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    pool: &SqlitePool,
    id: i64,
    name: &str,
) -> Result<String> {
    let editor = pick_editor(id)?;

    let existing = crate::db::agent_repo::custom_config(pool, id).await?;
    let seed = match &existing {
        Some(raw) => raw.clone(),
        None => custom_config_template(pool, id, name).await?,
    };

    let path = std::env::temp_dir().join(format!("sbx-custom-{id}-{}.jsonc", uuid::Uuid::new_v4()));
    std::fs::write(&path, &seed)?;

    restore_terminal()?;
    let _ = term.show_cursor();
    // 落到 vi 族时先把退出方法说清,并等人按一下回车。
    //
    // 归根结底是因为这个提示**没处可放**:TUI 已经挂起,而编辑器一启动就会
    // 盖掉屏幕上任何东西。删掉这一步的代价是把不会用 vi 的人扒进一个
    // 退不出来的界面 —— 而那时候他的 TUI 也回不了。
    if editor.needs_exit_hint {
        println!();
        println!("没设 $EDITOR,自动挑了这台机器上的 {}。", editor.cmd);
        println!("  存盘退出:按 Esc,再输 :wq 回车");
        println!("  不改了:  按 Esc,再输 :q! 回车");
        println!("  不想用它:退出 TUI 后 `export EDITOR=nano` 再进来");
        println!();
        print!("按回车打开…");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut discard = String::new();
        let _ = std::io::stdin().read_line(&mut discard);
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{} {}", editor.cmd, path.display()))
        .status();
    // 先把终端接回来,再判断结果。顺序反过来的话,编辑器异常退出时会从这里
    // return,把人扒在一个 raw mode 没关的 shell 里。
    enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    term.clear()?;

    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => anyhow::bail!("编辑器非零退出(exit {:?}),没改库", s.code()),
        Err(e) => anyhow::bail!("起不来 {}:{e}", editor.cmd),
    }
    let edited = edited?;

    // 校验在存库**之前**。存了再验的话,一份写坏的片段会跟着 revision 下发出去,
    // 而人要从 `agent_events` 里的 config_apply_failed 才能发现。
    let obj = crate::service::validate_custom(&edited)?;
    if obj.is_empty() {
        if existing.is_none() {
            return Ok(format!("{name} 的自定义配置没变(本来就没有)"));
        }
        let rev = crate::db::agent_repo::set_custom_config(pool, id, None).await?;
        return Ok(format!(
            "清空了 —— {name} 恢复成默认配置(rev {rev}),下次下发后只剩 direct 出站"
        ));
    }
    if existing.as_deref() == Some(edited.as_str()) {
        return Ok(format!("{name} 的自定义配置没改动"));
    }
    let rev = crate::db::agent_repo::set_custom_config(pool, id, Some(&edited)).await?;
    Ok(format!(
        "已存 {name} 的自定义配置:{}(rev {rev})。按 [K] 让它自己的 sing-box 再验一次",
        obj.keys().cloned().collect::<Vec<_>>().join(" / ")
    ))
}

/// 选定的编辑器,以及进去之前要不要先告诉人怎么退出。
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorChoice {
    cmd: String,
    /// vi 族且**不是人自己指定的**时为真。
    ///
    /// 自己 `export EDITOR=vim` 的人不需要被教怎么退 vim,多一步回车只是碍事。
    /// 而被自动挑中的人可能压根没用过它。
    needs_exit_hint: bool,
}

/// 按这个顺序挑编辑器。**排序的依据是「新手能不能自己退出来」**,
/// 不是好用程度:
///   * `nano` / `micro` 把按键写在屏幕底下(Ctrl-X / Ctrl-Q),不需要额外提示;
///   * `vi` 族放最后,而且落到它们时要先把退出方法说清。
///
/// Alpine 上通常只有 busybox 的 `vi`,Debian/Ubuntu 默认带 `nano` ——
/// 所以带提示的那条路主要是给 Alpine 走的。
const EDITOR_CANDIDATES: &[&str] = &["nano", "micro", "nvim", "vim", "vi"];

fn is_vi_family(cmd: &str) -> bool {
    // 只看第一个词:`EDITOR="vim -u NONE"` 这种带参数的很常见。
    let bin = cmd.split_whitespace().next().unwrap_or("");
    let bin = bin.rsplit('/').next().unwrap_or(bin);
    matches!(bin, "vi" | "vim" | "nvim" | "view" | "vimdiff")
}

/// 先听人的,再看机器上有什么。
///
/// **为何不是「没设 $EDITOR 就报错」。** 那是这个功能刚上线时的做法,理由是
/// 「不猜编辑器,别把不会用 vi 的人扒进退不出来的界面」。真机上立即碰到了
/// 比那个风险更大的问题:**干净的 VPS 上 `$EDITOR` 本来就没设**,于是
/// 整个功能直接不可达。
///
/// 而且当时那句提示让人「先 export 再重进这一页」—— **那是错的**:
/// 环境变量读的是本进程的,外面改不了一个已经在跑的 TUI。照那句做一定
/// 失败,而人会得出「这功能坏的」这个结论。
///
/// 现在的做法两边都要:挑一个**真的装了的**编辑器,优先挑能自己退出来的;
/// 实在只剩 vi 族就先把退出方法说清。一个都没有才报错。
fn pick_editor(id: i64) -> Result<EditorChoice> {
    // `VISUAL` 在前:惯例里它是全屏编辑器,`EDITOR` 可能是 `ed` 这种行编辑器。
    // git 的优先级也是 VISUAL 在 EDITOR 之前。
    let explicit = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|s| !s.trim().is_empty());
    pick_editor_from(explicit.as_deref(), which, id)
}

/// `pick_editor` 的纯那一半:环境变量与「装了没装」都从参数进来。
///
/// 拆开的理由是**可测性**,而且这一次是被教训推着拆的:上一版测试直接改
/// 进程的 `EDITOR` / `PATH` 来造场景,而 `cargo test` 默认多线程同进程 ——
/// 把 `PATH` 清空的那一瞬,任何并行跑着、会 `sh -c` 出去的测试(doctor 那几条
/// 就是)都可能莫名挂掉。那种失败跟不上任何代码改动,排起来特别费劳。
fn pick_editor_from(
    explicit: Option<&str>,
    found: impl Fn(&str) -> bool,
    id: i64,
) -> Result<EditorChoice> {
    if let Some(cmd) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(EditorChoice { cmd: cmd.to_string(), needs_exit_hint: false });
    }
    for cand in EDITOR_CANDIDATES {
        if found(cand) {
            return Ok(EditorChoice {
                cmd: (*cand).to_string(),
                needs_exit_hint: is_vi_family(cand),
            });
        }
    }
    anyhow::bail!(
        "这台机器上一个编辑器都没有(找过 {})。三条路任选:\n\
         · 装一个:apk add nano 或 apt install -y nano\n\
         · 指定一个:按 [q] 退出 TUI,`export EDITOR=…` 后重新 `sbx tui`\n\
           (环境变量对已经在跑的进程没用,必须重进)\n\
         · 不用编辑器:sbx agent-config-set {id} 片段.json(也可以 `-` 读 stdin)",
        EDITOR_CANDIDATES.join(" / ")
    )
}

/// 这个命令在 PATH 里吗。
///
/// 用 `command -v` 而不是自己扫 PATH:它能同时认 shell 内建、函数和 alias,
/// 而且下面真正启动编辑器走的也是 `sh -c` —— **两边用同一个解释器判断**,
/// 否则会出现「挑中了但起不来」。
fn which(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 第一次打开编辑器时的初始内容。
///
/// **当前生效的配置只作为注释放进去,不作为内容。** 这一条很关键:
/// 预填成实际内容的话,人「打开看一眼、按保存」就意外接管了
/// `default_domain_resolver`,于是 `[o]` 静默失效。现在原样保存 = 仍然是空 = 什么都没变。
///
/// sing-box 接受 `//`、`#`、`/* */` 和尾随逗号(实测确认过),所以这份带注释的
/// 模板原样存下去也能过 —— 不需要让人先把注释删干净。
/// 模板里带的示例 —— 独立成常量，用**原始字符串**：
/// JSON 里四处都是大括号，塞进 `format!` 的字符串等于每对都要转义成 `{{}}`，
/// 读的人是看不出原文的。原始字符串里它们就是自己。
const CUSTOM_EXAMPLE: &str = r#"/*
{
  "dns": { "servers": [{ "type": "local", "tag": "local" }] },
  // 1.14 起远程 rule-set 的下载通道走顶层 http_clients(旧的 download_detour 已弃用)
  "http_clients": [{ "tag": "fetch", "detour": "direct" }],
  "outbounds": [
    { "type": "direct", "tag": "direct-v6",
      "domain_resolver": { "server": "local", "strategy": "ipv6_only" } }
  ],
  "route": {
    "default_http_client": "fetch",
    "rule_set": [
      { "tag": ["ai"], "type": "remote", "format": "binary",
        "url": "https://github.com/DustinWin/ruleset_geodata/releases/download/sing-box-ruleset/{tag}.srs",
        "http_client": "fetch" }
    ],
    "rules": [
      { "rule_set": ["ai"], "action": "route", "outbound": "direct-v6" },
      { "protocol": "bittorrent", "action": "reject" }
    ],
    "final": "direct"
  },
  // 不开这个,每次 config.apply 都会重下一遍全部规则集。path 必须写:
  // sing-box 缺省是相对路径 cache.db,agent 的 systemd unit 是 ProtectSystem=strict,
  // 只除了 StateDirectory 别处全只读 —— 相对路径必然启动失败
  "experimental": { "cache_file": { "enabled": true, "path": "/var/lib/sbx-agent/cache.db" } }
}
*/"#;

async fn custom_config_template(pool: &SqlitePool, id: i64, name: &str) -> Result<String> {
    let effective = crate::service::build_agent_config(pool, id).await?;
    // 当前的出站策略只用一行交代。它值得占这一行,是因为人在下面写
    // `route.default_domain_resolver` 就等于接管它 —— 不说的话那是个静默的意外。
    let now = match effective
        .get("route")
        .and_then(|r| r.get("default_domain_resolver"))
        .and_then(|d| d.get("strategy"))
        .and_then(|s| s.as_str())
    {
        Some(s) => {
            format!("【o】出站策略现在是 {s};你写 route.default_domain_resolver 就接管它")
        }
        None => "【o】出站策略现在是自动(没写 default_domain_resolver)".to_string(),
    };
    Ok(format!(
        "// {name} 的自定义片段。可写 outbounds / route / dns / http_clients / experimental
// outbounds 是追加,tag 不能叫 direct。inbounds 归主控(记账靠 inbound tag),在「节点」页加减。
// 注释与尾随逗号都行;清空存盘 = 恢复默认;存完按【K】让它自己的 sing-box 验一遍。
// {now}
//
// 下面是个例子:AI 站点走 IPv6、屏蔽 BT。要用就把 /* 和 */ 两行删掉,
// 并把最后那个空的 {{}} 一并删掉;不用就整段删掉。
{example}
{{
}}
",
        example = CUSTOM_EXAMPLE
    ))
}

async fn event_loop<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    mut app: App,
) -> Result<()> {
    // 输入放在单独的线程里:crossterm 的 poll/read 是阻塞的,直接在 async
    // 循环里调用会把 runtime 的工作线程按住。走 channel 之后,主循环可以用
    // select 同时等按键和刷新计时。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // `input_paused`:拉外部程序(vim / 升级脚本)期间必须把键盘线程停住 ——
    // 子进程和这个线程共用同一个 tty,谁先醒谁拿走字节。不停的话 vim 里
    // 按键要输两遍;更糟的是被偷走的键排在 channel 里,编辑器一关会**回放成
    // TUI 的随机操作**(vim 里敲的 :wq,被偷走的 q 就是 TUI 的退出键)。
    let input_paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let paused_in_thread = input_paused.clone();
    std::thread::spawn(move || loop {
        if paused_in_thread.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }
        match event::poll(Duration::from_millis(200)) {
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        break; // 主循环退出了
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });

    // 公网地址后台探一次。失败不影响任何东西 —— resolve_host 会按次序退回去。
    let slot = app.probed_ip.clone();
    tokio::spawn(async move {
        if let Some(ip) = install::probe_public_ip().await {
            if let Ok(mut g) = slot.lock() {
                *g = Some(ip);
            }
        }
    });

    app.refresh().await?;
    let mut ticker = tokio::time::interval(TICK);

    loop {
        // 记下这一帧的高度给按键那一侧算滚动(见 `App::term_h`)。
        // 取 `draw` 实际画到的区域,而不是另外去问一次终端 —— 改窗口大小时
        // 这两个值会有一拍不一样,而滚动要跟着**看到的**那一屏走。
        app.term_h = term.draw(|f| draw(f, &app))?.area.height;
        if app.quit {
            return Ok(());
        }

        tokio::select! {
            Some(ev) = rx.recv() => {
                if let Event::Key(k) = ev {
                    // Windows 上按下和抬起各来一次,不过滤会让每个按键生效两遍。
                    if k.kind == KeyEventKind::Press {
                        if let Some(action) = on_key(&mut app, k) {
                            perform(&mut app, action).await;
                            if app.want_self_upgrade {
                                app.want_self_upgrade = false;
                                input_paused.store(true, std::sync::atomic::Ordering::SeqCst);
                                let msg = run_self_upgrade(term);
                                match msg {
                                    Ok(m) => app.note(m),
                                    Err(e) => app.fail(format!("升级失败: {e}")),
                                }
                                resume_input(&input_paused, &mut rx);
                            }
                            // 与自升级同一个做法:要挂起终端的事必须在主循环里做,
                            // `perform` 里拿不到 `term`。
                            if let Some((id, name)) = app.want_edit_custom.take() {
                                input_paused.store(true, std::sync::atomic::Ordering::SeqCst);
                                match edit_custom_config(term, &app.pool, id, &name).await {
                                    Ok(m) => app.note(m),
                                    Err(e) => app.fail(format!("{name} 的自定义配置没存上: {e}")),
                                }
                                resume_input(&input_paused, &mut rx);
                            }
                            app.refresh().await?;
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                if let Err(e) = app.refresh().await {
                    app.fail(format!("刷新失败: {e}"));
                }
            }
        }
    }
}

/// 外部程序(编辑器 / 升级脚本)退场后恢复键盘线程。
///
/// 先把编辑期间被它偷走的键从 channel 里排掉、再放行 —— 顺序反过来的话,
/// 刚放行就可能读到新键,和要排掉的旧键分不开;而那些旧键里完全可能有
/// TUI 的退出键(人在 vim 里敲的 `:wq`),放给主循环就是随机操作。
fn resume_input(
    input_paused: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<crossterm::event::Event>,
) {
    while rx.try_recv().is_ok() {}
    input_paused.store(false, std::sync::atomic::Ordering::SeqCst);
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    // 页签占**两行**:一行文字 + 一行下边框。给 1 行的话 `Borders::BOTTOM`
    // 会把那唯一一行吃掉,结果是页签整条消失、只剩一条横线 —— 这正是 v0.3.0
    // 换成 ratatui `Tabs` 之后出的回归(tabs_are_actually_visible 盯着它)。
    // 四段,自上而下:页签(2) / 主体 / 「操作」面板(4) / 全局状态栏(1)。
    //
    // 顺序照 sb-manager:**版本那一行在最底下**。它是整个界面的页脚 ——
    // 版本号、规模、以及哪儿都能按的那几个键。把它夹在主体和操作栏中间
    // (上一版就是这么错的)会让人以为它属于当页的操作。
    //
    // 「操作」是一个**带边框的面板**而不是一行裸文字:第一行是选中项摘要
    // (在哪个用户/节点上、它的订阅地址),第二行才是这一页能按的键。
    // 摘要必须和键放在一起 —— 「[d]删除」和「选中: alice」隔开摆的话,
    // 按下去之前得先抬头去表里找光标在哪。
    // 「操作」面板的高度跟着内容走:上下边框 2 + 摘要 n 行 + 按键 1 行。
    // **仪表盘给 0** —— 那是只读页,没有专有操作,摆一个只写着
    // 「只读页,要动手请去别处」的空框纯属浪费四行(sb-manager 的仪表盘也没有)。
    let ops = app.ops_lines();
    let ops_h = if ops.is_empty() { 0 } else { ops.len() as u16 + 3 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(ops_h),
            Constraint::Length(1),
        ])
        .split(f.area());

    modal::tabs(f, chunks[0], &PAGES, app.page as usize);
    let now = chrono::Local::now().timestamp();
    match app.page {
        Page::Dashboard => pages::dashboard(
            f,
            chunks[1],
            &pages::Dash {
                agents: &app.agents,
                nodes: &app.nodes,
                users: &app.users,
                history: app.speed.history(),
                now,
                focus: app.dash_focus(),
            },
        ),
        Page::Agents => pages::agents(f, chunks[1], &app.agents, app.sel[1], now),
        Page::Nodes => pages::nodes(f, chunks[1], &app.nodes, app.sel[2]),
        Page::Users => pages::users(f, chunks[1], &app.users, app.sel[3], app.sub_base(), now),
        Page::Settings => pages::settings(f, chunks[1], &settings::all(&app.cfg), app.sel[4]),
    }
    if ops_h > 0 {
        modal::ops_panel(
            f,
            chunks[2],
            &ops,
            app.ops_keys(),
            app.status.as_deref(),
            app.status_is_error,
        );
    }
    modal::info_bar(f, chunks[3], &app.header_line());

    if let Some(o) = &app.overlay {
        match &o.body {
            OverlayBody::Table(rows) => {
                pages::breakdown(f, f.area(), &o.title, &o.head, &o.info, rows)
            }
            // 渲染是只读的:滚动越界由按键那一侧夹住(见 `scroll_by`)。
            // 这里改状态的话,draw 就得拿 `&mut App`,而它每秒跑一次、
            // 还要在 panic hook 里用 —— 不值得为一个夹取范围把签名改掉。
            OverlayBody::Text { lines, scroll } => {
                pages::config_text(f, f.area(), &o.title, &o.head, lines, *scroll);
            }
        }
    }
    if let Some(m) = &app.modal {
        modal::render(f, f.area(), m);
    }
}

/// 处理一次按键。返回 `Some(action)` 表示要执行一个写操作。
fn on_key(app: &mut App, k: KeyEvent) -> Option<Action> {
    // 弹窗打开时吃掉全部按键 —— 否则「输入名字」会顺带触发页面快捷键。
    if let Some(m) = &mut app.modal {
        return match m.handle(k) {
            Outcome::Stay => None,
            Outcome::Close(msg) => {
                app.modal = None;
                if let Some(msg) = msg {
                    app.note(msg);
                }
                None
            }
            Outcome::Run(a) => {
                app.modal = None;
                Some(a)
            }
        };
    }
    // 二级页面是只读的。放在页面快捷键**之前** —— 否则它开着的时候
    // 按 d 会去删掉后面那张表里选中的东西。
    //
    // 可视行数要在借 `overlay` 之前算完:`overlay_view_h` 读的是 `app`,
    // 而下面那一句把 `app` 整个可变借走了。
    let view_h = app.overlay_view_h();
    if let Some(o) = &mut app.overlay {
        // 明细表一屏放得下,任意键关掉。
        // 配置是长文本,**方向键必须留给滚动** —— 否则想往下看一眼就把页面关了,
        // 而人会以为是崩了。它只认 Esc / q / Enter。
        let scrollable = matches!(o.body, OverlayBody::Text { .. });
        if !scrollable {
            app.overlay = None;
            return None;
        }
        // 翻页留**一行重叠**:新的一屏顶上那行是上一屏的末行。
        // 整屏跳的话前后两屏之间没有共同的一行,在两百行的 JSON 里
        // 很容易接不上("刚才那个 inbound 讲完了吗")。
        let page = (view_h as i64 - 1).max(1);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.overlay = None,
            KeyCode::Down | KeyCode::Char('j') => o.scroll_by(1, view_h),
            KeyCode::Up | KeyCode::Char('k') => o.scroll_by(-1, view_h),
            KeyCode::PageDown | KeyCode::Char(' ') => o.scroll_by(page, view_h),
            KeyCode::PageUp => o.scroll_by(-page, view_h),
            KeyCode::Home | KeyCode::Char('g') => o.scroll_by(i64::MIN / 2, view_h),
            KeyCode::End | KeyCode::Char('G') => o.scroll_by(i64::MAX / 2, view_h),
            _ => {}
        }
        return None;
    }
    // 上一次操作的回执只留到下一次按键为止,之后让位给常驻的快捷键提示。
    app.status = None;

    match k.code {
        // 大写 U:升主控自己。小写 u 在服务管理页是「升级 agent」——
        // 两者影响面差一个数量级(一台被控 vs 主控自己),所以差一个 Shift
        // 但**分属不同作用域**:U 是全局的,u 只在那一页有。
        KeyCode::Char('U') => {
            app.modal = Some(Modal::confirm(
                "升级主控",
                vec![
                    format!(
                        "当前 v{}。脚本会比对版本,已是最新就什么都不做。",
                        crate::upgrade::target_version()
                    ),
                    "界面会临时退出去跑脚本,跑完自动回来。".into(),
                    "**新二进制要重进 TUI 才生效** —— 当前这个进程还是老的。".into(),
                    "daemon 由脚本自己 restart(它本来就在跑的话)。".into(),
                ],
                Action::SelfUpgrade,
            ));
        }
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
        // 数字直达。五页里跳来跳去时,Tab 要按四下才到得了最后一页,
        // 而人心里想的是「去第 5 页」。
        //
        // 页签上印的序号是 `i + 1`,所以这里要减 1 —— 按 3 去的是第三个页签(节点),
        // 不是下标 3 的那一页。
        KeyCode::Char(c @ '1'..='5') => {
            app.page = Page::from_index(c as usize - '1' as usize);
        }
        // 常规刷新是每秒一次,但有些东西(改完配置、另一个进程动了库)
        // 要立刻看到结果。`R` 是大写的:小写 r 在服务管理页是「轮换 token」,
        // 那是一个不可撤销的动作,不能和刷新只差一个 Shift 却又长得像。
        KeyCode::Char('R') => return Some(Action::Refresh),
        // 仪表盘上 ←/→ 换的是**哪一栏有焦点**(左用户 / 右节点)——
        // 那是它们在这一页最自然的含义,两张表就是左右摆着的。
        // 换页仍然走 Tab 和 1-5,页脚里写的也是这两个。
        KeyCode::Right | KeyCode::Char('l') if app.page == Page::Dashboard => {
            app.dash_on_nodes = true;
        }
        KeyCode::Left | KeyCode::Char('h') if app.page == Page::Dashboard => {
            app.dash_on_nodes = false;
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            app.page = Page::from_index((app.page as usize + 1) % PAGES.len());
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
            app.page = Page::from_index((app.page as usize + PAGES.len() - 1) % PAGES.len());
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let len = app.len();
            if len > 0 {
                let s = app.sel_mut();
                *s = (*s + 1) % len;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let len = app.len();
            if len > 0 {
                let s = app.sel_mut();
                *s = if *s == 0 { len - 1 } else { *s - 1 };
            }
        }
        _ => return page_key(app, k),
    }
    None
}

fn page_key(app: &mut App, k: KeyEvent) -> Option<Action> {
    match app.page {
        // 仪表盘的两张表也能下钻,复用节点页/用户页那套明细。
        //
        // 选中的下标是**排序后**的行号,必须经同一份次序解回原始行 ——
        // 两边各排一次的话,光标停在第 1 行、打开的却是另一个人,而且不会报错。
        Page::Dashboard => {
            if matches!(k.code, KeyCode::Enter | KeyCode::Char('v')) {
                if app.dash_on_nodes {
                    let order = pages::dashboard_node_order(&app.nodes);
                    match order.get(app.dash_sel[1]).and_then(|&i| app.nodes.get(i)) {
                        Some(n) => {
                            return Some(Action::ShowNodeUsers {
                                id: n.id,
                                tag: n.tag.clone(),
                                agent: n.agent_name.clone(),
                            })
                        }
                        None => app.fail("这一栏还没有节点"),
                    }
                } else {
                    let order = pages::dashboard_user_order(&app.users);
                    match order.get(app.dash_sel[0]).and_then(|&i| app.users.get(i)) {
                        Some(u) => {
                            return Some(Action::ShowUserNodes { id: u.id, name: u.name.clone() })
                        }
                        None => app.fail("这一栏还没有用户"),
                    }
                }
            }
        }

        Page::Agents => match k.code {
            KeyCode::Char('a') => app.modal = Some(forms::agent_add()),
            KeyCode::Char('E') => match app.selected_agent() {
                Some(a) => app.modal = Some(agent_edit(a)),
                None => app.fail("没有选中任何被控服务器"),
            },
            // 「这台机器网卡烧了多少、都是哪个节点在跑」。厂商按网卡计费,
            // 所以这是每月对账时问的第一个问题 —— 与节点页/用户页的 Enter 同一个手势。
            KeyCode::Enter | KeyCode::Char('v') => match app.selected_agent() {
                Some(a) => {
                    let (id, name) = (a.id, a.name.clone());
                    return Some(Action::ShowAgentNics { id, name });
                }
                None => app.fail("没有选中任何被控服务器"),
            },
            // 「这台机器上 sing-box 到底跑的是什么」。排查「改了没生效」时,
            // 这是唯一能把「主控以为的」和「机器上实际的」对上的地方。
            KeyCode::Char('c') => match app.selected_agent() {
                Some(a) => {
                    let (id, name) = (a.id, a.name.clone());
                    return Some(Action::ShowAgentConfig { id, name });
                }
                None => app.fail("没有选中任何被控服务器"),
            },
            // 把同一份配置交给那台机器自己的 sing-box 试建一次。
            //
            // `K` 而不是 `k`:小写 `k` 是向上选择。误按 `K` 没代价 ——
            // 这个动作是只读的,不会碰到正在跑的 box。
            KeyCode::Char('K') => match app.selected_agent() {
                Some(a) => {
                    let (id, name) = (a.id, a.name.clone());
                    return Some(Action::CheckAgentConfig { id, name });
                }
                None => app.fail("没有选中任何被控服务器"),
            },
            // `c` 看、`C` 改 —— 改的是自定义追加段,不是 `[c]` 那份产物全文。
            KeyCode::Char('C') => match app.selected_agent() {
                Some(a) => {
                    let (id, name) = (a.id, a.name.clone());
                    return Some(Action::EditAgentConfig { id, name });
                }
                None => app.fail("没有选中任何被控服务器"),
            },
            KeyCode::Char('i') => match app.selected_agent() {
                Some(a) => {
                    let (id, name) = (a.id, a.name.clone());
                    return Some(Action::ShowInstall { id, name });
                }
                None => app.fail("没有选中任何被控服务器"),
            },
            // `o` 循环切下一个策略。做成一个键循环而不是弹窗选单,是因为
            // 它只有五个取值、而且每次改都要立刻看到效果 —— 开个框选完再关,
            // 比按两下 o 慢得多。误按的代价也小:再按几下就转回来了,
            // 而且它只改解析策略,不动任何凭据。
            KeyCode::Char('o') => match app.selected_agent() {
                Some(a) => {
                    let all = crate::model::outbound::OutboundStrategy::all();
                    let cur = all.iter().position(|s| *s == a.outbound).unwrap_or(0);
                    let next = all[(cur + 1) % all.len()];
                    return Some(Action::SetOutbound {
                        id: a.id,
                        name: a.name.clone(),
                        strategy: next,
                    });
                }
                None => app.fail("没有选中任何被控服务器"),
            },
            // 小写 u:升级 agent。大写 U 是主控自升级(全局键)——
            // 两者影响面差一个数量级(一台被控 vs 主控自己),不该共用一个键。
            KeyCode::Char('u') => match app.selected_agent() {
                Some(a) => {
                    let online = app.agents.iter().filter(|x| x.status == "online").count();
                    app.modal = Some(Modal::Upgrade {
                        agent_id: a.id,
                        name: a.name.clone(),
                        online,
                        version: crate::upgrade::target_version().to_string(),
                    });
                }
                None => app.fail("没有选中任何被控服务器"),
            },
            KeyCode::Char('r') => {
                if let Some(a) = app.selected_agent() {
                    let (id, name) = (a.id, a.name.clone());
                    app.modal = Some(Modal::confirm(
                        "轮换 token",
                        vec![
                            format!("将为 {name} 生成新 token,旧的立即失效。"),
                            "已建立的连接不会被立刻踢掉,下次重连时才生效(§8.1)。".into(),
                            "新 token 只会显示这一次 —— 连同一条可以直接跑的重装命令。".into(),
                        ],
                        Action::RotateToken { id, name },
                    ));
                } else {
                    app.fail("没有选中任何被控服务器");
                }
            }
            KeyCode::Char('d') => {
                if let Some(a) = app.selected_agent() {
                    let (id, name, nodes) = (a.id, a.name.clone(), a.node_count);
                    app.modal = Some(Modal::confirm(
                        "删除被控服务器",
                        vec![
                            format!("将删除 {name},以及它名下的 {nodes} 个节点。"),
                            "这些节点上的用户分配会一并清除,且不可撤销。".into(),
                        ],
                        Action::DeleteAgent { id, name },
                    ));
                } else {
                    app.fail("没有选中任何被控服务器");
                }
            }
            _ => {}
        },

        Page::Nodes => match k.code {
            KeyCode::Char('a') => {
                if app.agents.is_empty() {
                    app.fail("先在「服务管理」页(按 2)加一台被控服务器");
                    return None;
                }
                // 默认选中在服务管理页选着的那台 —— 多数时候人刚从那页过来。
                let preselect = app.sel[Page::Agents as usize].min(app.agents.len() - 1);
                app.modal = Some(forms::node_add(&app.agents, preselect));
            }
            KeyCode::Char('E') => match app.selected_node() {
                Some(n) => app.modal = Some(forms::node_edit(n)),
                None => app.fail("没有选中任何节点"),
            },
            // 「这个节点上都有谁、各跑了多少」。这是排查「某台机器流量异常」
            // 时的第一个问题,而在此之前只能去用户页一个个看。
            KeyCode::Enter | KeyCode::Char('v') => match app.selected_node() {
                Some(n) => {
                    return Some(Action::ShowNodeUsers {
                        id: n.id,
                        tag: n.tag.clone(),
                        agent: n.agent_name.clone(),
                    })
                }
                None => app.fail("没有选中任何节点"),
            },
            KeyCode::Char('d') => {
                if let Some(n) = app.selected_node() {
                    let (id, tag, users) = (n.id, n.tag.clone(), n.user_count);
                    app.modal = Some(Modal::confirm(
                        "删除节点",
                        vec![
                            format!("将删除节点 #{id} {tag}。"),
                            format!("{users} 个用户对它的分配会一并清除。"),
                        ],
                        Action::DeleteNode { id, tag },
                    ));
                } else {
                    app.fail("没有选中任何节点");
                }
            }
            _ => {}
        },

        Page::Users => match k.code {
            KeyCode::Char('a') => app.modal = Some(forms::user_add()),
            KeyCode::Char('E') => match app.selected_user() {
                Some(u) => app.modal = Some(forms::user_edit(u)),
                None => app.fail("没有选中任何用户"),
            },
            // 「这个用户在哪几个节点上、各跑了多少」。跨 agent 的用户
            // 在列表里只看得到一个合计,分不出是哪台机器在承载。
            KeyCode::Enter | KeyCode::Char('v') => match app.selected_user() {
                Some(u) => return Some(Action::ShowUserNodes { id: u.id, name: u.name.clone() }),
                None => app.fail("没有选中任何用户"),
            },
            // 一个键切换启停,而不是启用/停用各一个键:人看着那一行的状态按,
            // 「当前是启用 → 按一下变停用」是唯一说得通的直觉。
            KeyCode::Char('t') => match app.selected_user() {
                Some(u) => {
                    return Some(Action::SetUserEnabled {
                        name: u.name.clone(),
                        enabled: !u.enabled,
                    })
                }
                None => app.fail("没有选中任何用户"),
            },
            KeyCode::Char('n') => match app.selected_user() {
                Some(u) => app.modal = Some(forms::assign_nodes(u, &app.nodes)),
                None => app.fail("没有选中任何用户"),
            },
            // 绑网卡:只改这个用户订阅响应头里的流量数字(§10.3)。
            KeyCode::Char('b') => {
                if app.agents.is_empty() {
                    app.fail("还没有被控服务器可绑");
                    return None;
                }
                match app.selected_user() {
                    Some(u) => app.modal = Some(forms::bind_nics(u, &app.agents)),
                    None => app.fail("没有选中任何用户"),
                }
            }
            // 大写 T。小写 t 是「启/停用户」,而这两件事完全不同:
            // 停用挡的是代理连接,撤销 token 挡的是订阅下载。
            // 让它们只差一个 Shift 又都放在这一页,已经是最省的方案 ——
            // 再省就得把其中一个挪走,而两个都是用户页该有的操作。
            KeyCode::Char('T') => match app.selected_user() {
                Some(u) => {
                    app.modal = Some(Modal::Token {
                        user_id: u.id,
                        name: u.name.clone(),
                        active: !crate::db::node_repo::is_revoked(&u.sub_token),
                    })
                }
                None => app.fail("没有选中任何用户"),
            },
            KeyCode::Char('r') => match app.selected_user() {
                Some(u) => {
                    app.modal = Some(Modal::Confirm {
                        title: "确认重置流量".into(),
                        body: vec![
                            format!("重置 '{}' 的流量?", u.name),
                            "只清零已用流量,不会改动月重置日期".into(),
                        ],
                        action: Action::ResetUserTraffic { user_id: u.id, user: u.name.clone() },
                    })
                }
                None => app.fail("没有选中任何用户"),
            },
            KeyCode::Char('s') => {
                if let Some(u) = app.selected_user() {
                    app.modal = Some(sub_modal(&app.cfg, &u.name, &u.sub_token));
                } else {
                    app.fail("没有选中任何用户");
                }
            }
            KeyCode::Char('d') => {
                if let Some(u) = app.selected_user() {
                    let name = u.name.clone();
                    app.modal = Some(Modal::confirm(
                        "删除用户",
                        vec![
                            format!("将删除用户 {name},以及它的节点分配与流量记录。"),
                            "各 agent 的 config_revision 会推进,在线的会重建 box。".into(),
                        ],
                        Action::DeleteUser { name },
                    ));
                } else {
                    app.fail("没有选中任何用户");
                }
            }
            _ => {}
        },

        Page::Settings => {
            if matches!(k.code, KeyCode::Enter | KeyCode::Char('E')) {
                let items = settings::all(&app.cfg);
                let Some(item) = items.into_iter().nth(app.sel[Page::Settings as usize]) else {
                    app.fail("没有选中任何设置项");
                    return None;
                };
                // 布尔项按一下就切,不弹框 —— 为一个 true/false 开表单太重了。
                if let settings::Kind::Bool(_) = item.kind {
                    return Some(Action::SetConfig {
                        section: item.section,
                        key: item.key,
                        value: item.to_toml("").unwrap_or_else(|_| "false".into()),
                        label: item.label.clone(),
                    });
                }
                app.modal = Some(forms::setting_edit(item));
            }
        }
    }
    None
}

/// 把 `agent.upgrade` **放进队列**,由 daemon 取走下发。
///
/// **TUI 不能自己发 RPC。** 它和 daemon 是两个进程,WS 连接活在 daemon 里
/// (模块头那段说明的直接后果)。所以这里只做主控侧知道的那部分 ——
/// 升到哪个版本、产物在哪、校验和是多少 —— 拼好塞进 `agent_commands`,
/// daemon 的巡检循环下一拍取走执行。
///
/// 两件事在入队**之前**做完,任一步失败就不入队:
///   1. 认架构:release 只出 amd64 / arm64,别的架构没有产物;
///   2. 取 sha256:它是 agent 侧唯一能挡住「下到坏文件就替换自己」的东西
///      (`agent/master/conn.go` 的 `replaceExecutable`),取不到宁可不升。
///
/// 离线的机器直接跳过。升级是**一次性指令**而不是状态,不会在它下次重连时
/// 补发 —— 所以回执里必须把跳过的台数说出来,否则人会以为全升了。
async fn upgrade_agents(app: &mut App, only: Option<i64>, name: &str) -> Result<String> {
    let version = crate::upgrade::target_version();
    let now = chrono::Local::now().timestamp();

    let targets: Vec<(i64, String, Option<String>, String)> = app
        .agents
        .iter()
        .filter(|a| match only {
            Some(id) => a.id == id,
            None => a.status == "online",
        })
        .map(|a| (a.id, a.name.clone(), a.arch.clone(), a.status.clone()))
        .collect();
    if targets.is_empty() {
        return Ok(if only.is_some() {
            format!("{name} 不在了,按 [R] 刷新")
        } else {
            "没有在线的被控服务器可升级".into()
        });
    }

    // sha256 按**架构**取,不是按台取:同一架构的产物是同一个文件,
    // 十台 arm64 没必要把同一个 .sha256 拉十遍。
    let mut sums: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let (mut queued, mut offline, mut failed) = (0usize, 0usize, Vec::<String>::new());

    for (id, aname, arch, status) in targets {
        if status != "online" {
            offline += 1;
            continue;
        }
        let Some(a) = arch.as_deref().and_then(crate::upgrade::normalize_arch) else {
            failed
                .push(format!("{aname}(架构 {} 没有发布产物)", arch.as_deref().unwrap_or("未知")));
            continue;
        };
        let url = crate::upgrade::agent_asset_url(version, a);
        if !sums.contains_key(a) {
            match crate::upgrade::fetch_sha256(&url).await {
                Ok(sum) => {
                    sums.insert(a.to_string(), sum);
                }
                Err(e) => {
                    failed.push(format!("{aname}(取校验和失败:{e})"));
                    continue;
                }
            }
        }
        let payload = serde_json::json!({ "url": url, "sha256": sums[a] });
        match crate::db::command_repo::enqueue(&app.pool, id, "upgrade", &payload, now).await {
            Ok(_) => queued += 1,
            Err(e) => failed.push(format!("{aname}({e})")),
        }
    }

    let mut msg = format!("已排队 {queued} 台升级到 v{version}(由 daemon 下发)");
    if offline > 0 {
        msg.push_str(&format!(",跳过 {offline} 台离线的"));
    }
    if !failed.is_empty() {
        msg.push_str(&format!(";失败:{}", failed.join("、")));
    }
    Ok(msg)
}

/// 「这条订阅怎么用」。
///
/// 光给一条 URL 是不够的:默认配置下订阅服务**只听 127.0.0.1:18081**,
/// 那条地址在别的机器上打不开。这是有意的默认(裸端口对公网暴露一个能列出
/// 全部节点凭据的接口不合适),但如果界面不说,人只会看到一条打不开的链接,
/// 然后去怀疑 token 或者节点配置 —— 那是查不到头的方向。
///
/// 所以这个框按当前配置分三种情形说话:服务关着 / 没配对外地址 / 一切就绪。
fn sub_modal(cfg: &Config, name: &str, token: &str) -> Modal {
    let base = cfg.subscription.public_base.trim().trim_end_matches('/').to_string();
    let path = format!("/sub/{token}");
    let listen = &cfg.subscription.listen;

    // 撤销过的 token 是一个**故意不合法**的值(`!revoked:<id>`)。
    // 照常拼一条 URL 给出去,那条地址一定 404,而人会以为是服务坏了 ——
    // 这一页得先说清「是你自己撤的」。
    if crate::db::node_repo::is_revoked(token) {
        return Modal::info(
            &format!("{name} 的订阅"),
            vec![
                "这个用户的订阅已被撤销,地址现在返回 404。".into(),
                String::new(),
                "按 [T] → [g] 重新生成一个 token 即可恢复。".into(),
                "注意:恢复出来的是**新地址**,老链接不会再能用。".into(),
            ],
        );
    }

    if !cfg.subscription.enabled {
        return Modal::info(
            &format!("{name} 的订阅"),
            vec![
                "订阅服务是关的,这条地址现在一律返回 404。".into(),
                String::new(),
                format!("路径:{path}"),
                String::new(),
                "去设置页(按 5)把「订阅服务」打开,再重启 daemon:".into(),
                "  systemctl restart sbx".into(),
            ],
        );
    }

    if base.is_empty() {
        return Modal::info(
            &format!("{name} 的订阅"),
            vec![
                format!("路径:{path}"),
                String::new(),
                format!("现在订阅服务只听 {listen},外面还访问不到。两条路选一条:"),
                String::new(),
                "① 挂 nginx(推荐,能上 TLS):".into(),
                format!("     location /sub/ {{ proxy_pass http://{listen}; }}"),
                "   然后在设置页(按 5)把「订阅对外地址」填成 https://你的域名".into(),
                String::new(),
                "② 直接对外听(仅内网或临时用):".into(),
                "   设置页把「订阅监听」改成 0.0.0.0:18081,".into(),
                "   「订阅对外地址」填 http://你的IP:18081 —— 注意这是明文,".into(),
                "   而这条地址能列出该用户全部节点的凭据。".into(),
                String::new(),
                "改完都要重启 daemon:systemctl restart sbx".into(),
            ],
        );
    }

    let url = format!("{base}{path}");
    Modal::info_copyable(
        &format!("{name} 的订阅"),
        vec![
            url.clone(),
            String::new(),
            "贴进客户端就能用 —— 客户端的 UA 会被自动识别成对应格式。".into(),
            "浏览器打开是流量统计页。".into(),
            String::new(),
            "要强制某种格式,地址后面加:".into(),
            "  ?type=clash    Clash / Mihomo 的 YAML".into(),
            "  ?type=base64   通用的 base64 链接列表".into(),
            "  ?type=stats    流量统计页".into(),
        ],
        url,
    )
}

/// 编辑被控服务器。放在这里而不是 `forms.rs`,因为它要读 `AgentRow` ——
/// 那是 TUI 自己的视图模型,`forms` 里其余表单也都从视图模型取预填值。
fn agent_edit(a: &data::AgentRow) -> Modal {
    use modal::{val, Field, Form};
    let id = a.id;
    let quota = match a.nic_quota_bytes {
        Some(q) if q > 0 => format!("{:.0}", q as f64 / 1_073_741_824.0),
        _ => "0".into(),
    };
    let reset = a.nic_reset_day.map(|d| d.to_string()).unwrap_or_default();
    let tz = forms::nic_offset_label(a.nic_reset_offset_secs);
    // agent 报了什么写进**标题行**,不写进字段标签:标签列宽被夹在 34 列以内
    // (`modal.rs` 的 `label_w`),超了会被静默截成 `…`,而标题行是整宽的。
    // 也不新开 note 行 —— 弹窗高度在 80×24 上已经接近上限,超了牺牲的是底部按键提示。
    let reported = match a.reported_utc_offset_secs {
        Some(s) => format!(",agent 报 {}", crate::tg::fmt::format_offset(s as i32)),
        None => ",agent 还没报过时区".into(),
    };
    let nic_modes = crate::model::agent::NicAccountingMode::all();
    let nic_mode_idx = nic_modes.iter().position(|m| *m == a.nic_accounting_mode).unwrap_or(0);

    Modal::Form(
        Form::new(
            "编辑被控服务器",
            vec![
                Field::text("name", "名称 *必填", &a.name),
                Field::text("quota", "网卡月配额 GB (0 = 不限)", &quota),
                Field::select(
                    "nic_mode",
                    "网卡记账口径 (←/→ 切换)",
                    nic_modes.iter().map(|m| m.label().to_string()).collect(),
                    nic_mode_idx,
                ),
                Field::text("reset", "配额重置日 (1-31,留空 = 不重置)", &reset),
                Field::text("tz", "重置时区 (留空 = 跟随 agent 上报)", &tz),
            ],
            Box::new(move |f| {
                let name = val(f, "name");
                if name.is_empty() {
                    return Err("名称不能为空".into());
                }
                let gb: f64 = val(f, "quota").parse().map_err(|_| "配额要是一个数字(0 = 不限)")?;
                if gb < 0.0 {
                    return Err("配额不能是负数".into());
                }
                Ok(Action::EditAgent {
                    id,
                    name,
                    quota_bytes: if gb > 0.0 { Some((gb * 1_073_741_824.0) as i64) } else { None },
                    reset_day: forms::parse_reset_day(&val(f, "reset"))?,
                    nic_accounting_mode: crate::model::agent::NicAccountingMode::all()
                        .get(f.iter().find(|x| x.key == "nic_mode").map(|x| x.index()).unwrap_or(0))
                        .copied()
                        .unwrap_or_default(),
                    nic_reset_offset_secs: forms::parse_nic_offset(&val(f, "tz"))?,
                })
            }),
        )
        .head(format!(
            "#{} {}(IP 由 agent 自探上报,改了会被下一次上报覆盖{reported})",
            a.id, a.name
        ))
        .with_note(Box::new(|_| {
            vec![
                "网卡配额按所选口径读取机器原始进出字节,不是用户计费用量。".into(),
                "出站 = 机器发出(服务器→客户端,即客户端那边的下载),入站 = 机器收到。".into(),
                "两个方向一直分开记,换口径只重算当前周期的显示,不清零原始流量。".into(),
                "重置时区写 UTC-07:00 这种形式,决定每月哪一刻翻月(厂商按机房当地零点结算)。".into(),
                "它只影响界面上的进度条与告警,不会限制 agent 转发流量。".into(),
            ]
        })),
    )
}

/// 摘要行里跟在「● 离线」后面的那一小段:离线多久。
///
/// **为什么值得占这几个字。** 表里那个红点只说「现在连不上」,而决定要不要动身
/// 去查那台机器的是**多久**:离线 3 分钟大概是它自己在重连(agent 退避上限 60 秒),
/// 离线 3 天就是机器或防火墙的事了。以前这个信息只在库里,排查时得手工去
/// `SELECT last_seen`,而那正是最不该需要开一个 sqlite 会话的时刻。
///
/// 三种情况都返回空串,各有理由:
///   * 在线 —— `last_seen` 约等于现在,写出来是噪声;
///   * `None`(从没连上过)—— 状态那一格已经写着「○ 从未连接」,再说一遍是重复。
///     而且它和「连过又断了」是两回事:前者通常是 token 或防火墙,后者是机器/网络;
///   * `last_seen` 在未来 —— 时钟回拨或库被手改过。宁可不说,也不要显示「离线 -5 天」。
fn offline_for(a: &data::AgentRow, now: i64) -> String {
    if a.status == "online" {
        return String::new();
    }
    match a.last_seen.map(|t| now - t) {
        Some(d) if d > 0 => format!(" {}", pages::uptime_label(d)),
        _ => String::new(),
    }
}

/// 摘要行里跟在「出站」后面的那一小段:有没有自定义配置,以及它是不是把
/// 出站策略接管了。
///
/// **接管了就必须说出来。** `[o]` 与自定义配置写的是同一个字段
/// (`route.default_domain_resolver`)—— `[o]` 本质上就是自定义配置的一个预设。
/// 自定义写了之后策略让位(见 `outbound::apply`),而界面上还显示着「仅 IPv4」
/// 就是个谎 —— 那种「界面说一套、实际跑一套」是最难查的不一致。
///
/// 只在选中的那一行解一次 JSON,不是每行都解。
fn custom_note(a: &data::AgentRow) -> String {
    let Some(raw) = a.custom_json.as_deref().filter(|s| !s.trim().is_empty()) else {
        return String::new();
    };
    // 读不懂也要说「有自定义」—— 存在本身就是人要知道的事,
    // 而「库里那份读不懂」由组装时那条 warn 负责。
    let takes_over = crate::service::validate_custom(raw)
        .map(|o| o.get("route").and_then(|r| r.get("default_domain_resolver")).is_some())
        .unwrap_or(false);
    if takes_over {
        "(由自定义配置接管)".into()
    } else {
        "  自定义: 有".into()
    }
}

/// 接入命令的信息框:命令自己一行,底下是说明,`y` 复制整条。
///
/// 命令**单独占一行**放在最上面,而不是混在说明里:它是这个框存在的唯一理由,
/// 而且是那条要被复制走的东西 —— 终端不支持 OSC 52 时人得用鼠标去选它。
fn install_modal(
    cfg: &Config,
    host: &str,
    title: &str,
    token: Option<&str>,
    tail: Vec<String>,
) -> Modal {
    let cmd = install::command(cfg, host, token);
    let mut body = vec![cmd.clone(), String::new()];
    body.extend(install::notes(host, token.is_some()));
    body.extend(tail);
    Modal::info_copyable(title, body, cmd)
}

/// 执行一个写操作。**所有错误都落到状态栏,不中断 TUI** ——
/// 一次唯一约束冲突不该让人被踢回 shell。
async fn perform(app: &mut App, action: Action) {
    let r = perform_inner(app, &action).await;
    match r {
        Ok(msg) => app.note(msg),
        Err(e) => app.fail(format!("失败: {e}")),
    }
}

async fn perform_inner(app: &mut App, action: &Action) -> Result<String> {
    use crate::db::{agent_repo, node_repo};
    let now = chrono::Local::now().timestamp();

    match action {
        Action::AddAgent {
            name,
            quota_bytes,
            reset_day,
            nic_accounting_mode,
            nic_reset_offset_secs,
        } => {
            let (id, token) = agent_repo::create(&app.pool, name, now).await?;
            // 配额、重置日、记账口径和重置时区在创建时一并写入。
            agent_repo::update_settings(
                &app.pool,
                id,
                name,
                *quota_bytes,
                *reset_day,
                *nic_accounting_mode,
                *nic_reset_offset_secs,
            )
            .await?;
            // token 明文只在这里出现这一次。库里只有 hash 与前 8 位(§8.1)。
            let host = app.master_host();
            app.modal = Some(install_modal(
                &app.cfg,
                &host,
                &format!("agent #{id} {name} —— 在被控机上跑这一条"),
                Some(&token),
                Vec::new(),
            ));
            Ok(format!("已新增 agent #{id} {name}"))
        }

        Action::EditAgent {
            id,
            name,
            quota_bytes,
            reset_day,
            nic_accounting_mode,
            nic_reset_offset_secs,
        } => {
            agent_repo::update_settings(
                &app.pool,
                *id,
                name,
                *quota_bytes,
                *reset_day,
                *nic_accounting_mode,
                *nic_reset_offset_secs,
            )
            .await?;
            let tz = match nic_reset_offset_secs {
                Some(s) => format!("重置时区 {}", crate::tg::fmt::format_offset(*s as i32)),
                None => "重置时区跟随 agent".into(),
            };
            Ok(format!(
                "已保存 agent #{id} {name} 的设置(网卡口径:{};{tz};下一拍生效,不改 agent 配置)",
                nic_accounting_mode.label()
            ))
        }

        Action::ShowInstall { id, name } => {
            let host = app.master_host();
            app.modal = Some(install_modal(
                &app.cfg,
                &host,
                &format!("agent #{id} {name} 的接入命令"),
                None,
                Vec::new(),
            ));
            Ok(String::new())
        }

        Action::RotateToken { id, name } => {
            let token = agent_repo::rotate_token(&app.pool, *id).await?;
            let host = app.master_host();
            app.modal = Some(install_modal(
                &app.cfg,
                &host,
                &format!("{name} 的新 token —— 在那台机器上重跑这一条"),
                Some(&token),
                vec![
                    String::new(),
                    "旧 token 已失效。在线连接不会被立刻踢掉,下次重连时生效。".into(),
                    "被控机上原有的 agent.toml 会自动备份成 agent.toml.bak。".into(),
                ],
            ));
            Ok(format!("已轮换 {name} 的 token"))
        }

        Action::DeleteAgent { id, name } => {
            agent_repo::delete(&app.pool, *id).await?;
            Ok(format!("已删除 agent {name} 及其节点"))
        }

        Action::AddNode(d) => {
            let mut params = crate::model::node::NodeParams {
                server_name: d.server_name.clone(),
                path: d.path.clone(),
                ipv6: d.ipv6,
                relay: crate::model::node::RelaySetting {
                    host: d.relay_host.clone(),
                    port: d.relay_port,
                },
                ..Default::default()
            };
            // 与 CLI 走同一条路:密钥材料在**建节点时**生成一次(§9.1)。
            crate::secrets::fill(d.protocol, &mut params)?;
            // revision 推进了但**不写进回执**:那是个内部计数器,每改一次加一,
            // 摆给人看只是一串越来越大的噪音。要紧的是「什么时候生效」。
            let (id, _rev) =
                node_repo::add_node(&app.pool, d.agent_id, &d.tag, d.protocol, d.port, &params)
                    .await?;
            Ok(format!("已新增节点 #{id} {},在线的机器约 1s 后重建 box", d.tag))
        }

        Action::EditNode { id, draft } => {
            // **在原 params 上改**,不是造一个新的:reality 密钥对、自签证书、
            // ss 服务端密钥都在里面,清掉等于客户端静默全部失联(§9.1)。
            let node = app
                .nodes
                .iter()
                .find(|n| n.id == *id)
                .ok_or_else(|| anyhow::anyhow!("节点 #{id} 已经不在了(是不是刚被删掉?)"))?;
            let mut params = node.params.clone();
            params.server_name = draft.server_name.clone();
            params.path = draft.path.clone();
            params.ipv6 = draft.ipv6;
            params.relay = crate::model::node::RelaySetting {
                host: draft.relay_host.clone(),
                port: draft.relay_port,
            };
            // 协议要求的字段被清空时补回默认值(比如 reality 的 server_name),
            // 否则下发时 `build_inbound` 会报「缺少 server_name」。
            crate::secrets::fill(draft.protocol, &mut params)?;

            let tag = node.tag.clone();
            let (_agent_id, _rev) =
                node_repo::update_node(&app.pool, *id, draft.port, &params).await?;
            Ok(format!("已保存节点 {tag},已下发生效"))
        }

        Action::DeleteNode { id, tag } => {
            let (_agent_id, _rev) = node_repo::delete_node(&app.pool, *id).await?;
            Ok(format!("已删除节点 {tag},已下发生效"))
        }

        Action::AddUser { name, quota_gb, multiplier, expire, reset_day } => {
            // **先把输入全解析完再动库。** 顺序反过来的话,一个填错的日期会留下
            // 一个已经建好、但计费设置没写进去的用户 —— 而界面上只会显示一句
            // 报错,没人知道那个号已经躺在库里了。
            let quota = parse_quota(quota_gb)?;
            let mult = parse_multiplier(multiplier)?;
            let expire_at = forms::parse_expire(expire).map_err(|e| anyhow::anyhow!(e))?;
            let day = forms::parse_reset_day(reset_day).map_err(|e| anyhow::anyhow!(e))?;

            let id = node_repo::add_user(&app.pool, name, quota, now).await?;
            // add_user 只认名称与配额(它是 CLI 也在用的那条路),其余项建完再写一次。
            node_repo::update_user(&app.pool, id, quota, mult, expire_at, day).await?;
            Ok(format!("已新增用户 #{id} {name};按 [n] 给它分配节点,否则订阅是空的"))
        }

        Action::EditUser { id, name, quota_gb, multiplier, expire, reset_day } => {
            let quota = parse_quota(quota_gb)?;
            let mult = parse_multiplier(multiplier)?;
            let expire_at = forms::parse_expire(expire).map_err(|e| anyhow::anyhow!(e))?;
            let day = forms::parse_reset_day(reset_day).map_err(|e| anyhow::anyhow!(e))?;
            node_repo::update_user(&app.pool, *id, quota, mult, expire_at, day).await?;
            Ok(format!("已保存 {name} 的计费设置(不重建 box;下次巡检时生效)"))
        }

        Action::SetUserEnabled { name, enabled } => {
            node_repo::set_user_enabled(&app.pool, name, *enabled).await?;
            if *enabled {
                Ok(format!("已启用 {name};在线 agent 会收到 user.state"))
            } else {
                // 这条路径**不重建 box**,已建立的连接会跑完(§7.5)。
                Ok(format!("已停用 {name}(手动停用,自动流程不会恢复它);只挡新连接"))
            }
        }

        Action::DeleteUser { name } => {
            node_repo::delete_user(&app.pool, name).await?;
            Ok(format!("已删除用户 {name}"))
        }

        Action::SetConfig { section, key, value, label } => {
            crate::config::set_value(&app.cfg_path, section, key, value)?;
            // 立刻重读:不重读的话页面上还是旧值,人会以为没保存成功再改一遍。
            app.reload_config();
            Ok(format!("已保存「{label}」到 {}(重启 daemon 生效)", app.cfg_path))
        }

        Action::ShowNodeUsers { id, tag, agent } => {
            let rows = data::node_breakdown(&app.pool, *id).await?;
            let n = rows.len();
            // 各行倍率不同,标记跟在每个用户名后面(见 `node_breakdown`),
            // 标题只说明这些数字是哪个口径。
            app.overlay = Some(Overlay::table(
                format!("节点 {tag} 上的用户"),
                format!("{tag}(在 {agent} 上)· {n} 个用户 · 数字含各自倍率"),
                rows,
            ));
            Ok(String::new())
        }

        Action::ShowUserNodes { id, name } => {
            let rows = data::user_breakdown(&app.pool, *id).await?;
            let n = rows.len();
            // 倍率写在标题里,不写在每一行:整张表都是这一个用户,
            // 逐行重复 `x2` 只是噪声。但**必须写在某处** —— 表里的数字
            // 都乘过它,不说的话对不上客户端自己记的单倍数。
            let mult = app
                .users
                .iter()
                .find(|u| u.id == *id)
                .map(|u| format!(" · 计费 {}", u.mult_tag()))
                .unwrap_or_default();
            app.overlay = Some(Overlay::table(
                format!("{name} 的节点用量"),
                format!("{name} · {n} 个节点{mult}"),
                rows,
            ));
            Ok(String::new())
        }

        Action::ShowAgentNics { id, name } => {
            let rows = data::agent_breakdown(&app.pool, *id).await?;
            let n = rows.len();
            // agent 的实时读数只在内存里(SpeedTracker),库里查不到 ——
            // 所以这里从已经加载好的那一份取,而不是重新查库。
            let info = match app.agents.iter().find(|a| a.id == *id) {
                Some(a) => pages::nic_info(a, chrono::Local::now().timestamp()),
                None => vec![Line::from(Span::styled(
                    "  这台机器刚被删掉了,按 [R] 刷新",
                    Style::default().fg(theme::DIM),
                ))],
            };
            app.overlay = Some(Overlay {
                title: format!("{name} 的网卡明细"),
                // 这一页同屏摆着两个口径:上面几行是整机网卡(厂商按它开账单),
                // 下面每行是节点上各用户的用量 × 倍率。不写明的话两组数对不上,
                // 看起来像其中一组坏了(§6.4 / §7.2)。
                head: format!("{name} · {n} 个节点 · 上=整机网卡,下=节点计费用量"),
                info,
                body: OverlayBody::Table(rows),
            });
            Ok(String::new())
        }

        Action::CheckAgentConfig { id, name } => {
            // 离线就直接拒,不要入队。入了队的下场是 daemon 取走、发不出去、
            // 回一句「agent 不在线」—— 绕一大圈才告诉人一件此处当场就能知道的事。
            let online = app.agents.iter().any(|a| a.id == *id && a.status == "online");
            if !online {
                return Ok(format!(
                    "{name} 不在线 —— 校验要用那台机器自己的 sing-box 试建,必须在线"
                ));
            }
            // 校验的就是**下发给它的那一份**:同一个 `build_agent_config`,
            // 和 `[c]` 看到的、和 `config.apply` 发出去的是同一份字节。
            // 抽一份可能不一样的去验等于没验。
            let cfg = crate::service::build_agent_config(&app.pool, *id).await?;
            let now = chrono::Local::now().timestamp();
            let cmd_id = crate::db::command_repo::enqueue(
                &app.pool,
                *id,
                "config_check",
                &serde_json::json!({ "options": cfg }),
                now,
            )
            .await?;
            app.check_wait = Some(CheckWait { cmd_id, agent: name.clone(), at: now });
            Ok(format!("已请 {name} 用自己的 sing-box 试建一次,结果马上回来"))
        }

        Action::EditAgentConfig { id, name } => {
            // 真正的编辑在主循环里做(要挂起终端,这里拿不到 `Terminal`)。
            app.want_edit_custom = Some((*id, name.clone()));
            Ok(String::new())
        }

        Action::ShowAgentConfig { id, name } => {
            // 主控**现场组装**,不向 agent 要:下发给它的就是这份字节,
            // 两边必然一致;而且离线的机器也能看 —— 那恰恰是最需要看的时候。
            // **原文,不脱敏。** 这一页的用处就是「能不能直接 `sing-box -c` 跑起来」,
            // 而遮掉私钥的配置跑不起来 —— 那样这一页就只剩个大概样子,
            // 真要核对 reality 密钥对不对时反而看不到。
            //
            // 前提是它只在主控的终端上显示:这些凭据本来就是主控生成、主控保管的
            // (§9.1 的密钥全部产自这里),库里也是明文。§11.3 管的是**列表和摘要行**
            // —— 那些是平时一直摆在屏幕上的,而这一页要按 [c] 才打开。
            let cfg = crate::service::build_agent_config(&app.pool, *id).await?;
            let text = serde_json::to_string_pretty(&cfg)?;
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            let n = app.nodes.iter().filter(|x| x.agent_id == *id).count();
            app.overlay = Some(Overlay {
                title: format!("{name} 的 sing-box 配置"),
                head: format!(
                    "{name} · {n} 个节点 · {} 行 · 含凭据原文,注意别外传   [↑↓/PgUp/PgDn]滚动  [Esc]关闭",
                    lines.len()
                ),
                info: vec![],
                body: OverlayBody::Text { lines, scroll: 0 },
            });
            Ok(String::new())
        }

        Action::SetUserNics { user_id, user, agent_ids } => {
            node_repo::set_user_nics(&app.pool, *user_id, agent_ids).await?;
            if agent_ids.is_empty() {
                return Ok(format!("{user} 的订阅流量已改回按自己的用量报"));
            }
            Ok(format!(
                "{user} 的订阅流量改成报 {} 台机器的网卡用量之和(只影响订阅响应头)",
                agent_ids.len()
            ))
        }

        // 三个都**不推进 revision**:订阅 token 与计费数字都不进 sing-box 配置,
        // agent 不需要知道它们。回执里因此不说「等下发」,而是说「立刻生效」——
        // 订阅那条路是主控自己的 HTTP 服务,改完下一次请求就是新的。
        Action::RegenSubToken { user_id, user } => {
            node_repo::regenerate_sub_token(&app.pool, *user_id).await?;
            Ok(format!("{user} 的订阅 token 已重新生成,老 URL 立刻失效。按 [s] 看新地址"))
        }

        Action::RevokeSubToken { user_id, user } => {
            node_repo::revoke_sub_token(&app.pool, *user_id).await?;
            Ok(format!("{user} 的订阅已撤销,地址返回 404。[T] → [g] 可以恢复"))
        }

        Action::ResetUserTraffic { user_id, user } => {
            node_repo::reset_user_traffic(&app.pool, *user_id).await?;
            Ok(format!("{user} 的本周期流量已清零(月重置日期没动)"))
        }

        Action::SetOutbound { id, name, strategy } => {
            // revision 推进了,但**不写进回执**:那是个内部计数器,每改一次加一,
            // 对着人显示只是一串越来越大的噪音。要紧的是「什么时候生效」——
            // TUI 只改库,下发由 daemon 在下次握手或下发时做(§4.1)。
            crate::db::agent_repo::set_outbound_strategy(&app.pool, *id, *strategy).await?;
            Ok(format!("{name} 的出站策略改成「{}」,已下发生效", strategy.label()))
        }

        Action::UpgradeAgents { only, name } => upgrade_agents(app, *only, name).await,

        // 这一条**不在这里做**:它要挂起终端(离开 alternate screen、关 raw mode)
        // 才能让脚本的输出被人看见,而终端句柄在主循环手上。这里只置标记。
        Action::SelfUpgrade => {
            app.want_self_upgrade = true;
            Ok(String::new())
        }

        // 刷新本身在主循环里做(每个动作之后都会 refresh 一次),这里只给回执。
        Action::Refresh => Ok("已刷新".into()),

        Action::SetUserNodes { user_id, user, node_ids } => {
            let affected = node_repo::set_user_nodes(&app.pool, *user_id, node_ids).await?;
            if affected.is_empty() {
                return Ok(format!("{user} 的节点分配没有变化"));
            }
            // 与别处一致:**不印 revision**。那是个内部计数器,每改一次加一,
            // 摆给人看只是噪音。改说「碰到了几台机器」——那才是这次操作的影响面。
            Ok(format!(
                "{user} 现在有 {} 个节点,{} 台机器会重建 box",
                node_ids.len(),
                affected.len()
            ))
        }
    }
}

/// 计费倍率。新增与编辑共用一份 —— 两处各写一遍的话,校验迟早会漂开
/// (一边挡住负数、另一边没挡)。
fn parse_multiplier(s: &str) -> Result<f64> {
    let v: f64 = s.parse().map_err(|_| anyhow::anyhow!("计费倍率 {s} 不是数字"))?;
    if v < 0.0 {
        anyhow::bail!("计费倍率不能是负数");
    }
    Ok(v)
}

fn parse_quota(gb: &str) -> Result<i64> {
    let v: f64 = gb.parse().map_err(|_| anyhow::anyhow!("配额 {gb} 不是数字"))?;
    if v < 0.0 {
        anyhow::bail!("配额不能是负数");
    }
    Ok((v * 1_073_741_824.0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 接入命令的信息框:命令必须**自己占一行**排在最前面。
    /// 它是这个框存在的唯一理由,也是终端不支持 OSC 52 时要用鼠标去选的那一行。
    #[test]
    fn install_modal_puts_the_command_on_its_own_first_line() {
        let mut cfg = Config::default();
        cfg.cluster.tls = false;
        let m = install_modal(&cfg, "203.0.113.8", "t", Some("tok"), Vec::new());
        let Modal::Info { body, copy, .. } = &m else { panic!("应当是信息框") };
        assert!(body[0].starts_with("(curl -fsSL "), "第一行就该是命令: {:?}", body[0]);
        assert_eq!(copy.as_deref(), Some(body[0].as_str()), "按 y 复制的就该是那一行");
        assert!(body.iter().any(|l| l.contains("只显示这一次")), "带 token 时要有警告");
    }

    /// 按 y 之后弹窗**不关**,并且给一句如实的回执 ——
    /// OSC 52 没有回执,关掉的话人根本不知道刚才那一下有没有生效。
    #[tokio::test]
    async fn copying_keeps_the_modal_open_and_says_what_happened() {
        let mut cfg = Config::default();
        cfg.cluster.tls = false;
        let mut a = app();
        a.modal = Some(install_modal(&cfg, "203.0.113.8", "t", Some("tok"), Vec::new()));
        assert!(on_key(&mut a, key('y')).is_none());
        let Some(Modal::Info { copied, .. }) = &a.modal else { panic!("弹窗不该关掉") };
        let msg = copied.as_deref().unwrap_or_default();
        assert!(msg.contains("OSC 52"), "回执要说清用的是什么机制:{msg}");
        assert!(msg.contains("不支持"), "要说清可能不生效:{msg}");

        // 其它键仍然是关闭。
        on_key(&mut a, key('x'));
        assert!(a.modal.is_none());
    }

    /// 没什么可复制的信息框(订阅地址那种)按 y 就是关掉,不能卡住。
    #[tokio::test]
    async fn a_plain_info_box_closes_on_any_key() {
        let mut a = app();
        a.modal = Some(Modal::info("t", vec!["x".into()]));
        on_key(&mut a, key('y'));
        assert!(a.modal.is_none());
    }

    /// 接入命令的信息框长什么样,给人看一眼:
    ///
    /// ```sh
    /// cargo test tui::tests::preview_install_modal -- --nocapture
    /// ```
    #[test]
    fn preview_install_modal() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut cfg = Config::default();
        cfg.cluster.tls = false;
        cfg.subscription.public_base = "https://sub.example.com".into();
        let mut m = install_modal(
            &cfg,
            "sub.example.com",
            "agent #1 tokyo-1 —— 在被控机上跑这一条",
            Some("Zm9vYmFyYmF6cXV1eDEyMzQ1Njc4OWFiY2RlZg"),
            Vec::new(),
        );
        for label in ["未复制", "按过 y 之后"] {
            let mut term = Terminal::new(TestBackend::new(116, 26)).unwrap();
            term.draw(|f| modal::render(f, f.area(), &m)).unwrap();
            let buf = term.backend().buffer().clone();
            let out: Vec<String> = (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .filter(|l| !l.is_empty())
                .collect();
            println!("── 接入命令({label})──\n{}\n", out.join("\n"));
            if let Modal::Info { copied, .. } = &mut m {
                *copied = Some(
                    "已发给终端(OSC 52)。终端不支持的话这一下不会有任何效果 —— \
                     那就用鼠标选中复制,或者在主控上跑 sbx agent-add 从普通终端里复制。"
                        .into(),
                );
            }
        }
    }

    /// 明细表开着时,页面快捷键必须被吃掉。
    ///
    /// 这一条守的是一个会丢数据的错:明细表盖在节点表上,那时按 `d`
    /// 会去删掉**后面那张表里选中的节点**,而人以为自己只是在关一个只读的框。
    #[tokio::test]
    async fn an_open_breakdown_swallows_page_shortcuts() {
        let mut a = app();
        a.page = Page::Nodes;
        a.nodes = vec![stub_node(7, "n7")];
        a.overlay = Some(Overlay::table("t".into(), "h".into(), vec![]));

        assert!(on_key(&mut a, key('d')).is_none(), "不该产生删除动作");
        assert!(a.modal.is_none(), "也不该弹出确认框");
        assert!(a.overlay.is_none(), "任意键关掉明细表");

        // 关掉之后快捷键恢复正常。
        assert!(on_key(&mut a, key('d')).is_none());
        assert!(matches!(a.modal, Some(Modal::Confirm { .. })), "这次才该弹确认框");
    }

    /// 三个列表页的 Enter 各自打开自己方向的明细 —— 同一个手势,三种视角。
    ///
    /// 服务管理页那一条尤其要盯着:`r`(轮换 token)就在旁边,而 Enter 一旦
    /// 被接到别的分支上,人按下去会得到一个不可撤销的动作而不是一张只读表。
    #[tokio::test]
    async fn enter_opens_the_right_breakdown() {
        let mut a = app();
        a.page = Page::Nodes;
        a.nodes = vec![stub_node(7, "n7")];
        let act = on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Some(Action::ShowNodeUsers { id: 7, .. })), "节点页要看各用户");

        a.page = Page::Users;
        a.users = vec![stub_user(true)];
        let act = on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Some(Action::ShowUserNodes { id: 1, .. })), "用户页要看各节点");

        a.page = Page::Agents;
        a.agents = vec![stub_agent(3)];
        let act = on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Some(Action::ShowAgentNics { id: 3, .. })), "服务管理页要看网卡明细");
        assert!(a.modal.is_none(), "Enter 不该弹出任何会改东西的框");
    }

    /// 空表上按 Enter 只该给一句提示,不能 panic —— 这三页的 `selected_*`
    /// 在没有行的时候返回 None,少一个分支就是一次下标越界。
    #[tokio::test]
    async fn enter_on_an_empty_list_just_complains() {
        for page in [Page::Agents, Page::Nodes, Page::Users] {
            let mut a = app();
            a.page = page;
            let act = on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(act.is_none(), "{page:?} 空表不该产生动作");
            assert!(a.status_is_error, "{page:?} 该给一句提示");
        }
    }

    /// 订阅框按当前配置说三种不同的话。
    ///
    /// 这一条守的是「链接打不开时,界面得说出真正的原因」。默认配置下订阅
    /// 只听 127.0.0.1,给一条打不开的 URL 会把人引去查 token 和节点配置 ——
    /// 那个方向永远查不出结果。
    #[test]
    fn subscription_modal_explains_why_the_link_may_not_open() {
        let body = |m: &Modal| match m {
            Modal::Info { body, .. } => body.join(
                "
",
            ),
            _ => panic!("订阅框应当是只读信息框"),
        };

        // ① 服务关着 —— 先说这个,别的都没意义。
        let mut cfg = Config::default();
        cfg.subscription.enabled = false;
        cfg.subscription.public_base = "https://sub.example.com".into();
        let t = body(&sub_modal(&cfg, "alice", "tok"));
        assert!(
            t.contains("404"),
            "关着的时候要说清返回 404:
{t}"
        );

        // ② 开着但没配对外地址 —— 要给出**怎么让它能被访问到**,而不只是一条路径。
        let cfg = Config::default();
        assert!(cfg.subscription.enabled);
        assert!(cfg.subscription.public_base.is_empty());
        let t = body(&sub_modal(&cfg, "alice", "tok"));
        assert!(
            t.contains("/sub/tok"),
            "路径要给:
{t}"
        );
        assert!(
            t.contains("127.0.0.1:18081"),
            "要点出它现在只听本机:
{t}"
        );
        assert!(
            t.contains("nginx"),
            "要给出反代这条路:
{t}"
        );

        // ③ 配好了 —— 给完整地址,并且能一键复制。
        let mut cfg = Config::default();
        cfg.subscription.public_base = "https://sub.example.com/".into();
        let m = sub_modal(&cfg, "alice", "tok");
        let t = body(&m);
        assert!(
            t.contains("https://sub.example.com/sub/tok"),
            "尾部斜杠不该变成双斜杠:
{t}"
        );
        match m {
            // 订阅地址是这个框存在的全部理由,必须能 `y` 复制 ——
            // 一串二十位的随机 token 靠手抄是抄不对的。
            Modal::Info { copy: Some(c), .. } => {
                assert_eq!(c, "https://sub.example.com/sub/tok")
            }
            _ => panic!("配好之后要能复制"),
        }
    }

    /// `R` 是大写的:小写 `r` 在服务管理页是「轮换 token」,
    /// 那是一个不可撤销的动作,不能和刷新只差一个 Shift 却又长得像。
    #[tokio::test]
    async fn refresh_is_uppercase_only() {
        let mut a = app();
        a.page = Page::Agents;
        a.agents = vec![stub_agent(1)];
        assert!(matches!(on_key(&mut a, key('R')), Some(Action::Refresh)));

        let act = on_key(&mut a, key('r'));
        assert!(act.is_none());
        assert!(matches!(a.modal, Some(Modal::Confirm { .. })), "小写 r 仍是轮换 token 的确认框");
    }

    /// 设置页:布尔项按一下就切,不弹框。
    #[tokio::test]
    async fn a_boolean_setting_flips_without_a_form() {
        let mut a = app();
        a.page = Page::Settings;
        // 第一个布尔项是「订阅服务」(下标 2)。
        a.sel[Page::Settings as usize] = 2;
        let act = on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Some(Action::SetConfig { section, key, value, .. }) = act else {
            panic!("应当直接产生一个写配置的动作,而不是弹表单")
        };
        assert_eq!((section, key), ("subscription", "enabled"));
        assert_eq!(value, "false", "默认是开,按一下该变关");
        assert!(a.modal.is_none());
    }

    /// 非布尔项开一个单字段表单,并且**凭据不预填**(§11.3)。
    #[tokio::test]
    async fn a_secret_setting_opens_an_empty_form() {
        let mut a = app();
        a.cfg.telegram.bot_token = "1234567890:AAHsecret".into();
        a.page = Page::Settings;
        let items = settings::all(&a.cfg);
        a.sel[Page::Settings as usize] = items.iter().position(|s| s.key == "bot_token").unwrap();

        on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let Some(Modal::Form(form)) = &a.modal else { panic!("应当开一个表单") };
        assert_eq!(form.fields[0].value(), "", "不该把旧凭据铺在输入框里");
    }

    /// 设置页写的是**真的文件**,并且写完立刻重读 ——
    /// 不重读的话页面上还是旧值,人会以为没保存成功再改一遍。
    #[tokio::test]
    async fn saving_a_setting_writes_the_file_and_reloads() {
        let dir = std::env::temp_dir().join(format!("sbx-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# 别把我删了\n[cluster]\ntls = true\n").unwrap();

        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        let mut a = App::new(pool, Config::default(), path.to_string_lossy().into_owned());

        let msg = perform_inner(
            &mut a,
            &Action::SetConfig {
                section: "subscription",
                key: "public_base",
                value: "\"https://sub.example.com\"".into(),
                label: "订阅对外地址".into(),
            },
        )
        .await
        .unwrap();
        assert!(msg.contains("重启 daemon"), "{msg}");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 别把我删了"), "注释被抹掉了:\n{text}");
        assert!(text.contains("public_base = \"https://sub.example.com\""), "{text}");
        // 内存里的那份也要更新。
        assert_eq!(a.cfg.subscription.public_base, "https://sub.example.com");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **页签文字必须真的画出来。**
    ///
    /// v0.3.0 换成 ratatui 的 `Tabs` + `Borders::BOTTOM` 之后,页签区还是只给了
    /// 一行 —— 那一行被下边框吃掉,结果整条页签消失,屏幕上只剩一条横线。
    /// 界面「看起来只是少了点东西」,而实际是导航条整个没了。
    ///
    /// 断言的是渲染出来的字符,不是布局参数:参数对不对只有画出来才知道。
    #[tokio::test]
    async fn tabs_are_actually_visible() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let a = app();
        let mut term = Terminal::new(TestBackend::new(120, 22)).unwrap();
        term.draw(|f| draw(f, &a)).unwrap();
        let buf = term.backend().buffer().clone();
        let h = buf.area.height as usize;

        let row = |y: u16| -> String {
            (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect()
        };
        let flat = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };

        // 页签
        let first = row(0);
        let ff = flat(&first);
        for (i, t) in PAGES.iter().enumerate() {
            let want = flat(&format!("{t}[{}]", i + 1));
            assert!(ff.contains(&want), "页签第一行里没有 {want}:{first:?}");
        }
        let second = row(1);
        assert!(second.contains('─'), "页签下面该有一条分隔线:{second:?}");

        // **页脚在最底下那一行**,不是夹在中间。夹中间会让人以为版本那一行
        // 属于当页的操作(上一版就是这么错的)。
        // 比对前要去空白:一个汉字占两个终端格,buffer 里读出来是「切 页」。
        let info = row(h as u16 - 1);
        let fi = flat(&info);
        assert!(fi.contains("sbxv"), "最后一行该是页脚:{info:?}");
        assert!(fi.contains("切页"), "页脚里应有切页提示:{info:?}");
        assert!(fi.contains("退出"), "页脚里应有退出提示:{info:?}");
        assert!(fi.contains("用户:"), "页脚里应有规模:{info:?}");

        // 仪表盘是只读页,**没有**「操作」面板 —— 页脚上面直接就是内容。
        let above = row(h as u16 - 2);
        assert!(!flat(&above).contains("操作"), "仪表盘不该有操作面板:{above:?}");

        // 换到用户页:那里该有,而且它**不该**重复通用键 ——
        // 重复正是这次要拆开的那个毛病。
        let mut b = app();
        b.page = Page::Users;
        b.users = vec![stub_user(true)];
        let mut term2 = Terminal::new(TestBackend::new(120, 22)).unwrap();
        term2.draw(|f| draw(f, &b)).unwrap();
        let buf2 = term2.backend().buffer().clone();
        let row2 = |y: u16| -> String {
            (0..buf2.area.width).map(|x| buf2[(x, y)].symbol().to_string()).collect()
        };
        // 用户页的摘要是一行,所以面板 = 边框 + 摘要 + 按键 + 边框 = 4 行,
        // 标题那条边框在页脚往上第 4 行。
        let ops_title = row2(h as u16 - 5);
        assert!(flat(&ops_title).contains("操作"), "用户页该有操作面板:{ops_title:?}");
        let keys = row2(h as u16 - 3);
        assert!(!flat(&keys).contains("退出"), "操作面板不该重复通用键:{keys:?}");
        assert!(flat(&keys).contains("token"), "按键行该在这儿:{keys:?}");

        println!(
            "{}
{}",
            info.trim_end(),
            row2(h as u16 - 3).trim_end()
        );
    }

    /// `[o]` 循环切出站策略,五个取值转一圈回到原点。
    ///
    /// 做成循环而不是弹窗选单:只有五个取值,而且改完要立刻看到效果 ——
    /// 开个框选完再关比按两下慢得多。误按的代价也小(再按几下就转回来了,
    /// 而且它不动任何凭据),所以不值得为它加一次确认。
    #[tokio::test]
    async fn the_outbound_key_cycles_through_every_strategy() {
        use crate::model::outbound::OutboundStrategy;

        let mut a = app();
        a.page = Page::Agents;
        a.agents = vec![stub_agent(7)];
        assert_eq!(a.agents[0].outbound, OutboundStrategy::Auto, "默认该是自动");

        // 从 Auto 开始按一圈,顺序要和 `all()` 一致,最后回到 Auto。
        let want = OutboundStrategy::all();
        for step in 1..=want.len() {
            let act = on_key(&mut a, key('o'));
            let expect = want[step % want.len()];
            match act {
                Some(Action::SetOutbound { id, strategy, .. }) => {
                    assert_eq!(id, 7);
                    assert_eq!(strategy, expect, "第 {step} 下该切到 {expect:?}");
                    // 主循环会写库再 refresh;这里手动同步,好接着按下一下。
                    a.agents[0].outbound = strategy;
                }
                other => panic!("[o] 该产生 SetOutbound,实际 {}", other.is_some()),
            }
        }
        assert_eq!(a.agents[0].outbound, OutboundStrategy::Auto, "转一圈该回到原点");
    }

    /// 没选中任何机器时按 `[o]` 只给提示,不该 panic 也不该产生动作。
    #[tokio::test]
    async fn the_outbound_key_on_an_empty_list_just_complains() {
        let mut a = app();
        a.page = Page::Agents;
        assert!(on_key(&mut a, key('o')).is_none());
        assert!(a.status_is_error);
    }

    /// 摘要行要显示**当前**策略 —— 这是按下 `[o]` 之后唯一能确认改对了的地方
    /// (列表里没有这一列)。
    #[tokio::test]
    async fn the_ops_line_shows_the_current_strategy() {
        use crate::model::outbound::OutboundStrategy;

        let mut a = app();
        a.page = Page::Agents;
        a.agents = vec![data::AgentRow {
            outbound: OutboundStrategy::Ipv6Only,
            nic_accounting_mode: crate::model::agent::NicAccountingMode::Max,
            ..stub_agent(1)
        }];
        let ops = a.ops_lines().join("\n");
        assert!(ops.contains("仅 IPv6"), "摘要里该有当前策略:\n{ops}");
        // 网卡口径没有自己的列(那会让本来就挤的表更挤),所以摘要行是
        // 唯一能扫到它的地方 —— 少了它,「这台为什么只算了一半」无从查起。
        assert!(ops.contains("取大"), "摘要里该有当前网卡口径:\n{ops}");
    }

    /// 编辑框要**预选**这台机器当前的网卡口径,提交后原样带回来。
    ///
    /// 预选错的表现最阴:人只想改个配额,一保存却把口径顺手改回了默认,
    /// 而界面上没有任何提示 —— 下个月的账就对不上了。
    #[tokio::test]
    async fn the_agent_form_round_trips_the_nic_accounting_mode() {
        use crate::model::agent::NicAccountingMode;
        use modal::Outcome;

        for mode in NicAccountingMode::all() {
            let row = data::AgentRow { nic_accounting_mode: *mode, ..stub_agent(9) };
            let mut m = agent_edit(&row);
            let Modal::Form(f) = &m else { panic!("应当是表单") };
            let field = f.fields.iter().find(|x| x.key == "nic_mode").expect("该有口径字段");
            assert_eq!(field.value(), mode.label(), "{mode:?} 该被预选中");

            // 不动任何字段直接提交,取值必须原样回来。
            let Outcome::Run(Action::EditAgent { nic_accounting_mode, .. }) =
                m.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            else {
                panic!("Enter 该提交出一个 EditAgent")
            };
            assert_eq!(nic_accounting_mode, *mode, "提交后该还是 {mode:?}");
        }
    }

    /// 重置时区往返。两个方向都要测:
    ///   * 有覆盖值 → 预填成 `-07:00`,原样提交回来;
    ///   * 没有覆盖值 → 预填是**空串**,提交回 `None`(= 跟随 agent)。
    ///
    /// 后一半是那个空串陷阱真正会被踩到的地方:`parse_timezone("")` 是 `+00:00`,
    /// 少一层判断就会把「跟随」变成「钉死 UTC」。
    #[tokio::test]
    async fn the_agent_form_round_trips_the_nic_reset_offset() {
        use modal::Outcome;

        let row = data::AgentRow { nic_reset_offset_secs: Some(-25200), ..stub_agent(9) };
        let mut m = agent_edit(&row);
        let Modal::Form(f) = &m else { panic!("应当是表单") };
        let field = f.fields.iter().find(|x| x.key == "tz").expect("该有时区字段");
        assert_eq!(field.value(), "UTC-07:00", "该按 UTC±HH:MM 预填");
        let Outcome::Run(Action::EditAgent { nic_reset_offset_secs, .. }) =
            m.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Enter 该提交出一个 EditAgent")
        };
        assert_eq!(nic_reset_offset_secs, Some(-25200));

        let row = data::AgentRow { nic_reset_offset_secs: None, ..stub_agent(9) };
        let mut m = agent_edit(&row);
        let Modal::Form(f) = &m else { panic!("应当是表单") };
        assert_eq!(
            f.fields.iter().find(|x| x.key == "tz").unwrap().value(),
            "",
            "没有覆盖值时预填必须是空串,不能是 +00:00"
        );
        let Outcome::Run(Action::EditAgent { nic_reset_offset_secs, .. }) =
            m.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Enter 该提交出一个 EditAgent")
        };
        assert_eq!(nic_reset_offset_secs, None, "留空 = 跟随 agent,不是 UTC");
    }

    /// 编辑框的标题行里要写出 agent 报的是什么 —— 一个人要决定填不填覆盖值,
    /// 得先看见 agent 自己说它在哪个时区。
    ///
    /// 写在**标题行**而不是字段标签里:标签列宽被夹在 34 列以内,超了会被静默截成 `…`。
    #[tokio::test]
    async fn the_edit_form_shows_what_the_agent_reported() {
        let row = data::AgentRow { reported_utc_offset_secs: Some(-25200), ..stub_agent(9) };
        let m = agent_edit(&row);
        let Modal::Form(f) = &m else { panic!("应当是表单") };
        let head = f.head.as_deref().unwrap_or_default();
        assert!(head.contains("UTC-07:00"), "标题行里该有 agent 报的值:{head}");

        let row = data::AgentRow { reported_utc_offset_secs: None, ..stub_agent(9) };
        let m = agent_edit(&row);
        let Modal::Form(f) = &m else { panic!("应当是表单") };
        let head = f.head.as_deref().unwrap_or_default();
        assert!(head.contains("还没报过"), "没报过要说清楚:{head}");
    }

    /// **字段标签不能超过 34 列。**
    ///
    /// `modal.rs` 的 `label_w` 把标签列夹在 `[14, 34]`,超出的部分被 `theme::pad`
    /// **静默截成 `…`** —— 现场表现是「标签后半截看不见」,而且它是最长的那个标签
    /// 决定整列宽度,所以一个人加长自己那一项会顺带把别人的挤掉。
    /// 这条守的就是那个静默:超了在 CI 里挂,而不是等有人截图问「为什么被省略了」。
    #[tokio::test]
    async fn agent_form_labels_fit_the_label_column() {
        const CAP: usize = 34;
        let row = data::AgentRow { reported_utc_offset_secs: Some(-25200), ..stub_agent(9) };
        for (what, m) in [("编辑", agent_edit(&row)), ("新增", forms::agent_add())] {
            let Modal::Form(f) = &m else { panic!("应当是表单") };
            for field in &f.fields {
                let w = theme::cols(&field.label);
                assert!(w <= CAP, "{what}表单的「{}」标签占 {w} 列,超过 {CAP}", field.label);
            }
        }
    }

    /// 新增框不填时区 → `None`,也就是「装上之后跟随 agent 自己上报」。
    #[tokio::test]
    async fn the_add_agent_form_leaves_the_timezone_to_the_agent() {
        use modal::Outcome;
        let mut m = forms::agent_add();
        for c in "tokyo".chars() {
            m.handle(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let Outcome::Run(Action::AddAgent { nic_reset_offset_secs, .. }) =
            m.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Enter 该提交出一个 AddAgent")
        };
        assert_eq!(nic_reset_offset_secs, None);
    }

    /// 新增框默认「入出总计」——升级到这一版的行为不该变。
    /// ←/→ 循环一圈能取到全部四个口径。
    #[tokio::test]
    async fn the_add_agent_form_defaults_to_sum_and_cycles_through_every_mode() {
        use crate::model::agent::NicAccountingMode;
        use modal::Outcome;

        let all = NicAccountingMode::all();
        for (steps, want) in (0..all.len()).map(|i| (i, all[i])) {
            let mut m = forms::agent_add();
            // 焦点先落到口径那一栏(名称 → 配额 → 口径)。
            for _ in 0..2 {
                m.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            }
            for _ in 0..steps {
                m.handle(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
            }
            let Modal::Form(f) = &m else { panic!("应当是表单") };
            // 名称必填,补一个再提交。
            let name_idx = f.fields.iter().position(|x| x.key == "name").unwrap();
            let Modal::Form(f) = &mut m else { unreachable!() };
            f.focus = name_idx;
            for c in "a".chars() {
                m.handle(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            let Outcome::Run(Action::AddAgent { nic_accounting_mode, .. }) =
                m.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            else {
                panic!("Enter 该提交出一个 AddAgent")
            };
            assert_eq!(nic_accounting_mode, want, "按了 {steps} 下 → 该是 {want:?}");
        }
    }

    /// `[T]` 打开 token 管理,`[r]` 弹重置流量的确认框。
    ///
    /// 这两个键和已有的 `t`(启/停)、`R`(刷新)只差大小写,所以这条测试
    /// 盯的是**每个键各自落到哪儿** —— 接错线的表现是「按了个看起来无害的键,
    /// 结果做了另一件不可撤销的事」。
    #[tokio::test]
    async fn token_and_reset_keys_open_the_right_thing() {
        let mut a = app();
        a.page = Page::Users;
        a.users = vec![stub_user(true)];

        assert!(on_key(&mut a, key('T')).is_none(), "[T] 只开框,不该直接动手");
        assert!(matches!(a.modal, Some(Modal::Token { user_id: 1, .. })), "[T] 该开 token 管理");
        a.modal = None;

        assert!(on_key(&mut a, key('r')).is_none(), "[r] 只开框,不该直接动手");
        match &a.modal {
            Some(Modal::Confirm { body, action, .. }) => {
                assert!(
                    matches!(action, Action::ResetUserTraffic { user_id: 1, .. }),
                    "[r] 该是重置流量"
                );
                let text = body.join(" ");
                assert!(text.contains("不会改动月重置日期"), "得说清不动档期:{text}");
            }
            other => panic!("[r] 该弹确认框,实际 {other:?}", other = other.is_some()),
        }
        a.modal = None;

        // 小写 t 仍然是启/停 —— 没被 T 抢走。
        let act = on_key(&mut a, key('t'));
        assert!(matches!(act, Some(Action::SetUserEnabled { .. })), "[t] 该还是启/停");
    }

    /// token 框里 `[g]` / `[v]` 各自触发哪个动作;撤销过的不再给 `[v]`。
    #[tokio::test]
    async fn the_token_modal_offers_revoke_only_while_active() {
        let mut m = Modal::Token { user_id: 7, name: "alice".into(), active: true };
        assert!(
            matches!(m.handle(key('v')), Outcome::Run(Action::RevokeSubToken { user_id: 7, .. })),
            "开着的时候 [v] 该能撤销"
        );
        assert!(
            matches!(m.handle(key('g')), Outcome::Run(Action::RegenSubToken { user_id: 7, .. })),
            "[g] 该重新生成"
        );

        // 已撤销:[v] 不再有动作(免得对着一个已经关掉的订阅再关一次),
        // 但 [g] 必须还在 —— 它是唯一的恢复路径。
        let mut m = Modal::Token { user_id: 7, name: "alice".into(), active: false };
        assert!(matches!(m.handle(key('v')), Outcome::Close(_)), "已撤销时 [v] 该没有动作");
        assert!(
            matches!(m.handle(key('g')), Outcome::Run(Action::RegenSubToken { .. })),
            "已撤销时 [g] 必须还能恢复"
        );
    }

    /// 把「操作」面板和页脚画出来看一眼(用户页,有选中项)。
    ///
    /// ```sh
    /// cargo test tui::tests::preview_footer -- --nocapture
    /// ```
    #[tokio::test]
    async fn preview_footer() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut a = app();
        a.page = Page::Users;
        a.users = vec![stub_user(true)];
        a.agents = vec![stub_agent(1)];
        a.nodes = vec![stub_node(1, "tokyo-reality")];
        a.cfg.subscription.public_base = "https://sub.example.com".into();

        a.nodes = vec![stub_node(1, "vless")];
        for (label, page) in
            [("用户页", Page::Users), ("节点页", Page::Nodes), ("仪表盘", Page::Dashboard)]
        {
            // 每页一个**新**终端。复用同一个 TestBackend 跨页画会留下上一页的残字:
            // 面板高度不同,而汉字占两格,旧内容的后半格不会被覆盖。
            let mut term = Terminal::new(TestBackend::new(140, 20)).unwrap();
            a.page = page;
            term.draw(|f| draw(f, &a)).unwrap();
            let buf = term.backend().buffer().clone();
            let h = buf.area.height;
            println!("── {label} ──");
            for y in (h - 7)..h {
                let line: String =
                    (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
                println!("{}", line.trim_end());
            }
            println!();
        }
    }

    /// **摘要行要说清「离线多久」。**
    ///
    /// 表里只有一个红点,而决定要不要动身去查那台机器的是时长:离线 3 分钟大概是
    /// 它自己在重连(agent 退避上限 60 秒),离线 3 天就是机器或防火墙的事。
    /// 这个信息以前只在库里,排查时得手工 `SELECT last_seen` —— 而那正是最不该
    /// 需要开一个 sqlite 会话的时刻。
    #[tokio::test]
    async fn the_agent_ops_line_says_how_long_it_has_been_offline() {
        let now = chrono::Local::now().timestamp();
        let line = |a: data::AgentRow| {
            let mut app = app();
            app.page = Page::Agents;
            app.agents = vec![a];
            app.ops_lines().join("\n")
        };

        let off = data::AgentRow {
            status: "offline".into(),
            last_seen: Some(now - 3 * 86_400 - 5 * 3600),
            ..stub_agent(1)
        };
        let out = line(off);
        assert!(out.contains("● 离线 3 天 5 小时"), "离线时长要跟在状态后面:{out}");

        // 在线的机器不写时长:`last_seen` 约等于现在,写出来是噪声。
        let on =
            data::AgentRow { status: "online".into(), last_seen: Some(now - 3), ..stub_agent(1) };
        let out = line(on);
        assert!(out.contains("● 在线"), "{out}");
        assert!(!out.contains("分"), "在线不该带时长:{out}");

        // 从没连上过:状态那一格已经写着「从未连接」,不再重复一遍。
        // 它和「连过又断了」是两回事 —— 前者查 token/防火墙,后者查机器/网络。
        let never = data::AgentRow {
            status: "never".into(),
            last_seen: None,
            custom_json: None,
            ..stub_agent(1)
        };
        let out = line(never);
        assert!(out.contains("○ 从未连接"), "{out}");
        assert!(!out.contains("天"), "没连过就没有「多久」可说:{out}");

        // 时钟回拨或库被手改过时不能显示「离线 -5 天」,宁可不说。
        //
        // 断言盯的是 `offline_for` 自己而不是整条摘要行。盯整行(比如
        // `!out.contains('-')`)看着更严,实际是错的:真实的 `token_prefix` 是
        // token 前 8 位,而 token 是 base64url —— **本身就能带 `-`**。
        // 那样写能过只是因为 stub 用了 `abcd1234`;哪天有人把 stub 改得更真实
        // (那是个改进),测试会因为完全无关的原因挂掉。
        let future = data::AgentRow {
            status: "offline".into(),
            last_seen: Some(now + 9999),
            ..stub_agent(1)
        };
        assert_eq!(offline_for(&future, now), "", "未来的 last_seen 不该算出时长");
        let out = line(future);
        assert!(!out.contains("离线 -"), "摘要行里不能出现负数时长:{out}");
    }

    /// 「操作」摘要行要带上那几件**列里放不下、又必须看得见**的事。
    ///
    /// 这三条原来盯的是「详情」面板。那个面板和摘要行前半段逐字重复,
    /// 已经去掉 —— 守卫跟着搬过来,否则删掉面板的同时也把这几条保证删了。
    #[tokio::test]
    async fn ops_lines_carry_what_the_table_cannot_show() {
        // ① 中转:客户端实际连的不是节点自身端口。丢了这句会有人对着
        //    节点端口查为什么连不上。
        let mut a = app();
        a.page = Page::Nodes;
        let mut n = stub_node(1, "tokyo-reality");
        n.params.relay =
            crate::model::node::RelaySetting { host: "198.51.100.9".into(), port: Some(12345) };
        a.nodes = vec![n];
        let ops = a.ops_lines().join("\n");
        assert!(ops.contains("198.51.100.9:12345"), "中转落点要显示出来:\n{ops}");
        assert!(ops.contains("客户端连这里"), "{ops}");

        // ② 订阅地址:这一页最常要看的东西(要发给用户)。
        let mut a = app();
        a.page = Page::Users;
        a.users = vec![stub_user(true)];
        a.cfg.subscription.public_base = "https://sub.example.com/".into();
        let ops = a.ops_lines().join("\n");
        assert!(ops.contains("https://sub.example.com/sub/"), "订阅地址要给全:\n{ops}");

        // ③ 一个节点都没分配的用户,订阅是空的 —— 必须显眼。
        let mut a = app();
        a.page = Page::Users;
        a.users = vec![data::UserRow { node_ids: vec![], ..stub_user(true) }];
        let ops = a.ops_lines().join("\n");
        assert!(ops.contains("未分配"), "没分配节点要说清楚:\n{ops}");
    }

    /// **摘要行绝不能渲染密钥材料**(§11.3)。
    ///
    /// `NodeParams` 里躺着 reality 私钥、证书私钥、ss 服务端密钥。
    /// 摘要行现在要取 `server_name` / `path` / `relay` 这几项,取的时候
    /// 一不小心就会把整个 params 塞进去 —— 而那是个**没有任何报错**的泄露。
    #[tokio::test]
    async fn ops_lines_never_leak_key_material() {
        let mut a = app();
        a.page = Page::Nodes;
        let mut n = stub_node(1, "tokyo-reality");
        n.params.private_key = Some("PRIVATE-KEY-MUST-NOT-APPEAR".into());
        n.params.key_pem = Some("KEY-PEM-MUST-NOT-APPEAR".into());
        n.params.ss_password = Some("SS-PASSWORD-MUST-NOT-APPEAR".into());
        a.nodes = vec![n];
        let ops = a.ops_lines().join("\n");
        assert!(!ops.contains("MUST-NOT-APPEAR"), "密钥材料被写进摘要行了:\n{ops}");
    }

    /// 仪表盘**不该**有「操作」面板 —— 它是只读页,没有专有操作。
    /// 摆一个只写着「要动手请去别处」的空框,纯粹浪费四行(sb-manager 也没有)。
    #[tokio::test]
    async fn the_dashboard_has_no_ops_panel() {
        let mut a = app();
        a.page = Page::Dashboard;
        assert!(a.ops_lines().is_empty(), "仪表盘不该有操作摘要");

        // 其余页都要有。
        for page in [Page::Agents, Page::Nodes, Page::Users, Page::Settings] {
            a.page = page;
            assert!(!a.ops_lines().is_empty(), "{page:?} 该有操作摘要");
        }
    }

    /// 仪表盘的 Enter 要打开**光标停的那一个**。
    ///
    /// 两张表都按用量排过序,而 `users` / `nodes` 本身是按 id 排的。
    /// 选中的下标是排序后的行号 —— 解析时必须走同一份次序,否则光标停在
    /// 第 1 行、打开的却是另一个人。这种错不报任何错,只会给出一张
    /// 「看起来正常但属于别人」的明细表,所以专门钉住。
    #[tokio::test]
    async fn the_dashboard_drills_into_the_highlighted_row() {
        let mut a = app();
        a.page = Page::Dashboard;
        // id 顺序 1 → 2;用量顺序 2(大)→ 1(小)。两者刚好相反。
        a.users = vec![
            data::UserRow { id: 1, name: "small".into(), cycle_up: 1, ..stub_user(true) },
            data::UserRow { id: 2, name: "big".into(), cycle_up: 9_999, ..stub_user(true) },
        ];

        a.dash_sel = [0, 0];
        match on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            Some(Action::ShowUserNodes { id, ref name }) => {
                assert_eq!((id, name.as_str()), (2, "big"), "第 0 行该是用量最大的那个");
            }
            other => panic!("该开用户明细,实际 {}", other.is_some()),
        }

        a.dash_sel = [1, 0];
        let act = on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Some(Action::ShowUserNodes { id: 1, .. })), "第 1 行该是 small");
    }

    /// ←/→ 在仪表盘上换的是**栏**,不是页 —— 两张表就是左右摆着的。
    /// 换页仍然走 Tab 和 1-5。
    #[tokio::test]
    async fn arrows_switch_panes_on_the_dashboard_but_pages_elsewhere() {
        let mut a = app();
        a.page = Page::Dashboard;
        a.users = vec![stub_user(true)];
        a.nodes = vec![stub_node(1, "n1")];

        assert!(!a.dash_on_nodes, "默认焦点在左边的用户栏");
        on_key(&mut a, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(a.dash_on_nodes, "→ 该把焦点挪到节点栏");
        assert_eq!(a.page, Page::Dashboard, "→ 不该顺手翻页");

        let act = on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(act, Some(Action::ShowNodeUsers { id: 1, .. })),
            "焦点在节点栏就开节点明细"
        );

        on_key(&mut a, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(!a.dash_on_nodes, "← 该挪回用户栏");

        on_key(&mut a, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(a.page, Page::Agents, "Tab 照旧翻页");

        on_key(&mut a, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(a.page, Page::Nodes, "非仪表盘页上 → 仍是翻页");
    }

    /// 两栏各有自己的光标,上下键只动有焦点的那一栏。
    #[tokio::test]
    async fn each_dashboard_pane_keeps_its_own_cursor() {
        let mut a = app();
        a.page = Page::Dashboard;
        a.users = vec![stub_user(true), stub_user(true), stub_user(true)];
        a.nodes = vec![stub_node(1, "n1"), stub_node(2, "n2")];

        on_key(&mut a, key('j'));
        assert_eq!(a.dash_sel, [1, 0], "只该动用户栏的光标");

        on_key(&mut a, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        on_key(&mut a, key('j'));
        assert_eq!(a.dash_sel, [1, 1], "节点栏有自己的光标,用户栏那个不该被动");

        // 节点栏只有 2 行,再往下绕回 0(与别的页一致)。
        on_key(&mut a, key('j'));
        assert_eq!(a.dash_sel[1], 0, "该绕回第一行");
    }

    /// `[u]` 开升级框,`[a]` / `[u]` 各自对应「全部」与「这一台」。
    ///
    /// TUI **不会**自己发 RPC(它和 daemon 是两个进程),这里只验到
    /// 「产生了正确的 Action」为止;真正的下发在 daemon 的巡检里。
    #[tokio::test]
    async fn the_upgrade_modal_offers_one_or_all() {
        let mut a = app();
        a.page = Page::Agents;
        a.agents = vec![stub_agent(3)];

        assert!(on_key(&mut a, key('u')).is_none(), "[u] 只开框");
        match &a.modal {
            Some(Modal::Upgrade { agent_id, online, .. }) => {
                assert_eq!(*agent_id, 3);
                assert_eq!(*online, 1, "在线台数要写出来 —— 「全部升」会碰几台得让人看见");
            }
            _ => panic!("[u] 该开升级框"),
        }

        let mut m = Modal::Upgrade {
            agent_id: 3,
            name: "tokyo".into(),
            online: 2,
            version: "9.9.9".into(),
        };
        assert!(
            matches!(m.handle(key('u')), Outcome::Run(Action::UpgradeAgents { only: Some(3), .. })),
            "[u] 只升这一台"
        );
        assert!(
            matches!(m.handle(key('a')), Outcome::Run(Action::UpgradeAgents { only: None, .. })),
            "[a] 升全部"
        );
        // 别的键一律取消 —— 「全部升级」会让整个集群依次重启,不该有手滑的可能。
        assert!(matches!(m.handle(key('x')), Outcome::Close(_)), "别的键该取消");
    }

    /// 绑网卡是**多选**,而且打开时已绑的要是勾上的 —— 与分配节点同一个道理:
    /// 一片空白会让人以为原来什么都没绑,一保存就把绑定全清了。
    #[tokio::test]
    async fn the_nic_picker_starts_from_the_current_binding() {
        let mut a = app();
        a.page = Page::Users;
        a.agents = vec![stub_agent(1), stub_agent(2), stub_agent(3)];
        let mut u = stub_user(true);
        u.nic_agent_ids = vec![1, 3];
        a.users = vec![u];

        on_key(&mut a, key('b'));
        let Some(Modal::Picker(p)) = &a.modal else { panic!("应当开一个多选框") };
        let checked: Vec<i64> = p.items.iter().filter(|i| i.checked).map(|i| i.id).collect();
        assert_eq!(checked, vec![1, 3]);
        // 抬头必须说清它只影响订阅报出去的数字。
        assert!(p.head.contains("订阅"), "{}", p.head);
    }

    /// 没有 agent 时不能开绑定框 —— 那个框里一个可勾的都没有。
    #[tokio::test]
    async fn binding_nics_without_any_agent_explains_itself() {
        let mut a = app();
        a.page = Page::Users;
        a.users = vec![stub_user(true)];
        on_key(&mut a, key('b'));
        assert!(a.modal.is_none());
        assert!(a.status_is_error);
    }

    fn app() -> App {
        // 只用来测按键与选择逻辑,不碰数据库。
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        App::new(pool, Config::default(), "sbx-test.toml".into())
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[tokio::test]
    async fn tab_cycles_through_all_five_pages() {
        let mut a = app();
        assert!(a.page == Page::Dashboard);
        for want in [Page::Agents, Page::Nodes, Page::Users, Page::Settings, Page::Dashboard] {
            on_key(&mut a, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert!(a.page == want, "Tab 顺序不对");
        }
    }

    /// 数字键直达。Tab 要按三下才到得了最后一页,而人心里想的是「去第 4 页」。
    #[tokio::test]
    async fn number_keys_jump_straight_to_a_page() {
        let mut a = app();
        for (c, want) in [
            ('3', Page::Nodes),
            ('1', Page::Dashboard),
            ('5', Page::Settings),
            ('4', Page::Users),
            ('2', Page::Agents),
        ] {
            on_key(&mut a, key(c));
            assert!(a.page == want, "按 {c} 应当到对应的页");
        }
        // 6 不是页码,不该有任何反应(也不该被当成别的快捷键)。
        on_key(&mut a, key('6'));
        assert!(a.page == Page::Agents);
    }

    /// 空列表时按上下键不能 panic,也不能把光标挪到不存在的行。
    #[tokio::test]
    async fn navigation_on_an_empty_list_is_safe() {
        let mut a = app();
        a.page = Page::Agents;
        on_key(&mut a, key('j'));
        on_key(&mut a, key('k'));
        assert_eq!(a.sel[Page::Agents as usize], 0);
    }

    /// 概览页没有可选中的行 —— 上下键在那里必须是空操作。
    #[tokio::test]
    async fn dashboard_has_nothing_to_select() {
        let mut a = app();
        a.agents = (0..3).map(stub_agent).collect();
        on_key(&mut a, key('j'));
        assert_eq!(a.sel[Page::Dashboard as usize], 0);
        assert_eq!(a.sel[Page::Agents as usize], 0, "不该动到别的页的选择");
    }

    #[tokio::test]
    async fn navigation_wraps_around() {
        let mut a = app();
        a.page = Page::Agents;
        a.agents = (0..3).map(stub_agent).collect();
        on_key(&mut a, key('k'));
        assert_eq!(a.sel[Page::Agents as usize], 2, "从第一行往上应当绕到最后一行");
        on_key(&mut a, key('j'));
        assert_eq!(a.sel[Page::Agents as usize], 0);
    }

    /// 弹窗打开时,页面快捷键必须被吃掉 ——
    /// 否则在输入框里打一个 'q' 会直接退出程序。
    #[tokio::test]
    async fn modal_swallows_page_shortcuts() {
        let mut a = app();
        a.modal = Some(forms::user_add());
        on_key(&mut a, key('q'));
        assert!(!a.quit, "'q' 应当被当成输入,不是退出");
        let Some(Modal::Form(f)) = &a.modal else { panic!() };
        assert_eq!(f.fields[0].value(), "q");
    }

    /// 数字键在弹窗里同样必须是**输入**,不是切页。
    /// 端口 8443 里的每一个数字都会撞上页签快捷键。
    #[tokio::test]
    async fn digits_type_into_a_modal_instead_of_switching_pages() {
        let mut a = app();
        a.page = Page::Nodes;
        a.modal = Some(forms::user_add());
        for c in ['1', '2', '3', '4'] {
            on_key(&mut a, key(c));
        }
        assert!(a.page == Page::Nodes, "弹窗开着时不该切页");
        let Some(Modal::Form(f)) = &a.modal else { panic!() };
        assert_eq!(f.fields[0].value(), "1234");
    }

    /// 确认框只认 y。其它键一律当取消 —— 删除类操作不该有「手滑确认」的可能。
    #[tokio::test]
    async fn confirm_only_accepts_y() {
        for (k, want_action) in [('y', true), ('n', false), ('d', false), (' ', false)] {
            let mut a = app();
            a.modal =
                Some(Modal::confirm("删", vec![], Action::DeleteAgent { id: 1, name: "x".into() }));
            let act = on_key(&mut a, key(k));
            assert_eq!(act.is_some(), want_action, "按 {k:?} 时的行为不对");
            assert!(a.modal.is_none());
        }
    }

    /// 启停是**一个键切换**:按之前是启用,按完就该是停用。
    #[tokio::test]
    async fn t_toggles_the_selected_user() {
        let mut a = app();
        a.page = Page::Users;
        a.users = vec![stub_user(true)];
        let Some(Action::SetUserEnabled { enabled, .. }) = on_key(&mut a, key('t')) else {
            panic!("应当产生一个启停动作")
        };
        assert!(!enabled, "当前是启用,按 t 应当停用");

        a.users = vec![stub_user(false)];
        let Some(Action::SetUserEnabled { enabled, .. }) = on_key(&mut a, key('t')) else {
            panic!()
        };
        assert!(enabled);
    }

    /// 分配框打开时,已分配的节点必须是**勾上的**。
    /// 一片空白会让人以为原来什么都没选,一保存就把现有分配全清了。
    #[tokio::test]
    async fn the_node_picker_starts_from_the_current_assignment() {
        let mut a = app();
        a.page = Page::Users;
        a.nodes = vec![stub_node(7, "n7"), stub_node(8, "n8"), stub_node(9, "n9")];
        let mut u = stub_user(true);
        u.node_ids = vec![7, 9];
        a.users = vec![u];

        on_key(&mut a, key('n'));
        let Some(Modal::Picker(p)) = &a.modal else { panic!("应当开一个多选框") };
        let checked: Vec<i64> = p.items.iter().filter(|i| i.checked).map(|i| i.id).collect();
        assert_eq!(checked, vec![7, 9]);
        // 同名 tag 可以出现在不同机器上,备注必须能把它们分开。
        assert!(p.items[0].note.contains("tokyo-1"), "{}", p.items[0].note);
    }

    /// 没有 agent 时不能开节点表单 —— 那个表单的第一个字段就没有可选项。
    #[tokio::test]
    async fn adding_a_node_without_any_agent_explains_itself() {
        let mut a = app();
        a.page = Page::Nodes;
        on_key(&mut a, key('a'));
        assert!(a.modal.is_none());
        assert!(a.status_is_error);
        assert!(a.status.as_deref().unwrap().contains("服务管理"), "{:?}", a.status);
    }

    fn stub_agent(id: i64) -> data::AgentRow {
        data::AgentRow {
            id,
            name: format!("a{id}"),
            token_prefix: "abcd1234".into(),
            status: "online".into(),
            agent_version: None,
            arch: Some("amd64".into()),
            outbound: Default::default(),
            ipv4: None,
            ipv6: None,
            nic_quota_bytes: None,
            nic_reset_day: None,
            nic_accounting_mode: Default::default(),
            reported_utc_offset_secs: None,
            nic_reset_offset_secs: None,
            cycle_rx: 0,
            cycle_tx: 0,
            up_per_sec: None,
            down_per_sec: None,
            node_count: 0,
            cpu_pct: None,
            mem_used: None,
            mem_total: None,
            load1: None,
            uptime_secs: None,
            sysinfo_at: None,
            last_seen: None,
            custom_json: None,
        }
    }

    fn stub_node(id: i64, tag: &str) -> data::NodeRow {
        data::NodeRow {
            id,
            agent_id: 1,
            agent_name: "tokyo-1".into(),
            tag: tag.into(),
            protocol: "vless-reality".into(),
            listen_port: 443,
            user_count: 0,
            cycle_up: 0,
            cycle_down: 0,
            params: Default::default(),
        }
    }

    fn stub_user(enabled: bool) -> data::UserRow {
        data::UserRow {
            id: 1,
            name: "alice".into(),
            enabled,
            auto_disabled: false,
            quota_bytes: 0,
            cycle_up: 0,
            cycle_down: 0,
            traffic_multiplier: 1.0,
            expire_at: None,
            reset_day: None,
            node_ids: vec![],
            nic_agent_ids: vec![],
            sub_token: "t".into(),
        }
    }

    /// 走一遍**真库 → 数据层 → 渲染**的完整路径。
    ///
    /// 上面那些测试各自只盖一段:按键逻辑用假数据,渲染用手搓的结构体。
    /// 这一条把它们串起来 —— SQL 写错列顺序、JOIN 漏了行、渲染读了空表,
    /// 都只有在真的查一次库之后才暴露。
    #[tokio::test]
    async fn loads_real_rows_and_renders_every_page() {
        use crate::model::node::{NodeParams, Protocol};
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let path = std::env::temp_dir().join(format!("sbx-tui-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();

        let (agent_id, _) = crate::db::agent_repo::create(&pool, "tokyo-1", 0).await.unwrap();
        sqlx::query(
            "UPDATE agents SET ipv4 = '203.0.113.8', ipv6 = '2001:db8:1:aaaa::1',
                    status = 'online', agent_version = 'v0.1.0',
                    nic_quota_bytes = 536870912000, nic_reset_day = 22 WHERE id = ?",
        )
        .bind(agent_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO agent_nic_traffic
                (agent_id, boot_id, last_rx, last_tx, cycle_rx, cycle_tx, cycle_start, updated_at)
             VALUES (?, 'boot-a', 1000, 2000, 36507222016, 1073741824, 0, 1000)",
        )
        .bind(agent_id)
        .execute(&pool)
        .await
        .unwrap();
        crate::db::agent_repo::log_event(
            &pool,
            Some(agent_id),
            "counter_reset",
            "计数器重置",
            1000,
        )
        .await
        .unwrap();

        let mut params = NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
        let (node_id, _) = crate::db::node_repo::add_node(
            &pool,
            agent_id,
            "tokyo-reality",
            Protocol::VlessReality,
            8443,
            &params,
        )
        .await
        .unwrap();
        let uid =
            crate::db::node_repo::add_user(&pool, "alice", 100 * 1_073_741_824, 0).await.unwrap();
        crate::db::node_repo::assign_node(&pool, uid, node_id).await.unwrap();

        let mut app = App::new(pool, Config::default(), "sbx-test.toml".into());
        app.refresh().await.expect("刷新不该失败");

        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.agents[0].name, "tokyo-1");
        assert_eq!(app.agents[0].node_count, 1, "JOIN 出来的节点数不对");
        assert_eq!(app.agents[0].nic_reset_day, Some(22));
        assert_eq!(app.agents[0].quota_ratio(), Some(0.07), "36.5G/500G ≈ 7%");
        // 第一次刷新只有一个采样点,速率必须是 None(界面显示 `--`)。
        assert_eq!(app.agents[0].up_per_sec, None);

        assert_eq!(app.nodes.len(), 1);
        assert_eq!(app.nodes[0].tag, "tokyo-reality");
        assert_eq!(app.nodes[0].user_count, 1);
        // reality 的 server_name 由 secrets::fill 填的默认值,编辑表单要靠它预填。
        assert_eq!(app.nodes[0].params.server_name.as_deref(), Some("www.apple.com"));

        assert_eq!(app.users.len(), 1);
        assert_eq!(app.users[0].name, "alice");
        assert_eq!(app.users[0].node_ids, vec![node_id]);

        // 用量明细两个方向都要查得出来(节点页 / 用户页的 Enter)。
        // 节点方向每行带倍率标记 —— 同一个节点上的用户可以各是各的倍率,
        // 而那一行的数字都乘过它,不标就没法解释(§6.3)。
        let by_node = data::node_breakdown(&app.pool, node_id).await.unwrap();
        assert_eq!(by_node.len(), 1);
        assert_eq!(by_node[0].label, "alice");
        assert_eq!(by_node[0].mult.as_deref(), Some("[2.0x]"));
        let by_user = data::user_breakdown(&app.pool, uid).await.unwrap();
        assert_eq!(by_user.len(), 1);
        assert!(by_user[0].label.starts_with("tokyo-reality @ tokyo-1"), "{}", by_user[0].label);

        // 四个页面都要能画出来。ratatui 越界写入是直接 panic 的,
        // 所以「画得出来」本身就是一条有效断言。
        let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
        for page in [Page::Dashboard, Page::Agents, Page::Nodes, Page::Users, Page::Settings] {
            app.page = page;
            term.draw(|f| draw(f, &app)).unwrap();
        }
        // 「操作」面板的摘要要认出选中项(token 前缀是日志对号用的,§8.1)。
        app.page = Page::Agents;
        let ops = app.ops_lines().join(" ");
        assert!(ops.contains("tokyo-1"), "{ops}");
        assert!(ops.contains("token:"), "{ops}");
    }

    /// 编辑节点**不能动密钥材料**。这条是 §9.1 最贵的那个错误的回归锚点:
    /// 换一套 reality 密钥 = 所有客户端静默失联,而界面上什么异常都没有。
    #[tokio::test]
    async fn editing_a_node_preserves_its_key_material() {
        use crate::model::node::{NodeParams, Protocol};

        let path = std::env::temp_dir().join(format!("sbx-tui-edit-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();
        let (agent_id, _) = crate::db::agent_repo::create(&pool, "a", 0).await.unwrap();
        let mut params = NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
        let (node_id, _) = crate::db::node_repo::add_node(
            &pool,
            agent_id,
            "in",
            Protocol::VlessReality,
            443,
            &params,
        )
        .await
        .unwrap();

        let mut app = App::new(pool, Config::default(), "sbx-test.toml".into());
        app.refresh().await.unwrap();

        let msg = perform_inner(
            &mut app,
            &Action::EditNode {
                id: node_id,
                draft: modal::NodeDraft {
                    agent_id,
                    tag: String::new(),
                    protocol: Protocol::VlessReality,
                    port: 9443,
                    server_name: Some("www.microsoft.com".into()),
                    path: None,
                    ipv6: true,
                    relay_host: "198.51.100.9".into(),
                    relay_port: Some(20443),
                },
            },
        )
        .await
        .unwrap();
        // 回执要说清「什么时候生效」——TUI 只改库,下发是 daemon 的事。
        // (原来这里断言的是 "config_revision",那只是把内部计数器摆给人看,
        //  已经从回执里去掉了。)
        //
        // 断言「下发」而不是笼统的「生效」:要盯的是**有没有把下发这件事说出来**。
        // 早先回执写「由 daemon 下发生效」,而那时根本没有下发这条路 ——
        // 一句话读起来没毛病却是假的。现在那条路真的有了(v0.4.10)。
        assert!(msg.contains("下发"), "回执该说明下发:{msg}");

        app.refresh().await.unwrap();
        let n = &app.nodes[0];
        assert_eq!(n.listen_port, 9443);
        assert_eq!(n.params.server_name.as_deref(), Some("www.microsoft.com"));
        assert!(n.params.ipv6);
        assert_eq!(n.params.relay.host, "198.51.100.9");
        assert_eq!(n.params.relay.port, Some(20443));
        assert_eq!(n.params.private_key, params.private_key, "reality 私钥被换掉了");
        assert_eq!(n.params.short_id, params.short_id, "short_id 被换掉了");
    }

    /// 清空 server_name 之后,下发时必须还能组装出配置 ——
    /// reality 的 inbound 缺了它会直接报错。表单允许留空,补默认值是这里的责任。
    #[tokio::test]
    async fn clearing_a_required_field_falls_back_to_the_default() {
        use crate::model::node::{NodeParams, Protocol};

        let path = std::env::temp_dir().join(format!("sbx-tui-clear-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();
        let (agent_id, _) = crate::db::agent_repo::create(&pool, "a", 0).await.unwrap();
        let mut params = NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
        let (node_id, _) = crate::db::node_repo::add_node(
            &pool,
            agent_id,
            "in",
            Protocol::VlessReality,
            443,
            &params,
        )
        .await
        .unwrap();

        let mut app = App::new(pool, Config::default(), "sbx-test.toml".into());
        app.refresh().await.unwrap();
        perform_inner(
            &mut app,
            &Action::EditNode {
                id: node_id,
                draft: modal::NodeDraft {
                    agent_id,
                    tag: String::new(),
                    protocol: Protocol::VlessReality,
                    port: 443,
                    server_name: None,
                    path: None,
                    ipv6: false,
                    relay_host: String::new(),
                    relay_port: None,
                },
            },
        )
        .await
        .unwrap();

        app.refresh().await.unwrap();
        assert_eq!(
            app.nodes[0].params.server_name.as_deref(),
            Some("www.apple.com"),
            "留空之后应当回落到默认值,而不是留一个下发时会报错的 None"
        );
    }

    /// **`[c]` 给出的必须是能直接 `sing-box -c` 跑起来的那份原文。**
    ///
    /// 三件事一起钉住:
    ///   1. 结构完整 —— `inbounds` / `outbounds` / `log` 都在;
    ///   2. 改过出站策略的机器,`dns` 与 `route.default_domain_resolver` 不能少
    ///      (缺了 server tag,sing-box 起不来);
    ///   3. **凭据是原文** —— 遮掉私钥的配置跑不起来,那这一页就只剩个大概样子。
    ///      这一页只在主控的终端上、按 [c] 才打开,而这些密钥本来就产自主控(§9.1)。
    /// **`[K]` 校验的必须是下发给它的那一份字节。**
    ///
    /// 抽一份可能不一样的去验等于没验。这条把入队的 payload 和 `[c]` 那一页逐字节对上 ——
    /// 两边都该是 `service::build_agent_config` 的输出。哪天有人给校验路径单独组一份
    /// 「简化版」配置,这里会挂。
    #[tokio::test]
    async fn a_check_validates_exactly_the_bytes_that_get_pushed() {
        use crate::model::node::{NodeParams, Protocol};

        let path = std::env::temp_dir().join(format!("sbx-chk-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();
        let (agent_id, _) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();
        let mut params = NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
        let (node_id, _) = crate::db::node_repo::add_node(
            &pool,
            agent_id,
            "in-1",
            Protocol::VlessReality,
            8443,
            &params,
        )
        .await
        .unwrap();
        let uid = crate::db::node_repo::add_user(&pool, "alice", 0, 0).await.unwrap();
        crate::db::node_repo::assign_node(&pool, uid, node_id).await.unwrap();

        let mut app = App::new(pool.clone(), Config::default(), "sbx-test.toml".into());
        app.refresh().await.unwrap();
        // 在线才能校验 —— 手动把状态抬成 online(没有真的 WS 连接)。
        for a in &mut app.agents {
            a.status = "online".into();
        }

        perform_inner(&mut app, &Action::CheckAgentConfig { id: agent_id, name: "tokyo".into() })
            .await
            .unwrap();
        let w = app.check_wait.clone().expect("该记下正在等结果");

        let queued: String =
            sqlx::query_scalar("SELECT payload_json FROM agent_commands WHERE id = ?")
                .bind(w.cmd_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let queued: serde_json::Value = serde_json::from_str(&queued).unwrap();
        let want = crate::service::build_agent_config(&pool, agent_id).await.unwrap();
        assert_eq!(queued["options"], want, "入队的配置与下发的不是同一份");

        let kind: String = sqlx::query_scalar("SELECT kind FROM agent_commands WHERE id = ?")
            .bind(w.cmd_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kind, "config_check", "daemon 靠 kind 认它");
    }

    /// 离线的机器**不入队**。
    ///
    /// 入了队的下场是 daemon 取走、发不出去、回一句「agent 不在线」——
    /// 绕一大圈才告诉人一件按键的那一刻就能知道的事。
    #[tokio::test]
    async fn checking_an_offline_agent_does_not_queue_anything() {
        let path = std::env::temp_dir().join(format!("sbx-chkoff-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();
        let (agent_id, _) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();
        let mut app = App::new(pool.clone(), Config::default(), "sbx-test.toml".into());
        app.refresh().await.unwrap();

        let msg = perform_inner(
            &mut app,
            &Action::CheckAgentConfig { id: agent_id, name: "tokyo".into() },
        )
        .await
        .unwrap();
        assert!(msg.contains("不在线"), "要当场说清为什么不能验:{msg}");
        assert!(app.check_wait.is_none(), "不该进等待状态");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_commands")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "一条都不该入队");
    }

    /// **结果要真的回到状态行。**
    ///
    /// 校验是跨进程异步的,没有这一步的话按下 `K` 之后就永远没下文了。
    /// 错误分支特别要把 **sing-box 的原文**带出来:主控里没有 sing-box,
    /// 字段名拼错这类错只有它能报。
    #[tokio::test]
    async fn a_finished_check_shows_up_in_the_status_line() {
        let path = std::env::temp_dir().join(format!("sbx-chkres-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();
        let (agent_id, _) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();
        let mut app = App::new(pool.clone(), Config::default(), "sbx-test.toml".into());

        // 成功
        let id = crate::db::command_repo::enqueue(
            &pool,
            agent_id,
            "config_check",
            &serde_json::json!({}),
            0,
        )
        .await
        .unwrap();
        crate::db::command_repo::finish(&pool, id, None, 1).await.unwrap();
        app.check_wait = Some(CheckWait { cmd_id: id, agent: "tokyo".into(), at: 0 });
        app.poll_config_check(2).await;
        assert!(app.check_wait.is_none(), "出结果了就不该还在等");
        assert!(!app.status_is_error, "通过不是错误");
        assert!(app.status.as_deref().unwrap_or_default().contains("通过"), "{:?}", app.status);

        // 失败 —— 原文要在
        let id2 = crate::db::command_repo::enqueue(
            &pool,
            agent_id,
            "config_check",
            &serde_json::json!({}),
            0,
        )
        .await
        .unwrap();
        crate::db::command_repo::finish(&pool, id2, Some("json: unknown field \"outbonds\""), 1)
            .await
            .unwrap();
        app.check_wait = Some(CheckWait { cmd_id: id2, agent: "tokyo".into(), at: 0 });
        app.poll_config_check(2).await;
        let s = app.status.clone().unwrap_or_default();
        assert!(app.status_is_error, "没通过该标成错误:{s}");
        assert!(s.contains("outbonds"), "sing-box 的原文丢了:{s}");
    }

    /// **没人取走要说成「daemon 没在跑」,不能说成校验失败。**
    ///
    /// 两者的下一步完全不同:一个去 `systemctl status sbx`,一个去改配置。
    /// 而在宽容限之内还不能下结论 —— 巡检周期默认 30 秒,提前报错是误报。
    #[tokio::test]
    async fn a_command_nobody_takes_blames_the_daemon_not_the_config() {
        let path = std::env::temp_dir().join(format!("sbx-chknod-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();
        let (agent_id, _) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();
        let mut app = App::new(pool.clone(), Config::default(), "sbx-test.toml".into());
        let id = crate::db::command_repo::enqueue(
            &pool,
            agent_id,
            "config_check",
            &serde_json::json!({}),
            0,
        )
        .await
        .unwrap();
        app.check_wait = Some(CheckWait { cmd_id: id, agent: "tokyo".into(), at: 0 });

        // 宽容限之内:什么都不说,继续等。
        app.poll_config_check(CHECK_TAKEN_GRACE_SECS).await;
        assert!(app.check_wait.is_some(), "还在宽容限内,不该下结论");
        assert!(app.status.is_none(), "也不该刷状态行:{:?}", app.status);

        // 超了:指向 daemon。
        app.poll_config_check(CHECK_TAKEN_GRACE_SECS + 1).await;
        let s = app.status.clone().unwrap_or_default();
        assert!(app.check_wait.is_none(), "该放弃等了");
        assert!(s.contains("daemon"), "要指向 daemon 而不是配置:{s}");
    }

    /// **自定义接管了出站策略时,摘要行必须说出来。**
    ///
    /// `[o]` 与自定义配置写的是同一个字段。自定义写了之后策略让位
    /// (`outbound::apply`),而界面上还显示着「仅 IPv4」就是个谎 ——
    /// 那种「界面说一套、实际跑一套」是最难查的不一致。
    #[tokio::test]
    async fn the_ops_line_says_when_a_custom_config_takes_over_the_strategy() {
        let line = |custom: Option<&str>| {
            let mut app = app();
            app.page = Page::Agents;
            app.agents =
                vec![data::AgentRow { custom_json: custom.map(str::to_string), ..stub_agent(1) }];
            app.ops_lines().join("\n")
        };

        assert!(!line(None).contains("自定义"), "没自定义就不该提它");

        // 有自定义但没接管 resolver。
        let out = line(Some(r#"{ "outbounds": [{ "type": "direct", "tag": "warp" }] }"#));
        assert!(out.contains("自定义: 有"), "{out}");
        assert!(!out.contains("接管"), "没写 resolver 就不算接管:{out}");

        // 写了 resolver → 接管。
        let out = line(Some(
            r#"{ "route": { "default_domain_resolver": { "server": "x", "strategy": "prefer_ipv6" } } }"#,
        ));
        assert!(out.contains("由自定义配置接管"), "{out}");

        // 库里那份读不懂时仍然要说「有自定义」—— 存在本身就是人要知道的事。
        let out = line(Some("{ 这不是 json"));
        assert!(out.contains("自定义: 有"), "读不懂也该提一句:{out}");
    }

    /// **人自己指定的编辑器优先,而且不要多一步回车。**
    ///
    /// 自己 `export EDITOR=vim` 的人**不需要**被教怎么退 vim —— 那一步回车
    /// 只是碍事。提示只给被自动挑中的人。
    ///
    /// 走纯函数 `pick_editor_from` 而不是改进程环境变量:`cargo test` 默认多线程
    /// 同进程,而这个仓库里有不少会 `sh -c` 出去的测试。
    #[test]
    fn an_explicit_editor_wins_and_needs_no_hand_holding() {
        let none = |_: &str| false;
        let all = |_: &str| true;

        let c = pick_editor_from(Some("my-editor --flag"), none, 1).unwrap();
        assert_eq!(c.cmd, "my-editor --flag", "带参数的 EDITOR 要原样拿着");
        assert!(!c.needs_exit_hint);

        // 显式指定 vim 也不给提示。
        let c = pick_editor_from(Some("vim"), all, 1).unwrap();
        assert_eq!(c.cmd, "vim");
        assert!(!c.needs_exit_hint, "自己指定的 vim 不应该被教怎么退");

        // 前后空白得去掉 —— `export EDITOR="nano "` 带着尾空格很常见。
        assert_eq!(pick_editor_from(Some("  nano  "), none, 1).unwrap().cmd, "nano");

        // 空串与纯空白当没设(`EDITOR=` 写在 profile 里很常见)—— 否则会变成
        // 「起不来 」这种看不懂的错。落到探测,而探测说啥都有 → 挑第一个候选。
        for blank in [Some(""), Some("   "), None] {
            assert_eq!(
                pick_editor_from(blank, all, 1).unwrap().cmd,
                EDITOR_CANDIDATES[0],
                "{blank:?} 该当没设"
            );
        }
    }

    /// 自动挑中 vi 族时要标上「得先告诉人怎么退」。
    #[test]
    fn an_auto_picked_vi_asks_for_a_heads_up_but_nano_does_not() {
        // 只有 vi 的机器(Alpine 的典型情况:busybox 带 vi,没 nano)。
        let c = pick_editor_from(None, |c| c == "vi", 1).unwrap();
        assert_eq!(c.cmd, "vi");
        assert!(c.needs_exit_hint, "自动挑中的 vi 必须先把退出方法说清");

        // 装了 nano 的机器(Debian/Ubuntu 默认)—— nano 把按键写在屏幕底下。
        let c = pick_editor_from(None, |c| c == "nano" || c == "vi", 1).unwrap();
        assert_eq!(c.cmd, "nano", "两个都有时该挑 nano");
        assert!(!c.needs_exit_hint);
    }

    /// **vi 族的识别要能穿过路径和参数。**
    ///
    /// 认错的代价不对称:把 `nano` 误判成 vi 族只是多一屏提示;把 `vi` 漏判成
    /// 非 vi 族,就是把一个不会用它的人直接扒进退不出来的界面 ——
    /// 而那时候 TUI 已经挂起,他连回去的路都没有。
    #[test]
    fn the_vi_family_is_recognised_through_paths_and_flags() {
        for yes in ["vi", "vim", "nvim", "/usr/bin/vim", "vim -u NONE", "view", "vimdiff"] {
            assert!(is_vi_family(yes), "{yes} 该算 vi 族");
        }
        for no in ["nano", "micro", "/usr/bin/nano", "code --wait", "emacs", "mcedit", ""] {
            assert!(!is_vi_family(no), "{no} 不该算 vi 族");
        }

        // **包装形式认不出来,而这不要紧。**
        //
        // 看的是第一个词,所以 `busybox vi` 里取到的是 busybox。一开始这条写的是
        // 断言它该被识别,跑了才发现想错了 —— 两条进入这个函数的路都不会构成风险:
        //
        //   * 自动探测传进来的只有 EDITOR_CANDIDATES 里的裸名字。Alpine 上 `vi` 是
        //     指向 busybox 的符链接,`command -v vi` 找到的就是 `vi` 这个词 —— 认得出来。
        //   * 自己 `export EDITOR="busybox vi"` 的人本来就不走提示这条路。
        //
        // 所以这里钉的是**真实契约**而不是我当时以为的那个。哪天把包装形式也开成
        // 自动候选(比如直接探测 busybox),这条会提醒同时把判据改成扫全部词。
        assert!(!is_vi_family("busybox vi"), "看的是第一个词");
        assert!(!is_vi_family("busybox"), "busybox 本身不是编辑器");
        assert!(
            !EDITOR_CANDIDATES.iter().any(|c| c.contains(' ')),
            "候选里出现了带参数的形式 —— is_vi_family 的判据得跟着改成扫全部词"
        );
    }

    /// 候选顺序的依据是「新手能不能自己退出来」,不是好用程度。
    /// nano / micro 把按键写在屏幕底下,所以它们得在 vi 族前面。
    #[test]
    fn the_candidate_order_puts_self_explanatory_editors_first() {
        let pos = |c: &str| EDITOR_CANDIDATES.iter().position(|x| *x == c);
        let (nano, micro) = (pos("nano").expect("nano 该在候选里"), pos("micro").unwrap());
        for vi in ["nvim", "vim", "vi"] {
            let p = pos(vi).unwrap_or_else(|| panic!("{vi} 该在候选里"));
            assert!(nano < p && micro < p, "{vi} 排在了 nano/micro 前面");
        }
        // 每个候选都得能被 is_vi_family 正确分类 —— 否则提示会发错对象。
        assert!(!is_vi_family("nano") && !is_vi_family("micro"));
        assert!(is_vi_family("vi") && is_vi_family("vim") && is_vi_family("nvim"));
    }

    /// **一个编辑器都没有时,三条出路都要给。**
    ///
    /// 尤其是不能再说「重进这一页」—— 那是第一版真实发出去过的提示,而它是错的:
    /// 环境变量读的是本进程的,外面改不了一个已经在跑的 TUI。照那句做一定失败,
    /// 而人会得出「这功能坏的」这个结论。
    #[test]
    fn with_no_editor_at_all_the_error_offers_three_ways_out() {
        let e = pick_editor_from(None, |_| false, 5).unwrap_err().to_string();
        assert!(e.contains("nano"), "该告诉人装一个:{e}");
        assert!(e.contains("agent-config-set 5"), "该给不用编辑器的那条路,带上 id:{e}");
        assert!(e.contains("sbx tui"), "该说清要重进整个 TUI:{e}");
        assert!(!e.contains("重进这一页"), "这句是错的 —— 环境变量改不了已在跑的进程:{e}");
    }

    /// **模板里当前生效的配置只能是注释。**
    ///
    /// 预填成实际内容的话,人「打开看一眼、按保存」就意外接管了
    /// `default_domain_resolver`,于是 `[o]` 静默失效。原样保存必须等于什么都没变。
    #[tokio::test]
    async fn the_editor_template_seeds_nothing_but_comments() {
        let path = std::env::temp_dir().join(format!("sbx-tpl-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();
        let (id, _) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();
        crate::db::agent_repo::set_outbound_strategy(
            &pool,
            id,
            crate::model::outbound::OutboundStrategy::Ipv4Only,
        )
        .await
        .unwrap();

        let tpl = custom_config_template(&pool, id, "tokyo").await.unwrap();
        // 参考信息得在 —— 否则人对着空文件不知道现在跑的是什么。
        assert!(tpl.contains("default_domain_resolver"), "当前生效的配置没写进参考里");
        assert!(tpl.contains("ipv4_only"));
        assert!(tpl.contains("inbounds"), "要说清 inbounds 为何不在内");

        // 而它们只能是注释:原样存盘 → 解析出来是空对象。
        let obj = crate::service::validate_custom(&tpl).expect("模板本身必须能过校验");
        assert!(obj.is_empty(), "模板不该带实际内容,否则「打开就保存」会意外接管:{obj:?}");
    }

    /// **模板里 /* */ 包着的那个示例必须本身就能存。**
    ///
    /// 它是给人删掉注释直接用的 —— 一旦示例本身过不了校验(比如 cache_file
    /// 忘了写 path),第一个照着做的人就会在存盘时撞上一句跟示例矛盾的报错,
    /// 那比没有示例更让人迷糊。模板改了什么,这条部得跟着重跑一遍。
    #[test]
    fn the_template_example_is_savable_as_is() {
        // 常量自带 /* */ 包裹(模板靠它当注释);验的是删掉包裹后的内容 ——
        // 那才是人删掉两行注释后要存的东西。
        let inner = CUSTOM_EXAMPLE
            .strip_prefix("/*")
            .and_then(|s| s.strip_suffix("*/"))
            .expect("示例常量必须由 /* */ 包着");
        let obj = crate::service::validate_custom(inner.trim())
            .expect("模板示例删掉 /* */ 后必须能直接存");
        assert!(!obj.is_empty(), "示例得有实际内容,不然演示不了任何东西");
        assert!(obj.get("experimental").is_some(), "示例要演示 cache_file 的正确写法");
    }

    #[tokio::test]
    async fn the_config_view_is_a_runnable_original() {
        use crate::model::node::{NodeParams, Protocol};
        use crate::model::outbound::OutboundStrategy;

        let path = std::env::temp_dir().join(format!("sbx-cfgview-{}.db", uuid::Uuid::new_v4()));
        let pool = crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap();
        let (agent_id, _) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        let mut params = NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
        let priv_key = params.private_key.clone().expect("reality 该有私钥");
        let (node_id, _) = crate::db::node_repo::add_node(
            &pool,
            agent_id,
            "in-1",
            Protocol::VlessReality,
            8443,
            &params,
        )
        .await
        .unwrap();
        let uid = crate::db::node_repo::add_user(&pool, "alice", 0, 0).await.unwrap();
        crate::db::node_repo::assign_node(&pool, uid, node_id).await.unwrap();
        // 出站策略设成非 Auto,这样配置里才会出现 dns / route。
        crate::db::agent_repo::set_outbound_strategy(&pool, agent_id, OutboundStrategy::Ipv4Only)
            .await
            .unwrap();

        let mut app = App::new(pool, Config::default(), "sbx-test.toml".into());
        app.refresh().await.unwrap();
        perform_inner(&mut app, &Action::ShowAgentConfig { id: agent_id, name: "tokyo".into() })
            .await
            .unwrap();

        let text = match &app.overlay.as_ref().expect("该打开配置页").body {
            OverlayBody::Text { lines, .. } => lines.join("\n"),
            _ => panic!("配置页该是文本,不是表格"),
        };

        for must in ["\"inbounds\"", "\"outbounds\"", "\"log\""] {
            assert!(text.contains(must), "配置缺 {must}:\n{text}");
        }
        // 出站策略那一套:server tag 指不到的话 sing-box 起不来。
        assert!(text.contains("\"dns\""), "改过出站策略就该有 dns:\n{text}");
        assert!(text.contains("default_domain_resolver"), "缺 route 解析器:\n{text}");
        assert!(text.contains("ipv4_only"), "策略值没写进去:\n{text}");
        // 凭据原文 —— 这一条是刻意的,不是漏了脱敏。
        assert!(text.contains(&priv_key), "reality 私钥该是原文,遮掉就跑不起来了");
        assert!(!text.contains("***"), "这一页不做脱敏");
        // 顺带确认它真是能解析回来的 JSON,而不是被截断过的片段。
        serde_json::from_str::<serde_json::Value>(&text).expect("显示出来的该是合法 JSON 全文");
    }

    /// **配置页开着的时候,方向键归滚动,不能关页面。**
    ///
    /// 明细表是「任意键关掉」—— 一屏放得下,没什么好翻的。配置页照抄那条规则的话:
    /// 想往下看一眼就把页面关了,而人会以为是崩了,不会想到「原来 ↓ 是关闭键」。
    #[tokio::test]
    async fn the_config_page_scrolls_instead_of_closing_on_arrow_keys() {
        let mut a = app();
        a.term_h = 24; // 减掉边框/标题/空行,一屏 20 行
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        a.overlay = Some(Overlay {
            title: "t".into(),
            head: "h".into(),
            info: vec![],
            body: OverlayBody::Text { lines, scroll: 0 },
        });

        let top = |a: &App| match &a.overlay.as_ref().expect("页面不该被关掉").body {
            OverlayBody::Text { scroll, .. } => *scroll,
            _ => panic!("该是文本页"),
        };

        on_key(&mut a, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(top(&a), 1, "↓ 该滚一行,而不是把页面关掉");
        on_key(&mut a, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(top(&a), 20, "PgDn 该翻一屏(20 行)并留一行重叠");

        // 滚到底:最后一行贴着底边,再按也不该继续走 ——
        // 越过去就是整屏空白,而那时人会以为内容没了。
        on_key(&mut a, key('G'));
        assert_eq!(top(&a), 80, "到底时最后一行该贴着底边");
        on_key(&mut a, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(top(&a), 80, "已经到底了就别再动");
        on_key(&mut a, key('g'));
        assert_eq!(top(&a), 0, "g 回到开头");
        on_key(&mut a, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(top(&a), 0, "开头之上没有内容,不该滚成负的");

        on_key(&mut a, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(a.overlay.is_none(), "Esc 该关掉配置页");
    }

    /// **翻页的步长要跟着终端高度走。**
    ///
    /// 写死 20 行的话,在一个 60 行高的终端上按 PgDn 只走三分之一屏 ——
    /// 更糟的是**滚动上界**也按 20 算,于是滚到底之后底下还压着 30 行看不到,
    /// 而界面上没有任何迹象说明还有内容。
    #[tokio::test]
    async fn the_page_step_follows_the_terminal_height() {
        let tall = |h: u16| {
            let mut a = app();
            a.term_h = h;
            a.overlay = Some(Overlay {
                title: "t".into(),
                head: "h".into(),
                info: vec![],
                body: OverlayBody::Text {
                    lines: (0..100).map(|i| format!("line {i}")).collect(),
                    scroll: 0,
                },
            });
            on_key(&mut a, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
            match &a.overlay.as_ref().unwrap().body {
                OverlayBody::Text { scroll, .. } => *scroll,
                _ => unreachable!(),
            }
        };
        // 高终端一屏看得多,滚到底时的首行就更靠前 —— 100 行减去一屏。
        assert_eq!(tall(24), 80, "24 行高:一屏 20 行");
        assert_eq!(tall(64), 40, "64 行高:一屏 60 行,滚到底该停在第 40 行");

        // 还没画过第一帧时(测试里构造的 App、或第一次按键早于第一帧)
        // 退回 20 行:宁可少翻也不能多翻,多翻会跳过没看过的内容。
        assert_eq!(app().overlay_view_h(), 20, "没有高度信息时该退回保守值");
    }
}
