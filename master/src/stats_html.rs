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

/// 原始字节数 × 倍率。负数当 0 —— 一个负的用量会让进度条和百分比一起变成负数。
fn billed(raw: i64, mult: f64) -> i64 {
    (raw.max(0) as f64 * mult.max(0.0)) as i64
}

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
    /// 绑了网卡时的替代口径(§10.3)。`Some` = 页面上那几个用量数字换成网卡口径。
    ///
    /// **账号状态(正常/超额/停用)仍然按用户自己的用量判**。绑网卡是一个
    /// 纯显示功能,不该能把人停掉 —— 所以页面上必须同时把两个数字都写出来,
    /// 否则「已用 900 GB」旁边挂着「正常」会像是个 bug。
    pub nic: Option<NicUsage>,
}

/// 所绑机器的网卡用量之和。
#[derive(Debug, Clone, Copy, Default)]
pub struct NicUsage {
    pub agents: usize,
    pub up: i64,
    pub down: i64,
    /// 各机器配额之和;**只要有一台没设配额就是 0(不限)**。
    /// 把设了的几台加起来当上限,会给出一个看起来精确、其实根本不是上限的数字。
    pub quota: i64,
}

impl NicUsage {
    fn total(&self) -> i64 {
        self.up.saturating_add(self.down)
    }
}

impl StatsView {
    /// 页面主区显示的用量。绑了网卡就是网卡口径,否则是账号自己的计费用量。
    fn shown_used(&self) -> i64 {
        match &self.nic {
            Some(n) => n.total(),
            None => self.used(),
        }
    }

    /// 页面主区显示的上限。
    fn shown_quota(&self) -> i64 {
        match &self.nic {
            Some(n) => n.quota,
            None => self.quota_bytes,
        }
    }

    /// 页面上那两个上行/下行数字。绑了网卡就是网卡口径,否则是**计费口径**(含倍率)。
    ///
    /// 早先未绑网卡时这里给的是单倍原始值,而正上方的「已用」是乘过倍率的 ——
    /// 于是上行加下行对不上已用,用户会照着这个差额来问是不是算错了。
    fn shown_up(&self) -> i64 {
        match &self.nic {
            Some(n) => n.up.max(0),
            None => billed(self.cycle_up, self.traffic_multiplier),
        }
    }

    fn shown_down(&self) -> i64 {
        match &self.nic {
            Some(n) => n.down.max(0),
            None => billed(self.cycle_down, self.traffic_multiplier),
        }
    }

    /// 账号自己的计费用量(含倍率)。与 §6.3 的配额判定、TUI 的用量列同一个口径 ——
    /// 三处显示不同的数字会让用户来问「到底哪个准」。
    ///
    /// 分别乘再相加,与 `shown_up + shown_down` 严格相等 ——
    /// 先加再乘会差最多 1 字节,而这两个数就摆在同一屏上。
    fn used(&self) -> i64 {
        billed(self.cycle_up, self.traffic_multiplier)
            .saturating_add(billed(self.cycle_down, self.traffic_multiplier))
    }

    fn percent(&self) -> f64 {
        if self.quota_bytes <= 0 {
            return 0.0;
        }
        (self.used() as f64 / self.quota_bytes as f64 * 100.0).clamp(0.0, 100.0)
    }

    /// 进度条的百分比。跟着主区显示的那两个数走 —— 条画的是 `已用 / 上限`,
    /// 这两个数换成网卡口径了,条还按账号自己的算就对不上,看着像画错了。
    fn shown_percent(&self) -> f64 {
        let quota = self.shown_quota();
        if quota <= 0 {
            return 0.0;
        }
        (self.shown_used() as f64 / quota as f64 * 100.0).clamp(0.0, 100.0)
    }
}

/// 渲染整页。`base_url` 形如 `https://sub.example.com`(不带尾斜杠)。
pub fn render(v: &StatsView, links: &[ShareLink], base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    // 状态判定始终用**账号自己的**用量:绑网卡是纯显示功能,不该影响
    // 「这个号现在能不能用」的结论(§10.3)。
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

    let shown_quota = v.shown_quota();
    let total_str = if shown_quota <= 0 { "不限".to_string() } else { fmt_bytes(shown_quota) };
    let reset_desc = match v.reset_day {
        Some(d) if (1..=31).contains(&d) => format!("每月 {d} 号"),
        _ => "不重置".into(),
    };
    // 绑了网卡时倍率是不参与的(网卡数字直接报),写倍率会误导。
    let billing = match &v.nic {
        Some(n) => format!("网卡口径 · {} 台机器", n.agents),
        None if (v.traffic_multiplier - 1.0).abs() < 0.01 => "单向".to_string(),
        None if (v.traffic_multiplier - 2.0).abs() < 0.01 => "双向".to_string(),
        None => format!("{:.1}x", v.traffic_multiplier),
    };
    // 绑了网卡就多一行,把账号自己的用量也写出来 —— 少了这一行,
    // 「已用 900 GB」旁边挂着「正常」会像是个 bug,而那才是账号的真实状态。
    let own_row = match &v.nic {
        Some(_) => {
            format!(r#"<span>账号自身: <b>{}</b>(停用判定按这个数)</span>"#, fmt_bytes(v.used()))
        }
        None => String::new(),
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
      {own_row}
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
        pct = v.shown_percent(),
        used = fmt_bytes(v.shown_used()),
        total = total_str,
        reset = reset_desc,
        expire = describe_expire(v.expire_at),
        up = fmt_bytes(v.shown_up()),
        down = fmt_bytes(v.shown_down()),
        billing = billing,
        own_row = own_row,
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
            nic: None,
            expire_at: None,
            reset_day: Some(22),
            sub_token: "tok".into(),
        }
    }

    fn link(tag: &str, link: &str) -> ShareLink {
        ShareLink { tag: tag.into(), protocol: "vless-reality".into(), link: link.into() }
    }

    fn nic_view() -> StatsView {
        StatsView {
            nic: Some(NicUsage {
                agents: 2,
                up: 300 * 1_073_741_824,
                down: 600 * 1_073_741_824,
                quota: 2000 * 1_073_741_824,
            }),
            ..view()
        }
    }

    /// 把绑了网卡的那张页面的关键几行打出来看一眼:
    ///
    /// ```sh
    /// cargo test stats_html::tests::preview_nic -- --nocapture
    /// ```
    ///
    /// 断言能守住数字,守不住「读起来是不是那个意思」—— 同一屏上摆着两个
    /// 口径的数字,措辞稍微含糊一点就会被读成自相矛盾。
    #[test]
    fn preview_nic() {
        let html = render(&nic_view(), &[], "https://sub.example.com");
        for line in html.lines() {
            let t = line.trim();
            if t.starts_with("<div class=\"usage\">")
                || t.starts_with("<span>")
                || t.starts_with("<h1>")
            {
                // 去掉标签,只留人眼看得见的文字。
                let mut out = String::new();
                let mut skip = false;
                for c in t.chars() {
                    match c {
                        '<' => skip = true,
                        '>' => skip = false,
                        _ if !skip => out.push(c),
                        _ => {}
                    }
                }
                let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
                if !out.is_empty() {
                    println!("  {out}");
                }
                // 注:`.usage` 那一行是 flex + space-between 的两个 span
                // (左「已用 X」、右「上限」),上面的剥标签会把它们并成一串 ——
                // 浏览器里它们分居两端。
            }
        }
    }

    /// 绑了网卡之后,统计页上那几个用量数字要换成**网卡口径**。
    ///
    /// 这是这个功能存在的理由:VPS 厂商按网卡计费,管理员要能用任意一个
    /// 代理客户端(或者浏览器)看到「这几台机器这个月烧了多少」,不用 ssh 上去查。
    /// 之前只换了 `subscription-userinfo` 响应头,网页上还是账号自己的用量 ——
    /// 于是绑完之后打开页面,看到的还是原来那个数,像是没生效。
    #[test]
    fn binding_nics_replaces_the_numbers_on_the_page() {
        let html = render(&nic_view(), &[], "https://sub.example.com");
        assert!(html.contains("900.00 GB"), "已用该是网卡的 300+600:\n{html}");
        assert!(html.contains("300.00 GB"), "上行该是网卡上行");
        assert!(html.contains("600.00 GB"), "下行该是网卡下行");
        assert!(html.contains("1.95 TB"), "上限该是各机器配额之和");
        assert!(html.contains("网卡口径"), "得标出来这是网卡口径,不然两个数对不上没法解释");
        assert!(html.contains("2 台机器"), "绑了几台要写出来");
    }

    /// **账号状态仍然按账号自己的用量判。**
    ///
    /// 绑网卡是一个纯显示功能,不该能把人停掉(§10.3)。所以页面上既要显示
    /// 网卡的大数字,又必须把账号自身的用量写出来 —— 少了后面这一行,
    /// 「已用 900 GB / 上限 1.95 TB」旁边挂着一个「正常」会像是个 bug,
    /// 而那恰恰是账号的真实状态。
    #[test]
    fn binding_nics_never_changes_the_account_status() {
        let html = render(&nic_view(), &[], "https://sub.example.com");
        assert!(html.contains("正常"), "账号自己只用了 75 GB / 100 GB,该是正常:\n{html}");
        assert!(html.contains("账号自身"), "得把账号自己的用量也写出来");
        assert!(html.contains("75.00 GB"), "账号自身的计费用量该照原样显示");

        // 网卡烧爆了也不该把状态染红 —— 那是厂商的账,不是这个号的账。
        let mut v = nic_view();
        v.nic = Some(NicUsage {
            agents: 1,
            up: 0,
            down: 3000 * 1_073_741_824,
            quota: 100 * 1_073_741_824,
        });
        let html = render(&v, &[], "https://sub.example.com");
        assert!(html.contains("正常"), "网卡超了不该让账号显示成超额:\n{html}");
    }

    /// 没绑网卡的用户,页面一个字都不该变。
    #[test]
    fn without_a_binding_the_page_is_unchanged() {
        let html = render(&view(), &[], "https://sub.example.com");
        assert!(html.contains("75.00 GB"), "该显示账号自己的计费用量");
        assert!(!html.contains("网卡口径"), "没绑就不该出现网卡的说法:\n{html}");
        assert!(!html.contains("账号自身"), "没绑就不需要那一行补充说明");
    }

    /// **上行 + 下行要等于「已用」。**
    ///
    /// 这一页是用户唯一能自己看到的账,而它上面同时摆着三个数。
    /// 早先上下行给的是单倍原始值、已用是乘过倍率的,于是 x2 的用户看到
    /// 「上行 20 GB + 下行 55 GB」下面写着「已用 150 GB」—— 差了整整一倍,
    /// 只能来问是不是算错了。
    #[test]
    fn the_two_directions_add_up_to_the_used_figure() {
        let v = StatsView { traffic_multiplier: 2.0, ..view() };
        let html = render(&v, &[], "https://sub.example.com");
        assert!(html.contains("40.00 GB"), "上行该是 20 GiB × 2:\n{html}");
        assert!(html.contains("110.00 GB"), "下行该是 55 GiB × 2:\n{html}");
        assert!(html.contains("150.00 GB"), "已用该是两者之和:\n{html}");
        // 单倍原始值不该再出现 —— 它就是那个让人对不上账的数。
        assert!(!html.contains("20.00 GB"), "不该再露出单倍上行:\n{html}");
    }

    /// 绑了网卡的话,上下行仍然是**网卡口径** —— 那是这个功能的全部意义。
    ///
    /// 倍率只作用在账号自己那本账上;把倍率乘到网卡数字上,
    /// 得到的既不是厂商的账也不是用户的账。
    #[test]
    fn a_bound_nic_is_reported_raw_regardless_of_the_multiplier() {
        let v = StatsView { traffic_multiplier: 2.0, ..nic_view() };
        let html = render(&v, &[], "https://sub.example.com");
        assert!(html.contains("300.00 GB"), "上行还是网卡上行,不乘倍率:\n{html}");
        assert!(html.contains("600.00 GB"), "下行还是网卡下行,不乘倍率:\n{html}");
        // 账号自己那一行照旧按倍率算(20+55)×2 = 150。
        assert!(html.contains("150.00 GB"), "账号自身那行该含倍率:\n{html}");
    }

    /// 只要有一台机器没设配额,总量就报「不限」。
    ///
    /// 把设了配额的几台加起来当上限,会给出一个看起来精确、其实根本不是上限的
    /// 数字 —— 那比不给更糟,因为页面会拿它算百分比、画进度条。
    #[test]
    fn an_uncapped_machine_makes_the_total_unlimited() {
        let mut v = nic_view();
        v.nic = Some(NicUsage { agents: 2, up: 1 << 30, down: 1 << 30, quota: 0 });
        let html = render(&v, &[], "https://sub.example.com");
        assert!(html.contains("不限"), "有一台没配额就该报不限:\n{html}");
        assert!(html.contains("width:0.0%"), "上限未知时条该是空的,不是满的");
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
        let html =
            render(&v, &[link(evil_tag, r#"vless://x?sni="evil'"#)], "https://sub.example.com");

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
