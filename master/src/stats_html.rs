//! 订阅统计页(DESIGN.md §10)。移植自旧项目 `service/stats_html.rs`。
//!
//! 浏览器访问 `/sub/<token>` 时默认返回它:深色主题、用量进度条、
//! 每个节点一段可复制的链接 + 内联 SVG 二维码。
//!
//! **它仍然只读。** 这一页不提供任何管理能力 —— 订阅服务是 §2「不做 Web 面板」
//! 的唯一例外,而例外的边界就是「只吐内容」。
//!
//! ## 这里的转义为什么写得这么小心
//!
//! 页面上的每一段动态文本都来自数据库里管理员可编辑的字段(用户名、节点 tag、
//! SNI),而它们会落到三种不同的上下文里:HTML 文本、HTML 属性、以及
//! `onclick="copy(this,'…')"` 里的 **JS 字符串**。最后一种最危险:
//! 属性用双引号包着,所以 JS 字符串里的双引号必须一起转掉,否则一个含 `"`
//! 的 tag 就能闭合 onclick 属性、注入任意事件处理器。

use crate::sub::ShareLink;
use qrcode::{render::svg, QrCode};

/// 渲染这一页需要的用户信息。
pub struct StatsView {
    pub name: String,
    pub enabled: bool,
    pub auto_disabled: bool,
    pub quota_bytes: i64,
    pub cycle_up: i64,
    pub cycle_down: i64,
    pub traffic_multiplier: f64,
    pub expire_at: Option<i64>,
    pub reset_day: Option<i64>,
    pub sub_token: String,
}

impl StatsView {
    /// 计费口径的已用量(含倍率)。与 §6.3 的配额判定、TUI 的用量列同一个口径 ——
    /// 三处显示不同的数字会让用户来问「到底哪个准」。
    fn used(&self) -> i64 {
        let raw = self.cycle_up.saturating_add(self.cycle_down);
        (raw.max(0) as f64 * self.traffic_multiplier.max(0.0)) as i64
    }

    fn percent(&self) -> f64 {
        if self.quota_bytes <= 0 {
            return 0.0;
        }
        (self.used() as f64 / self.quota_bytes as f64 * 100.0).clamp(0.0, 100.0)
    }
}

/// 渲染整页。`base_url` 形如 `https://sub.example.com`(不带尾斜杠)。
pub fn render(v: &StatsView, links: &[ShareLink], base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let pct = v.percent();
    let expired = crate::model::user::expired(v.expire_at, chrono::Local::now().timestamp());
    let over = crate::model::user::over_quota(v.quota_bytes, v.used());

    // 状态标签的判定顺序 = 优先级顺序:先说「为什么现在用不了」,再说「快用完了」。
    let (bar_cls, status_cls, status_label) = if !v.enabled {
        ("bad", "bad", if v.auto_disabled { "已自动停用" } else { "已停用" })
    } else if expired {
        ("bad", "bad", "已到期")
    } else if over {
        ("bad", "bad", "已超额")
    } else if pct >= 95.0 {
        ("bad", "bad", "即将耗尽")
    } else if pct >= 80.0 {
        ("warn", "warn", "偏高")
    } else {
        ("", "", "正常")
    };

    let total_str = if v.quota_bytes <= 0 { "不限".to_string() } else { fmt_bytes(v.quota_bytes) };
    let reset_desc = match v.reset_day {
        Some(d) if (1..=31).contains(&d) => format!("每月 {d} 号"),
        _ => "不重置".into(),
    };
    let billing = if (v.traffic_multiplier - 1.0).abs() < 0.01 {
        "单向".to_string()
    } else if (v.traffic_multiplier - 2.0).abs() < 0.01 {
        "双向".to_string()
    } else {
        format!("{:.1}x", v.traffic_multiplier)
    };

    let sub_sing = format!("{base}/sub/{}", v.sub_token);
    let sub_clash = format!("{base}/sub/{}?type=clash", v.sub_token);
    let sub_rows = format!(
        "{}{}",
        copy_row("sing-box / v2rayN", &sub_sing),
        copy_row("mihomo / Clash Meta", &sub_clash),
    );

    let node_rows = if links.is_empty() {
        r#"<div class="empty">暂无可用节点。请联系管理员分配。</div>"#.to_string()
    } else {
        links.iter().map(node_block).collect::<Vec<_>>().join("\n")
    };

    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow">
<title>{name_h} · 订阅</title>
<style>
:root {{
  --bg:#0d1117; --card:#161b22; --border:#30363d;
  --text:#e6edf3; --muted:#8b949e; --accent:#58a6ff;
  --ok:#3fb950; --warn:#d29922; --bad:#f85149;
}}
* {{ box-sizing:border-box; }}
html,body {{ margin:0; padding:0; }}
body {{
  background:var(--bg); color:var(--text);
  font-family:-apple-system,BlinkMacSystemFont,"Segoe UI","Noto Sans SC",sans-serif;
  min-height:100vh; padding:24px 16px;
}}
.wrap {{ max-width:760px; margin:0 auto; }}
.card {{ background:var(--card); border:1px solid var(--border); border-radius:12px; padding:20px; margin-bottom:16px; }}
h1,h2 {{ margin-top:0; font-weight:600; }}
h1 {{ font-size:22px; display:flex; align-items:center; gap:12px; flex-wrap:wrap; }}
h2 {{ font-size:12px; color:var(--muted); text-transform:uppercase; letter-spacing:.8px; margin-bottom:14px; }}
.status {{ font-size:12px; padding:3px 10px; border-radius:999px; background:#1f2937; color:var(--ok); font-weight:500; }}
.status.warn {{ color:var(--warn); background:#2b2310; }}
.status.bad {{ color:var(--bad); background:#2b1010; }}
.meta {{ display:grid; grid-template-columns:1fr 1fr; gap:6px 16px; color:var(--muted); font-size:13px; margin-top:10px; }}
.meta b {{ color:var(--text); font-weight:500; }}
.bar {{ height:10px; background:#21262d; border-radius:6px; overflow:hidden; margin:14px 0 8px; }}
.bar>span {{ display:block; height:100%; background:var(--ok); }}
.bar>span.warn {{ background:var(--warn); }}
.bar>span.bad {{ background:var(--bad); }}
.usage {{ font-size:14px; color:var(--muted); display:flex; justify-content:space-between; }}
.usage b {{ color:var(--text); font-weight:600; }}
.row {{ display:flex; align-items:center; gap:8px; margin-bottom:10px; flex-wrap:wrap; }}
.row:last-child {{ margin-bottom:0; }}
.row .name {{ min-width:150px; color:var(--muted); font-size:13px; }}
.row code {{
  flex:1; min-width:0; background:#010409; color:var(--accent);
  padding:8px 10px; border-radius:6px; font-size:12px;
  overflow:hidden; text-overflow:ellipsis; white-space:nowrap;
  font-family:ui-monospace,"SF Mono","Consolas",monospace; border:1px solid var(--border);
}}
button {{
  background:#21262d; color:var(--text); border:1px solid var(--border);
  border-radius:6px; padding:7px 14px; font-size:12px; cursor:pointer;
}}
button:hover {{ background:#30363d; }}
button.done {{ background:#1f6feb; border-color:#388bfd; color:#fff; }}
.node {{ padding:10px 0; border-top:1px solid var(--border); }}
.node:first-child {{ border-top:0; padding-top:0; }}
.empty {{ color:var(--muted); font-size:13px; }}
details summary {{ cursor:pointer; color:var(--muted); font-size:12px; margin-top:4px; user-select:none; }}
details[open] summary {{ color:var(--accent); }}
details svg {{ display:block; margin:10px auto 4px; background:#fff; padding:10px; border-radius:8px; max-width:220px; width:100%; height:auto; }}
.foot {{ text-align:center; color:var(--muted); font-size:11px; margin-top:24px; padding-bottom:12px; }}
@media (max-width:520px) {{
  .meta {{ grid-template-columns:1fr; }}
  .row .name {{ min-width:0; flex-basis:100%; }}
}}
</style>
</head>
<body>
<div class="wrap">
  <div class="card">
    <h1>{name_h} <span class="status {status_cls}">{status_label}</span></h1>
    <div class="bar"><span class="{bar_cls}" style="width:{pct:.1}%"></span></div>
    <div class="usage"><span>已用 <b>{used}</b></span><span><b>{total}</b></span></div>
    <div class="meta">
      <span>重置: <b>{reset}</b></span>
      <span>到期: <b>{expire}</b></span>
      <span>上行: <b>{up}</b></span>
      <span>下行: <b>{down}</b></span>
      <span>计费: <b>{billing}</b></span>
      <span>节点: <b>{n_nodes}</b></span>
    </div>
  </div>
  <div class="card">
    <h2>订阅导入</h2>
{sub_rows}
  </div>
  <div class="card">
    <h2>单节点 ({n_nodes})</h2>
{node_rows}
  </div>
  <div class="foot">由 sbx 生成</div>
</div>
<script>
function copy(btn,text){{
  navigator.clipboard.writeText(text).then(function(){{
    var old=btn.textContent;
    btn.textContent='已复制';
    btn.classList.add('done');
    setTimeout(function(){{ btn.textContent=old; btn.classList.remove('done'); }},1200);
  }}).catch(function(){{ btn.textContent='复制失败'; }});
}}
</script>
</body>
</html>"#,
        name_h = html_escape(&v.name),
        status_cls = status_cls,
        status_label = status_label,
        bar_cls = bar_cls,
        pct = pct,
        used = fmt_bytes(v.used()),
        total = total_str,
        reset = reset_desc,
        expire = describe_expire(v.expire_at),
        up = fmt_bytes(v.cycle_up.max(0)),
        down = fmt_bytes(v.cycle_down.max(0)),
        billing = billing,
        n_nodes = links.len(),
        sub_rows = sub_rows,
        node_rows = node_rows,
    )
}

fn fmt_bytes(n: i64) -> String {
    crate::model::user::User::format_bytes(n)
}

fn copy_row(label: &str, url: &str) -> String {
    format!(
        r#"    <div class="row">
      <span class="name">{label_h}</span>
      <code>{url_h}</code>
      <button onclick="copy(this,'{url_j}')">复制</button>
    </div>
"#,
        label_h = html_escape(label),
        url_h = html_escape(url),
        url_j = js_escape(url),
    )
}

fn node_block(l: &ShareLink) -> String {
    format!(
        r#"    <div class="node">
      <div class="row">
        <span class="name">{tag_h} <span style="color:var(--muted);">· {proto_h}</span></span>
        <code>{link_h}</code>
        <button onclick="copy(this,'{link_j}')">复制</button>
      </div>
      <details><summary>QR</summary>{qr}</details>
    </div>"#,
        tag_h = html_escape(&l.tag),
        proto_h = html_escape(&l.protocol),
        link_h = html_escape(&l.link),
        link_j = js_escape(&l.link),
        qr = qrcode_svg(&l.link),
    )
}

fn describe_expire(expire_at: Option<i64>) -> String {
    let Some(ts) = expire_at else { return "无限期".into() };
    let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) else { return "?".into() };
    let date = dt.with_timezone(&chrono::Local).date_naive();
    let today = chrono::Local::now().date_naive();
    let days = (date - today).num_days();
    if days < 0 {
        format!("{date} (已过期 {} 天)", -days)
    } else if days == 0 {
        format!("{date} (今日到期)")
    } else {
        format!("{date} (还有 {days} 天)")
    }
}

fn qrcode_svg(data: &str) -> String {
    match QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<svg::Color>()
            .min_dimensions(200, 200)
            .dark_color(svg::Color("#0d1117"))
            .light_color(svg::Color("#ffffff"))
            .build(),
        // 链接太长会超出二维码容量。少一个二维码不该让整页 500 ——
        // 链接本身还在,复制按钮照常能用。
        Err(_) => r#"<div class="empty">链接过长,无法生成二维码</div>"#.into(),
    }
}

/// HTML 文本节点 / 属性的最小转义。
///
/// 属性我们只用双引号,但单引号也一起转:这样同一个函数在两种属性写法下都安全,
/// 不用调用方去记「这里能不能用」。
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// 在 JS 单引号字符串里安全嵌入。
///
/// 这些字符串还会被放进 HTML 属性(`onclick="copy(this,'…')"`),所以:
///   * **双引号必须转** —— 否则一个含 `"` 的 tag 就能闭合 onclick 属性;
///   * **`&` 必须转** —— 浏览器先做 HTML 实体解码,不转的话 `&#39;` 会还原成引号;
///   * **`<` `>` 必须转** —— 防止 `</script>` 提前截断脚本块。
fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\x27"),
            '"' => out.push_str("\\x22"),
            '&' => out.push_str("\\x26"),
            '<' => out.push_str("\\x3c"),
            '>' => out.push_str("\\x3e"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> StatsView {
        StatsView {
            name: "alice".into(),
            enabled: true,
            auto_disabled: false,
            quota_bytes: 100 * 1_073_741_824,
            cycle_up: 20 * 1_073_741_824,
            cycle_down: 55 * 1_073_741_824,
            traffic_multiplier: 1.0,
            expire_at: None,
            reset_day: Some(22),
            sub_token: "tok".into(),
        }
    }

    fn link(tag: &str, link: &str) -> ShareLink {
        ShareLink { tag: tag.into(), protocol: "vless-reality".into(), link: link.into() }
    }

    #[test]
    fn js_escape_closes_every_attribute_escape_hatch() {
        let evil = r#"a"b'c&d<e>f\g"#;
        let out = js_escape(evil);
        for bad in ['"', '\'', '&', '<', '>'] {
            assert!(!out.contains(bad), "{bad} 未被转义: {out}");
        }
        assert!(out.contains("\\\\"), "反斜杠要转义,否则能吃掉后面的转义符: {out}");
    }

    #[test]
    fn html_escape_covers_quotes() {
        assert_eq!(html_escape(r#"<a href="x">"#), "&lt;a href=&quot;x&quot;&gt;");
        assert_eq!(html_escape("a'b"), "a&#39;b");
    }

    /// 用户名和节点 tag 都来自库里可编辑的字段。它们落在三种上下文里
    /// (HTML 文本、HTML 属性、onclick 里的 JS 字符串),每一种都不能被逃出去。
    #[test]
    fn hostile_names_do_not_break_out_of_the_page() {
        let mut v = view();
        v.name = r#"<script>alert(1)</script>"#.into();
        let evil_tag = r#"a"><img src=x onerror=alert(1)>"#;
        let html = render(
            &v,
            &[link(evil_tag, r#"vless://x?sni="evil'"#)],
            "https://sub.example.com",
        );

        // 判据是「有没有生成新标签」,不是「文本里有没有出现 onerror」——
        // 转义之后 `onerror=` 这几个字仍然会作为**可见文本**留在页面上,那是无害的。
        assert!(!html.contains("<script>alert"), "用户名生成了脚本标签");
        assert!(!html.contains("<img"), "tag 生成了 img 标签");
        // 危险字符确实被转成了实体 / JS 转义。
        assert!(html.contains("&lt;script&gt;"), "用户名没被 HTML 转义");
        assert!(html.contains("\\x22"), "onclick 里的双引号没被 JS 转义");
        // 页面自己的那段 <script> 还在,而且没有被链接里的 `<` 提前截断。
        assert!(html.contains("function copy(btn,text)"));
        assert_eq!(html.matches("<script>").count(), 1, "只该有一段脚本");
    }

    #[test]
    fn usage_applies_the_multiplier_and_is_capped_at_100() {
        let mut v = view();
        assert_eq!(v.used(), 75 * 1_073_741_824);
        assert!((v.percent() - 75.0).abs() < 0.01);

        v.traffic_multiplier = 2.0;
        assert!((v.percent() - 100.0).abs() < 0.01, "超额要夹到 100%,不能画出界");
    }

    #[test]
    fn status_label_reflects_why_it_is_unusable() {
        let cases: Vec<(StatsView, &str)> = vec![
            (StatsView { enabled: false, ..view() }, "已停用"),
            (StatsView { enabled: false, auto_disabled: true, ..view() }, "已自动停用"),
            (StatsView { expire_at: Some(0), ..view() }, "已到期"),
            (StatsView { cycle_up: 200 * 1_073_741_824, ..view() }, "已超额"),
            (view(), "正常"),
        ];
        for (v, want) in cases {
            let html = render(&v, &[], "https://x");
            assert!(html.contains(want), "期望状态 {want}");
        }
    }

    #[test]
    fn unlimited_quota_shows_no_number() {
        let v = StatsView { quota_bytes: 0, ..view() };
        let html = render(&v, &[], "https://x");
        assert!(html.contains("<b>不限</b>"), "无配额应显示「不限」");
        assert!(html.contains("width:0.0%"), "无配额时进度条应当是空的");
    }

    #[test]
    fn nodes_get_links_and_qr_codes() {
        let html = render(&view(), &[link("tokyo", "vless://abc@1.2.3.4:443#tokyo")], "https://s");
        assert!(html.contains("vless://abc@1.2.3.4:443#tokyo"));
        assert!(html.contains("<svg"), "应当内联二维码");
        assert!(html.contains("单节点 (1)"));
    }

    #[test]
    fn no_nodes_says_so_instead_of_rendering_an_empty_card() {
        let html = render(&view(), &[], "https://s");
        assert!(html.contains("暂无可用节点"));
    }

    /// base_url 的尾斜杠不该产生 `https://s//sub/tok`。
    #[test]
    fn subscription_urls_have_no_double_slash() {
        let html = render(&view(), &[], "https://s/");
        assert!(html.contains("https://s/sub/tok"));
        assert!(!html.contains("https://s//sub/tok"));
        assert!(html.contains("https://s/sub/tok?type=clash"));
    }

    #[test]
    fn expire_description_counts_days() {
        assert_eq!(describe_expire(None), "无限期");
        let tomorrow = (chrono::Local::now() + chrono::Duration::days(2)).timestamp();
        assert!(describe_expire(Some(tomorrow)).contains("还有"));
        let past = (chrono::Local::now() - chrono::Duration::days(3)).timestamp();
        assert!(describe_expire(Some(past)).contains("已过期"));
        // 坏时间戳不该 panic。
        assert_eq!(describe_expire(Some(i64::MAX)), "?");
    }

    /// 这一页**不能**出现任何服务端凭据。链接里有用户自己的密码是必然的
    /// (那是他要用的),但 reality 私钥、证书私钥一律不该到这里。
    #[test]
    fn page_contains_no_server_side_secrets() {
        let html = render(&view(), &[link("t", "vless://u@1.2.3.4:443?pbk=PUBLIC#t")], "https://s");
        assert!(!html.contains("BEGIN PRIVATE KEY"));
        assert!(!html.contains("private_key"));
    }
}
