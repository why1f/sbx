//! TUI 的弹窗:输入表单、确认框、只读信息框(DESIGN.md §8.1)。
//!
//! 三种弹窗覆盖了全部需要停下来跟人确认的动作:
//!   * `Input`   —— 新增 agent / 节点 / 用户;
//!   * `Confirm` —— 删除类操作,**并且要说清影响面**(删 agent 会连带删它的节点);
//!   * `Info`    —— token 只显示这一次,以及订阅地址。
//!
//! token 弹窗是这里最要紧的一个:库里只存 `token_hash` 与 `token_prefix`,
//! 明文关掉就再也拿不回来了(§8.1)。所以它必须**显式**告诉人这件事。

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use super::theme;

/// 弹窗确认后要执行的动作。**按键处理只负责产生它,不直接碰数据库** ——
/// 实际执行在主循环里统一做,这样每个动作的错误处理和刷新时机只有一处。
#[derive(Debug, Clone)]
pub enum Action {
    AddAgent { name: String },
    RotateToken { id: i64, name: String },
    DeleteAgent { id: i64, name: String },
    AddNode { agent_id: i64, tag: String, port: String, protocol: String },
    DeleteNode { id: i64, tag: String },
    AddUser { name: String, quota_gb: String },
    SetUserEnabled { name: String, enabled: bool },
    DeleteUser { name: String },
    AssignNode { user: String, node_id: i64 },
}

#[derive(Debug, Clone)]
pub struct Field {
    pub label: String,
    pub value: String,
    /// 灰色提示,写清默认值或格式。表单里最贵的错误是「填错了但看起来像对的」。
    pub hint: String,
}

impl Field {
    pub fn new(label: &str, hint: &str) -> Self {
        Self { label: label.into(), value: String::new(), hint: hint.into() }
    }
    pub fn with(label: &str, value: &str, hint: &str) -> Self {
        Self { label: label.into(), value: value.into(), hint: hint.into() }
    }
}

pub enum Modal {
    Input {
        title: String,
        fields: Vec<Field>,
        focus: usize,
        /// 拿填好的字段值构造动作。返回 `Err` 表示校验没过,消息直接显示在弹窗里。
        build: fn(&[Field]) -> Result<Action, String>,
        error: Option<String>,
    },
    Confirm {
        title: String,
        body: Vec<String>,
        action: Action,
    },
    Info {
        title: String,
        body: Vec<String>,
    },
}

impl Modal {
    pub fn input(title: &str, fields: Vec<Field>, build: fn(&[Field]) -> Result<Action, String>) -> Self {
        Modal::Input { title: title.into(), fields, focus: 0, build, error: None }
    }

    pub fn confirm(title: &str, body: Vec<String>, action: Action) -> Self {
        Modal::Confirm { title: title.into(), body, action }
    }

    pub fn info(title: &str, body: Vec<String>) -> Self {
        Modal::Info { title: title.into(), body }
    }
}

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
        Modal::Input { title, fields, focus, error, .. } => {
            // 每个字段两行(标签 + 输入框),再加边框、提示行和可能的错误行。
            let h = (fields.len() as u16 * 3) + 5 + u16::from(error.is_some());
            let rect = centered(area, 60, 40, 44, h.max(9));
            f.render_widget(Clear, rect);

            let mut lines: Vec<Line> = Vec::new();
            for (i, fld) in fields.iter().enumerate() {
                let focused = i == *focus;
                let marker = if focused { "▸ " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(marker, Style::default().fg(theme::ACCENT)),
                    Span::styled(
                        format!("{}: ", fld.label),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    // 光标只画在当前字段上,避免同时出现多个「正在输入」的错觉。
                    Span::raw(fld.value.clone()),
                    if focused { Span::styled("█", Style::default().fg(theme::ACCENT)) } else { Span::raw("") },
                ]));
                lines.push(Line::from(Span::styled(
                    format!("    {}", fld.hint),
                    Style::default().fg(theme::DIM),
                )));
                lines.push(Line::from(""));
            }
            if let Some(e) = error {
                lines.push(Line::from(Span::styled(
                    format!("  {e}"),
                    Style::default().fg(ratatui::style::Color::Red),
                )));
            }
            lines.push(Line::from(Span::styled(
                "  [Tab]切换字段  [Enter]确定  [Esc]取消",
                Style::default().fg(theme::DIM),
            )));

            f.render_widget(
                Paragraph::new(lines)
                    .block(Block::default().borders(Borders::ALL).title(format!(" {title} ")))
                    .wrap(Wrap { trim: false }),
                rect,
            );
        }

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
                            .border_style(Style::default().fg(ratatui::style::Color::Red)),
                    )
                    .wrap(Wrap { trim: false }),
                rect,
            );
        }

        Modal::Info { title, body } => {
            let rect = centered(area, 76, 50, 50, body.len() as u16 + 4);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = body.iter().map(|l| Line::from(format!("  {l}"))).collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [任意键]关闭",
                Style::default().fg(theme::DIM),
            )));
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

/// 底部状态条。
pub fn status_bar(f: &mut Frame, area: Rect, text: &str, is_error: bool) {
    let style = if is_error {
        Style::default().fg(ratatui::style::Color::Red)
    } else {
        Style::default().fg(theme::DIM)
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(text.to_string(), style))), area);
}

/// 顶部页签。
pub fn tabs(f: &mut Frame, area: Rect, titles: &[&str], selected: usize) {
    let mut spans = vec![Span::raw(" ")];
    for (i, t) in titles.iter().enumerate() {
        let style = if i == selected {
            Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(theme::DIM)
        };
        spans.push(Span::styled(format!(" {t} "), style));
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

    /// 终端被拉到很小时,百分比会算出 0 宽/0 高 —— 弹窗直接消失。
    /// 下限保证它至少还在,哪怕会被裁掉一部分。
    #[test]
    fn centered_respects_minimum_size() {
        let tiny = Rect { x: 0, y: 0, width: 20, height: 6 };
        let r = centered(tiny, 60, 40, 44, 9);
        assert!(r.width > 0 && r.height > 0);
        // 不能超出屏幕,否则渲染会 panic。
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
}
