//! 主控配置。分节 TOML + `#[serde(default)]`,模式照搬旧项目 `model/config.rs`(§11.2)。
//!
//! `#[serde(default)]` 铺满每一层是刻意的:配置文件缺一整节时应当拿到默认值,
//! 而不是启动失败。旧项目在这点上吃过苦头——加一个新配置项就让所有老配置文件报错。
//!
//! **没有 `[kernel]` 段。** 旧项目的 `KernelConfig { update_repo, ... }` 整个不存在(§9.2):
//! 不 fork sing-box、不自编译内核,版本就是 `agent/go.mod` 里的一行。

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
}
