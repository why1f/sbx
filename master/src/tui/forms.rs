//! 各个表单的组装(DESIGN.md §8.1)。
//!
//! 这里只做**表单长什么样、填的东西怎么变成一个 `Action`**;
//! 真正写库在 `mod.rs::perform_inner`,渲染与按键在 `modal.rs`。
//! 三者分开是因为它们的改动理由完全不同:改文案不该碰 SQL,改按键不该碰校验。
//!
//! ## 协议决定字段
//!
//! `server_name` 只对 reality/trojan/tuic/anytls 有意义,`path` 只对两个 ws 协议有意义
//! —— 这两条与 `secrets::fill` 和 `service::build_inbound` 是同一份事实,
//! 所以下面两个函数是**唯一**的判定处,页面和表单都从这里取。
//! 让 shadowsocks 的表单上摆着一个填了不生效的 `path` 框,比不显示它更容易出错。

use chrono::{Datelike, NaiveDate, TimeZone};

use super::data::{AgentRow, NodeRow, UserRow};
use super::modal::{on, val, Action, Field, Form, Modal, NodeDraft, PickItem, Picker};
use crate::model::node::Protocol;

/// 需要 TLS SNI 的协议(inbound 的 `tls.server_name` 生效)。
///
/// **hysteria2 不在其中**:它的 inbound 不带 `server_name`(官方示例亦无),
/// 客户端的 sni 由订阅链接决定 —— 与 `secrets::fill` 里的注释是同一件事。
pub fn uses_sni(p: Protocol) -> bool {
    matches!(p, Protocol::VlessReality | Protocol::Trojan | Protocol::Tuic | Protocol::Anytls)
}

/// 需要 WebSocket path 的协议。
pub fn uses_path(p: Protocol) -> bool {
    matches!(p, Protocol::VlessWs | Protocol::VmessWs)
}

/// 表单底部随协议变化的一句说明。写的是**填完之后会发生什么**,
/// 而不是字段的字面意思 —— 后者标签里已经有了。
fn protocol_note(p: Protocol) -> &'static str {
    match p {
        Protocol::VlessReality => {
            "reality:密钥对与 short_id 建节点时自动生成;server_name 同时是握手目标,要挑一个真实可达的大站"
        }
        Protocol::VlessWs | Protocol::VmessWs => {
            "ws:后端不启 TLS,建议前面挂 nginx/caddy 终结 TLS 再反代到这个端口"
        }
        Protocol::Shadowsocks => "shadowsocks-2022:服务端密钥自动生成;没有 SNI / path 可填",
        Protocol::Hysteria2 => "hysteria2:UDP/QUIC;证书自动自签,客户端订阅带 insecure=1",
        Protocol::Trojan | Protocol::Tuic | Protocol::Anytls => {
            "自签证书在建节点时生成,CN 取当时的 server_name;客户端订阅自动带 allowInsecure"
        }
        Protocol::Unknown => "",
    }
}

fn protocol_names() -> Vec<String> {
    Protocol::all().iter().map(|p| p.as_str().to_string()).collect()
}

// ─────────────────────────── agent ───────────────────────────

/// 新增被控服务器。
///
/// 多问一个「主控地址」是为了能直接吐出一条**可以整条复制**的接入命令:
/// 主控只知道自己 `listen` 在 `0.0.0.0:18443`,不知道被控机该往哪个地址回连
/// (公网 IP、内网 IP、还是一个域名),这件事只有人知道。
pub fn agent_add(default_host: &str) -> Modal {
    Modal::Form(Form::new(
        "新增被控服务器",
        vec![
            Field::text("name", "名称", "", "例如 tokyo-1;只是给人看的标识,须唯一"),
            Field::text(
                "host",
                "主控地址",
                default_host,
                "被控机回连主控用的 IP 或域名,不含端口。留空则命令里给占位符",
            ),
        ],
        Box::new(|f| {
            let name = val(f, "name");
            if name.is_empty() {
                return Err("名称不能为空".into());
            }
            Ok(Action::AddAgent { name, host: val(f, "host") })
        }),
    ))
}

// ─────────────────────────── 节点 ───────────────────────────

/// 新增节点。所属 agent 与协议都是 ←/→ 选的 —— 手打协议名是上一版最常见的错。
pub fn node_add(agents: &[AgentRow], preselect: usize) -> Modal {
    let names: Vec<String> = agents.iter().map(|a| format!("#{} {}", a.id, a.name)).collect();
    let ids: Vec<i64> = agents.iter().map(|a| a.id).collect();

    let form = Form::new(
        "新增节点",
        vec![
            Field::select("agent", "所属服务器", names, preselect, "在哪台被控机上建这个 inbound"),
            Field::text("tag", "Tag", "", "同一台机器内唯一;也是 (用户, tag) 记账口径的一半(§7.1)"),
            Field::select("proto", "协议", protocol_names(), 0, "八选一,←/→ 切换"),
            Field::text("port", "监听端口", "443", "1-65535;同一台机器上别和已有服务撞"),
            Field::text("sni", "server_name", "", "留空按协议取默认(reality → www.apple.com)"),
            Field::text("path", "path", "", "留空取 /vless 或 /vmess"),
            Field::toggle("ipv6", "订阅优先 IPv6", false, "订阅导出这个节点时优先用 agent 的 IPv6 地址"),
            Field::text("relay_host", "中转地址", "", "留空 = 不中转。只改订阅里导出的落点,不动 inbound"),
            Field::text("relay_port", "中转端口", "", "留空则沿用上面的监听端口"),
        ],
        Box::new(move |f| {
            let i = f.iter().find(|x| x.key == "agent").map(|x| x.index()).unwrap_or(0);
            let agent_id = *ids.get(i).ok_or("先在「服务管理」页加一台被控服务器")?;
            let mut draft = draft_from(f, agent_id)?;
            draft.tag = val(f, "tag");
            if draft.tag.is_empty() {
                return Err("Tag 不能为空".into());
            }
            draft.protocol = Protocol::parse(&val(f, "proto"));
            Ok(Action::AddNode(draft))
        }),
    )
    .visible_when(Box::new(|fields, f| field_applies(Protocol::parse(&val(fields, "proto")), f.key)))
    .with_note(Box::new(|fields| {
        vec![protocol_note(Protocol::parse(&val(fields, "proto"))).into()]
    }));

    Modal::Form(form)
}

/// 编辑节点。
///
/// tag / 协议 / 所属机器**不在这里改**,理由在 `node_repo::update_node` 的文档里:
/// 改 tag 会把历史流量和新流量记到两个互不相认的账上;改协议等于换一整套密钥材料。
/// 所以它们放在标题行里只读显示,要改就删掉重建。
pub fn node_edit(n: &NodeRow) -> Modal {
    let proto = Protocol::parse(&n.protocol);
    let id = n.id;
    let agent_id = n.agent_id;
    let relay_port = n.params.relay.port.map(|p| p.to_string()).unwrap_or_default();

    let form = Form::new(
        "编辑节点",
        vec![
            Field::text("port", "监听端口", &n.listen_port.to_string(), "1-65535"),
            Field::text(
                "sni",
                "server_name",
                n.params.server_name.as_deref().unwrap_or(""),
                "改它不会重签证书:自签证书的 CN 是建节点时定的,客户端本来就走 insecure",
            ),
            Field::text("path", "path", n.params.path.as_deref().unwrap_or(""), "WebSocket 路径"),
            Field::toggle("ipv6", "订阅优先 IPv6", n.params.ipv6, "订阅导出这个节点时优先用 agent 的 IPv6"),
            Field::text("relay_host", "中转地址", &n.params.relay.host, "留空 = 不中转"),
            Field::text("relay_port", "中转端口", &relay_port, "留空则沿用监听端口"),
        ],
        Box::new(move |f| {
            let mut draft = draft_from(f, agent_id)?;
            draft.protocol = proto;
            Ok(Action::EditNode { id, draft })
        }),
    )
    .head(format!(
        "#{} {} · {} · 在 {} 上(tag / 协议不可改,要改请删掉重建)",
        n.id, n.tag, n.protocol, n.agent_name
    ))
    .visible_when(Box::new(move |_, f| field_applies(proto, f.key)))
    .with_note(Box::new(move |_| vec![protocol_note(proto).into()]));

    Modal::Form(form)
}

/// 某个字段在当前协议下是否有意义。新增与编辑共用一份判定。
fn field_applies(proto: Protocol, key: &str) -> bool {
    match key {
        "sni" => uses_sni(proto),
        "path" => uses_path(proto),
        _ => true,
    }
}

/// 表单 → `NodeDraft` 的公共部分(端口、SNI、path、IPv6、中转)。
fn draft_from(f: &[Field], agent_id: i64) -> Result<NodeDraft, String> {
    let port: u16 = val(f, "port").parse().map_err(|_| "监听端口需要是 1-65535 的数字")?;
    if port == 0 {
        return Err("监听端口需要是 1-65535 的数字".into());
    }
    let relay_host = val(f, "relay_host");
    let relay_port_raw = val(f, "relay_port");
    let relay_port = if relay_port_raw.is_empty() {
        None
    } else {
        Some(relay_port_raw.parse::<u16>().map_err(|_| "中转端口需要是 1-65535 的数字")?)
    };
    // 只填端口不填地址是**填错了**,不是「用默认地址中转」——
    // 静默忽略的话订阅里的落点会和人以为的不一样,而这种错只有客户端连不上才发现。
    if relay_host.is_empty() && relay_port.is_some() {
        return Err("填了中转端口就要填中转地址(留空两项 = 不中转)".into());
    }

    Ok(NodeDraft {
        agent_id,
        tag: String::new(),
        protocol: Protocol::Unknown,
        port,
        server_name: none_if_empty(val(f, "sni")),
        path: none_if_empty(val(f, "path")),
        ipv6: on(f, "ipv6"),
        relay_host,
        relay_port,
    })
}

fn none_if_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ─────────────────────────── 用户 ───────────────────────────

pub fn user_add() -> Modal {
    Modal::Form(Form::new(
        "新增用户",
        vec![
            Field::text("name", "名称", "", "唯一;也是 sing-box inbound 里的用户名"),
            Field::text("quota", "配额 GB", "0", "0 = 不限流量"),
        ],
        Box::new(|f| {
            let name = val(f, "name");
            if name.is_empty() {
                return Err("名称不能为空".into());
            }
            Ok(Action::AddUser { name, quota_gb: val(f, "quota") })
        }),
    ))
}

/// 编辑用户的计费属性。名字不可改 —— 它是 inbound 里的用户标识,
/// 改掉等于换一个人,历史流量还挂在旧名字上。
pub fn user_edit(u: &UserRow) -> Modal {
    let id = u.id;
    let name = u.name.clone();
    let quota_gb = if u.quota_bytes > 0 {
        format!("{:.0}", u.quota_bytes as f64 / 1_073_741_824.0)
    } else {
        "0".into()
    };
    let expire = u.expire_at.map(fmt_date).unwrap_or_default();
    let reset = u.reset_day.map(|d| d.to_string()).unwrap_or_default();

    Modal::Form(
        Form::new(
            "编辑用户",
            vec![
                Field::text("quota", "配额 GB", &quota_gb, "0 = 不限流量"),
                Field::text(
                    "mult",
                    "计费倍率",
                    &format!("{:.1}", u.traffic_multiplier),
                    "计费用量 = (上行 + 下行) × 倍率。1.0 = 按实际,2.0 = 双倍",
                ),
                Field::text("expire", "到期", &expire, "YYYY-MM-DD;留空 = 永久。当天 23:59:59 到期"),
                Field::text("reset", "重置日", &reset, "1-31,每月这天清零本周期用量;留空 = 不重置"),
            ],
            Box::new(move |f| {
                Ok(Action::EditUser {
                    id,
                    name: name.clone(),
                    quota_gb: val(f, "quota"),
                    multiplier: val(f, "mult"),
                    expire: val(f, "expire"),
                    reset_day: val(f, "reset"),
                })
            }),
        )
        .head(format!(
            "#{} {}(名字不可改:它是 inbound 里的用户标识,改了历史流量会对不上)",
            u.id, u.name
        ))
        .with_note(Box::new(|_| {
            vec![
                "配额/到期只被 §6.3 的巡检读,不进 sing-box 配置 —— 改它不会重建 box。".into(),
                "把配额调大之后,原先因超额被系统停用的用户会在下一次巡检时自动放出来。".into(),
            ]
        })),
    )
}

/// 给用户勾选节点。
///
/// 这里是**多选**而不是「输入一个 node_id」:后者要求人先去另一页把编号记下来,
/// 而且一次只能加一个,取消分配还得走另一条路。
pub fn assign_nodes(u: &UserRow, nodes: &[NodeRow]) -> Modal {
    let items: Vec<PickItem> = nodes
        .iter()
        .map(|n| PickItem {
            id: n.id,
            label: n.tag.clone(),
            // 同名 tag 在不同机器上是合法的,所以备注里必须带上机器名,
            // 否则两行长得一模一样,只能靠顺序猜。
            note: format!("{} · :{} · 在 {} 上", n.protocol, n.listen_port, n.agent_name),
            checked: u.node_ids.contains(&n.id),
        })
        .collect();

    let user_id = u.id;
    let user = u.name.clone();
    Modal::Picker(Picker::new(
        "分配节点",
        format!("用户 {}(勾上的就是这个用户能用的节点)", u.name),
        items,
        Box::new(move |ids| Action::SetUserNodes {
            user_id,
            user: user.clone(),
            node_ids: ids.to_vec(),
        }),
    ))
}

// ─────────────────────────── 日期 ───────────────────────────

/// `YYYY-MM-DD` → 那一天**结束时**的时间戳(本地时区 23:59:59)。
///
/// 取当天末尾而不是开头:§6.3 的判定是 `expire_at > now` 才算有效,
/// 用 00:00:00 的话「到期 12-31」在 12 月 31 日当天就已经断了 ——
/// 而所有人对「到期日」的理解都是「这一天还能用」。
pub fn parse_expire(s: &str) -> Result<Option<i64>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| format!("到期日要写成 YYYY-MM-DD(收到:{s})"))?;
    let end = d.and_hms_opt(23, 59, 59).ok_or("日期不合法")?;
    match chrono::Local.from_local_datetime(&end).earliest() {
        Some(dt) => Ok(Some(dt.timestamp())),
        // 夏令时切换那一小时可能不存在。退一步用 UTC,总比让人填不进去好。
        None => Ok(Some(end.and_utc().timestamp())),
    }
}

/// 时间戳 → `YYYY-MM-DD`(本地时区)。与 `parse_expire` 往返一致。
pub fn fmt_date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".into())
}

pub fn parse_reset_day(s: &str) -> Result<Option<i64>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    match s.parse::<i64>() {
        Ok(d) if (1..=31).contains(&d) => Ok(Some(d)),
        _ => Err(format!("重置日要是 1-31 之间的数字,或留空表示不重置(收到:{s})")),
    }
}

/// 「距离到期还有几天」。负数表示已经过期。给用户页的到期列用。
pub fn days_until(ts: i64, now: i64) -> i64 {
    (ts - now).div_euclid(86_400)
}

/// 本月的重置日文案。`32` 之类的越界值在库里是可能的(手改过),不该显示「每月 32 日」。
pub fn reset_day_label(day: Option<i64>) -> String {
    match day {
        Some(d) if (1..=31).contains(&d) => format!("{d} 日"),
        _ => "—".into(),
    }
}

/// 今天是几号 —— 用来在用户页把「今天正好是重置日」标出来。
pub fn today_day(now: i64) -> u32 {
    chrono::DateTime::from_timestamp(now, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).day())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SNI / path 的适用范围必须和 `secrets::fill`、`service::build_inbound` 对得上。
    /// 对不上的表现是:表单让人填了一个下发时被丢掉的值,或者该填的框没出现,
    /// 提交后报「缺少 server_name」。
    #[test]
    fn sni_and_path_apply_to_the_right_protocols() {
        for p in [Protocol::VlessReality, Protocol::Trojan, Protocol::Tuic, Protocol::Anytls] {
            assert!(uses_sni(p), "{p} 需要 server_name");
            assert!(!uses_path(p), "{p} 不该有 path");
        }
        // hysteria2 的 inbound 没有 server_name(secrets::fill 里同样特判)。
        assert!(!uses_sni(Protocol::Hysteria2));
        assert!(!uses_sni(Protocol::Shadowsocks));
        for p in [Protocol::VlessWs, Protocol::VmessWs] {
            assert!(uses_path(p), "{p} 需要 path");
            assert!(!uses_sni(p), "ws 系不走 TLS,不该有 server_name");
        }
    }

    /// 八个协议每个都要有一句说明。漏一个的表现是表单底下空一行,
    /// 而那一行恰恰是新手唯一的提示。
    #[test]
    fn every_protocol_has_a_note() {
        for p in Protocol::all() {
            assert!(!protocol_note(*p).is_empty(), "{p} 没有说明文字");
        }
    }

    /// 到期日必须落在**当天末尾**:用 00:00:00 的话,「到期 12-31」在 31 号当天
    /// 就已经被 §6.3 判成过期,而所有人的理解都是「这一天还能用」。
    #[test]
    fn expire_lands_at_the_end_of_the_day() {
        let ts = parse_expire("2026-12-31").unwrap().unwrap();
        let local = chrono::DateTime::from_timestamp(ts, 0).unwrap().with_timezone(&chrono::Local);
        assert_eq!(local.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-12-31 23:59:59");
        // 当天中午还没到期。
        let noon = parse_expire("2026-12-31").unwrap().unwrap() - 12 * 3600;
        assert!(ts > noon);
    }

    #[test]
    fn expire_round_trips_through_the_form() {
        let ts = parse_expire("2026-03-05").unwrap().unwrap();
        assert_eq!(fmt_date(ts), "2026-03-05");
    }

    #[test]
    fn expire_accepts_empty_and_rejects_garbage() {
        assert_eq!(parse_expire("").unwrap(), None);
        assert_eq!(parse_expire("   ").unwrap(), None);
        assert!(parse_expire("2026/12/31").is_err());
        assert!(parse_expire("2026-13-01").is_err());
        assert!(parse_expire("明年").is_err());
    }

    #[test]
    fn reset_day_bounds() {
        assert_eq!(parse_reset_day("").unwrap(), None);
        assert_eq!(parse_reset_day("1").unwrap(), Some(1));
        assert_eq!(parse_reset_day("31").unwrap(), Some(31));
        assert!(parse_reset_day("0").is_err());
        assert!(parse_reset_day("32").is_err());
        assert!(parse_reset_day("每月").is_err());
        // 库里已经存了越界值时,显示不能是「每月 32 日」。
        assert_eq!(reset_day_label(Some(32)), "—");
        assert_eq!(reset_day_label(Some(22)), "22 日");
        assert_eq!(reset_day_label(None), "—");
    }

    #[test]
    fn days_until_counts_whole_days_both_ways() {
        let now = 1_700_000_000;
        assert_eq!(days_until(now + 86_400 * 3 + 5, now), 3);
        assert_eq!(days_until(now, now), 0);
        assert_eq!(days_until(now - 1, now), -1, "已过期要给负数");
        assert_eq!(days_until(now - 86_400 * 2, now), -2);
    }

    /// 中转只填端口不填地址是填错了,必须挡下来 ——
    /// 静默忽略的话订阅里的落点和人以为的不一样,只有客户端连不上才会发现。
    #[test]
    fn relay_port_without_a_host_is_rejected() {
        let f = vec![
            Field::text("port", "", "443", ""),
            Field::text("relay_host", "", "", ""),
            Field::text("relay_port", "", "12345", ""),
        ];
        let err = draft_from(&f, 1).unwrap_err();
        assert!(err.contains("中转地址"), "{err}");

        // 两项都留空 = 不中转,正常通过。
        let f = vec![
            Field::text("port", "", "443", ""),
            Field::text("relay_host", "", "", ""),
            Field::text("relay_port", "", "", ""),
        ];
        let d = draft_from(&f, 1).unwrap();
        assert!(d.relay_host.is_empty() && d.relay_port.is_none());
    }

    #[test]
    fn port_must_be_a_valid_number() {
        for bad in ["", "0", "70000", "四四三"] {
            let f = vec![Field::text("port", "", bad, "")];
            assert!(draft_from(&f, 1).is_err(), "端口 {bad:?} 应当被拒");
        }
    }
}
