//! TUI 的二级页面:表单、多选框、确认框、只读信息框(DESIGN.md §8.1)。
//!
//! 四种弹窗覆盖了全部需要停下来跟人确认的动作:
//!   * `Form`    —— 新增/编辑 agent、节点、用户;
//!   * `Picker`  —— 给用户勾选节点(多选);
//!   * `Confirm` —— 删除类操作,**并且要说清影响面**(删 agent 会连带删它的节点);
//!   * `Info`    —— 一键接入命令、订阅地址。
//!
//! ## 表单为什么要有「字段类型」
//!
//! 早先的表单只有纯文本框,协议是**手打**的 —— 打错一个字母的反馈是提交之后
//! 一句「无法识别的协议」,而正确的八个值一个都没显示出来。现在协议、所属 agent
//! 这类**取值来自一个有限集合**的字段是 `Select`,用 ←/→ 循环,打不错。
//!
//! ## 字段为什么要能隐藏
//!
//! sing-box 的 inbound 里,`server_name` 只对 reality/trojan/tuic/anytls 有意义,
//! `path` 只对两个 ws 协议有意义。让 shadowsocks 的表单上也摆着一个 `path` 框,
//! 填了不生效、不填又像漏了 —— 两种理解都是错的。所以表单支持按当前取值隐藏字段
//! (`Form::visible`),协议一换,字段跟着换。
//!
//! token 弹窗仍然是这里最要紧的一个:库里只存 `token_hash` 与 `token_prefix`,
//! 明文关掉就再也拿不回来了(§8.1)。所以它必须**显式**告诉人这件事。

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap},
    Frame,
};

use super::theme;
use crate::model::node::Protocol;

/// 弹窗确认后要执行的动作。**按键处理只负责产生它,不直接碰数据库** ——
/// 实际执行在主循环里统一做,这样每个动作的错误处理和刷新时机只有一处。
#[derive(Debug, Clone)]
pub enum Action {
    AddAgent {
        name: String,
        /// `None` = 不限流量。0 和 NULL 在界面上是同一件事,库里统一存 NULL。
        quota_bytes: Option<i64>,
        reset_day: Option<i64>,
    },
    EditAgent {
        id: i64,
        name: String,
        quota_bytes: Option<i64>,
        reset_day: Option<i64>,
    },
    RotateToken { id: i64, name: String },
    DeleteAgent { id: i64, name: String },
    /// 重新打印接入命令。**token 位置是占位符** —— 明文早就没了(§8.1),
    /// 这条只用来提醒「命令长什么样、缺的那段要去哪儿拿」。
    ShowInstall { id: i64, name: String },
    /// 改一个配置项。`value` 已经是 TOML 字面量(字符串带引号)。
    SetConfig { section: &'static str, key: &'static str, value: String, label: String },
    /// 打开「这个节点上各用户用了多少」。
    ShowNodeUsers { id: i64, tag: String, agent: String },
    /// 打开「这个用户在各节点上用了多少」。
    ShowUserNodes { id: i64, name: String },
    /// 打开「这台机器的网卡明细」:整机网卡用量 + 配额 + 各节点跑了多少。
    ShowAgentNics { id: i64, name: String },
    /// 打开「这台机器上 sing-box 跑的是什么配置」。
    ///
    /// 配置由主控**现场组装**(`service::build_agent_config`),不向 agent 要 ——
    /// 下发给它的就是这份字节,所以两边必然一致;而且离线的机器也能看,
    /// 那恰恰是最需要看的时候(「为什么这台一直连不上/没生效」)。
    ShowAgentConfig { id: i64, name: String },
    /// 立刻刷一遍。常规刷新是每秒一次,但改完配置或另一个进程动了库时要马上看到。
    Refresh,
    AddNode(NodeDraft),
    EditNode { id: i64, draft: NodeDraft },
    DeleteNode { id: i64, tag: String },
    AddUser {
        name: String,
        quota_gb: String,
        multiplier: String,
        expire: String,
        reset_day: String,
    },
    EditUser {
        id: i64,
        name: String,
        quota_gb: String,
        multiplier: String,
        expire: String,
        reset_day: String,
    },
    SetUserEnabled { name: String, enabled: bool },
    DeleteUser { name: String },
    SetUserNodes { user_id: i64, user: String, node_ids: Vec<i64> },
    /// 把用户订阅响应头里的流量换成这几台机器的网卡用量之和(§10.3)。
    /// 空列表 = 解绑,改回按用户自己的用量报。
    SetUserNics { user_id: i64, user: String, agent_ids: Vec<i64> },
    /// 重新生成订阅 token。老 URL 立即失效。
    RegenSubToken { user_id: i64, user: String },
    /// 撤销订阅 token。订阅地址返回 404,`[g]` 可恢复。
    RevokeSubToken { user_id: i64, user: String },
    /// 手动清零本周期流量。**不动**月重置日期。
    ResetUserTraffic { user_id: i64, user: String },
    /// 升级一台 agent(`None` = 在线的全部升)。
    UpgradeAgents { only: Option<i64>, name: String },
    /// 升级**主控自己**。要挂起 TUI 去跑安装脚本,所以由主循环处理。
    SelfUpgrade,
    /// 改这台 agent 的出站地址族策略。进 sing-box 配置,会推进 config_revision。
    SetOutbound { id: i64, name: String, strategy: crate::model::outbound::OutboundStrategy },
}

/// 节点表单填出来的东西。密钥材料**不在这里** —— 新增时由 `secrets::fill` 生成,
/// 编辑时从库里原样读回再写回去(§9.1:换一套密钥 = 客户端静默全部失联)。
#[derive(Debug, Clone)]
pub struct NodeDraft {
    pub agent_id: i64,
    pub tag: String,
    pub protocol: Protocol,
    pub port: u16,
    pub server_name: Option<String>,
    pub path: Option<String>,
    pub ipv6: bool,
    pub relay_host: String,
    pub relay_port: Option<u16>,
}

// ─────────────────────────── 字段 ───────────────────────────

pub enum FieldKind {
    Text { value: String },
    /// 取值来自一个有限集合,←/→ 循环。协议、所属 agent 走这条。
    Select { options: Vec<String>, idx: usize },
    Toggle { on: bool },
}

pub struct Field {
    /// 给 `build` / `visible` 闭包按名字取值用。用下标取值的话,
    /// 中间插一个字段就会静默改掉后面每一个的含义。
    pub key: &'static str,
    /// **提示写在标签里**,不再单独占一行(`端口 *必填 (默认 443)`)。
    ///
    /// 早先每个字段是「标签 / 灰色提示 / 空行」三行,八九个字段就把弹窗撑到快满屏,
    /// 读起来是散的:视线要跨过两行才能从一个输入框走到下一个。
    /// 装不进括号的长说明放到表单底部的说明区(`Form::with_note`)。
    pub label: String,
    pub kind: FieldKind,
}

impl Field {
    pub fn text(key: &'static str, label: &str, value: &str) -> Self {
        Self { key, label: label.into(), kind: FieldKind::Text { value: value.into() } }
    }

    pub fn select(key: &'static str, label: &str, options: Vec<String>, idx: usize) -> Self {
        let idx = if options.is_empty() { 0 } else { idx.min(options.len() - 1) };
        Self { key, label: label.into(), kind: FieldKind::Select { options, idx } }
    }

    pub fn toggle(key: &'static str, label: &str, on: bool) -> Self {
        Self { key, label: label.into(), kind: FieldKind::Toggle { on } }
    }

    /// 当前值的字符串形式。`Toggle` 给的是**显示用**的「开/关」,
    /// 判断真假请用 `is_on()` —— 拿 `value() == "开"` 去比会在换文案时静默失效。
    pub fn value(&self) -> String {
        match &self.kind {
            FieldKind::Text { value } => value.clone(),
            FieldKind::Select { options, idx } => options.get(*idx).cloned().unwrap_or_default(),
            FieldKind::Toggle { on } => if *on { "开" } else { "关" }.into(),
        }
    }

    pub fn trimmed(&self) -> String {
        self.value().trim().to_string()
    }

    pub fn is_on(&self) -> bool {
        matches!(self.kind, FieldKind::Toggle { on: true })
    }

    /// `Select` 的当前下标。`build` 里用它把选项映射回 id —— 按显示文本反查
    /// 会在两台 agent 同名时挑错一台。
    pub fn index(&self) -> usize {
        match &self.kind {
            FieldKind::Select { idx, .. } => *idx,
            _ => 0,
        }
    }
}

/// 按 key 取字段。取不到时返回一个空值而不是 panic:
/// 一个拼错的 key 不该把整个 TUI 打回 shell。
pub fn val(fields: &[Field], key: &str) -> String {
    fields.iter().find(|f| f.key == key).map(|f| f.trimmed()).unwrap_or_default()
}

pub fn on(fields: &[Field], key: &str) -> bool {
    fields.iter().find(|f| f.key == key).is_some_and(|f| f.is_on())
}

// ─────────────────────────── 表单 ───────────────────────────

type Visible = Box<dyn Fn(&[Field], &Field) -> bool + Send>;
type Note = Box<dyn Fn(&[Field]) -> Vec<String> + Send>;
type Build = Box<dyn Fn(&[Field]) -> Result<Action, String> + Send>;

pub struct Form {
    pub title: String,
    /// 标题下方的一行只读上下文(编辑谁、在哪台机器上)。
    pub head: Option<String>,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
    visible: Visible,
    note: Note,
    build: Build,
}

impl Form {
    pub fn new(title: &str, fields: Vec<Field>, build: Build) -> Self {
        Self {
            title: title.into(),
            head: None,
            fields,
            focus: 0,
            error: None,
            visible: Box::new(|_, _| true),
            note: Box::new(|_| Vec::new()),
            build,
        }
    }

    pub fn head(mut self, head: impl Into<String>) -> Self {
        self.head = Some(head.into());
        self
    }

    pub fn visible_when(mut self, f: Visible) -> Self {
        self.visible = f;
        self
    }

    pub fn with_note(mut self, f: Note) -> Self {
        self.note = f;
        self
    }

    fn shown(&self) -> Vec<usize> {
        (0..self.fields.len())
            .filter(|i| (self.visible)(&self.fields, &self.fields[*i]))
            .collect()
    }

    /// 焦点必须始终落在**可见**字段上。协议一换,`server_name` 可能就地消失,
    /// 焦点留在那里的话下一次按键会改一个屏幕上根本看不见的框。
    fn settle_focus(&mut self, forward: bool) {
        let shown = self.shown();
        if shown.is_empty() {
            self.focus = 0;
            return;
        }
        if shown.contains(&self.focus) {
            return;
        }
        self.focus = if forward {
            shown.iter().copied().find(|i| *i > self.focus).unwrap_or(shown[0])
        } else {
            shown.iter().copied().rev().find(|i| *i < self.focus).unwrap_or(shown[shown.len() - 1])
        };
    }

    fn step(&mut self, forward: bool) {
        let shown = self.shown();
        if shown.is_empty() {
            return;
        }
        let pos = shown.iter().position(|i| *i == self.focus).unwrap_or(0);
        let next = if forward {
            (pos + 1) % shown.len()
        } else if pos == 0 {
            shown.len() - 1
        } else {
            pos - 1
        };
        self.focus = shown[next];
    }
}

// ─────────────────────────── 多选 ───────────────────────────

pub struct PickItem {
    pub id: i64,
    pub label: String,
    /// 右侧灰字:协议、端口、所属 agent。分配节点时最容易选错的就是同名 tag。
    pub note: String,
    pub checked: bool,
}

/// 勾完之后拿选中的 id 造一个动作。
type PickBuild = Box<dyn Fn(&[i64]) -> Action + Send>;

pub struct Picker {
    pub title: String,
    pub head: String,
    pub items: Vec<PickItem>,
    pub cursor: usize,
    build: PickBuild,
}

impl Picker {
    pub fn new(title: &str, head: impl Into<String>, items: Vec<PickItem>, build: PickBuild) -> Self {
        Self { title: title.into(), head: head.into(), items, cursor: 0, build }
    }
}

// ─────────────────────────── 弹窗 ───────────────────────────

pub enum Modal {
    Form(Form),
    Picker(Picker),
    Confirm { title: String, body: Vec<String>, action: Action },
    Info {
        title: String,
        body: Vec<String>,
        /// 按 `y` 要复制走的东西。`None` = 这个框里没什么值得复制的。
        ///
        /// 接入命令那种两百多字符、还带着 token 的东西,用鼠标跨行选中很容易
        /// 漏掉一截或多带一个换行,而漏一截的表现是「粘过去跑了一半」。
        copy: Option<String>,
        /// 复制之后的回执,渲染在框里。
        copied: Option<String>,
    },
    /// 升级 agent:`[u]` 只升这一台、`[a]` 在线的全部升。
    ///
    /// 与 `Confirm` 一样是破坏性动作,但这里有**两个**互斥的选择,
    /// 而 `Confirm` 只能带一个。两者的影响面差一个数量级,所以必须让人
    /// 在按下去之前就看见自己选的是哪个。
    Upgrade {
        agent_id: i64,
        name: String,
        /// 在线的 agent 台数。「全部升」实际会碰几台,得写出来。
        online: usize,
        version: String,
    },
    /// 订阅 token 管理:`[g]` 重新生成、`[v]` 撤销。
    ///
    /// 为什么不用 `Confirm`:这里有**两个**互斥的破坏性动作,而 `Confirm`
    /// 只能带一个。做成两级菜单(先选再确认)要多按一次键,而这两个动作的
    /// 后果都写在选项旁边了 —— 按下去之前就知道会发生什么。
    Token {
        user_id: i64,
        name: String,
        /// 当前订阅是开着的吗(token 没被撤销)。撤销过的不再显示 `[v]`。
        active: bool,
    },
}

/// 一次按键之后弹窗该怎么办。
pub enum Outcome {
    /// 留着,继续输入。
    Stay,
    /// 关掉,不执行任何动作。`Some` 是给状态栏的一句话。
    Close(Option<String>),
    /// 关掉并执行。
    Run(Action),
}

impl Modal {
    pub fn confirm(title: &str, body: Vec<String>, action: Action) -> Self {
        Modal::Confirm { title: title.into(), body, action }
    }

    pub fn info(title: &str, body: Vec<String>) -> Self {
        Modal::Info { title: title.into(), body, copy: None, copied: None }
    }

    /// 带一键复制的信息框。`copy` 是按 `y` 时发给终端的内容。
    pub fn info_copyable(title: &str, body: Vec<String>, copy: impl Into<String>) -> Self {
        Modal::Info { title: title.into(), body, copy: Some(copy.into()), copied: None }
    }

    /// 处理一次按键。**弹窗打开时主循环把全部按键都交给这里** ——
    /// 否则在输入框里打一个 'q' 会直接退出程序。
    pub fn handle(&mut self, k: crossterm::event::KeyEvent) -> Outcome {
        use crossterm::event::KeyCode;
        match self {
            // 只读信息框:`y` 复制,其它键关掉。
            //
            // 复制**不关窗**:OSC 52 没有回执(见 clip.rs),关掉的话人根本不知道
            // 刚才那一下有没有生效。留着并显示一行回执,至少能看出程序确实做了动作。
            Modal::Info { copy, copied, .. } => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if copy.is_some() => {
                    let text = copy.clone().unwrap_or_default();
                    *copied = Some(match super::clip::copy(&text) {
                        Ok(()) => "已发给终端(OSC 52)。终端不支持的话这一下不会有任何效果 —— \
                                   那就用鼠标选中复制,或者在主控上跑 sbx agent-add 从普通终端里复制。"
                            .into(),
                        Err(e) => format!("复制失败:{e}"),
                    });
                    Outcome::Stay
                }
                _ => Outcome::Close(None),
            },

            // 删除类操作只认 y。其它键一律当取消 —— 不该有「手滑确认」的可能。
            Modal::Confirm { action, .. } => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Outcome::Run(action.clone()),
                _ => Outcome::Close(Some("已取消".into())),
            },

            // 升级 agent。只认那两个字母,别的键一律关掉 —— 与 Confirm 同理:
            // 「全部升级」会让整个集群依次重启,不该有手滑的可能。
            Modal::Upgrade { agent_id, name, .. } => match k.code {
                KeyCode::Char('u') | KeyCode::Char('U') => Outcome::Run(Action::UpgradeAgents {
                    only: Some(*agent_id),
                    name: name.clone(),
                }),
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    Outcome::Run(Action::UpgradeAgents { only: None, name: name.clone() })
                }
                _ => Outcome::Close(None),
            },

            // token 管理。两个动作都是**不可撤销**的,所以只认那两个字母,
            // 别的键一律关掉 —— 与 Confirm 同一个道理:不该有手滑的可能。
            Modal::Token { user_id, name, active } => match k.code {
                KeyCode::Char('g') | KeyCode::Char('G') => Outcome::Run(Action::RegenSubToken {
                    user_id: *user_id,
                    user: name.clone(),
                }),
                KeyCode::Char('v') | KeyCode::Char('V') if *active => {
                    Outcome::Run(Action::RevokeSubToken { user_id: *user_id, user: name.clone() })
                }
                _ => Outcome::Close(None),
            },

            Modal::Form(form) => {
                match k.code {
                    KeyCode::Esc => return Outcome::Close(Some("已取消".into())),
                    KeyCode::Tab | KeyCode::Down => {
                        form.step(true);
                        return Outcome::Stay;
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        form.step(false);
                        return Outcome::Stay;
                    }
                    KeyCode::Enter => {
                        return match (form.build)(&form.fields) {
                            Ok(a) => Outcome::Run(a),
                            Err(msg) => {
                                // 校验没过就把弹窗留着,消息显示在里面 ——
                                // 关掉重填等于把人已经打好的字全扔了。
                                form.error = Some(msg);
                                Outcome::Stay
                            }
                        };
                    }
                    _ => {}
                }

                // 一有输入就把上一次的报错清掉,否则它会一直挂到下次提交。
                form.error = None;
                let focus = form.focus;
                let Some(field) = form.fields.get_mut(focus) else { return Outcome::Stay };
                match (&mut field.kind, k.code) {
                    (FieldKind::Select { options, idx }, KeyCode::Left) if !options.is_empty() => {
                        *idx = if *idx == 0 { options.len() - 1 } else { *idx - 1 };
                    }
                    (FieldKind::Select { options, idx }, KeyCode::Right) if !options.is_empty() => {
                        *idx = (*idx + 1) % options.len();
                    }
                    (FieldKind::Toggle { on }, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) => {
                        *on = !*on;
                    }
                    (FieldKind::Text { value }, KeyCode::Backspace) => {
                        value.pop();
                    }
                    (FieldKind::Text { value }, KeyCode::Char(c)) => {
                        value.push(c);
                    }
                    _ => return Outcome::Stay,
                }
                // 改了 Select 可能让当前字段之外的字段显隐变化;焦点本身仍然可见,
                // 但下面这一下保证「协议切到 shadowsocks 时焦点不会卡在已消失的 path 上」。
                form.settle_focus(true);
                Outcome::Stay
            }

            Modal::Picker(p) => match k.code {
                KeyCode::Esc => Outcome::Close(Some("已取消".into())),
                KeyCode::Down | KeyCode::Char('j') => {
                    if !p.items.is_empty() {
                        p.cursor = (p.cursor + 1) % p.items.len();
                    }
                    Outcome::Stay
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if !p.items.is_empty() {
                        p.cursor = if p.cursor == 0 { p.items.len() - 1 } else { p.cursor - 1 };
                    }
                    Outcome::Stay
                }
                KeyCode::Char(' ') => {
                    if let Some(it) = p.items.get_mut(p.cursor) {
                        it.checked = !it.checked;
                    }
                    Outcome::Stay
                }
                // 全选 / 全不选。节点多的时候一个个按空格太慢。
                KeyCode::Char('a') => {
                    let all = p.items.iter().all(|i| i.checked);
                    for it in &mut p.items {
                        it.checked = !all;
                    }
                    Outcome::Stay
                }
                KeyCode::Enter => {
                    let ids: Vec<i64> =
                        p.items.iter().filter(|i| i.checked).map(|i| i.id).collect();
                    Outcome::Run((p.build)(&ids))
                }
                _ => Outcome::Stay,
            },
        }
    }
}

// ─────────────────────────── 渲染 ───────────────────────────

/// 在屏幕中央挖一块区域。宽高按百分比取,但**都有下限** ——
/// 终端被拉得很窄时百分比会算出 0 宽,弹窗直接看不见。
pub fn centered(area: Rect, pct_x: u16, pct_y: u16, min_w: u16, min_h: u16) -> Rect {
    let w = (area.width * pct_x / 100).max(min_w).min(area.width);
    let h = (area.height * pct_y / 100).max(min_h).min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect { x, y, width: w, height: h }
}

pub fn render(f: &mut Frame, area: Rect, modal: &Modal) {
    match modal {
        Modal::Form(form) => render_form(f, area, form),
        Modal::Picker(p) => render_picker(f, area, p),

        Modal::Confirm { title, body, .. } => {
            let rect = centered(area, 55, 30, 44, body.len() as u16 + 4);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = body.iter().map(|l| Line::from(format!("  {l}"))).collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [y]确认  [n / Esc]取消",
                Style::default().fg(theme::DIM),
            )));
            f.render_widget(
                Paragraph::new(lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!(" {title} "))
                            .border_style(Style::default().fg(Color::Red)),
                    )
                    .wrap(Wrap { trim: false }),
                rect,
            );
        }

        Modal::Info { title, body, copy, copied } => {
            let w = (area.width * 84 / 100).clamp(50, 110).min(area.width.max(1));
            let inner = (w as usize).saturating_sub(6);
            // 接入命令有两百多字符,一定会折行。自己折才能把折出来的行数
            // 算进高度 —— 交给 Wrap 的话框底下会被裁掉,命令只剩前两行。
            let wrapped: Vec<Vec<String>> = body.iter().map(|l| theme::wrap(l, inner)).collect();
            let n: usize = wrapped.iter().map(|l| l.len()).sum();
            let extra = 4 + u16::from(copied.is_some()) * 3;
            let h = (n as u16 + extra).min(area.height.max(1));
            let rect = centered(area, 0, 0, w, h);
            f.render_widget(Clear, rect);

            let mut lines: Vec<Line> = Vec::new();
            for seg in wrapped.iter().flatten() {
                lines.push(Line::from(format!("  {seg}")));
            }
            if let Some(msg) = copied {
                lines.push(Line::from(""));
                for seg in theme::wrap(msg, inner) {
                    lines.push(Line::from(Span::styled(
                        format!("  {seg}"),
                        Style::default().fg(theme::ONLINE),
                    )));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                if copy.is_some() { "  [y]复制到剪贴板  [任意键]关闭" } else { "  [任意键]关闭" },
                Style::default().fg(theme::DIM),
            )));
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {title} "))
                        .border_style(Style::default().fg(theme::ACCENT)),
                ),
                rect,
            );
        }

        Modal::Upgrade { name, online, version, .. } => {
            let w = 72.min(area.width.max(1));
            let h = 12u16.min(area.height.max(1));
            let rect = centered(area, 0, 0, w, h);
            f.render_widget(Clear, rect);
            let lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::raw("  升级到 "),
                    Span::styled(
                        format!("v{version}"),
                        Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("(主控自己的版本)", Style::default().fg(theme::DIM)),
                ]),
                Line::from(""),
                Line::from(format!("  [u]  只升级 {name}")),
                Line::from(format!("  [a]  升级在线的全部 {online} 台")),
                Line::from(""),
                Line::from(Span::styled(
                    "  升完 agent 会替换自己的二进制并退出,由 systemd 拉起新的。",
                    Style::default().fg(theme::DIM),
                )),
                Line::from(Span::styled(
                    "  期间那台机器上的代理会断几秒。离线的机器跳过。",
                    Style::default().fg(theme::DIM),
                )),
                Line::from(""),
                Line::from(Span::styled("  [Esc/任意键] 取消", Style::default().fg(theme::DIM))),
            ];
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" 升级 agent ")
                        .border_style(Style::default().fg(theme::ACCENT)),
                ),
                rect,
            );
        }

        // token 管理。两个动作的**后果**写在选项那一行上,不是折叠在下面的说明里
        // —— 它们都不可撤销,人得在按下去之前就看见。
        Modal::Token { name, active, .. } => {
            let w = 64.min(area.width.max(1));
            let h = 10u16.min(area.height.max(1));
            let rect = centered(area, 0, 0, w, h);
            f.render_widget(Clear, rect);

            let (dot, state) = if *active {
                ("●", "订阅已开启")
            } else {
                ("○", "订阅已撤销")
            };
            let mut lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::raw(format!("  用户: {name}    当前: ")),
                    Span::styled(
                        format!("{dot} {state}"),
                        Style::default()
                            .fg(if *active { theme::ONLINE } else { theme::OFFLINE })
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from("  [g]  重新生成 token(老 URL 立即失效)"),
            ];
            if *active {
                lines.push(Line::from("  [v]  撤销 token(订阅返回 404,[g] 可恢复)"));
            } else {
                lines.push(Line::from(Span::styled(
                    "       已撤销;按 [g] 重新生成即可恢复订阅",
                    Style::default().fg(theme::DIM),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Esc/任意键] 取消",
                Style::default().fg(theme::DIM),
            )));

            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Token 管理 ")
                        .border_style(Style::default().fg(theme::ACCENT)),
                ),
                rect,
            );
        }
    }
}

/// 表单底部那行按键提示。做成常量是为了能把它的宽度算进弹窗宽度 ——
/// 它比大多数字段都长,不算进去就会被切成「[Esc]取」。
const FORM_HINT: &str = "[Tab/↑↓]切换  [←→]改选项  [空格]开关  [Enter]确定  [Esc]取消";
const PICKER_HINT: &str = "[↑↓/jk]移动  [空格]勾选  [a]全选/全不选  [Enter]保存  [Esc]取消";

fn render_form(f: &mut Frame, area: Rect, form: &Form) {
    let shown = form.shown();

    // 标签列按**最长的可见标签**对齐,所有取值就落在同一竖线上。
    // 夹上下限:太窄挤在一起,太宽会把取值推到屏幕右边。
    let label_w = shown
        .iter()
        .map(|i| theme::cols(&form.fields[*i].label))
        .max()
        .unwrap_or(16)
        .clamp(14, 34);

    let raw_notes = (form.note)(&form.fields);
    // 宽度按**内容**给,不按百分比:百分比在宽屏上会拉出一个空荡荡的大框,
    // 而窄一点的框会把说明和按键提示切掉。三样都要塞得进去:
    // 字段行、说明行、底部按键提示。
    let widest_note = raw_notes.iter().map(|n| theme::cols(n.trim())).max().unwrap_or(0);
    let w = (label_w as u16 + 44)
        .max(widest_note as u16 + 8)
        .max(theme::cols(FORM_HINT) as u16 + 4)
        .clamp(50, 84)
        .min(area.width.max(1));
    // 左右边框 + 续行的缩进。说明区**自己折行**,交给 ratatui 的 Wrap 的话行数
    // 算不进下面的高度,底下几条会被静默裁掉(theme::wrap 的文档里有原委)。
    let inner = (w as usize).saturating_sub(6);

    let head: Vec<String> =
        form.head.as_deref().map(|h| theme::wrap(h, inner)).unwrap_or_default();
    let notes: Vec<Vec<String>> =
        raw_notes.iter().map(|n| theme::wrap(n.trim(), inner)).collect();
    let note_lines: usize = notes.iter().map(|n| n.len()).sum();

    // 每个字段**两行**:标签 + 取值一行,再空一行。
    // 以前提示单独占第三行,八九个字段就把弹窗撑到快满屏 —— 视线要跨过两行
    // 才能从一个输入框走到下一个,读起来是散的。提示现在并进标签或落到底部。
    //
    // 末尾 +3 = 上下边框 2 + 按键提示 1。字段那一段自带尾随空行,不用再留。
    let h = shown.len() as u16 * 2
        + note_lines as u16
        + head.len() as u16
        + u16::from(!head.is_empty())
        + u16::from(form.error.is_some())
        + 3;

    let rect = centered(area, 0, 0, w, h.max(7));
    f.render_widget(Clear, rect);

    let mut lines: Vec<Line> = Vec::new();
    for (i, seg) in head.iter().enumerate() {
        lines.push(Line::from(Span::styled(
            format!("{}{seg}", if i == 0 { "  " } else { "    " }),
            Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        )));
    }
    if !head.is_empty() {
        lines.push(Line::from(""));
    }

    for i in shown {
        let fld = &form.fields[i];
        let focused = i == form.focus;
        // 聚焦的取值用反白底,和旧项目 `tui/forms.rs` 是同一套观感 ——
        // 只靠一个箭头标记的话,在一屏九个字段里很难一眼看出焦点在哪。
        let value_style = if focused {
            // 深灰蓝底 + 原色前景。**不用黄底深字** —— 很多终端配色会把
            // `Color::Black` 渲染成一个偏亮的灰,结果是黄底浅字,
            // 对比度反而比不选中还低(theme::SELECT_BG 的注释里有原委)。
            Style::default().bg(theme::SELECT_BG).add_modifier(Modifier::BOLD)
        } else if fld.is_on() {
            Style::default().fg(theme::ONLINE)
        } else {
            Style::default()
        };
        let label_style = if focused {
            Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::ACCENT)
        };

        let value: Vec<Span> = match &fld.kind {
            // ◀ ▶ 是**可操作的提示**,不是装饰:没有它就没人知道这里能左右切。
            FieldKind::Select { .. } | FieldKind::Toggle { .. } => {
                let arrow = if focused { theme::ACCENT } else { theme::DIM };
                vec![
                    Span::styled("◀ ", Style::default().fg(arrow)),
                    Span::styled(fld.value(), value_style),
                    Span::styled(" ▶", Style::default().fg(arrow)),
                ]
            }
            FieldKind::Text { value } => {
                let cursor = if focused { "_" } else { "" };
                vec![Span::styled(format!(" {value}{cursor}  "), value_style)]
            }
        };

        let mut row = vec![
            Span::styled(if focused { " ▸ " } else { "   " }, Style::default().fg(theme::ACCENT)),
            Span::styled(theme::pad(&fld.label, label_w), label_style),
            Span::raw(" "),
        ];
        row.extend(value);
        lines.push(Line::from(row));
        lines.push(Line::from(""));
    }

    for note in &notes {
        for (i, seg) in note.iter().enumerate() {
            // 续行多缩进两格,免得和下一条说明的开头长得一样。
            lines.push(Line::from(Span::styled(
                format!("{}{seg}", if i == 0 { "  " } else { "    " }),
                Style::default().fg(theme::DIM),
            )));
        }
    }
    if let Some(e) = &form.error {
        lines.push(Line::from(Span::styled(format!("  ! {e}"), Style::default().fg(Color::Red))));
    }
    lines.push(Line::from(Span::styled(
        format!("  {FORM_HINT}"),
        Style::default().fg(theme::DIM),
    )));

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(format!(" {} ", form.title))),
        rect,
    );
}

fn render_picker(f: &mut Frame, area: Rect, p: &Picker) {
    // 宽度按最长的一项给,高度按实际条数给。用百分比的话,
    // 一个只有一行的多选框会在高屏幕上撑成一个大空框。
    let widest = p
        .items
        .iter()
        .map(|i| theme::cols(&i.label).min(24) + theme::cols(&i.note) + 10)
        .max()
        .unwrap_or(0);
    let w = (widest as u16)
        .max(theme::cols(&p.head) as u16 + 6)
        .max(theme::cols(PICKER_HINT) as u16 + 4)
        .clamp(50, 92)
        .min(area.width.max(1));
    // 标题 1 + 空行 1 + 条目 n + 空行 1 + 提示 1 + 上下边框 2。
    let h = (p.items.len().max(1) as u16 + 6).min(area.height.max(1));
    let rect = centered(area, 0, 0, w, h);
    f.render_widget(Clear, rect);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!("  {}", p.head),
            Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if p.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "  还没有节点。先去「节点」页按 [a] 建一个。",
            Style::default().fg(theme::DIM),
        )));
    }
    for (i, it) in p.items.iter().enumerate() {
        let sel = i == p.cursor;
        let mark = if it.checked { "[x]" } else { "[ ]" };
        lines.push(Line::from(vec![
            Span::styled(if sel { " ▸ " } else { "   " }, Style::default().fg(theme::ACCENT)),
            Span::styled(
                format!("{mark} "),
                Style::default().fg(if it.checked { theme::ONLINE } else { theme::DIM }),
            ),
            Span::styled(
                format!("{:<24}", theme::truncate(&it.label, 24)),
                if sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
            Span::styled(it.note.clone(), Style::default().fg(theme::DIM)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  {PICKER_HINT}"),
        Style::default().fg(theme::DIM),
    )));

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(format!(" {} ", p.title))),
        rect,
    );
}

/// 底部状态条。
/// 「操作」面板:选中项摘要 + 这一页能按的键,外面套一个边框。
///
/// 两行放在同一个框里是有意的:「[d]删除」和「选中: alice」隔开摆的话,
/// 按下去之前得先抬头去表里找光标在哪 —— 而那正是最不该需要确认两次的时刻。
///
/// `msg` 是一次性回执(某个操作刚做完)。有它的时候**顶掉摘要那一行**,
/// 而不是再挤出一行来:回执是当下最要紧的信息,而摘要下一帧就会回来。
pub fn ops_panel(
    f: &mut Frame,
    area: Rect,
    summary: &[String],
    keys: &str,
    msg: Option<&str>,
    is_error: bool,
) {
    let inner = (area.width as usize).saturating_sub(2);
    let mut lines: Vec<Line> = Vec::new();
    match msg {
        // 一次性回执**顶掉第一行摘要**,不再另挤一行出来:回执是当下最要紧的
        // 信息,而摘要下一帧就回来了。
        Some(m) => {
            lines.push(Line::from(Span::styled(
                theme::truncate(&format!("  {m}"), inner),
                if is_error {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::ONLINE)
                },
            )));
            for extra in summary.iter().skip(1) {
                lines.push(Line::from(theme::truncate(extra, inner)));
            }
        }
        None => {
            for l in summary {
                lines.push(Line::from(theme::truncate(l, inner)));
            }
        }
    }
    lines.push(Line::from(Span::styled(
        theme::truncate(keys, inner),
        Style::default().fg(theme::DIM),
    )));
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" 操作 ")),
        area,
    );
}

/// 最底下那条页脚:版本、规模、每页都一样的那几个键。
///
/// 与上面那个「操作」面板分开是刻意的:一条里既有「哪儿都能按的键」又有
/// 「只有这一页能按的键」时,人得逐个读完才知道哪个属于当前页。
/// 分开之后「操作」永远只回答一个问题:「在这一页我能做什么」。
pub fn info_bar(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            theme::truncate(text, area.width as usize),
            Style::default().fg(theme::DIM),
        ))),
        area,
    );
}

/// 顶部页签。
///
/// 样式照旧项目 `tui/widgets/tab_bar.rs`:`名字[序号]`,用 ratatui 的 `Tabs`
/// 加一条下边框把页签区和内容区分开。序号就是**能直接按的键**(§8.2)——
/// 只有 Tab 循环的话,从第一页跳到第五页要按四下,而人心里想的是「去第 5 页」。
pub fn tabs(f: &mut Frame, area: Rect, titles: &[&str], selected: usize) {
    let items: Vec<Line> =
        titles.iter().enumerate().map(|(i, t)| Line::from(format!(" {t}[{}] ", i + 1))).collect();
    f.render_widget(
        Tabs::new(items)
            .select(selected)
            .divider("│")
            .block(Block::default().borders(Borders::BOTTOM).border_style(Style::default().fg(theme::TRACK)))
            .highlight_style(Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD))
            .style(Style::default().fg(theme::DIM)),
        area,
    );
}

#[cfg(test)]
mod render_preview {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(w: u16, h: u16, m: &Modal) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, f.area(), m)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("
")
    }

    /// `cargo test render_preview::token -- --nocapture` 看一眼长什么样。
    #[test]
    fn token() {
        for active in [true, false] {
            let m = Modal::Token { user_id: 1, name: "alice".into(), active };
            let out = draw(80, 12, &m);
            println!("── active={active} ──
{}
", out.trim_end());
            // 状态那一行必须说清现在是开是关 —— 两个动作的后果取决于它。
            let flat: String = out.chars().filter(|c| !c.is_whitespace()).collect();
            if active {
                assert!(flat.contains("订阅已开启"), "{out}");
                assert!(flat.contains("[v]"), "开着的时候要给撤销:{out}");
            } else {
                assert!(flat.contains("订阅已撤销"), "{out}");
                assert!(!flat.contains("[v]"), "已撤销就不该再给 [v]:{out}");
                assert!(flat.contains("[g]"), "恢复路径必须留着:{out}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn form() -> Form {
        Form::new(
            "t",
            vec![
                Field::text("tag", "Tag", ""),
                Field::select("proto", "协议", vec!["a".into(), "b".into(), "c".into()], 0),
                Field::text("sni", "SNI", ""),
                Field::toggle("v6", "IPv6", false),
            ],
            Box::new(|f| Ok(Action::DeleteUser { name: val(f, "tag") })),
        )
        // "sni" 只在协议 = "a" 时出现,用来测显隐与焦点。
        .visible_when(Box::new(|fields, f| f.key != "sni" || val(fields, "proto") == "a"))
    }

    #[test]
    fn select_cycles_with_arrows_and_wraps() {
        let mut m = Modal::Form(form());
        let Modal::Form(f) = &mut m else { unreachable!() };
        f.focus = 1;
        m.handle(key(KeyCode::Right));
        let Modal::Form(f) = &m else { unreachable!() };
        assert_eq!(val(&f.fields, "proto"), "b");

        m.handle(key(KeyCode::Left));
        m.handle(key(KeyCode::Left));
        let Modal::Form(f) = &m else { unreachable!() };
        assert_eq!(val(&f.fields, "proto"), "c", "从第一项往左应当绕到最后一项");
    }

    #[test]
    fn toggle_flips_on_space_and_arrows() {
        let mut m = Modal::Form(form());
        let Modal::Form(f) = &mut m else { unreachable!() };
        f.focus = 3;
        m.handle(key(KeyCode::Char(' ')));
        let Modal::Form(f) = &m else { unreachable!() };
        assert!(on(&f.fields, "v6"));
        m.handle(key(KeyCode::Left));
        let Modal::Form(f) = &m else { unreachable!() };
        assert!(!on(&f.fields, "v6"));
    }

    /// 空格落在开关上是「切换」,落在文本框里必须是**打一个空格** ——
    /// 名字里带空格的节点 tag 是合法的。
    #[test]
    fn space_types_into_a_text_field() {
        let mut m = Modal::Form(form());
        m.handle(key(KeyCode::Char('a')));
        m.handle(key(KeyCode::Char(' ')));
        m.handle(key(KeyCode::Char('b')));
        let Modal::Form(f) = &m else { unreachable!() };
        assert_eq!(f.fields[0].value(), "a b");
    }

    /// Tab 只在**可见**字段之间走。隐藏的 SNI 不该被跳进去 ——
    /// 那会表现成「按了一下 Tab,光标不见了」。
    #[test]
    fn tab_skips_hidden_fields() {
        let mut m = Modal::Form(form());
        // 协议默认 "a" → sni 可见,四个字段都能到。
        let seen = |m: &mut Modal| {
            let mut v = vec![];
            for _ in 0..4 {
                let Modal::Form(f) = &*m else { unreachable!() };
                v.push(f.focus);
                m.handle(key(KeyCode::Tab));
            }
            v
        };
        assert_eq!(seen(&mut m), vec![0, 1, 2, 3]);

        // 切到 "b" 之后 sni(下标 2)必须被跳过。
        let Modal::Form(f) = &mut m else { unreachable!() };
        f.focus = 1;
        m.handle(key(KeyCode::Right));
        let Modal::Form(f) = &mut m else { unreachable!() };
        f.focus = 0;
        let mut v = vec![];
        for _ in 0..3 {
            let Modal::Form(f) = &m else { unreachable!() };
            v.push(f.focus);
            m.handle(key(KeyCode::Tab));
        }
        assert_eq!(v, vec![0, 1, 3], "隐藏的 SNI 不该被 Tab 到");
    }

    /// 焦点停在某个字段上,而这个字段因为别处的改动消失了 ——
    /// 之后的按键必须落到一个看得见的框里。
    #[test]
    fn focus_leaves_a_field_that_just_disappeared() {
        let mut m = Modal::Form(form());
        let Modal::Form(f) = &mut m else { unreachable!() };
        f.focus = 1; // 协议
        m.handle(key(KeyCode::Right)); // → "b",sni 消失
        let Modal::Form(f) = &m else { unreachable!() };
        assert_eq!(f.focus, 1, "焦点本来就在协议上,不该乱跳");

        // 手动把焦点放到已隐藏的 sni 上,再敲一下:必须自己挪走。
        let Modal::Form(f) = &mut m else { unreachable!() };
        f.focus = 2;
        m.handle(key(KeyCode::Char('x')));
        let Modal::Form(f) = &m else { unreachable!() };
        assert_ne!(f.focus, 2, "焦点不能留在看不见的字段上");
        assert_eq!(f.fields[2].value(), "x", "这一下按键仍然落到原字段(下一帧才纠正)");
    }

    /// 校验没过时弹窗要留着,人打的字不能丢。
    #[test]
    fn failed_validation_keeps_the_form_open() {
        let mut m = Modal::Form(Form::new(
            "t",
            vec![Field::text("name", "名称", "")],
            Box::new(|f| {
                if val(f, "name").is_empty() {
                    Err("名称不能为空".into())
                } else {
                    Ok(Action::DeleteUser { name: val(f, "name") })
                }
            }),
        ));
        assert!(matches!(m.handle(key(KeyCode::Enter)), Outcome::Stay));
        let Modal::Form(f) = &m else { unreachable!() };
        assert_eq!(f.error.as_deref(), Some("名称不能为空"));

        m.handle(key(KeyCode::Char('x')));
        let Modal::Form(f) = &m else { unreachable!() };
        assert!(f.error.is_none(), "一有输入就该把上次的报错清掉");
        assert!(matches!(m.handle(key(KeyCode::Enter)), Outcome::Run(_)));
    }

    #[test]
    fn confirm_only_accepts_y() {
        for (c, want_run) in [('y', true), ('Y', true), ('n', false), ('d', false), (' ', false)] {
            let mut m = Modal::confirm("删", vec![], Action::DeleteUser { name: "x".into() });
            let out = m.handle(key(KeyCode::Char(c)));
            assert_eq!(matches!(out, Outcome::Run(_)), want_run, "按 {c:?} 时的行为不对");
        }
    }

    fn picker() -> Modal {
        Modal::Picker(Picker::new(
            "分配节点",
            "alice",
            vec![
                PickItem { id: 7, label: "n1".into(), note: String::new(), checked: true },
                PickItem { id: 8, label: "n2".into(), note: String::new(), checked: false },
                PickItem { id: 9, label: "n3".into(), note: String::new(), checked: false },
            ],
            Box::new(|ids| Action::SetUserNodes {
                user_id: 1,
                user: "alice".into(),
                node_ids: ids.to_vec(),
            }),
        ))
    }

    #[test]
    fn picker_toggles_and_returns_checked_ids() {
        let mut m = picker();
        m.handle(key(KeyCode::Down)); // → n2
        m.handle(key(KeyCode::Char(' ')));
        let Outcome::Run(Action::SetUserNodes { node_ids, .. }) = m.handle(key(KeyCode::Enter))
        else {
            panic!("Enter 应当提交")
        };
        assert_eq!(node_ids, vec![7, 8]);
    }

    /// 全部取消勾选后提交,给出的必须是**空列表**而不是「没变化」——
    /// 「把这个用户的节点全部收回」是一个正当操作。
    #[test]
    fn picker_can_submit_an_empty_selection() {
        let mut m = picker();
        m.handle(key(KeyCode::Char('a'))); // 有未勾选的 → 全选
        m.handle(key(KeyCode::Char('a'))); // 全勾着 → 全不选
        let Outcome::Run(Action::SetUserNodes { node_ids, .. }) = m.handle(key(KeyCode::Enter))
        else {
            panic!()
        };
        assert!(node_ids.is_empty());
    }

    /// 终端被拉到很小时,百分比会算出 0 宽/0 高 —— 弹窗直接消失。
    /// 下限保证它至少还在,哪怕会被裁掉一部分。
    #[test]
    fn centered_respects_minimum_size() {
        let tiny = Rect { x: 0, y: 0, width: 20, height: 6 };
        let r = centered(tiny, 60, 40, 44, 9);
        assert!(r.width > 0 && r.height > 0);
        assert!(r.width <= tiny.width);
        assert!(r.height <= tiny.height);
    }

    #[test]
    fn centered_is_actually_centered() {
        let area = Rect { x: 0, y: 0, width: 100, height: 40 };
        let r = centered(area, 50, 50, 10, 5);
        assert_eq!(r.width, 50);
        assert_eq!(r.height, 20);
        assert_eq!(r.x, 25);
        assert_eq!(r.y, 10);
    }

    /// 弹窗不能画到 area 之外 —— ratatui 会 panic。
    #[test]
    fn centered_never_escapes_the_area() {
        for w in 1..40u16 {
            for h in 1..20u16 {
                let area = Rect { x: 3, y: 2, width: w, height: h };
                let r = centered(area, 80, 60, 44, 9);
                assert!(r.x >= area.x && r.y >= area.y, "{r:?} 越过左上角 {area:?}");
                assert!(r.x + r.width <= area.x + area.width, "{r:?} 越过右边界 {area:?}");
                assert!(r.y + r.height <= area.y + area.height, "{r:?} 越过下边界 {area:?}");
            }
        }
    }

    /// 表单/多选框在极窄终端下都不能 panic。
    #[test]
    fn modals_render_in_tiny_terminals() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        for (w, h) in [(1u16, 1u16), (20, 4), (46, 9), (200, 60)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let f = Modal::Form(form());
            term.draw(|frame| render(frame, frame.area(), &f)).unwrap();
            let p = picker();
            term.draw(|frame| render(frame, frame.area(), &p)).unwrap();
            let i = Modal::info("t", vec!["a".into(); 30]);
            term.draw(|frame| render(frame, frame.area(), &i)).unwrap();
        }
    }
}
