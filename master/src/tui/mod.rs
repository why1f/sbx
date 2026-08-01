//! TUI 主循环与应用状态(DESIGN.md §8)。
//!
//! 三个页面:服务管理(agents,两行式)、节点、用户。
//!
//! **它是一个独立进程,不是 daemon 的一部分。** `sbx tui` 与 `sbx daemon` 各跑各的,
//! 之间只通过 SQLite 交换状态。这带来一个直接后果:TUI 看不到 daemon 内存里的
//! 网速采样,所以它自己按刷新节奏从 `agent_nic_traffic` 做差(见 `data::SpeedTracker`)。
//!
//! 另一个后果是 TUI **只能改库,不能直接推送**:改完之后 revision 会推进,
//! 在线 agent 由 daemon 在下次握手或下发时同步(§4.1)。所以每个写操作之后
//! 状态栏都会说明「什么时候生效」,免得人对着「改了但没变」发懵。

mod data;
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
use data::SpeedTracker;
use modal::{Action, Field, Modal};

const PAGES: [&str; 3] = ["服务管理", "节点", "用户"];
/// 刷新间隔。1 秒足够跟上 30 秒一次的上报,又不会让 SQLite 被空转拖累。
const TICK: Duration = Duration::from_millis(1000);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Agents = 0,
    Nodes = 1,
    Users = 2,
}

struct App {
    pool: SqlitePool,
    cfg: Config,
    page: Page,
    sel: [usize; 3],
    agents: Vec<data::AgentRow>,
    nodes: Vec<data::NodeRow>,
    users: Vec<data::UserRow>,
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
            page: Page::Agents,
            sel: [0; 3],
            agents: Vec::new(),
            nodes: Vec::new(),
            users: Vec::new(),
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
        let common = "[Tab]切页  [↑↓/jk]选择  [q]退出";
        match self.page {
            Page::Agents => {
                let sel = match self.selected_agent() {
                    Some(a) => format!(
                        "  │  #{} {} · token {}… · {} 个节点",
                        a.id, a.name, a.token_prefix, a.node_count
                    ),
                    None => "  │  还没有被控服务器,按 [a] 加一台".into(),
                };
                format!("{common}  [a]新增  [r]轮换token  [d]删除{sel}")
            }
            Page::Nodes => {
                let sel = match self.selected_node() {
                    Some(n) => format!(
                        "  │  #{} {} · agent #{} · {} 个用户",
                        n.id, n.tag, n.agent_id, n.user_count
                    ),
                    None => "  │  还没有节点,按 [a] 建一个".into(),
                };
                format!("{common}  [a]新增  [d]删除{sel}")
            }
            Page::Users => {
                let sel = match self.selected_user() {
                    Some(u) => format!("  │  #{} {} · {} 个节点", u.id, u.name, u.node_count),
                    None => "  │  还没有用户,按 [a] 建一个".into(),
                };
                format!("{common}  [a]新增  [e]启用  [x]停用  [n]分配节点  [s]订阅  [d]删除{sel}")
            }
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        self.agents = data::load_agents(&self.pool, &mut self.speed).await?;
        self.nodes = data::load_nodes(&self.pool).await?;
        self.users = data::load_users(&self.pool).await?;
        // 删掉最后一行之后光标会落在表外,下一帧渲染就会读到不存在的下标。
        for (i, len) in [self.agents.len(), self.nodes.len(), self.users.len()].iter().enumerate() {
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
    match app.page {
        Page::Agents => pages::agents(f, chunks[1], &app.agents, app.sel[0]),
        Page::Nodes => pages::nodes(f, chunks[1], &app.nodes, app.sel[1]),
        Page::Users => pages::users(f, chunks[1], &app.users, app.sel[2]),
    }
    modal::status_bar(f, chunks[2], &app.status_line(), app.status_is_error);

    if let Some(m) = &app.modal {
        modal::render(f, f.area(), m);
    }
}

/// 处理一次按键。返回 `Some(action)` 表示要执行一个写操作。
fn on_key(app: &mut App, k: KeyEvent) -> Option<Action> {
    // 弹窗打开时吃掉全部按键 —— 否则「输入名字」会顺带触发页面快捷键。
    if app.modal.is_some() {
        return modal_key(app, k);
    }
    // 上一次操作的回执只留到下一次按键为止,之后让位给常驻的快捷键提示。
    app.status = None;

    match k.code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            app.page = match app.page {
                Page::Agents => Page::Nodes,
                Page::Nodes => Page::Users,
                Page::Users => Page::Agents,
            };
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
            app.page = match app.page {
                Page::Agents => Page::Users,
                Page::Nodes => Page::Agents,
                Page::Users => Page::Nodes,
            };
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
        Page::Agents => match k.code {
            KeyCode::Char('a') => {
                app.modal = Some(Modal::input(
                    "新增被控服务器",
                    vec![Field::new("名称", "例如 tokyo-1;只是给人看的标识")],
                    |f| {
                        let name = f[0].value.trim().to_string();
                        if name.is_empty() {
                            return Err("名称不能为空".into());
                        }
                        Ok(Action::AddAgent { name })
                    },
                ));
            }
            KeyCode::Char('r') => {
                if let Some(a) = app.selected_agent() {
                    let (id, name) = (a.id, a.name.clone());
                    app.modal = Some(Modal::confirm(
                        "轮换 token",
                        vec![
                            format!("将为 {name} 生成新 token,旧的立即失效。"),
                            "已建立的连接不会被立刻踢掉,下次重连时才生效(§8.1)。".into(),
                            "新 token 只会显示这一次。".into(),
                        ],
                        Action::RotateToken { id, name },
                    ));
                } else {
                    app.fail("没有选中任何 agent");
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
                    app.fail("没有选中任何 agent");
                }
            }
            _ => {}
        },

        Page::Nodes => match k.code {
            KeyCode::Char('a') => {
                let agent_id = app.selected_agent().map(|a| a.id).or_else(|| app.agents.first().map(|a| a.id));
                let Some(agent_id) = agent_id else {
                    app.fail("先在「服务管理」页加一台 agent");
                    return None;
                };
                app.modal = Some(Modal::input(
                    &format!("在 agent #{agent_id} 上新增节点"),
                    vec![
                        Field::with("agent_id", &agent_id.to_string(), "在哪台被控服务器上建"),
                        Field::new("tag", "同一 agent 内唯一;也是流量记账的一半(§7.1)"),
                        Field::with("端口", "443", "监听端口"),
                        Field::with("协议", "vless-reality", "八选一,见 sbx node-add --help"),
                    ],
                    |f| {
                        let agent_id: i64 = f[0].value.trim().parse().map_err(|_| "agent_id 不是数字")?;
                        let tag = f[1].value.trim().to_string();
                        if tag.is_empty() {
                            return Err("tag 不能为空".into());
                        }
                        Ok(Action::AddNode {
                            agent_id,
                            tag,
                            port: f[2].value.trim().into(),
                            protocol: f[3].value.trim().into(),
                        })
                    },
                ));
            }
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
            KeyCode::Char('a') => {
                app.modal = Some(Modal::input(
                    "新增用户",
                    vec![
                        Field::new("名称", "唯一;也是 sing-box inbound 里的用户名"),
                        Field::with("配额 GB", "0", "0 = 不限流量"),
                    ],
                    |f| {
                        let name = f[0].value.trim().to_string();
                        if name.is_empty() {
                            return Err("名称不能为空".into());
                        }
                        Ok(Action::AddUser { name, quota_gb: f[1].value.trim().into() })
                    },
                ));
            }
            KeyCode::Char('e') => {
                if let Some(u) = app.selected_user() {
                    return Some(Action::SetUserEnabled { name: u.name.clone(), enabled: true });
                }
                app.fail("没有选中任何用户");
            }
            KeyCode::Char('x') => {
                if let Some(u) = app.selected_user() {
                    return Some(Action::SetUserEnabled { name: u.name.clone(), enabled: false });
                }
                app.fail("没有选中任何用户");
            }
            KeyCode::Char('n') => {
                let node_id = app.selected_node().map(|n| n.id).or_else(|| app.nodes.first().map(|n| n.id));
                let Some(node_id) = node_id else {
                    app.fail("还没有节点可分配");
                    return None;
                };
                let Some(u) = app.selected_user() else {
                    app.fail("没有选中任何用户");
                    return None;
                };
                let user = u.name.clone();
                app.modal = Some(Modal::input(
                    &format!("把节点分配给 {user}"),
                    vec![Field::with("node_id", &node_id.to_string(), "在「节点」页可以看到编号")],
                    |f| {
                        let node_id: i64 = f[0].value.trim().parse().map_err(|_| "node_id 不是数字")?;
                        // 用户名塞在 label 里传不过来,所以这里先占位,
                        // 由调用方在 modal_key 里补上(见那里的注释)。
                        Ok(Action::AssignNode { user: String::new(), node_id })
                    },
                ));
            }
            KeyCode::Char('s') => {
                if let Some(u) = app.selected_user() {
                    let base = app.cfg.subscription.public_base.trim_end_matches('/').to_string();
                    let url = if base.is_empty() {
                        format!("/sub/{}(未配置 public_base,只能给出路径)", u.sub_token)
                    } else {
                        format!("{base}/sub/{}", u.sub_token)
                    };
                    app.modal = Some(Modal::info(
                        &format!("{} 的订阅", u.name),
                        vec![
                            url,
                            String::new(),
                            "Clash / Mihomo:地址后加 ?type=clash".into(),
                            "(客户端 UA 也会被自动识别)".into(),
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

/// 弹窗打开时的按键。
fn modal_key(app: &mut App, k: KeyEvent) -> Option<Action> {
    let selected_user = app.selected_user().map(|u| u.name.clone());
    let modal = app.modal.as_mut()?;

    match modal {
        Modal::Info { .. } => {
            app.modal = None;
            None
        }

        Modal::Confirm { action, .. } => match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let a = action.clone();
                app.modal = None;
                Some(a)
            }
            _ => {
                app.modal = None;
                app.note("已取消");
                None
            }
        },

        Modal::Input { fields, focus, build, error, .. } => match k.code {
            KeyCode::Esc => {
                app.modal = None;
                app.note("已取消");
                None
            }
            KeyCode::Tab | KeyCode::Down => {
                *focus = (*focus + 1) % fields.len();
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                *focus = if *focus == 0 { fields.len() - 1 } else { *focus - 1 };
                None
            }
            KeyCode::Backspace => {
                fields[*focus].value.pop();
                None
            }
            KeyCode::Char(c) => {
                fields[*focus].value.push(c);
                None
            }
            KeyCode::Enter => match build(fields) {
                Ok(mut action) => {
                    // AssignNode 的用户名来自当前选中行,表单里没有这个字段。
                    if let Action::AssignNode { user, .. } = &mut action {
                        match selected_user {
                            Some(n) => *user = n,
                            None => {
                                *error = Some("没有选中任何用户".into());
                                return None;
                            }
                        }
                    }
                    app.modal = None;
                    Some(action)
                }
                Err(msg) => {
                    *error = Some(msg);
                    None
                }
            },
            _ => None,
        },
    }
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
        Action::AddAgent { name } => {
            let (id, token) = agent_repo::create(&app.pool, name, now).await?;
            let fp = if app.cfg.cluster.tls {
                crate::tls::fingerprint(&app.cfg.cluster.cert_path)
                    .unwrap_or_else(|_| "<还没生成证书,启动一次 daemon 就有了>".into())
            } else {
                "<cluster.tls = false,agent 侧要写 insecure = true>".into()
            };
            let scheme = if app.cfg.cluster.tls { "wss" } else { "ws" };
            // token 明文只在这里出现这一次。库里只有 hash 与前 8 位(§8.1)。
            app.modal = Some(Modal::info(
                &format!("agent #{id} {name} 的接入信息"),
                vec![
                    "把下面几行写进被控机的 /etc/sbx/agent.toml:".into(),
                    String::new(),
                    format!("server      = \"{scheme}://<主控地址>:{}/ws\"", port_of(&app.cfg.cluster.listen)),
                    format!("token       = \"{token}\""),
                    format!("fingerprint = \"{fp}\""),
                    "state_dir   = \"/var/lib/sbx-agent\"".into(),
                    String::new(),
                    "⚠ token 明文只显示这一次,关掉就再也拿不回来了。".into(),
                    "  丢了就用 [r] 轮换一个新的。".into(),
                ],
            ));
            Ok(format!("已新增 agent #{id} {name}"))
        }

        Action::RotateToken { id, name } => {
            let token = agent_repo::rotate_token(&app.pool, *id).await?;
            app.modal = Some(Modal::info(
                &format!("{name} 的新 token"),
                vec![
                    format!("token = \"{token}\""),
                    String::new(),
                    "旧 token 已失效。在线连接不会被立刻踢掉,下次重连时生效。".into(),
                    "⚠ 同样只显示这一次。".into(),
                ],
            ));
            Ok(format!("已轮换 {name} 的 token"))
        }

        Action::DeleteAgent { id, name } => {
            agent_repo::delete(&app.pool, *id).await?;
            Ok(format!("已删除 agent {name} 及其节点"))
        }

        Action::AddNode { agent_id, tag, port, protocol } => {
            let port: u16 = port.parse().map_err(|_| anyhow::anyhow!("端口 {port} 不是 1..65535"))?;
            let proto = crate::model::node::Protocol::parse(protocol);
            if matches!(proto, crate::model::node::Protocol::Unknown) {
                anyhow::bail!(
                    "无法识别的协议 {protocol};可选:{}",
                    crate::model::node::Protocol::all()
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                );
            }
            let mut params = crate::model::node::NodeParams::default();
            // 与 CLI 走同一条路:密钥材料在**建节点时**生成一次(§9.1)。
            crate::secrets::fill(proto, &mut params)?;
            let (id, rev) =
                node_repo::add_node(&app.pool, *agent_id, tag, proto, port, &params).await?;
            Ok(format!("已新增节点 #{id} {tag}(agent #{agent_id} 的 config_revision → {rev})"))
        }

        Action::DeleteNode { id, tag } => {
            let (agent_id, rev) = node_repo::delete_node(&app.pool, *id).await?;
            Ok(format!("已删除节点 {tag}(agent #{agent_id} → rev {rev})"))
        }

        Action::AddUser { name, quota_gb } => {
            let gb: f64 = quota_gb.parse().map_err(|_| anyhow::anyhow!("配额 {quota_gb} 不是数字"))?;
            if gb < 0.0 {
                anyhow::bail!("配额不能是负数");
            }
            let quota = (gb * 1_073_741_824.0) as i64;
            let id = node_repo::add_user(&app.pool, name, quota, now).await?;
            Ok(format!("已新增用户 #{id} {name};按 [n] 给它分配节点"))
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

        Action::AssignNode { user, node_id } => {
            let u = node_repo::get_user_by_name(&app.pool, user)
                .await?
                .ok_or_else(|| anyhow::anyhow!("没有名为 {user} 的用户"))?;
            let (agent_id, rev) = node_repo::assign_node(&app.pool, u.id, *node_id).await?;
            Ok(format!("已把节点 #{node_id} 分配给 {user}(agent #{agent_id} → rev {rev})"))
        }
    }
}

/// 从 `0.0.0.0:18443` 里取出端口。取不到就用默认值 —— 这个字符串只是拼给人看的
/// 提示,解析失败不该让「新增 agent」整个失败。
fn port_of(listen: &str) -> String {
    listen.rsplit_once(':').map(|(_, p)| p.to_string()).unwrap_or_else(|| "18443".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_of_handles_ipv4_ipv6_and_garbage() {
        assert_eq!(port_of("0.0.0.0:18443"), "18443");
        assert_eq!(port_of("[::]:9443"), "9443");
        assert_eq!(port_of("nonsense"), "18443");
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
    async fn tab_cycles_through_all_three_pages() {
        let mut a = app();
        assert!(a.page == Page::Agents);
        on_key(&mut a, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(a.page == Page::Nodes);
        on_key(&mut a, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(a.page == Page::Users);
        on_key(&mut a, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(a.page == Page::Agents, "应当回到第一页");
    }

    /// 空列表时按上下键不能 panic,也不能把光标挪到不存在的行。
    #[tokio::test]
    async fn navigation_on_an_empty_list_is_safe() {
        let mut a = app();
        on_key(&mut a, key('j'));
        on_key(&mut a, key('k'));
        assert_eq!(a.sel[0], 0);
    }

    #[tokio::test]
    async fn navigation_wraps_around() {
        let mut a = app();
        a.agents = (0..3).map(stub_agent).collect();
        on_key(&mut a, key('k'));
        assert_eq!(a.sel[0], 2, "从第一行往上应当绕到最后一行");
        on_key(&mut a, key('j'));
        assert_eq!(a.sel[0], 0);
    }

    /// 弹窗打开时,页面快捷键必须被吃掉 ——
    /// 否则在输入框里打一个 'q' 会直接退出程序。
    #[tokio::test]
    async fn modal_swallows_page_shortcuts() {
        let mut a = app();
        a.modal = Some(Modal::input("t", vec![Field::new("名称", "")], |_| {
            Err("never".into())
        }));
        on_key(&mut a, key('q'));
        assert!(!a.quit, "'q' 应当被当成输入,不是退出");
        let Some(Modal::Input { fields, .. }) = &a.modal else { panic!() };
        assert_eq!(fields[0].value, "q");
    }

    #[tokio::test]
    async fn input_validation_keeps_the_modal_open() {
        let mut a = app();
        a.modal = Some(Modal::input("t", vec![Field::new("名称", "")], |f| {
            if f[0].value.trim().is_empty() {
                Err("名称不能为空".into())
            } else {
                Ok(Action::AddAgent { name: f[0].value.clone() })
            }
        }));
        // 空名字提交:弹窗留着,错误显示出来。
        let act = on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(act.is_none());
        let Some(Modal::Input { error, .. }) = &a.modal else { panic!("弹窗不该关掉") };
        assert_eq!(error.as_deref(), Some("名称不能为空"));

        // 填上之后再提交就通过了。
        on_key(&mut a, key('x'));
        let act = on_key(&mut a, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Some(Action::AddAgent { .. })));
        assert!(a.modal.is_none());
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

    #[tokio::test]
    async fn backspace_edits_the_focused_field_only() {
        let mut a = app();
        a.modal = Some(Modal::input(
            "t",
            vec![Field::with("a", "aa", ""), Field::with("b", "bb", "")],
            |_| Err("x".into()),
        ));
        on_key(&mut a, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        on_key(&mut a, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        let Some(Modal::Input { fields, .. }) = &a.modal else { panic!() };
        assert_eq!(fields[0].value, "aa", "没聚焦的字段不该被改");
        assert_eq!(fields[1].value, "b");
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

        assert_eq!(app.users.len(), 1);
        assert_eq!(app.users[0].name, "alice");
        assert_eq!(app.users[0].node_count, 1);

        // 三个页面都要能画出来。ratatui 越界写入是直接 panic 的,
        // 所以「画得出来」本身就是一条有效断言。
        let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
        for page in [Page::Agents, Page::Nodes, Page::Users] {
            app.page = page;
            term.draw(|f| draw(f, &app)).unwrap();
        }
        // 状态栏摘要要认出选中项(token 前缀是日志对号用的,§8.1)。
        app.page = Page::Agents;
        assert!(app.status_line().contains("tokyo-1"), "{}", app.status_line());
        assert!(app.status_line().contains("token "), "{}", app.status_line());
    }
}
