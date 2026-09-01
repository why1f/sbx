//! Telegram 的消息文案与 inline keyboard(DESIGN.md §9.1)。
//!
//! 这里也是纯函数:输入数据、输出字符串或 JSON。渲染与 IO 分开,
//! 是为了让「文案里有没有漏转义」这类问题可以直接单测。
//!
//! **callback data 的格式是 `<域>:<动作>[:<参数>]`**,Telegram 限制它 ≤ 64 字节。
//! 域只有两个:`u`(用户自己)和 `a`(管理员)。用户名可能很长,所以凡是要带
//! 用户名的按钮都走 `a:user:<name>` 这种形式,并在生成时截断。

use serde_json::{json, Value};

use super::fmt::{self, h};
use super::repo::{AdminPrefs, TgUser};

/// Telegram 的 `callback_data` 上限是 64 **字节**。超了 API 直接报错,
/// 表现成「这个按钮点了没反应」—— 而按钮是静态构造的,这种错只会在
/// 某个人的用户名恰好够长时才出现。所以在构造处直接断言。
const CALLBACK_MAX: usize = 64;

fn row(buttons: Vec<(&str, String)>) -> Vec<Value> {
    buttons
        .into_iter()
        .map(|(text, data)| {
            debug_assert!(
                data.len() <= CALLBACK_MAX,
                "callback_data 超过 {CALLBACK_MAX} 字节,Telegram 会拒绝这个按钮: {data}"
            );
            json!({ "text": text, "callback_data": data })
        })
        .collect()
}

fn keyboard(rows: Vec<Vec<Value>>) -> Value {
    json!({ "inline_keyboard": rows })
}

// ─────────────────────────── 用户侧 ───────────────────────────

pub fn user_home_keyboard() -> Value {
    keyboard(vec![
        row(vec![("📊 我的流量", "u:usage".into()), ("🔗 订阅", "u:sub".into())]),
        row(vec![("⚙️ 通知设置", "u:settings".into())]),
    ])
}

pub fn user_back_keyboard() -> Value {
    keyboard(vec![row(vec![("⬅️ 返回", "u:home".into())])])
}

pub fn user_sub_keyboard() -> Value {
    keyboard(vec![
        row(vec![("📋 全部链接", "u:sub_links".into()), ("🧩 base64", "u:sub_b64".into())]),
        row(vec![("⬅️ 返回", "u:home".into())]),
    ])
}

pub fn user_settings_keyboard(u: &TgUser) -> Value {
    keyboard(vec![
        row(vec![
            (if u.notify_80 { "🔔 80% 开" } else { "🔕 80% 关" }, "u:t80".into()),
            (if u.notify_90 { "🔔 90% 开" } else { "🔕 90% 关" }, "u:t90".into()),
            (if u.notify_100 { "🔔 100% 开" } else { "🔕 100% 关" }, "u:t100".into()),
        ]),
        row(vec![(
            if u.schedule_enabled { "⏰ 定时播报 开" } else { "⏰ 定时播报 关" },
            "u:sched".into(),
        )]),
        row(vec![("🕘 修改播报时间", "u:sched_time".into())]),
        row(vec![("⬅️ 返回", "u:home".into())]),
    ])
}

/// 未绑定时的引导。**不给任何按钮** —— 没绑定之前没有任何可操作的东西。
pub fn unbound_text() -> String {
    "👋 <b>还没有绑定账号</b>\n\n\
     请向管理员索取绑定码,然后发送:\n\
     <code>/bind 你的绑定码</code>"
        .to_string()
}

pub fn user_home_text(u: &TgUser) -> String {
    let pct = u.percent();
    format!(
        "📊 <b>我的账号</b>\n\n\
         账号:  <b>{name}</b>\n\
         状态:  {emoji} {state}\n\
         套餐:  <b>{quota}</b>({billing})\n\
         已用:  <b>{used}</b> / {quota}\n\
         剩余:  <b>{remain}</b>\n\
         进度:  <code>{bar} {pct:.1}%</code>\n\
         重置:  {reset}\n\
         到期:  {expire}",
        name = h(&u.name),
        emoji = fmt::status_emoji(u.enabled),
        state = if u.enabled { "启用" } else { "停用" },
        quota = h(&fmt::quota_label(u.quota_bytes)),
        billing = h(&fmt::billing_label(u.traffic_multiplier)),
        used = h(&crate::model::user::User::format_bytes(u.used())),
        remain = h(&remaining_label(u)),
        bar = fmt::progress_bar(pct),
        pct = pct,
        reset = h(&fmt::reset_label(u.reset_day)),
        expire = h(&fmt::expire_label(u.expire_at)),
    )
}

pub fn remaining_label(u: &TgUser) -> String {
    match u.remaining() {
        Some(v) => crate::model::user::User::format_bytes(v),
        None => "不限".into(),
    }
}

pub fn user_settings_text(u: &TgUser, default_times: &[String]) -> String {
    let times = u.schedule_times();
    let (shown, source) = if times.is_empty() {
        (default_times.join(", "), "(默认)")
    } else {
        (times.join(", "), "")
    };
    format!(
        "⚙️ <b>通知设置</b>\n\n\
         80% 提醒:  {n80}\n\
         90% 提醒:  {n90}\n\
         100% 提醒: {n100}\n\
         定时播报:  {sched}\n\
         播报时间:  <code>{times}</code> {source}",
        n80 = fmt::on_off(u.notify_80),
        n90 = fmt::on_off(u.notify_90),
        n100 = fmt::on_off(u.notify_100),
        sched = fmt::on_off(u.schedule_enabled),
        times = h(&shown),
        source = source,
    )
}

pub fn quota_alert_text(u: &TgUser, level: u8) -> String {
    let pct = u.percent();
    format!(
        "{emoji} <b>流量提醒</b>\n\n\
         账号:  <b>{name}</b>\n\
         已用:  <b>{used}</b> / {quota}({pct:.1}%)\n\
         剩余:  <b>{remain}</b>\n\
         进度:  <code>{bar}</code>\n\
         重置:  {reset}",
        emoji = fmt::quota_alert_emoji(level),
        name = h(&u.name),
        used = h(&crate::model::user::User::format_bytes(u.used())),
        quota = h(&fmt::quota_label(u.quota_bytes)),
        pct = pct,
        remain = h(&remaining_label(u)),
        bar = fmt::progress_bar(pct),
        reset = h(&fmt::reset_label(u.reset_day)),
    )
}

pub fn scheduled_user_text(now: &str, u: &TgUser) -> String {
    format!("⏰ <b>定时流量播报</b>\n时间: {}\n\n{}", h(now), user_home_text(u))
}

// ─────────────────────────── 管理员侧 ───────────────────────────

pub fn admin_home_keyboard() -> Value {
    keyboard(vec![
        row(vec![("📈 全部用户流量", "a:usages".into())]),
        row(vec![("⚙️ 管理员通知设置", "a:settings".into())]),
    ])
}

pub fn admin_back_keyboard() -> Value {
    keyboard(vec![row(vec![("⬅️ 返回", "a:home".into())])])
}

pub fn admin_settings_keyboard(p: &AdminPrefs) -> Value {
    keyboard(vec![
        row(vec![(
            if p.notify_quota { "🔔 配额告警 开" } else { "🔕 配额告警 关" },
            "a:quota".into(),
        )]),
        row(vec![(
            if p.schedule_enabled { "⏰ 定时汇总 开" } else { "⏰ 定时汇总 关" },
            "a:sched".into(),
        )]),
        row(vec![("🕘 修改汇总时间", "a:sched_time".into())]),
        row(vec![("⬅️ 返回", "a:home".into())]),
    ])
}

pub fn admin_settings_text(p: &AdminPrefs, default_times: &[String]) -> String {
    let times = p.schedule_times();
    let (shown, source) = if times.is_empty() {
        (default_times.join(", "), "(默认)")
    } else {
        (times.join(", "), "")
    };
    format!(
        "⚙️ <b>管理员通知设置</b>\n\n\
         配额告警:  {q}\n\
         定时汇总:  {s}\n\
         汇总时间:  <code>{times}</code> {source}",
        q = fmt::on_off(p.notify_quota),
        s = fmt::on_off(p.schedule_enabled),
        times = h(&shown),
        source = source,
    )
}

pub fn admin_quota_alert_text(u: &TgUser, level: u8) -> String {
    format!(
        "{emoji} <b>用户流量提醒</b>\n\n\
         <b>{name}</b> 已达 <b>{level}%</b>\n\
         已用:  {used} / {quota}\n\
         剩余:  {remain}",
        emoji = fmt::quota_alert_emoji(level),
        name = h(&u.name),
        level = level,
        used = h(&crate::model::user::User::format_bytes(u.used())),
        quota = h(&fmt::quota_label(u.quota_bytes)),
        remain = h(&remaining_label(u)),
    )
}

/// 全部用户的一览。
///
/// 按「用得最多的排前面」而不是按名字:这一屏的用途是找出该关注谁。
/// 条数封顶,免得用户多了之后单条消息超长 —— 超了 Telegram 直接拒收整条。
pub fn all_usages_text(users: &[TgUser], limit: usize) -> String {
    if users.is_empty() {
        return "暂无用户。".into();
    }
    let mut sorted: Vec<&TgUser> = users.iter().collect();
    sorted.sort_by(|a, b| {
        b.percent()
            .partial_cmp(&a.percent())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.used().cmp(&a.used()))
    });

    let shown = sorted.len().min(limit);
    let mut out = format!("📈 <b>全部用户流量</b>({} 个)\n", users.len());
    for u in sorted.iter().take(shown) {
        let state = if u.enabled { "" } else { " ⛔" };
        let quota = fmt::quota_label(u.quota_bytes);
        let line = if u.quota_bytes > 0 {
            format!(
                "\n<b>{}</b>{}\n  {} / {}({:.0}%)",
                h(&u.name),
                state,
                h(&crate::model::user::User::format_bytes(u.used())),
                h(&quota),
                u.percent()
            )
        } else {
            format!(
                "\n<b>{}</b>{}\n  {} · 不限",
                h(&u.name),
                state,
                h(&crate::model::user::User::format_bytes(u.used()))
            )
        };
        out.push_str(&line);
    }
    if sorted.len() > shown {
        out.push_str(&format!("\n\n…另有 {} 个用户未显示", sorted.len() - shown));
    }
    out
}

pub fn scheduled_admin_text(now: &str, users: &[TgUser], limit: usize) -> String {
    format!("⏰ <b>定时汇总</b>  {}\n\n{}", h(now), all_usages_text(users, limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str, quota_gb: i64, used_gb: i64) -> TgUser {
        TgUser {
            id: 1,
            name: name.into(),
            enabled: true,
            quota_bytes: quota_gb * 1_073_741_824,
            traffic_multiplier: 1.0,
            expire_at: None,
            reset_day: Some(22),
            sub_token: "tok".into(),
            cycle_up: used_gb * 1_073_741_824,
            cycle_down: 0,
            chat_id: 1,
            notify_80: true,
            notify_90: true,
            notify_100: true,
            schedule_enabled: true,
            schedule_times_json: "[]".into(),
            last_quota_level: 0,
            last_schedule_dates_json: "{}".into(),
        }
    }

    /// 用户名进 HTML 文案前必须转义,否则一个带 `<` 的名字会让整条消息发不出去
    /// (Telegram 报 can't parse entities)。
    #[test]
    fn user_names_are_escaped_in_every_message_body() {
        let u = user("<b>evil</b>&co", 100, 50);
        // 设置页不含用户名,所以不在这个列表里。
        for body in [
            user_home_text(&u),
            quota_alert_text(&u, 80),
            admin_quota_alert_text(&u, 80),
            all_usages_text(std::slice::from_ref(&u), 10),
        ] {
            assert!(body.contains("&lt;b&gt;evil&lt;/b&gt;&amp;co"), "没转义: {body}");
            // 名字里的标签不该变成真的标签。
            assert!(!body.contains("<b>evil</b>"), "逃逸了: {body}");
        }
    }

    /// 用户自己设的播报时间也是用户输入,同样要转义。
    #[test]
    fn schedule_times_are_escaped_too() {
        let mut u = user("alice", 100, 0);
        u.schedule_times_json = r#"["<img src=x>"]"#.into();
        let body = user_settings_text(&u, &[]);
        assert!(!body.contains("<img"), "时间表里的输入逃逸了: {body}");
        assert!(body.contains("&lt;img"), "{body}");
    }

    #[test]
    fn unlimited_users_show_no_percentage() {
        let u = user("alice", 0, 30);
        let body = user_home_text(&u);
        assert!(body.contains("不限"));
        let list = all_usages_text(&[u], 10);
        assert!(list.contains("· 不限"), "{list}");
        assert!(!list.contains('%'), "不限流量的行不该有百分比:{list}");
    }

    /// 一览按用量比例降序 —— 这一屏是用来找「该关注谁」的。
    #[test]
    fn all_usages_puts_the_heaviest_first() {
        let users = vec![user("light", 100, 10), user("heavy", 100, 95), user("mid", 100, 50)];
        let out = all_usages_text(&users, 10);
        let pos = |n: &str| out.find(n).unwrap();
        assert!(pos("heavy") < pos("mid"), "{out}");
        assert!(pos("mid") < pos("light"), "{out}");
    }

    /// 用户多了之后要截断,否则单条消息超 4096 字符会被 Telegram 整条拒收。
    #[test]
    fn all_usages_truncates_and_says_so() {
        let users: Vec<TgUser> = (0..50).map(|i| user(&format!("u{i}"), 100, i)).collect();
        let out = all_usages_text(&users, 10);
        assert!(out.contains("另有 40 个用户未显示"), "{out}");
        assert!(out.contains("(50 个)"), "总数要如实报:{out}");
    }

    #[test]
    fn empty_user_list_says_so() {
        assert_eq!(all_usages_text(&[], 10), "暂无用户。");
    }

    /// 停用的用户要在一览里能一眼看出来。
    #[test]
    fn disabled_users_are_marked() {
        let mut u = user("alice", 100, 10);
        u.enabled = false;
        assert!(all_usages_text(&[u], 10).contains('⛔'));
    }

    /// 设置页要说清「现在这个时间表是自己设的还是默认的」——
    /// 不说的话,用户会以为自己设过了。
    #[test]
    fn settings_marks_default_schedule_times() {
        let u = user("alice", 100, 0);
        let body = user_settings_text(&u, &["09:00".into(), "21:30".into()]);
        assert!(body.contains("09:00, 21:30"));
        assert!(body.contains("(默认)"), "{body}");

        let mut u2 = user("alice", 100, 0);
        u2.schedule_times_json = r#"["07:00"]"#.into();
        let body = user_settings_text(&u2, &["09:00".into()]);
        assert!(body.contains("07:00"));
        assert!(!body.contains("(默认)"), "{body}");
    }

    #[test]
    fn keyboards_reflect_current_toggle_state() {
        let mut u = user("alice", 100, 0);
        let on = user_settings_keyboard(&u).to_string();
        assert!(on.contains("80% 开"));
        u.notify_80 = false;
        u.schedule_enabled = false;
        let off = user_settings_keyboard(&u).to_string();
        assert!(off.contains("80% 关"));
        assert!(off.contains("定时播报 关"));
    }

    /// 每个键盘里的 callback_data 都要在长度限制内,而且非空。
    #[test]
    fn every_button_has_valid_callback_data() {
        let u = user("alice", 100, 0);
        let prefs = AdminPrefs {
            chat_id: 1,
            notify_quota: true,
            schedule_enabled: true,
            schedule_times_json: "[]".into(),
            last_schedule_dates_json: "{}".into(),
        };
        let kbs = [
            user_home_keyboard(),
            user_back_keyboard(),
            user_sub_keyboard(),
            user_settings_keyboard(&u),
            admin_home_keyboard(),
            admin_back_keyboard(),
            admin_settings_keyboard(&prefs),
        ];
        for kb in kbs {
            for row in kb["inline_keyboard"].as_array().unwrap() {
                for btn in row.as_array().unwrap() {
                    let data = btn["callback_data"].as_str().expect("必须有 callback_data");
                    assert!(!data.is_empty());
                    assert!(data.len() <= CALLBACK_MAX, "{data} 超长");
                    assert!(!btn["text"].as_str().unwrap().is_empty());
                }
            }
        }
    }
}
