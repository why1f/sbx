//! TUI 的配色与自绘部件(DESIGN.md §8.2)。
//!
//! 这里只有两样东西是「设计」而不是「代码风格」的:渐变进度条和字节格式化。
//! 其余颜色常量集中放这里,是为了让 agents / nodes / users 三个页面看起来是一套东西。

use ratatui::style::Color;
use ratatui::text::Span;

pub const ONLINE: Color = Color::Rgb(0x3d, 0xdc, 0x84);
/// **只给「agent 掉线」用。** 停用的用户、关掉的开关请用 `INACTIVE`。
///
/// 掉线是红的,不是灰的:灰读作「这一项没启用」,一眼扫过去会跳过它;
/// 而一台掉线的机器上跑着的用户此刻全部连不上,那是要立刻处理的事。
///
/// 用的是进度条最后那一档同一个红(`STOPS` 的 1.00)—— 这套界面里
/// 「红 = 出事了」只该有一个色号。
pub const OFFLINE: Color = Color::Rgb(0xe5, 0x48, 0x4d);
/// 「关着 / 停用 / 没启用」。这些是**正常状态**,不是故障 ——
/// 管理员手动停用一个用户、把某个开关设成 false,都不该染成告警色。
///
/// 与 `OFFLINE` 分开,是因为原先它们共用一个常量:掉线改红的时候,
/// 设置页里一个 `false` 会跟着变成刺眼的红,而那什么事都没有。
pub const INACTIVE: Color = Color::Rgb(0x9a, 0x9a, 0x9a);
/// 「从没连上过」与「连过又断了」要分开:前者通常是 token 没贴对或防火墙,
/// 后者是网络或进程问题,排查方向完全不同(model/agent.rs 同样的理由)。
///
/// 橙和红在某些终端配色下会靠得比较近,所以这两态**还靠形状分**:
/// 掉线是实心 `●`,从未连接是空心 `○`(见 `pages::agents`)。
/// 只靠颜色区分的话,色觉障碍的人这里就少了一个信息维度。
pub const NEVER: Color = Color::Rgb(0xe5, 0x9a, 0x3a);
pub const UP: Color = Color::Rgb(0x3d, 0xdc, 0x84);
pub const DOWN: Color = Color::Rgb(0x5a, 0x9d, 0xf0);
pub const DIM: Color = Color::Rgb(0x7a, 0x7a, 0x7a);
pub const TRACK: Color = Color::Rgb(0x3a, 0x3a, 0x3a);
pub const ACCENT: Color = Color::Rgb(0xf5, 0xc4, 0x51);

/// 表单里聚焦那一格的底色。
///
/// **不用 ACCENT 做底。** 黄底配深色前景在很多终端配色下会被主题改掉前景色,
/// 结果是黄底浅字,对比度反而比不选中还低。深灰蓝底 + 原色前景在任何配色下
/// 都是「亮一块」,不依赖终端把 `Color::Black` 渲染成真的黑。
pub const SELECT_BG: Color = Color::Rgb(0x2d, 0x3a, 0x4e);
/// 表格里选中行的底色。比 `SELECT_BG` 淡一点 —— 那是「光标在这一行」,
/// 不是「正在编辑这一格」,两者不该一样强。
pub const ROW_BG: Color = Color::Rgb(0x2a, 0x2a, 0x2a);

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

/// 一个字符占几列。东亚宽字符(中日韩、全角标点)占两列,其余按一列算。
///
/// 需要这个是因为 `format!("{:<12}", s)` 补的是**字符数**,不是列数:
/// 「计数器重置」5 个字符会被当成还差 7 格,于是补出 7 个空格,
/// 而它实际已经占了 10 列 —— 后面的列全部错位。
fn char_cols(c: char) -> usize {
    let u = c as u32;
    let wide = (0x1100..=0x115F).contains(&u)      // 韩文字母
        || (0x2E80..=0xA4CF).contains(&u)          // CJK 部首 → 汉字 → 注音
        || (0xAC00..=0xD7A3).contains(&u)          // 韩文音节
        || (0xF900..=0xFAFF).contains(&u)          // CJK 兼容汉字
        || (0xFE30..=0xFE6F).contains(&u)          // CJK 兼容形式
        || (0xFF00..=0xFF60).contains(&u)          // 全角 ASCII
        || (0xFFE0..=0xFFE6).contains(&u)          // 全角符号
        || (0x20000..=0x3FFFD).contains(&u); // CJK 扩展
    if wide {
        2
    } else {
        1
    }
}

/// 字符串占几列。
pub fn cols(s: &str) -> usize {
    s.chars().map(char_cols).sum()
}

/// 按**显示宽度**截断,尾部补 `…`。
///
/// 按列而不是按字符切:agent 名和节点 tag 可以是中文,一个中文占两列,
/// 按字符数算会让「东京节点一号」这样的名字把列撑破一倍。
/// 也不能按字节切 —— 那会切出半个字符(直接乱码或 panic)。
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if cols(s) <= max {
        return s.to_string();
    }
    // 省略号自己占一列。
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_cols(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// 截断到 `w` 列,再用空格补齐到正好 `w` 列。
///
/// `Paragraph` 里手工排的列要用它,不能用 `{:<w}` —— 见 `char_cols` 的说明。
pub fn pad(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    let mut out = t;
    for _ in cols(&out)..w {
        out.push(' ');
    }
    out
}

/// 按显示宽度折行,每行都不超过 `width` 列。
///
/// **为什么不交给 ratatui 的 `Wrap`。** 两个原因,都不是审美问题:
///   1. 它折出来的行数**算不进我们自己的高度计算**,于是弹窗底下几行被静默裁掉
///      —— 表现是「说明只有半句」。
///   2. 它的续行顶到最左边,和下一条说明的起始位置一样,读起来像另起了一条。
///
/// 优先在空格处断,断不了就硬断:中文没有空格,一律等空格会让一整段挤成一行。
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = char_cols(c);
        // `used > 0` 是必须的:一个宽字符比整行还宽时,不加这条会空推无数行。
        if used > 0 && used + w > width {
            // 回退到最后一个空格断行,免得把一个英文单词劈成两半。
            // 只在尾巴不长时回退 —— 否则一行只剩几个字,反而更难读。
            match line.rfind(' ') {
                Some(i) if i + 1 < line.len() && cols(&line[i + 1..]) <= 16 => {
                    let tail = line[i + 1..].to_string();
                    line.truncate(i);
                    out.push(std::mem::take(&mut line));
                    used = cols(&tail);
                    line = tail;
                }
                _ => {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
            }
            // 回退到空格之后可能**还是**放不下(尾巴本身就快占满一行了),
            // 那就再断一次。少了这一下会折出一行比 width 还宽的东西,
            // 而那正是这个函数存在的理由。
            if used > 0 && used + w > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
        }
        line.push(c);
        used += w;
    }
    out.push(line);
    out
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
    fn truncate_counts_display_columns_not_characters() {
        assert_eq!(truncate("abcdef", 10), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // 中文一个字占两列:6 个字 = 12 列,放进 3 列只装得下一个字加省略号。
        assert_eq!(truncate("东京节点一号", 3), "东…");
        assert_eq!(truncate("东京节点一号", 12), "东京节点一号");
        assert_eq!(truncate("东京节点一号", 11), "东京节点一…");
        // 省略号自己占一列,所以 10 列只放得下 4 个字(8 列)—— 第 5 个字放不下。
        assert_eq!(truncate("东京节点一号", 10), "东京节点…");
        assert_eq!(truncate("2001:db8:1:aaaa::1", 12), "2001:db8:1:…");
        assert_eq!(truncate("abc", 0), "");
        // 切在中文中间不能切出半个字符。
        assert_eq!(cols(&truncate("节点一号", 5)), 5);
    }

    /// 手工排的列要用 `pad` 而不是 `{:<w}`:后者补的是字符数。
    /// 「计数器重置」5 个字符已经占 10 列,`{:<12}` 还会再补 7 个空格。
    #[test]
    fn pad_produces_exactly_the_requested_columns() {
        assert_eq!(cols(&pad("abc", 8)), 8);
        assert_eq!(cols(&pad("计数器重置", 12)), 12);
        assert_eq!(cols(&pad("从未连接的机器", 12)), 12, "超宽的要被截到正好 12 列");
        assert_eq!(pad("", 3), "   ");
        assert_eq!(cols(&pad("x", 0)), 0);
    }

    /// 折行的每一行都不能超宽 —— 超了 `Paragraph` 会再折一次,
    /// 而那一次的行数算不进弹窗高度,底下就被裁掉了。
    #[test]
    fn wrap_never_exceeds_the_width() {
        let texts = [
            "Tag:同一台机器内唯一;也是 (用户, tag) 记账口径的一半(§7.1),建好之后不能改。",
            "reality:密钥对与 short_id 建节点时自动生成;server_name 同时是握手目标",
            "(curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh 2>/dev/null || wget -qO- https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh) | SBX_SERVER='wss://203.0.113.8:18443/ws' sh",
            "短",
            "",
        ];
        for t in texts {
            for w in [1usize, 2, 10, 30, 60, 200] {
                for line in wrap(t, w) {
                    assert!(cols(&line) <= w.max(1) || cols(&line) <= 2, "宽度 {w} 折出了 {line:?}");
                }
            }
        }
    }

    /// 折完再拼回去必须还是原文(除了断行处那个被吃掉的空格)。
    /// 丢字的折行比不折行糟糕得多 —— 少一句提示看得出来,少两个字看不出来。
    #[test]
    fn wrap_preserves_the_text() {
        let t = "中转:地址留空 = 不启用;只填地址则端口沿用监听端口;订阅按中转地址导出。";
        let joined: String = wrap(t, 24).join("");
        assert_eq!(joined.replace(' ', ""), t.replace(' ', ""));
    }

    #[test]
    fn wrap_prefers_breaking_at_spaces() {
        let lines = wrap("alpha beta gamma delta", 12);
        assert!(lines.iter().all(|l| !l.starts_with(' ')), "{lines:?}");
        // 不该把单词劈开
        assert!(lines.iter().all(|l| l.split(' ').all(|w| "alpha beta gamma delta".contains(w))));
    }

    /// 一个宽字符比整行还窄时不能空转 —— 早先的写法会推无数个空行然后 OOM。
    #[test]
    fn wrap_survives_widths_narrower_than_one_char() {
        assert!(wrap("中文", 1).len() <= 4);
        assert_eq!(wrap("", 10), vec![String::new()]);
        assert_eq!(wrap("abc", 0), vec![String::new()]);
    }

    #[test]
    fn rate_rejects_nonsense_values() {        assert_eq!(rate(f64::NAN), "--");
        assert_eq!(rate(-1.0), "--");
        assert!(rate(1024.0).ends_with("/s"));
    }
}
