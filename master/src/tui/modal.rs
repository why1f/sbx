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
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use super::theme;
use crate::model::node::Protocol;

/// 弹窗确认后要执行的动作。**按键处理只负责产生它,不直接碰数据库** ——
/// 实际执行在主循环里统一做,这样每个动作的错误处理和刷新时机只有一处。
#[derive(Debug, Clone)]
pub enum Action {
    AddAgent { name: String, host: String },
    EditAgent {
        id: i64,
        name: String,
        /// `None` = 不限流量。0 和 NULL 在界面上是同一件事,库里统一存 NULL。
        quota_bytes: Option<i64>,
        reset_day: Option<i64>,
    },
    RotateToken { id: i64, name: String, host: String },
    DeleteAgent { id: i64, name: String },
    /// 重新打印接入命令。**token 位置是占位符** —— 明文早就没了(§8.1),
    /// 这条只用来提醒「命令长什么样、缺的那段要去哪儿拿」。
    ShowInstall { id: i64, name: String, host: String },
    AddNode(NodeDraft),
    EditNode { id: i64, draft: NodeDraft },
    DeleteNode { id: i64, tag: String },
    AddUser { name: String, quota_gb: String },
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
    pub label: String,
    /// 灰色提示,写清默认值或格式。表单里最贵的错误是「填错了但看起来像对的」。
    pub hint: String,
    pub kind: FieldKind,
}

impl Field {
    pub fn text(key: &'static str, label: &str, value: &str, hint: &str) -> Self {
        Self {
            key,
            label: label.into(),
            hint: hint.into(),
            kind: FieldKind::Text { value: value.into() },
        }
    }

    pub fn select(key: &'static str, label: &str, options: Vec<String>, idx: usize, hint: &str) -> Self {
        let idx = if options.is_empty() { 0 } else { idx.min(options.len() - 1) };
        Self { key, label: label.into(), hint: hint.into(), kind: FieldKind::Select { options, idx } }
    }

    pub fn toggle(key: &'static str, label: &str, on: bool, hint: &str) -> Self {
        Self { key, label: label.into(), hint: hint.into(), kind: FieldKind::Toggle { on } }
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
    Info { title: String, body: Vec<String> },
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
        Modal::Info { title: title.into(), body }
    }

    /// 处理一次按键。**弹窗打开时主循环把全部按键都交给这里** ——
    /// 否则在输入框里打一个 'q' 会直接退出程序。
    pub fn handle(&mut self, k: crossterm::event::KeyEvent) -> Outcome {
        use crossterm::event::KeyCode;
        match self {
            // 只读信息框:任意键关掉。
            Modal::Info { .. } => Outcome::Close(None),

            // 删除类操作只认 y。其它键一律当取消 —— 不该有「手滑确认」的可能。
            Modal::Confirm { action, .. } => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Outcome::Run(action.clone()),
                _ => Outcome::Close(Some("已取消".into())),
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

        Modal::Info { title, body } => {
            let rect = centered(area, 84, 60, 50, body.len() as u16 + 4);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = body.iter().map(|l| Line::from(format!("  {l}"))).collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("  [任意键]关闭", Style::default().fg(theme::DIM))));
            f.render_widget(
                Paragraph::new(lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!(" {title} "))
                            .border_style(Style::default().fg(theme::ACCENT)),
                    )
                    .alignment(Alignment::Left)
                    .wrap(Wrap { trim: false }),
                rect,
            );
        }
    }
}

fn render_form(f: &mut Frame, area: Rect, form: &Form) {
    let shown = form.shown();
    let notes = (form.note)(&form.fields);

    // 高度按**实际内容**算。写死行数的话,加一个字段就会被静默裁掉 ——
    // 表单里少一行的表现是「那个字段填不了」,而不是「界面短了一点」。
    let h = shown.len() as u16 * 3
        + notes.len() as u16
        + u16::from(form.head.is_some()) * 2
        + u16::from(form.error.is_some())
        + 4;
    let rect = centered(area, 66, 70, 46, h.max(9));
    f.render_widget(Clear, rect);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(head) = &form.head {
        lines.push(Line::from(Span::styled(
            format!("  {head}"),
            Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    for i in shown {
        let fld = &form.fields[i];
        let focused = i == form.focus;
        let marker = if focused { "▸ " } else { "  " };
        let label_style = if focused {
            Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };

        let value: Vec<Span> = match &fld.kind {
            // ◀ ▶ 是**可操作的提示**,不是装饰:没有它就没人知道这里能左右切。
            FieldKind::Select { .. } | FieldKind::Toggle { .. } => {
                let arrow = if focused { theme::ACCENT } else { theme::DIM };
                vec![
                    Span::styled("◀ ", Style::default().fg(arrow)),
                    Span::styled(
                        fld.value(),
                        Style::default().fg(if fld.is_on() { theme::ONLINE } else { Color::Reset }),
                    ),
                    Span::styled(" ▶", Style::default().fg(arrow)),
                ]
            }
            FieldKind::Text { value } => {
                let mut v = vec![Span::raw(value.clone())];
                if focused {
                    v.push(Span::styled("█", Style::default().fg(theme::ACCENT)));
                }
                v
            }
        };

        let mut row = vec![
            Span::styled(marker, Style::default().fg(theme::ACCENT)),
            Span::styled(format!("{}: ", fld.label), label_style),
        ];
        row.extend(value);
        lines.push(Line::from(row));
        lines.push(Line::from(Span::styled(
            format!("    {}", fld.hint),
            Style::default().fg(theme::DIM),
        )));
        lines.push(Line::from(""));
    }

    for n in &notes {
        lines.push(Line::from(Span::styled(format!("  {n}"), Style::default().fg(theme::DIM))));
    }
    if let Some(e) = &form.error {
        lines.push(Line::from(Span::styled(format!("  ! {e}"), Style::default().fg(Color::Red))));
    }
    lines.push(Line::from(Span::styled(
        "  [Tab/↑↓]切换字段  [←→]改选项  [空格]开关  [Enter]确定  [Esc]取消",
        Style::default().fg(theme::DIM),
    )));

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(format!(" {} ", form.title)))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

fn render_picker(f: &mut Frame, area: Rect, p: &Picker) {
    let h = p.items.len().max(1) as u16 + 7;
    let rect = centered(area, 66, 70, 46, h.min(area.height.max(1)));
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
        "  [↑↓/jk]移动  [空格]勾选  [a]全选/全不选  [Enter]保存  [Esc]取消",
        Style::default().fg(theme::DIM),
    )));

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(format!(" {} ", p.title)))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

/// 底部状态条。
pub fn status_bar(f: &mut Frame, area: Rect, text: &str, is_error: bool) {
    let style = if is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(theme::DIM)
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(text.to_string(), style))), area);
}

/// 顶部页签。
///
/// 每个页签前面带序号,并且那个序号就是**能直接按的键**(§8.2)。
/// 只有 Tab 循环的话,从第一页跳到第四页要按三下,而人心里想的是「去第 4 页」。
pub fn tabs(f: &mut Frame, area: Rect, titles: &[&str], selected: usize) {
    let mut spans = vec![Span::raw(" ")];
    for (i, t) in titles.iter().enumerate() {
        let active = i == selected;
        let (num_style, text_style) = if active {
            (
                Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
                Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else {
            (Style::default().fg(theme::TRACK), Style::default().fg(theme::DIM))
        };
        spans.push(Span::styled(format!(" {}", i + 1), num_style));
        spans.push(Span::styled(format!(" {t} "), text_style));
        if i + 1 < titles.len() {
            spans.push(Span::styled("│", Style::default().fg(theme::TRACK)));
        }
    }
    let layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0)])
        .split(area);
    f.render_widget(Paragraph::new(Line::from(spans)), layout[0]);
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
                Field::text("tag", "Tag", "", ""),
                Field::select("proto", "协议", vec!["a".into(), "b".into(), "c".into()], 0, ""),
                Field::text("sni", "SNI", "", ""),
                Field::toggle("v6", "IPv6", false, ""),
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
            vec![Field::text("name", "名称", "", "")],
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
