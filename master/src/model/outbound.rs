//! 出站地址族策略(每台 agent 一个)。
//!
//! ## 为什么不是 `domain_strategy`
//!
//! sing-box 的 dial 字段 `domain_strategy` 从 1.12.0 起弃用、**1.14.0 已移除**,
//! 而 agent 内嵌的正是 v1.14.0-beta.3(§0.3)。写它的后果不是「不生效」,
//! 是 `box.New()` 直接失败 —— 整台机器的代理一起停,而错误信息只说
//! 「配置无效」,离真正的原因很远。
//!
//! 替代品是 1.12.0 引入的 `route.default_domain_resolver`,它要**指向一个
//! DNS server tag**,所以写策略时得顺带保证 `dns.servers` 里有一个可引用的项。
//!
//! ## 为什么按 agent 存
//!
//! 这是「这台落地机怎么解析域名」的问题,天然是每台一份:
//! 一台只有 IPv4 出口的机器要 `ipv4_only`,另一台双栈的可以 `prefer_ipv6`。
//! 放在全局配置里就没法表达这个差别。

use serde::{Deserialize, Serialize};

/// `dns.servers` 里没有可引用项时,我们自己插的那个的 tag。
const LOCAL_DNS_TAG: &str = "local";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum OutboundStrategy {
    /// 不写 `default_domain_resolver`,交给 sing-box 的默认行为
    /// (系统解析器返回什么就用什么)。
    #[default]
    Auto,
    PreferIpv4,
    PreferIpv6,
    Ipv4Only,
    Ipv6Only,
}

impl OutboundStrategy {
    /// sing-box 侧的取值。`Auto` 没有对应值 —— 它的含义就是「这一项不写」。
    pub fn as_singbox(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::PreferIpv4 => Some("prefer_ipv4"),
            Self::PreferIpv6 => Some("prefer_ipv6"),
            Self::Ipv4Only => Some("ipv4_only"),
            Self::Ipv6Only => Some("ipv6_only"),
        }
    }

    /// 存库用的短名。**不要**直接存 `as_singbox` 的值:那样 `Auto` 就没法表示了
    /// (它对应 `None`),而「没设过」和「设成自动」在界面上是同一件事。
    pub fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::PreferIpv4 => "prefer4",
            Self::PreferIpv6 => "prefer6",
            Self::Ipv4Only => "v4only",
            Self::Ipv6Only => "v6only",
        }
    }

    /// 表格里那一列用的短名 —— 完整说法(「自动(跟随系统解析)」)有 14 列宽,
    /// 摆进列表会把别的列挤没。
    pub fn short(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::PreferIpv4 => "优先v4",
            Self::PreferIpv6 => "优先v6",
            Self::Ipv4Only => "仅 v4",
            Self::Ipv6Only => "仅 v6",
        }
    }

    /// 界面上的说法。
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动(跟随系统解析)",
            Self::PreferIpv4 => "优先 IPv4",
            Self::PreferIpv6 => "优先 IPv6",
            Self::Ipv4Only => "仅 IPv4",
            Self::Ipv6Only => "仅 IPv6",
        }
    }

    /// 认不出来一律当 `Auto`。库里可能是手改的,或者以后新增了取值又回滚了版本 ——
    /// 那时**退回默认行为**远比让整台机器的配置组装失败要好。
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "prefer4" | "prefer_ipv4" => Self::PreferIpv4,
            "prefer6" | "prefer_ipv6" => Self::PreferIpv6,
            "v4only" | "ipv4_only" => Self::Ipv4Only,
            "v6only" | "ipv6_only" => Self::Ipv6Only,
            _ => Self::Auto,
        }
    }

    /// 界面上按 ←/→ 循环的顺序。
    pub fn all() -> &'static [Self] {
        &[Self::Auto, Self::PreferIpv4, Self::PreferIpv6, Self::Ipv4Only, Self::Ipv6Only]
    }
}

/// 把策略写进一份已经组好的 sing-box 配置。
///
/// `Auto` 什么都不写(连 `route` 都不建),其余会:
///   1. 保证 `dns.servers` 里有一个带 tag 的项 —— `default_domain_resolver.server`
///      必须指向一个存在的 tag,指不到的话 sing-box 起不来;
///   2. 写 `route.default_domain_resolver = { server, strategy }`。
///
/// **不碰已有的 `dns.servers`**:那里面可能是别处配好的上游,这里只借一个 tag 用。
///
/// **自定义配置已经写了 `default_domain_resolver` 时让位。** 这两个功能写的是
/// 同一个字段 —— `[o]` 本质上就是自定义配置的一个预设。无条件 `insert` 会静默
/// 盖掉人手写的那一份,而界面上 `[o]` 还显示着一个看上去生效的值 ——
/// 那是最难查的一类不一致。现在让位,并由 `has_custom_resolver` 告知界面。
pub fn apply(cfg: &mut serde_json::Value, strategy: OutboundStrategy) {
    let Some(value) = strategy.as_singbox() else {
        return;
    };
    if has_custom_resolver(cfg) {
        return;
    }
    let tag = ensure_dns_server(cfg);
    if let Some(root) = cfg.as_object_mut() {
        let route = root
            .entry("route")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(route) = route.as_object_mut() {
            route.insert(
                "default_domain_resolver".into(),
                serde_json::json!({ "server": tag, "strategy": value }),
            );
        }
    }
}

/// 这份配置里的 `route.default_domain_resolver` 是不是已经有人写过了。
///
/// 给两个地方用:`apply` 靠它让位,TUI 靠它把 `[o]` 那一项显示成
/// 「由自定义配置接管」而不是继续显示一个不生效的值。
///
/// **两处必须用同一个判据**。各写一份的话,总会漂成「界面说接管了、
/// 实际没接管」或反过来。
pub fn has_custom_resolver(cfg: &serde_json::Value) -> bool {
    cfg.get("route").and_then(|r| r.get("default_domain_resolver")).is_some()
}

/// 返回一个可供引用的 DNS server tag,没有就插一个。
///
/// 插的是 `{ "type": "local", "tag": "local" }` —— 1.12.0 起的新式写法。
/// 旧的 `{ "address": "local" }` 形式 1.14.0 已移除,写了同样起不来。
fn ensure_dns_server(cfg: &mut serde_json::Value) -> String {
    let Some(root) = cfg.as_object_mut() else {
        return LOCAL_DNS_TAG.to_string();
    };
    let dns =
        root.entry("dns").or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(dns) = dns.as_object_mut() else {
        return LOCAL_DNS_TAG.to_string();
    };
    let servers = dns.entry("servers").or_insert_with(|| serde_json::Value::Array(vec![]));
    let Some(servers) = servers.as_array_mut() else {
        return LOCAL_DNS_TAG.to_string();
    };
    if let Some(tag) = servers
        .iter()
        .filter_map(|s| s.get("tag").and_then(serde_json::Value::as_str))
        .find(|t| !t.trim().is_empty())
    {
        return tag.to_string();
    }
    servers.push(serde_json::json!({ "type": "local", "tag": LOCAL_DNS_TAG }));
    LOCAL_DNS_TAG.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **绝不能写 `domain_strategy`。** 那个字段 1.14.0 已移除,而 agent 内嵌的
    /// 就是 1.14.0-beta.3 —— 写了不是「不生效」,是 `box.New()` 直接失败,
    /// 整台机器的代理一起停,错误信息只说「配置无效」。
    #[test]
    fn never_writes_the_removed_domain_strategy_field() {
        for s in OutboundStrategy::all() {
            let mut cfg = serde_json::json!({ "inbounds": [], "outbounds": [] });
            apply(&mut cfg, *s);
            let dumped = cfg.to_string();
            assert!(!dumped.contains("domain_strategy"), "{s:?} 写出了 domain_strategy:{dumped}");
        }
    }

    /// Auto = 什么都不写。留一个空的 `route` 壳子也不行 ——
    /// 那会让「没设过」和「设过又改回来」在配置上长得不一样,徒增 diff。
    #[test]
    fn auto_writes_nothing_at_all() {
        let mut cfg = serde_json::json!({ "inbounds": [] });
        apply(&mut cfg, OutboundStrategy::Auto);
        assert_eq!(cfg, serde_json::json!({ "inbounds": [] }));
    }

    /// 非 Auto 要同时写出 resolver 和它指向的那个 DNS server。
    /// 少了后者 sing-box 起不来(server tag 指不到)。
    #[test]
    fn a_strategy_also_provides_the_server_it_points_at() {
        let mut cfg = serde_json::json!({ "inbounds": [] });
        apply(&mut cfg, OutboundStrategy::Ipv6Only);

        let r = &cfg["route"]["default_domain_resolver"];
        assert_eq!(r["strategy"], "ipv6_only");
        let tag = r["server"].as_str().unwrap();
        let servers = cfg["dns"]["servers"].as_array().unwrap();
        assert!(servers.iter().any(|s| s["tag"] == tag), "resolver 指向的 tag 必须真的存在:{cfg}");
        assert_eq!(servers[0]["type"], "local", "要用 1.12+ 的新式写法");
    }

    /// 已有的 DNS 上游**不改写**,只借它的 tag。
    #[test]
    fn an_existing_dns_server_is_reused_not_replaced() {
        let mut cfg = serde_json::json!({
            "dns": { "servers": [{ "type": "udp", "server": "1.1.1.1", "tag": "cf" }] }
        });
        apply(&mut cfg, OutboundStrategy::PreferIpv4);
        assert_eq!(cfg["route"]["default_domain_resolver"]["server"], "cf");
        let servers = cfg["dns"]["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1, "不该插入多余的 server:{cfg}");
        assert_eq!(servers[0]["server"], "1.1.1.1", "原来的上游不该被动");
    }

    /// 存的短名要能往返;认不出来的一律退回 Auto,而不是让组装失败。
    #[test]
    fn keys_round_trip_and_garbage_falls_back_to_auto() {
        for s in OutboundStrategy::all() {
            assert_eq!(OutboundStrategy::parse(s.key()), *s, "{s:?} 该能往返");
        }
        // sing-box 侧的写法也认(万一有人手改成那个)。
        assert_eq!(OutboundStrategy::parse("ipv4_only"), OutboundStrategy::Ipv4Only);
        for bad in ["", "  ", "ipv5", "yes"] {
            assert_eq!(OutboundStrategy::parse(bad), OutboundStrategy::Auto, "{bad} 该退回 Auto");
        }
    }
}
