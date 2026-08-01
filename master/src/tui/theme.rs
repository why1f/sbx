//! TUI 的配色与自绘部件(DESIGN.md §8.2)。
//!
//! 这里只有两样东西是「设计」而不是「代码风格」的:渐变进度条和字节格式化。
//! 其余颜色常量集中放这里,是为了让 agents / nodes / users 三个页面看起来是一套东西。

use ratatui::style::Color;
use ratatui::text::Span;

pub const ONLINE: Color = Color::Rgb(0x3d, 0xdc, 0x84);
pub const OFFLINE: Color = Color::Rgb(0x9a, 0x9a, 0x9a);
/// 「从没连上过」与「连过又断了」要分开:前者通常是 token 没贴对或防火墙,
/// 后者是网络或进程问题,排查方向完全不同(model/agent.rs 同样的理由)。
pub const NEVER: Color = Color::Rgb(0xe5, 0x9a, 0x3a);
pub const UP: Color = Color::Rgb(0x3d, 0xdc, 0x84);
pub const DOWN: Color = Color::Rgb(0x5a, 0x9d, 0xf0);
pub const DIM: Color = Color::Rgb(0x7a, 0x7a, 0x7a);
pub const TRACK: Color = Color::Rgb(0x3a, 0x3a, 0x3a);
pub const ACCENT: Color = Color::Rgb(0xf5, 0xc4, 0x51);

/// 进度条的颜色停靠点:绿 → 黄 → 橙 → 红(§8.2)。
///
/// 位置是「用了百分之多少」,颜色在相邻停靠点之间线性插值。
/// 分段而不是单一渐变,是为了让「快满了」在视觉上是一个突变而不是缓慢变化 ——
/// 一眼扫过去要能看出哪台机器要爆了。
const STOPS: [(f64, (u8, u8, u8)); 4] = [
    (0.00, (0x3d, 0xdc, 0x84)),
    (0.50, (0xf5, 0xc4, 0x51)),
    (0.80, (0xf0, 0x8c, 0x3a)),
    (1.00, (0xe5, 0x48, 0x4d)),
];

fn lerp(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * t).round().clamp(0.0, 255.0) as u8
}

/// 取渐变上某一点的颜色。`pos` 会被夹到 `[0, 1]`。
pub fn gradient_at(pos: f64) -> Color {
    let pos = pos.clamp(0.0, 1.0);
    for w in STOPS.windows(2) {
        let (p0, c0) = w[0];
        let (p1, c1) = w[1];
        if pos <= p1 {
            // p1 == p0 时 t 会是 NaN,所以这里防一手(当前 STOPS 不会出现,
            // 但改停靠点的人不该因此看到一片黑)。
            let t = if (p1 - p0).abs() < f64::EPSILON { 0.0 } else { (pos - p0) / (p1 - p0) };
            return Color::Rgb(lerp(c0.0, c1.0, t), lerp(c0.1, c1.1, t), lerp(c0.2, c1.2, t));
        }
    }
    let last = STOPS[STOPS.len() - 1].1;
    Color::Rgb(last.0, last.1, last.2)
}

/// 自绘渐变进度条:每格一个字符,颜色按**该格在条上的位置**取渐变。
///
/// **不能用 ratatui 的 `Gauge`** —— 它只支持单色(§8.2)。
///
/// 注意颜色取的是格子自身的位置,不是整体百分比:这样 30% 和 90% 的条,
/// 左边那 30 格颜色完全一样,视觉上是「同一条尺子上填到不同位置」,
/// 而不是「整条变色」。后者会让人误以为颜色本身有额外含义。
pub fn gradient_bar(ratio: f64, width: usize) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let ratio = ratio.clamp(0.0, 1.0);
    // 至少填一格:非零用量显示成空条会让人以为没在跑。
    let filled = if ratio <= 0.0 {
        0
    } else {
        ((ratio * width as f64).round() as usize).clamp(1, width)
    };

    (0..width)
        .map(|i| {
            if i < filled {
                let pos = if width <= 1 { ratio } else { i as f64 / (width - 1) as f64 };
                Span::styled("█", ratatui::style::Style::default().fg(gradient_at(pos)))
            } else {
                Span::styled("░", ratatui::style::Style::default().fg(TRACK))
            }
        })
        .collect()
}

/// 人类可读的字节数。与 `model::user::User::format_bytes` 是同一套口径 ——
/// 直接转调,不另起一份,免得列表里和详情里显示出不同的数字。
pub fn bytes(n: i64) -> String {
    crate::model::user::User::format_bytes(n)
}

/// 速率。单位固定跟着字节走,后面加 `/s`。
pub fn rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec < 0.0 {
        return "--".into();
    }
    format!("{}/s", bytes(bytes_per_sec.round() as i64))
}

/// 按显示宽度截断,尾部补 `…`。
///
/// 按 `char` 而不是字节切:IPv6 全是 ASCII 无所谓,但 agent 名和节点 tag
/// 可以是中文,按字节切会切出半个字符。
///
/// 这不是完美的东亚宽度处理(中文占两列),但列表里的截断只需要「不撑破布局」,
/// 而 ratatui 自己会再兜一层。
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    s.chars().take(keep).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_runs_green_to_red() {
        assert_eq!(gradient_at(0.0), Color::Rgb(0x3d, 0xdc, 0x84));
        assert_eq!(gradient_at(0.5), Color::Rgb(0xf5, 0xc4, 0x51));
        assert_eq!(gradient_at(1.0), Color::Rgb(0xe5, 0x48, 0x4d));
        // 越界不该 panic,也不该回绕成绿色。
        assert_eq!(gradient_at(-1.0), Color::Rgb(0x3d, 0xdc, 0x84));
        assert_eq!(gradient_at(9.0), Color::Rgb(0xe5, 0x48, 0x4d));
    }

    /// 绿色分量随占用**单调不增** —— 这是「越满越警示」这个直觉的形式化。
    ///
    /// 注意不能拿红色分量来测:黄(0xf5)的红比橙(0xf0)还高一点,
    /// 所以红色在 50%→80% 之间会小幅回落。绿色才是这条渐变里单调的那一维
    /// (0xdc → 0xc4 → 0x8c → 0x48)。
    #[test]
    fn gradient_gets_less_green_as_it_fills() {
        let mut last = 255u8;
        for i in 0..=100 {
            let Color::Rgb(_, g, _) = gradient_at(i as f64 / 100.0) else {
                panic!("应当是 Rgb")
            };
            assert!(g <= last, "在 {i}% 处绿色分量回升了:{last} → {g}");
            last = g;
        }
    }

    #[test]
    fn bar_fills_the_right_number_of_cells() {
        let count_filled = |spans: &[Span]| spans.iter().filter(|s| s.content == "█").count();
        assert_eq!(count_filled(&gradient_bar(0.0, 10)), 0);
        assert_eq!(count_filled(&gradient_bar(0.5, 10)), 5);
        assert_eq!(count_filled(&gradient_bar(1.0, 10)), 10);
        // 每一格都要有,不能因为没填就漏掉 —— 否则条会变短,右边界跟着晃。
        assert_eq!(gradient_bar(0.3, 10).len(), 10);
    }

    /// 用了一点点也要显示一格。四舍五入到 0 会让人以为完全没流量。
    #[test]
    fn tiny_usage_still_shows_one_cell() {
        let spans = gradient_bar(0.001, 20);
        assert_eq!(spans.iter().filter(|s| s.content == "█").count(), 1);
    }

    #[test]
    fn bar_handles_degenerate_sizes() {
        assert!(gradient_bar(0.5, 0).is_empty());
        assert_eq!(gradient_bar(0.5, 1).len(), 1);
        // 超界的 ratio 不该 panic 也不该溢出格数
        assert_eq!(gradient_bar(5.0, 4).iter().filter(|s| s.content == "█").count(), 4);
    }

    #[test]
    fn truncate_keeps_char_boundaries() {
        assert_eq!(truncate("abcdef", 10), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // 中文按字符切,不该切出半个字符(按字节切会 panic 或出乱码)。
        assert_eq!(truncate("东京节点一号", 3), "东京…");
        assert_eq!(truncate("2001:db8:1:aaaa::1", 12), "2001:db8:1:…");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn rate_rejects_nonsense_values() {
        assert_eq!(rate(f64::NAN), "--");
        assert_eq!(rate(-1.0), "--");
        assert!(rate(1024.0).ends_with("/s"));
    }
}
