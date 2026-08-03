//! TUI 的四个页面(DESIGN.md §8.2)。
//!
//! 概览 / 服务管理 / 节点 / 用户。
//!
//! agents 页是**两行式**的:`Row::new(cells).height(2)`,每个 cell 装一个
//! 两行的 `Text`。列宽用 `Constraint::Length`,最后留一列 `Min(0)` 吃掉多余宽度
//! —— 不留的话表格会被拉伸,列宽跟着终端晃。
//!
//! ## 列宽为什么要跟着终端走
//!
//! 固定列宽在窄终端上会**静默截断**:80 列时「每月 22 日重置」被切成「每 」,
//! 界面看起来还很正常,只是少了信息(§13.4 要求专门看这个)。所以节点页和用户页
//! 都按可用宽度**挑列**(`pick`):列表按重要性排,放不下就从尾巴上砍整列。
//! 砍掉一整列是看得见的,压窄一列是看不见的 —— 这就是宁可砍列的理由。

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
    Frame,
};

use super::data::{AgentRow, EventRow, NodeRow, UserRow};
use super::forms;
use super::theme;

fn header(cols: &[&str]) -> Row<'static> {
    Row::new(
        cols.iter()
            .map(|h| {
                Cell::from(h.to_string())
                    .style(Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD))
            })
            .collect::<Vec<_>>(),
    )
}

fn row_style(selected: bool) -> Style {
    if selected {
        Style::default().bg(Color::Rgb(0x2a, 0x2a, 0x2a)).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

/// 按可用宽度挑列:放不下就按 `drop_order` 从前往后整列砍掉。
///
/// 算宽度时必须把 ratatui 在列之间插的间隔算进去(n 列有 n-1 个),
/// 再加左右边框 2 和末尾那根吃余量的空列。漏算的话总宽超出可用空间,
/// ratatui 会**静默压缩**各列 —— 表现就是省略号都放不下、中文被切掉半截。
///
/// `drop_order` 用完了还塞不下(终端被拖到只剩几十列)就从尾巴上继续砍,
/// 直到只剩两列。那个宽度下界面已经没什么用了,但至少不该是一团被压扁的乱码。
fn pick<T: Copy + PartialEq>(total: u16, all: &[T], width: fn(T) -> u16, drop_order: &[T]) -> Vec<T> {
    let mut cols = all.to_vec();
    let fits = |cols: &Vec<T>| {
        let sum: u16 = cols.iter().map(|c| width(*c)).sum();
        // +1 是末尾吃余量的空列,它自身宽 0 但也占一个间隔位。
        sum + cols.len() as u16 + 2 <= total
    };
    for d in drop_order {
        if fits(&cols) || cols.len() <= 2 {
            return cols;
        }
        cols.retain(|c| c != d);
    }
    while !fits(&cols) && cols.len() > 2 {
        cols.pop();
    }
    cols
}

// ─────────────────────────── 概览 ───────────────────────────

/// 概览页。回答的是打开界面第一秒想知道的三件事:
/// 有没有机器掉线、流量烧到哪儿了、刚才发生过什么。
///
/// 最后那一项(最近事件)是刻意放进来的:数字不对时第一个该看的就是它 ——
/// 计数器重置、配额自动停用、配置下发失败都记在 `agent_events` 里,
/// 而在此之前这张表只能靠 `sqlite3` 手查。
pub fn dashboard(
    f: &mut Frame,
    area: Rect,
    agents: &[AgentRow],
    nodes: &[NodeRow],
    users: &[UserRow],
    events: &[EventRow],
    now: i64,
) {
    let c = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(6), Constraint::Length(8)])
        .split(area);

    summary(f, c[0], agents, nodes, users);

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(c[1]);
    top_users(f, mid[0], users, now);
    agent_quotas(f, mid[1], agents);

    recent_events(f, c[2], events);
}

fn summary(f: &mut Frame, area: Rect, agents: &[AgentRow], nodes: &[NodeRow], users: &[UserRow]) {
    let online = agents.iter().filter(|a| a.status == "online").count();
    let offline = agents.iter().filter(|a| a.status == "offline").count();
    let never = agents.len() - online - offline;

    let enabled = users.iter().filter(|u| u.enabled).count();
    let auto_off = users.iter().filter(|u| !u.enabled && u.auto_disabled).count();
    let manual_off = users.iter().filter(|u| !u.enabled && !u.auto_disabled).count();

    // 速率只把**有读数**的 agent 算进来。把 None 当 0 会让「刚打开界面」
    // 和「全网都闲着」看起来一样(data.rs 里同一个理由)。
    let known = agents.iter().filter(|a| a.up_per_sec.is_some()).count();
    let up: f64 = agents.iter().filter_map(|a| a.up_per_sec).sum();
    let down: f64 = agents.iter().filter_map(|a| a.down_per_sec).sum();
    let speed = if known == 0 {
        Span::styled("↑ --   ↓ --", Style::default().fg(theme::DIM))
    } else {
        Span::styled(
            format!("↑ {}   ↓ {}", theme::rate(up), theme::rate(down)),
            Style::default().fg(theme::UP),
        )
    };

    let nic: i64 = agents.iter().map(|a| a.used()).sum();
    let billed: i64 = users.iter().map(|u| u.used()).sum();

    let lines = vec![
        Line::from(vec![
            Span::raw("  被控服务器  "),
            Span::styled(
                format!("{:<5}", agents.len()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("● 在线 {online}   "), Style::default().fg(theme::ONLINE)),
            Span::styled(format!("● 离线 {offline}   "), Style::default().fg(theme::OFFLINE)),
            Span::styled(format!("○ 从未连接 {never}"), Style::default().fg(theme::NEVER)),
        ]),
        Line::from(vec![
            Span::raw("  节点 / 用户  "),
            Span::styled(
                format!("{} 个 · {} 人   ", nodes.len(), users.len()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("启用 {enabled}   "), Style::default().fg(theme::ONLINE)),
            Span::styled(format!("自动停用 {auto_off}   "), Style::default().fg(theme::NEVER)),
            Span::styled(format!("手动停用 {manual_off}"), Style::default().fg(theme::OFFLINE)),
        ]),
        Line::from(vec![Span::raw("  当前网速    "), speed]),
        Line::from(vec![
            Span::raw("  本周期用量  "),
            Span::styled(format!("网卡 {}   ", theme::bytes(nic)), Style::default().fg(theme::DOWN)),
            Span::styled(format!("计费 {}", theme::bytes(billed)), Style::default().fg(theme::ACCENT)),
            // 两个数字口径不同,不写清楚会被当成对不上的 bug(§6.4 / §7.2)。
            Span::styled(
                "   (网卡 = 机器进出总量;计费 = 各用户用量 × 倍率)",
                Style::default().fg(theme::DIM),
            ),
        ]),
    ];

    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" 概况 ")),
        area,
    );
}

fn top_users(f: &mut Frame, area: Rect, users: &[UserRow], now: i64) {
    let mut top: Vec<&UserRow> = users.iter().collect();
    top.sort_by_key(|u| std::cmp::Reverse(u.used()));
    let rows = area.height.saturating_sub(2) as usize;
    let bar_w = (area.width.saturating_sub(40)).clamp(0, 18) as usize;

    let mut lines: Vec<Line> = Vec::new();
    if top.is_empty() {
        lines.push(Line::from(Span::styled(
            "  还没有用户。按 [4] 去用户页,再按 [a] 建一个。",
            Style::default().fg(theme::DIM),
        )));
    }
    for u in top.into_iter().take(rows) {
        let mut spans = vec![
            Span::styled(
                format!("  {}", theme::pad(&u.name, 14)),
                Style::default().fg(state_color(u)),
            ),
            Span::raw(format!("{:>10}  ", theme::bytes(u.used()))),
        ];
        match u.quota_ratio() {
            Some(r) => {
                if bar_w >= 4 {
                    spans.extend(theme::gradient_bar(r, bar_w));
                }
                spans.push(Span::styled(
                    format!(" {:>3.0}%", r * 100.0),
                    Style::default().fg(theme::gradient_at(r)),
                ));
            }
            // 不限流量的行**不画条**:一根满条读起来是「用完了」,正好相反(§8.2)。
            None => spans.push(Span::styled(" 不限流量", Style::default().fg(theme::DIM))),
        }
        if let Some(ts) = u.expire_at {
            let d = forms::days_until(ts, now);
            if d < 0 {
                spans.push(Span::styled("  已过期", Style::default().fg(Color::Red)));
            } else if d <= 7 {
                spans.push(Span::styled(format!("  {d} 天后到期"), Style::default().fg(theme::NEVER)));
            }
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" 用量 Top ")),
        area,
    );
}

fn agent_quotas(f: &mut Frame, area: Rect, agents: &[AgentRow]) {
    let rows = area.height.saturating_sub(2) as usize;
    let bar_w = (area.width.saturating_sub(34)).clamp(0, 14) as usize;

    let mut lines: Vec<Line> = Vec::new();
    if agents.is_empty() {
        lines.push(Line::from(Span::styled(
            "  还没有被控服务器。按 [2] 去服务管理页,再按 [a] 加一台。",
            Style::default().fg(theme::DIM),
        )));
    }
    for a in agents.iter().take(rows) {
        let (dot, color) = match a.status.as_str() {
            "online" => ("●", theme::ONLINE),
            "offline" => ("●", theme::OFFLINE),
            _ => ("○", theme::NEVER),
        };
        let mut spans = vec![
            Span::styled(format!("  {dot} "), Style::default().fg(color)),
            Span::raw(theme::pad(&a.name, 12)),
            Span::raw(format!("{:>10}  ", theme::bytes(a.used()))),
        ];
        match a.quota_ratio() {
            Some(r) => {
                if bar_w >= 4 {
                    spans.extend(theme::gradient_bar(r, bar_w));
                }
                spans.push(Span::styled(
                    format!(" {:>3.0}%", r * 100.0),
                    Style::default().fg(theme::gradient_at(r)),
                ));
            }
            None => spans.push(Span::styled(" 不限流量", Style::default().fg(theme::DIM))),
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" 被控服务器 ")),
        area,
    );
}

/// 事件类型 →(中文名, 颜色)。库里的 kind 是英文标识,直接显示等于让人去猜。
fn event_style(kind: &str) -> (&'static str, Color) {
    match kind {
        "auth_failed" => ("认证失败", Color::Red),
        "config_apply_failed" => ("下发失败", Color::Red),
        "config_build_failed" => ("组装失败", Color::Red),
        "user_auto_disabled" => ("自动停用", theme::NEVER),
        "user_auto_enabled" => ("自动恢复", theme::ONLINE),
        "user_cycle_reset" => ("周期重置", theme::ONLINE),
        "counter_reset" => ("计数器重置", theme::DOWN),
        "nic_counter_reset" => ("网卡计数重置", theme::DOWN),
        "box_event" => ("agent 事件", theme::DIM),
        other => {
            // 认不出来的 kind 也要显示出来,不能吃掉整行:
            // 新加的事件类型忘了在这里登记时,至少还能看到它发生了。
            let _ = other;
            ("事件", theme::DIM)
        }
    }
}

fn recent_events(f: &mut Frame, area: Rect, events: &[EventRow]) {
    let rows = area.height.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    if events.is_empty() {
        lines.push(Line::from(Span::styled(
            "  还没有事件。上线/掉线、计数器重置、配额自动停用都会记在这里。",
            Style::default().fg(theme::DIM),
        )));
    }
    for e in events.iter().take(rows) {
        let (label, color) = event_style(&e.kind);
        lines.push(Line::from(vec![
            Span::styled(format!("  {}  ", short_time(e.at)), Style::default().fg(theme::DIM)),
            Span::styled(theme::pad(label, 13), Style::default().fg(color)),
            Span::styled(
                theme::pad(e.agent_name.as_deref().unwrap_or("—"), 13),
                Style::default().fg(theme::DIM),
            ),
            Span::raw(e.message.clone()),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" 最近事件 "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn short_time(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "??".into())
}

fn state_color(u: &UserRow) -> Color {
    match (u.enabled, u.auto_disabled) {
        (true, _) => theme::ONLINE,
        (false, true) => theme::NEVER,
        (false, false) => theme::OFFLINE,
    }
}

// ─────────────────────────── agents(两行式)───────────────────────────

/// 「流量」列在宽屏下的目标宽度。
const TRAFFIC_COL: u16 = 38;

/// agents 页的列宽。进度条是**可牺牲**的那一项:
/// 重置日是信息,进度条只是同一份信息的图形化。
#[derive(Clone, Copy)]
struct Cols {
    name: u16,
    ip: u16,
    speed: u16,
    traffic: u16,
    /// 0 = 窄到画不下,只显示重置日文字。
    bar: usize,
}

/// 「每月 22 日重置」按终端列宽算约 14 列,留两格余量。
const RESET_LABEL_COLS: u16 = 16;

fn columns(total_width: u16) -> Cols {
    let avail = total_width.saturating_sub(2 + 4);
    const IDEAL: (u16, u16, u16, u16) = (18, 22, 14, TRAFFIC_COL);
    const NARROW: (u16, u16, u16, u16) = (14, 18, 13, 0);
    let narrow_fixed = NARROW.0 + NARROW.1 + NARROW.2;

    let (name, ip, speed, traffic) = if avail >= IDEAL.0 + IDEAL.1 + IDEAL.2 + IDEAL.3 {
        IDEAL
    } else if avail > narrow_fixed {
        (NARROW.0, NARROW.1, NARROW.2, (avail - narrow_fixed).min(TRAFFIC_COL))
    } else {
        let unit = avail / 4;
        (unit, unit, unit, avail.saturating_sub(unit * 3))
    };

    let bar = traffic.saturating_sub(RESET_LABEL_COLS).min(20) as usize;
    Cols { name, ip, speed, traffic, bar: if bar < 4 { 0 } else { bar } }
}

pub fn agents(f: &mut Frame, area: Rect, rows: &[AgentRow], selected: usize) {
    let c = columns(area.width);
    let table_rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let (dot, dot_color, state_text) = match a.status.as_str() {
                "online" => ("●", theme::ONLINE, "在线"),
                "offline" => ("●", theme::OFFLINE, "离线"),
                // 「从没连上过」单独一个颜色:它几乎总是配置问题(token 贴错、
                // 防火墙没开),和「连过又断了」的排查方向完全不同(§8.2)。
                _ => ("○", theme::NEVER, "从未连接"),
            };

            let name_w = c.name.saturating_sub(2) as usize;
            let name_cell = Cell::from(Text::from(vec![
                Line::from(vec![
                    Span::styled(format!("{dot} "), Style::default().fg(dot_color)),
                    Span::raw(theme::truncate(&a.name, name_w)),
                ]),
                Line::from(Span::styled(
                    format!("  {}", theme::truncate(a.agent_version.as_deref().unwrap_or(state_text), name_w)),
                    Style::default().fg(theme::DIM),
                )),
            ]));

            // IPv6 在列表里**不加方括号** —— 那只是 URL 场景的要求(§8.2)。
            let ip_w = c.ip.saturating_sub(1) as usize;
            let ip_cell = Cell::from(Text::from(vec![
                Line::from(theme::truncate(a.ipv4.as_deref().unwrap_or("—"), ip_w)),
                Line::from(Span::styled(
                    theme::truncate(a.ipv6.as_deref().unwrap_or("—"), ip_w),
                    Style::default().fg(theme::DIM),
                )),
            ]));

            let speed_cell = Cell::from(Text::from(vec![
                Line::from(Span::styled(
                    match a.up_per_sec {
                        Some(v) => format!("↑ {}", theme::rate(v)),
                        None => "↑ --".into(),
                    },
                    Style::default().fg(theme::UP),
                )),
                Line::from(Span::styled(
                    match a.down_per_sec {
                        Some(v) => format!("↓ {}", theme::rate(v)),
                        None => "↓ --".into(),
                    },
                    Style::default().fg(theme::DOWN),
                )),
            ]));

            // 流量列:第一行是数字,第二行是进度条 + 重置日。
            //
            // **没有配额时不画进度条**(§8.2)。画一根满条会被读成「用完了」,
            // 而实际含义正相反 —— 是「不限」。
            let used = theme::bytes(a.used());
            let reset = Span::styled(
                format!(" {}", reset_label(a.nic_reset_day)),
                Style::default().fg(theme::DIM),
            );
            let (line1, line2): (Line, Line) = match a.quota_ratio() {
                Some(ratio) if c.bar > 0 => {
                    let quota = theme::bytes(a.nic_quota_bytes.unwrap_or(0));
                    let mut bar = vec![Span::raw(" ")];
                    bar.extend(theme::gradient_bar(ratio, c.bar));
                    bar.push(reset);
                    (Line::from(format!("{used} / {quota}")), Line::from(bar))
                }
                // 有配额但画不下条:百分比改用文字,挪到第二行接在重置日前面。
                Some(ratio) => {
                    let quota = theme::bytes(a.nic_quota_bytes.unwrap_or(0));
                    (
                        Line::from(format!("{used}/{quota}")),
                        Line::from(vec![
                            Span::styled(
                                format!(" {:.0}%", ratio * 100.0),
                                Style::default().fg(theme::gradient_at(ratio)),
                            ),
                            reset,
                        ]),
                    )
                }
                None => (
                    Line::from(vec![
                        Span::raw(used),
                        Span::styled(" · 不限流量", Style::default().fg(theme::DIM)),
                    ]),
                    Line::from(reset),
                ),
            };

            Row::new(vec![
                name_cell,
                ip_cell,
                speed_cell,
                Cell::from(Text::from(vec![line1, line2])),
                Cell::from(""),
            ])
            .height(2)
            .style(row_style(i == selected))
        })
        .collect();

    f.render_widget(
        Table::new(
            table_rows,
            [
                Constraint::Length(c.name),
                Constraint::Length(c.ip),
                Constraint::Length(c.speed),
                Constraint::Length(c.traffic),
                Constraint::Min(0), // 吃掉余下宽度,别让上面几列被拉伸
            ],
        )
        .header(header(&["名称 / 版本", "IP 地址", "网速", "流量", ""]))
        .block(Block::default().borders(Borders::ALL).title(" 服务管理 ")),
        area,
    );
}

fn reset_label(day: Option<i64>) -> String {
    match day {
        Some(d) if (1..=31).contains(&d) => format!("每月 {d} 日重置"),
        _ => "无需重置".into(),
    }
}

// ─────────────────────────── nodes ───────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum NCol {
    Id,
    Agent,
    Tag,
    Proto,
    Port,
    Users,
    Param,
    Relay,
    Export,
}

fn ncol_width(c: NCol) -> u16 {
    match c {
        NCol::Id => 4,
        NCol::Agent => 14,
        NCol::Tag => 18,
        NCol::Proto => 14,
        NCol::Port => 6,
        NCol::Users => 6,
        NCol::Param => 22,
        NCol::Relay => 18,
        NCol::Export => 6,
    }
}

fn ncol_title(c: NCol) -> &'static str {
    match c {
        NCol::Id => "#",
        NCol::Agent => "所属服务器",
        NCol::Tag => "Tag",
        NCol::Proto => "协议",
        NCol::Port => "端口",
        NCol::Users => "用户数",
        NCol::Param => "SNI / path",
        NCol::Relay => "中转",
        NCol::Export => "导出",
    }
}

/// 砍列顺序:先砍图形化/次要的,`Tag` 和协议永远留着 —— 没有它们这张表就没用了。
const NCOL_ALL: [NCol; 9] = [
    NCol::Id,
    NCol::Agent,
    NCol::Tag,
    NCol::Proto,
    NCol::Port,
    NCol::Users,
    NCol::Param,
    NCol::Relay,
    NCol::Export,
];
const NCOL_DROP: [NCol; 5] = [NCol::Export, NCol::Relay, NCol::Param, NCol::Id, NCol::Agent];

pub fn nodes(f: &mut Frame, area: Rect, rows: &[NodeRow], selected: usize) {
    // 底部详情面板:列里放不下的东西(完整的 SNI、中转地址)在这里给全。
    let c = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .split(area);

    let cols = pick(c[0].width, &NCOL_ALL, ncol_width, &NCOL_DROP);
    let table_rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let cells: Vec<Cell> = cols
                .iter()
                .map(|col| match col {
                    NCol::Id => Cell::from(n.id.to_string()),
                    NCol::Agent => Cell::from(theme::truncate(&n.agent_name, 13)),
                    NCol::Tag => Cell::from(theme::truncate(&n.tag, 17)),
                    NCol::Proto => Cell::from(n.protocol.clone()),
                    NCol::Port => Cell::from(n.listen_port.to_string()),
                    NCol::Users => Cell::from(n.user_count.to_string()),
                    NCol::Param => Cell::from(theme::truncate(&node_param(n), 21)),
                    NCol::Relay => match relay_label(n) {
                        Some(l) => Cell::from(theme::truncate(&format!("→ {l}"), 17))
                            .style(Style::default().fg(theme::DOWN)),
                        None => Cell::from("—").style(Style::default().fg(theme::DIM)),
                    },
                    NCol::Export => {
                        if n.params.ipv6 {
                            Cell::from("IPv6").style(Style::default().fg(theme::DOWN))
                        } else {
                            Cell::from("IPv4").style(Style::default().fg(theme::DIM))
                        }
                    }
                })
                .chain(std::iter::once(Cell::from("")))
                .collect();
            Row::new(cells).style(row_style(i == selected))
        })
        .collect();

    let constraints: Vec<Constraint> = cols
        .iter()
        .map(|c| Constraint::Length(ncol_width(*c)))
        .chain(std::iter::once(Constraint::Min(0)))
        .collect();
    let titles: Vec<&str> = cols.iter().map(|c| ncol_title(*c)).chain(std::iter::once("")).collect();

    f.render_widget(
        Table::new(table_rows, constraints)
            .header(header(&titles))
            .block(Block::default().borders(Borders::ALL).title(" 节点 ")),
        c[0],
    );
    f.render_widget(
        Paragraph::new(node_detail(rows.get(selected)))
            .block(Block::default().borders(Borders::ALL).title(" 详情 "))
            .wrap(Wrap { trim: false }),
        c[1],
    );
}

/// 该协议下最要紧的那个参数。SNI 与 path 不会同时存在(见 forms::uses_*)。
fn node_param(n: &NodeRow) -> String {
    let p = crate::model::node::Protocol::parse(&n.protocol);
    if forms::uses_sni(p) {
        n.params.server_name.clone().unwrap_or_else(|| "—".into())
    } else if forms::uses_path(p) {
        n.params.path.clone().unwrap_or_else(|| "—".into())
    } else {
        "—".into()
    }
}

fn relay_label(n: &NodeRow) -> Option<String> {
    if !n.params.relay.is_enabled() {
        return None;
    }
    Some(match n.params.relay.port {
        Some(p) => format!("{}:{}", n.params.relay.host, p),
        None => format!("{}:{}", n.params.relay.host, n.listen_port),
    })
}

fn node_detail(n: Option<&NodeRow>) -> Vec<Line<'static>> {
    let Some(n) = n else {
        return vec![Line::from(Span::styled(
            "  还没有节点。按 [a] 建一个。",
            Style::default().fg(theme::DIM),
        ))];
    };
    // **只渲染人填的那几项。** params 里还有 reality 私钥、证书私钥、ss 服务端密钥,
    // 它们不得出现在界面上(§11.3,model/node.rs 有同样的提醒)。
    let p = crate::model::node::Protocol::parse(&n.protocol);
    let mut spans = vec![
        Span::styled("  #", Style::default().fg(theme::DIM)),
        Span::styled(n.id.to_string(), Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(" {} · {} · 监听 {} · 在 {} 上", n.tag, n.protocol, n.listen_port, n.agent_name)),
    ];
    if forms::uses_sni(p) {
        spans.push(Span::styled(
            format!("  server_name: {}", n.params.server_name.as_deref().unwrap_or("(未设,下发时取默认)")),
            Style::default().fg(theme::ACCENT),
        ));
    }
    if forms::uses_path(p) {
        spans.push(Span::styled(
            format!("  path: {}", n.params.path.as_deref().unwrap_or("(未设,下发时取默认)")),
            Style::default().fg(theme::ACCENT),
        ));
    }

    let mut second = vec![Span::raw("  订阅导出:")];
    match relay_label(n) {
        Some(l) => second.push(Span::styled(
            format!(" 中转 {l}(客户端连这里,不是节点自身端口)"),
            Style::default().fg(theme::DOWN),
        )),
        None => second.push(Span::raw(format!(" {} 的 {}", n.agent_name, if n.params.ipv6 { "IPv6" } else { "IPv4" }))),
    }
    if n.params.port_reuse {
        second.push(Span::styled("  · 端口复用(导出端口固定 443)", Style::default().fg(theme::DIM)));
    }

    vec![Line::from(spans), Line::from(second)]
}

// ─────────────────────────── users ───────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum UCol {
    Name,
    State,
    Up,
    Down,
    Usage,
    Bar,
    Reset,
    Expire,
    Mult,
    Nodes,
}

fn ucol_width(c: UCol) -> u16 {
    match c {
        UCol::Name => 13,
        UCol::State => 11,
        UCol::Up => 10,
        UCol::Down => 10,
        UCol::Usage => 20,
        UCol::Bar => 10,
        UCol::Reset => 6,
        // 「2025-12-04 3天」按终端列宽算 14(中文占两格),留一格余量。
        // 少一格的表现是「天」被切掉,而那正是这一列多出来的那点信息。
        UCol::Expire => 15,
        UCol::Mult => 6,
        UCol::Nodes => 5,
    }
}

fn ucol_title(c: UCol) -> &'static str {
    match c {
        UCol::Name => "用户",
        UCol::State => "状态",
        UCol::Up => "上行",
        UCol::Down => "下行",
        UCol::Usage => "本周期计费用量",
        UCol::Bar => "",
        UCol::Reset => "重置",
        UCol::Expire => "到期",
        UCol::Mult => "倍率",
        UCol::Nodes => "节点",
    }
}

const UCOL_ALL: [UCol; 10] = [
    UCol::Name,
    UCol::State,
    UCol::Up,
    UCol::Down,
    UCol::Usage,
    UCol::Bar,
    UCol::Reset,
    UCol::Expire,
    UCol::Mult,
    UCol::Nodes,
];
/// 砍列顺序。倍率最先走(多数部署一直是 1.0),上下行拆分次之 ——
/// 它们的和已经在「用量」列里了。**进度条排在到期之后**:到期是信息,条只是图形。
const UCOL_DROP: [UCol; 6] = [UCol::Mult, UCol::Reset, UCol::Up, UCol::Down, UCol::Bar, UCol::Expire];

pub fn users(f: &mut Frame, area: Rect, rows: &[UserRow], selected: usize, sub_base: &str, now: i64) {
    let c = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(4)])
        .split(area);

    let cols = pick(c[0].width, &UCOL_ALL, ucol_width, &UCOL_DROP);
    let today = forms::today_day(now);

    let table_rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(i, u)| {
            // 「系统自动停用」与「管理员手动停用」必须分开显示:
            // 前者会在月重置日自动恢复,后者不会(§6.3)。看不出区别的话,
            // 管理员会以为自己关掉的用户下个月又自己开了。
            let (mark, color) = match (u.enabled, u.auto_disabled) {
                (true, _) => ("● 启用", theme::ONLINE),
                (false, true) => ("◐ 自动停用", theme::NEVER),
                (false, false) => ("○ 手动停用", theme::OFFLINE),
            };
            let cells: Vec<Cell> = cols
                .iter()
                .map(|col| match col {
                    UCol::Name => Cell::from(theme::truncate(&u.name, 13)),                    UCol::State => {
                        Cell::from(Span::styled(mark.to_string(), Style::default().fg(color)))
                    }
                    UCol::Up => Cell::from(Span::styled(
                        theme::bytes(u.cycle_up),
                        Style::default().fg(theme::UP),
                    )),
                    UCol::Down => Cell::from(Span::styled(
                        theme::bytes(u.cycle_down),
                        Style::default().fg(theme::DOWN),
                    )),
                    UCol::Usage => Cell::from(if u.quota_bytes > 0 {
                        format!("{} / {}", theme::bytes(u.used()), theme::bytes(u.quota_bytes))
                    } else {
                        format!("{} · 不限", theme::bytes(u.used()))
                    }),
                    UCol::Bar => Cell::from(Line::from(match u.quota_ratio() {
                        Some(r) => theme::gradient_bar(r, ucol_width(UCol::Bar) as usize - 2),
                        // 不限流量的行不画条(§8.2)。
                        None => vec![Span::styled("—", Style::default().fg(theme::DIM))],
                    })),
                    UCol::Reset => {
                        let label = forms::reset_day_label(u.reset_day);
                        // 今天正好是重置日 → 标出来。用量突然归零时,
                        // 第一反应是「统计坏了」,而这一格能立刻解释清楚。
                        if u.reset_day == Some(today as i64) {
                            Cell::from(Span::styled(label, Style::default().fg(theme::ACCENT)))
                        } else {
                            Cell::from(label)
                        }
                    }
                    UCol::Expire => Cell::from(expire_cell(u.expire_at, now)),
                    UCol::Mult => Cell::from(format!("{:.1}x", u.traffic_multiplier)),
                    UCol::Nodes => {
                        let n = u.node_count();
                        if n == 0 {
                            // 一个都没分配的用户订阅是空的 —— 这是最容易忽略的配置错。
                            Cell::from(Span::styled("0", Style::default().fg(theme::NEVER)))
                        } else {
                            Cell::from(n.to_string())
                        }
                    }
                })
                .chain(std::iter::once(Cell::from("")))
                .collect();
            Row::new(cells).style(row_style(i == selected))
        })
        .collect();

    let constraints: Vec<Constraint> = cols
        .iter()
        .map(|c| Constraint::Length(ucol_width(*c)))
        .chain(std::iter::once(Constraint::Min(0)))
        .collect();
    let titles: Vec<&str> = cols.iter().map(|c| ucol_title(*c)).chain(std::iter::once("")).collect();

    f.render_widget(
        Table::new(table_rows, constraints)
            .header(header(&titles))
            .block(Block::default().borders(Borders::ALL).title(" 用户 ")),
        c[0],
    );
    f.render_widget(
        Paragraph::new(user_detail(rows.get(selected), sub_base))
            .block(Block::default().borders(Borders::ALL).title(" 详情 "))
            .wrap(Wrap { trim: false }),
        c[1],
    );
}

/// 到期列。**带上「还剩几天」** —— 光一个日期要人自己心算,
/// 而「3 天后」这种事恰恰是要提前知道的。
fn expire_cell(expire_at: Option<i64>, now: i64) -> Line<'static> {
    let Some(ts) = expire_at else {
        return Line::from(Span::styled("永久", Style::default().fg(theme::DIM)));
    };
    let d = forms::days_until(ts, now);
    let (text, color) = if d < 0 {
        ("已过期".to_string(), Color::Red)
    } else if d <= 7 {
        (format!("{} {}天", forms::fmt_date(ts), d), theme::NEVER)
    } else {
        (forms::fmt_date(ts), Color::Reset)
    };
    Line::from(Span::styled(text, Style::default().fg(color)))
}

fn user_detail(u: Option<&UserRow>, sub_base: &str) -> Vec<Line<'static>> {
    let Some(u) = u else {
        return vec![Line::from(Span::styled(
            "  还没有用户。按 [a] 建一个。",
            Style::default().fg(theme::DIM),
        ))];
    };
    let sub = if sub_base.is_empty() {
        format!("/sub/{}(配置里没填 subscription.public_base,只能给出路径)", u.sub_token)
    } else {
        format!("{}/sub/{}", sub_base.trim_end_matches('/'), u.sub_token)
    };
    vec![
        Line::from(vec![
            Span::styled("  #", Style::default().fg(theme::DIM)),
            Span::styled(u.id.to_string(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!(
                " {} · 上行 {} · 下行 {} · 倍率 {:.1}x · 已分配 {} 个节点",
                u.name,
                theme::bytes(u.cycle_up),
                theme::bytes(u.cycle_down),
                u.traffic_multiplier,
                u.node_count()
            )),
        ]),
        Line::from(vec![
            Span::raw("  订阅: "),
            Span::styled(sub, Style::default().fg(theme::ACCENT)),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const NOW: i64 = 1_764_547_200; // 2025-12-01 前后,测试里只当一个固定基准用

    #[test]
    fn reset_label_covers_the_null_case() {
        assert_eq!(reset_label(Some(22)), "每月 22 日重置");
        assert_eq!(reset_label(None), "无需重置");
        // 库里存了越界的值时不该显示「每月 99 日重置」。
        assert_eq!(reset_label(Some(0)), "无需重置");
        assert_eq!(reset_label(Some(32)), "无需重置");
    }

    // ─────────── 渲染快照 ───────────
    //
    // 用 TestBackend 把界面画进一块内存 buffer,再断言上面的字符。
    // 这组测试守的是**布局本身**:两行式、进度条画没画、不限流量时不画条。
    // 光看代码看不出这些 —— ratatui 的 `Row::height(2)` 少写一个,
    // 表格会静默退化成单行,第二行内容直接消失。

    fn agent(quota: Option<i64>, day: Option<i64>) -> AgentRow {
        AgentRow {
            id: 1,
            name: "tokyo-1".into(),
            token_prefix: "abcd1234".into(),
            status: "online".into(),
            agent_version: Some("v0.1.0".into()),
            ipv4: Some("203.0.113.8".into()),
            ipv6: Some("2001:db8:1:aaaa:1234:5678:9abc:def0".into()),
            nic_quota_bytes: quota,
            nic_reset_day: day,
            cycle_rx: 34 * 1_073_741_824,
            cycle_tx: 0,
            up_per_sec: Some(8_600.0),
            down_per_sec: Some(6_900.0),
            node_count: 2,
        }
    }

    fn node() -> NodeRow {
        NodeRow {
            id: 1,
            agent_id: 1,
            agent_name: "tokyo-1".into(),
            tag: "tokyo-reality".into(),
            protocol: "vless-reality".into(),
            listen_port: 8443,
            user_count: 3,
            params: crate::model::node::NodeParams {
                server_name: Some("www.apple.com".into()),
                ..Default::default()
            },
        }
    }

    fn user() -> UserRow {
        UserRow {
            id: 1,
            name: "alice".into(),
            enabled: true,
            auto_disabled: false,
            quota_bytes: 100 * 1_073_741_824,
            cycle_up: 20 * 1_073_741_824,
            cycle_down: 55 * 1_073_741_824,
            traffic_multiplier: 1.0,
            expire_at: Some(NOW + 86_400 * 40),
            reset_day: Some(22),
            node_ids: vec![1, 2],
            sub_token: "tok".into(),
        }
    }

    fn render_agents(rows: &[AgentRow]) -> String {
        draw_to_string(120, 12, |f| agents(f, f.area(), rows, 0))
    }

    /// 把一帧画进内存 buffer 再抠成字符串。
    ///
    /// **中文字符在 buffer 里占两个 cell**:第一个放字,第二个是占位。
    /// 直接把每个 cell 的 symbol 接起来会在中文之间插入空格
    /// (「服 务 管 理」),所以断言中文时要走下面的 `flat()`。
    fn draw_to_string(w: u16, h: u16, f: impl FnOnce(&mut Frame)) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(f).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 去掉全部空白后再比 —— 用于含中文的断言(见 `draw_to_string` 的说明)。
    fn flat(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn has_cjk(haystack: &str, needle: &str) -> bool {
        flat(haystack).contains(&flat(needle))
    }

    /// 一行 agent 占**两行**,四列的第二行内容都要出现(§8.2 的表格)。
    #[test]
    fn agent_row_renders_two_lines() {
        let out = render_agents(&[agent(Some(500 * 1_073_741_824), Some(22))]);
        assert!(out.contains("tokyo-1"), "{out}");
        assert!(out.contains("v0.1.0"), "第二行的版本号没画出来\n{out}");
        assert!(out.contains("203.0.113.8"), "{out}");
        assert!(out.contains("2001:db8:1"), "第二行的 IPv6 没画出来\n{out}");
        assert!(out.contains('↑') && out.contains('↓'), "上下行速率\n{out}");
        assert!(has_cjk(&out, "每月 22 日重置"), "重置日被截断了\n{out}");
    }

    /// 有配额 → 画进度条;`nic_quota_bytes IS NULL` → **不画**(§8.2)。
    ///
    /// 这条是刻意与 `1.png` 不同的地方:参考图在「不限流量」的行上也画了一根满条,
    /// 而一根满条读起来就是「用完了」—— 与「不限」正好相反。
    #[test]
    fn progress_bar_only_appears_when_there_is_a_quota() {
        let with_quota = render_agents(&[agent(Some(500 * 1_073_741_824), Some(22))]);
        assert!(with_quota.contains('█'), "有配额时应当画进度条\n{with_quota}");
        assert!(with_quota.contains("GB / "), "应当是「已用 / 配额」\n{with_quota}");

        let unlimited = render_agents(&[agent(None, None)]);
        assert!(!unlimited.contains('█'), "不限流量时不该画进度条\n{unlimited}");
        assert!(has_cjk(&unlimited, "不限流量"), "{unlimited}");
        assert!(has_cjk(&unlimited, "无需重置"), "{unlimited}");
    }

    /// 还没有两次采样时速率显示 `--`,不是 0 —— 0 会被读成「这台机器闲着」。
    #[test]
    fn missing_speed_renders_as_dashes() {
        let mut a = agent(None, None);
        a.up_per_sec = None;
        a.down_per_sec = None;
        let out = render_agents(&[a]);
        assert!(out.contains("↑ --") && out.contains("↓ --"), "{out}");
    }

    #[test]
    fn long_addresses_are_truncated() {
        let out = render_agents(&[agent(None, None)]);
        assert!(out.contains('…'), "超宽的 IPv6 应当被截断\n{out}");
    }

    #[test]
    fn empty_agent_list_renders_headers_only() {
        let out = render_agents(&[]);
        assert!(has_cjk(&out, "服务管理"), "{out}");
        assert!(has_cjk(&out, "网速"), "{out}");
    }

    /// 终端被拉到极窄时不能 panic。ratatui 对越界写入是直接 panic 的,
    /// 而「窗口拖小了整个程序就崩」是最难看的一类 bug。
    #[test]
    fn narrow_terminals_do_not_panic() {
        for (w, h) in [(1u16, 1u16), (10, 3), (30, 5), (60, 10), (200, 60)] {
            draw_to_string(w, h, |f| agents(f, f.area(), &[agent(Some(1000), Some(1))], 0));
            draw_to_string(w, h, |f| nodes(f, f.area(), &[node()], 0));
            draw_to_string(w, h, |f| users(f, f.area(), &[user()], 0, "https://x.example", NOW));
            draw_to_string(w, h, |f| {
                dashboard(f, f.area(), &[agent(None, None)], &[node()], &[user()], &[], NOW)
            });
        }
    }

    /// §13.4:80 列的窄终端下,**重置日不能被截断**,IPv6 要留住省略号。
    #[test]
    fn narrow_terminal_keeps_the_reset_day_intact() {
        let out = draw_to_string(80, 8, |f| {
            agents(f, f.area(), &[agent(Some(500 * 1_073_741_824), Some(22))], 0)
        });
        assert!(has_cjk(&out, "每月 22 日重置"), "80 列下重置日被截断了:\n{out}");
        assert!(out.contains('…'), "IPv6 应当截断并保留省略号:\n{out}");
        assert!(out.contains('█'), "80 列下仍应画得下进度条:\n{out}");
    }

    #[test]
    fn column_widths_always_fit() {
        for w in 20..200u16 {
            let c = columns(w);
            let used = c.name + c.ip + c.speed + c.traffic;
            let avail = w.saturating_sub(2 + 4);
            assert!(used <= avail.max(1) || avail == 0, "宽度 {w}:列合计 {used} > 可用 {avail}");
        }
    }

    #[test]
    fn percentage_moves_to_text_when_the_bar_does_not_fit() {
        let out = draw_to_string(70, 6, |f| {
            agents(f, f.area(), &[agent(Some(100 * 1_073_741_824), Some(22))], 0)
        });
        assert!(!out.contains('█'), "这么窄不该画条:\n{out}");
        assert!(out.contains("34%"), "画不下条时要给文字百分比:\n{out}");
        assert!(has_cjk(&out, "每月 22 日重置"), "重置日仍要在:\n{out}");
    }

    /// 挑列的结果必须始终塞得进可用宽度(含列间隔与边框),
    /// 否则 ratatui 会静默压缩各列 —— 那正是这套机制要避免的。
    #[test]
    fn picked_columns_always_fit() {
        for w in 20..200u16 {
            let cols = pick(w, &NCOL_ALL, ncol_width, &NCOL_DROP);
            let total: u16 =
                cols.iter().map(|c| ncol_width(*c)).sum::<u16>() + cols.len() as u16 + 2;
            assert!(total <= w || cols.len() <= 2, "节点页宽度 {w}:合计 {total} > {w}");

            let cols = pick(w, &UCOL_ALL, ucol_width, &UCOL_DROP);
            let total: u16 =
                cols.iter().map(|c| ucol_width(*c)).sum::<u16>() + cols.len() as u16 + 2;
            assert!(total <= w || cols.len() <= 2, "用户页宽度 {w}:合计 {total} > {w}");
        }
    }

    /// 120 列(最常见的宽度)下**每一列都该在**。掉列是给窄终端的退让,
    /// 不该在正常宽度上就发生 —— 那样倍率、重置日这些就永远看不到了。
    #[test]
    fn a_normal_terminal_shows_every_column() {
        assert_eq!(pick(120, &UCOL_ALL, ucol_width, &UCOL_DROP).len(), UCOL_ALL.len());
        assert_eq!(pick(120, &NCOL_ALL, ncol_width, &NCOL_DROP).len(), NCOL_ALL.len());
    }

    /// 砍列是有顺序的:80 列下倍率该没了,用户名和状态必须还在。
    #[test]
    fn narrow_user_table_keeps_the_important_columns() {
        let cols = pick(80, &UCOL_ALL, ucol_width, &UCOL_DROP);
        assert!(cols.contains(&UCol::Name) && cols.contains(&UCol::State));
        assert!(cols.contains(&UCol::Usage), "用量是这张表存在的理由");
        assert!(!cols.contains(&UCol::Mult), "80 列下倍率该被砍掉");
    }

    /// 「系统自动停用」和「管理员手动停用」必须能一眼分开(§6.3):
    /// 前者到了重置日会自己恢复,后者不会。
    #[test]
    fn user_page_distinguishes_auto_from_manual_disable() {
        let base = UserRow { enabled: false, auto_disabled: true, quota_bytes: 0, ..user() };
        let render =
            |u: UserRow| draw_to_string(120, 8, move |f| users(f, f.area(), &[u], 0, "", NOW));

        assert!(has_cjk(&render(base.clone()), "自动停用"));
        assert!(has_cjk(&render(UserRow { auto_disabled: false, ..base.clone() }), "手动停用"));
        assert!(has_cjk(&render(UserRow { enabled: true, ..base }), "启用"));
    }

    /// 快到期要提前看得见 —— 只给一个日期等于让人自己心算。
    #[test]
    fn expiry_within_a_week_is_called_out() {
        let soon = UserRow { expire_at: Some(NOW + 86_400 * 3), ..user() };
        let out = draw_to_string(120, 8, |f| users(f, f.area(), &[soon], 0, "", NOW));
        assert!(has_cjk(&out, "3天"), "快到期应当显示还剩几天:\n{out}");

        let gone = UserRow { expire_at: Some(NOW - 10), ..user() };
        let out = draw_to_string(120, 8, |f| users(f, f.area(), &[gone], 0, "", NOW));
        assert!(has_cjk(&out, "已过期"), "{out}");

        let forever = UserRow { expire_at: None, ..user() };
        let out = draw_to_string(120, 8, |f| users(f, f.area(), &[forever], 0, "", NOW));
        assert!(has_cjk(&out, "永久"), "{out}");
    }

    /// 一个节点都没分配的用户,订阅是空的 —— 这一格必须显眼。
    #[test]
    fn users_without_nodes_are_visible() {
        let u = UserRow { node_ids: vec![], ..user() };
        let out = draw_to_string(120, 8, |f| users(f, f.area(), &[u], 0, "", NOW));
        assert!(has_cjk(&out, "已分配 0 个节点"), "详情里要说清楚:\n{out}");
    }

    /// 详情面板给的是订阅地址,**不是**任何凭据。
    #[test]
    fn user_detail_shows_the_subscription_url() {
        let out = draw_to_string(120, 8, |f| {
            users(f, f.area(), &[user()], 0, "https://sub.example.com/", NOW)
        });
        assert!(out.contains("https://sub.example.com/sub/tok"), "{out}");
    }

    /// 节点页要能看出「客户端到底会连到哪儿」:中转配了就显示中转。
    #[test]
    fn node_page_shows_where_clients_actually_connect() {
        let mut n = node();
        n.params.relay = crate::model::node::RelaySetting {
            host: "198.51.100.9".into(),
            port: Some(12345),
        };
        let out = draw_to_string(120, 8, |f| nodes(f, f.area(), &[n], 0));
        assert!(out.contains("198.51.100.9:12345"), "中转落点要显示出来:\n{out}");
        assert!(has_cjk(&out, "客户端连这里"), "{out}");
    }

    /// 节点详情**绝不能**渲染密钥材料。这条测试守的是 §11.3。
    #[test]
    fn node_detail_never_leaks_key_material() {
        let mut n = node();
        n.params.private_key = Some("PRIVATE-KEY-MUST-NOT-APPEAR".into());
        n.params.key_pem = Some("KEY-PEM-MUST-NOT-APPEAR".into());
        n.params.ss_password = Some("SS-PASSWORD-MUST-NOT-APPEAR".into());
        let out = draw_to_string(200, 20, |f| nodes(f, f.area(), &[n], 0));
        assert!(!out.contains("MUST-NOT-APPEAR"), "密钥材料被画到界面上了:\n{out}");
    }

    /// 概览页要能一眼看出有机器掉线、有人快超额。
    #[test]
    fn dashboard_summarises_the_cluster() {
        let mut offline = agent(None, None);
        offline.id = 2;
        offline.name = "osaka-2".into();
        offline.status = "offline".into();
        let events = vec![EventRow {
            at: NOW,
            agent_name: Some("tokyo-1".into()),
            kind: "counter_reset".into(),
            message: "计数器重置,按全量入账".into(),
        }];
        let out = draw_to_string(120, 24, |f| {
            dashboard(f, f.area(), &[agent(Some(1000), Some(1)), offline], &[node()], &[user()], &events, NOW)
        });
        assert!(has_cjk(&out, "在线 1"), "{out}");
        assert!(has_cjk(&out, "离线 1"), "{out}");
        assert!(out.contains("alice"), "用量 Top 里要有用户:\n{out}");
        assert!(has_cjk(&out, "计数器重置"), "最近事件要显示中文类型:\n{out}");
    }

    /// 空库时概览页要给出「下一步按什么」,而不是四个空框。
    #[test]
    fn empty_dashboard_tells_you_what_to_do_next() {
        let out = draw_to_string(120, 24, |f| dashboard(f, f.area(), &[], &[], &[], &[], NOW));
        assert!(has_cjk(&out, "按 [2] 去服务管理页"), "{out}");
        assert!(has_cjk(&out, "按 [4] 去用户页"), "{out}");
    }

    /// 把四个页面画出来打到 stdout,给人看一眼:
    ///
    /// ```sh
    /// cargo test tui::pages::tests::preview -- --nocapture
    /// ```
    ///
    /// 布局问题(列宽不够、中文被截、进度条压到边界)读代码看不出来,
    /// 而 CI 和很多开发环境里又起不了真的 TTY。这个 test 不断言任何东西,
    /// 它的价值是**让人能看到**改动的效果。
    #[test]
    fn preview() {
        let agents_rows = vec![
            agent(None, None),
            AgentRow {
                id: 2,
                name: "osaka-2".into(),
                status: "offline".into(),
                ipv4: Some("198.51.100.7".into()),
                ipv6: Some("2001:db8:2:3525:aaaa:bbbb:cccc:dddd".into()),
                nic_quota_bytes: Some(500 * 1_073_741_824),
                nic_reset_day: Some(22),
                cycle_rx: 30 * 1_073_741_824,
                cycle_tx: 4 * 1_073_741_824,
                up_per_sec: Some(14_400.0),
                down_per_sec: Some(13_900.0),
                ..agent(None, None)
            },
            AgentRow {
                id: 3,
                name: "从未连接的机器".into(),
                status: "never".into(),
                agent_version: None,
                ipv4: None,
                ipv6: None,
                nic_quota_bytes: Some(100 * 1_073_741_824),
                nic_reset_day: Some(1),
                cycle_rx: 96 * 1_073_741_824,
                cycle_tx: 0,
                up_per_sec: None,
                down_per_sec: None,
                ..agent(None, None)
            },
        ];
        let node_rows = vec![
            node(),
            NodeRow {
                id: 2,
                agent_id: 2,
                agent_name: "osaka-2".into(),
                tag: "osaka-hy2".into(),
                protocol: "hysteria2".into(),
                listen_port: 8444,
                user_count: 1,
                params: crate::model::node::NodeParams {
                    ipv6: true,
                    relay: crate::model::node::RelaySetting {
                        host: "relay.example.com".into(),
                        port: Some(20443),
                    },
                    ..Default::default()
                },
            },
        ];
        let user_rows = vec![
            user(),
            UserRow {
                id: 2,
                name: "bob".into(),
                enabled: false,
                auto_disabled: true,
                quota_bytes: 50 * 1_073_741_824,
                cycle_up: 30 * 1_073_741_824,
                cycle_down: 25 * 1_073_741_824,
                traffic_multiplier: 1.0,
                expire_at: Some(NOW + 86_400 * 3),
                reset_day: None,
                node_ids: vec![],
                sub_token: "tok2".into(),
            },
        ];
        let events = vec![
            EventRow {
                at: NOW,
                agent_name: Some("tokyo-1".into()),
                kind: "counter_reset".into(),
                message: "agent 重启,计数器归零,本次按全量入账".into(),
            },
            EventRow {
                at: NOW - 300,
                agent_name: None,
                kind: "user_auto_disabled".into(),
                message: "用户 bob 配额用尽,已自动停用".into(),
            },
        ];

        println!(
            "\n── 概览 ──\n{}\n",
            draw_to_string(120, 24, |f| dashboard(
                f,
                f.area(),
                &agents_rows,
                &node_rows,
                &user_rows,
                &events,
                NOW
            ))
        );
        println!("── 服务管理 ──\n{}\n", draw_to_string(120, 9, |f| agents(f, f.area(), &agents_rows, 1)));
        println!("── 节点 ──\n{}\n", draw_to_string(120, 9, |f| nodes(f, f.area(), &node_rows, 1)));
        println!(
            "── 用户 ──\n{}\n",
            draw_to_string(120, 9, |f| users(f, f.area(), &user_rows, 0, "https://sub.example.com", NOW))
        );
        println!(
            "── 用户(80 列)──\n{}\n",
            draw_to_string(80, 9, |f| users(f, f.area(), &user_rows, 0, "https://sub.example.com", NOW))
        );
    }
}
