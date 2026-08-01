//! 被控服务器(agent)模型。旧项目没有对应物,整个是新增的。
//!
//! 这里有一批 `#[allow(dead_code)]`,它们**不是**遗留垃圾:
//! `AgentStatus`、`Agent` 的那几个判定方法、`AgentNicTraffic` 都是给 TUI(§8)用的,
//! 而 TUI 还没写。db 层目前直接读写字符串/整数列,不经过这层封装。
//! TUI 落地后这些 allow 应当逐个删掉 —— 如果那时发现某个还是没人用,那才是该删的代码。

use serde::{Deserialize, Serialize};

/// agent 连接状态。`never` 与 `offline` 要分开:
/// 前者是「加了但从没连上过」(通常是 token 没贴对或防火墙),
/// 后者是「连过,现在断了」——两者的排查方向完全不同,TUI 上也应该显示不同的提示。
#[allow(dead_code)] // TUI(§8)的 agents 页会用
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Never,
    Online,
    Offline,
}

#[allow(dead_code)] // 同上
impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Never => "never",
            AgentStatus::Online => "online",
            AgentStatus::Offline => "offline",
        }
    }

    /// 未知取值按 `never` 处理而不是 panic:库可能被手工改过。
    pub fn parse(s: &str) -> Self {
        match s {
            "online" => AgentStatus::Online,
            "offline" => AgentStatus::Offline,
            _ => AgentStatus::Never,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Agent {
    pub id: i64,
    pub name: String,
    /// sha256(token) 十六进制。**不参与序列化输出到任何用户可见的地方**(§11.3)。
    #[serde(skip)]
    pub token_hash: String,
    /// 前 8 位,列表里给人识别。这是 token 唯一可以显示的部分。
    pub token_prefix: String,
    pub status: String,
    pub last_seen: Option<i64>,
    pub agent_version: Option<String>,
    pub singbox_version: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub hostname: Option<String>,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
    /// NULL = 不限流量。**不是 0** —— 0 会与「配额为零」歧义(§6.4)。
    pub nic_quota_bytes: Option<i64>,
    /// NULL = 无需重置;否则 1..31。
    pub nic_reset_day: Option<i64>,
    pub config_revision: i64,
    /// 与 `config_revision` **独立递增**(§4.1)。
    pub user_state_revision: i64,
    pub created_at: i64,
}

#[allow(dead_code)] // 全部是 TUI(§8.2 的两行式列表)要用的判定,见文件头注释
impl Agent {
    pub fn status(&self) -> AgentStatus {
        AgentStatus::parse(&self.status)
    }

    pub fn is_online(&self) -> bool {
        self.status() == AgentStatus::Online
    }

    /// 网卡流量是否设了配额。NULL = 不限,此时 TUI 不画进度条(§8.2)。
    pub fn has_nic_quota(&self) -> bool {
        self.nic_quota_bytes.filter(|q| *q > 0).is_some()
    }

    /// 网卡用量百分比。无配额时返回 `None`——
    /// 返回 `None` 而不是 0.0,是为了让调用方**必须**显式处理「不限流量」这一态,
    /// 否则很容易画出一个永远是 0% 的进度条。
    pub fn nic_used_percent(&self, cycle_rx: i64, cycle_tx: i64) -> Option<f64> {
        let q = self.nic_quota_bytes.filter(|q| *q > 0)?;
        Some(((cycle_rx + cycle_tx) as f64 / q as f64 * 100.0).min(100.0))
    }
}

/// 网卡流量累计(§6.4)。与用户代理流量是**两套不同的数字**,不要试图对齐(§7.2)。
///
/// 目前 supervisor 与 ingest 直接对 `agent_nic_traffic` 表读写,不经过这个结构;
/// 它是给 TUI(§8.2)准备的强类型视图。
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AgentNicTraffic {
    pub agent_id: i64,
    /// 网卡流量的 epoch(§5.2)。
    pub boot_id: Option<String>,
    pub last_rx: i64,
    pub last_tx: i64,
    pub cycle_rx: i64,
    pub cycle_tx: i64,
    pub cycle_start: i64,
    pub updated_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(nic_quota: Option<i64>) -> Agent {
        Agent {
            id: 1,
            name: "a".into(),
            token_hash: "h".into(),
            token_prefix: "pfx".into(),
            status: "online".into(),
            last_seen: None,
            agent_version: None,
            singbox_version: None,
            os: None,
            arch: None,
            hostname: None,
            ipv4: None,
            ipv6: None,
            nic_quota_bytes: nic_quota,
            nic_reset_day: None,
            config_revision: 0,
            user_state_revision: 0,
            created_at: 0,
        }
    }

    #[test]
    fn unknown_status_string_parses_as_never() {
        assert_eq!(AgentStatus::parse("garbage"), AgentStatus::Never);
        assert_eq!(AgentStatus::parse("online"), AgentStatus::Online);
        assert_eq!(AgentStatus::parse("offline"), AgentStatus::Offline);
    }

    /// 「不限流量」必须与「配额 0」区分开,否则 TUI 会画出一个假的 0% 进度条(§8.2)。
    #[test]
    fn no_quota_yields_none_not_zero_percent() {
        assert_eq!(agent(None).nic_used_percent(500, 500), None);
        assert!(!agent(None).has_nic_quota());
        assert_eq!(agent(Some(0)).nic_used_percent(500, 500), None, "配额 0 也按不限处理");
    }

    #[test]
    fn nic_percent_sums_rx_and_tx_and_clamps_at_100() {
        let a = agent(Some(1_000));
        assert_eq!(a.nic_used_percent(300, 200), Some(50.0), "口径是 RX+TX(§7.2)");
        assert_eq!(a.nic_used_percent(9_000, 9_000), Some(100.0), "超出应截到 100");
    }

    /// token_hash 绝不能出现在序列化结果里(§11.3 凭据处理)。
    #[test]
    fn token_hash_is_never_serialized() {
        let s = serde_json::to_string(&agent(None)).unwrap();
        assert!(!s.contains("token_hash"), "token_hash 不该被序列化: {s}");
        assert!(!s.contains("\"h\""), "token_hash 的值不该出现: {s}");
        assert!(s.contains("token_prefix"), "prefix 是可显示的那部分");
    }
}
