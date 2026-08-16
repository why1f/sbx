//! 被控服务器(agent)模型。旧项目没有对应物,整个是新增的。
//!
//! 这里有一批 `#[allow(dead_code)]`。TUI(§8)读的是它自己的视图模型
//! (`tui/data.rs::AgentRow`,那边要 JOIN 网卡流量表并在内存里算网速),
//! 所以这层封装目前只有 daemon 侧在用,`AgentStatus` 的判定方法还没有调用点。
//! 它们留着是因为语义定义在这里最合适;真要删,该连着 `tui/data.rs` 一起想清楚
//! 「状态判定到底住在哪一层」,而不是因为编译器说没人用就顺手删掉。

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

/// 网卡记账口径:本周期算多少,由这一项决定(§6.4)。
///
/// **方向站在被控机的角度看**,与 `/proc/net/dev` 一致:
///   * 出站 = Transmit = TX = 机器**发出**去的字节。代理场景里这就是
///     「服务器 → 客户端」那一段,也就是客户端那边看到的下载。
///   * 入站 = Receive = RX = 机器**收进来**的字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NicAccountingMode {
    /// RX + TX；保持升级前的默认口径。
    #[default]
    Sum,
    /// 仅计算机器发出的 TX(服务器 → 客户端)。
    Outbound,
    /// 仅计算机器收到的 RX(客户端 → 服务器)。
    Inbound,
    /// 在完整周期累计 RX/TX 中取较大值。
    Max,
}

impl NicAccountingMode {
    pub fn all() -> &'static [Self] {
        &[Self::Sum, Self::Outbound, Self::Inbound, Self::Max]
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Outbound => "outbound",
            Self::Inbound => "inbound",
            Self::Max => "max",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Sum => "入出总计",
            Self::Outbound => "仅出站(机器发出)",
            Self::Inbound => "仅入站(机器收到)",
            Self::Max => "入出取大",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Sum => "总计",
            Self::Outbound => "出站",
            Self::Inbound => "入站",
            Self::Max => "取大",
        }
    }

    /// 未知值按原来的 RX+TX 口径处理。库可能被手工改过，也可能发生版本回滚。
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "outbound" => Self::Outbound,
            "inbound" => Self::Inbound,
            "max" => Self::Max,
            _ => Self::Sum,
        }
    }

    /// 从本周期完整的原始累计值投影记账用量。负值按 0，避免损坏的库产生负用量。
    pub fn account(self, rx: i64, tx: i64) -> i64 {
        let rx = rx.max(0);
        let tx = tx.max(0);
        match self {
            Self::Sum => rx.saturating_add(tx),
            Self::Outbound => tx,
            Self::Inbound => rx,
            Self::Max => rx.max(tx),
        }
    }

    /// 投影到 subscription-userinfo 的 (upload, download)。未计入的方向归零，
    /// 因而两项之和始终等于 `account`。相等时把取大值保留在出站/TX。
    pub fn project(self, rx: i64, tx: i64) -> (u64, u64) {
        let rx = rx.max(0) as u64;
        let tx = tx.max(0) as u64;
        match self {
            Self::Sum => (tx, rx),
            Self::Outbound => (tx, 0),
            Self::Inbound => (0, rx),
            Self::Max if tx >= rx => (tx, 0),
            Self::Max => (0, rx),
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
    /// 原始 cycle_rx/cycle_tx 的主控侧记账投影；默认 sum 保持旧行为。
    pub nic_accounting_mode: String,
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
        let used = NicAccountingMode::parse(&self.nic_accounting_mode).account(cycle_rx, cycle_tx);
        Some((used as f64 / q as f64 * 100.0).min(100.0))
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
            nic_accounting_mode: "sum".into(),
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
    fn nic_modes_use_complete_directional_totals() {
        let cases = [
            (NicAccountingMode::Sum, 300, 200, 500, (200, 300)),
            (NicAccountingMode::Outbound, 300, 200, 200, (200, 0)),
            (NicAccountingMode::Inbound, 300, 200, 300, (0, 300)),
            (NicAccountingMode::Max, 300, 200, 300, (0, 300)),
            (NicAccountingMode::Max, 200, 300, 300, (300, 0)),
            (NicAccountingMode::Max, 300, 300, 300, (300, 0)),
        ];
        for (mode, rx, tx, used, projected) in cases {
            assert_eq!(mode.account(rx, tx), used, "{mode:?}");
            assert_eq!(mode.project(rx, tx), projected, "{mode:?}");
            assert_eq!(projected.0 + projected.1, used as u64, "{mode:?}");
        }
        assert_eq!(NicAccountingMode::Sum.account(i64::MAX, i64::MAX), i64::MAX);
        assert_eq!(NicAccountingMode::Sum.account(-1, 7), 7);
    }

    #[test]
    fn nic_mode_keys_round_trip_and_unknown_falls_back_to_sum() {
        for mode in NicAccountingMode::all() {
            assert_eq!(NicAccountingMode::parse(mode.key()), *mode);
        }
        for bad in ["", "unknown", " SUM "] {
            assert_eq!(NicAccountingMode::parse(bad), NicAccountingMode::Sum);
        }
    }

    #[test]
    fn nic_percent_uses_selected_mode_and_clamps_at_100() {
        let mut a = agent(Some(1_000));
        assert_eq!(a.nic_used_percent(300, 200), Some(50.0), "默认口径是 RX+TX");
        a.nic_accounting_mode = "inbound".into();
        assert_eq!(a.nic_used_percent(300, 200), Some(30.0));
        a.nic_accounting_mode = "outbound".into();
        assert_eq!(a.nic_used_percent(300, 2_000), Some(100.0), "超出应截到 100");
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
