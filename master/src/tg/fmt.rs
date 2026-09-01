//! Telegram 消息里的纯格式化与解析(DESIGN.md §9.1)。
//!
//! 这个文件里**没有 IO**。放在一起是因为它们全是「输入 → 字符串」的纯函数,
//! 可以直接单测 —— 而 bot 的其余部分要么在等网络、要么在等数据库。

use anyhow::{anyhow, Result};
use chrono::{DateTime, FixedOffset, Timelike};
use std::collections::BTreeMap;

/// HTML 转义。
///
/// **所有用户输入嵌进 HTML 文案前都必须走这个。** Telegram 的 HTML 模式只识别
/// 少量标签(b/i/u/s/code/pre/a),其余按字面量处理,所以三个字符就够 ——
/// 但少了它,一个名字里带 `<` 的用户会让整条消息发送失败(Telegram 直接报
/// "can't parse entities"),而不是显示成乱码。
pub fn h(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// 20 格文本进度条:每格 5%,整格 `█`,半格(余量 ≥ 0.5)`▓`,空格 `░`。
///
/// 半格那一档是有用的:没有它,0% 和 4% 长得一模一样。
pub fn progress_bar(pct: f64) -> String {
    let pct = pct.clamp(0.0, 100.0);
    const CELLS: usize = 20;
    let exact = pct * CELLS as f64 / 100.0;
    let filled = exact.floor() as usize;
    let remainder = exact - filled as f64;

    let mut s = String::with_capacity(CELLS * 4 + 2);
    s.push('[');
    for i in 0..CELLS {
        if i < filled {
            s.push('█');
        } else if i == filled && remainder >= 0.5 {
            s.push('▓');
        } else {
            s.push('░');
        }
    }
    s.push(']');
    s
}

/// 告警档位。**只有 80 / 90 / 100 三档**,其余为 0(不告警)。
///
/// 去重就靠它:数据库里记着「已经通知到哪一档」,只在档位**上升**时推送。
/// 用连续百分比做去重会让 80.1% → 80.2% 也算一次变化,每 30 秒推一条。
pub fn quota_level(percent: f64) -> u8 {
    if percent >= 100.0 {
        100
    } else if percent >= 90.0 {
        90
    } else if percent >= 80.0 {
        80
    } else {
        0
    }
}

pub fn quota_alert_emoji(level: u8) -> &'static str {
    match level {
        100 => "🚨",
        90 => "⚠️",
        80 => "🔔",
        _ => "📊",
    }
}

pub fn status_emoji(enabled: bool) -> &'static str {
    if enabled {
        "✅"
    } else {
        "⛔"
    }
}

pub fn on_off(v: bool) -> &'static str {
    if v {
        "开启"
    } else {
        "关闭"
    }
}

pub fn billing_label(multiplier: f64) -> String {
    if (multiplier - 2.0).abs() < 0.01 {
        "双向".into()
    } else if (multiplier - 1.0).abs() < 0.01 {
        "单向".into()
    } else {
        format!("{multiplier:.1}x")
    }
}

pub fn quota_label(quota_bytes: i64) -> String {
    if quota_bytes <= 0 {
        "不限".into()
    } else {
        crate::model::user::User::format_bytes(quota_bytes)
    }
}

pub fn reset_label(reset_day: Option<i64>) -> String {
    match reset_day {
        Some(d) if (1..=31).contains(&d) => format!("每月 {d} 日"),
        _ => "不重置".into(),
    }
}

pub fn expire_label(expire_at: Option<i64>) -> String {
    let Some(ts) = expire_at else { return "永久".into() };
    match chrono::DateTime::from_timestamp(ts, 0) {
        Some(dt) => dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string(),
        None => "?".into(),
    }
}

// ─────────────────────────── 时间表 ───────────────────────────

/// 解析用户输入的时间表(`09:00, 21:30`)。
pub fn parse_schedule_input(text: &str) -> Result<Vec<String>> {
    let list: Vec<String> =
        text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    let out = normalize_schedule(&list);
    if out.is_empty() {
        return Err(anyhow!("未解析出有效时间"));
    }
    Ok(out)
}

/// 规范化时间表:丢掉非法项、补零、排序、去重。
///
/// 丢掉而不是报错,是因为这个函数也用在**配置文件**和**库里的旧值**上:
/// 一个手改坏的时间点不该让整份时间表失效。用户输入那条路径由
/// `parse_schedule_input` 负责在全都无效时报错。
pub fn normalize_schedule(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = list
        .iter()
        .filter_map(|item| parse_single_time(item).map(|(h, m)| format!("{h:02}:{m:02}")))
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn parse_single_time(text: &str) -> Option<(u32, u32)> {
    let (hh, mm) = text.trim().split_once(':')?;
    let hh: u32 = hh.trim().parse().ok()?;
    let mm: u32 = mm.trim().parse().ok()?;
    (hh < 24 && mm < 60).then_some((hh, mm))
}

/// 现在这一分钟里,哪些时间点该播报了。
///
/// `dates` 记的是「每个时间点最后一次播报的日期」。巡检每 30 秒跑一次,
/// 同一分钟会命中两次 —— 靠日期比对把第二次挡掉。
pub fn due_times(
    now: &DateTime<FixedOffset>,
    times: &[String],
    dates: &BTreeMap<String, String>,
) -> Vec<String> {
    let today = now.format("%Y-%m-%d").to_string();
    times
        .iter()
        .filter(|item| {
            let Some((hh, mm)) = parse_single_time(item) else { return false };
            now.hour() == hh && now.minute() == mm && dates.get(*item) != Some(&today)
        })
        .cloned()
        .collect()
}

/// 解析 `telegram.timezone`。支持:
///   * 空串 / `UTC` / `Z` → `+00:00`
///   * **不走夏令时的** IANA 别名 → 写死的固定偏移
///   * `+HH:MM` / `-HH:MM` / `+HHMM` / `+HH`
///
/// 不引 `chrono-tz` 是为了不把整个 tzdata 编进二进制。会 DST 的时区
/// (`Europe/London`、`America/*`、`Australia/Sydney` 等)**故意不在别名表里** ——
/// 给它们一个固定偏移会在夏令时期间整整偏一小时,而「播报晚了一小时」
/// 比「回落到默认时区」更难被发现。那些时区请填显式偏移。
pub fn parse_timezone(s: &str) -> Option<FixedOffset> {
    let s = s.trim();
    if s.is_empty()
        || s.eq_ignore_ascii_case("UTC")
        || s.eq_ignore_ascii_case("GMT")
        || s.eq_ignore_ascii_case("Z")
    {
        return FixedOffset::east_opt(0);
    }
    // 允许 `UTC-07:00` / `GMT+8` 这种带前缀的写法。**必须接受**,因为
    // `format_offset` 现在就是这么显示的 —— 不认自己的输出会让「打开表单、
    // 什么都不动、按确定」直接报错,那是最气人的一类 bug。
    let s = ["UTC", "GMT"]
        .iter()
        .find_map(|p| s.get(..p.len()).filter(|h| h.eq_ignore_ascii_case(p)).map(|_| &s[p.len()..]))
        .map(str::trim)
        .filter(|rest| !rest.is_empty())
        .unwrap_or(s);
    let aliased = match s {
        "Asia/Shanghai" | "Asia/Hong_Kong" | "Asia/Taipei" | "Asia/Singapore" | "Asia/Macau"
        | "Asia/Kuala_Lumpur" | "Asia/Manila" => Some(8 * 3600),
        "Asia/Tokyo" | "Asia/Seoul" => Some(9 * 3600),
        "Asia/Bangkok" | "Asia/Ho_Chi_Minh" | "Asia/Jakarta" => Some(7 * 3600),
        "Asia/Kolkata" | "Asia/Calcutta" => Some(5 * 3600 + 30 * 60),
        "Asia/Dubai" => Some(4 * 3600),
        "Australia/Brisbane" | "Australia/Perth" => Some(10 * 3600),
        _ => None,
    };
    if let Some(secs) = aliased {
        return FixedOffset::east_opt(secs);
    }

    let mut chars = s.chars();
    let sign = match chars.next()? {
        '+' => 1i32,
        '-' => -1i32,
        _ => return None,
    };
    let rest = &s[1..];
    let (hh, mm) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else if rest.len() == 4 {
        (&rest[..2], &rest[2..])
    } else if rest.len() == 1 || rest.len() == 2 {
        (rest, "0")
    } else {
        return None;
    };
    let h: i32 = hh.parse().ok()?;
    let m: i32 = mm.parse().ok()?;
    if !(0..=14).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

/// `parse_timezone` 的反向:把偏移秒数写成 `UTC±HH:MM`(零点写成 `UTC`)。
///
/// 和它成对放在这里,而不是丢到调用方(TUI 表单要用它做预填、网卡明细要用它做展示):
/// 分开写的话很容易出现「能解析但显示成另一种格式」,于是编辑一次表单值就变了样。
///
/// **带 `UTC` 前缀**是因为裸 `-07:00` 读起来像个数字而不是时区。但**只加 UTC**,
/// 不做 `PDT`/`CST` 这类字母缩写:`CST` 同时是中国标准时间(+8)和美国中部时间(−6),
/// `PDT`/`PST` 还随夏令时变 —— 而这里存的就是一个固定偏移(主控不带 tzdata)。
/// 一个偏移也对应很多时区(−07:00 夏天是 PDT,亚利桑那全年是 MST),挑哪个都是猜。
///
/// 分钟部分照样写出来(`UTC+08:00` 而不是 `UTC+8`),因为印度那类 `+05:30` 存在,
/// 省掉分钟会让它变成一个静默错误的输入。
pub fn format_offset(secs: i32) -> String {
    if secs == 0 {
        return "UTC".into();
    }
    let sign = if secs < 0 { '-' } else { '+' };
    let abs = secs.abs();
    format!("UTC{}{:02}:{:02}", sign, abs / 3600, (abs % 3600) / 60)
}

/// 按字符切分长消息。Telegram 单条消息上限 4096 **字符**(不是字节)。
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars || max_chars == 0 {
        return vec![text.to_string()];
    }
    chars.chunks(max_chars).map(|c| c.iter().collect::<String>()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_escape_covers_the_three_that_matter() {
        assert_eq!(h("a<b>c&d"), "a&lt;b&gt;c&amp;d");
        // 引号在 Telegram 的 HTML 模式里没有特殊含义,不必转 —— 转了反而显示成实体。
        assert_eq!(h(r#"a"b'c"#), r#"a"b'c"#);
    }

    #[test]
    fn progress_bar_boundaries() {
        assert!(progress_bar(0.0).starts_with("[░"));
        assert_eq!(progress_bar(100.0), format!("[{}]", "█".repeat(20)));
        // 越界不 panic,也不画出格。
        assert_eq!(progress_bar(150.0), progress_bar(100.0));
        assert_eq!(progress_bar(-5.0), progress_bar(0.0));
        assert_eq!(progress_bar(50.0).chars().count(), 22, "20 格 + 两个方括号");
    }

    /// 半格那一档存在的意义:让 0% 和 4% 看起来不一样。
    #[test]
    fn progress_bar_shows_a_half_cell() {
        assert!(progress_bar(3.0).contains('▓'), "3% 应当有半格:{}", progress_bar(3.0));
        assert!(!progress_bar(1.0).contains('▓'), "1% 不到半格");
    }

    /// 档位只有三级。用连续百分比去重会让 80.1% → 80.2% 也推一条。
    #[test]
    fn quota_level_has_exactly_three_steps() {
        assert_eq!(quota_level(0.0), 0);
        assert_eq!(quota_level(79.9), 0);
        assert_eq!(quota_level(80.0), 80);
        assert_eq!(quota_level(89.9), 80);
        assert_eq!(quota_level(90.0), 90);
        assert_eq!(quota_level(99.9), 90);
        assert_eq!(quota_level(100.0), 100);
        assert_eq!(quota_level(500.0), 100);
    }

    #[test]
    fn schedule_input_is_sorted_and_deduped() {
        assert_eq!(parse_schedule_input("21:30, 09:00,21:30").unwrap(), vec!["09:00", "21:30"]);
        assert_eq!(parse_schedule_input("9:5").unwrap(), vec!["09:05"], "应当补零");
        assert!(parse_schedule_input("").is_err());
        assert!(parse_schedule_input("25:00").is_err(), "全无效时要报错");
    }

    /// 配置/库里的时间表混着坏值时,丢掉坏的而不是整份作废。
    #[test]
    fn normalize_drops_invalid_entries_instead_of_failing() {
        let list = ["09:00".to_string(), "25:00".into(), "21:30".into(), "abc".into()];
        assert_eq!(normalize_schedule(&list), vec!["09:00", "21:30"]);
        assert!(normalize_schedule(&[]).is_empty());
    }

    fn at(hh: u32, mm: u32) -> DateTime<FixedOffset> {
        let tz = FixedOffset::east_opt(8 * 3600).unwrap();
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1)
            .unwrap()
            .and_hms_opt(hh, mm, 0)
            .unwrap()
            .and_local_timezone(tz)
            .unwrap()
    }

    #[test]
    fn due_times_fires_only_in_the_matching_minute() {
        let times = vec!["09:00".to_string(), "21:30".into()];
        let empty = BTreeMap::new();
        assert_eq!(due_times(&at(9, 0), &times, &empty), vec!["09:00"]);
        assert!(due_times(&at(9, 1), &times, &empty).is_empty());
        assert!(due_times(&at(8, 59), &times, &empty).is_empty());
    }

    /// 巡检每 30 秒一次,同一分钟会命中两次 —— 第二次必须被日期挡掉,
    /// 否则每个时间点都会连发两条。
    #[test]
    fn due_times_does_not_fire_twice_in_the_same_minute() {
        let times = vec!["09:00".to_string()];
        let mut dates = BTreeMap::new();
        dates.insert("09:00".to_string(), "2026-08-01".to_string());
        assert!(due_times(&at(9, 0), &times, &dates).is_empty());

        // 但换一天要重新播。
        dates.insert("09:00".to_string(), "2026-07-31".to_string());
        assert_eq!(due_times(&at(9, 0), &times, &dates), vec!["09:00"]);
    }

    #[test]
    fn timezone_accepts_offsets_and_dst_free_aliases() {
        let e8 = FixedOffset::east_opt(8 * 3600).unwrap();
        assert_eq!(parse_timezone("Asia/Shanghai"), Some(e8));
        assert_eq!(parse_timezone("+08:00"), Some(e8));
        assert_eq!(parse_timezone("+0800"), Some(e8));
        assert_eq!(parse_timezone("+8"), Some(e8));
        assert_eq!(parse_timezone("Asia/Kolkata"), FixedOffset::east_opt(5 * 3600 + 1800));
        assert_eq!(parse_timezone("-05:00"), FixedOffset::east_opt(-5 * 3600));
        assert_eq!(parse_timezone(""), FixedOffset::east_opt(0));
        assert_eq!(parse_timezone("UTC"), FixedOffset::east_opt(0));
    }

    /// 会夏令时的时区**故意不认**:给固定偏移会在夏令时期间整整偏一小时,
    /// 而那种错误比「回落到默认」难发现得多。
    #[test]
    fn timezone_rejects_dst_zones_so_they_fall_back_loudly() {
        for z in ["Europe/London", "America/New_York", "Australia/Sydney", "Europe/Paris"] {
            assert_eq!(parse_timezone(z), None, "{z} 不该被当成固定偏移");
        }
    }

    #[test]
    fn timezone_rejects_garbage() {
        for z in ["nonsense", "+25:00", "+08:99", "08:00", "++08"] {
            assert_eq!(parse_timezone(z), None, "{z}");
        }
    }

    /// `format_offset` 必须是 `parse_timezone` 的逆:表单预填走前者、提交走后者,
    /// 两者对不上就会出现「打开编辑框、什么都不动、一按确定值就变了」。
    #[test]
    fn format_offset_round_trips_through_parse_timezone() {
        for secs in [0, 8 * 3600, -7 * 3600, 5 * 3600 + 30 * 60, -3 * 3600 - 30 * 60, 53_940] {
            let text = format_offset(secs);
            assert_eq!(
                parse_timezone(&text).map(|o| o.local_minus_utc()),
                Some(secs),
                "{secs} → {text} 又解不回来"
            );
        }
        assert_eq!(format_offset(0), "UTC", "零点写成 UTC,不是 +00:00");
        assert_eq!(format_offset(-25200), "UTC-07:00");
        // 分钟不能省:印度那类 +05:30 存在,省掉会变成一个静默错误的值。
        assert_eq!(format_offset(19800), "UTC+05:30");
        // 带前缀的写法必须能解回来 —— 这正是 format_offset 的输出格式,
        // 不认自己的输出会让「打开表单、什么都不动、按确定」直接报错。
        for (text, want) in [
            ("UTC-07:00", -25200),
            ("utc+8", 28800),
            ("GMT+05:30", 19800),
            ("UTC-0700", -25200),
            ("UTC +8", 28800),
        ] {
            assert_eq!(
                parse_timezone(text).map(|o| o.local_minus_utc()),
                Some(want),
                "{text} 该解成 {want}"
            );
        }
        // 但纯前缀仍然是 UTC 本身,不是「前缀后面空了」那种错误。
        assert_eq!(parse_timezone("UTC").map(|o| o.local_minus_utc()), Some(0));
        assert_eq!(parse_timezone("GMT").map(|o| o.local_minus_utc()), Some(0));
    }

    #[test]
    fn split_message_respects_char_boundaries() {
        assert_eq!(split_message("abc", 10), vec!["abc"]);
        assert_eq!(split_message("abcdef", 2), vec!["ab", "cd", "ef"]);
        // 中文按字符切,不能切出半个字符。
        let parts = split_message("东京大阪札幌", 2);
        assert_eq!(parts, vec!["东京", "大阪", "札幌"]);
    }

    #[test]
    fn labels_read_naturally() {
        assert_eq!(quota_label(0), "不限");
        assert!(quota_label(1_073_741_824).contains("GB"));
        assert_eq!(billing_label(1.0), "单向");
        assert_eq!(billing_label(2.0), "双向");
        assert_eq!(billing_label(1.5), "1.5x");
        assert_eq!(reset_label(Some(22)), "每月 22 日");
        assert_eq!(reset_label(None), "不重置");
        assert_eq!(reset_label(Some(99)), "不重置", "越界值不该显示成「每月 99 日」");
        assert_eq!(expire_label(None), "永久");
    }
}
