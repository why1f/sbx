//! TUI 主循环与应用状态(DESIGN.md §8)。
//!
//! 四个页面:概览、服务管理(agents,两行式)、节点、用户。
//! 页签前面的序号就是**能直接按的键**:`1`-`4` 直达,Tab 循环。
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
mod theme;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::prelude::{CrosstermBackend, Terminal};
use sqlx::SqlitePool;
use std::time::Duration;

use crate::config::Config;
use crate::install;
use data::SpeedTracker;
use modal::{Action, Modal, Outcome};

const PAGES: [&str; 4] = ["概览", "服务管理", "节点", "用户"];
/// 刷新间隔。1 秒足够跟上 30 秒一次的上报,又不会让 SQLite 被空转拖累。
const TICK: Duration = Duration::from_millis(1000);
/// 概览页显示多少条事件。取够填满面板即可,查历史该用 `sqlite3`。
const EVENT_LIMIT: i64 = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Dashboard = 0,
    Agents = 1,
    Nodes = 2,
    Users = 3,
}

impl Page {
    fn from_index(i: usize) -> Page {
        match i {
            1 => Page::Agents,
            2 => Page::Nodes,
            3 => Page::Users,
            _ => Page::Dashboard,
        }
    }
}

struct App {
    pool: SqlitePool,
    cfg: Config,
    page: Page,
    sel: [usize; 4],
    agents: Vec<data::AgentRow>,
    nodes: Vec<data::NodeRow>,
    users: Vec<data::UserRow>,
    events: Vec<data::EventRow>,
    speed: SpeedTracker,
    modal: Option<Modal>,
    /// 一次性消息(某个操作的结果)。为 `None` 时状态栏显示当前页的快捷键
    /// 与选中项摘要 —— 那是**常驻**信息,不该被上一次操作的回执长期占着。
    status: Option<String>,
    status_is_error: bool,
    quit: bool,
}

impl App {
    fn new(pool: SqlitePool, cfg: Config) -> Self {
        Self {
            pool,
            cfg,
            page: Page::Dashboard,
            sel: [0; 4],
            agents: Vec::new(),
            nodes: Vec::new(),
            users: Vec::new(),
            events: Vec::new(),
            speed: SpeedTracker::default(),
            modal: None,
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
            // 概览页没有可选中的行,上下键在这里就该什么都不做。
            Page::Dashboard => 0,
            Page::Agents => self.agents.len(),
            Page::Nodes => self.nodes.len(),
            Page::Users => self.users.len(),
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

    /// 状态栏内容:有一次性消息就显示它,否则显示快捷键 + 选中项摘要。
    ///
    /// 摘要里带 `token_prefix` 是有实际用途的:主控日志里认证失败只会记前 8 位
    /// (§8.1 不回显完整 token),对不上号的时候要靠这里把日志和某一行连起来。
    fn status_line(&self) -> String {
        if let Some(msg) = &self.status {
            return msg.clone();
        }
        let common = "[1-4]切页  [↑↓/jk]选择  [q]退出";
        match self.page {
            Page::Dashboard => format!("{common}  │  概览是只读的;要动手请去 2/3/4 页"),
            Page::Agents => {
                let sel = match self.selected_agent() {
                    Some(a) => format!(
                        "  │  #{} {} · token {}… · {} 个节点",
                        a.id, a.name, a.token_prefix, a.node_count
                    ),
                    None => "  │  还没有被控服务器,按 [a] 加一台".into(),
                };
                format!("{common}  [a]新增  [E]编辑  [i]接入命令  [r]轮换token  [d]删除{sel}")
            }
            Page::Nodes => {
                let sel = match self.selected_node() {
                    Some(n) => format!("  │  #{} {} · {} 个用户在用", n.id, n.tag, n.user_count),
                    None => "  │  还没有节点,按 [a] 建一个".into(),
                };
                format!("{common}  [a]新增  [E]编辑  [d]删除{sel}")
            }
            Page::Users => {
                let sel = match self.selected_user() {
                    Some(u) => format!("  │  #{} {}", u.id, u.name),
                    None => "  │  还没有用户,按 [a] 建一个".into(),
                };
                format!("{common}  [a]新增  [E]编辑  [n]分配节点  [t]启/停  [s]订阅  [d]删除{sel}")
            }
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        self.agents = data::load_agents(&self.pool, &mut self.speed).await?;
        self.nodes = data::load_nodes(&self.pool).await?;
        self.users = data::load_users(&self.pool).await?;
        self.events = data::load_events(&self.pool, EVENT_LIMIT).await?;
        // 删掉最后一行之后光标会落在表外,下一帧渲染就会读到不存在的下标。
        let lens = [0, self.agents.len(), self.nodes.len(), self.users.len()];
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
}

/// 进入 TUI。返回时终端一定已经恢复原状(正常退出、出错、panic 三条路都覆盖)。
pub async fn run(pool: SqlitePool, cfg: Config) -> Result<()> {
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

    let result = event_loop(&mut term, App::new(pool, cfg)).await;

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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
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
            &app.events,
            now,
        ),
        Page::Agents => pages::agents(f, chunks[1], &app.agents, app.sel[1], now),
        Page::Nodes => pages::nodes(f, chunks[1], &app.nodes, app.sel[2]),
        Page::Users => pages::users(f, chunks[1], &app.users, app.sel[3], app.sub_base(), now),
    }
    modal::status_bar(f, chunks[2], &app.status_line(), app.status_is_error);

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
    // 上一次操作的回执只留到下一次按键为止,之后让位给常驻的快捷键提示。
    app.status = None;

    match k.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
        // 数字直达。四页里跳来跳去时,Tab 要按三下才到得了最后一页,
        // 而人心里想的是「去第 4 页」。
        //
        // 页签上印的序号是 `i + 1`,所以这里要减 1 —— 按 3 去的是第三个页签(节点),
        // 不是下标 3 的那一页。
        KeyCode::Char(c @ '1'..='4') => {
            app.page = Page::from_index(c as usize - '1' as usize);
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
        Page::Dashboard => {}

        Page::Agents => match k.code {
            KeyCode::Char('a') => {
                let host = install::default_host(&app.cfg);
                app.modal = Some(forms::agent_add(&host));
            }
            KeyCode::Char('E') => match app.selected_agent() {
                Some(a) => app.modal = Some(agent_edit(a)),
                None => app.fail("没有选中任何被控服务器"),
            },
            KeyCode::Char('i') => match app.selected_agent() {
                Some(a) => {
                    let (id, name) = (a.id, a.name.clone());
                    return Some(Action::ShowInstall { id, name, host: install::default_host(&app.cfg) });
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
                        Action::RotateToken { id, name, host: install::default_host(&app.cfg) },
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
            KeyCode::Char('s') => {
                if let Some(u) = app.selected_user() {
                    let base = app.cfg.subscription.public_base.trim_end_matches('/').to_string();
                    let url = if base.is_empty() {
                        format!("/sub/{}(配置里没填 subscription.public_base,只能给出路径)", u.sub_token)
                    } else {
                        format!("{base}/sub/{}", u.sub_token)
                    };
                    app.modal = Some(Modal::info(
                        &format!("{} 的订阅", u.name),
                        vec![
                            url,
                            String::new(),
                            "浏览器打开 → 流量统计页;客户端 UA 会被自动识别。".into(),
                            "强制格式:地址后加 ?type=clash / ?type=stats。".into(),
                        ],
                    ));
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
    }
    None
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
fn install_modal(
    cfg: &Config,
    title: &str,
    host: &str,
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
        Action::AddAgent { name, host } => {
            let (id, token) = agent_repo::create(&app.pool, name, now).await?;
            // token 明文只在这里出现这一次。库里只有 hash 与前 8 位(§8.1)。
            app.modal = Some(install_modal(
                &app.cfg,
                &format!("agent #{id} {name} —— 在被控机上跑这一条"),
                host,
                Some(&token),
                Vec::new(),
            ));
            Ok(format!("已新增 agent #{id} {name}"))
        }

        Action::EditAgent { id, name, quota_bytes, reset_day } => {
            agent_repo::update_settings(&app.pool, *id, name, *quota_bytes, *reset_day).await?;
            Ok(format!("已保存 agent #{id} {name} 的设置(只影响主控侧的记账口径)"))
        }

        Action::ShowInstall { id, name, host } => {
            app.modal = Some(install_modal(
                &app.cfg,
                &format!("agent #{id} {name} 的接入命令"),
                host,
                None,
                Vec::new(),
            ));
            Ok(String::new())
        }

        Action::RotateToken { id, name, host } => {
            let token = agent_repo::rotate_token(&app.pool, *id).await?;
            app.modal = Some(install_modal(
                &app.cfg,
                &format!("{name} 的新 token —— 在那台机器上重跑这一条"),
                host,
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
        let m = install_modal(&cfg, "t", "203.0.113.8", Some("tok"), Vec::new());
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
        a.modal = Some(install_modal(&cfg, "t", "203.0.113.8", Some("tok"), Vec::new()));
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
        let mut m = install_modal(
            &cfg,
            "agent #1 tokyo-1 —— 在被控机上跑这一条",
            "203.0.113.8",
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

    fn app() -> App {
        // 只用来测按键与选择逻辑,不碰数据库。
        let pool = SqlitePool::connect_lazy("sqlite::memory:").unwrap();
        App::new(pool, Config::default())
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[tokio::test]
    async fn tab_cycles_through_all_four_pages() {
        let mut a = app();
        assert!(a.page == Page::Dashboard);
        for want in [Page::Agents, Page::Nodes, Page::Users, Page::Dashboard] {
            on_key(&mut a, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert!(a.page == want, "Tab 顺序不对");
        }
    }

    /// 数字键直达。Tab 要按三下才到得了最后一页,而人心里想的是「去第 4 页」。
    #[tokio::test]
    async fn number_keys_jump_straight_to_a_page() {
        let mut a = app();
        for (c, want) in
            [('3', Page::Nodes), ('1', Page::Dashboard), ('4', Page::Users), ('2', Page::Agents)]
        {
            on_key(&mut a, key(c));
            assert!(a.page == want, "按 {c} 应当到对应的页");
        }
        // 5 不是页码,不该有任何反应(也不该被当成别的快捷键)。
        on_key(&mut a, key('5'));
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

        let mut app = App::new(pool, Config::default());
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

        assert_eq!(app.events.len(), 1, "事件表要读得出来(概览页要用)");
        assert_eq!(app.events[0].agent_name.as_deref(), Some("tokyo-1"));

        // 四个页面都要能画出来。ratatui 越界写入是直接 panic 的,
        // 所以「画得出来」本身就是一条有效断言。
        let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
        for page in [Page::Dashboard, Page::Agents, Page::Nodes, Page::Users] {
            app.page = page;
            term.draw(|f| draw(f, &app)).unwrap();
        }
        // 状态栏摘要要认出选中项(token 前缀是日志对号用的,§8.1)。
        app.page = Page::Agents;
        assert!(app.status_line().contains("tokyo-1"), "{}", app.status_line());
        assert!(app.status_line().contains("token "), "{}", app.status_line());
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

        let mut app = App::new(pool, Config::default());
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

        let mut app = App::new(pool, Config::default());
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
