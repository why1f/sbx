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
/// 新增被控服务器。
///
/// **不问主控地址** —— 那不用人告诉:配了订阅域名就用域名(TLS 也才说得通),
/// 否则自动探本机出口地址(`install::resolve_host`)。探错了去「设置」页
/// 填订阅对外地址覆盖,比在每次新增时重打一遍强。
pub fn agent_add() -> Modal {
    Modal::Form(
        Form::new(
            "新增被控服务器",
            vec![
                Field::text("name", "名称 *必填", ""),
                Field::text("quota", "网卡月配额 GB (0 = 不限)", "0"),
                Field::select(
                    "nic_mode",
                    "网卡记账口径 (←/→ 切换)",
                    crate::model::agent::NicAccountingMode::all()
                        .iter()
                        .map(|m| m.label().to_string())
                        .collect(),
                    0,
                ),
                Field::text("reset", "配额重置日 (1-31,留空 = 不重置)", ""),
                // **必须放在最后。** `the_add_agent_form_defaults_to_sum_and_cycles_
                // through_every_mode` 靠按两次 Tab 走到 nic_mode;插在它前面会静默
                // 改掉那条测试的目标字段,失败信息还会去怪记账口径。
                Field::text("tz", "重置时区 (留空 = 跟随 agent 上报)", ""),
            ],
            Box::new(|f| {
                let name = val(f, "name");
                if name.is_empty() {
                    return Err("名称不能为空".into());
                }
                let gb: f64 = val(f, "quota").parse().map_err(|_| "配额要是一个数字(0 = 不限)")?;
                if gb < 0.0 {
                    return Err("配额不能是负数".into());
                }
                Ok(Action::AddAgent {
                    name,
                    quota_bytes: if gb > 0.0 { Some((gb * 1_073_741_824.0) as i64) } else { None },
                    reset_day: parse_reset_day(&val(f, "reset"))?,
                    nic_accounting_mode: crate::model::agent::NicAccountingMode::all()
                        .get(f.iter().find(|x| x.key == "nic_mode").map(|x| x.index()).unwrap_or(0))
                        .copied()
                        .unwrap_or_default(),
                    nic_reset_offset_secs: parse_nic_offset(&val(f, "tz"))?,
                })
            }),
        )
        .with_note(Box::new(|_| {
            vec![
                "名称只是给人看的标识(例 tokyo-1),须唯一。".into(),
                "网卡配额按所选口径读取这台机器的原始进出字节(§6.4),不是用户计费用量;".into(),
                "出站 = 机器发出(服务器→客户端,即客户端那边的下载),入站 = 机器收到;".into(),
                "原始两个方向一直分开记,换口径只重算显示,不清零也不改历史。".into(),
                "重置时区决定每月哪一刻翻月:厂商按机房当地零点结算。写 UTC-07:00 这种形式,".into(),
                "留空则跟随 agent 上报的本机偏移 —— 新机器接入即对齐,不必现在就填。".into(),
                "它只影响界面上的进度条与告警,不会限制 agent 转发流量。".into(),
                "确定之后会给出一条填好 token 的接入命令,复制到被控机上跑即可。".into(),
            ]
        })),
    )
}

// ─────────────────────────── 节点 ───────────────────────────

/// 新增节点。所属 agent 与协议都是 ←/→ 选的 —— 手打协议名是上一版最常见的错。
pub fn node_add(agents: &[AgentRow], preselect: usize) -> Modal {
    let names: Vec<String> = agents.iter().map(|a| format!("#{} {}", a.id, a.name)).collect();
    let ids: Vec<i64> = agents.iter().map(|a| a.id).collect();

    let form = Form::new(
        "新增节点",
        vec![
            Field::select("agent", "所属服务器 (←/→ 切换)", names, preselect),
            Field::text("tag", "Tag *必填", ""),
            Field::select("proto", "协议 (←/→ 切换)", protocol_names(), 0),
            Field::text("port", "监听端口 *必填", "443"),
            Field::text("sni", "server_name (SNI,留空取默认)", ""),
            Field::text("path", "path (留空取默认)", ""),
            Field::toggle("ipv6", "订阅优先 IPv6 (空格切换)", false),
            Field::text("relay_host", "中转地址 (例 1.2.3.4 / relay.com)", ""),
            Field::text("relay_port", "中转端口 (例 12345)", ""),
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
        let mut v = vec![TAG_NOTE.to_string()];
        v.push(protocol_note(Protocol::parse(&val(fields, "proto"))).into());
        v.push(RELAY_NOTE.into());
        v
    }));

    Modal::Form(form)
}

/// Tag 的含义装不进标签的括号里,但它是这张表单里最容易随手填错的一项。
const TAG_NOTE: &str = "Tag:同一台机器内唯一,建好之后不能改 —— 它是 (用户, tag) 记账口径的一半(§7.1)。";
/// 中转的语义同理 —— 三种取值组合各是什么意思,一行括号写不下。
const RELAY_NOTE: &str = "中转:地址留空 = 不启用;只填地址则沿用监听端口。只改订阅导出的落点,不动 inbound。";

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
            Field::text("port", "监听端口 *必填", &n.listen_port.to_string()),
            Field::text("sni", "server_name (SNI)", n.params.server_name.as_deref().unwrap_or("")),
            Field::text("path", "path", n.params.path.as_deref().unwrap_or("")),
            Field::toggle("ipv6", "订阅优先 IPv6 (空格切换)", n.params.ipv6),
            Field::text("relay_host", "中转地址 (例 1.2.3.4 / relay.com)", &n.params.relay.host),
            Field::text("relay_port", "中转端口 (例 12345)", &relay_port),
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
    .with_note(Box::new(move |_| {
        let mut v = vec![protocol_note(proto).to_string()];
        if uses_sni(proto) && needs_cert(proto) {
            v.push("改 server_name 不会重签证书:CN 是建节点时定的,客户端本来就走 insecure。".into());
        }
        v.push(RELAY_NOTE.into());
        v
    }));

    Modal::Form(form)
}

/// 会生成自签证书的协议。改 `server_name` 时要提醒一句证书不会跟着重签。
fn needs_cert(p: Protocol) -> bool {
    matches!(p, Protocol::Trojan | Protocol::Tuic | Protocol::Anytls | Protocol::Hysteria2)
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
    // 字段与「编辑用户」**一一对应**(除了名称)。
    //
    // 早先这里只有名称和配额,倍率/到期/重置日都得建完再进一次编辑框改 ——
    // 而这三项恰恰是建号时就该定的:到期和重置日决定这个号什么时候停,
    // 漏填一次就是一个「永不过期、永不重置」的号悄悄躺在那里。
    //
    // 留空的默认值写在括号里,和编辑框保持同一套措辞。
    Modal::Form(
        Form::new(
            "新增用户",
            vec![
                Field::text("name", "名称 *必填", ""),
                Field::text("quota", "配额 GB (0 = 不限)", "0"),
                Field::text(
                    "mult",
                    "计费倍率 (2.0 双向 / 1.0 单向)",
                    &format!("{:.1}", crate::db::node_repo::DEFAULT_TRAFFIC_MULTIPLIER),
                ),
                Field::text("expire", "到期 (YYYY-MM-DD,留空 = 永久)", ""),
                Field::text("reset", "重置日 (1-31,留空 = 不重置)", ""),
            ],
            Box::new(|f| {
                let name = val(f, "name");
                if name.is_empty() {
                    return Err("名称不能为空".into());
                }
                Ok(Action::AddUser {
                    name,
                    quota_gb: val(f, "quota"),
                    multiplier: val(f, "mult"),
                    expire: val(f, "expire"),
                    reset_day: val(f, "reset"),
                })
            }),
        )
        .with_note(Box::new(|_| {
            vec![
                "名称唯一,也是 inbound 里的用户名,建好之后不能改。".into(),
                "建完记得按 [n] 分配节点:没分配节点的用户,订阅是空的。".into(),
                "倍率 2.0 = 双向(与 VPS 厂商的网卡口径一致);到期按当天 23:59:59 算。".into(),
            ]
        })),
    )
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
                Field::text("quota", "配额 GB (0 = 不限)", &quota_gb),
                Field::text("mult", "计费倍率 (2.0 双向 / 1.0 单向)", &format!("{:.1}", u.traffic_multiplier)),
                Field::text("expire", "到期 (YYYY-MM-DD,留空 = 永久)", &expire),
                Field::text("reset", "重置日 (1-31,留空 = 不重置)", &reset),
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
        .head(format!("#{} {}(名字不可改:它是 inbound 里的用户标识)", u.id, u.name))
        .with_note(Box::new(|_| {
            vec![
                "计费用量 = (上行 + 下行) × 倍率;到期按当天 23:59:59 算。".into(),
                "倍率 2.0 = 双向:代理要把同样的数据跟目标站点再跑一遍,".into(),
                "网卡上的量约是记账数字的两倍,而 VPS 厂商按网卡收钱。1.0 只算客户端那一段。".into(),
                "这四项不进 sing-box 配置,改它不会重建 box,下次巡检(30s)时生效。".into(),
                "把配额调大之后,原先因超额被系统停用的用户会自动放出来。".into(),
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

/// 把用户的订阅流量绑到几台机器的网卡上(§10.3)。
///
/// 这个功能只影响**订阅响应头里的那三个数字**,别的一概不变:订阅内容、
/// 用户自己的计费与停用判定、界面上的用户用量都走原来的路。
/// 说清楚这一点很要紧 —— 一个只影响「显示」的开关不该让人以为它会限流。
pub fn bind_nics(u: &UserRow, agents: &[AgentRow]) -> Modal {
    let items: Vec<PickItem> = agents
        .iter()
        .map(|a| PickItem {
            id: a.id,
            label: a.name.clone(),
            note: match a.nic_quota_bytes.filter(|q| *q > 0) {
                Some(q) => format!(
                    "本周期 {} / 配额 {} · {}",
                    crate::tui::theme::bytes(a.used()),
                    crate::tui::theme::bytes(q),
                    a.nic_accounting_mode.short()
                ),
                None => format!(
                    "本周期 {} · 未设配额 · {}",
                    crate::tui::theme::bytes(a.used()),
                    a.nic_accounting_mode.short()
                ),
            },
            checked: u.nic_agent_ids.contains(&a.id),
        })
        .collect();

    let user_id = u.id;
    let user = u.name.clone();
    Modal::Picker(Picker::new(
        "订阅按网卡流量报",
        format!("用户 {}(勾上的机器,网卡用量之和会替换掉这个订阅报出去的流量)", u.name),
        items,
        Box::new(move |ids| Action::SetUserNics {
            user_id,
            user: user.clone(),
            agent_ids: ids.to_vec(),
        }),
    ))
}

/// 改一个配置项。一个字段的小表单 —— 布尔项走不到这里(按一下就切了)。
pub fn setting_edit(item: crate::tui::settings::Setting) -> Modal {
    let section = item.section;
    let key = item.key;
    let label = item.label.clone();
    let note = item.note.clone();
    let title = format!("设置 · {}", item.label);
    let prefill = item.edit_value();

    Modal::Form(
        Form::new(
            &title,
            vec![Field::text("v", &item.label, &prefill)],
            Box::new(move |f| {
                let value = item.to_toml(&val(f, "v"))?;
                Ok(Action::SetConfig {
                    section,
                    key,
                    value,
                    label: label.clone(),
                })
            }),
        )
        .head(format!("{section}.{key}"))
        .with_note(Box::new(move |_| {
            vec![
                note.clone(),
                "改的是配置文件本身,注释与排版都保留。".into(),
                "daemon 启动时才读配置 —— 改完记得 systemctl restart sbx。".into(),
            ]
        })),
    )
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

/// 表单里的「重置时区」框 → 偏移秒数。`None` = 不覆盖,跟随 agent 上报(§6.4)。
///
/// **留空必须先判,不能直接丢给 `parse_timezone`。** 那个函数对空串返回
/// `Some(+00:00)`(它服务的是 `telegram.timezone`,那里空 = 用 UTC),
/// 于是「留空 = 跟随 agent」会变成「钉死在 UTC」—— 语义正好相反,
/// 而且只有非 UTC 的机器才看得出错,是那种能活很久的 bug。
pub fn parse_nic_offset(s: &str) -> Result<Option<i64>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    match crate::tg::fmt::parse_timezone(s) {
        Some(off) => Ok(Some(off.local_minus_utc() as i64)),
        None => Err(format!(
            "时区要写成 ±HH:MM(例 -07:00),或留空跟随 agent 上报。\
             会夏令时的时区(America/Los_Angeles 这类)请填显式偏移(收到:{s})"
        )),
    }
}

/// 覆盖值的显示/预填形式。`None`(没覆盖)是空串 —— 表单预填空串才能表达「跟随」。
pub fn nic_offset_label(secs: Option<i64>) -> String {
    secs.map(|s| crate::tg::fmt::format_offset(s as i32)).unwrap_or_default()
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

    /// 表单里的「重置时区」框。
    ///
    /// **最重要的一条是空串。** `parse_timezone("")` 返回 `Some(+00:00)`
    /// (它服务的是 `telegram.timezone`,那里空 = UTC),而表单里空 = 「跟随 agent」。
    /// 直接转手会把「跟随」悄悄变成「钉死 UTC」,而且只有非 UTC 的机器看得出来。
    #[test]
    fn nic_offset_field_treats_empty_as_follow_not_as_utc() {
        assert_eq!(parse_nic_offset("").unwrap(), None, "留空 = 跟随 agent,不是 UTC");
        assert_eq!(parse_nic_offset("   ").unwrap(), None, "全空白也一样");
        assert_eq!(parse_nic_offset("UTC").unwrap(), Some(0), "显式写 UTC 才是钉成 0");

        assert_eq!(parse_nic_offset("-07:00").unwrap(), Some(-25200));
        assert_eq!(parse_nic_offset("-0700").unwrap(), Some(-25200));
        assert_eq!(parse_nic_offset("-7").unwrap(), Some(-25200));
        assert_eq!(parse_nic_offset("Asia/Shanghai").unwrap(), Some(28800));
        // 会夏令时的时区必须被拒:主控不带 tzdata,算不出规则(§6.4)。
        assert!(parse_nic_offset("America/Los_Angeles").is_err());
        assert!(parse_nic_offset("胡说").is_err());

        // 预填/显示形式与解析必须闭环 —— None 是空串,才能表达「跟随」。
        assert_eq!(nic_offset_label(None), "");
        assert_eq!(nic_offset_label(Some(-25200)), "UTC-07:00");
        assert_eq!(parse_nic_offset(&nic_offset_label(Some(28800))).unwrap(), Some(28800));
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
            Field::text("port", "", "443"),
            Field::text("relay_host", "", ""),
            Field::text("relay_port", "", "12345"),
        ];
        let err = draft_from(&f, 1).unwrap_err();
        assert!(err.contains("中转地址"), "{err}");

        // 两项都留空 = 不中转,正常通过。
        let f = vec![
            Field::text("port", "", "443"),
            Field::text("relay_host", "", ""),
            Field::text("relay_port", "", ""),
        ];
        let d = draft_from(&f, 1).unwrap();
        assert!(d.relay_host.is_empty() && d.relay_port.is_none());
    }

    #[test]
    fn port_must_be_a_valid_number() {
        for bad in ["", "0", "70000", "四四三"] {
            let f = vec![Field::text("port", "", bad)];
            assert!(draft_from(&f, 1).is_err(), "端口 {bad:?} 应当被拒");
        }
    }
}

#[cfg(test)]
mod preview {
    use super::*;
    use crate::tui::modal;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// 把每个二级页面画出来打到 stdout,给人看一眼:
    ///
    /// ```sh
    /// cargo test tui::forms::preview -- --nocapture
    /// ```
    ///
    /// 「留白太多、太散」这类问题读代码看不出来,必须看渲染结果。
    #[test]
    fn all_forms() {
        let agents = vec![crate::tui::data::AgentRow {
            id: 1,
            name: "azure".into(),
            token_prefix: "abcd1234".into(),
            status: "online".into(),
            agent_version: Some("v0.2.1".into()),
            arch: Some("amd64".into()),
            outbound: Default::default(),
            ipv4: Some("203.0.113.8".into()),
            ipv6: None,
            nic_quota_bytes: None,
            nic_reset_day: None,
            nic_accounting_mode: Default::default(),
        reported_utc_offset_secs: None,
        nic_reset_offset_secs: None,
            cycle_rx: 0,
            cycle_tx: 0,
            up_per_sec: None,
            down_per_sec: None,
            node_count: 1,
            cpu_pct: None,
            mem_used: None,
            mem_total: None,
            load1: None,
            uptime_secs: None,
            sysinfo_at: None,
        }];
        let node = NodeRow {
            id: 1,
            agent_id: 1,
            agent_name: "azure".into(),
            tag: "vless".into(),
            protocol: "vless-reality".into(),
            listen_port: 443,
            user_count: 0,
            cycle_up: 0,
            cycle_down: 0,
            params: crate::model::node::NodeParams {
                server_name: Some("www.apple.com".into()),
                ..Default::default()
            },
        };
        let user = UserRow {
            id: 1,
            name: "alice".into(),
            enabled: true,
            auto_disabled: false,
            quota_bytes: 100 * 1_073_741_824,
            cycle_up: 0,
            cycle_down: 0,
            traffic_multiplier: 1.0,
            expire_at: None,
            reset_day: Some(22),
            node_ids: vec![1],
            nic_agent_ids: vec![],
            sub_token: "tok".into(),
        };

        for (name, m) in [
            ("新增节点", node_add(&agents, 0)),
            ("编辑节点", node_edit(&node)),
            ("新增用户", user_add()),
            ("编辑用户", user_edit(&user)),
            ("新增被控服务器", agent_add()),
            ("分配节点", assign_nodes(&user, std::slice::from_ref(&node))),
            (
                "改设置",
                setting_edit(
                    crate::tui::settings::all(&crate::config::Config::default())
                        .into_iter()
                        .next()
                        .unwrap(),
                ),
            ),
        ] {
            let mut term = Terminal::new(TestBackend::new(116, 30)).unwrap();
            term.draw(|f| modal::render(f, f.area(), &m)).unwrap();
            let buf = term.backend().buffer().clone();
            let out: Vec<String> = (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .filter(|l| !l.is_empty())
                .collect();
            println!("── {name} ──\n{}\n", out.join("\n"));
        }
    }
}
