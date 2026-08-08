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

use super::data::{AgentRow, BreakdownRow, NodeRow, UserRow};
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
pub fn dashboard(
    f: &mut Frame,
    area: Rect,
    agents: &[AgentRow],
    nodes: &[NodeRow],
    users: &[UserRow],
    history: &VecDeque<(f64, f64)>,
    now: i64,
) {
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
    top_users(f, mid[0], users, now);
    // 右边是**节点视图**,不是被控机视图。每台机器的网卡明细搬去了服务管理页的
    // 二级页面(按 Enter)—— 网卡是整机口径,和这里两张表用的用户口径不是一回事,
    // 摆在同一屏上并排比只会让人把两个数字当成同一个数字的两次统计(§6.4)。
    top_nodes(f, mid[1], nodes, !agents.is_empty());
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

    let nic_up: i64 = agents.iter().map(|a| a.cycle_rx).sum();
    let nic_down: i64 = agents.iter().map(|a| a.cycle_tx).sum();
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
        line2.push(Span::styled(format!("手动停用 {manual_off}"), Style::default().fg(theme::OFFLINE)));
    }

    let mut speed_line = vec![Span::raw("  当前网速    ")];
    speed_line.extend(speed);

    let usage = vec![
        Span::raw("  本周期用量  "),
        Span::styled("网卡 ", Style::default().fg(theme::DIM)),
        Span::styled(format!("↑ {}", theme::bytes(nic_up)), Style::default().fg(theme::UP)),
        Span::raw(" "),
        Span::styled(format!("↓ {}", theme::bytes(nic_down)), Style::default().fg(theme::DOWN)),
        Span::styled("   计费 ", Style::default().fg(theme::DIM)),
        Span::styled(theme::bytes(billed), Style::default().fg(theme::ACCENT)),
        // 两个数字口径不同,不写清楚会被当成对不上的 bug(§6.4 / §7.2)。
        Span::styled("   (网卡 = 整机进出;计费 = 用户用量 × 倍率)", Style::default().fg(theme::DIM)),
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

fn top_users(f: &mut Frame, area: Rect, users: &[UserRow], now: i64) {
    let mut top: Vec<&UserRow> = users.iter().collect();
    top.sort_by_key(|u| std::cmp::Reverse(u.used()));
    let rows = area.height.saturating_sub(2) as usize;
    let bar_w = (area.width.saturating_sub(46)).clamp(0, 16) as usize;

    let mut lines: Vec<Line> = Vec::new();
    if top.is_empty() {
        lines.push(Line::from(Span::styled(
            "  还没有用户。按 [4] 去用户页,再按 [a] 建一个。",
            Style::default().fg(theme::DIM),
        )));
    }
    for u in top.into_iter().take(rows) {
        let mut spans = vec![
            Span::styled(format!("  {}", theme::pad(&u.name, 12)), Style::default().fg(state_color(u))),
            Span::styled(format!("↑{:>9} ", theme::bytes(u.cycle_up)), Style::default().fg(theme::UP)),
            Span::styled(
                format!("↓{:>9} ", theme::bytes(u.cycle_down)),
                Style::default().fg(theme::DOWN),
            ),
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
        lines.push(Line::from(spans));
    }

    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" 用量 Top ")),
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
fn top_nodes(f: &mut Frame, area: Rect, nodes: &[NodeRow], has_agents: bool) {
    let mut top: Vec<&NodeRow> = nodes.iter().collect();
    top.sort_by_key(|n| std::cmp::Reverse(n.cycle_up.saturating_add(n.cycle_down)));
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
    let fixed = 2 + 11 + 11; // 缩进 + `↑{:>9} ` + `↓{:>9} `
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
    for n in top.into_iter().take(rows) {
        let used = n.cycle_up.saturating_add(n.cycle_down);
        let share = if total > 0 { used as f64 / total as f64 } else { 0.0 };
        let mut spans = vec![
            Span::raw(format!(
                "  {}",
                theme::pad(&format!("{}@{}", n.tag, n.agent_name), label_w)
            )),
            Span::styled(format!("↑{:>9} ", theme::bytes(n.cycle_up)), Style::default().fg(theme::UP)),
            Span::styled(
                format!("↓{:>9} ", theme::bytes(n.cycle_down)),
                Style::default().fg(theme::DOWN),
            ),
        ];
        if show_pct {
            // 份额用中性色。跟着比例变红会把「这个节点承载得多」染成告警,
            // 而它恰恰是正常的 —— 只有配额才有「超了」这回事。
            spans.push(Span::styled(
                format!("{:>4.0}%", share * 100.0),
                Style::default().fg(theme::DIM),
            ));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" 节点用量(% = 占全网份额) ")),
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

    let nic_total = a.cycle_rx.saturating_add(a.cycle_tx);
    out.push(Line::from(vec![
        Span::styled("  网卡本周期  ", Style::default().fg(theme::DIM)),
        Span::styled(format!("↑ {:<10}", theme::bytes(a.cycle_rx)), Style::default().fg(theme::UP)),
        Span::styled(format!("↓ {:<10}", theme::bytes(a.cycle_tx)), Style::default().fg(theme::DOWN)),
        Span::styled("合计 ", Style::default().fg(theme::DIM)),
        Span::styled(theme::bytes(nic_total), Style::default().add_modifier(Modifier::BOLD)),
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

    let mut cycle = vec![
        Span::styled("  重置        ", Style::default().fg(theme::DIM)),
        Span::raw(format!("每月 {}", forms::reset_day_label(a.nic_reset_day))),
    ];
    if let Some(d) = a.nic_reset_day.filter(|d| (1..=31).contains(d)) {
        let today = forms::today_day(now) as i64;
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
        (false, false) => theme::OFFLINE,
    }
}

// ─────────────────────────── agents(两行式)───────────────────────────

/// 「流量」列在宽屏下的目标宽度。
const TRAFFIC_COL: u16 = 38;
/// 「主机」列(CPU / 内存)。只在**宽到有余量**时才出现 ——
/// 它是最锦上添花的一列,不该从流量或重置日那里抢地方。
const HOST_COL: u16 = 22;

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
    /// 0 = 窄到画不下,只显示重置日文字。
    bar: usize,
}

/// 「每月 22 日重置」按终端列宽算约 14 列,留两格余量。
const RESET_LABEL_COLS: u16 = 16;

fn columns(total_width: u16) -> Cols {
    // 减掉左右边框(2),以及 ratatui 在各列之间插的间隔(五列 = 四个;
    // 加上主机列就是五个)。漏算的话总宽超出可用空间,ratatui 会静默压缩各列。
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
    let host = if avail > ideal_sum + HOST_COL { HOST_COL } else { 0 };

    // 进度条用流量列里除去重置日之后剩下的地方。剩不下 4 格就别画了:
    // 三四格的条读不出比例,只是占地方 —— 那时改用文字百分比。
    let bar = traffic.saturating_sub(RESET_LABEL_COLS).min(20) as usize;
    Cols { name, ip, speed, traffic, host, bar: if bar < 4 { 0 } else { bar } }
}

pub fn agents(f: &mut Frame, area: Rect, rows: &[AgentRow], selected: usize, now: i64) {
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

            let mut cells = vec![name_cell, ip_cell, speed_cell, Cell::from(Text::from(vec![line1, line2]))];
            if c.host > 0 {
                cells.push(host_cell);
            }
            cells.push(Cell::from(""));
            Row::new(cells).height(2).style(row_style(i == selected))
        })
        .collect();

    let mut constraints = vec![
        Constraint::Length(c.name),
        Constraint::Length(c.ip),
        Constraint::Length(c.speed),
        Constraint::Length(c.traffic),
    ];
    let mut titles = vec!["名称 / 版本", "IP 地址", "网速", "流量"];
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
    // **没有单独的「详情」面板。** 它显示的东西(tag / 协议 / 端口 / 所属机器)
    // 与底下「操作」面板的第一行逐字重复 —— 同一屏上把同一件事说两遍,
    // 还各占四行。列里放不下的那几项(完整 SNI、中转落点、导出族)
    // 现在归「操作」那一行管(mod.rs::ops_lines)。
    let c = [area];

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
    Some(match n.params.relay.port {
        Some(p) => format!("{}:{}", n.params.relay.host, p),
        None => format!("{}:{}", n.params.relay.host, n.listen_port),
    })
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
        UCol::State => 11,
        UCol::Up => 9,
        UCol::Down => 9,
        UCol::Usage => 20,
        UCol::Bar => 8,
        UCol::Reset => 6,
        // 「2025-12-04 3天」按终端列宽算 14(中文占两格),留一格余量。
        // 少一格的表现是「天」被切掉,而那正是这一列多出来的那点信息。
        UCol::Expire => 15,
        UCol::Mult => 5,
        UCol::Nodes => 4,
        UCol::Nic => 8,
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
                (false, false) => ("○ 手动停用", theme::OFFLINE),
            };
            let cells: Vec<Cell> = cols
                .iter()
                .map(|col| match col {
                    UCol::Name => Cell::from(theme::truncate(&u.name, 11)),                    UCol::State => {
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
                super::settings::Kind::Bool(false) => Style::default().fg(theme::OFFLINE),
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
        let mut spans = vec![
            Span::raw("  "),
            Span::raw(theme::pad(&r.label, 22)),
            Span::styled(format!("↑{:>9} ", theme::bytes(r.cycle_up)), Style::default().fg(theme::UP)),
            Span::styled(
                format!("↓{:>9}  ", theme::bytes(r.cycle_down)),
                Style::default().fg(theme::DOWN),
            ),
        ];
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
            arch: Some("amd64".into()),
            ipv4: Some("203.0.113.8".into()),
            ipv6: Some("2001:db8:1:aaaa:1234:5678:9abc:def0".into()),
            nic_quota_bytes: quota,
            nic_reset_day: day,
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
            draw_to_string(w, h, |f| agents(f, f.area(), &[agent(Some(1000), Some(1))], 0, NOW));
            draw_to_string(w, h, |f| nodes(f, f.area(), &[node()], 0));
            draw_to_string(w, h, |f| users(f, f.area(), &[user()], 0, "https://x.example", NOW));
            let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
            draw_to_string(w, h, |f| {
                dashboard(f, f.area(), &[agent(None, None)], &[node()], &[user()], &hist, NOW)
            });
            draw_to_string(w, h, |f| {
                settings(f, f.area(), &crate::tui::settings::all(&crate::config::Config::default()), 0)
            });
            draw_to_string(w, h, |f| breakdown(f, f.area(), "t", "h", &[], &[]));
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
        assert!(has_cjk(&out, "无需重置"), "加了主机列不该把流量列挤掉:\n{out}");
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
            dashboard(f, f.area(), &[agent(Some(1000), Some(1)), offline], &[node()], &[user()], &hist, NOW)
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
            dashboard(f, f.area(), &[agent(None, None)], &[], &[], &hist, NOW)
        });
        assert!(has_cjk(&out, "还在攒数据"), "{out}");
    }

    /// 矮终端上整块让掉折线图 —— 一条挤成两行的曲线不如把地方给下面的数字。
    #[test]
    fn a_short_terminal_drops_the_chart_not_the_numbers() {
        let hist: VecDeque<(f64, f64)> = (0..40).map(|i| (i as f64, i as f64)).collect();
        let out = draw_to_string(120, 18, |f| {
            dashboard(f, f.area(), &[agent(None, None)], &[node()], &[user()], &hist, NOW)
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
            dashboard(f, f.area(), &[agent(Some(1 << 40), Some(22))], &[node()], &[user()], &hist, NOW)
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
                super::top_nodes(f, a, &rows, true)
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
                super::top_nodes(f, a, &rows, true)
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
            super::top_nodes(f, a, &[], false)
        });
        assert!(has_cjk(&out, "按 [2] 去服务管理页"), "{out}");

        let out = draw_to_string(60, 6, |f| {
            let a = f.area();
            super::top_nodes(f, a, &[], true)
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
        let out = draw_to_string(120, 30, |f| dashboard(f, f.area(), &[], &[], &[], &empty, NOW));
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
                &agents_rows,
                &node_rows,
                &user_rows,
                &hist,
                NOW
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
        let bd = vec![
            BreakdownRow {
                label: "alice".into(),
                note: "启用".into(),
                cycle_up: 20 * 1_073_741_824,
                cycle_down: 55 * 1_073_741_824,
                total_up: 120 * 1_073_741_824,
                total_down: 300 * 1_073_741_824,
            },
            BreakdownRow {
                label: "bob".into(),
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
                "tokyo-reality(在 tokyo-1 上)· 2 个用户",
                &[],
                &bd
            ))
        );
        // 服务管理页按 Enter 出来的那一张:上面五行是整机网卡口径,
        // 下面的表是节点(sing-box)口径。两组数字并排,谁也别冒充谁。
        let nic_rows = vec![
            BreakdownRow {
                label: "tokyo-reality".into(),
                note: "2 人 · vless-reality".into(),
                cycle_up: 20 * 1_073_741_824,
                cycle_down: 55 * 1_073_741_824,
                total_up: 120 * 1_073_741_824,
                total_down: 300 * 1_073_741_824,
            },
            BreakdownRow {
                label: "tokyo-ws".into(),
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
        println!("── 服务管理 ──\n{}\n", draw_to_string(120, 9, |f| agents(f, f.area(), &agents_rows, 1, NOW)));
        // 宽一点才画得下「主机」列(CPU / 内存)。
        println!(
            "── 服务管理(140 列,多一个主机列)──\n{}\n",
            draw_to_string(140, 9, |f| agents(f, f.area(), &agents_rows, 1, NOW))
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
