//! TUI 的三个页面(DESIGN.md §8.2)。
//!
//! agents 页是**两行式**的:`Row::new(cells).height(2)`,每个 cell 装一个
//! 两行的 `Text`。这与旧项目 `tui/pages/nodes.rs` 是同一套写法,
//! 唯一区别就是行高从 1 变 2。列宽用 `Constraint::Length`,
//! 最后留一列 `Min(0)` 吃掉多余宽度 —— 不留的话表格会被拉伸,列宽跟着终端晃。

use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Row, Table},
    Frame,
};

use super::data::{AgentRow, NodeRow, UserRow};
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
        Style::default().bg(ratatui::style::Color::Rgb(0x2a, 0x2a, 0x2a)).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

// ─────────────────────────── agents(两行式)───────────────────────────

/// 「流量」列在宽屏下的目标宽度。
const TRAFFIC_COL: u16 = 38;

/// agents 页的列宽。
///
/// 固定宽度在窄终端上会**静默截断**:80 列时「每月 22 日重置」会被切成「每 」,
/// IPv6 连省略号都放不下 —— 界面看起来还很正常,只是少了信息(§13.4 要求专门看这个)。
/// 所以列宽跟着可用宽度走,并且把进度条当成**可牺牲**的那一项:
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
    // 减掉左右边框(2),以及 ratatui 在五个列之间插的四个间隔。
    // 漏算间隔的话总宽会超出可用空间,ratatui 会**静默压缩**各列 ——
    // 表现就是省略号都放不下、重置日被切掉半截。
    let avail = total_width.saturating_sub(2 + 4);
    const IDEAL: (u16, u16, u16, u16) = (18, 22, 14, TRAFFIC_COL);
    const NARROW: (u16, u16, u16, u16) = (14, 18, 13, 0);
    let narrow_fixed = NARROW.0 + NARROW.1 + NARROW.2;

    let (name, ip, speed, traffic) = if avail >= IDEAL.0 + IDEAL.1 + IDEAL.2 + IDEAL.3 {
        IDEAL
    } else if avail > narrow_fixed {
        // 窄屏:三个固定列收一点,余下全给「流量」——那一列信息密度最高。
        (NARROW.0, NARROW.1, NARROW.2, (avail - narrow_fixed).min(TRAFFIC_COL))
    } else {
        // 极窄:连收缩后的固定列都放不下。按比例分,保证**总和不超出** ——
        // 超出的话 ratatui 会自己压缩,那时连省略号都画不出来。
        // 这个宽度下界面已经没什么用了,但至少不该是一团乱码。
        let unit = avail / 4;
        (unit, unit, unit, avail.saturating_sub(unit * 3))
    };

    // 进度条用流量列里除去重置日之后剩下的地方。剩不下 4 格就别画了:
    // 三四格的条读不出比例,只是占地方 —— 那时改用文字百分比。
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

            // 截断长度按列宽算,留出圆点和缩进占的两格。
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
                // 比例这个信息不能丢 —— 它才是这一列存在的理由。
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

pub fn nodes(f: &mut Frame, area: Rect, rows: &[NodeRow], selected: usize) {
    let table_rows: Vec<Row> = rows
        .iter()
        .enumerate()
        .map(|(i, n)| {
            Row::new(vec![
                Cell::from(n.id.to_string()),
                Cell::from(theme::truncate(&n.agent_name, 16)),
                Cell::from(theme::truncate(&n.tag, 22)),
                Cell::from(n.protocol.clone()),
                Cell::from(n.listen_port.to_string()),
                Cell::from(n.user_count.to_string()),
                Cell::from(""),
            ])
            .style(row_style(i == selected))
        })
        .collect();

    f.render_widget(
        Table::new(
            table_rows,
            [
                Constraint::Length(5),
                Constraint::Length(18),
                Constraint::Length(24),
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Min(0),
            ],
        )
        .header(header(&["#", "所属 agent", "Tag", "协议", "端口", "用户数", ""]))
        .block(Block::default().borders(Borders::ALL).title(" 节点 ")),
        area,
    );
}

// ─────────────────────────── users ───────────────────────────

pub fn users(f: &mut Frame, area: Rect, rows: &[UserRow], selected: usize) {
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
            let quota = if u.quota_bytes > 0 {
                format!("{} / {}", theme::bytes(u.used()), theme::bytes(u.quota_bytes))
            } else {
                format!("{} · 不限", theme::bytes(u.used()))
            };
            let bar: Vec<Span> = match u.quota_ratio() {
                Some(r) => theme::gradient_bar(r, 16),
                None => vec![Span::styled("—", Style::default().fg(theme::DIM))],
            };

            Row::new(vec![
                Cell::from(theme::truncate(&u.name, 18)),
                Cell::from(Span::styled(mark.to_string(), Style::default().fg(color))),
                Cell::from(quota),
                Cell::from(Line::from(bar)),
                Cell::from(u.node_count.to_string()),
                Cell::from(expire_label(u.expire_at)),
                Cell::from(""),
            ])
            .style(row_style(i == selected))
        })
        .collect();

    f.render_widget(
        Table::new(
            table_rows,
            [
                Constraint::Length(20),
                Constraint::Length(14),
                Constraint::Length(26),
                Constraint::Length(18),
                Constraint::Length(8),
                Constraint::Length(14),
                Constraint::Min(0),
            ],
        )
        .header(header(&["用户", "状态", "本周期用量", "", "节点", "到期", ""]))
        .block(Block::default().borders(Borders::ALL).title(" 用户 ")),
        area,
    );
}

fn expire_label(expire_at: Option<i64>) -> String {
    match expire_at {
        None => "—".into(),
        Some(ts) => chrono::DateTime::from_timestamp(ts, 0)
            .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "?".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn reset_label_covers_the_null_case() {
        assert_eq!(reset_label(Some(22)), "每月 22 日重置");
        assert_eq!(reset_label(None), "无需重置");
        // 库里存了越界的值时不该显示「每月 99 日重置」。
        assert_eq!(reset_label(Some(0)), "无需重置");
        assert_eq!(reset_label(Some(32)), "无需重置");
    }

    #[test]
    fn expire_label_handles_missing_and_bogus_timestamps() {
        assert_eq!(expire_label(None), "—");
        assert_eq!(expire_label(Some(i64::MAX)), "?", "坏时间戳不该 panic");
        assert_eq!(expire_label(Some(0)).len(), 10);
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
        // 重置日整段都要在,不能被列宽截掉半截。
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

    /// 超长的 IPv6 要截断并补省略号,不能把列撑破。
    #[test]
    fn long_addresses_are_truncated() {
        let out = render_agents(&[agent(None, None)]);
        assert!(out.contains('…'), "超宽的 IPv6 应当被截断\n{out}");
    }

    /// 空列表只画表头,不 panic。
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
        for (w, h) in [(1u16, 1u16), (10, 3), (30, 5), (200, 60)] {
            draw_to_string(w, h, |f| agents(f, f.area(), &[agent(Some(1000), Some(1))], 0));
            draw_to_string(w, h, |f| nodes(f, f.area(), &[], 0));
            draw_to_string(w, h, |f| users(f, f.area(), &[], 0));
        }
    }

    /// §13.4:80 列的窄终端下,**重置日不能被截断**,IPv6 要留住省略号。
    ///
    /// 这是从真实渲染里发现的回归锚点:早先固定列宽的版本在 80 列下把
    /// 「每月 22 日重置」切成了「每 」,而界面看起来完全正常 —— 只是少了信息。
    /// 根因是列宽没算 ratatui 在列之间插的四个间隔,总宽超出后各列被静默压缩。
    #[test]
    fn narrow_terminal_keeps_the_reset_day_intact() {
        let out = draw_to_string(80, 8, |f| {
            agents(f, f.area(), &[agent(Some(500 * 1_073_741_824), Some(22))], 0)
        });
        assert!(has_cjk(&out, "每月 22 日重置"), "80 列下重置日被截断了:\n{out}");
        assert!(out.contains('…'), "IPv6 应当截断并保留省略号:\n{out}");
        assert!(out.contains('█'), "80 列下仍应画得下进度条:\n{out}");
    }

    /// 列宽必须始终塞得进可用空间(边框 + 列间隔算在内),
    /// 否则 ratatui 会压缩各列,省略号和标签就都放不下了。
    #[test]
    fn column_widths_always_fit() {
        for w in 20..200u16 {
            let c = columns(w);
            let used = c.name + c.ip + c.speed + c.traffic;
            let avail = w.saturating_sub(2 + 4);
            assert!(used <= avail.max(1) || avail == 0, "宽度 {w}:列合计 {used} > 可用 {avail}");
        }
    }

    /// 窄到画不下进度条时,比例改用文字给出 —— 不能连比例都看不到了。
    #[test]
    fn percentage_moves_to_text_when_the_bar_does_not_fit() {
        let out = draw_to_string(70, 6, |f| {
            agents(f, f.area(), &[agent(Some(100 * 1_073_741_824), Some(22))], 0)
        });
        assert!(!out.contains('█'), "这么窄不该画条:\n{out}");
        assert!(out.contains("34%"), "画不下条时要给文字百分比:\n{out}");
        assert!(has_cjk(&out, "每月 22 日重置"), "重置日仍要在:\n{out}");
    }

    /// 「系统自动停用」和「管理员手动停用」必须能一眼分开(§6.3):
    /// 前者到了重置日会自己恢复,后者不会。
    #[test]
    fn user_page_distinguishes_auto_from_manual_disable() {
        let base = UserRow {
            id: 1,
            name: "alice".into(),
            enabled: false,
            auto_disabled: true,
            quota_bytes: 0,
            cycle_up: 0,
            cycle_down: 0,
            traffic_multiplier: 1.0,
            expire_at: None,
            node_count: 0,
            sub_token: "t".into(),
        };
        let render = |u: UserRow| draw_to_string(110, 6, move |f| users(f, f.area(), &[u], 0));

        assert!(has_cjk(&render(base.clone()), "自动停用"));
        assert!(has_cjk(&render(UserRow { auto_disabled: false, ..base.clone() }), "手动停用"));
        assert!(has_cjk(&render(UserRow { enabled: true, ..base }), "启用"));
    }

    /// 把三个页面画出来打到 stdout,给人看一眼:
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
        println!("\n{}\n", draw_to_string(120, 9, |f| agents(f, f.area(), &agents_rows, 1)));
        // §13.4 要求窄终端也看一眼:80 列是最常见的下限。
        println!("── 80 列 ──\n{}\n", draw_to_string(80, 9, |f| agents(f, f.area(), &agents_rows, 1)));

        let node_rows = vec![NodeRow {
            id: 1,
            agent_id: 1,
            agent_name: "tokyo-1".into(),
            tag: "tokyo-reality".into(),
            protocol: "vless-reality".into(),
            listen_port: 8443,
            user_count: 3,
        }];
        println!("{}\n", draw_to_string(120, 5, |f| nodes(f, f.area(), &node_rows, 0)));

        let user_rows = vec![UserRow {
            id: 1,
            name: "alice".into(),
            enabled: true,
            auto_disabled: false,
            quota_bytes: 100 * 1_073_741_824,
            cycle_up: 20 * 1_073_741_824,
            cycle_down: 55 * 1_073_741_824,
            traffic_multiplier: 1.0,
            expire_at: Some(1_893_456_000),
            node_count: 2,
            sub_token: "t".into(),
        }];
        println!("{}\n", draw_to_string(120, 5, |f| users(f, f.area(), &user_rows, 0)));
    }
}
