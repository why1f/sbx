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
    symbols::Marker,
    text::{Line, Span, Text},
    widgets::{Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table},
    Frame,
};
use std::collections::VecDeque;

use super::data::{self, AgentRow, BreakdownRow, NodeRow, UserRow};
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
        Style::default().bg(theme::ROW_BG).add_modifier(Modifier::BOLD)
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

// ─────────────────────────── 仪表盘 ───────────────────────────

/// 仪表盘。三块,从上到下按「多久看一次」排:
///
///   1. 概况 —— 一眼扫,回答「有没有东西挂了」;
///   2. 网速折线(盲文点阵)—— 趋势,回答「现在忙不忙、什么时候忙」;
///   3. 用量 Top / 各机器 —— 明细,回答「谁在烧、烧在哪台」。
///
/// 折线图在矮终端上整块让掉:一条挤成两行的曲线不如把地方给下面的条形和数字。
/// 仪表盘要画的东西。打成一个结构体而不是摊成一串参数:
/// 三张表 + 历史 + 时间 + 焦点已经是 6 个,再摊下去调用处就只能靠位置记谁是谁。
pub struct Dash<'a> {
    pub agents: &'a [AgentRow],
    pub nodes: &'a [NodeRow],
    pub users: &'a [UserRow],
    pub history: &'a VecDeque<(f64, f64)>,
    pub now: i64,
    /// `(焦点是否在左边那栏, 该栏选中第几行)`。`None` = 没有选中态。
    pub focus: Option<(bool, usize)>,
}

pub fn dashboard(f: &mut Frame, area: Rect, d: &Dash<'_>) {
    let Dash { agents, nodes, users, history, now, focus } = *d;
    let chart_h = if area.height >= 24 { 9 } else { 0 };
    let c = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Length(chart_h), Constraint::Min(4)])
        .split(area);

    summary(f, c[0], agents, nodes, users);
    if chart_h > 0 {
        net_charts(f, c[1], history);
    }

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(c[2]);
    let (u_sel, n_sel) = match focus {
        Some((true, i)) => (Some(i), None),
        Some((false, i)) => (None, Some(i)),
        None => (None, None),
    };
    top_users(f, mid[0], users, now, u_sel);
    // 右边是**节点视图**,不是被控机视图。每台机器的网卡明细搬去了服务管理页的
    // 二级页面(按 Enter)—— 网卡是整机口径,和这里两张表用的用户口径不是一回事,
    // 摆在同一屏上并排比只会让人把两个数字当成同一个数字的两次统计(§6.4)。
    top_nodes(f, mid[1], nodes, !agents.is_empty(), n_sel);
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
    let speed: Vec<Span> = if known == 0 {
        vec![Span::styled("↑ --   ↓ --", Style::default().fg(theme::DIM))]
    } else {
        vec![
            Span::styled(format!("↑ {}", theme::rate(up)), Style::default().fg(theme::UP)),
            Span::raw("   "),
            Span::styled(format!("↓ {}", theme::rate(down)), Style::default().fg(theme::DOWN)),
        ]
    };

    let billed: i64 = users.iter().map(|u| u.used()).sum();

    let mut head = vec![
        Span::raw("  被控服务器  "),
        Span::styled(format!("{:<5}", agents.len()), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("● 在线 {online}   "), Style::default().fg(theme::ONLINE)),
        Span::styled(format!("● 离线 {offline}   "), Style::default().fg(theme::OFFLINE)),
    ];
    // 「从未连接」为 0 时不占地方 —— 一排 0 会把真正要看的数字挤走。
    if never > 0 {
        head.push(Span::styled(format!("○ 从未连接 {never}"), Style::default().fg(theme::NEVER)));
    }

    let mut line2 = vec![
        Span::raw("  节点 / 用户  "),
        Span::styled(
            format!("{} 个 · {} 人   ", nodes.len(), users.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("启用 {enabled}   "), Style::default().fg(theme::ONLINE)),
    ];
    if auto_off > 0 {
        line2.push(Span::styled(format!("自动停用 {auto_off}   "), Style::default().fg(theme::NEVER)));
    }
    if manual_off > 0 {
        line2.push(Span::styled(format!("手动停用 {manual_off}"), Style::default().fg(theme::INACTIVE)));
    }

    let mut speed_line = vec![Span::raw("  当前网速    ")];
    speed_line.extend(speed);

    // 网卡是整机进出(含系统更新、别的服务、协议开销)、从加入集群那一刻从 0 计,
    // 而用户用量是 sing-box tracker 记的账、含倍率、会被月重置清零。两个口径不同,
    // 放一起就是两套账,而概况回答的是「这个月用户用了多少」——
    // 厂商账单上的那个数看服务管理页,或拆开查各台机器的网卡明细(§6.4 / §7.2)。
    let billed_up: i64 = users.iter().map(|u| u.billed_up()).sum();
    let billed_down: i64 = users.iter().map(|u| u.billed_down()).sum();
    let usage = vec![
        Span::raw("  本周期用量  "),
        Span::styled(format!("↑ {}", theme::bytes(billed_up)), Style::default().fg(theme::UP)),
        Span::raw("  "),
        Span::styled(format!("↓ {}", theme::bytes(billed_down)), Style::default().fg(theme::DOWN)),
        Span::styled("   总计 ", Style::default().fg(theme::DIM)),
        Span::styled(theme::bytes(billed), Style::default().fg(theme::ACCENT)),
    ];

    f.render_widget(
        Paragraph::new(vec![
            Line::from(head),
            Line::from(line2),
            Line::from(speed_line),
            Line::from(usage),
        ])
        .block(Block::default().borders(Borders::ALL).title(" 概况 ")),
        area,
    );
}

/// 上下行两张盲文点阵折线图。
///
/// 一个点 = **一次刷新**(1s),120 点 ≈ 两分钟,与 sb-manager 同一个节奏。
/// 底层读数每 30 秒才变一次,所以曲线是阶梯状的 —— 那是这个数字本来的分辨率
/// (30 秒平均值,§8.2),不是渲染的毛病。
///
/// 纵轴上限取历史最大值的 1.2 倍,保底 1 KB:固定上限会让小流量时的曲线永远贴着底,
/// 而那时人恰恰想看的是「有没有波动」。
fn net_charts(f: &mut Frame, area: Rect, history: &VecDeque<(f64, f64)>) {
    let cc = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let up: Vec<(f64, f64)> = history.iter().enumerate().map(|(i, (u, _))| (i as f64, *u)).collect();
    let down: Vec<(f64, f64)> =
        history.iter().enumerate().map(|(i, (_, d))| (i as f64, *d)).collect();
    let cur_up = history.back().map(|(u, _)| *u).unwrap_or(0.0);
    let cur_down = history.back().map(|(_, d)| *d).unwrap_or(0.0);

    // 峰值写进标题。它原来在 y 轴 labels 里,而设 labels 会让 ratatui
    // 腾出一列并画一条竖线 —— 那条线看起来像图里多出来的一组数据。
    let peak = |pick: fn(&(f64, f64)) -> f64| {
        history.iter().map(pick).fold(0.0_f64, f64::max)
    };
    net_chart(
        f,
        cc[0],
        &up,
        theme::UP,
        format!(" ↑ 上行  {}   峰值 {} ", theme::rate(cur_up), theme::rate(peak(|(u, _)| *u))),
    );
    net_chart(
        f,
        cc[1],
        &down,
        theme::DOWN,
        format!(" ↓ 下行  {}   峰值 {} ", theme::rate(cur_down), theme::rate(peak(|(_, d)| *d))),
    );
}

fn net_chart(f: &mut Frame, area: Rect, data: &[(f64, f64)], color: Color, title: String) {
    if data.len() < 2 {
        // 一个点画不成线。直接说「在攒数据」,而不是画一张空图让人以为坏了。
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  还在攒数据(每秒一个点)",
                Style::default().fg(theme::DIM),
            )))
            .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
        return;
    }
    let y_max = data.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max).max(1024.0) * 1.2;

    let datasets = vec![Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(color))
        .data(data)];
    f.render_widget(
        Chart::new(datasets)
            .block(Block::default().borders(Borders::ALL).title(title))
            // 横轴跟着**当前点数**走,和 sb-manager 一样:曲线永远铺满整幅图宽,
            // 攒满 120 点(两分钟)之后就变成一个往左滚的窗口。
            //
            // v0.3.6 这里按容量(固定 120)画过一版,是为了躲开「3 个点被拉成
            // 一条横贯屏幕的斜线」。但那个毛病的根子在**采样太稀**(30 秒一个点),
            // 已经在 data.rs 里改成每次刷新一个点了 —— 每秒进一个点,
            // 图在几秒内就密起来,不再需要靠留白来遮丑。留白反而让图大半时间是空的。
            .x_axis(Axis::default().bounds([0.0, ((data.len() - 1) as f64).max(1.0)]))
            // **不设 y 轴 labels。** 设了 ratatui 会为标签腾出一列并画一条竖线,
            // 那条竖线看起来像图里多出来的一组数据(用户就是这么报的)。
            // 上限改写进标题,那里本来就有一行字。
            .y_axis(Axis::default().bounds([0.0, y_max])),
        area,
    );
}

/// 上下行那一对数字的显示宽度。
///
/// `theme::bytes` 最长吐 10 列(`1023.99 MB`),所以数字栏就是 10 ——
/// 早先按 9 算,一旦落进 1000~1024 那一段(`1012.98 MB`)就撑破格子,
/// 把右边的进度条和百分比整体推歪。
const BYTES_COLS: usize = 10;

/// 一对「↑ x  ↓ y」占多少列。`(箭头 + 空格 + 数字 + 分隔空格) × 2`。
const TRAFFIC_PAIR_COLS: usize = (1 + 1 + BYTES_COLS + 1) * 2;

/// 「名字 + 倍率标记」这一段,整体占 `total` 列。
///
/// **标记紧跟名字,空位补在标记右边。** 反过来(先把名字补满再接标记)
/// 会让短名字后面空出一大片、标记反而贴到右边的箭头上 ——
/// `admin     [2.0x] ↑ 5.26 MB` 里那个标记看起来是在解释箭头,
/// 而它解释的是左边那个名字。
///
/// 两个 span 而不是一个字符串:名字跟着状态色(启用/停用),标记恒定是灰的。
fn name_with_mult(name: &str, mult: &str, total: usize, name_style: Style) -> Vec<Span<'static>> {
    // 名字最多占「总宽 - 标记 - 中间那一格」,长了截断(带 `…`,看得出来)。
    let name_w = total.saturating_sub(mult.chars().count() + 1);
    let shown = theme::truncate(name, name_w);
    // 补的空格数按**实际画出来的宽度**算,不是按 `name_w` ——
    // 名字短的时候两者差很多,按后者补会把标记又推回右边去。
    let pad = total.saturating_sub(theme::cols(&shown) + 1 + mult.chars().count());
    vec![
        Span::styled(shown, name_style),
        Span::styled(format!(" {mult}{}", " ".repeat(pad)), Style::default().fg(theme::DIM)),
    ]
}

/// 渲染「↑ 上行  ↓ 下行」这一对。
///
/// **数字左对齐,不是右对齐。** 右对齐(早先的 `↑{:>9}`)会让箭头到数字的
/// 间距随数字长度变:8 位的 `20.00 GB` 前面空一格,9 位的 `110.00 GB` 一格不空。
/// 而下行通常比上行大一位,于是同一行里系统性地「上行离箭头远、下行贴着箭头」,
/// 看起来像两栏没对齐。左对齐后箭头后面恒定一个空格,数字起点也恒定 ——
/// 右边那些列照样对得齐,因为整块宽度是固定的。
fn traffic_pair(up: i64, down: i64) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("↑ {:<width$} ", theme::bytes(up), width = BYTES_COLS),
            Style::default().fg(theme::UP),
        ),
        Span::styled(
            format!("↓ {:<width$} ", theme::bytes(down), width = BYTES_COLS),
            Style::default().fg(theme::DOWN),
        ),
    ]
}

/// 仪表盘「用量 Top」的行序:按计费用量从大到小。
///
/// 单独抽出来是因为**渲染和「Enter 打开谁」必须用同一份次序**。
/// 两边各排一次的话,光标停在第 2 行、打开的却是另一个用户 ——
/// 而这种错不会报任何错,只会给出一张属于别人的明细表。
pub fn dashboard_user_order(users: &[UserRow]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..users.len()).collect();
    idx.sort_by_key(|&i| std::cmp::Reverse(users[i].used()));
    idx
}

/// 仪表盘「节点用量」的行序:按本周期用量从大到小。理由同上。
pub fn dashboard_node_order(nodes: &[NodeRow]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..nodes.len()).collect();
    idx.sort_by_key(|&i| std::cmp::Reverse(nodes[i].cycle_up.saturating_add(nodes[i].cycle_down)));
    idx
}

/// 仪表盘用户视图里「名字 + 倍率标记」整段的宽度。
///
/// 比用户页那张表窄 —— 这里只占半屏,而右边还要摆上下行、进度条和百分比。
/// 名字长了会被截,但截掉的名字看得出来(有 `…`),
/// 被挤没的进度条看不出来(§13.4)。
///
/// 名字实际能占的是这个减去标记和中间那一格,即 10 列。
const NAME_MULT_COLS: usize = 10 + 1 + data::MULT_TAG_COLS;

fn top_users(f: &mut Frame, area: Rect, users: &[UserRow], now: i64, sel: Option<usize>) {
    let order = dashboard_user_order(users);
    let top: Vec<&UserRow> = order.iter().map(|&i| &users[i]).collect();
    let rows = area.height.saturating_sub(2) as usize;
    // 进度条从内框宽度往回算:缩进 + 名字段 + 上下行那一对 + 百分比,
    // 剩下的才是条的地方。拍一个常数的话,标记或数字栏一改宽度就对不上,
    // 而对不上的表现是最右边的百分比被静默切掉(§13.4)。
    //
    // 名字段右边再留一格 —— 标记和箭头之间总得有个缝。
    let fixed = 2 + NAME_MULT_COLS + 1 + TRAFFIC_PAIR_COLS + PCT_LABEL_COLS as usize;
    // **内框宽**,不是 `area.width` —— 后者含左右两条边框。
    // 多算这 2 列的表现就是最右边的百分比被切掉(`100%` 变成 `10`)。
    let inner = area.width.saturating_sub(2) as usize;
    let bar_w = inner.saturating_sub(fixed).clamp(0, 16);

    let mut lines: Vec<Line> = Vec::new();
    if top.is_empty() {
        lines.push(Line::from(Span::styled(
            "  还没有用户。按 [4] 去用户页,再按 [a] 建一个。",
            Style::default().fg(theme::DIM),
        )));
    }
    for (i, u) in top.into_iter().take(rows).enumerate() {
        let picked = sel == Some(i);
        let mut spans = vec![Span::raw(if picked { "▸ " } else { "  " })];
        // 倍率标记紧跟名字。摆在这里而不是行尾:这两栏上下行是**乘过倍率的**,
        // 标记要挨着它解释的那个数字,否则看到 `↓ 132 MB` 的人无从知道
        // 那已经是 ×2 之后的值(§6.3)。
        spans.extend(name_with_mult(
            &u.name,
            &u.mult_tag(),
            NAME_MULT_COLS,
            Style::default().fg(state_color(u)),
        ));
        spans.extend(traffic_pair(u.billed_up(), u.billed_down()));
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
            None => spans.push(Span::styled(" 不限", Style::default().fg(theme::DIM))),
        }
        if let Some(ts) = u.expire_at {
            let d = forms::days_until(ts, now);
            let tail = if d < 0 {
                Some(("  已过期".to_string(), Color::Red))
            } else if d <= 7 {
                Some((format!("  {d} 天后到期"), theme::NEVER))
            } else {
                None
            };
            // 放得下才接上去。让 `Paragraph` 去截会切出「3」这种半截提示 ——
            // 比不显示更糟:它看起来像一个数字,而不是被切掉的一句话。
            if let Some((text, color)) = tail {
                let used: usize = spans.iter().map(|s| theme::cols(&s.content)).sum();
                if used + theme::cols(&text) + 2 <= area.width as usize {
                    spans.push(Span::styled(text, Style::default().fg(color)));
                }
            }
        }
        let line = Line::from(spans);
        lines.push(if picked { line.style(Style::default().bg(theme::ROW_BG)) } else { line });
    }

    let title = if sel.is_some() { " 用量 Top(Enter 看明细) " } else { " 用量 Top " };
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

/// 主机指标多久算过期。上报周期是 30s,门槛要留出余量 ——
/// 卡在 30s 上会让正常的网络抖动表现成「CPU 数字一闪一闪地消失」。
const HOST_METRICS_STALE_AFTER: i64 = 90;

/// 仪表盘右下角:各节点本周期跑了多少,按用量排。
///
/// 条形画的是**占全部节点总量的份额**,不是配额比例 —— 节点没有配额,
/// 这里回答的问题是「量都堆在哪个节点上」。tag 后面必须跟机器名:
/// tag 在不同机器上可以重名,只有 tag 的两行会长得一模一样。
fn top_nodes(
    f: &mut Frame,
    area: Rect,
    nodes: &[NodeRow],
    has_agents: bool,
    sel: Option<usize>,
) {
    let order = dashboard_node_order(nodes);
    let top: Vec<&NodeRow> = order.iter().map(|&i| &nodes[i]).collect();
    let rows = area.height.saturating_sub(2) as usize;
    let total: i64 = nodes.iter().map(|n| n.cycle_up.saturating_add(n.cycle_down)).sum();

    // 宽度从内框往回算,而不是拍几个常数:`tag@机器名` 比用户名长得多,
    // 而这一栏只占半屏。算错一格的后果是 `%` 被悄悄切掉,看起来像
    // 「份额是 0」而不是「这里显示不下」。
    //
    // **这里没有进度条,而且不该有。** 进度条的含义是「用了多少 / 上限多少」,
    // 而节点没有上限 —— 配额是用户和整机网卡两个层面的事(§6.3)。
    // 早先这里画的是「占全网流量的份额」,可它和别处的配额条长得一模一样,
    // 于是一个跑了 90% 流量的健康节点看起来像「快爆了」。
    // 份额只留那个百分比,并在标题里写明是占比。
    let inner = area.width.saturating_sub(2) as usize;
    let fixed = 2 + TRAFFIC_PAIR_COLS; // 缩进 + 「↑ 上行  ↓ 下行」
    let label_w = inner.saturating_sub(fixed + 5).clamp(8, 22);
    let rest = inner.saturating_sub(fixed + label_w);
    let show_pct = rest >= 5;

    let mut lines: Vec<Line> = Vec::new();
    if top.is_empty() {
        // 一台被控机都没有的时候,让人去节点页是死路 —— 建节点要先选机器。
        // 空状态得指向**真正的下一步**,否则它只是一句看起来有用的废话。
        lines.push(Line::from(Span::styled(
            if has_agents {
                "  还没有节点。按 [3] 去节点页,再按 [a] 建一个。"
            } else {
                "  还没有被控服务器。按 [2] 去服务管理页,再按 [a] 加一台。"
            },
            Style::default().fg(theme::DIM),
        )));
    }
    for (i, n) in top.into_iter().take(rows).enumerate() {
        let picked = sel == Some(i);
        let used = n.cycle_up.saturating_add(n.cycle_down);
        let share = if total > 0 { used as f64 / total as f64 } else { 0.0 };
        let mut spans = vec![
            Span::raw(format!(
                "{}{}",
                if picked { "▸ " } else { "  " },
                theme::pad(&format!("{}@{}", n.tag, n.agent_name), label_w)
            )),
        ];
        spans.extend(traffic_pair(n.cycle_up, n.cycle_down));
        if show_pct {
            // 份额用中性色。跟着比例变红会把「这个节点承载得多」染成告警,
            // 而它恰恰是正常的 —— 只有配额才有「超了」这回事。
            spans.push(Span::styled(
                format!("{:>4.0}%", share * 100.0),
                Style::default().fg(theme::DIM),
            ));
        }
        let line = Line::from(spans);
        lines.push(if picked { line.style(Style::default().bg(theme::ROW_BG)) } else { line });
    }

    let title = if sel.is_some() {
        " 节点用量(% = 占全网份额,Enter 看明细) "
    } else {
        " 节点用量(% = 占全网份额) "
    };
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

/// 网卡明细二级页面的表头补充行(服务管理页按 Enter)。
///
/// 这几行回答的是「这台机器**整机**烧了多少、还剩多少配额」,
/// 而下面的表回答「这台机器上是哪个节点在跑量」。两者口径不同:
/// 网卡是 `/proc/net/dev` 的整机进出(含系统更新、别的服务、协议开销),
/// 节点是 sing-box 记的账。厂商按前者计费,所以前者才是「这个月要付多少钱」
/// 的答案,而后者是「谁在用」的答案(§6.4 / §7.2)。
///
/// 网卡数字从**加入集群那一刻**开始从 0 计,不是从开机算起 —— 否则第一次
/// 上报就会把这台机器开机至今的历史全部计入本周期(v0.3.0 修的就是这个)。
pub fn nic_info(a: &AgentRow, now: i64) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();

    let accounted = a.used();
    out.push(Line::from(vec![
        Span::styled("  网卡本周期  ", Style::default().fg(theme::DIM)),
        // **tx 是上行,rx 是下行**(站在这台被控机上看):`/proc/net/dev` 的
        // Transmit 是它发出去的,Receive 是它收进来的。接反过的后果很隐蔽 ——
        // 平时代理流量两个方向都有,看不出来;只有 agent 自升级这种纯下载的
        // 时刻才露馅(涨的是 ↑)。
        Span::styled(format!("↑ {:<10}", theme::bytes(a.cycle_tx)), Style::default().fg(theme::UP)),
        Span::styled(format!("↓ {:<10}", theme::bytes(a.cycle_rx)), Style::default().fg(theme::DOWN)),
        Span::styled(
            format!("计入({}) ", a.nic_accounting_mode.short()),
            Style::default().fg(theme::DIM),
        ),
        Span::styled(theme::bytes(accounted), Style::default().add_modifier(Modifier::BOLD)),
    ]));

    let mut quota = vec![Span::styled("  配额        ", Style::default().fg(theme::DIM))];
    match (a.quota_ratio(), a.nic_quota_bytes) {
        (Some(r), Some(q)) => {
            quota.extend(theme::gradient_bar(r, 12));
            quota.push(Span::styled(
                format!(" {:>3.0}%  ", r * 100.0),
                Style::default().fg(theme::gradient_at(r)),
            ));
            quota.push(Span::raw(format!("{} / {}  ", theme::bytes(a.used()), theme::bytes(q))));
            // 剩余量单独写出来:百分比在「配额 10 TB、已用 91%」时看着还行,
            // 而真正要紧的是「还剩 900 GB」这个绝对值。
            quota.push(Span::styled(
                format!("剩 {}", theme::bytes((q - a.used()).max(0))),
                Style::default().fg(theme::DIM),
            ));
        }
        // 不限流量的行**不画条**:一根满条读起来是「用完了」,正好相反(§8.2)。
        _ => quota.push(Span::styled("不限流量", Style::default().fg(theme::DIM))),
    }
    out.push(Line::from(quota));

    let (off, off_src) = a.nic_offset(now);
    let mut cycle = vec![
        Span::styled("  重置        ", Style::default().fg(theme::DIM)),
        Span::raw(format!("每月 {}", forms::reset_day_label(a.nic_reset_day))),
    ];
    if let Some(d) = a.nic_reset_day.filter(|d| (1..=31).contains(d)) {
        use chrono::Datelike;
        // **按这台自己的时区算「今天几号」**,不是 TUI 进程的时区。
        // 用 `forms::today_day` 的话,一台 UTC-7 的机器在主控的傍晚会被说成
        // 「就是今天」,而它当地还要等好几个小时才翻月 —— 弹窗自己和边界打架。
        let today = chrono::DateTime::from_timestamp(now, 0)
            .map(|dt| dt.with_timezone(&off).day() as i64)
            .unwrap_or(0);
        // 同一天就是「今天」,否则算到下一个该日子还有几天(跨月按 30 天估)。
        let days = if d == today {
            0
        } else if d > today {
            d - today
        } else {
            30 - today + d
        };
        cycle.push(Span::styled(
            if days == 0 { "  (就是今天)".to_string() } else { format!("  (约 {days} 天后)") },
            Style::default().fg(theme::DIM),
        ));
    }
    out.push(Line::from(cycle));

    // 生效时区**和它的来源**都要写出来。只写值不写来源的话,最常见的那种错
    // (VPS 镜像出厂是 UTC,厂商却按机房当地时间计费,于是 agent 老实上报 +00:00)
    // 看起来和「人手工确认过是 UTC」一模一样。
    out.push(Line::from(vec![
        Span::styled("  重置时区    ", Style::default().fg(theme::DIM)),
        Span::raw(crate::tg::fmt::format_offset(off.local_minus_utc())),
        Span::styled(format!("  ({})", off_src.label()), Style::default().fg(theme::DIM)),
    ]));

    let speed = match (a.up_per_sec, a.down_per_sec) {
        (Some(u), Some(d)) => vec![
            Span::styled(format!("↑ {:<10}", theme::rate(u)), Style::default().fg(theme::UP)),
            Span::styled(format!("↓ {}", theme::rate(d)), Style::default().fg(theme::DOWN)),
        ],
        // 没有两次可比的采样就写 `--`。当 0 显示会让「刚打开界面」
        // 和「这台机器闲着」看起来一模一样。
        _ => vec![Span::styled("↑ --        ↓ --", Style::default().fg(theme::DIM))],
    };
    let mut rate = vec![Span::styled("  当前速率    ", Style::default().fg(theme::DIM))];
    rate.extend(speed);
    out.push(Line::from(rate));

    out.push(Line::from(vec![
        Span::styled("  主机        ", Style::default().fg(theme::DIM)),
        Span::styled(host_metrics(a, now, 60), Style::default().fg(theme::DIM)),
    ]));

    out
}

/// 一行 CPU / 内存 / load /(放得下的话)运行时长。
///
/// 指标过期(或从没上报过)时整行是 `--`,**不是 0**:
/// 一台离线三天的机器显示「CPU 0%」看起来和一台闲着的在线机器一模一样。
///
/// 运行时长按**能不能整段放下**决定要不要接上去。让 `Paragraph` 去截会切出
/// 「已 」这种半截词 —— 那正是这一版到处在修的那类问题(§8.3)。
fn host_metrics(a: &AgentRow, now: i64, width: usize) -> String {
    if !a.host_metrics_fresh(now, HOST_METRICS_STALE_AFTER) {
        return "CPU --   内存 --   负载 --".into();
    }
    let cpu = a.cpu_pct.map(|c| format!("{c:.0}%")).unwrap_or_else(|| "--".into());
    let mem = match (a.mem_ratio(), a.mem_used) {
        (Some(r), Some(used)) => format!("{} ({:.0}%)", theme::bytes(used), r * 100.0),
        _ => "--".into(),
    };
    let load = a.load1.map(|l| format!("{l:.2}")).unwrap_or_else(|| "--".into());
    let base = format!("CPU {cpu}   内存 {mem}   负载 {load}");

    match a.uptime_secs.filter(|s| *s > 0) {
        Some(s) => {
            let tail = format!("   已运行 {}", uptime_label(s));
            if theme::cols(&base) + theme::cols(&tail) <= width {
                base + &tail
            } else {
                base
            }
        }
        None => base,
    }
}

/// 秒 → 「3 天 4 小时」。只给两级 —— 「3 天 4 小时 12 分 7 秒」没人会读到最后。
fn uptime_label(secs: i64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d} 天 {h} 小时")
    } else if h > 0 {
        format!("{h} 小时 {m} 分")
    } else {
        format!("{m} 分")
    }
}




fn state_color(u: &UserRow) -> Color {
    match (u.enabled, u.auto_disabled) {
        (true, _) => theme::ONLINE,
        (false, true) => theme::NEVER,
        (false, false) => theme::INACTIVE,
    }
}

// ─────────────────────────── agents(两行式)───────────────────────────

/// 「流量」列在**重置日还跟在它后面**时的宽度(窄屏回退那条路)。
/// 第二行要装下「条 + 百分比 + 每月 22 日重置」。
const TRAFFIC_COL: u16 = 38;

/// 「流量」列在**重置日已经有自己一列**时的宽度。
///
/// 那时第二行只剩「一格缩进 + 条(20) + 百分比(5)」= 26,第一行
/// 「999.99 GB / 999.99 GB」也就 21。再宽出去的部分是纯空白 ——
/// 而空白会把后面的重置/出站/主机白白推远(拆列之后就是这个毛病:
/// 百分比和重置日中间隔着十几格,看起来像两块不相干的东西)。
const TRAFFIC_COL_TIGHT: u16 = 26;
/// 「主机」列(CPU / 内存)。只在**宽到有余量**时才出现 ——
/// 它是最锦上添花的一列,不该从流量或重置日那里抢地方。
const HOST_COL: u16 = 22;

/// 出站策略列。最长的取值是「自动(跟随系统解析)」,但列表里用短名
/// (「自动」/「优先v4」…),10 列够放。
const OUTBOUND_COL: u16 = 10;

/// agents 页的列宽。进度条是**可牺牲**的那一项:
/// 重置日是信息,进度条只是同一份信息的图形化。
#[derive(Clone, Copy)]
struct Cols {
    name: u16,
    ip: u16,
    speed: u16,
    traffic: u16,
    /// 0 = 放不下,整列不画。
    host: u16,
    /// 出站策略列。0 = 放不下,整列不画。
    ///
    /// 它值得占一列而不是只写在底部摘要里:摘要那一行会被操作回执顶掉,
    /// 于是「按完 [o] 想确认改对没有」恰恰是看不到它的那一刻。
    outbound: u16,
    /// 重置日列。0 = 放不下,整列不画。
    ///
    /// 从流量列里拆出来的:原来「进度条 + 每月 22 日重置」挤在同一行,
    /// 那行长得离谱,而这两件事本来无关 —— 一个是「用了多少」,
    /// 一个是「什么时候清零」。
    reset: u16,
    /// 0 = 窄到画不下,改用文字百分比。
    bar: usize,
}

/// 重置日列。两行式(「每月」/「22 日」)只要 8 列;
/// 写成一行的「每月 22 日重置」要 14,那正是它原来把流量那行撑长的原因。
const RESET_COL: u16 = 8;

/// 进度条右边给百分比留的地方(「 100%」= 5 列,留一格余量)。
const PCT_LABEL_COLS: u16 = 6;

/// 窄屏回退时,「每月 22 日重置」跟在百分比后面要占的地方(中文按两列算)。
const RESET_LABEL_COLS: u16 = 15;

/// 完整 IPv6 的最大列宽:八组四位 + 七个冒号 = 39,**不再额外留白**。
///
/// v0.4.15 这里是 40(39 + 1 格列内留白),因为渲染处会 `c.ip - 1`。
/// 但那一格是白花的:ratatui 的 `Table` 本来就在相邻两列之间插 `column_spacing`
/// (默认 1),留白之后地址和网速列之间是**两格**,而别的列都是一格。
/// 代价是整张表多占一列 —— 146 列的终端上正好卡在「七列全在 + 完整 IPv6」的
/// 门槛下面一格,最长的那个地址又从尾巴少掉两个字符(`…febb:5a…`)。
/// 现在按 39 算、渲染处也不再减一,分隔交给 `column_spacing`。
const IPV6_COLS: u16 = 39;

fn columns(total_width: u16) -> Cols {
    // 减掉左右边框(2),以及**基础形态**下 ratatui 在各列之间插的间隔:
    // 四个内容列 + 末尾吃余量的空列 = 五列 = 四个间隔。
    // 每多一个可选列就多一个间隔,那部分在下面逐项扣。
    let avail = total_width.saturating_sub(2 + 4);
    const IDEAL: (u16, u16, u16, u16) = (18, 22, 14, TRAFFIC_COL);
    const NARROW: (u16, u16, u16, u16) = (14, 18, 13, 0);
    let ideal_sum = IDEAL.0 + IDEAL.1 + IDEAL.2 + IDEAL.3;
    let narrow_fixed = NARROW.0 + NARROW.1 + NARROW.2;

    let (name, ip, speed, traffic) = if avail >= ideal_sum {
        IDEAL
    } else if avail > narrow_fixed {
        // 窄屏:三个固定列收一点,余下全给「流量」——那一列信息密度最高。
        (NARROW.0, NARROW.1, NARROW.2, (avail - narrow_fixed).min(TRAFFIC_COL))
    } else {
        // 极窄:连收缩后的固定列都放不下。按比例分,保证**总和不超出** ——
        // 超出的话 ratatui 会自己压缩,那时连省略号都画不出来。
        let unit = avail / 4;
        (unit, unit, unit, avail.saturating_sub(unit * 3))
    };

    // 主机列多占一个列间隔,所以门槛比它自身宽一格。
    // 出站策略列排在主机列**前面**让位:主机那两个数字(CPU/内存)是锦上添花,
    // 而出站策略是一个「改了就得确认」的设置项 —— 窄屏上先保它。
    let outbound = if avail > ideal_sum + OUTBOUND_COL { OUTBOUND_COL } else { 0 };

    // 重置列排在出站之后、主机之前让位:它比 CPU/内存要紧(「这个号什么时候
    // 清零」是运营信息),但比出站策略次要 —— 后者是个能当场改的开关。
    let reset = if avail > ideal_sum + OUTBOUND_COL + RESET_COL { RESET_COL } else { 0 };

    // 重置日搬进自己那一列之后,流量列不必再给那句话留位置 —— **收窄它**。
    // 不收的话右边会空出十几格,把重置/出站/主机整体推远,而百分比和重置日
    // 本来该是挨着的。这一行是那个「太远了」的直接修法。
    let traffic = if reset > 0 { traffic.min(TRAFFIC_COL_TIGHT) } else { traffic };

    // 主机列**按已经定下来的那几列算余量**,不能像上面两列那样独立判断。
    // 独立判断会让它在 121 列凭空出现:那时前面几列已经把宽度吃满,总宽被
    // 撑过边框最多 8 格,而 ratatui 会静默压缩各列 ——「终端明明更宽了,
    // 每一列反而更挤」正是这么来的。`+ 1` 是它自己多占的那个列间隔。
    let fixed = name + ip + speed + traffic + outbound + reset;
    let optional_cols = u16::from(outbound > 0) + u16::from(reset > 0);
    let host = if fixed + HOST_COL + optional_cols < avail { HOST_COL } else { 0 };

    // 进度条用流量列里除去重置日之后剩下的地方。剩不下 4 格就别画了:
    // 三四格的条读不出比例,只是占地方 —— 那时改用文字百分比。
    // 条要给「百分比」和(窄屏回退时)「每月 22 日重置」让地方。
    //
    // 重置列被让掉时那句话回到这一行,它要 14 列 —— 不预留的话条会把它挤出去,
    // 表现是「每月」后面被切掉,而 §13.4 要防的正是这种静默截断。
    let tail = PCT_LABEL_COLS + if reset == 0 { RESET_LABEL_COLS } else { 0 };
    let bar = traffic.saturating_sub(tail).min(20) as usize;

    // **把右边剩下的地方补给 IP 列,让 IPv6 完整显示。**
    //
    // 完整 IPv6 要 39 列,而这一列的常规宽度只有 22 —— 于是
    // `2605:52c0:2:3525:505…` 从**尾巴**被截掉,而尾部恰恰是主机位:
    // 同一个 /64 下的两台机器截完长得一模一样,这一列就白占了。
    //
    // 放在最后做:前面那些「够不够宽就加一列」的判断都基于常规宽度,
    // 先把 IP 撑宽会把出站/重置/主机列挤掉 —— 那几列比「看全 IPv6」更常用。
    //
    // `avail` 开头只按基础形态扣了 4 个间隔,每个可选列还各多占一个。
    // 少扣的那部分不能算进可用余量,否则会把总宽撑过边框、
    // 让 ratatui 静默压缩各列(§13.4 要防的正是这个)。
    let shown_cols = 4 + u16::from(host > 0) + u16::from(outbound > 0) + u16::from(reset > 0);
    let gaps_unaccounted = shown_cols.saturating_sub(4);
    let used = name + ip + speed + traffic + host + outbound + reset + gaps_unaccounted;
    let ip = ip + avail.saturating_sub(used).min(IPV6_COLS.saturating_sub(ip));

    Cols { name, ip, speed, traffic, host, outbound, reset, bar: if bar < 4 { 0 } else { bar } }
}

pub fn agents(f: &mut Frame, area: Rect, rows: &[AgentRow], selected: usize, now: i64) {
    let c = columns(area.width);
    // 一台 agent 占两行；一条终端空行就是用户所说的「半行」间距。
    // 这张表目前没有滚动视口，所以只有全部记录都放得下时才加间距，
    // 否则保持原来的紧凑布局，不能为了好看把选中项挤出屏幕。
    let data_height = area.height.saturating_sub(3) as usize; // 上下边框 + 表头
    let spaced_height = rows.len().saturating_mul(3).saturating_sub(1);
    let add_spacing = !rows.is_empty() && spaced_height <= data_height;
    let mut table_rows: Vec<Row> = Vec::with_capacity(rows.len() * 2);
    for (i, a) in rows.iter().enumerate() {
        // 间隔用一条**独立的空 Row**,不是给上一行加 `bottom_margin`:
        // margin 属于它所在的那一行,选中态的底色会连着盖过去 ——
        // 于是恰恰在正看着的那一台下面,间距被填成一整块实心底色,
        // 白加了一行还看不出分隔。空 Row 没有 style,永远是干净的。
        if add_spacing && i > 0 {
            table_rows.push(Row::new(Vec::<Cell>::new()).height(1));
        }
        {
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
            // 列宽直接用满:列与列之间的分隔由 `column_spacing` 负责(见 IPV6_COLS)。
            let ip_w = c.ip as usize;
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
            // 重置日通常在自己那一列(reset_cell)。但**窄屏上那一列会被让掉**,
            // 而 §13.4 要求它在任何宽度下都看得见 —— 所以那时把它退回流量列的
            // 第二行(就是拆分之前的样子)。
            //
            // 少了这条回退,120 列以下重置日会整个消失:那正是 §13.4 要防的
            // 「静默丢信息」——界面看起来毫无异常,只是少了一项。
            let inline_reset = if c.reset == 0 {
                Some(Span::styled(
                    format!(" {}", reset_text(a.nic_reset_day)),
                    Style::default().fg(theme::DIM),
                ))
            } else {
                None
            };
            let used = theme::bytes(a.used());
            let (line1, line2): (Line, Line) = match a.quota_ratio() {
                Some(ratio) if c.bar > 0 => {
                    let quota = theme::bytes(a.nic_quota_bytes.unwrap_or(0));
                    let mut bar = vec![Span::raw(" ")];
                    bar.extend(theme::gradient_bar(ratio, c.bar));
                    bar.push(Span::styled(
                        format!(" {:.0}%", ratio * 100.0),
                        Style::default().fg(theme::gradient_at(ratio)),
                    ));
                    bar.extend(inline_reset);
                    (Line::from(format!("{used} / {quota}")), Line::from(bar))
                }
                // 有配额但画不下条:百分比改用文字。
                Some(ratio) => {
                    let quota = theme::bytes(a.nic_quota_bytes.unwrap_or(0));
                    let mut second = vec![Span::styled(
                        format!(" {:.0}%", ratio * 100.0),
                        Style::default().fg(theme::gradient_at(ratio)),
                    )];
                    second.extend(inline_reset);
                    (Line::from(format!("{used}/{quota}")), Line::from(second))
                }
                None => {
                    let mut second = vec![Span::styled(" —", Style::default().fg(theme::DIM))];
                    second.extend(inline_reset);
                    (
                        Line::from(vec![
                            Span::raw(used),
                            Span::styled(" · 不限流量", Style::default().fg(theme::DIM)),
                        ]),
                        Line::from(second),
                    )
                }
            };

            // 重置列:两行式,跟着表格本来的两行走。
            let reset_cell = Cell::from(Text::from(match a.nic_reset_day {
                Some(d) if (1..=31).contains(&d) => vec![
                    Line::from(Span::styled("每月", Style::default().fg(theme::DIM))),
                    Line::from(format!("{d} 日")),
                ],
                // 越界值(手改过库)当作没设,而不是显示「每月 99 日」。
                _ => vec![
                    Line::from(Span::styled("不重置", Style::default().fg(theme::DIM))),
                    Line::from(""),
                ],
            }));

            // 主机列:CPU 一行、内存一行。指标过期时是 `--` 而不是 0 ——
            // 一台离线三天的机器显示「CPU 0%」看起来和闲着的在线机器一模一样。
            let fresh = a.host_metrics_fresh(now, HOST_METRICS_STALE_AFTER);
            let host_cell = Cell::from(Text::from(vec![
                Line::from(Span::styled(
                    match (fresh, a.cpu_pct) {
                        (true, Some(v)) => format!("CPU {v:.0}%"),
                        _ => "CPU --".into(),
                    },
                    Style::default().fg(match (fresh, a.cpu_pct) {
                        (true, Some(v)) => theme::gradient_at(v / 100.0),
                        _ => theme::DIM,
                    }),
                )),
                Line::from(Span::styled(
                    match (fresh, a.mem_ratio()) {
                        (true, Some(r)) => format!("内存 {:.0}%", r * 100.0),
                        _ => "内存 --".into(),
                    },
                    Style::default().fg(theme::DIM),
                )),
            ]));

            // 出站策略:第一行是当前值,第二行提示怎么改。
            // 第二行不是废话 —— 这一列是只读的表格,不写的话没人知道
            // 它能改、更不知道按哪个键。
            let outbound_cell = Cell::from(Text::from(vec![
                Line::from(Span::styled(
                    a.outbound.short(),
                    Style::default().fg(if a.outbound == crate::model::outbound::OutboundStrategy::Auto {
                        theme::DIM
                    } else {
                        theme::ACCENT
                    }),
                )),
                Line::from(Span::styled("[o] 改", Style::default().fg(theme::DIM))),
            ]));

            let mut cells = vec![name_cell, ip_cell, speed_cell, Cell::from(Text::from(vec![line1, line2]))];
            if c.reset > 0 {
                cells.push(reset_cell);
            }
            if c.outbound > 0 {
                cells.push(outbound_cell);
            }
            if c.host > 0 {
                cells.push(host_cell);
            }
            cells.push(Cell::from(""));
            table_rows.push(Row::new(cells).height(2).style(row_style(i == selected)));
        }
    }

    let mut constraints = vec![
        Constraint::Length(c.name),
        Constraint::Length(c.ip),
        Constraint::Length(c.speed),
        Constraint::Length(c.traffic),
    ];
    let mut titles = vec!["名称 / 版本", "IP 地址", "网速", "流量"];
    if c.reset > 0 {
        constraints.push(Constraint::Length(c.reset));
        titles.push("重置");
    }
    if c.outbound > 0 {
        constraints.push(Constraint::Length(c.outbound));
        titles.push("出站");
    }
    if c.host > 0 {
        constraints.push(Constraint::Length(c.host));
        titles.push("主机");
    }
    // 最后一列吃掉余下宽度,别让上面几列被拉伸。
    constraints.push(Constraint::Min(0));
    titles.push("");

    f.render_widget(
        Table::new(table_rows, constraints)
            .header(header(&titles))
            .block(Block::default().borders(Borders::ALL).title(" 服务管理 ")),
        area,
    );
}

/// 重置日的一行式说法。**只给窄屏回退用** —— 宽屏上重置日有自己一列,
/// 走的是两行式(「每月」/「22 日」)。
///
/// 越界的值(库被手改过)当作没设,不能显示成「每月 99 日重置」。
fn reset_text(day: Option<i64>) -> String {
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

/// 中转落点撑到头要多少**列宽**:`→ ` 两列 + `[完整IPv6]` 41 列 +
/// `:65535` 6 列 + 1 格列内留白 = 50。
const RELAY_MAX: u16 = 50;
/// SNI / path 的最大列宽(含一格列内留白)。
const PARAM_MAX: u16 = 41;

/// 当前数据真正需要的中转列宽。
///
/// - 全是 `—`:标题 4 列 + 留白 = 5,导出就紧跟过来;
/// - IPv4/域名:只给实际文本所需的宽度;
/// - IPv6:最多撑到 `RELAY_MAX`,这时才保留原先那段大空间。
fn relay_need(rows: &[NodeRow]) -> u16 {
    rows.iter()
        .filter_map(relay_label)
        .map(|s| (theme::cols(&format!("→ {s}")) + 1) as u16)
        .max()
        .unwrap_or(5)
        .clamp(5, RELAY_MAX)
}

fn param_need(rows: &[NodeRow]) -> u16 {
    rows.iter()
        .map(|n| (theme::cols(&node_param(n)) + 1) as u16)
        .max()
        .unwrap_or(11)
        .clamp(ncol_width(NCol::Param), PARAM_MAX)
}

/// 各列的实际宽度。中转列先按内容**收窄**,再只在确实需要时吃余量。
fn ncol_widths(cols: &[NCol], total: u16, relay_want: u16, param_want: u16) -> Vec<u16> {
    let mut w: Vec<u16> = cols
        .iter()
        .map(|c| match c {
            // 先允许缩到内容宽度。没有中转时从 18 收到 5,导出列自然贴过来。
            NCol::Relay => ncol_width(*c).min(relay_want),
            _ => ncol_width(*c),
        })
        .collect();
    let sum: u16 = w.iter().sum();
    let mut spare = total.saturating_sub(sum + cols.len() as u16 + 2);

    // 有 IPv4 时通常只补到 20~22;有 IPv6 时才继续扩到当前终端能给的宽度。
    for (target, want) in [(NCol::Relay, relay_want), (NCol::Param, param_want)] {
        if spare == 0 {
            break;
        }
        if let Some(i) = cols.iter().position(|c| *c == target) {
            let add = spare.min(want.saturating_sub(w[i]));
            w[i] += add;
            spare -= add;
        }
    }
    w
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
    // **没有单独的「详情」面板。** 它显示的东西(tag / 协议 / 端口 / 所属机器)
    // 与底下「操作」面板的第一行逐字重复 —— 同一屏上把同一件事说两遍,
    // 还各占四行。列里放不下的那几项(完整 SNI、中转落点、导出族)
    // 现在归「操作」那一行管(mod.rs::ops_lines)。
    let c = [area];

    let cols = pick(c[0].width, &NCOL_ALL, ncol_width, &NCOL_DROP);
    let relay_want = relay_need(rows);
    let param_want = param_need(rows);
    let widths = ncol_widths(&cols, c[0].width, relay_want, param_want);
    let cell_w = |col: NCol| {
        cols.iter().position(|c| *c == col).map_or(0, |i| widths[i]).saturating_sub(1) as usize
    };
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
                    NCol::Param => Cell::from(theme::truncate(&node_param(n), cell_w(NCol::Param))),
                    NCol::Relay => match relay_label(n) {
                        Some(l) => {
                            Cell::from(theme::truncate(&format!("→ {l}"), cell_w(NCol::Relay)))
                                .style(Style::default().fg(theme::DOWN))
                        }
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

    let constraints: Vec<Constraint> = widths
        .iter()
        .map(|w| Constraint::Length(*w))
        .chain(std::iter::once(Constraint::Min(0)))
        .collect();
    let titles: Vec<&str> = cols.iter().map(|c| ncol_title(*c)).chain(std::iter::once("")).collect();

    f.render_widget(
        Table::new(table_rows, constraints)
            .header(header(&titles))
            .block(Block::default().borders(Borders::ALL).title(" 节点 ")),
        c[0],
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

pub fn relay_label(n: &NodeRow) -> Option<String> {
    if !n.params.relay.is_enabled() {
        return None;
    }
    let host = &n.params.relay.host;
    // IPv6 与端口拼在一起必须加方括号,否则
    // `2600:...:5ad5:40000` 根本分不出哪一段是地址、哪一段是端口。
    let host = match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("[{host}]"),
        _ => host.clone(),
    };
    let port = n.params.relay.port.map(i64::from).unwrap_or(n.listen_port);
    Some(format!("{host}:{port}"))
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
    /// 订阅是否改报网卡流量(§10.3)。
    Nic,
}

fn ucol_width(c: UCol) -> u16 {
    match c {
        UCol::Name => 12,
        // 最长的是「◐ 自动停用」= 1+1+8 = 10 列(◐/●/○ 都是半角)。
        UCol::State => 10,
        // 10 而不是 9:`theme::bytes` 最长吐 10 列(`1012.98 MB` 这种落在
        // 1000~1024 之间的值)。给 9 的话 ratatui 会静默截成 `1012.98 M`,
        // 而「M」和「MB」差 1024 倍 —— 这种截断比对不齐危险得多。
        UCol::Up => 10,
        UCol::Down => 10,
        UCol::Usage => 20,
        UCol::Bar => 8,
        UCol::Reset => 6,
        // 「2025-12-04 3天」按终端列宽算 14(中文占两格),留一格余量。
        // 少一格的表现是「天」被切掉,而那正是这一列多出来的那点信息。
        UCol::Expire => 15,
        // 内容 `2.0x` 和表头「倍率」都正好 4 列。
        UCol::Mult => 4,
        UCol::Nodes => 4,
        UCol::Nic => 8,
    }
}

fn ucol_title(c: UCol) -> &'static str {
    match c {
        UCol::Name => "用户",
        UCol::State => "状态",
        // 「计费」两个字不能省:这两列是乘过倍率的,而人手上往往有一份客户端
        // 自己记的单倍数字 —— 表头不写清口径,对不上时只会当成统计坏了。
        UCol::Up => "计费上行",
        UCol::Down => "计费下行",
        UCol::Usage => "本周期计费用量",
        UCol::Bar => "",
        UCol::Reset => "重置",
        UCol::Expire => "到期",
        UCol::Mult => "倍率",
        UCol::Nodes => "节点",
        UCol::Nic => "订阅口径",
    }
}

const UCOL_ALL: [UCol; 11] = [
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
    UCol::Nic,
];
/// 砍列顺序。倍率最先走(多数部署一直是 1.0),上下行拆分次之 ——
/// 它们的和已经在「用量」列里了。**「订阅口径」排得很靠后**:它标的是
/// 「客户端里看到的数字和这张表不一样」,丢了它就没人知道为什么对不上。
const UCOL_DROP: [UCol; 7] =
    [UCol::Mult, UCol::Reset, UCol::Up, UCol::Down, UCol::Bar, UCol::Nic, UCol::Expire];

pub fn users(f: &mut Frame, area: Rect, rows: &[UserRow], selected: usize, sub_base: &str, now: i64) {
    // 与节点页同理:没有单独的「详情」面板了 —— 它和「操作」面板的第一行
    // (选中谁、分了几个节点、订阅地址)重复。绑网卡那句说明也搬去了那里。
    let _ = sub_base;
    let c = [area];

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
                (false, false) => ("○ 手动停用", theme::INACTIVE),
            };
            let cells: Vec<Cell> = cols
                .iter()
                .map(|col| match col {
                    UCol::Name => Cell::from(theme::truncate(&u.name, 11)),
                    UCol::State => {
                        Cell::from(Span::styled(mark.to_string(), Style::default().fg(color)))
                    }
                    UCol::Up => Cell::from(Span::styled(
                        theme::bytes(u.billed_up()),
                        Style::default().fg(theme::UP),
                    )),
                    UCol::Down => Cell::from(Span::styled(
                        theme::bytes(u.billed_down()),
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
                    // 绑了网卡的用户,**订阅里报的数字和这张表里的不一样**(§10.3)。
                    // 不标出来的话,那是一条永远查不明白的「客户端和后台对不上」。
                    UCol::Nic => {
                        if u.nic_agent_ids.is_empty() {
                            Cell::from(Span::styled("—", Style::default().fg(theme::DIM)))
                        } else {
                            Cell::from(Span::styled(
                                format!("网卡×{}", u.nic_agent_ids.len()),
                                Style::default().fg(theme::DOWN),
                            ))
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

// ─────────────────────────── 设置 ───────────────────────────

pub fn settings(f: &mut Frame, area: Rect, items: &[super::settings::Setting], selected: usize) {
    let c = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    // 常驻提醒。daemon 启动时读一次配置就不再看,而「改了但没变」
    // 是这个页面最容易造出来的困惑 —— 所以这句话不能藏在某一项的说明里。
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  改完要重启 daemon 才生效:", Style::default().fg(theme::NEVER)),
            Span::styled(
                "systemctl restart sbx",
                Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("    (改的是配置文件本身,注释与排版都保留)", Style::default().fg(theme::DIM)),
        ]))
        .block(Block::default().borders(Borders::ALL).title(" 设置 ")),
        c[0],
    );

    let rows: Vec<Row> = items
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let value_style = match s.kind {
                super::settings::Kind::Bool(true) => Style::default().fg(theme::ONLINE),
                super::settings::Kind::Bool(false) => Style::default().fg(theme::INACTIVE),
                super::settings::Kind::Secret => Style::default().fg(theme::NEVER),
                _ => Style::default().fg(theme::ACCENT),
            };
            let shown = if s.shown.trim().is_empty() { "(未设置)".to_string() } else { s.shown.clone() };
            Row::new(vec![
                Cell::from(theme::truncate(&s.label, 25)),
                Cell::from(Span::styled(theme::truncate(&shown, 29), value_style)),
                Cell::from(Span::styled(s.note.clone(), Style::default().fg(theme::DIM))),
                Cell::from(""),
            ])
            .style(row_style(i == selected))
        })
        .collect();

    f.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(26),
                Constraint::Length(30),
                Constraint::Min(20),
                Constraint::Length(0),
            ],
        )
        .header(header(&["项", "当前值", "说明", ""]))
        .block(Block::default().borders(Borders::ALL).title(" 配置项 ")),
        c[1],
    );
}

// ─────────────────────────── 用量明细(二级页面)───────────────────────────

/// 「某节点上各用户」/「某用户在各节点」的明细弹窗。
///
/// 两个方向共用一个渲染:它们查的是 `user_traffic` 的同一张表,只是分组的那一维不同。
/// 表里同时给**本周期**和**累计**:本周期是计费口径(会被月重置清零),
/// 累计是从建账起的总量 —— 只给其中一个,总会有人拿它去回答另一个问题。
/// 二级页面里「名字(+ 倍率标记)」整段的宽度。
///
/// 带标记和不带标记的行占一样宽 —— 二级页面上下两块可能是不同的维度
/// (节点视图列用户、用户视图列节点),宽度不一致的话上下行会错开一格。
const BD_NAME_COLS: usize = 22;

/// 长文本二级页面一屏能显示几行。
///
/// 单独拎出来是因为**按键那一侧也要用它**:滚动的上界、翻页的步长都得按
/// 真实可视行数算,而那个数只有布局知道。两边各写一遍「减 4」的话,
/// 改一次边框就会滚过头 —— 表现是翻一页跳掉两行,而人不会发现。
pub fn config_view_h(total_h: u16) -> usize {
    // 边框 2 + 标题行 1 + 空行 1 = 4。
    (total_h as usize).saturating_sub(4)
}

/// 一个只读的长文本二级页面(现在用来看 sing-box 配置)。
///
/// 铺满可用区域而不是像明细表那样居中留边:配置是宽的
/// (缩进 + 长 base64),挤在 104 列里会到处折行。
pub fn config_text(
    f: &mut Frame,
    area: Rect,
    title: &str,
    head: &str,
    lines: &[String],
    scroll: usize,
) {
    let rect = area;
    f.render_widget(ratatui::widgets::Clear, rect);

    let view_h = config_view_h(rect.height);
    let shown: Vec<Line> = lines
        .iter()
        .skip(scroll)
        .take(view_h)
        .map(|l| {
            // JSON 里 `"key":` 的键名着色,让两三百行里能扫到结构。
            // 值不着色 —— 那样满屏彩字反而看不出层次。
            let trimmed = l.trim_start();
            if let Some(rest) = trimmed.strip_prefix('"').and_then(|r| r.split_once("\":")) {
                let indent = &l[..l.len() - trimmed.len()];
                return Line::from(vec![
                    Span::raw(indent.to_string()),
                    Span::styled(
                        format!("\"{}\":", rest.0),
                        Style::default().fg(theme::ACCENT),
                    ),
                    Span::raw(rest.1.to_string()),
                ]);
            }
            Line::from(l.clone())
        })
        .collect();

    let mut body = vec![Line::from(Span::styled(
        format!("  {head}"),
        Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
    ))];
    body.push(Line::from(""));
    body.extend(shown);

    // 标题里带滚动位置。没有它,在一份两百行的配置中间完全不知道自己在哪 ——
    // 也分不出「到底了」和「卡住了」。
    let pos = if lines.len() > view_h {
        format!(
            " {title} [{}-{}/{}] ",
            scroll + 1,
            (scroll + view_h).min(lines.len()),
            lines.len()
        )
    } else {
        format!(" {title} ")
    };
    f.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(pos)),
        rect,
    );
}

pub fn breakdown(
    f: &mut Frame,
    area: Rect,
    title: &str,
    head: &str,
    info: &[Line<'static>],
    rows: &[BreakdownRow],
) {
    // 92 是给「累计 + 说明」那段留的最小体面宽度;终端更宽就用到 104,
    // 网卡明细的说明(协议 · 端口 · 人数)在 92 上正好放不下。
    let w = 104.min(area.width.max(1));
    // info 行要算进高度里。不算的话网卡明细会把最后几行节点顶出框外 ——
    // 而 Paragraph 是**静默**截断的,看起来就像那几个节点不存在。
    let h = (rows.len().max(1) as u16 + info.len() as u16 + 6).min(area.height.max(1));
    let rect = super::modal::centered(area, 0, 0, w, h);
    f.render_widget(ratatui::widgets::Clear, rect);

    let total: i64 = rows.iter().map(|r| r.cycle()).sum();
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format!("  {head}"),
        Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(info.iter().cloned());
    lines.push(Line::from(""));
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  还没有可显示的行。建好但一次没跑过流量的也会列在这里(全 0)。",
            Style::default().fg(theme::DIM),
        )));
    }
    for r in rows {
        // 条形按**占这次明细总量的比例**画,不是按配额:这里回答的问题是
        // 「这些量里谁占大头」,而配额比例在列表页已经有了。
        let share = if total > 0 { r.cycle() as f64 / total as f64 } else { 0.0 };
        let bar = if w >= 80 { theme::gradient_bar(share, 10) } else { Vec::new() };
        // 名字和倍率标记分两个 span:标记是灰的,名字跟着主色 ——
        // 与仪表盘用户视图同一个观感。拼成一个字符串就只能整体一个颜色。
        //
        // 两者合起来占 22 列;没有标记的那一维(用户视图、网卡明细)
        // 名字独占这 22 列 —— 于是不管哪个方向,右边的上下行都从同一列起。
        let mut spans = vec![Span::raw("  ")];
        match &r.mult {
            Some(m) => spans.extend(name_with_mult(&r.label, m, BD_NAME_COLS, Style::default())),
            None => spans.push(Span::raw(theme::pad(&r.label, BD_NAME_COLS))),
        }
        // 名字段和箭头之间留一格,和仪表盘那边一致。
        spans.push(Span::raw(" "));
        spans.extend(traffic_pair(r.cycle_up, r.cycle_down));
        spans.extend(bar);
        spans.push(Span::styled(
            format!(" {:>3.0}%  ", share * 100.0),
            Style::default().fg(theme::gradient_at(share)),
        ));
        // 尾巴**自己截**,不交给 Paragraph。Paragraph 是无声切断的,
        // 切出来的 `vless-real` 看着像一个完整的协议名;`vless-rea…` 一眼就知道是被截了。
        let tail = format!("累计 {}  {}", theme::bytes(r.total_up + r.total_down), r.note);
        let used: usize = spans.iter().map(|s| theme::cols(&s.content)).sum();
        let room = (w as usize).saturating_sub(2).saturating_sub(used);
        spans.push(Span::styled(theme::truncate(&tail, room), Style::default().fg(theme::DIM)));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  本周期合计 {}    [任意键]关闭", theme::bytes(total)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const NOW: i64 = 1_764_547_200; // 2025-12-01 前后,测试里只当一个固定基准用

    /// 重置日**单独一列、两行式**:上面「每月」、下面「22 日」。
    ///
    /// 原来它是接在进度条后面的一整句「每月 22 日重置」,那一行长得离谱 ——
    /// 而「用了多少」和「什么时候清零」本来就是两件事。
    ///
    /// 越界的值(库被手改过)当作没设,不能显示成「每月 99 日」。
    /// 这条规则是从原来的 `reset_label` 搬过来的,函数没了但规则还在。
    #[test]
    fn the_reset_column_is_two_lines_and_rejects_out_of_range_days() {
        let out = draw_to_string(140, 6, |f| {
            agents(f, f.area(), &[agent(Some(500 * 1_073_741_824), Some(22))], 0, NOW)
        });
        assert!(has_cjk(&out, "重置"), "该有表头:\n{out}");
        assert!(has_cjk(&out, "每月"), "第一行该是「每月」:\n{out}");
        assert!(has_cjk(&out, "22 日"), "第二行该是日期:\n{out}");
        // 拆出去之后,流量那一行不该再带着这一整句。
        assert!(!flat(&out).contains(&flat("日重置")), "流量列不该再拖着重置文字:\n{out}");

        for bad in [Some(0), Some(32), Some(99), None] {
            let out = draw_to_string(140, 6, |f| {
                agents(f, f.area(), &[agent(Some(1 << 30), bad)], 0, NOW)
            });
            assert!(has_cjk(&out, "不重置"), "{bad:?} 该显示「不重置」:\n{out}");
            assert!(!has_cjk(&out, "每月"), "{bad:?} 不该显示「每月」:\n{out}");
        }
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
            arch: Some("amd64".into()),
            outbound: Default::default(),
            ipv4: Some("203.0.113.8".into()),
            ipv6: Some("2001:db8:1:aaaa:1234:5678:9abc:def0".into()),
            nic_quota_bytes: quota,
            nic_reset_day: day,
            nic_accounting_mode: Default::default(),
        reported_utc_offset_secs: None,
        nic_reset_offset_secs: None,
            cycle_rx: 34 * 1_073_741_824,
            cycle_tx: 0,
            up_per_sec: Some(8_600.0),
            down_per_sec: Some(6_900.0),
            node_count: 2,
            cpu_pct: Some(37.0),
            mem_used: Some(3 * 1_073_741_824),
            mem_total: Some(8 * 1_073_741_824),
            load1: Some(0.62),
            uptime_secs: Some(86_400 * 3 + 3600 * 4),
            sysinfo_at: Some(NOW - 5),
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
            cycle_up: 0,
            cycle_down: 0,
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
            nic_agent_ids: vec![],
            sub_token: "tok".into(),
        }
    }

    fn render_agents(rows: &[AgentRow]) -> String {
        draw_to_string(120, 12, |f| agents(f, f.area(), rows, 0, NOW))
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
        // 重置日现在有自己一列(两行式:「每月」/「22 日」),
        // 不再是拖在进度条后面的那一整句。
        assert!(has_cjk(&out, "每月") && has_cjk(&out, "22 日"), "重置日没画出来\n{out}");
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
        // 没设重置日时,重置那一列写「不重置」(它已经不在流量列里了)。
        assert!(has_cjk(&unlimited, "不重置"), "{unlimited}");
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

    /// **宽屏上「中转」落点要完整显示 —— 尤其是端口。**
    ///
    /// 这一列原本写死 18 列,而 `→ 64.186.234.7:40000` 要 20 列,于是被截成
    /// `→ 64.186.234.7:4…` —— 截掉的恰恰是**端口号**,而中转落点的用处
    /// 就是「打到哪个地址的哪个端口」。少了端口这一列基本等于没显示。
    /// 而那时表格右边还空着三十几列(余量全被末尾那个 `Min(0)` 空列吃掉了)。
    #[test]
    fn a_relay_target_keeps_its_port_on_a_wide_terminal() {
        let mut n = node();
        n.params.relay = crate::model::node::RelaySetting {
            host: "64.186.234.7".into(),
            port: Some(40000),
        };
        for w in [140, 160, 200] {
            let out = draw_to_string(w, 10, |f| nodes(f, f.area(), &[n.clone()], 0));
            assert!(
                out.contains("64.186.234.7:40000"),
                "{w} 列上中转落点该完整显示(含端口):\n{out}"
            );
        }
    }

    /// 「中转」到「导出」的距离要跟内容走,不能一律为最长 IPv6 预留。
    #[test]
    fn relay_column_width_follows_the_actual_address_family() {
        let cols = NCOL_ALL.to_vec();
        let relay_i = cols.iter().position(|c| *c == NCol::Relay).unwrap();

        let none = vec![node()];
        let none_w = super::ncol_widths(&cols, 200, super::relay_need(&none), PARAM_MAX)[relay_i];
        assert_eq!(none_w, 5, "没有中转时只留得下标题和破折号即可");

        let mut v4 = node();
        v4.params.relay = crate::model::node::RelaySetting {
            host: "64.186.234.7".into(),
            port: Some(40000),
        };
        let v4 = vec![v4];
        let v4_w = super::ncol_widths(&cols, 200, super::relay_need(&v4), PARAM_MAX)[relay_i];

        let mut v6 = node();
        v6.params.relay = crate::model::node::RelaySetting {
            host: "2600:1700:3a90:c620:be24:11ff:febb:5ad5".into(),
            port: Some(40000),
        };
        let v6 = vec![v6];
        let v6_w = super::ncol_widths(&cols, 200, super::relay_need(&v6), PARAM_MAX)[relay_i];

        assert!(none_w < v4_w && v4_w < v6_w, "该随内容增长:none={none_w},v4={v4_w},v6={v6_w}");
        assert_eq!(super::relay_label(&v6[0]).as_deref(), Some("[2600:1700:3a90:c620:be24:11ff:febb:5ad5]:40000"),
            "IPv6 与端口拼接必须加方括号");
    }

    /// 补宽度**不能把总宽撑过边框**。撑过去 ratatui 会静默压缩各列,
    /// 那正是 §13.4 要防的那种「看起来正常、其实每列都少了一格」。
    #[test]
    fn widening_a_column_never_overflows_the_table() {
        for w in [60u16, 90, 120, 140, 160, 200, 300] {
            let cols = super::pick(w, &NCOL_ALL, super::ncol_width, &NCOL_DROP);
            let widths = super::ncol_widths(&cols, w, RELAY_MAX, PARAM_MAX);
            let total: u16 = widths.iter().sum::<u16>() + cols.len() as u16 + 2;
            assert!(total <= w, "{w} 列:补完宽度后总宽 {total} 超了");
        }
    }

    /// 宽度不够时仍然截断,而且**看得出来被截了**(§13.4 防的是静默截断)。
    ///
    /// 120 列是刻意挑的:九列的常规宽度加间隔边框正好 119,所以这个宽度上
    /// 中转列**还在**、但只多出一列余量。再窄一点它会被整列砍掉
    /// (`NCOL_DROP` 里它排第二),那就没有截断可看了。
    #[test]
    fn a_relay_target_is_visibly_truncated_when_narrow() {
        let mut n = node();
        n.params.relay = crate::model::node::RelaySetting {
            host: "a-very-long-relay-hostname.example.com".into(),
            port: Some(40000),
        };
        let out = draw_to_string(120, 10, |f| nodes(f, f.area(), &[n.clone()], 0));
        // 断内容而不是断表头:`draw_to_string` 是逐格取字符的,宽字符第二格
        // 是空白 —— 表头「中转」提取出来是「中 转」。那是取字方式的产物,
        // 不是渲染问题,拿它做断言只会得到一条误导人的失败。
        assert!(out.contains("a-very-long"), "这个宽度上中转列该还在:\n{out}");
        assert!(out.contains('…'), "放不下时要带省略号:\n{out}");
    }

    /// **宽屏上 IPv6 要完整显示,一个字符都不能少。**
    ///
    /// 早先 IP 列写死 22 列,而运营商给的地址普遍 25~30 列 —— 于是
    /// `2605:52c0:2:3525:505…` 从**尾巴**被截掉。尾部恰恰是主机位:
    /// 同一个 /64 下的两台机器截完长得一模一样,这一列就白占了。
    /// 而那时表格右边还空着十几列。
    ///
    /// 理论最长的 IPv6(8 组 4 位 + 7 个冒号 = 39)要到 ~145 列才排得下,
    /// 所以这里用一个**真实形状**的地址(压缩过、去了前导零)。
    #[test]
    fn a_real_ipv6_is_shown_in_full_at_common_widths() {
        const ADDR: &str = "2605:52c0:2:3525:505:1:0:1234";
        let mut a = agent(None, None);
        a.ipv6 = Some(ADDR.into());
        for w in [140, 160, 200] {
            let out = draw_to_string(w, 12, |f| agents(f, f.area(), &[a.clone()], 0, NOW));
            assert!(out.contains(ADDR), "{w} 列上 IPv6 该完整显示:\n{out}");
            // 不能为它牺牲别的列 —— 出站/重置都得还在。
            let c = super::columns(w);
            assert!(c.outbound > 0 && c.reset > 0, "{w} 列:撑宽 IP 不该把别的列挤掉");
        }
    }

    /// 理论最长的 IPv6 在足够宽的终端上也要完整显示。
    /// 这条用的是现场触发 bug 的地址:八组全是四位,**正好 39 个字符**。
    /// 早先列宽虽写 39,渲染却会减一格做留白,实际只给地址 38 格,
    /// 因而显示成 `...:febb:5a…`,最后两个字符永远看不到。
    #[test]
    fn the_longest_possible_ipv6_fits_on_a_wide_terminal() {
        const MAX: &str = "2600:1700:3a90:c620:be24:11ff:febb:5ad5";
        assert_eq!(MAX.len(), 39, "这条回归必须真的是理论最长形式");
        let mut a = agent(None, None);
        a.ipv6 = Some(MAX.into());
        let out = draw_to_string(170, 12, |f| agents(f, f.area(), &[a.clone()], 0, NOW));
        assert!(out.contains(MAX), "170 列该放得下最长的 IPv6:\n{out}");
    }

    /// **146 列:七列全在,最长的 IPv6 也要一个字符不少。**
    ///
    /// 这是现场那台终端的宽度。列内那一格留白让整张表多占一列,正好卡在
    /// 门槛下面 —— 地址被截成 `…febb:5a…`,而右边看起来还空着。
    /// 留白是白花的:ratatui 本来就在相邻两列之间插了一格 `column_spacing`。
    #[test]
    fn the_longest_ipv6_survives_at_the_exact_threshold_width() {
        const MAX: &str = "2600:1700:3a90:c620:be24:11ff:febb:5ad5";
        let mut a = agent(None, None);
        a.ipv6 = Some(MAX.into());
        a.ipv4 = Some("76.9.111.80".into());

        let c = super::columns(146);
        assert!(c.host > 0 && c.outbound > 0 && c.reset > 0, "146 列该画得下全部七列");

        let out = draw_to_string(146, 8, |f| agents(f, f.area(), &[a.clone()], 0, NOW));
        assert!(out.contains(MAX), "146 列该完整显示最长的 IPv6:\n{out}");
        // 顺带确认没把别的列挤掉 —— 挤掉了「完整显示」就没有意义。
        assert!(has_cjk(&out, "主机") && has_cjk(&out, "出站"), "别的列不该被牺牲:\n{out}");
    }

    /// 145 列真的放不下,那就该**看得出来被截了**,而不是悄悄少两位。
    #[test]
    fn one_column_short_still_truncates_visibly() {
        const MAX: &str = "2600:1700:3a90:c620:be24:11ff:febb:5ad5";
        let mut a = agent(None, None);
        a.ipv6 = Some(MAX.into());
        let out = draw_to_string(145, 8, |f| agents(f, f.area(), &[a.clone()], 0, NOW));
        assert!(!out.contains(MAX), "145 列本来就放不下:\n{out}");
        assert!(out.contains('…'), "放不下要带省略号:\n{out}");
    }

    /// 窄屏放不下时仍然截断,而且**看得出来被截了**(带 `…`)。
    /// §13.4 要防的是静默截断,不是截断本身。
    #[test]
    fn a_long_address_is_visibly_truncated_when_narrow() {
        let mut a = agent(None, None);
        a.ipv6 = Some("2605:52c0:2:3525:aaaa:bbbb:cccc:dddd".into());
        let out = draw_to_string(90, 12, |f| agents(f, f.area(), &[a.clone()], 0, NOW));
        assert!(out.contains('…'), "窄屏截断要带省略号\n{out}");
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
            draw_to_string(w, h, |f| agents(f, f.area(), &[agent(Some(1000), Some(1))], 0, NOW));
            draw_to_string(w, h, |f| nodes(f, f.area(), &[node()], 0));
            draw_to_string(w, h, |f| users(f, f.area(), &[user()], 0, "https://x.example", NOW));
            let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
            draw_to_string(w, h, |f| {
                dashboard(f, f.area(), &Dash { agents: &[agent(None, None)], nodes: &[node()], users: &[user()], history: &hist, now: NOW, focus: None })
            });
            draw_to_string(w, h, |f| {
                settings(f, f.area(), &crate::tui::settings::all(&crate::config::Config::default()), 0)
            });
            draw_to_string(w, h, |f| breakdown(f, f.area(), "t", "h", &[], &[]));
            // 配置页在 1×1 上也不能炸:它要减掉边框/标题算可视行数,
            // 再按那个数去切一份两百行的文本 —— 高度不够时两处都会越界。
            let cfg: Vec<String> = (0..200).map(|i| format!("  \"key{i}\": {i},")).collect();
            draw_to_string(w, h, |f| config_text(f, f.area(), "t", "h", &cfg, 0));
            // 滚到底那一屏是最容易切过头的地方。
            draw_to_string(w, h, |f| config_text(f, f.area(), "t", "h", &cfg, 199));
        }
    }

    /// **配置页的标题里要带滚动位置。**
    ///
    /// 没有它,在一份两百行的配置中间完全不知道自己在哪 —— 更要紧的是
    /// 分不出「已经到底了」和「按键卡住了」:两种情况下按 ↓ 都是屏幕不动。
    #[test]
    fn the_config_page_shows_where_you_are() {
        let lines: Vec<String> = (0..200).map(|i| format!("\"key{i}\": {i}")).collect();
        let out =
            draw_to_string(80, 24, |f| config_text(f, f.area(), "tokyo 的配置", "h", &lines, 40));
        // 24 行高 → 一屏 20 行,从第 41 行起。
        assert!(out.contains("[41-60/200]"), "标题该带滚动位置:\n{out}");
        assert!(out.contains("\"key40\""), "该从第 41 行开始显示:\n{out}");
        assert!(!out.contains("\"key60\""), "不该越过这一屏往下多画:\n{out}");
    }

    /// 一屏放得下的时候标题里**不带**位置 —— 那时它只是噪音。
    #[test]
    fn a_short_config_gets_no_scroll_indicator() {
        let lines: Vec<String> = (0..3).map(|i| format!("\"key{i}\": {i}")).collect();
        let out = draw_to_string(80, 24, |f| config_text(f, f.area(), "小配置", "h", &lines, 0));
        assert!(!out.contains('/'), "一屏放得下就不该有 x-y/z:\n{out}");
    }

    /// 出站策略要有**自己一列**,不能只写在底部摘要里。
    ///
    /// 摘要那一行会被操作回执顶掉 —— 而「按完 [o] 想确认改对没有」恰恰就是
    /// 回执还挂着的那一刻。这条是用起来才发现的:操作完选中态一丢,
    /// 就没地方看当前策略了。
    #[test]
    fn the_agents_table_has_an_outbound_column() {
        use crate::model::outbound::OutboundStrategy;

        let rows = vec![
            AgentRow { outbound: OutboundStrategy::Ipv6Only, ..agent(None, None) },
            AgentRow {
                outbound: OutboundStrategy::PreferIpv4,
                name: "osaka".into(),
                ..agent(None, None)
            },
        ];
        let out = draw_to_string(120, 8, |f| agents(f, f.area(), &rows, 0, NOW));
        assert!(has_cjk(&out, "出站"), "该有表头:\n{out}");
        assert!(has_cjk(&out, "仅 v6"), "该显示当前策略:\n{out}");
        assert!(has_cjk(&out, "优先v4"), "第二行也要显示自己的:\n{out}");
        // 这一列是只读表格,不写按键的话没人知道它能改。
        assert!(out.contains("[o]"), "该提示怎么改:\n{out}");
    }

    /// 窄屏上出站策略比主机列**先保**:CPU/内存是锦上添花,
    /// 而出站策略是一个「改了就得确认」的设置项。
    #[test]
    fn the_outbound_column_outranks_the_host_column() {
        // 120 列:出站在、主机不在(主机要 140 才画得下)。
        let c = super::columns(120);
        assert!(c.outbound > 0, "120 列该画得下出站列");
        assert_eq!(c.host, 0, "120 列还画不下主机列");

        // 140 列:两个都在。
        let c = super::columns(140);
        assert!(c.outbound > 0 && c.host > 0, "140 列两个都该有");

        // 极窄:两个都让掉,但表格本身还得能画(不 panic)。
        let c = super::columns(70);
        assert_eq!((c.outbound, c.host), (0, 0), "70 列该把两个都让掉");
        let out = draw_to_string(70, 6, |f| agents(f, f.area(), &[agent(None, None)], 0, NOW));
        assert!(!out.is_empty());
    }

    /// 重置日拆出去之后,流量列要**跟着收窄**。
    ///
    /// 不收的话它还按「重置日跟在后面」那会儿的 38 列算,而实际内容只有 25 ——
    /// 右边空出十几格,把重置 / 出站 / 主机整体推远,百分比和重置日中间隔着
    /// 一大片空白,看起来像两块不相干的东西。用户就是这么报的。
    ///
    /// 反过来,窄屏回退(重置日回到流量列第二行)时**不能**收窄:
    /// 那时那一行要装下「条 + 百分比 + 每月 22 日重置」。
    #[test]
    fn the_traffic_column_tightens_once_reset_moves_out() {
        // 宽屏:重置列在,流量列该收到 TIGHT。
        let wide = columns(140);
        assert!(wide.reset > 0, "140 列该有重置列");
        assert_eq!(wide.traffic, super::TRAFFIC_COL_TIGHT, "重置搬走了,流量列该收窄");

        // 窄屏:重置列没了,流量列得留着原来的宽度装那句话。
        let narrow = columns(100);
        assert_eq!(narrow.reset, 0, "100 列放不下重置列");
        assert_eq!(narrow.traffic, super::TRAFFIC_COL, "回退时不该收窄");

        // 收窄之后条仍然画得出来(收过头会把条挤没,那是另一种退化)。
        assert!(wide.bar >= 4, "收窄后仍该画得下进度条:bar={}", wide.bar);

        // 渲染出来核对:百分比和「每月」之间不该隔着一大片空白。
        let out = draw_to_string(140, 6, |f| {
            agents(f, f.area(), &[agent(Some(500 * 1_073_741_824), Some(22))], 0, NOW)
        });
        let line = out
            .lines()
            .find(|l| l.contains('%') && l.contains('█'))
            .expect("该有一行同时带条和百分比");
        // 从**进度条**往右找那个百分比。用 rfind('%') 会命中主机列的
        // 「内存 38%」,而那在重置日右边 —— 切片会反过来直接 panic。
        let bar_end = line.rfind('░').or_else(|| line.rfind('█')).expect("该有条");
        let pct_end = bar_end + line[bar_end..].find('%').expect("条右边该是百分比");
        let reset_at = pct_end + line[pct_end..].find("22").expect("再往右该是重置日");
        let gap = line[pct_end + 1..reset_at].chars().filter(|c| *c == ' ').count();
        assert!(gap <= 6, "百分比和重置日之间空了 {gap} 格,太远:\n{out}");
    }

    /// **任何宽度下重置日都得看得见**(§13.4)。
    ///
    /// 拆成独立一列之后差点丢掉这条:那一列在 120 列以下会被让掉,
    /// 而我一开始没做回退 —— 表现是窄屏上重置日**整个消失**,
    /// 界面看起来毫无异常,只是少了一项。§13.4 要防的正是这种静默丢信息。
    ///
    /// 现在宽屏走独立列(两行式),窄屏退回流量列第二行(一行式),
    /// 两条路都得有。
    #[test]
    fn the_reset_day_survives_every_width() {
        for w in [70u16, 80, 100, 110, 120, 140, 160, 200] {
            let out = draw_to_string(w, 6, |f| {
                agents(f, f.area(), &[agent(Some(500 * 1_073_741_824), Some(22))], 0, NOW)
            });
            let flat_out = flat(&out);
            let two_line = flat_out.contains(&flat("每月")) && flat_out.contains(&flat("22 日"));
            let one_line = flat_out.contains(&flat("每月 22 日重置"));
            assert!(two_line || one_line, "{w} 列下重置日不见了:\n{out}");
        }
    }

    /// §13.4:80 列的窄终端下,**重置日不能被截断**,IPv6 要留住省略号。
    #[test]
    fn narrow_terminal_keeps_the_reset_day_intact() {
        let out = draw_to_string(80, 8, |f| {
            agents(f, f.area(), &[agent(Some(500 * 1_073_741_824), Some(22))], 0, NOW)
        });
        assert!(has_cjk(&out, "每月 22 日重置"), "80 列下重置日被截断了:\n{out}");
        assert!(out.contains('…'), "IPv6 应当截断并保留省略号:\n{out}");
        assert!(out.contains('█'), "80 列下仍应画得下进度条:\n{out}");
    }

    #[test]
    fn column_widths_always_fit() {
        for w in 20..260u16 {
            let c = columns(w);
            // 每一根 `Constraint::Length` 都要算进去,还有 ratatui 在列之间插的
            // 间隔(列数 - 1,末尾那根吃余量的空列也占一个位)和左右边框。
            // 只核对四个基础列的话,「变宽了反而更挤」那类溢出根本抓不到:
            // 主机列在 121 列凭空出现时,总���一次撑过边框 8 格。
            let cols = 4 + u16::from(c.host > 0) + u16::from(c.outbound > 0)
                + u16::from(c.reset > 0)
                + 1;
            let total = c.name + c.ip + c.speed + c.traffic + c.host + c.outbound + c.reset
                + (cols - 1)
                + 2;
            assert!(total <= w.max(1), "宽度 {w}:列合计 {total} 超了");
        }
    }

    /// 一台机器的用量、进度条、网卡明细都要跟着**它自己的**记账口径走。
    #[test]
    fn the_traffic_column_follows_the_agents_accounting_mode() {
        use crate::model::agent::NicAccountingMode;
        let base = AgentRow {
            cycle_rx: 30 * 1_073_741_824,
            cycle_tx: 10 * 1_073_741_824,
            ..agent(Some(100 * 1_073_741_824), Some(22))
        };
        for (mode, used, pct) in [
            (NicAccountingMode::Sum, "40.00 GB", "40%"),
            (NicAccountingMode::Outbound, "10.00 GB", "10%"),
            (NicAccountingMode::Inbound, "30.00 GB", "30%"),
            (NicAccountingMode::Max, "30.00 GB", "30%"),
        ] {
            let a = AgentRow { nic_accounting_mode: mode, ..base.clone() };
            let out =
                draw_to_string(140, 6, |f| agents(f, f.area(), std::slice::from_ref(&a), 0, NOW));
            assert!(out.contains(used), "{mode:?} 的已用量该是 {used}:\n{out}");
            assert!(out.contains(pct), "{mode:?} 的百分比该是 {pct}:\n{out}");
        }
    }

    /// 网卡明细要同时给出**原始方向**和**按口径计入**的那个数 ——
    /// 少了前者没法和厂商账单对,少了后者不知道进度条是怎么算出来的。
    #[test]
    fn nic_info_shows_raw_directions_and_the_accounted_total() {
        use crate::model::agent::NicAccountingMode;
        let a = AgentRow {
            nic_accounting_mode: NicAccountingMode::Outbound,
            cycle_rx: 30 * 1_073_741_824,
            cycle_tx: 10 * 1_073_741_824,
            ..agent(Some(100 * 1_073_741_824), Some(22))
        };
        let text: String = nic_info(&a, NOW)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("↑ 10.00 GB"), "原始 TX 要在:{text}");
        assert!(text.contains("↓ 30.00 GB"), "原始 RX 要在:{text}");
        assert!(has_cjk(&text, "计入(出站)"), "要写明按哪个口径计:{text}");
        assert!(text.contains("10.00 GB / 100.00 GB"), "配额行该用计入值:{text}");
        assert!(has_cjk(&text, "剩 90.00 GB"), "剩余也该按计入值算:{text}");
    }

    /// 网卡明细要写出**生效的重置时区和它的来源**。
    ///
    /// 只写值不写来源是不够的:最常见的错法是 VPS 镜像出厂就是 UTC、厂商却按机房
    /// 当地时间计费,于是 agent 老老实实报 `+00:00`,看起来和「人确认过是 UTC」
    /// 一模一样。标出来源才能把这两种区分开。
    #[test]
    fn nic_info_says_which_timezone_the_month_rolls_in() {
        let base = agent(Some(100 * 1_073_741_824), Some(22));
        let render = |a: &AgentRow| -> String {
            nic_info(a, NOW)
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
                .collect()
        };

        let reported =
            AgentRow { reported_utc_offset_secs: Some(-25200), ..base.clone() };
        let text = render(&reported);
        assert!(text.contains("-07:00"), "该写出生效偏移:{text}");
        assert!(has_cjk(&text, "agent 上报"), "该标明来源是 agent:{text}");

        let manual = AgentRow {
            reported_utc_offset_secs: Some(-25200),
            nic_reset_offset_secs: Some(0),
            ..base.clone()
        };
        let text = render(&manual);
        assert!(text.contains("+00:00"), "手工值该压过上报值:{text}");
        assert!(has_cjk(&text, "手工"), "该标明是人填的:{text}");

        let neither = AgentRow { ..base };
        assert!(has_cjk(&render(&neither), "主控时区"), "都没有时该说明回落到主控");
    }

    /// 倒计时按**这台自己的时区**算。用 TUI 进程的时区的话,一台 UTC-7 的机器
    /// 会在主控的傍晚被说成「就是今天」,而它当地还要等好几个小时 —— 弹窗自己
    /// 和真正的重置边界打架。
    #[test]
    fn nic_info_counts_down_in_the_agents_own_timezone() {
        use chrono::TimeZone;
        // 2026-08-22 03:00 UTC:UTC 那台已经是 22 号,UTC-7 那台当地还是 21 号。
        let now = chrono::Utc.with_ymd_and_hms(2026, 8, 22, 3, 0, 0).single().unwrap().timestamp();
        let render = |a: &AgentRow| -> String {
            nic_info(a, now)
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
                .collect()
        };

        let utc = AgentRow {
            nic_reset_offset_secs: Some(0),
            ..agent(Some(100 * 1_073_741_824), Some(22))
        };
        assert!(has_cjk(&render(&utc), "就是今天"), "UTC 那台当地已是 22 号");

        let west = AgentRow {
            nic_reset_offset_secs: Some(-7 * 3600),
            ..agent(Some(100 * 1_073_741_824), Some(22))
        };
        let text = render(&west);
        assert!(!has_cjk(&text, "就是今天"), "UTC-7 那台当地还是 21 号:{text}");
        assert!(has_cjk(&text, "约 1 天后"), "还差一天:{text}");
    }

    /// 不限流量的那一行,第二行画一条**暗色短横线**而不是留空。
    ///
    /// 留空的那一版看起来像这一行只有一行内容,和上下两台机器黏在一起 ——
    /// 而缺 IPv6 时早就是用 `—` 占位的,两处要一致。
    #[test]
    fn unlimited_traffic_draws_a_dim_dash_instead_of_a_gap() {
        let mut term = Terminal::new(TestBackend::new(140, 6)).unwrap();
        term.draw(|f| agents(f, f.area(), &[agent(None, None)], 0, NOW)).unwrap();
        let buf = term.backend().buffer().clone();

        // y=2 是第一行内容,y=3 是它的第二行。IPv4/IPv6 这一台都有,
        // 所以第二行上的 `—` 只可能来自流量列。
        let dashes: Vec<u16> =
            (0..buf.area.width).filter(|x| buf[(*x, 3)].symbol() == "—").collect();
        assert_eq!(dashes.len(), 1, "第二行该正好有一条短横线:{dashes:?}");
        assert_eq!(
            buf[(dashes[0], 3)].style().fg,
            Some(theme::DIM),
            "占位的短横线要用暗色,不能抢眼"
        );
    }

    /// 缺 IPv6 同样是一条短横线 —— 这条规则一直在,但没人钉过。
    #[test]
    fn a_missing_ipv6_renders_as_a_dash() {
        let a = AgentRow { ipv6: None, ..agent(Some(500 * 1_073_741_824), Some(22)) };
        let out =
            draw_to_string(140, 6, |f| agents(f, f.area(), std::slice::from_ref(&a), 0, NOW));
        assert!(out.contains('—'), "缺 IPv6 该占位:\n{out}");
    }

    /// 放得下的时候,相邻两台机器之间空一行 —— 用户说的「半行」间距。
    ///
    /// 一台机器占两行,而终端画不出半行,所以一条空行就是最小的那一档。
    /// **最后一台后面不加**:加了会在底边框前面多出一条空白,
    /// 而且会让恰好放得下的那一屏少掉一行内容。
    #[test]
    fn agents_are_separated_by_one_blank_line_when_there_is_room() {
        let rows = vec![
            agent(Some(500 * 1_073_741_824), Some(22)),
            AgentRow { id: 2, name: "osaka".into(), ..agent(None, None) },
        ];
        // 边框 2 + 表头 1 + (2 + 1 + 2) = 8,正好放得下。
        let out = draw_to_string(140, 8, |f| agents(f, f.area(), &rows, 0, NOW));
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[2].contains("tokyo-1"), "第一台在这儿:\n{out}");
        assert!(lines[3].contains("v0.1.0"), "它的第二行:\n{out}");
        assert!(blank_inner(lines[4]), "两台之间该空一行:\n{out}");
        assert!(lines[5].contains("osaka"), "第二台紧跟着空行:\n{out}");
        assert!(lines[6].contains("v0.1.0"), "它的第二行:\n{out}");
        assert!(lines[7].starts_with('└'), "最后一台后面不该再插空行:\n{out}");
    }

    /// 一行除了左右边框之外什么都没有。`draw_to_string` 只裁行尾空白,
    /// 表格的竖边框仍在,所以不能直接 `trim().is_empty()`。
    fn blank_inner(line: &str) -> bool {
        line.trim_matches(|c| c == '│' || c == ' ').is_empty()
    }

    /// 空行**不属于选中行**:选中态的底色只该盖住那两行内容。
    #[test]
    fn the_spacer_row_is_not_part_of_the_selection() {
        let rows = vec![
            agent(Some(500 * 1_073_741_824), Some(22)),
            AgentRow { id: 2, name: "osaka".into(), ..agent(None, None) },
        ];
        let mut term = Terminal::new(TestBackend::new(140, 8)).unwrap();
        term.draw(|f| agents(f, f.area(), &rows, 0, NOW)).unwrap();
        let buf = term.backend().buffer().clone();
        for y in [2u16, 3] {
            assert_eq!(buf[(2, y)].style().bg, Some(theme::ROW_BG), "选中行第 {y} 行该有底色");
        }
        assert_ne!(buf[(2, 4)].style().bg, Some(theme::ROW_BG), "空行不该跟着高亮");
    }

    /// **高度不够就退回紧凑布局。** 这张表没有滚动视口,行数一多就从底下裁掉;
    /// 无条件加空行等于凭空少放三分之一的机器,而被裁掉的那台看不出来。
    #[test]
    fn spacing_is_dropped_when_the_rows_would_not_all_fit() {
        let rows: Vec<AgentRow> = (1..=3)
            .map(|i| AgentRow { id: i, name: format!("m{i}"), ..agent(None, None) })
            .collect();
        // 三台紧凑要 6 行,带间距要 8 行。给 7 行数据高度(总高 10)只能紧凑。
        let out = draw_to_string(140, 10, |f| agents(f, f.area(), &rows, 0, NOW));
        let lines: Vec<&str> = out.lines().collect();
        for (i, y) in [2usize, 4, 6].iter().enumerate() {
            assert!(lines[*y].contains(&format!("m{}", i + 1)), "第 {i} 台该在第 {y} 行:\n{out}");
        }

        // 再给一行就放得开了。
        let out = draw_to_string(140, 11, |f| agents(f, f.area(), &rows, 0, NOW));
        let lines: Vec<&str> = out.lines().collect();
        assert!(blank_inner(lines[4]), "宽裕时该插空行:\n{out}");
        assert!(lines[8].contains("m3"), "第三台该被推到第 8 行:\n{out}");
    }

    #[test]
    fn percentage_moves_to_text_when_the_bar_does_not_fit() {
        let out = draw_to_string(70, 6, |f| {
            agents(f, f.area(), &[agent(Some(100 * 1_073_741_824), Some(22))], 0, NOW)
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
            |u: UserRow| draw_to_string(120, 10, move |f| users(f, f.area(), &[u], 0, "", NOW));

        assert!(has_cjk(&render(base.clone()), "自动停用"));
        assert!(has_cjk(&render(UserRow { auto_disabled: false, ..base.clone() }), "手动停用"));
        assert!(has_cjk(&render(UserRow { enabled: true, ..base }), "启用"));
    }

    /// 快到期要提前看得见 —— 只给一个日期等于让人自己心算。
    #[test]
    fn expiry_within_a_week_is_called_out() {
        let soon = UserRow { expire_at: Some(NOW + 86_400 * 3), ..user() };
        let out = draw_to_string(120, 10, |f| users(f, f.area(), &[soon], 0, "", NOW));
        assert!(has_cjk(&out, "3天"), "快到期应当显示还剩几天:\n{out}");

        let gone = UserRow { expire_at: Some(NOW - 10), ..user() };
        let out = draw_to_string(120, 10, |f| users(f, f.area(), &[gone], 0, "", NOW));
        assert!(has_cjk(&out, "已过期"), "{out}");

        let forever = UserRow { expire_at: None, ..user() };
        let out = draw_to_string(120, 10, |f| users(f, f.area(), &[forever], 0, "", NOW));
        assert!(has_cjk(&out, "永久"), "{out}");
    }

    // 「订阅地址」「中转落点」「没分配节点」这三条搬去了 mod.rs ——
    // 它们现在由「操作」面板的摘要行负责(`App::ops_lines`),
    // 而这一层只剩表格。

    /// 节点**表格**绝不能渲染密钥材料(§11.3)。
    ///
    /// 摘要行那一侧由 `mod.rs::ops_lines_never_leak_key_material` 守 ——
    /// 详情面板去掉之后,那些字段搬到了摘要行,守卫也要跟着搬,
    /// 不然这条 §11.3 就只剩半边。
    #[test]
    fn node_detail_never_leaks_key_material() {
        let mut n = node();
        n.params.private_key = Some("PRIVATE-KEY-MUST-NOT-APPEAR".into());
        n.params.key_pem = Some("KEY-PEM-MUST-NOT-APPEAR".into());
        n.params.ss_password = Some("SS-PASSWORD-MUST-NOT-APPEAR".into());
        let out = draw_to_string(200, 20, |f| nodes(f, f.area(), &[n], 0));
        assert!(!out.contains("MUST-NOT-APPEAR"), "密钥材料被画到界面上了:\n{out}");
    }

    /// 主机指标过期时必须显示 `--`,**不能显示上一次的数字**。
    ///
    /// 这是这一组里唯一真正会误导人的失败模式:一台离线三天的机器
    /// 挂着三天前那个「CPU 3%」,看起来和一台闲着的在线机器一模一样。
    #[test]
    fn stale_host_metrics_read_as_dashes() {
        let fresh = agent(None, None);
        assert!(fresh.host_metrics_fresh(NOW, HOST_METRICS_STALE_AFTER));
        assert!(host_metrics(&fresh, NOW, 80).contains("37%"));

        let stale = AgentRow { sysinfo_at: Some(NOW - 3 * 86_400), ..agent(None, None) };
        let line = host_metrics(&stale, NOW, 80);
        assert!(line.contains("CPU --"), "{line}");
        assert!(!line.contains("37"), "过期了还在显示上一次的数字:{line}");

        // 从没上报过(sysinfo_at IS NULL)同样是 `--`。
        let never = AgentRow { sysinfo_at: None, ..agent(None, None) };
        assert!(host_metrics(&never, NOW, 80).contains("CPU --"));

        // 上报周期是 30s,门槛要留余量:29 秒前的数据仍然算新鲜。
        let recent = AgentRow { sysinfo_at: Some(NOW - 29), ..agent(None, None) };
        assert!(recent.host_metrics_fresh(NOW, HOST_METRICS_STALE_AFTER));
    }

    #[test]
    fn uptime_reads_in_two_units_at_most() {
        assert_eq!(uptime_label(86_400 * 3 + 3600 * 4 + 725), "3 天 4 小时");
        assert_eq!(uptime_label(3600 * 5 + 60 * 7), "5 小时 7 分");
        assert_eq!(uptime_label(180), "3 分");
        assert_eq!(uptime_label(0), "0 分");
    }

    #[test]
    fn mem_ratio_needs_a_total() {
        let a = agent(None, None);
        assert_eq!(a.mem_ratio(), Some(0.375), "3G / 8G");
        assert_eq!(AgentRow { mem_total: Some(0), ..agent(None, None) }.mem_ratio(), None);
        assert_eq!(AgentRow { mem_total: None, ..agent(None, None) }.mem_ratio(), None);
        assert_eq!(AgentRow { mem_used: None, ..agent(None, None) }.mem_ratio(), None);
    }

    /// 主机列只在宽到有余量时出现,而且出现时不能把别的列挤窄。
    #[test]
    fn the_host_column_only_appears_when_there_is_room() {
        assert_eq!(columns(80).host, 0, "80 列放不下主机列");
        assert!(columns(140).host > 0, "140 列该有主机列");
        let out = draw_to_string(140, 6, |f| agents(f, f.area(), &[agent(None, None)], 0, NOW));
        assert!(out.contains("CPU 37%"), "{out}");
        assert!(has_cjk(&out, "内存 38%"), "{out}");
        // 加了主机列之后前面几列都还得在(重置列写「不重置」)。
        assert!(has_cjk(&out, "不重置"), "加了主机列不该把别的列挤掉:\n{out}");
    }

    /// 概况的「本周期用量」是**计费口径**的上行/下行/总计,不再有网卡那一段。
    ///
    /// 整机网卡的总和在这里没有意义:它把系统更新、别的服务、协议开销全算进来,
    /// 而这一行旁边就是用户数和节点数 —— 人读到的是「这些用户用了多少」。
    /// 两个口径并排摆了几个版本,结果是每次都要一句注释去解释它们为什么对不上。
    /// 厂商账单上的那个数在服务管理页(每台机器一行)和它的网卡明细里。
    #[test]
    fn the_overview_reports_billed_traffic_not_nic_traffic() {
        let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
        // 倍率 2.0,原始 20 GiB↑ / 55 GiB↓ → 计费 40 GiB↑ / 110 GiB↓ / 150 GiB 总计。
        let u = UserRow { traffic_multiplier: 2.0, ..user() };
        // 网卡数字给一个**极其显眼**的值:它要是漏在页面上,一眼就能看见。
        let a = AgentRow { cycle_rx: 777 * 1_073_741_824, cycle_tx: 888 * 1_073_741_824, ..agent(None, None) };
        let out = draw_to_string(120, 30, |f| {
            dashboard(f, f.area(), &Dash { agents: &[a], nodes: &[node()], users: &[u], history: &hist, now: NOW, focus: None })
        });

        assert!(has_cjk(&out, "本周期用量"), "{out}");
        assert!(out.contains("↑ 40.00 GB"), "上行该是 20 GiB × 2:\n{out}");
        assert!(out.contains("↓ 110.00 GB"), "下行该是 55 GiB × 2:\n{out}");
        assert!(has_cjk(&out, "总计") && out.contains("150.00 GB"), "总计该是两者之和:\n{out}");
        // 网卡的数字和「网卡」这个词都不该再出现在概况里。
        assert!(!out.contains("777.00 GB") && !out.contains("888.00 GB"), "网卡流量该整块去掉:\n{out}");
        assert!(!has_cjk(&out, "网卡 = 整机进出"), "那句注释跟着一起去掉:\n{out}");
    }

    /// 上行 + 下行必须**正好**等于总计。
    ///
    /// 先加再乘会和分别乘再加差最多 1 字节 —— 而这三个数就摆在同一行上,
    /// 差一个字节也是肉眼可见的「加不起来」。
    #[test]
    fn the_overview_three_numbers_add_up_exactly() {
        let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
        // 故意选会产生小数的倍率和奇数字节。
        let users: Vec<UserRow> = [(1.5, 333, 777), (2.0, 101, 202), (1.0, 55, 7)]
            .iter()
            .enumerate()
            .map(|(i, &(m, up, down))| UserRow {
                id: i as i64 + 1,
                name: format!("u{i}"),
                traffic_multiplier: m,
                cycle_up: up,
                cycle_down: down,
                ..user()
            })
            .collect();
        let out = draw_to_string(120, 30, |f| {
            dashboard(f, f.area(), &Dash { agents: &[], nodes: &[], users: &users, history: &hist, now: NOW, focus: None })
        });
        // 499 + 202 + 55 = 756 上行;1165 + 404 + 7 = 1576 下行;合计 2332。
        assert!(out.contains("756 B"), "上行:\n{out}");
        assert!(out.contains("1.54 KB"), "下行 1576 B:\n{out}");
        assert!(out.contains("2.28 KB"), "总计 2332 B 该正好是两者之和:\n{out}");
    }

    /// 仪表盘用户视图:名字后面跟倍率标记,上下行是乘过倍率的数。
    ///
    /// 标记必须在**名字旁边**,不是行尾 —— 它解释的是紧接着的那两个数字。
    #[test]
    fn the_dashboard_user_view_tags_the_multiplier_next_to_the_name() {
        let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
        let double = UserRow { traffic_multiplier: 2.0, ..user() };
        let single = UserRow {
            id: 2,
            name: "bob".into(),
            traffic_multiplier: 1.0,
            cycle_up: 1_073_741_824,
            cycle_down: 1_073_741_824,
            ..user()
        };
        let out = draw_to_string(120, 30, |f| {
            dashboard(f, f.area(), &Dash { agents: &[], nodes: &[node()], users: &[double, single], history: &hist, now: NOW, focus: None })
        });
        // **按原样断言,不走 `flat`。** `flat` 把空白全删掉,于是
        // `alice     [2.0x]`(标记被推到箭头旁边)和 `alice [2.0x]` 一样能过 ——
        // 而这条测试要盯的正是这两者的区别。
        assert!(out.contains("alice [2.0x]"), "[2.0x] 该紧跟在名字后面:\n{out}");
        // 1.0x 也要标出来:省掉的话「没标记」有两种含义,而人分不出来。
        assert!(out.contains("bob [1.0x]"), "单倍也要标:\n{out}");
        assert!(out.contains("40.00 GB") && out.contains("110.00 GB"), "上下行该乘过倍率:\n{out}");
    }

    /// 名字长短不同的两行,标记都紧跟各自的名字,而**右边的箭头仍然对齐**。
    ///
    /// 这两件事看着矛盾,做法是把空位补在标记右边而不是名字右边。
    /// 反过来(先把名字补满再接标记)就是 v0.4.6 那个观感问题:
    /// `admin     [2.0x] ↑ 5.26 MB` —— 标记贴着箭头,看起来像在解释箭头,
    /// 而它解释的是左边那个名字。
    #[test]
    fn the_multiplier_tag_hugs_the_name_while_the_arrows_stay_aligned() {
        let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
        let short = UserRow { id: 1, name: "ad".into(), traffic_multiplier: 2.0, ..user() };
        let long = UserRow { id: 2, name: "a-long-name".into(), traffic_multiplier: 1.0, ..user() };
        let out = draw_to_string(120, 30, |f| {
            dashboard(f, f.area(), &Dash { agents: &[], nodes: &[], users: &[short, long], history: &hist, now: NOW, focus: None })
        });

        let rows: Vec<&str> =
            out.lines().filter(|l| l.contains("[2.0x]") || l.contains("[1.0x]")).collect();
        assert_eq!(rows.len(), 2, "该有两行用户:\n{out}");
        let mut arrow_at = Vec::new();
        for line in &rows {
            let ch: Vec<char> = line.chars().collect();
            let at = ch.iter().position(|&c| c == '[').unwrap();
            // 标记前恰好一格空白,而再往左不是空白 —— 即标记贴着名字末尾。
            assert_eq!(ch[at - 1], ' ', "标记前该有一格:{line}");
            assert_ne!(ch[at - 2], ' ', "标记前只该有一格,名字长短都一样:{line}");
            arrow_at.push(ch.iter().position(|&c| c == '↑').unwrap());
        }
        assert_eq!(arrow_at[0], arrow_at[1], "名字长短不同,箭头仍要对齐:\n{out}");
    }

    /// **箭头到数字的距离恒定一格**,不随数字长短变。
    ///
    /// 早先用的是 `↑{:>9}`(右对齐):8 位的 `20.00 GB` 前面空一格,
    /// 9 位的 `110.00 GB` 一格不空。而下行通常比上行大一位,于是同一行里
    /// 系统性地「上行离箭头远、下行贴着箭头」,看着像两栏没对齐。
    ///
    /// 顺带盯住 10 位数(`1012.98 MB`,落在 1000~1024 那一段):
    /// 9 列的格子装不下它,会把右边的条和百分比整体推歪。
    #[test]
    fn the_gap_after_each_arrow_never_changes() {
        let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
        // 三种长度都摆上:7 位、9 位、10 位。
        let rows = vec![
            // 1_062_185_533 B = 1012.98 MB —— 正好 10 列,9 列的格子装不下。
            NodeRow { cycle_up: 2_759_000, cycle_down: 1_062_185_533, ..node() },
            NodeRow { id: 2, tag: "osaka".into(), cycle_up: 3 << 30, cycle_down: 1 << 30, ..node() },
        ];
        let out = draw_to_string(120, 30, |f| {
            dashboard(f, f.area(), &Dash { agents: &[], nodes: &rows, users: &[], history: &hist, now: NOW, focus: None })
        });
        for line in out.lines().filter(|l| l.contains('↑') && !l.contains("上行")) {
            for arrow in ['↑', '↓'] {
                let rest: String = match line.find(arrow) {
                    Some(i) => line[i..].chars().skip(1).take(2).collect(),
                    None => continue,
                };
                // 恰好一个空格:第一个是空格,第二个已经是数字了。
                // 只断言「第一个是空格」是不够的 —— 右对齐短数字(`↑  2.63 MB`)
                // 也满足那一条,而它正是要防的那种参差。
                let mut c = rest.chars();
                assert_eq!(c.next(), Some(' '), "{arrow} 后面要有一个空格:\n{line}");
                assert!(
                    !matches!(c.next(), Some(' ')),
                    "{arrow} 后面只该有一个空格,多出来的是右对齐留的:\n{line}"
                );
            }
        }
        // 10 位的数字要完整,不能被截成 `1012.98 M`(M 和 MB 差 1024 倍)。
        assert!(out.contains("1012.98 MB"), "10 位数该完整显示:\n{out}");
    }

    /// 概览页要能一眼看出有机器掉线、有人快超额。
    #[test]
    fn dashboard_summarises_the_cluster() {        let mut offline = agent(None, None);
        offline.id = 2;
        offline.name = "osaka-2".into();
        offline.status = "offline".into();
        let hist: VecDeque<(f64, f64)> =
            (0..40).map(|i| (1000.0 * (i % 7) as f64, 800.0 * (i % 5) as f64)).collect();
        let out = draw_to_string(120, 30, |f| {
            dashboard(f, f.area(), &Dash { agents: &[agent(Some(1000), Some(1)), offline], nodes: &[node()], users: &[user()], history: &hist, now: NOW, focus: None })
        });
        assert!(has_cjk(&out, "在线 1"), "{out}");
        assert!(has_cjk(&out, "离线 1"), "{out}");
        assert!(out.contains("alice"), "用量 Top 里要有用户:\n{out}");
        assert!(has_cjk(&out, "上行") && has_cjk(&out, "下行"), "要有上下行两张图:\n{out}");
        // 盲文点阵:曲线画出来的字符落在 U+2800 区段。
        assert!(out.chars().any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)), "折线没画出来:\n{out}");
    }

    /// 点数不够时不画空图,直接说「在攒数据」——
    /// 一张空坐标系看起来像坏了,而它其实只是还没到第二次上报。
    #[test]
    fn a_chart_with_one_point_says_it_is_collecting() {
        let hist: VecDeque<(f64, f64)> = VecDeque::from(vec![(1.0, 2.0)]);
        let out = draw_to_string(120, 30, |f| {
            dashboard(f, f.area(), &Dash { agents: &[agent(None, None)], nodes: &[], users: &[], history: &hist, now: NOW, focus: None })
        });
        assert!(has_cjk(&out, "还在攒数据"), "{out}");
    }

    /// 矮终端上整块让掉折线图 —— 一条挤成两行的曲线不如把地方给下面的数字。
    #[test]
    fn a_short_terminal_drops_the_chart_not_the_numbers() {
        let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
        let out = draw_to_string(120, 18, |f| {
            dashboard(f, f.area(), &Dash { agents: &[agent(None, None)], nodes: &[node()], users: &[user()], history: &hist, now: NOW, focus: None })
        });
        assert!(!has_cjk(&out, "上行  "), "矮屏不该画折线图:\n{out}");
        assert!(out.contains("alice"), "但明细必须留着:\n{out}");
    }

    /// 仪表盘右下角是**节点视图**,不是被控机视图。
    ///
    /// 网卡那一栏搬去了服务管理页的二级页面 —— 它是整机口径,
    /// 和这一屏上另外两张表的用户口径不是一回事(§6.4)。
    #[test]
    fn dashboard_bottom_row_is_users_and_nodes() {
        let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
        let out = draw_to_string(120, 30, |f| {
            dashboard(f, f.area(), &Dash { agents: &[agent(Some(1 << 40), Some(22))], nodes: &[node()], users: &[user()], history: &hist, now: NOW, focus: None })
        });
        assert!(has_cjk(&out, "用量 Top"), "用户视图要在:
{out}");
        assert!(has_cjk(&out, "节点用量"), "节点视图要在:
{out}");
        assert!(has_cjk(&out, "tokyo-reality@tokyo-1"), "节点行要带机器名:
{out}");
        // 「被控服务器」那个面板整块搬走了。留着它等于同一屏上摆两个口径的数字。
        assert!(!flat(&out).contains(&flat("┌ 被控服务器")), "网卡面板不该还在仪表盘上:
{out}");
    }

    /// 节点视图的百分比不能被切掉。
    ///
    /// 少一个 `%` 会把「份额 100%」读成「份额 100」,而更糟的是
    /// `0%` 被切成 `0` —— 那看起来像一个正常的数字,没人会发现这一列是残的。
    #[test]
    fn node_view_never_clips_the_percent_sign() {
        let rows = vec![NodeRow { cycle_up: 3 << 30, cycle_down: 1 << 30, ..node() }];
        // 57 是 120 列屏幕上这一栏的实际宽度 —— 原来的算法在这里正好差一格,
        // 把 `%` 挤掉了。50 是再窄一档:此时条形该整列消失,但百分比要留住。
        for w in [50u16, 57, 80, 100, 120, 160] {
            let out = draw_to_string(w, 10, |f| {
                let a = f.area();
                super::top_nodes(f, a, &rows, true, None)
            });
            assert!(out.contains("100%"), "{w} 列下百分比被切了:
{out}");
        }
    }

    /// 看一眼网速图:`cargo test tui::pages::tests::preview_chart -- --nocapture`
    #[test]
    fn preview_chart() {
        let sparse: VecDeque<(f64, f64)> =
            vec![(1000.0, 2000.0), (1500.0, 2500.0), (900.0, 1800.0), (1200.0, 2200.0)]
                .into_iter()
                .collect();
        println!("── 只有 4 个点(刚开界面两分钟)──");
        println!("{}", draw_to_string(120, 9, |f| {
            let a = f.area();
            super::net_charts(f, a, &sparse);
        }));

        // 攒满整窗,带真实波动。
        let full: VecDeque<(f64, f64)> = (0..crate::tui::data::HISTORY_LEN)
            .map(|i| {
                let t = i as f64;
                let up = 8000.0 + 5000.0 * (t / 7.0).sin() + 2000.0 * (t / 3.0).cos();
                let down = 20000.0 + 12000.0 * (t / 11.0).cos() + 4000.0 * (t / 2.0).sin();
                (up.max(0.0), down.max(0.0))
            })
            .collect();
        println!("
── 攒满 60 点(1 秒上报 = 一分钟)──");
        println!("{}", draw_to_string(120, 9, |f| {
            let a = f.area();
            super::net_charts(f, a, &full);
        }));
    }

    /// 网速图:不该有轴线竖线,而且**从第一帧起就铺满整幅图宽**。
    ///
    /// 竖线那一条是用户报的:设了 y 轴 labels 之后 ratatui 会腾一列画轴线,
    /// 看起来像图里多出来的一组数据。峰值改写进标题就不需要那一列了。
    ///
    /// 铺满那一条是 sb-manager 的行为:横轴跟着当前点数走,曲线永远占满宽度,
    /// 攒满之后变成往左滚的窗口。v0.3.6 一度改成按固定容量画(右边留白),
    /// 结果图上大半时间是空的 —— 那是在拿留白遮「采样太稀」的丑,
    /// 而真正的根子已经在 data.rs 里治了(改成每次刷新采一个点)。
    #[test]
    fn net_chart_has_no_axis_line_and_fills_the_width() {
        // 才跑了几秒,只有 4 个点。
        let hist: VecDeque<(f64, f64)> =
            vec![(1000.0, 2000.0), (1500.0, 2500.0), (900.0, 1800.0), (1200.0, 2200.0)]
                .into_iter()
                .collect();
        let out = draw_to_string(120, 10, |f| {
            let a = f.area();
            super::net_charts(f, a, &hist);
        });

        // 竖线:框内不该有 `│`。左右两个图各有自己的边框,那是 `┌│└`,
        // 出现在行首/行尾;轴线会出现在**内容区里**。
        for line in out.lines() {
            let inner: String =
                line.chars().skip(1).take(line.chars().count().saturating_sub(2)).collect();
            let inner = inner.replace("││", ""); // 两图相邻处的边框
            assert!(!inner.contains('│'), "图里不该有轴线竖线:\n{out}");
        }

        // 峰值写进标题(原来在 y 轴 labels 里)。
        assert!(has_cjk(&out, "峰值"), "标题里该有峰值:\n{out}");

        // 铺满:曲线要一直画到右半边去,而不是缩在左下角。
        let braille = |s: &str| s.chars().filter(|c| ('\u{2800}'..='\u{28FF}').contains(c)).count();
        let right_half: usize = out
            .lines()
            .map(|l| {
                let n = l.chars().count();
                braille(&l.chars().skip(n / 2).collect::<String>())
            })
            .sum();
        assert!(right_half > 0, "曲线该铺到右半边(和 sb-manager 一样):\n{out}");
    }

    /// 节点视图**不该有进度条**。
    ///
    /// 进度条的含义是「用了多少 / 上限多少」,而节点没有上限 —— 配额只存在于
    /// 用户和整机网卡两个层面(§6.3)。早先这里画的是「占全网份额」,
    /// 可它和别处的配额条长得一模一样,于是一个承载了 95% 流量的健康节点
    /// 看起来像「快爆了」。份额只留百分比,标题里写明口径。
    #[test]
    fn node_view_has_no_progress_bar() {
        let rows = vec![
            NodeRow { cycle_up: 19 << 30, cycle_down: 1 << 30, ..node() },
            NodeRow { id: 2, tag: "osaka".into(), cycle_up: 1 << 30, cycle_down: 0, ..node() },
        ];
        for w in [60u16, 80, 120, 160] {
            let out = draw_to_string(w, 8, |f| {
                let a = f.area();
                super::top_nodes(f, a, &rows, true, None)
            });
            assert!(
                !out.contains('█') && !out.contains('░'),
                "{w} 列下节点视图又画出条形了:
{out}"
            );
            // 份额本身要留着,并且标题得说清那个 % 是什么。
            assert!(out.contains('%'), "{w} 列下份额百分比没了:
{out}");
            assert!(has_cjk(&out, "占全网份额"), "{w} 列下标题没写明口径:
{out}");
        }
    }

    /// 一台被控机都没有的时候,节点视图要指向**真正的下一步**。
    /// 让人去节点页是死路 —— 建节点的表单要先选一台机器。
    #[test]
    fn node_view_points_at_adding_a_machine_first() {
        let out = draw_to_string(60, 6, |f| {
            let a = f.area();
            super::top_nodes(f, a, &[], false, None)
        });
        assert!(has_cjk(&out, "按 [2] 去服务管理页"), "{out}");

        let out = draw_to_string(60, 6, |f| {
            let a = f.area();
            super::top_nodes(f, a, &[], true, None)
        });
        assert!(has_cjk(&out, "按 [3] 去节点页"), "{out}");
    }

    /// 网卡明细的表头:没有配额就不画条(一根满条读起来是「用完了」),
    /// 没有速率读数就写 `--`(写 0 会让「刚打开」和「闲着」看起来一样)。
    #[test]
    fn nic_info_says_unlimited_and_unknown_rather_than_zero() {
        let mut a = agent(None, None);
        a.up_per_sec = None;
        a.down_per_sec = None;
        let out = draw_to_string(100, 10, |f| {
            let area = f.area();
            breakdown(f, area, "t", "h", &nic_info(&a, NOW), &[])
        });
        assert!(has_cjk(&out, "不限流量"), "无配额要写不限:
{out}");
        assert!(out.contains("--"), "没读数要写 --:
{out}");
        assert!(!out.contains("0 B/s"), "不该把没读数显示成 0:
{out}");
    }

    /// 网卡明细同屏摆着两个口径的数字,标题里必须说清哪个是厂商计费的那个。
    /// 不说的话,两个对不上的数字就是一条永远查不明白的「bug」(§6.4)。
    #[test]
    fn nic_info_shows_the_machine_wide_numbers() {
        let a = agent(Some(500 * 1_073_741_824), Some(22));
        let out = draw_to_string(110, 12, |f| {
            let area = f.area();
            breakdown(f, area, "tokyo-1 的网卡明细", "tokyo-1 · 网卡按厂商口径计费", &nic_info(&a, NOW), &[])
        });
        assert!(has_cjk(&out, "网卡本周期"), "{out}");
        assert!(has_cjk(&out, "剩"), "剩余量要写出来,只有百分比不够用:
{out}");
        assert!(has_cjk(&out, "每月 22 日"), "重置日要在:
{out}");
    }

    /// 空库时仪表盘要给出「下一步按什么」,而不是几个空框。
    #[test]
    fn empty_dashboard_tells_you_what_to_do_next() {
        let empty: VecDeque<(f64, f64)> = VecDeque::new();
        let out = draw_to_string(120, 30, |f| dashboard(f, f.area(), &Dash { agents: &[], nodes: &[], users: &[], history: &empty, now: NOW, focus: None }));
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
                // 离线的机器指标是旧的 —— 界面上必须显示 `--`,不是最后一次的数字。
                sysinfo_at: Some(NOW - 3 * 86_400),
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
                cpu_pct: None,
                mem_used: None,
                mem_total: None,
                load1: None,
                uptime_secs: None,
                sysinfo_at: None,
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
                cycle_up: 0,
                cycle_down: 0,
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
                nic_agent_ids: vec![1],
                sub_token: "tok2".into(),
            },
        ];
        // 造一段像样的历史:两个不同周期的正弦,看起来像真的流量。
        let hist: VecDeque<(f64, f64)> = (0..60)
            .map(|i| {
                let t = i as f64;
                (
                    900_000.0 * (1.0 + (t / 7.0).sin()) + 50_000.0,
                    1_800_000.0 * (1.0 + (t / 11.0).cos()) + 80_000.0,
                )
            })
            .collect();

        println!(
            "\n── 仪表盘 ──\n{}\n",
            draw_to_string(120, 30, |f| dashboard(
                f,
                f.area(),
                &Dash {
                    agents: &agents_rows,
                    nodes: &node_rows,
                    users: &user_rows,
                    history: &hist,
                    now: NOW,
                    focus: Some((true, 0)),
                },
            ))
        );
        println!(
            "── 设置 ──\n{}\n",
            draw_to_string(120, 18, |f| settings(
                f,
                f.area(),
                &crate::tui::settings::all(&crate::config::Config::default()),
                2
            ))
        );
        // 行名带倍率标记、数字乘过倍率 —— 与 `data::node_breakdown` 出来的一致。
        // 预览要照着生产的样子造,否则它会替一个不存在的界面背书。
        let bd = vec![
            BreakdownRow {
                label: "alice".into(),
                mult: Some("[2.0x]".into()),
                note: "启用".into(),
                cycle_up: 40 * 1_073_741_824,
                cycle_down: 110 * 1_073_741_824,
                total_up: 240 * 1_073_741_824,
                total_down: 600 * 1_073_741_824,
            },
            BreakdownRow {
                label: "bob".into(),
                mult: Some("[1.0x]".into()),
                note: "自动停用".into(),
                cycle_up: 3 * 1_073_741_824,
                cycle_down: 1_073_741_824,
                total_up: 9 * 1_073_741_824,
                total_down: 4 * 1_073_741_824,
            },
        ];
        println!(
            "── 节点用量明细 ──\n{}\n",
            draw_to_string(120, 12, |f| breakdown(
                f,
                f.area(),
                "节点 tokyo-reality 上的用户",
                "tokyo-reality(在 tokyo-1 上)· 2 个用户 · 数字含各自倍率",
                &[],
                &bd
            ))
        );
        // 服务管理页按 Enter 出来的那一张:上面五行是整机网卡口径,
        // 下面的表是节点(sing-box)口径。两组数字并排,谁也别冒充谁。
        let nic_rows = vec![
            BreakdownRow {
                label: "tokyo-reality".into(),
                mult: None,
                note: "2 人 · vless-reality".into(),
                cycle_up: 20 * 1_073_741_824,
                cycle_down: 55 * 1_073_741_824,
                total_up: 120 * 1_073_741_824,
                total_down: 300 * 1_073_741_824,
            },
            BreakdownRow {
                label: "tokyo-ws".into(),
                mult: None,
                note: "0 人 · vless-ws".into(),
                cycle_up: 0,
                cycle_down: 0,
                total_up: 0,
                total_down: 0,
            },
        ];
        let a = agent(Some(500 * 1_073_741_824), Some(22));
        println!(
            "── 网卡明细(服务管理页 Enter)──\n{}\n",
            draw_to_string(120, 16, |f| breakdown(
                f,
                f.area(),
                "tokyo-1 的网卡明细",
                "tokyo-1 · 2 个节点 · 网卡按厂商口径计费",
                &nic_info(&a, NOW),
                &nic_rows
            ))
        );
        println!("── 服务管理 ──\n{}\n", draw_to_string(120, 12, |f| agents(f, f.area(), &agents_rows, 1, NOW)));
        // 宽一点才画得下「主机」列(CPU / 内存)。
        println!(
            "── 服务管理(140 列,多一个主机列)──\n{}\n",
            draw_to_string(140, 12, |f| agents(f, f.area(), &agents_rows, 1, NOW))
        );
        // 高度不够时自动退回紧凑布局,不留空行。
        println!(
            "── 服务管理(高度不足,紧凑)──\n{}\n",
            draw_to_string(120, 9, |f| agents(f, f.area(), &agents_rows, 1, NOW))
        );
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
