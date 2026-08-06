//! 订阅导出(DESIGN.md §10)。移植自旧项目 `service/sub_service.rs`。
//!
//! **相对旧版唯一的实质改动:节点地址来源。** 旧项目只管一台机器,地址靠本机公网 IP 探测;
//! sbx 一个用户的节点散在多台 agent 上,所以地址读的是 `agents.ipv4 / ipv6`
//! (agent 握手时自报,主控可手工覆盖,§7.3)。
//!
//! 保留下来的三件事:
//!   * 中转覆盖(`RelaySetting`)—— 只换订阅里的落点,SNI / 凭据 / 传输参数都不变;
//!   * `use_public_base_as_server` 的四步优先级链;
//!   * IPv6 字面量补方括号 —— 不补的话拼进 URL 是非法的,客户端直接解析失败。
//!
//! 一个用户的订阅 = 它在 `user_nodes` 里的**全部节点,跨 agent 拼在一起**。
//!
//! 三种输出格式:
//!   * base64 的分享链接列表(sing-box / v2rayN 等通用);
//!   * Clash/Mihomo 的 YAML;
//!   * (流量统计 HTML 页在 `stats_html`,不在这里)

use crate::model::node::{NodeParams, Protocol};
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use sqlx::SqlitePool;
use std::fmt::Write as _;

/// 一条分享链接。
#[derive(Debug, Clone)]
pub struct ShareLink {
    /// 节点 tag。TUI(§8)的节点页会按 tag 展示;CLI 目前只打印协议和链接。
    #[allow(dead_code)]
    pub tag: String,
    pub protocol: String,
    pub link: String,
}

/// 订阅里要导出的一个节点:节点自身 + 它所属 agent 的对外地址。
#[derive(Debug, Clone)]
pub struct ExportNode {
    pub tag: String,
    pub protocol: Protocol,
    pub listen_port: u16,
    pub params: NodeParams,
    /// 所属 agent 自报的公网地址。**可能是 None** —— agent 从没连上来过,
    /// 或者探测失败且管理员也没手工填。这种节点导不出地址,只能跳过。
    pub agent_ipv4: Option<String>,
    pub agent_ipv6: Option<String>,
}

/// 订阅用户的凭据。三个字段按协议取用:
/// vless/vmess 用 uuid,trojan/hy2/anytls 用 password,tuic 两个都要,
/// shadowsocks 用 uuid 派生的 16 字节密钥。
#[derive(Debug, Clone)]
pub struct SubUser {
    pub name: String,
    pub uuid: String,
    pub password: String,
}

/// 影响地址解析的部署配置(对应 `[subscription]` 那一节)。
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOptions<'a> {
    pub use_public_base_as_server: bool,
    pub public_base: &'a str,
    /// 请求的 Host 头。前两级都拿不到地址时的最后兜底。
    pub request_host: Option<&'a str>,
}

/// `export_nodes` 的一行:`(tag, protocol, listen_port, params_json, agent.ipv4, agent.ipv6)`。
///
/// 起别名不只是为了让 clippy 闭嘴 —— 后两个 `Option<String>` 相邻且同型,
/// 解构时写反了就是「v4 节点导出 v6 地址」,而那种链接看起来完全正常,只是连不上。
type NodeRow = (String, String, i64, String, Option<String>, Option<String>);

/// 查出某用户可导出的全部节点(跨 agent)。
pub async fn export_nodes(pool: &SqlitePool, user_id: i64) -> Result<Vec<ExportNode>> {
    let rows: Vec<NodeRow> = sqlx::query_as(
        "SELECT n.tag, n.protocol, n.listen_port, n.params_json, a.ipv4, a.ipv6
           FROM nodes n
           JOIN user_nodes un ON un.node_id = n.id
           JOIN agents a ON a.id = n.agent_id
          WHERE un.user_id = ?
          ORDER BY n.agent_id, n.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(tag, proto, port, params_json, ipv4, ipv6)| ExportNode {
            tag,
            protocol: Protocol::parse(&proto),
            listen_port: port.clamp(0, u16::MAX as i64) as u16,
            // 参数解不出来时用默认值:一个字段写坏了不该让整份订阅 500。
            // 结果是该节点缺 sni/密钥,链接生成时会被跳过。
            params: serde_json::from_str(&params_json).unwrap_or_default(),
            agent_ipv4: ipv4.filter(|s| !s.trim().is_empty()),
            agent_ipv6: ipv6.filter(|s| !s.trim().is_empty()),
        })
        .collect())
}

/// 按 sub_token 查用户。找不到返回 `None`(订阅服务据此回 404)。
pub async fn find_by_sub_token(pool: &SqlitePool, token: &str) -> Result<Option<(i64, SubUser)>> {
    let row: Option<(i64, String, String, String)> =
        sqlx::query_as("SELECT id, name, uuid, password FROM users WHERE sub_token = ?")
            .bind(token)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id, name, uuid, password)| (id, SubUser { name, uuid, password })))
}

// ─────────────────────────── 地址与端口 ───────────────────────────

/// 规范化一个主机地址。IPv6 字面量必须补方括号,否则拼进 URL 非法。
///
/// 长度上限 64 是防御性的:这个值会进 URL、进 HTML 属性,来源是数据库里
/// 可被管理员编辑的字段。
pub fn normalize_host(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.len() > 64 {
        return None;
    }
    Some(if t.contains(':') && !t.starts_with('[') {
        format!("[{t}]")
    } else {
        t.to_string()
    })
}

/// 从 `public_base`(形如 `https://sub.example.com:8443/x`)里取出 host[:port]。
fn public_base_host(public_base: &str) -> Option<String> {
    let s = public_base.trim();
    if s.is_empty() {
        return None;
    }
    let rest = s
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(s);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 这里**不能**用 normalize_host:public_base 里的 IPv6 已经是带方括号的
    // URL 形式了,再包一层会变成 [[::1]]。
    let host = host.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// 订阅里对外暴露的 `(地址, 端口)`。
///
/// 端口优先级:中转端口 > 端口复用固定 443 > `listen_port`。
/// 地址优先级:
///   1. 中转机地址(`relay.host`)—— 中转只换落点,凭据和 SNI 都不动;
///   2. `use_public_base_as_server` 时的 `public_base` 主机;
///   3. 节点所属 agent 的公网地址(按节点的 `ipv6` 偏好选,取不到就用另一个);
///   4. `public_base` 主机;
///   5. 请求的 Host 头。
///
/// 全都拿不到就返回 `None`,该节点**跳过而不是报错** —— 一台 agent 没上报过 IP
/// 不该让这个用户的其它节点也导不出来。
pub fn endpoint(node: &ExportNode, opts: &ExportOptions<'_>) -> Option<(String, u16)> {
    let own_port = if node.params.port_reuse { 443 } else { node.listen_port };
    let port = node.params.relay.port.unwrap_or(own_port);

    if node.params.relay.is_enabled() {
        if let Some(h) = normalize_host(&node.params.relay.host) {
            return Some((h, port));
        }
    }
    if opts.use_public_base_as_server {
        if let Some(h) = public_base_host(opts.public_base) {
            return Some((h, port));
        }
    }
    let (first, second) = if node.params.ipv6 {
        (&node.agent_ipv6, &node.agent_ipv4)
    } else {
        (&node.agent_ipv4, &node.agent_ipv6)
    };
    for cand in [first, second] {
        if let Some(h) = cand.as_deref().and_then(normalize_host) {
            return Some((h, port));
        }
    }
    if let Some(h) = public_base_host(opts.public_base) {
        return Some((h, port));
    }
    if let Some(h) = opts.request_host.and_then(public_base_host) {
        return Some((h, port));
    }
    None
}

// ─────────────────────────── 分享链接 ───────────────────────────

/// 订阅链接里是否要带「跳过证书校验」。
///
/// sbx 的 TLS 系协议一律用主控生成的**自签证书**(§9.1),客户端校验不过,
/// 必须显式跳过。reality 是例外:它借的是真实站点的证书,走完整校验。
///
/// 这里没有旧项目那条 "acme / 用户自备证书" 分支 —— sbx 不支持那两种,
/// 加一个永远为假的判断只会让人以为它存在。
fn skip_cert_verify(proto: Protocol) -> bool {
    matches!(
        proto,
        Protocol::Trojan | Protocol::Hysteria2 | Protocol::Tuic | Protocol::Anytls
    )
}

fn enc(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn param(params: &NodeParams, f: impl Fn(&NodeParams) -> Option<&String>) -> Option<String> {
    f(params).map(|s| s.to_string()).filter(|s| !s.is_empty())
}

/// 生成一个用户的全部分享链接。
///
/// 缺地址或缺密钥材料的节点**跳过**,不报错 —— 订阅是给终端用户看的,
/// 一个坏节点不该让他整份订阅拿不到。运维侧的可见性由 `sbx node-list` 负责。
pub fn generate_links(user: &SubUser, nodes: &[ExportNode], opts: &ExportOptions<'_>) -> Vec<ShareLink> {
    let mut links = Vec::with_capacity(nodes.len());
    for node in nodes {
        let Some((server, port)) = endpoint(node, opts) else {
            continue;
        };
        let Some(link) = share_link(user, node, &server, port) else {
            continue;
        };
        links.push(ShareLink {
            tag: node.tag.clone(),
            protocol: node.protocol.as_str().to_string(),
            link,
        });
    }
    links
}

fn share_link(u: &SubUser, n: &ExportNode, s: &str, port: u16) -> Option<String> {
    let tag = enc(&n.tag);
    let p = &n.params;
    let insec = skip_cert_verify(n.protocol);

    match n.protocol {
        Protocol::VlessReality => {
            // 缺 sni / 公钥的节点导不出可用链接,跳过而不是导一条连不上的。
            let sni = param(p, |x| x.server_name.as_ref())?;
            let pbk = param(p, |x| x.public_key.as_ref())?;
            let sid = param(p, |x| x.short_id.as_ref()).unwrap_or_default();
            Some(format!(
                "vless://{}@{}:{}?encryption=none&flow=xtls-rprx-vision&security=reality\
                 &sni={}&fp=chrome&pbk={}&sid={}&type=tcp#{}",
                u.uuid,
                s,
                port,
                enc(&sni),
                enc(&pbk),
                enc(&sid),
                tag
            ))
        }

        // ws 系在 sbx 里**不启用后端 TLS**(§9.1):正常部署由 nginx/caddy 做终结。
        // 所以 security=none,也没有 sni / allowInsecure 参数。
        Protocol::VlessWs => {
            let path = param(p, |x| x.path.as_ref()).unwrap_or_else(|| "/".into());
            Some(format!(
                "vless://{}@{}:{}?encryption=none&security=none&type=ws&path={}#{}",
                u.uuid,
                s,
                port,
                enc(&path),
                tag
            ))
        }

        Protocol::VmessWs => {
            let path = param(p, |x| x.path.as_ref()).unwrap_or_else(|| "/".into());
            // vmess 的分享格式是 base64 的一坨 JSON,不是 query string。
            let obj = serde_json::json!({
                "v": "2",
                "ps": n.tag,
                "add": s,
                "port": port.to_string(),
                "id": u.uuid,
                "aid": "0",
                "net": "ws",
                "type": "none",
                "host": p.server_name.clone().unwrap_or_else(|| s.to_string()),
                "path": path,
                "tls": "",
            });
            Some(format!("vmess://{}", STANDARD.encode(obj.to_string())))
        }

        Protocol::Shadowsocks => {
            let method = param(p, |x| x.ss_method.as_ref())
                .unwrap_or_else(|| crate::secrets::SS_DEFAULT_METHOD.to_string());
            let psk = param(p, |x| x.ss_password.as_ref())?;
            // 2022 系列:服务端密钥和用户密钥用冒号拼起来一起给客户端。
            let pw = format!("{}:{}", psk, crate::secrets::ss_user_password(&u.uuid));
            Some(format!(
                "ss://{}@{}:{}#{}",
                STANDARD.encode(format!("{method}:{pw}")),
                s,
                port,
                tag
            ))
        }

        Protocol::Trojan => Some(format!(
            "trojan://{}@{}:{}?{}#{}",
            enc(&u.password),
            s,
            port,
            query(&[
                Some("security=tls".into()),
                sni_param(p),
                Some("type=tcp".into()),
                insec.then(|| "allowInsecure=1".into()),
            ]),
            tag
        )),

        // hy2 的 inbound 不带 server_name(§9.1),所以这里通常没有 sni ——
        // 那正好:客户端会用连接地址当 SNI,而证书本来就是自签的。
        Protocol::Hysteria2 => Some(format!(
            "hysteria2://{}@{}:{}?{}#{}",
            enc(&u.password),
            s,
            port,
            query(&[sni_param(p), insec.then(|| "insecure=1".into())]),
            tag
        )),

        Protocol::Tuic => Some(format!(
            "tuic://{}:{}@{}:{}?{}#{}",
            u.uuid,
            enc(&u.password),
            s,
            port,
            query(&[
                Some("congestion_control=bbr".into()),
                Some("alpn=h3".into()),
                sni_param(p),
                Some("udp_relay_mode=native".into()),
                insec.then(|| "allow_insecure=1".into()),
            ]),
            tag
        )),

        Protocol::Anytls => Some(format!(
            "anytls://{}@{}:{}?{}#{}",
            enc(&u.password),
            s,
            port,
            query(&[sni_param(p), insec.then(|| "allowInsecure=1".into())]),
            tag
        )),

        Protocol::Unknown => None,
    }
}

/// 把若干可选的 `k=v` 拼成 query 串。
///
/// 用列表而不是字符串拼接:sni 是可选的,而它在不同协议里的位置不一样,
/// 手写拼接很容易漏掉分隔符,拼出 `?&insecure=1` 这种东西
/// —— 那正是这个函数被提出来的原因(测试 `sni_is_omitted_…` 抓到过一次)。
fn query(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .flatten()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("&")
}

/// `sni=<域名>`,节点没配 server_name 时返回 `None`。
///
/// **不拿连接地址兜底。** SNI 按定义是域名;把 IP 塞进去在 IPv6 场景下会变成
/// `sni=%5B2001%3Adb8%3A%3A1%5D` 这种东西,某些客户端和中间设备会直接拒掉。
/// 这些协议本来就走自签证书 + 跳过校验,少一个 sni 不影响连通。
fn sni_param(p: &NodeParams) -> Option<String> {
    param(p, |x| x.server_name.as_ref()).map(|sni| format!("sni={}", enc(&sni)))
}

/// base64 的订阅正文:每行一条链接,整体 base64。
pub fn subscription_b64(links: &[ShareLink]) -> String {
    STANDARD.encode(
        links
            .iter()
            .map(|l| l.link.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// 简易 YAML 标量。
///
/// 只要值里有可能改变 YAML 结构的字符就加引号。宁可多加 —— 多一对引号
/// 客户端照样解析,而少一对会让一个带冒号的 tag 把整份配置撑坏。
///
/// 判断分两类:
///   * **含**冒号 / `#` / 引号 / 换行 / 制表符 —— 会被当成 key 分隔符或注释起点;
///   * **以** YAML 指示符开头 —— `-` 开头像列表项,`{`/`[` 开头像流式集合。
fn yaml_str(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars().any(|c| matches!(c, ':' | '#' | '\'' | '"' | '\n' | '\t'))
        || s.starts_with(['-', '?', ',', '[', ']', '{', '}', '&', '*', '!', '|', '>', '%', '@', '`', ' '])
        || s.ends_with(' ');
    if needs_quote {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Clash / Mihomo 的 YAML 订阅。
pub fn generate_clash_yaml(user: &SubUser, nodes: &[ExportNode], opts: &ExportOptions<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# mihomo / clash-meta subscription for {}", user.name);
    let _ = writeln!(out, "mixed-port: 7890");
    let _ = writeln!(out, "allow-lan: false");
    let _ = writeln!(out, "mode: rule");
    let _ = writeln!(out, "log-level: info");
    let _ = writeln!(out);
    let _ = writeln!(out, "proxies:");

    let mut names: Vec<String> = Vec::new();
    for node in nodes {
        let Some((server, port)) = endpoint(node, opts) else {
            continue;
        };
        if clash_proxy(&mut out, user, node, &server, port) {
            names.push(node.tag.clone());
        }
    }
    if names.is_empty() {
        let _ = writeln!(out, "  []");
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "proxy-groups:");
    let _ = writeln!(out, "  - name: 节点选择");
    let _ = writeln!(out, "    type: select");
    let _ = writeln!(out, "    proxies:");
    let _ = writeln!(out, "      - 自动选择");
    let _ = writeln!(out, "      - DIRECT");
    for n in &names {
        let _ = writeln!(out, "      - {}", yaml_str(n));
    }
    let _ = writeln!(out, "  - name: 自动选择");
    let _ = writeln!(out, "    type: url-test");
    let _ = writeln!(out, "    url: http://www.gstatic.com/generate_204");
    let _ = writeln!(out, "    interval: 300");
    let _ = writeln!(out, "    proxies:");
    for n in &names {
        let _ = writeln!(out, "      - {}", yaml_str(n));
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "rules:");
    let _ = writeln!(out, "  - GEOIP,CN,DIRECT");
    let _ = writeln!(out, "  - MATCH,节点选择");
    out
}

/// 写一条 Clash proxy。返回 false 表示这个节点导不出(缺密钥材料),调用方跳过它。
///
/// 注意 `server:` 一律走 `yaml_str` —— IPv6 字面量是 `[2001:db8::1]` 这种形状,
/// 直接写进 YAML 会被解析成**流式序列**(一个列表),而不是字符串。
/// 那种配置 clash 不会报错,只会把 server 当成一个数组然后连不上。
fn clash_proxy(out: &mut String, u: &SubUser, n: &ExportNode, s: &str, port: u16) -> bool {
    let p = &n.params;
    let name = yaml_str(&n.tag);
    let server = yaml_str(s);
    let insec = skip_cert_verify(n.protocol);
    // SNI 只在节点显式配了 server_name 时才写。
    // **不拿连接地址兜底**:那会在 IP 直连的场景下把一个 IP(甚至带方括号的 IPv6)
    // 当成 SNI 发出去,而 SNI 按定义只能是域名。反正这些协议都走自签证书 +
    // skip-cert-verify,少一个 sni 不影响连通,多一个错的反而可能被中间设备拒掉。
    let sni = param(p, |x| x.server_name.as_ref());

    match n.protocol {
        Protocol::VlessReality => {
            let (Some(server_name), Some(pbk)) = (
                param(p, |x| x.server_name.as_ref()),
                param(p, |x| x.public_key.as_ref()),
            ) else {
                return false;
            };
            let sid = param(p, |x| x.short_id.as_ref()).unwrap_or_default();
            let _ = writeln!(out, "  - name: {name}");
            let _ = writeln!(out, "    type: vless");
            let _ = writeln!(out, "    server: {server}");
            let _ = writeln!(out, "    port: {port}");
            let _ = writeln!(out, "    uuid: {}", u.uuid);
            let _ = writeln!(out, "    network: tcp");
            let _ = writeln!(out, "    udp: true");
            let _ = writeln!(out, "    tls: true");
            let _ = writeln!(out, "    flow: xtls-rprx-vision");
            let _ = writeln!(out, "    servername: {}", yaml_str(&server_name));
            let _ = writeln!(out, "    reality-opts:");
            let _ = writeln!(out, "      public-key: {}", yaml_str(&pbk));
            let _ = writeln!(out, "      short-id: {}", yaml_str(&sid));
            let _ = writeln!(out, "    client-fingerprint: chrome");
        }

        Protocol::VlessWs => {
            let path = param(p, |x| x.path.as_ref()).unwrap_or_else(|| "/".into());
            let _ = writeln!(out, "  - name: {name}");
            let _ = writeln!(out, "    type: vless");
            let _ = writeln!(out, "    server: {server}");
            let _ = writeln!(out, "    port: {port}");
            let _ = writeln!(out, "    uuid: {}", u.uuid);
            let _ = writeln!(out, "    network: ws");
            let _ = writeln!(out, "    udp: true");
            let _ = writeln!(out, "    tls: false");
            let _ = writeln!(out, "    ws-opts:");
            let _ = writeln!(out, "      path: {}", yaml_str(&path));
        }

        Protocol::VmessWs => {
            let path = param(p, |x| x.path.as_ref()).unwrap_or_else(|| "/".into());
            let _ = writeln!(out, "  - name: {name}");
            let _ = writeln!(out, "    type: vmess");
            let _ = writeln!(out, "    server: {server}");
            let _ = writeln!(out, "    port: {port}");
            let _ = writeln!(out, "    uuid: {}", u.uuid);
            let _ = writeln!(out, "    alterId: 0");
            let _ = writeln!(out, "    cipher: auto");
            let _ = writeln!(out, "    network: ws");
            let _ = writeln!(out, "    udp: true");
            let _ = writeln!(out, "    tls: false");
            let _ = writeln!(out, "    ws-opts:");
            let _ = writeln!(out, "      path: {}", yaml_str(&path));
        }

        Protocol::Shadowsocks => {
            let method = param(p, |x| x.ss_method.as_ref())
                .unwrap_or_else(|| crate::secrets::SS_DEFAULT_METHOD.to_string());
            let Some(psk) = param(p, |x| x.ss_password.as_ref()) else {
                return false;
            };
            let pw = format!("{}:{}", psk, crate::secrets::ss_user_password(&u.uuid));
            let _ = writeln!(out, "  - name: {name}");
            let _ = writeln!(out, "    type: ss");
            let _ = writeln!(out, "    server: {server}");
            let _ = writeln!(out, "    port: {port}");
            let _ = writeln!(out, "    cipher: {}", yaml_str(&method));
            let _ = writeln!(out, "    password: {}", yaml_str(&pw));
            let _ = writeln!(out, "    udp: true");
        }

        Protocol::Trojan => {
            let _ = writeln!(out, "  - name: {name}");
            let _ = writeln!(out, "    type: trojan");
            let _ = writeln!(out, "    server: {server}");
            let _ = writeln!(out, "    port: {port}");
            let _ = writeln!(out, "    password: {}", yaml_str(&u.password));
            let _ = writeln!(out, "    udp: true");
            if let Some(sni) = &sni {
                let _ = writeln!(out, "    sni: {}", yaml_str(sni));
            }
            let _ = writeln!(out, "    skip-cert-verify: {insec}");
        }

        Protocol::Hysteria2 => {
            let _ = writeln!(out, "  - name: {name}");
            let _ = writeln!(out, "    type: hysteria2");
            let _ = writeln!(out, "    server: {server}");
            let _ = writeln!(out, "    port: {port}");
            let _ = writeln!(out, "    password: {}", yaml_str(&u.password));
            if let Some(sni) = &sni {
                let _ = writeln!(out, "    sni: {}", yaml_str(sni));
            }
            let _ = writeln!(out, "    skip-cert-verify: {insec}");
        }

        Protocol::Tuic => {
            let _ = writeln!(out, "  - name: {name}");
            let _ = writeln!(out, "    type: tuic");
            let _ = writeln!(out, "    server: {server}");
            let _ = writeln!(out, "    port: {port}");
            let _ = writeln!(out, "    uuid: {}", u.uuid);
            let _ = writeln!(out, "    password: {}", yaml_str(&u.password));
            if let Some(sni) = &sni {
                let _ = writeln!(out, "    sni: {}", yaml_str(sni));
            }
            let _ = writeln!(out, "    alpn: [h3]");
            let _ = writeln!(out, "    congestion-controller: bbr");
            let _ = writeln!(out, "    udp-relay-mode: native");
            let _ = writeln!(out, "    skip-cert-verify: {insec}");
        }

        Protocol::Anytls => {
            let _ = writeln!(out, "  - name: {name}");
            let _ = writeln!(out, "    type: anytls");
            let _ = writeln!(out, "    server: {server}");
            let _ = writeln!(out, "    port: {port}");
            let _ = writeln!(out, "    password: {}", yaml_str(&u.password));
            if let Some(sni) = &sni {
                let _ = writeln!(out, "    sni: {}", yaml_str(sni));
            }
            let _ = writeln!(out, "    skip-cert-verify: {insec}");
        }

        Protocol::Unknown => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::node::RelaySetting;

    fn user() -> SubUser {
        SubUser {
            name: "alice".into(),
            uuid: "b831381d-6324-4d53-ad4f-8cda48b30811".into(),
            password: "p@ss w/rd".into(), // 刻意带空格和斜杠:URL 编码必须生效
        }
    }

    fn node(proto: Protocol) -> ExportNode {
        let mut params = NodeParams::default();
        crate::secrets::fill(proto, &mut params).unwrap();
        ExportNode {
            tag: "tokyo-1".into(),
            protocol: proto,
            listen_port: 8443,
            params,
            agent_ipv4: Some("203.0.113.7".into()),
            agent_ipv6: Some("2001:db8::1".into()),
        }
    }

    fn opts() -> ExportOptions<'static> {
        ExportOptions::default()
    }

    // ─────────────── 地址与端口 ───────────────

    #[test]
    fn ipv4_stays_plain_and_ipv6_gets_brackets() {
        assert_eq!(normalize_host(" 1.2.3.4\n"), Some("1.2.3.4".into()));
        // 不补方括号的话 `vless://…@2001:db8::1:8443` 里的端口分隔符没法区分,
        // 客户端解析直接失败。
        assert_eq!(normalize_host("2001:db8::1"), Some("[2001:db8::1]".into()));
        assert_eq!(normalize_host("[2001:db8::1]"), Some("[2001:db8::1]".into()));
        assert_eq!(normalize_host("   "), None);
        assert_eq!(normalize_host(&"x".repeat(65)), None);
    }

    #[test]
    fn address_comes_from_the_owning_agent() {
        let n = node(Protocol::VlessReality);
        assert_eq!(endpoint(&n, &opts()), Some(("203.0.113.7".into(), 8443)));
    }

    /// 节点标了 ipv6 偏好就走 agent 的 v6 地址,而且要带方括号。
    #[test]
    fn ipv6_preference_selects_the_v6_address() {
        let mut n = node(Protocol::VlessReality);
        n.params.ipv6 = true;
        assert_eq!(endpoint(&n, &opts()), Some(("[2001:db8::1]".into(), 8443)));
    }

    /// 偏好的那一路没有地址时回落到另一路,而不是导不出。
    #[test]
    fn falls_back_to_the_other_family_when_preferred_is_missing() {
        let mut n = node(Protocol::VlessReality);
        n.params.ipv6 = true;
        n.agent_ipv6 = None;
        assert_eq!(endpoint(&n, &opts()), Some(("203.0.113.7".into(), 8443)));
    }

    /// agent 从没上报过地址 → 该节点跳过。**不该报错**,
    /// 否则一台没上线的 agent 会让这个用户的所有节点都导不出来。
    #[test]
    fn node_without_any_agent_address_is_skipped() {
        let mut n = node(Protocol::VlessReality);
        n.agent_ipv4 = None;
        n.agent_ipv6 = None;
        assert_eq!(endpoint(&n, &opts()), None);

        let links = generate_links(&user(), &[n, node(Protocol::Trojan)], &opts());
        assert_eq!(links.len(), 1, "坏节点跳过,好节点照常导出");
        assert_eq!(links[0].protocol, "trojan");
    }

    #[test]
    fn port_reuse_exports_443() {
        let mut n = node(Protocol::VlessReality);
        n.params.port_reuse = true;
        assert_eq!(endpoint(&n, &opts()).unwrap().1, 443);
    }

    /// 中转覆盖地址和端口,但**不动**凭据与 SNI ——
    /// 客户端连中转机,由它把流量转到落地机的 listen_port。
    #[test]
    fn relay_overrides_host_and_port_but_not_sni() {
        let mut n = node(Protocol::VlessReality);
        n.params.relay = RelaySetting { host: "relay.example.com".into(), port: Some(2087) };
        assert_eq!(endpoint(&n, &opts()), Some(("relay.example.com".into(), 2087)));

        let link = &generate_links(&user(), std::slice::from_ref(&n), &opts())[0].link;
        assert!(link.contains("relay.example.com:2087"));
        // sni 仍然是节点自己的伪装域名,不是中转机地址。
        assert!(link.contains(&format!("sni={}", n.params.server_name.clone().unwrap())));
    }

    /// 中转只填 host 不填 port 时,沿用节点自身对外端口。
    #[test]
    fn relay_without_port_keeps_the_node_port() {
        let mut n = node(Protocol::VlessReality);
        n.params.relay = RelaySetting { host: "relay.example.com".into(), port: None };
        assert_eq!(endpoint(&n, &opts()), Some(("relay.example.com".into(), 8443)));
    }

    #[test]
    fn public_base_as_server_overrides_agent_address() {
        let n = node(Protocol::VlessReality);
        let o = ExportOptions {
            use_public_base_as_server: true,
            public_base: "https://sub.example.com/x",
            request_host: None,
        };
        assert_eq!(endpoint(&n, &o), Some(("sub.example.com".into(), 8443)));
    }

    /// 优先级链的后两级:agent 没地址时依次退到 public_base、再退到 Host 头。
    #[test]
    fn falls_back_to_public_base_then_request_host() {
        let mut n = node(Protocol::VlessReality);
        n.agent_ipv4 = None;
        n.agent_ipv6 = None;

        let o = ExportOptions {
            use_public_base_as_server: false,
            public_base: "https://sub.example.com",
            request_host: Some("host.example.com"),
        };
        assert_eq!(endpoint(&n, &o).unwrap().0, "sub.example.com");

        let o2 = ExportOptions { public_base: "", ..o };
        assert_eq!(endpoint(&n, &o2).unwrap().0, "host.example.com");
    }

    /// 中转优先于 `use_public_base_as_server` —— 它是**按节点**的显式指定,
    /// 比全局开关更具体。
    #[test]
    fn relay_beats_public_base_override() {
        let mut n = node(Protocol::VlessReality);
        n.params.relay = RelaySetting { host: "relay.example.com".into(), port: None };
        let o = ExportOptions {
            use_public_base_as_server: true,
            public_base: "https://sub.example.com",
            request_host: None,
        };
        assert_eq!(endpoint(&n, &o).unwrap().0, "relay.example.com");
    }

    // ─────────────── 分享链接 ───────────────

    #[test]
    fn every_protocol_produces_a_link() {
        let nodes: Vec<_> = Protocol::all().iter().map(|p| node(*p)).collect();
        let links = generate_links(&user(), &nodes, &opts());
        assert_eq!(links.len(), 8, "八个协议都该导出:{links:?}");

        let schemes = ["vless://", "vless://", "vmess://", "ss://", "trojan://", "tuic://", "anytls://", "hysteria2://"];
        for (link, want) in links.iter().zip(schemes) {
            assert!(link.link.starts_with(want), "{} 应以 {want} 开头: {}", link.protocol, link.link);
        }
    }

    /// reality 链接必须带公钥 —— 少了它客户端握不上手,
    /// 而这正是「公钥只进订阅、私钥只进 inbound」那条分工的验收点。
    #[test]
    fn reality_link_carries_public_key_and_never_the_private_one() {
        let n = node(Protocol::VlessReality);
        let link = &generate_links(&user(), std::slice::from_ref(&n), &opts())[0].link;
        assert!(link.contains(&format!("pbk={}", n.params.public_key.clone().unwrap())));
        assert!(
            !link.contains(n.params.private_key.as_deref().unwrap()),
            "私钥泄进了订阅链接:{link}"
        );
        assert!(link.contains("flow=xtls-rprx-vision"));
        assert!(!link.contains("allowInsecure"), "reality 走真实握手,不该跳过校验");
    }

    /// 自签证书的四个协议必须让客户端跳过校验,否则一条都连不上。
    #[test]
    fn self_signed_protocols_ask_clients_to_skip_verification() {
        let cases = [
            (Protocol::Trojan, "allowInsecure=1"),
            (Protocol::Anytls, "allowInsecure=1"),
            (Protocol::Hysteria2, "insecure=1"),
            (Protocol::Tuic, "allow_insecure=1"),
        ];
        for (proto, flag) in cases {
            let link = &generate_links(&user(), &[node(proto)], &opts())[0].link;
            assert!(link.contains(flag), "{proto} 少了 {flag}: {link}");
        }
    }

    /// 凭据里的空格、斜杠这类字符必须编码 ——
    /// 不编码会把链接拆断,也会顺着订阅页流进 HTML 属性上下文。
    #[test]
    fn credentials_and_names_are_url_encoded() {
        let mut n = node(Protocol::Trojan);
        n.tag = "东京 #1".into();
        let link = &generate_links(&user(), &[n], &opts())[0].link;
        assert!(!link.contains("p@ss w/rd"), "密码没编码: {link}");
        assert!(link.contains("p%40ss%20w%2Frd"), "{link}");
        // fragment 里的空格和 # 也要编码,否则 # 会被当成新的 fragment 起点。
        assert!(!link.ends_with("东京 #1"), "tag 没编码: {link}");
    }

    /// shadowsocks 2022:服务端密钥和用户密钥要一起交给客户端(冒号拼接),
    /// 少了任何一半都连不上。
    #[test]
    fn shadowsocks_link_carries_both_keys() {
        let n = node(Protocol::Shadowsocks);
        let link = &generate_links(&user(), std::slice::from_ref(&n), &opts())[0].link;
        let b64 = link.trim_start_matches("ss://").split('@').next().unwrap();
        let decoded = String::from_utf8(STANDARD.decode(b64).unwrap()).unwrap();
        let psk = n.params.ss_password.clone().unwrap();
        assert!(decoded.starts_with(crate::secrets::SS_DEFAULT_METHOD), "{decoded}");
        assert!(decoded.contains(&psk), "缺服务端密钥: {decoded}");
        assert!(
            decoded.contains(&crate::secrets::ss_user_password(&user().uuid)),
            "缺用户密钥: {decoded}"
        );
    }

    #[test]
    fn vmess_link_is_base64_json_with_the_right_fields() {
        let n = node(Protocol::VmessWs);
        let link = &generate_links(&user(), &[n], &opts())[0].link;
        let raw = STANDARD.decode(link.trim_start_matches("vmess://")).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["id"], user().uuid);
        assert_eq!(v["net"], "ws");
        assert_eq!(v["path"], "/vmess");
        assert_eq!(v["tls"], "", "sbx 的 ws 系不开后端 TLS(§9.1)");
    }

    #[test]
    fn subscription_body_is_base64_of_newline_separated_links() {
        let nodes: Vec<_> = Protocol::all().iter().map(|p| node(*p)).collect();
        let links = generate_links(&user(), &nodes, &opts());
        let decoded = String::from_utf8(STANDARD.decode(subscription_b64(&links)).unwrap()).unwrap();
        assert_eq!(decoded.lines().count(), 8);
    }

    #[test]
    fn empty_subscription_is_empty_not_an_error() {
        assert_eq!(subscription_b64(&[]), "");
    }

    // ─────────────── Clash YAML ───────────────

    #[test]
    fn clash_yaml_lists_every_protocol_once() {
        let nodes: Vec<_> = Protocol::all()
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mut n = node(*p);
                n.tag = format!("n{i}");
                n
            })
            .collect();
        let yaml = generate_clash_yaml(&user(), &nodes, &opts());
        for t in ["vless", "vmess", "ss", "trojan", "hysteria2", "tuic", "anytls"] {
            assert!(yaml.contains(&format!("    type: {t}")), "缺 type: {t}\n{yaml}");
        }
        // 八个节点各出现在 proxies、节点选择、自动选择三处。
        //
        // 按**整行**匹配,不是裸子串:节点密钥是随机生成的,一个 base64 公钥里
        // 恰好含 "n2" 的概率不低 —— 那会让这条断言随机失败。
        for i in 0..8 {
            let in_proxies = yaml.matches(&format!("  - name: n{i}\n")).count();
            let in_groups = yaml.matches(&format!("      - n{i}\n")).count();
            assert_eq!(in_proxies, 1, "n{i} 应在 proxies 里出现一次\n{yaml}");
            assert_eq!(in_groups, 2, "n{i} 应在两个 group 里各出现一次\n{yaml}");
        }
    }

    /// 没有可导出节点时也要产出**结构合法**的 YAML:
    /// `proxies:` 后面直接跟 `proxy-groups:` 会让 clash 解析失败。
    #[test]
    fn clash_yaml_with_no_nodes_is_still_valid() {
        let yaml = generate_clash_yaml(&user(), &[], &opts());
        assert!(yaml.contains("proxies:\n  []"), "{yaml}");
    }

    /// YAML 标量里的特殊字符要加引号,否则一个带冒号的 tag 会把结构撑坏。
    #[test]
    fn yaml_quotes_special_characters() {
        assert_eq!(yaml_str("plain"), "plain");
        assert_eq!(yaml_str("a:b"), "\"a:b\"");
        assert_eq!(yaml_str("a\"b"), "\"a\\\"b\"");
        assert_eq!(yaml_str(""), "\"\"");
        // 开头的指示符会改变结构:`- foo` 像列表项。
        assert_eq!(yaml_str("-foo"), "\"-foo\"");
        assert_eq!(yaml_str(" pad"), "\" pad\"");
        // base64url 的 `-` / `_` 出现在中间是安全的,不该被无谓地加引号。
        assert_eq!(yaml_str("abc-def_gh"), "abc-def_gh");

        let mut n = node(Protocol::Trojan);
        n.tag = "tokyo: #1".into();
        let yaml = generate_clash_yaml(&user(), &[n], &opts());
        assert!(yaml.contains("- name: \"tokyo: #1\""), "{yaml}");
    }

    /// Clash 侧也要对自签证书跳过校验,口径与分享链接一致。
    #[test]
    fn clash_skips_cert_verify_for_self_signed_protocols() {
        for proto in [Protocol::Trojan, Protocol::Hysteria2, Protocol::Tuic, Protocol::Anytls] {
            let yaml = generate_clash_yaml(&user(), &[node(proto)], &opts());
            assert!(yaml.contains("skip-cert-verify: true"), "{proto}:\n{yaml}");
        }
        let yaml = generate_clash_yaml(&user(), &[node(Protocol::VlessReality)], &opts());
        assert!(!yaml.contains("skip-cert-verify"), "reality 不该跳过校验:\n{yaml}");
    }

    /// 公钥进 YAML、私钥绝不进。
    ///
    /// 比对时要走 `yaml_str`,不能直接拼原值:reality 公钥是 **base64url**
    /// (字母表含 `-` 和 `_`),64 分之一的概率首字符是 `-`,而 `-` 开头的标量
    /// 会被 `yaml_str` 加引号 —— 于是 `public-key: -abc` 变成 `public-key: "-abc"`,
    /// 拼原值的断言就挂了。
    ///
    /// 这正是它**偶发**的原因:每次跑都现生成一对新密钥,大多数时候首字符
    /// 不是 `-`,测试就绿。产品行为是对的(加引号更安全),错的是这条断言。
    #[test]
    fn clash_reality_carries_public_key_not_private() {
        let n = node(Protocol::VlessReality);
        let yaml = generate_clash_yaml(&user(), std::slice::from_ref(&n), &opts());
        let pbk = n.params.public_key.clone().unwrap();
        assert!(
            yaml.contains(&format!("public-key: {}", yaml_str(&pbk))),
            "公钥没进 YAML(pbk={pbk}):
{yaml}"
        );
        assert!(!yaml.contains(n.params.private_key.as_deref().unwrap()), "私钥泄进了 YAML");
    }

    /// 把上面那条的**偶发分支**钉死:公钥首字符是 `-` 时必须加引号。
    ///
    /// 不钉的话,这一支平均 64 次才走到一次 —— 等于没被测过。
    #[test]
    fn a_public_key_starting_with_a_dash_gets_quoted() {
        let mut n = node(Protocol::VlessReality);
        n.params.public_key = Some("-AbCdEf0123456789_-".into());
        let yaml = generate_clash_yaml(&user(), std::slice::from_ref(&n), &opts());
        assert!(
            yaml.contains(r#"public-key: "-AbCdEf0123456789_-""#),
            "`-` 开头的公钥必须加引号,否则 YAML 语义可能被读成别的东西:
{yaml}"
        );
    }

    /// IPv6 地址在 YAML 里必须加引号。
    ///
    /// `server: [2001:db8::1]` 是**流式序列**语法 —— clash 会把它解析成一个列表,
    /// 不报错,只是连不上。这条是从真实输出里发现的回归锚点。
    #[test]
    fn clash_quotes_ipv6_server_addresses() {
        let mut n = node(Protocol::Hysteria2);
        n.params.ipv6 = true;
        let yaml = generate_clash_yaml(&user(), &[n], &opts());
        assert!(
            yaml.contains("server: \"[2001:db8::1]\""),
            "IPv6 地址没加引号,会被当成 YAML 列表:\n{yaml}"
        );
        // IPv4 不需要引号,别过度加。
        let yaml4 = generate_clash_yaml(&user(), &[node(Protocol::Hysteria2)], &opts());
        assert!(yaml4.contains("server: 203.0.113.7"), "{yaml4}");
    }

    /// SNI 只在节点显式配了 server_name 时才出现。
    ///
    /// 拿连接地址兜底会在 IPv6 场景下生成 `sni=%5B2001%3Adb8%3A%3A1%5D` ——
    /// 一个语法上就不可能是域名的 SNI。
    #[test]
    fn sni_is_omitted_when_the_node_has_no_server_name() {
        // hysteria2 的 secrets::fill 不设 server_name(§9.1)。
        let n = node(Protocol::Hysteria2);
        assert!(n.params.server_name.is_none(), "前提:hy2 没有 server_name");

        let link = &generate_links(&user(), std::slice::from_ref(&n), &opts())[0].link;
        assert!(!link.contains("sni="), "不该凭空造一个 sni: {link}");
        assert!(link.contains("insecure=1"), "{link}");
        // query 串不能以 `?&` 开头。
        assert!(!link.contains("?&"), "query 串拼坏了: {link}");

        let yaml = generate_clash_yaml(&user(), &[n], &opts());
        assert!(!yaml.contains("sni:"), "{yaml}");

        // 配了 server_name 的协议照常带 sni。
        let t = node(Protocol::Trojan);
        let link = &generate_links(&user(), &[t], &opts())[0].link;
        assert!(link.contains("sni=bing.com"), "{link}");
    }

    // ─────────────── 查库 ───────────────

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-sub-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    /// 一个用户的订阅 = 它在 user_nodes 里的**全部节点,跨 agent 拼在一起**(§10)。
    #[tokio::test]
    async fn export_gathers_nodes_across_agents() {
        let p = pool().await;
        let mut ids = Vec::new();
        for (i, name) in ["tokyo", "osaka"].iter().enumerate() {
            let (agent_id, _) = crate::db::agent_repo::create(&p, name, 0).await.unwrap();
            sqlx::query("UPDATE agents SET ipv4 = ? WHERE id = ?")
                .bind(format!("203.0.113.{}", i + 1))
                .bind(agent_id)
                .execute(&p)
                .await
                .unwrap();
            let mut params = NodeParams::default();
            crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
            let (node_id, _) = crate::db::node_repo::add_node(
                &p,
                agent_id,
                &format!("{name}-in"),
                Protocol::VlessReality,
                8443,
                &params,
            )
            .await
            .unwrap();
            ids.push(node_id);
        }
        let uid = crate::db::node_repo::add_user(&p, "alice", 0, 0).await.unwrap();
        for id in &ids {
            crate::db::node_repo::assign_node(&p, uid, *id).await.unwrap();
        }

        let nodes = export_nodes(&p, uid).await.unwrap();
        assert_eq!(nodes.len(), 2, "两台 agent 上的节点都该在");
        assert_eq!(nodes[0].agent_ipv4.as_deref(), Some("203.0.113.1"));
        assert_eq!(nodes[1].agent_ipv4.as_deref(), Some("203.0.113.2"));

        // 每个节点用它**自己那台 agent** 的地址,不是共用一个。
        let u = SubUser { name: "alice".into(), uuid: "u".into(), password: "p".into() };
        let links = generate_links(&u, &nodes, &opts());
        assert!(links[0].link.contains("203.0.113.1:8443"));
        assert!(links[1].link.contains("203.0.113.2:8443"));
    }

    /// 没分配给该用户的节点不该出现在他的订阅里。
    #[tokio::test]
    async fn export_is_scoped_to_the_user() {
        let p = pool().await;
        let (agent_id, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let mut params = NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
        let (mine, _) = crate::db::node_repo::add_node(&p, agent_id, "mine", Protocol::VlessReality, 1, &params)
            .await
            .unwrap();
        crate::db::node_repo::add_node(&p, agent_id, "theirs", Protocol::VlessReality, 2, &params)
            .await
            .unwrap();

        let uid = crate::db::node_repo::add_user(&p, "alice", 0, 0).await.unwrap();
        crate::db::node_repo::assign_node(&p, uid, mine).await.unwrap();

        let nodes = export_nodes(&p, uid).await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].tag, "mine");
    }

    #[tokio::test]
    async fn sub_token_lookup_finds_the_right_user() {
        let p = pool().await;
        crate::db::node_repo::add_user(&p, "alice", 0, 0).await.unwrap();
        let token: String = sqlx::query_scalar("SELECT sub_token FROM users WHERE name = 'alice'")
            .fetch_one(&p)
            .await
            .unwrap();

        let (_, u) = find_by_sub_token(&p, &token).await.unwrap().expect("应当找到");
        assert_eq!(u.name, "alice");
        assert!(find_by_sub_token(&p, "no-such-token").await.unwrap().is_none());
    }
}
