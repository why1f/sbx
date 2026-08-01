//! 节点模型。移植自旧项目 `src/model/node.rs`(DESIGN.md §9.1)。
//!
//! 保留:`Protocol` 枚举(9 变体含 Unknown)、`InboundNode`、`RelaySetting`、
//! `EditNodeRequest`(None = 保持原样)。
//! 新增:`agent_id` —— 节点从此归属于某台 agent。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Protocol {
    VlessReality,
    VlessWs,
    VmessWs,
    Shadowsocks,
    Trojan,
    Tuic,
    Anytls,
    Hysteria2,
    /// 库里存了不认识的协议名时的兜底,避免解码失败让整行读不出来。
    Unknown,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::VlessReality => "vless-reality",
            Protocol::VlessWs => "vless-ws",
            Protocol::VmessWs => "vmess-ws",
            Protocol::Shadowsocks => "shadowsocks",
            Protocol::Trojan => "trojan",
            Protocol::Tuic => "tuic",
            Protocol::Anytls => "anytls",
            Protocol::Hysteria2 => "hysteria2",
            Protocol::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "vless-reality" => Protocol::VlessReality,
            "vless-ws" => Protocol::VlessWs,
            "vmess-ws" => Protocol::VmessWs,
            "shadowsocks" => Protocol::Shadowsocks,
            "trojan" => Protocol::Trojan,
            "tuic" => Protocol::Tuic,
            "anytls" => Protocol::Anytls,
            "hysteria2" => Protocol::Hysteria2,
            _ => Protocol::Unknown,
        }
    }

    /// 全部**真实**协议(不含 Unknown)。订阅生成与 TUI 选择列表用。
    pub fn all() -> &'static [Protocol] {
        &[
            Protocol::VlessReality,
            Protocol::VlessWs,
            Protocol::VmessWs,
            Protocol::Shadowsocks,
            Protocol::Trojan,
            Protocol::Tuic,
            Protocol::Anytls,
            Protocol::Hysteria2,
        ]
    }
}

/// 一个代理 inbound 节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundNode {
    pub id: i64,
    /// 归属的 agent。**新增字段** —— 旧项目只有一台机器,不需要它。
    pub agent_id: i64,
    /// inbound tag。与 `stats.report` 里的 `tag` 是同一个东西(§4.3),
    /// 也是 `(user, tag)` 记账口径的那一半(§7.1)。
    pub tag: String,
    pub protocol: Protocol,
    pub listen_port: u16,
    /// server_name / path / port_reuse / ipv6 / relay 等,按协议不同取用。
    pub params: NodeParams,
}

/// 节点参数。既有**人填的**部分(server_name / path / relay),
/// 也有**建节点时自动生成的密钥材料**(reality 密钥对、自签证书、ss 密码)。
///
/// 两者放在同一个结构里是刻意的:它整体就是 `nodes.params_json` 那一列,
/// 拆成两张表只会让「建节点」变成一次需要事务的多表写入,而它们的生命周期完全一致
/// —— 节点没了,密钥也就没有意义了。
///
/// **这里面有凭据。** `private_key` / `key_pem` / `ss_password` 不得出现在
/// 日志、TUI、订阅页面里(§11.3)。订阅链接只用得到 `public_key` 和 `short_id`,
/// 那两个本来就是要发给客户端的。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub port_reuse: bool,
    #[serde(default)]
    pub ipv6: bool,
    #[serde(default, skip_serializing_if = "RelaySetting::is_empty")]
    pub relay: RelaySetting,

    // ── reality(vless-reality)──
    /// X25519 私钥,base64url-nopad。**下发给 agent,不进订阅。**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    /// 对应公钥,base64url-nopad。**只进订阅,不下发给 agent** ——
    /// sing-box 的 reality inbound 根本没有这个字段,填了会被拒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// 8 个十六进制字符。inbound 与客户端两边都要,必须一致。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<String>,

    // ── shadowsocks ──
    /// 加密方式。默认 `2022-blake3-aes-128-gcm`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ss_method: Option<String>,
    /// **服务端**密钥,base64(16 字节)。与每个用户各自的 password 是两回事:
    /// 2022 系列方法里,服务端密钥 + 用户密钥共同参与派生。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ss_password: Option<String>,

    // ── 需要 TLS 证书的协议(trojan / hysteria2 / tuic / anytls)──
    /// 自签证书 PEM 全文。
    ///
    /// 存 PEM 内容而不是**路径**:主控和 agent 不在同一台机器上,
    /// 路径在这边有意义、在那边没有。sing-box 的 `certificate` / `key` 字段
    /// 接受内联 PEM(`option/tls.go` 里是 `Listable[string]`,读的时候用 "\n" 拼起来)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_pem: Option<String>,
    /// 对应私钥 PEM。**凭据。**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_pem: Option<String>,
}

/// 中转设置:订阅导出时把节点地址换成中转机的地址。
///
/// 典型用法是落地机在墙外、另有一台线路更好的中转机做 TCP/UDP 转发,
/// 客户端连中转机、由它转到本机的 `listen_port`。
/// **转发本身由中转机自己实现**,本工具只负责让订阅指向它。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySetting {
    /// 中转机地址(IP 或域名)。空串表示不启用中转。
    #[serde(default)]
    pub host: String,
    /// 中转机端口。留空则沿用节点自身对外端口。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl RelaySetting {
    pub fn is_enabled(&self) -> bool {
        !self.host.trim().is_empty()
    }

    fn is_empty(&self) -> bool {
        !self.is_enabled() && self.port.is_none()
    }
}

/// 新建节点请求。TUI(§8)的节点表单会填它;CLI 目前走 `node_repo::add_node`
/// 的位置参数,还没切过来。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct AddNodeRequest {
    pub agent_id: i64,
    pub tag: String,
    pub protocol: Protocol,
    pub listen_port: u16,
    pub server_name: Option<String>,
    pub path: Option<String>,
    pub port_reuse: bool,
    pub ipv6: bool,
    pub relay: RelaySetting,
}

/// 编辑节点请求。字段为 `None` 表示"保持原样"。
///
/// 用结构体而不是长参数列表:可改项已经到了 8 个,
/// 再按位置传参极容易把 server_name 和 path 之类的同型参数搞反。
///
/// 与 `AddNodeRequest` 一样,消费者是还没写的 TUI(§8)。
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct EditNodeRequest {
    pub node_id: i64,
    pub listen_port: Option<u16>,
    pub server_name: Option<String>,
    pub path: Option<String>,
    pub port_reuse: Option<bool>,
    pub ipv6: Option<bool>,
    /// `None` = 不改中转设置;`Some` = 整组覆盖(`host` 为空即关闭中转)。
    pub relay: Option<RelaySetting>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trips_through_string() {
        for p in Protocol::all() {
            assert_eq!(Protocol::parse(p.as_str()), *p, "{p} 应能往返");
        }
    }

    /// 库里出现不认识的协议名时不能让整行读不出来。
    #[test]
    fn unknown_protocol_is_a_fallback_not_a_failure() {
        assert_eq!(Protocol::parse("wireguard-plus"), Protocol::Unknown);
        assert_eq!(Protocol::parse(""), Protocol::Unknown);
        assert!(!Protocol::all().contains(&Protocol::Unknown), "Unknown 不该出现在可选列表里");
    }

    #[test]
    fn all_covers_the_eight_real_protocols() {
        assert_eq!(Protocol::all().len(), 8, "§9.1 要求 8 协议全部保留");
    }

    #[test]
    fn relay_blank_host_is_disabled() {
        assert!(!RelaySetting::default().is_enabled());
        assert!(!RelaySetting { host: "   ".into(), port: Some(443) }.is_enabled());
        assert!(RelaySetting { host: "relay.example.com".into(), port: None }.is_enabled());
    }

    /// 未设置的可选字段不该出现在 `params_json` 里——
    /// 库里存的 JSON 越窄,以后加字段时的兼容面越小。
    #[test]
    fn node_params_omits_unset_fields() {
        let s = serde_json::to_string(&NodeParams::default()).unwrap();
        assert_eq!(s, "{\"port_reuse\":false,\"ipv6\":false}", "得到的是: {s}");
    }

    #[test]
    fn node_params_round_trip_preserves_relay() {
        let p = NodeParams {
            server_name: Some("www.example.com".into()),
            relay: RelaySetting { host: "r.example.com".into(), port: Some(8443) },
            ..Default::default()
        };
        let back: NodeParams = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back.relay.host, "r.example.com");
        assert_eq!(back.relay.port, Some(8443));
        assert_eq!(back.server_name.as_deref(), Some("www.example.com"));
    }
}
