//! 主控配置。分节 TOML + `#[serde(default)]`,模式照搬旧项目 `model/config.rs`(§11.2)。
//!
//! `#[serde(default)]` 铺满每一层是刻意的:配置文件缺一整节时应当拿到默认值,
//! 而不是启动失败。旧项目在这点上吃过苦头——加一个新配置项就让所有老配置文件报错。
//!
//! **没有 `[kernel]` 段。** 旧项目的 `KernelConfig { update_repo, ... }` 整个不存在(§9.2):
//! 不 fork sing-box、不自编译内核,版本就是 `agent/go.mod` 里的一行。

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub db: DbConfig,
    pub cluster: ClusterConfig,
    pub subscription: SubscriptionConfig,
    pub telegram: TelegramConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DbConfig {
    pub path: String,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self { path: "/etc/sbx/sbx.db".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterConfig {
    pub listen: String,
    /// 被控机回连主控用的地址(IP 或域名,不含端口)。空 = 自动定。
    ///
    /// 自动那条路会先问外部服务「你看到我是谁」,再退回本机出口地址。
    /// 云主机的网卡拿到的是**厂商给的内网地址**,所以本机视角靠不住 ——
    /// 这个字段是给「自动也定不对」的场景留的最后一手(§8.1)。
    pub public_host: String,
    /// false 则明文 ws(agent 侧也要 `insecure = true`)。
    pub tls: bool,
    /// 不存在时由主控自己用 rcgen 生成一张自签证书写进去(§1.3)。
    pub cert_path: String,
    pub key_path: String,
    pub heartbeat_secs: u64,
    pub report_interval_secs: u64,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:18443".into(),
            public_host: String::new(),
            tls: true,
            cert_path: "/etc/sbx/tls/cert.pem".into(),
            key_path: "/etc/sbx/tls/key.pem".into(),
            heartbeat_secs: 10,
            report_interval_secs: 30,
        }
    }
}

/// 订阅 HTTP 监听是 §2「不做 Web 面板」的**唯一例外**:
/// 只吐订阅内容与 stats_html,不提供任何管理能力。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscriptionConfig {
    pub listen: String,
    pub public_base: String,
    pub use_public_base_as_server: bool,
    pub enabled: bool,
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:18081".into(),
            public_base: String::new(),
            use_public_base_as_server: false,
            enabled: true,
        }
    }
}

impl Config {
    /// 从 TOML 文本解析。文件不存在时调用方应当用 `Config::default()`。
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
    }
}

/// Telegram 通知(§9.1)。字段与默认值沿用旧项目 `TelegramConfig`。
///
/// **`bot_token` 是凭据**:日志与 TUI 里一律不回显完整值(§11.3)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelegramConfig {
    /// 默认关。填了 token 也要显式打开 —— 通知会主动往外发消息,
    /// 不该因为配置里留了一行就自己启动。
    pub enabled: bool,
    pub bot_token: String,
    /// 只接受 `±HH:MM` 偏移和**不走夏令时**的 IANA 别名。理由见 `tg::fmt::parse_timezone`。
    pub timezone: String,
    /// 管理员的 chat_id。空 = 没有管理员,只有用户侧通知。
    pub admin_chat_ids: Vec<i64>,
    pub poll_interval_secs: u64,
    pub request_timeout_secs: u64,

    /// 新用户的默认阈值开关与时间表。用户可以在 bot 里各自改。
    pub default_notify_quota_80: bool,
    pub default_notify_quota_90: bool,
    pub default_notify_quota_100: bool,
    pub default_schedule_enabled: bool,
    pub default_schedule_times: Vec<String>,

    pub admin_notify_quota: bool,
    pub admin_schedule_enabled: bool,
    pub admin_schedule_times: Vec<String>,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            timezone: "Asia/Shanghai".into(),
            admin_chat_ids: Vec::new(),
            poll_interval_secs: 2,
            request_timeout_secs: 10,
            default_notify_quota_80: true,
            default_notify_quota_90: true,
            default_notify_quota_100: true,
            default_schedule_enabled: true,
            default_schedule_times: vec!["09:00".into(), "21:30".into()],
            admin_notify_quota: true,
            admin_schedule_enabled: true,
            admin_schedule_times: vec!["09:00".into(), "21:30".into()],
        }
    }
}

// ─────────────────────────── 就地改配置 ───────────────────────────

/// 改一个配置项,**保留文件里的注释与排版**。
///
/// 为什么不是「反序列化 → 改字段 → `toml::to_string` 写回」:
/// 那样一次保存就会把 `config.example.toml` 里那几十行解释性注释全部抹掉,
/// 而那些注释是这个项目里唯一说明「这个端口为什么不能对外」的地方。
/// 所以这里做的是**行级替换**:找到 `[section]`,在它的范围内找 `key =`,
/// 只换等号右边那一段。找不到就补一行,连节都没有就补一节。
///
/// 只支持标量与字符串数组 —— 设置页要改的就这些。嵌套表不在范围内,
/// 需要的话应当去编辑器里改文件,而不是把这个函数越写越像一个 TOML 库。
pub fn set_value(path: &str, section: &str, key: &str, value: &str) -> anyhow::Result<()> {
    use std::io::Write;

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("读取配置 {path} 失败"))),
    };
    let updated = replace_in_toml(&text, section, key, value);

    // 先写临时文件再 rename:写到一半断电/磁盘满,留下的是旧配置而不是半个配置。
    // 半个配置的表现是 daemon 起不来,而那时人已经不记得刚才改了什么。
    let tmp = format!("{path}.new");
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("写 {tmp} 失败(目录不可写?)"))?;
        // 临时文件是按 umask 新建的(通常 0644),而原文件里有 bot_token,管理员可能
        // 特意 chmod 成 0600。rename 之后权限跟着临时文件走,所以先把原来的复制过来。
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = f.set_permissions(meta.permissions());
        }
        f.write_all(updated.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("替换 {path} 失败"))?;
    Ok(())
}

/// `set_value` 的纯函数内核。分出来是为了能测 —— 文件 IO 那一层没什么可测的。
pub fn replace_in_toml(text: &str, section: &str, key: &str, value: &str) -> String {
    let want_header = format!("[{section}]");
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();

    // 定位目标节的范围:从它的头部行到下一个 `[` 开头的行(或文件末尾)。
    let start = lines.iter().position(|l| l.trim() == want_header);
    let Some(start) = start else {
        // 整节都没有:补在末尾。
        if !lines.is_empty() && !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
            lines.push(String::new());
        }
        lines.push(want_header);
        lines.push(format!("{key} = {value}"));
        return finish(lines);
    };
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());

    // 在这一节里找 `key =`。**跳过被注释掉的行** —— 示例配置里到处是
    // `# public_base = "..."` 这样的注释样例,改到那上面等于什么都没改。
    let found = lines[start + 1..end].iter().position(|l| {
        let t = l.trim_start();
        !t.starts_with('#')
            && t.strip_prefix(key).map(|rest| rest.trim_start().starts_with('=')).unwrap_or(false)
    });

    match found {
        Some(i) => {
            let idx = start + 1 + i;
            // 保留原有缩进。
            let indent: String = lines[idx].chars().take_while(|c| c.is_whitespace()).collect();
            lines[idx] = format!("{indent}{key} = {value}");
        }
        None => {
            // 节里没有这个 key:插在节内最后一个非空行之后,别插到下一节头上。
            let mut at = end;
            while at > start + 1 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            lines.insert(at, format!("{key} = {value}"));
        }
    }
    finish(lines)
}

fn finish(mut lines: Vec<String>) -> String {
    // TOML 文件按惯例以换行结尾;少了它有些工具会把最后一行当成没写完。
    lines.push(String::new());
    lines.join("\n")
}

/// 把一个字符串转成 TOML 的基本字符串字面量。
pub fn toml_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空配置文件必须能解析成全默认值,而不是报错。
    #[test]
    fn empty_config_parses_to_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.cluster.listen, "0.0.0.0:18443");
        assert_eq!(c.cluster.heartbeat_secs, 10);
        assert_eq!(c.cluster.report_interval_secs, 30, "沿用旧项目的 30s(§9.3)");
        assert!(c.cluster.tls, "默认必须开 TLS");
        assert_eq!(c.subscription.listen, "127.0.0.1:18081");
        assert_eq!(c.db.path, "/etc/sbx/sbx.db");
    }

    /// 只写一节时,其它节仍取默认值 —— 这是 `#[serde(default)]` 铺满每层的理由。
    #[test]
    fn partial_config_keeps_other_sections_default() {
        let c = Config::parse("[cluster]\nlisten = \"0.0.0.0:9999\"\n").unwrap();
        assert_eq!(c.cluster.listen, "0.0.0.0:9999");
        assert_eq!(c.cluster.heartbeat_secs, 10, "同节内未写的字段也该有默认值");
        assert_eq!(c.subscription.listen, "127.0.0.1:18081", "未写的节应为默认");
    }

    #[test]
    fn config_round_trips_through_toml() {
        let mut c = Config::default();
        c.cluster.tls = false;
        c.subscription.public_base = "https://sub.example.com".into();
        let back = Config::parse(&toml::to_string(&c).unwrap()).unwrap();
        assert!(!back.cluster.tls);
        assert_eq!(back.subscription.public_base, "https://sub.example.com");
    }

    /// 配置里不该有 kernel 段(§9.2)。若有人加回来,这个测试会失败。
    #[test]
    fn unknown_kernel_section_is_not_a_field() {
        let toml_text = toml::to_string(&Config::default()).unwrap();
        assert!(!toml_text.contains("kernel"), "不该有 [kernel] 段: {toml_text}");
    }

    // ── 就地改配置 ──

    const SAMPLE: &str = r#"# 顶部说明
[cluster]
# 这个端口 agent 要能连到,必须对外
listen = "0.0.0.0:18443"
tls = true

[subscription]
# public_base = "https://sub.example.com"   ← 注释掉的样例
listen = "127.0.0.1:18081"
"#;

    /// **注释必须留着。** 这是这套行级替换存在的全部理由:
    /// 反序列化再写回会把配置文件里那几十行解释抹掉,而那些解释是唯一
    /// 说明「这个端口为什么不能对外」的地方。
    #[test]
    fn editing_preserves_comments_and_layout() {
        let out = replace_in_toml(SAMPLE, "cluster", "listen", "\"0.0.0.0:19443\"");
        assert!(out.contains("# 顶部说明"));
        assert!(out.contains("# 这个端口 agent 要能连到,必须对外"));
        assert!(out.contains("listen = \"0.0.0.0:19443\""));
        assert!(!out.contains("0.0.0.0:18443"));
        // 别的节不能被动到。
        assert!(out.contains("listen = \"127.0.0.1:18081\""));
        assert!(out.contains("tls = true"));
    }

    /// 同名 key 在两个节里都有(`listen`)—— 只能改指定的那一节。
    #[test]
    fn editing_only_touches_the_named_section() {
        let out = replace_in_toml(SAMPLE, "subscription", "listen", "\"0.0.0.0:8080\"");
        assert!(out.contains("listen = \"0.0.0.0:18443\""), "cluster 那个不该被改:\n{out}");
        assert!(out.contains("listen = \"0.0.0.0:8080\""));
    }

    /// **被注释掉的样例行不算数。** 示例配置里到处是 `# key = "..."`,
    /// 改到那上面等于什么都没改,而界面会显示「已保存」。
    #[test]
    fn a_commented_out_sample_is_not_the_key() {
        let out = replace_in_toml(SAMPLE, "subscription", "public_base", "\"https://x.example\"");
        assert!(
            out.contains("# public_base = \"https://sub.example.com\""),
            "注释行要留着:\n{out}"
        );
        assert!(out.contains("public_base = \"https://x.example\""));
        // 新行要插在这一节里,不能跑到下一节或文件末尾。
        let sub_start = out.find("[subscription]").unwrap();
        assert!(out[sub_start..].contains("public_base = \"https://x.example\""));
    }

    #[test]
    fn a_missing_section_is_appended() {
        let out = replace_in_toml("[cluster]\ntls = true\n", "telegram", "enabled", "true");
        assert!(out.contains("[telegram]"));
        assert!(out.contains("enabled = true"));
        assert!(Config::parse(&out).is_ok(), "补出来的东西必须还能解析:\n{out}");
    }

    /// 改完之后必须还是合法的 TOML,而且值真的变了 —— 这一条把上面几个串起来。
    #[test]
    fn the_result_still_parses_with_the_new_value() {
        let out = replace_in_toml(SAMPLE, "subscription", "public_base", "\"https://x.example\"");
        let c = Config::parse(&out).expect("改完还得能解析");
        assert_eq!(c.subscription.public_base, "https://x.example");
        assert_eq!(c.cluster.listen, "0.0.0.0:18443");
    }

    #[test]
    fn empty_file_gets_a_section_and_a_key() {
        let out = replace_in_toml("", "cluster", "tls", "false");
        let c = Config::parse(&out).unwrap();
        assert!(!c.cluster.tls);
    }

    /// 缩进要保留 —— 有人喜欢缩进写 TOML,改一次就被拉平会让 diff 里全是噪音。
    #[test]
    fn indentation_is_preserved() {
        let out = replace_in_toml("[cluster]\n    tls = true\n", "cluster", "tls", "false");
        assert!(out.contains("    tls = false"), "{out}");
    }

    #[test]
    fn toml_string_escapes_quotes_and_backslashes() {
        assert_eq!(toml_string("abc"), "\"abc\"");
        assert_eq!(toml_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(toml_string("C:\\x"), "\"C:\\\\x\"");
        // 转义之后必须还能被 TOML 解析回原文。
        let out = replace_in_toml("[db]\n", "db", "path", &toml_string("C:\\a\"b"));
        assert_eq!(Config::parse(&out).unwrap().db.path, "C:\\a\"b");
    }

    /// 对着**真的那个文件**跑一遍:`packaging/config.example.toml` 就是
    /// 装完之后人手里的那份,设置页要改的也正是它这个形状。
    ///
    /// 拿一个手写的小样本测不够 —— 真文件里有整段注释、注释掉的样例行、
    /// 空行分节,这些恰恰是行级替换最容易踩的地方。
    #[test]
    fn editing_the_real_example_config_keeps_every_comment() {
        let text = include_str!("../../packaging/config.example.toml");
        let comments_before = text.lines().filter(|l| l.trim_start().starts_with('#')).count();
        assert!(comments_before > 10, "样例配置本来就该有大量注释");

        // 依次改几项,模拟人在设置页里连着改。
        let mut out = text.to_string();
        for (section, key, value) in [
            ("subscription", "public_base", "\"https://sub.example.com\""),
            ("cluster", "heartbeat_secs", "15"),
            ("telegram", "enabled", "true"),
            ("telegram", "admin_chat_ids", "[123, 456]"),
        ] {
            out = replace_in_toml(&out, section, key, value);
        }

        let comments_after = out.lines().filter(|l| l.trim_start().starts_with('#')).count();
        assert_eq!(comments_after, comments_before, "注释被抹掉了:\n{out}");

        let c = Config::parse(&out).expect("改完必须还能解析");
        assert_eq!(c.subscription.public_base, "https://sub.example.com");
        assert_eq!(c.cluster.heartbeat_secs, 15);
        assert!(c.telegram.enabled);
        assert_eq!(c.telegram.admin_chat_ids, vec![123, 456]);
        // 没碰的项要保持原样。
        let orig = Config::parse(text).unwrap();
        assert_eq!(c.cluster.listen, orig.cluster.listen);
        assert_eq!(c.db.path, orig.db.path);
    }
}
