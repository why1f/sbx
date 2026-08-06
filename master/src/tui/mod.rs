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
//! 在线 agent 由 daemon 在下次握手或下发时同步(§4.1)。所以每个写操作之后
//! 状态栏都会说明「什么时候生效」,免得人对着「改了但没变」发懵。

mod clip;
mod data;
mod forms;
mod modal;
mod pages;
mod settings;
mod theme;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
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
    /// 一次性消息(某个操作的结果)。为 `None` 时状态栏显示当前页的快捷键
    /// 与选中项摘要 —— 那是**常驻**信息,不该被上一次操作的回执长期占着。
    status: Option<String>,
    status_is_error: bool,
    quit: bool,
}

impl App {
    fn new(pool: SqlitePool, cfg: Config, cfg_path: String) -> Self {
        Self {
            pool,
            cfg,
            cfg_path,
            page: Page::Dashboard,
            sel: [0; 5],
            agents: Vec::new(),
            nodes: Vec::new(),
            users: Vec::new(),
            speed: SpeedTracker::default(),
            modal: None,
            overlay: None,
            probed_ip: std::sync::Arc::new(std::sync::Mutex::new(None)),
            status: None,
            status_is_error: false,
            quit: false,
        }
    }

    fn sel_mut(&mut self) -> &mut usize {
        &mut self.sel[self.page as usize]
    }

    fn len(&self) -> usize {
        match self.page {
            // 仪表盘没有可选中的行,上下键在这里就该什么都不做。
            Page::Dashboard => 0,
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
            " sbx v{}  用户:{}  节点:{}  机器:{}  [1-5/Tab]切页  [↑↓/jk]选择  [R]刷新  [q]退出",
            env!("CARGO_PKG_VERSION"),
            self.users.len(),
            self.nodes.len(),
            self.agents.len(),
        )
    }

    /// 「操作」面板第一行:**当前选中的是什么**。
    ///
    /// 与下面那行按键放在同一个框里是有意的:「[d]删除」和「选中: alice」
    /// 隔开摆的话,按下去之前得先抬头去表里找光标在哪 —— 而那正是最不该
    /// 需要确认两次的时刻。
    fn ops_summary(&self) -> String {
        match self.page {
            Page::Dashboard => "  只读页;要动手请去 [2] 服务管理 / [3] 节点 / [4] 用户".into(),
            Page::Agents => match self.selected_agent() {
                // token_prefix 有实际用途:主控日志里认证失败只记前 8 位
                // (§8.1 不回显完整 token),对不上号时靠它把日志和某一行连起来。
                Some(a) => format!(
                    "  选中: {}  token: {}…  节点: {} 个  状态: {}",
                    a.name,
                    a.token_prefix,
                    a.node_count,
                    match a.status.as_str() {
                        "online" => "● 在线",
                        "offline" => "● 离线",
                        _ => "○ 从未连接",
                    }
                ),
                None => "  (还没有被控服务器,按 [a] 加一台)".into(),
            },
            Page::Nodes => match self.selected_node() {
                Some(n) => format!(
                    "  选中: {}  机器: {}  协议: {}  端口: {}  在用: {} 人",
                    n.tag, n.agent_name, n.protocol, n.listen_port, n.user_count
                ),
                None => "  (还没有节点,按 [a] 建一个)".into(),
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
                        "(已撤销)".to_string()
                    } else {
                        let base = self.sub_base().trim().trim_end_matches('/');
                        if base.is_empty() {
                            format!("/sub/{}", u.sub_token)
                        } else {
                            format!("{base}/sub/{}", u.sub_token)
                        }
                    };
                    format!("  选中: {}  节点: {}  订阅: {}", u.name, nodes, sub)
                }
                None => "  (还没有用户,按 [a] 建一个)".into(),
            },
            Page::Settings => "  改的是配置文件本身,注释与排版都保留;改完要重启 daemon".into(),
        }
    }

    /// 「操作」面板第二行:**这一页能按的键**。
    ///
    /// 通用键(切页/选择/刷新/退出)不在这里 —— 它们在最底下那条页脚里,
    /// 只写一处。混在一起会让它们在五个页面里重复五遍,把真正会变的那部分挤走。
    fn ops_keys(&self) -> &'static str {
        match self.page {
            Page::Dashboard => "",
            Page::Agents => "  [a]新增  [E]编辑  [Enter]网卡明细  [i]接入命令  [r]轮换token  [d]删除",
            Page::Nodes => "  [a]新增  [E]编辑  [Enter]用量明细  [d]删除",
            Page::Users => {
                "  [a]新增  [E]编辑  [Enter]明细  [n]分配节点  [b]绑网卡  [T]token  [r]重置流量  [t]启/停  [s]订阅  [d]删除"
            }
            Page::Settings => "  [Enter]改这一项",
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        let now = chrono::Local::now().timestamp();
        self.agents = data::load_agents(&self.pool, &mut self.speed, now).await?;
        self.nodes = data::load_nodes(&self.pool).await?;
        self.users = data::load_users(&self.pool).await?;
        // 删掉最后一行之后光标会落在表外,下一帧渲染就会读到不存在的下标。
        let lens =
            [0, self.agents.len(), self.nodes.len(), self.users.len(), settings::all(&self.cfg).len()];
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
}

/// 一张只读的用量明细表。
struct Overlay {
    title: String,
    head: String,
    /// 表头下面的补充行。节点/用户明细不需要,留空;
    /// 网卡明细要靠它把「整机烧了多少」和下面「各节点跑了多少」并排摆出来 ——
    /// 这两个数字口径不同,分开看会得出错误结论(§6.4)。
    info: Vec<Line<'static>>,
    rows: Vec<data::BreakdownRow>,
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

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

async fn event_loop<B: ratatui::backend::Backend>(term: &mut Terminal<B>, mut app: App) -> Result<()> {
    // 输入放在单独的线程里:crossterm 的 poll/read 是阻塞的,直接在 async
    // 循环里调用会把 runtime 的工作线程按住。走 channel 之后,主循环可以用
    // select 同时等按键和刷新计时。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || loop {
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
        term.draw(|f| draw(f, &app))?;
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .split(f.area());

    modal::tabs(f, chunks[0], &PAGES, app.page as usize);
    let now = chrono::Local::now().timestamp();
    match app.page {
        Page::Dashboard => pages::dashboard(
            f,
            chunks[1],
            &app.agents,
            &app.nodes,
            &app.users,
            app.speed.history(),
            now,
        ),
        Page::Agents => pages::agents(f, chunks[1], &app.agents, app.sel[1], now),
        Page::Nodes => pages::nodes(f, chunks[1], &app.nodes, app.sel[2]),
        Page::Users => pages::users(f, chunks[1], &app.users, app.sel[3], app.sub_base(), now),
        Page::Settings => pages::settings(f, chunks[1], &settings::all(&app.cfg), app.sel[4]),
    }
    modal::ops_panel(f, chunks[2], &app.ops_summary(), app.ops_keys(), app.status.as_deref(), app.status_is_error);
    modal::info_bar(f, chunks[3], &app.header_line());

    if let Some(o) = &app.overlay {
        pages::breakdown(f, f.area(), &o.title, &o.head, &o.info, &o.rows);
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
    // 明细表是只读的,任意键关掉。放在页面快捷键**之前** ——
    // 否则在它开着的时候按 d 会去删掉后面那张表里选中的东西。
    if app.overlay.is_some() {
        app.overlay = None;
        return None;
    }
    // 上一次操作的回执只留到下一次按键为止,之后让位给常驻的快捷键提示。
    app.status = None;

    match k.code {
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
        Page::Dashboard => {}

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
            KeyCode::Char('i') => match app.selected_agent() {
                Some(a) => {
                    let (id, name) = (a.id, a.name.clone());
                    return Some(Action::ShowInstall { id, name });
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
                    return Some(Action::SetUserEnabled { name: u.name.clone(), enabled: !u.enabled })
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

    Modal::Form(
        Form::new(
            "编辑被控服务器",
            vec![
                Field::text("name", "名称 *必填", &a.name),
                Field::text("quota", "网卡月配额 GB (0 = 不限)", &quota),
                Field::text("reset", "配额重置日 (1-31,留空 = 不重置)", &reset),
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
                })
            }),
        )
        .head(format!("#{} {}(IP 由 agent 自探上报,改了会被下一次上报覆盖)", a.id, a.name))
        .with_note(Box::new(|_| {
            vec![
                "网卡配额是**机器**进出总量的口径(§6.4),不是用户计费用量。".into(),
                "它只影响界面上的进度条与告警,不会限制 agent 转发流量。".into(),
            ]
        })),
    )
}

/// 接入命令的信息框:命令自己一行,底下是说明,`y` 复制整条。
///
/// 命令**单独占一行**放在最上面,而不是混在说明里:它是这个框存在的唯一理由,
/// 而且是那条要被复制走的东西 —— 终端不支持 OSC 52 时人得用鼠标去选它。
fn install_modal(cfg: &Config, host: &str, title: &str, token: Option<&str>, tail: Vec<String>) -> Modal {
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
        Action::AddAgent { name, quota_bytes, reset_day } => {
            let (id, token) = agent_repo::create(&app.pool, name, now).await?;
            // 配额与重置日在建的时候就填进去,免得建完还要再进一次编辑框。
            if quota_bytes.is_some() || reset_day.is_some() {
                agent_repo::update_settings(&app.pool, id, name, *quota_bytes, *reset_day).await?;
            }
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

        Action::EditAgent { id, name, quota_bytes, reset_day } => {
            agent_repo::update_settings(&app.pool, *id, name, *quota_bytes, *reset_day).await?;
            Ok(format!("已保存 agent #{id} {name} 的设置(只影响主控侧的记账口径)"))
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
            let (id, rev) =
                node_repo::add_node(&app.pool, d.agent_id, &d.tag, d.protocol, d.port, &params).await?;
            Ok(format!(
                "已新增节点 #{id} {}(agent #{} 的 config_revision → {rev};在线的会重建 box)",
                d.tag, d.agent_id
            ))
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
            let (agent_id, rev) = node_repo::update_node(&app.pool, *id, draft.port, &params).await?;
            Ok(format!("已保存节点 {tag}(agent #{agent_id} 的 config_revision → {rev})"))
        }

        Action::DeleteNode { id, tag } => {
            let (agent_id, rev) = node_repo::delete_node(&app.pool, *id).await?;
            Ok(format!("已删除节点 {tag}(agent #{agent_id} → rev {rev})"))
        }

        Action::AddUser { name, quota_gb } => {
            let quota = parse_quota(quota_gb)?;
            let id = node_repo::add_user(&app.pool, name, quota, now).await?;
            Ok(format!("已新增用户 #{id} {name};按 [n] 给它分配节点,否则订阅是空的"))
        }

        Action::EditUser { id, name, quota_gb, multiplier, expire, reset_day } => {
            let quota = parse_quota(quota_gb)?;
            let mult: f64 = multiplier
                .parse()
                .map_err(|_| anyhow::anyhow!("计费倍率 {multiplier} 不是数字"))?;
            if mult < 0.0 {
                anyhow::bail!("计费倍率不能是负数");
            }
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
            app.overlay = Some(Overlay {
                title: format!("节点 {tag} 上的用户"),
                head: format!("{tag}(在 {agent} 上)· {n} 个用户"),
                info: vec![],
                rows,
            });
            Ok(String::new())
        }

        Action::ShowUserNodes { id, name } => {
            let rows = data::user_breakdown(&app.pool, *id).await?;
            let n = rows.len();
            app.overlay = Some(Overlay {
                title: format!("{name} 的节点用量"),
                head: format!("{name} · {n} 个节点"),
                info: vec![],
                rows,
            });
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
                head: format!("{name} · {n} 个节点 · 网卡按厂商口径计费"),
                info,
                rows,
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

        // 刷新本身在主循环里做(每个动作之后都会 refresh 一次),这里只给回执。
        Action::Refresh => Ok("已刷新".into()),

        Action::SetUserNodes { user_id, user, node_ids } => {
            let affected = node_repo::set_user_nodes(&app.pool, *user_id, node_ids).await?;
            if affected.is_empty() {
                return Ok(format!("{user} 的节点分配没有变化"));
            }
            let detail = affected
                .iter()
                .map(|(a, r)| format!("#{a}→rev {r}"))
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!("{user} 现在有 {} 个节点({detail})", node_ids.len()))
        }
    }
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
        assert!(body[0].starts_with("curl -fsSL "), "第一行就该是命令: {:?}", body[0]);
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
        a.overlay = Some(Overlay { title: "t".into(), head: "h".into(), info: vec![], rows: vec![] });

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
            Modal::Info { body, .. } => body.join("
"),
            _ => panic!("订阅框应当是只读信息框"),
        };

        // ① 服务关着 —— 先说这个,别的都没意义。
        let mut cfg = Config::default();
        cfg.subscription.enabled = false;
        cfg.subscription.public_base = "https://sub.example.com".into();
        let t = body(&sub_modal(&cfg, "alice", "tok"));
        assert!(t.contains("404"), "关着的时候要说清返回 404:
{t}");

        // ② 开着但没配对外地址 —— 要给出**怎么让它能被访问到**,而不只是一条路径。
        let cfg = Config::default();
        assert!(cfg.subscription.enabled);
        assert!(cfg.subscription.public_base.is_empty());
        let t = body(&sub_modal(&cfg, "alice", "tok"));
        assert!(t.contains("/sub/tok"), "路径要给:
{t}");
        assert!(t.contains("127.0.0.1:18081"), "要点出它现在只听本机:
{t}");
        assert!(t.contains("nginx"), "要给出反代这条路:
{t}");

        // ③ 配好了 —— 给完整地址,并且能一键复制。
        let mut cfg = Config::default();
        cfg.subscription.public_base = "https://sub.example.com/".into();
        let m = sub_modal(&cfg, "alice", "tok");
        let t = body(&m);
        assert!(t.contains("https://sub.example.com/sub/tok"), "尾部斜杠不该变成双斜杠:
{t}");
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
        a.sel[Page::Settings as usize] =
            items.iter().position(|s| s.key == "bot_token").unwrap();

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

        // 「操作」面板占页脚上面那 4 行,带边框和标题。
        let ops_title = row(h as u16 - 5);
        assert!(flat(&ops_title).contains("操作"), "该有一个带标题的操作面板:{ops_title:?}");
        let hint = row(h as u16 - 3);
        let fh = flat(&hint);
        // 它**不该**再重复通用键 —— 重复正是这次要拆开的那个毛病。
        assert!(!fh.contains("退出"), "操作面板不该重复通用键:{hint:?}");
        assert!(!fh.contains("退出"), "底栏不该再重复通用键:{hint:?}");

        println!("{}\n{}\n…\n{}\n{}", first.trim_end(), second.trim_end(), info.trim_end(), hint.trim_end());
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

        let mut term = Terminal::new(TestBackend::new(140, 20)).unwrap();
        term.draw(|f| draw(f, &a)).unwrap();
        let buf = term.backend().buffer().clone();
        let h = buf.area.height;
        for y in (h - 6)..h {
            let line: String =
                (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{}", line.trim_end());
        }
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
            a.modal = Some(Modal::confirm(
                "删",
                vec![],
                Action::DeleteAgent { id: 1, name: "x".into() },
            ));
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
            ipv4: None,
            ipv6: None,
            nic_quota_bytes: None,
            nic_reset_day: None,
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
        crate::db::agent_repo::log_event(&pool, Some(agent_id), "counter_reset", "计数器重置", 1000)
            .await
            .unwrap();

        let mut params = NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
        let (node_id, _) =
            crate::db::node_repo::add_node(&pool, agent_id, "tokyo-reality", Protocol::VlessReality, 8443, &params)
                .await
                .unwrap();
        let uid = crate::db::node_repo::add_user(&pool, "alice", 100 * 1_073_741_824, 0).await.unwrap();
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
        let by_node = data::node_breakdown(&app.pool, node_id).await.unwrap();
        assert_eq!(by_node.len(), 1);
        assert_eq!(by_node[0].label, "alice");
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
        assert!(app.ops_summary().contains("tokyo-1"), "{}", app.ops_summary());
        assert!(app.ops_summary().contains("token:"), "{}", app.ops_summary());
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
        assert!(msg.contains("config_revision"), "{msg}");

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
        let (node_id, _) =
            crate::db::node_repo::add_node(&pool, agent_id, "in", Protocol::VlessReality, 443, &params)
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
}
