//! 从库里组装下发给 agent 的东西(DESIGN.md §4.2 / §6.3)。
//!
//! 这一层只做「查库 → 组 JSON → 推进 revision」,不碰 WebSocket——
//! 下发由 `cluster::rpc` 负责。
//!
//! 刻意写成一组平铺的函数而不是 `struct XxxService`:这些函数之间没有共享状态
//! (每个都只需要一个 `&SqlitePool`),包成 struct 只会多一层 `self.` 而不带来任何东西。
//!
//! 八个协议的 inbound 生成都在这里(§9.1)。密钥材料不在这一层生成 ——
//! 那是 `secrets::fill` 在**建节点时**做的一次性动作,结果存进 `nodes.params_json`。
//! 分开是因为这个函数会被反复调用(每次 config.apply 都走一遍),
//! 在这里生成等于每次下发都换一套密钥。

use crate::model::node::Protocol;
use crate::secrets;
use anyhow::Result;
use sqlx::SqlitePool;

/// 组装某台 agent 的完整 sing-box 配置(`config.apply` 的 `options` 字段)。
///
/// 一台 agent 的配置 = 它名下所有节点各成一个 inbound,每个 inbound 的用户列表
/// 来自 `user_nodes` 里分配给该节点的用户。
///
/// **只包含 `enabled = 1` 的用户吗?不。** 被禁用的用户仍然留在 inbound 里,
/// 由 agent 侧 tracker 的 `disabled` 集合在数据路径上拒绝(§7.5)。
/// 这是刻意的:把禁用做成配置变更会让每次配额超限都触发一次 box 重建,
/// 而那是全系统触发最频繁的自动化路径(§6.3)。
pub async fn build_agent_config(pool: &SqlitePool, agent_id: i64) -> Result<serde_json::Value> {
    let nodes: Vec<(i64, String, String, i64, String)> = sqlx::query_as(
        "SELECT id, tag, protocol, listen_port, params_json
           FROM nodes WHERE agent_id = ? ORDER BY id",
    )
    .bind(agent_id)
    .fetch_all(pool)
    .await?;

    let mut inbounds = Vec::with_capacity(nodes.len());
    for (node_id, tag, proto, port, params_json) in nodes {
        let users: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT u.name, u.uuid, u.password
               FROM users u JOIN user_nodes un ON un.user_id = u.id
              WHERE un.node_id = ? ORDER BY u.id",
        )
        .bind(node_id)
        .fetch_all(pool)
        .await?;

        let params: serde_json::Value =
            serde_json::from_str(&params_json).unwrap_or(serde_json::Value::Null);

        inbounds.push(build_inbound(&tag, Protocol::parse(&proto), port, &params, &users)?);
    }

    let mut cfg = serde_json::json!({
        "log": { "level": "warn" },
        "inbounds": inbounds,
        // 出站只要 direct。agent 是落地机,不做链式代理——
        // 中转由 §10 的 RelaySetting 在订阅侧处理,不在 sing-box 配置里。
        "outbounds": [{ "type": "direct", "tag": "direct" }],
    });

    // 自定义片段先并入,**出站策略最后叠**。顺序不能反:
    // `outbound::apply` 里的 `ensure_dns_server` 是「已经有带 tag 的 server 就复用」,
    // 自定义先到位的话它会复用人写的那个,不会插第二个 DNS server。
    // 反过来就会凭空多一个 `local`。
    let custom = crate::db::agent_repo::custom_config(pool, agent_id).await?;
    if let Some(raw) = &custom {
        // 库里的内容存之前已经校验过。这里**再错也不能把整台机器的组装拘死** ——
        // 库可能被手改过,而组装失败意味着这台再也收不到任何配置。
        // 跟 `outbound_strategy` 那一列同一个口径:读不懂就当没有,并留一条 warn。
        match validate_custom(raw) {
            Ok(obj) => merge_custom(&mut cfg, obj),
            Err(e) => tracing::warn!(
                agent_id,
                error = %e,
                "库里的自定义配置读不懂,本次组装当它不存在"
            ),
        }
    }

    // 出站地址族策略。写的是 `route.default_domain_resolver` 而**不是**
    // 已被 1.14.0 移除的 `domain_strategy`(见 model/outbound.rs)。
    // 认不出来的取值退回 Auto —— 库里那一列可能被手改过,而让整台机器的
    // 配置组装失败远比退回默认行为要糟。
    let strategy: String = sqlx::query_scalar("SELECT outbound_strategy FROM agents WHERE id = ?")
        .bind(agent_id)
        .fetch_optional(pool)
        .await?
        .unwrap_or_default();
    crate::model::outbound::apply(
        &mut cfg,
        crate::model::outbound::OutboundStrategy::parse(&strategy),
    );

    Ok(cfg)
}

/// 组一个 inbound(§9.1,八个协议)。
///
/// 缺密钥材料时**不兜底、不现生成**,直接返回 `Err`:
///   * 现生成会让每次 `config.apply` 都换一套密钥,客户端全部失联而且没有报错;
///   * 生成一个残缺的 inbound 会让 agent 的 `box.New()` 失败,回来的错误
///     指向「配置无效」,查起来离真正的原因(建节点时没生成密钥)很远。
///
/// `users` 是 `(name, uuid, password)`,由 `build_agent_config` 查好传进来。
fn build_inbound(
    tag: &str,
    proto: Protocol,
    port: i64,
    params: &serde_json::Value,
    users: &[(String, String, String)],
) -> Result<serde_json::Value> {
    let p = |k: &str| params.get(k).and_then(|v| v.as_str()).unwrap_or("");

    // 取一个必需的参数,缺了就明确报出是哪个节点缺哪个字段。
    let need = |k: &str| -> Result<String> {
        let v = p(k);
        if v.is_empty() {
            anyhow::bail!("节点 {tag}({proto})的 params 里缺 {k};建节点时应由 secrets::fill 生成");
        }
        Ok(v.to_string())
    };

    match proto {
        Protocol::VlessReality => {
            let server_name = need("server_name")?;
            Ok(serde_json::json!({
                "type": "vless",
                "tag": tag,
                "listen": "::",
                "listen_port": port,
                "users": vless_users(users, true),
                "tls": {
                    "enabled": true,
                    "server_name": server_name,
                    "reality": {
                        "enabled": true,
                        // handshake.server 跟 server_name 一致:两者不同的话,
                        // 客户端按 SNI 连过来、reality 却去握另一个站,特征反而更明显。
                        "handshake": { "server": server_name, "server_port": 443 },
                        // **public_key 不在这里。** sing-box 的 reality inbound
                        // 没有这个字段,它只属于客户端侧(订阅链接)。
                        "private_key": need("private_key")?,
                        "short_id": [need("short_id")?],
                    }
                }
            }))
        }

        // vless-ws / vmess-ws 默认**不启用 TLS**:正常部署会在前面挂 nginx/caddy
        // 做 TLS 终结,后端 ws 走明文。要后端直连 TLS 得另配证书,
        // 那是与「前置反代」互斥的一种部署,不该由默认值替用户选。
        Protocol::VlessWs => Ok(serde_json::json!({
            "type": "vless",
            "tag": tag,
            "listen": "::",
            "listen_port": port,
            "users": vless_users(users, false),
            "transport": ws_transport(p("path"), "/vless"),
        })),

        Protocol::VmessWs => Ok(serde_json::json!({
            "type": "vmess",
            "tag": tag,
            "listen": "::",
            "listen_port": port,
            "users": users.iter()
                .map(|(name, uuid, _)| serde_json::json!({
                    "name": name, "uuid": uuid, "alterId": 0
                }))
                .collect::<Vec<_>>(),
            "transport": ws_transport(p("path"), "/vmess"),
        })),

        Protocol::Shadowsocks => {
            let method = {
                let m = p("ss_method");
                if m.is_empty() {
                    secrets::SS_DEFAULT_METHOD
                } else {
                    m
                }
            };
            Ok(serde_json::json!({
                "type": "shadowsocks",
                "tag": tag,
                "listen": "::",
                "listen_port": port,
                "method": method,
                // 服务端密钥与每个用户各自的密钥是两回事:2022 系列方法里
                // 两者共同参与派生,少了服务端这把,所有用户都连不上。
                "password": need("ss_password")?,
                "users": users.iter()
                    .map(|(name, uuid, _)| serde_json::json!({
                        "name": name,
                        "password": secrets::ss_user_password(uuid),
                    }))
                    .collect::<Vec<_>>(),
            }))
        }

        Protocol::Trojan => Ok(serde_json::json!({
            "type": "trojan",
            "tag": tag,
            "listen": "::",
            "listen_port": port,
            "users": password_users(users),
            "tls": tls_block(&need("cert_pem")?, &need("key_pem")?, Some(&need("server_name")?), &[]),
        })),

        Protocol::Hysteria2 => Ok(serde_json::json!({
            "type": "hysteria2",
            "tag": tag,
            "listen": "::",
            "listen_port": port,
            "users": password_users(users),
            // hy2 的 inbound 不带 server_name(官方示例亦无),客户端的 sni 由订阅决定。
            "tls": tls_block(&need("cert_pem")?, &need("key_pem")?, None, &["h3"]),
        })),

        Protocol::Tuic => Ok(serde_json::json!({
            "type": "tuic",
            "tag": tag,
            "listen": "::",
            "listen_port": port,
            // tuic 的用户同时要 uuid 和 password,两者缺一不可。
            "users": users.iter()
                .map(|(name, uuid, pw)| serde_json::json!({
                    "name": name, "uuid": uuid, "password": pw
                }))
                .collect::<Vec<_>>(),
            "congestion_control": "bbr",
            "tls": tls_block(&need("cert_pem")?, &need("key_pem")?, Some(&need("server_name")?), &["h3"]),
        })),

        Protocol::Anytls => Ok(serde_json::json!({
            "type": "anytls",
            "tag": tag,
            "listen": "::",
            "listen_port": port,
            "users": password_users(users),
            // 空的 padding_scheme = 用内置默认方案。显式写出来是为了让
            // 「这里可以调」在配置里看得见。
            "padding_scheme": [],
            "tls": tls_block(&need("cert_pem")?, &need("key_pem")?, Some(&need("server_name")?), &["h2", "http/1.1"]),
        })),

        Protocol::Unknown => {
            // 不给未知协议生成兜底 inbound:曾经这里落成 direct,
            // 一个打错的协议名会静默生成一个开放的直连入站。
            anyhow::bail!("节点 {tag} 的协议无法识别,拒绝生成配置")
        }
    }
}

/// vless 的用户项。`flow` 只在 reality 下给 —— 无 TLS 的 ws 传输上带 vision
/// 会让握手失败,而报错信息与 flow 毫无关系。
fn vless_users(users: &[(String, String, String)], reality: bool) -> Vec<serde_json::Value> {
    users
        .iter()
        .map(|(name, uuid, _)| {
            let mut v = serde_json::json!({ "name": name, "uuid": uuid });
            if reality {
                v["flow"] = serde_json::Value::String("xtls-rprx-vision".into());
            }
            v
        })
        .collect()
}

/// trojan / hysteria2 / anytls 共用的用户项:名字 + 密码。
fn password_users(users: &[(String, String, String)]) -> Vec<serde_json::Value> {
    users.iter().map(|(name, _, pw)| serde_json::json!({ "name": name, "password": pw })).collect()
}

fn ws_transport(path: &str, default_path: &str) -> serde_json::Value {
    let path = if path.is_empty() { default_path } else { path };
    serde_json::json!({
        "type": "ws",
        "path": path,
        // 0-RTT 数据走 Sec-WebSocket-Protocol 头。这两项要与客户端侧一致,
        // 订阅生成(§10)必须带上同样的值,否则首包会被当成协议协商失败。
        "max_early_data": 2048,
        "early_data_header_name": "Sec-WebSocket-Protocol",
    })
}

/// 内联证书的 TLS 块。
///
/// 用 `certificate` / `key`(PEM 内容)而不是 `certificate_path` / `key_path`:
/// 主控与 agent 不在同一台机器上,路径在这边有意义、在那边没有。
/// sing-box 这两个字段都是 `Listable[string]`,读的时候用 "\n" 拼起来,
/// 所以直接塞整段 PEM 就行(`option/tls.go:22` / `common/tls/std_server.go:406`)。
fn tls_block(
    cert_pem: &str,
    key_pem: &str,
    server_name: Option<&str>,
    alpn: &[&str],
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "enabled": true,
        "certificate": cert_pem,
        "key": key_pem,
    });
    if let Some(sni) = server_name {
        v["server_name"] = serde_json::Value::String(sni.to_string());
    }
    if !alpn.is_empty() {
        v["alpn"] = serde_json::json!(alpn);
    }
    v
}

/// 全量禁用用户名单(`user.state` 的 `disabled` 字段)。
///
/// **全量而不是增量**(§4.2):幂等,丢一条消息不会让状态永久偏移。
///
/// 注意这是**全局**名单,不按 agent 过滤。多传几个该 agent 上不存在的用户名是无害的
/// (tracker 只在 `md.User` 命中时才查这个集合),而按 agent 过滤要多一次 JOIN
/// 且会在「用户刚被分配到新 agent」的瞬间产生一个不一致窗口。
pub async fn disabled_users(pool: &SqlitePool) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT name FROM users WHERE enabled = 0 ORDER BY name")
        .fetch_all(pool)
        .await?)
}

// ── 自定义配置片段(§1.2 / 迁移 012)─────────────────────────────

/// 自定义片段**只能**碰这几个顶层 key。
///
/// `inbounds` 不在这里,而且不是为了「少改点东西」:记账键是 (用户, inbound tag)
/// (§14)。改一个 tag,`ingest_stats` 会把上报当成「主控没给这个用户分配过的
/// (user, tag)」**直接丢弃** —— 流量静默停止记账,一句报错都没有。
/// 那是最坏的一类 bug,所以它得靠代码拦,不靠文档提醒。
///
/// `http_clients` 与 `experimental` 是后来补的,两个都是**远程 rule-set 真跑起来
/// 就需要**的:
///   * sing-box 1.14 把下载通道改成了顶层 `http_clients` + `route.default_http_client`,
///     旧的 `download_detour` 已弃用,而「隐式用默认出站下载」也已弃用 ——
///     也就是说不写 `http_clients` 就没有不带弃用警告的写法了;
///   * `experimental.cache_file` 不开的话远程 rule-set 不缓存,而 `config.apply` 每次都
///     重建 box —— 每改一次配置就重下一次全部规则集,慢且会失败。
const CUSTOM_ALLOWED_KEYS: &[&str] =
    &["log", "outbounds", "route", "dns", "http_clients", "experimental"];

/// `experimental` 里这两个子项不行:它们会开一个 HTTP 管理端口。
///
/// 与「agent 不开放管理端口,管理面只有 agent 主动连主控的 WebSocket」直接冲突 ——
/// 而且 clash_api 默认不带鉴权,相当于在落地机上开一个能改路由的匿名接口。
const CUSTOM_FORBIDDEN_EXPERIMENTAL: &[&str] = &["clash_api", "v2ray_api"];

/// 主控自己组的那个出站 tag。自定义里再出一个同名的,sing-box 会报重复 tag,
/// 而那句错误看不出是自己写的还是主控加的 —— 所以在这里先拦下来。
const DIRECT_TAG: &str = "direct";

/// 把 JSONC 剥成 serde_json 能吃的 JSON:去注释、去尾随逗号。
///
/// **为什么需要它。** sing-box 的解析器接受 `//`、`#`、`/* */` 和尾随逗号
/// (实测确认过),而主控侧的 serde_json 严格按 RFC 8259 —— 两个不一样的
/// 接受集。既然 sing-box 接受,人就一定会写注释;而库里存的是原文(迁移 012),
/// 所以组装时得先过这一道。
///
/// **不动字符串里的东西。** 这是全部难处:一个 `path` 里的 `//`、一个域名里的 `#`
/// 都不是注释。写法照 `db::split_sql` 处理 `''` 转义的那个状态机 ——
/// 正则在这个问题上是错的,而不是不好看。
pub fn strip_jsonc(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            // 转义序列整个抬过去。少了这一步,`"a\\"` 会被当成字符串没结束。
            if c == '\\' {
                if let Some(&n) = b.get(i + 1) {
                    out.push(n);
                    i += 2;
                    continue;
                }
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match (c, b.get(i + 1)) {
            ('"', _) => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            // 行注释:`//` 与 `#` 都到行尾。**换行要留着** —— 丢了的话
            // serde_json 报的行号全错位,而那个行号是人回编辑器里找错的唯一依据。
            ('/', Some('/')) | ('#', _) => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while i < b.len() {
                    if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                        i += 2;
                        break;
                    }
                    if b[i] == '\n' {
                        out.push('\n'); // 同上:保行号
                    }
                    i += 1;
                }
            }
            // 单个 `/` 不是注释开头。JSON 里它不合法,但**不在这里吞** ——
            // 吞了之后 serde_json 报的错误位置就对不上原文了。
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    strip_trailing_commas(&out)
}

/// 去掉 `}` / `]` 前面的逗号。单独一趟是因为它要**往后**看空白,
/// 而剥注释那一趟是往前看的。两件事搃在一个循环里只会让两边都难读。
fn strip_trailing_commas(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut in_str = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(&n) = b.get(i + 1) {
                    out.push(n);
                    i += 2;
                    continue;
                }
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
        } else if c == ',' {
            match b[i + 1..].iter().find(|c| !c.is_whitespace()) {
                Some('}') | Some(']') => {
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 校验一份自定义片段能不能存,返回解析好的对象。
///
/// 这一层只拦**主控能看出来的**错:JSON 语法、不允许的 key、tag 撞 `direct`。
/// 字段名拼错、类型不对、`route` 引用了不存在的 outbound tag 这些拦不了 ——
/// 主控里没有 sing-box(§0.3 结论一)。那一层由 `config.check` 守(TUI 的 `[K]`)。
pub fn validate_custom(raw: &str) -> Result<serde_json::Map<String, serde_json::Value>> {
    let stripped = strip_jsonc(raw);
    if stripped.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    let v: serde_json::Value = serde_json::from_str(&stripped)
        .map_err(|e| anyhow::anyhow!("不是合法 JSON:{e}(注释与尾随逗号是允许的)"))?;
    let serde_json::Value::Object(obj) = v else {
        anyhow::bail!("最外层得是一个对象 `{{ … }}`");
    };
    for k in obj.keys() {
        if k == "inbounds" {
            anyhow::bail!(
                "不能在这里改 inbounds —— 记账键是 (用户, inbound tag),\
                 改了 tag 流量会静默停止记账。节点请在「节点」页里加减"
            );
        }
        if !CUSTOM_ALLOWED_KEYS.contains(&k.as_str()) {
            // `endpoints` 是真字段,但本仓库的 agent 没编 wireguard。不说清的话
            // 人会以为是白名单小气,而真相是就算放进去也跑不起来。
            if k == "endpoints" {
                anyhow::bail!(
                    "endpoints 用不了:agent 的 sing-box 只带 with_quic,with_utls 两个 build tag,\
                     没编进 WireGuard(它会报 `WireGuard is not included in this build`)。\n\
                     要接 WARP 这类落地,得先给 agent 加 with_wireguard 重新构建"
                );
            }
            anyhow::bail!("不允许的顶层字段 `{k}`。只能写:{}", CUSTOM_ALLOWED_KEYS.join(" / "));
        }
    }
    // `log.output` 拒掉。agent 的 sing-box 日志本来就进进程的 stderr → journald,
    // 那里有现成的轮转;写成文件后没人给它转圈 —— 而这台机子上改成 info
    // 以后是**每条连接一行**,磁盘满只是时间问题。而且路径不在
    // StateDirectory 下的话,ProtectSystem=strict 下只读,跟 cache_file 同一类坑。
    if let Some(out) = obj.get("log").and_then(|v| v.get("output")) {
        anyhow::bail!(
            "log.output 用不了(你写的是 {out})—— agent 的 sing-box 日志已经进 journald,\
             `journalctl -u sbx-agent -f` 就能看;写成文件没人给它转圈,\
             而改成 info 以后是每条连接一行。只写 level 就行"
        );
    }
    // `cache_file` 开着却不写 `path`:sing-box 的缺省是**相对路径** `cache.db`
    // (落在工作目录),而 agent 的 systemd unit 是 ProtectSystem=strict ——
    // 整棵文件系统只读,只有 StateDirectory(默认 /var/lib/sbx-agent)可写。
    // 相对路径在 unit 里必炸,而且炸在 `Start` 而不是 `[K]` 的 Check
    // (box.New() 不碰磁盘) —— 表现是主控每轮巡检重试、错误刷屏。
    // 在存盘这一步拦下,把一处运行期的无限重试变成眼前的一句话。
    if let Some(cf) =
        obj.get("experimental").and_then(|v| v.get("cache_file")).and_then(|v| v.as_object())
    {
        let enabled = cf.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let path = cf.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
        if enabled && path.is_empty() {
            anyhow::bail!(
                "cache_file 开着就得写 path —— sing-box 的缺省是相对路径 cache.db,\
                 在 agent 的 systemd unit(ProtectSystem=strict)下落在只读区,必然启动失败。\
                 写 \"path\": \"/var/lib/sbx-agent/cache.db\"(agent 的 StateDirectory;\
                 改过 agent 的 state_dir 就换成那个目录)"
            );
        }
    }
    // `experimental` 整体放开了(cache_file 对远程 rule-set 是必需的),
    // 但不能靠它开管理端口。
    if let Some(exp) = obj.get("experimental").and_then(|v| v.as_object()) {
        for bad in CUSTOM_FORBIDDEN_EXPERIMENTAL {
            if exp.contains_key(*bad) {
                anyhow::bail!(
                    "experimental.{bad} 不行 —— 它会在这台机器上开一个 HTTP 管理端口,\
                     而 agent 的设计是不开管理端口、管理面只走它主动连主控的 WebSocket。\n\
                     experimental 里用 cache_file 就行(远程 rule-set 靠它缓存)"
                );
            }
        }
    }
    if let Some(arr) = obj.get("outbounds").and_then(|v| v.as_array()) {
        for o in arr {
            if o.get("tag").and_then(|t| t.as_str()) == Some(DIRECT_TAG) {
                anyhow::bail!(
                    "outbound tag `{DIRECT_TAG}` 已经被主控用掉了,换个名字 —— \
                     重名时 sing-box 报的错看不出是哪一边加的"
                );
            }
        }
    }
    if detours_to_master_direct(&serde_json::Value::Object(obj.clone())) {
        anyhow::bail!(
            "detour 不能指向 direct —— 主控那个 direct 出站是空配置(没配任何拨号选项),\
             sing-box 对「显式绕道一个空 direct」会报 `detour to an empty direct outbound\
             makes no sense`,而且只在 Start 阶段炸(【K】的 box.New 查不出来)。\
             想走默认就把 detour 整个去掉(缺省走 box 自己的直连,行为一样);\
             指自己加的出站没问题 —— 只要它配了拨号选项(domain_resolver 也算),\
             比如带 domain_resolver 的 direct-v6 就是合法目标;selector 之类不参与这个检查"
        );
    }
    Ok(obj)
}

/// 全文递归找 `"detour": "direct"`。
///
/// 不限定挂在哪一层:`outbounds` / `dns.servers` / `http_clients` 里都可能有 detour,
/// 白名单只管顶层键,里面怎么嵌是 sing-box 的事。tag 叫 `direct` 的出站只能是主控的
/// (自己想叫这个名会在更早的地方被拒),所以值等于 `"direct"` 时指向必是那个空出站。
fn detours_to_master_direct(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(m) => m.iter().any(|(k, val)| {
            (k == "detour" && val.as_str() == Some(DIRECT_TAG)) || detours_to_master_direct(val)
        }),
        serde_json::Value::Array(a) => a.iter().any(detours_to_master_direct),
        _ => false,
    }
}

/// 把自定义片段并入一份已组好的配置。
///
/// 三个 key 三种并法,各有理由:
///   * `outbounds` —— **追加**。主控那个 `direct` 必须留着、而且必须留在第一位:
///     节点默认走直连,`route` 里没匀到的流量靠它;而且主控从不写
///     `route.final`,sing-box 在 final 缺省时把第一个出站当默认出站
///     (adapter/outbound/manager.go:303)。所以往前插会让人写的第一个出站静默当上
///     默认出口,覆盖式写法则把它弄没 —— 两者表现都不是报错。
///   * `route` / `dns` —— **逐 key 并入**,里层同名字段以自定义为准。
///     整个替换会把后面 `outbound::apply` 要往里面塞的东西一起抹掉。
///   * `log` —— 同样逐 key 并入。主控写的是 `{ "level": "warn" }`,人只写
///     `{ "level": "info" }` 时只盖掉 level,以后主控往 log 里加别的字段也不会被抹。
fn merge_custom(cfg: &mut serde_json::Value, custom: serde_json::Map<String, serde_json::Value>) {
    let Some(root) = cfg.as_object_mut() else {
        return;
    };
    for (k, v) in custom {
        match k.as_str() {
            "outbounds" => {
                if let (Some(dst), Some(add)) =
                    (root.get_mut("outbounds").and_then(|o| o.as_array_mut()), v.as_array())
                {
                    dst.extend(add.iter().cloned());
                }
            }
            // 主控自己不写这两个,直接放进去就行 —— 而且它们是数组/对象两种形状,
            // 跟着下面那条「逐 key 并入」走会靠一个 fallthrough 担着,很难看出意图。
            "http_clients" | "experimental" => {
                root.insert(k, v);
            }
            // `route` / `dns` 主控也要往里面写(出站策略、DNS server tag),
            // 所以逐 key 并入而不是整个替换。
            _ => {
                let slot = root
                    .entry(k)
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                match (slot.as_object_mut(), v.as_object()) {
                    (Some(dst), Some(src)) => {
                        for (ik, iv) in src {
                            dst.insert(ik.clone(), iv.clone());
                        }
                    }
                    // 人把 `route` 写成了数组或字符串之类。原样放进去,
                    // 让 `config.check` 里的真 sing-box 去报 —— 它的错误比这里能给的清楚。
                    _ => {
                        *slot = v;
                    }
                }
            }
        }
    }
}

// 这里曾经有 bump_config_revision / bump_user_state_revision /
// bump_all_user_state_revisions 三个辅助函数。**删掉了,不要加回来。**
//
// 推进 revision 必须与引起变更的那条写操作在**同一个事务**里
// (`db::node_repo` 的每个写函数都是这么做的,`supervisor::sweep` 也是)。
// 一个只拿 `&SqlitePool` 的独立辅助函数做不到这一点:调用方会写成
// 「先 add_node,再 bump」,中间崩一次就留下一个配置变了但 revision 没动的库 ——
// 那台 agent 会一直以为自己是最新的,直到有人手工改数据库。
//
// 换句话说,这三个函数不只是没人用,它们的签名本身就是错的。

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-svc-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    /// **字符串里的 `//` 和 `#` 不是注释。**
    ///
    /// 这是剥注释唯一真正难的地方,也是用正则一定会错的地方。
    /// 真实配置里到处是 `https://…`、ws 的 `path`、带 `#` 的域名规则 ——
    /// 切错一个的后果是一份看起来没问题的配置您默少了一段。
    #[test]
    fn stripping_comments_leaves_strings_alone() {
        let src = r#"{
            // 真注释
            "a": "https://example.com/x", # 也是真注释
            "b": "tag#frag",
            "c": "/* 不是注释 */",
            "d": "反斜杠结尾\\",
            /* 块
               注释 */
            "e": 1,
        }"#;
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(src)).expect("该能解析");
        assert_eq!(v["a"], "https://example.com/x", "URL 里的 // 被当成注释了");
        assert_eq!(v["b"], "tag#frag", "字符串里的 # 被当成注释了");
        assert_eq!(v["c"], "/* 不是注释 */");
        assert_eq!(v["d"], "反斜杠结尾\\", "转义序列没抬对,字符串提前结束了");
        assert_eq!(v["e"], 1, "尾随逗号没去掉");
    }

    /// 剥注释**不能改行号**。
    ///
    /// 人拿到的报错是 `serde_json` 的 `line N`,而他要回编辑器里找那一行。
    /// 把注释行整行删掉的话,行号会整体前移,报错指向一个无关的位置。
    #[test]
    fn stripping_comments_keeps_line_numbers() {
        let src = "{\n// a\n// b\n/* c\n   d */\n\"x\": 1\n}";
        assert_eq!(
            src.lines().count(),
            strip_jsonc(src).lines().count(),
            "行数变了 —— serde_json 报的行号就指不到原文了"
        );
    }

    /// **`inbounds` 必须被拒绝,而且要说清理由。**
    ///
    /// 记账键是 (用户, inbound tag)(§14)。改了 tag,`ingest_stats` 会把上报当成
    /// 「主控没分配过的 (user, tag)」直接丢弃 —— 流量静默停止记账。
    /// 只说「不允许」的话人会以为是个无聊的限制,继而去手改库。
    #[test]
    fn custom_config_may_not_touch_inbounds() {
        let e = validate_custom(r#"{ "inbounds": [] }"#).unwrap_err().to_string();
        assert!(e.contains("inbounds"), "{e}");
        assert!(e.contains("记账"), "要说出为什么不允许:{e}");
    }

    #[test]
    fn custom_config_only_allows_the_whitelisted_keys() {
        for ok in [
            r#"{ "outbounds": [] }"#,
            r#"{ "route": {} }"#,
            r#"{ "dns": {} }"#,
            // 1.14 把远程 rule-set 的下载通道改成了顶层 `http_clients`,
            // 旧的 `download_detour` 已弃用 —— 不放它进来,就没有不带弃用警告的写法。
            // 注意这里不能带 `"detour": "direct"`:见 detour_may_not_point_at_the_master_direct。
            r#"{ "http_clients": [{ "tag": "hc" }] }"#,
            // 不开 cache_file 的话远程 rule-set 不缓存,而 `config.apply` 每次都重建 box
            // —— 每改一次配置就重下一次全部规则集。
            r#"{ "experimental": { "cache_file": { "enabled": true, "path": "/var/lib/sbx-agent/cache.db" } } }"#,
            // `log` 开放于 v0.4.34。主控钉的是 warn,而想看“哪个域名走了哪个出站”
            // 就得要 info 的路由日志 —— agent 不开 clash_api,日志是那台机器上唯一的观察窗。
            r#"{ "log": { "level": "info" } }"#,
            "",
            "   \n // 只有注释 \n ",
        ] {
            assert!(validate_custom(ok).is_ok(), "该接受:{ok:?}");
        }
        for bad in [r#"{ "ntp": {} }"#, r#"{ "certificate": {} }"#] {
            assert!(validate_custom(bad).is_err(), "该拒绝:{bad}");
        }
        // 最外层必须是对象 —— 人很容易直接粘一个 `outbounds` 数组进来。
        assert!(validate_custom("[]").is_err(), "数组该被拒");
    }

    /// **detour 不能指向主控的 direct(空出站)。**
    ///
    /// sing-box 1.14 起,「显式绕道一个空 direct 出站」是个启动期错误
    /// (`detour to an empty direct outbound makes no sense`)——旧的
    /// `download_detour` 写法内部豁免了这个检查,新 `http_clients` 不豁免,
    /// 于是 1.13 能跑的写法到 1.14 会炸。而且炸在 Start(远程 rule-set 首次
    /// 下载时),`[K]` 的 box.New 查不出来 —— 又一处「存得进去、起不起来」,
    /// 只能在存盘时拦。
    #[test]
    fn detour_may_not_point_at_the_master_direct() {
        // 三处常见挂点都得拦:不止 http_clients,dns.servers 和 outbounds 里也有 detour。
        for bad in [
            r#"{ "http_clients": [{ "tag": "hc", "detour": "direct" }] }"#,
            r#"{ "dns": { "servers": [{ "type": "udp", "tag": "d", "detour": "direct" }] } }"#,
            r#"{ "outbounds": [{ "type": "selector", "tag": "sel", "detour": "direct" }] }"#,
        ] {
            let e = validate_custom(bad).unwrap_err().to_string();
            assert!(e.contains("direct"), "要说清指错了谁:{e}");
            assert!(e.contains("去掉"), "要给出修法:{e}");
        }
        // 指向自己加的出站不拦:只要配了拨号选项就不算空,而 domain_resolver 就在
        // DialerOptions 里(见 agent 侧 TestDetourEmptyDirectVersusDomainResolver);
        // 完全不写 detour 也不拦(缺省走 box 自己的直连,没有这个检查)。
        validate_custom(r#"{ "http_clients": [{ "tag": "hc", "detour": "direct-v6" }] }"#).unwrap();
    }

    /// **日志级别可以改,但 `log.output` 不行。**
    ///
    /// 改级别是真需求:主控钉的是 `warn`,而想确认“某个域名到底走了哪个出站”
    /// 就得看 info 的路由日志 —— 而 agent 不开 clash_api(不在落地机开管理端口),
    /// 日志是那台机器上唯一的观察窗。并法得是逐 key 盖:只写 level 不能把主控
    /// 以后可能往 `log` 里加的字段抹掉。
    #[test]
    fn log_level_can_be_overridden_but_not_the_output_path() {
        let obj = validate_custom(r#"{ "log": { "level": "info" } }"#).unwrap();
        assert_eq!(obj["log"]["level"], "info");

        let e = validate_custom(r#"{ "log": { "level": "info", "output": "/var/log/box.log" } }"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("journalctl"), "要告诉人日志在哪看:{e}");
    }

    /// **`cache_file` 开着却不写 `path` 必须在存盘时拦下。**
    ///
    /// sing-box 的缺省路径是相对的 `cache.db`,而 agent 的 systemd unit 是
    /// ProtectSystem=strict(整棵树只读,只有 StateDirectory 可写)—— 这份配置
    /// 一路能过 `[K]`(box.New 不碰磁盘)、能过下发,然后在 agent 的 Start 上
    /// 炸成 `read-only file system`,主控从此每轮巡检重试。v0.4.29 的模板就
    /// 带过这么一份,真机上撞了个正着。
    #[test]
    fn cache_file_enabled_requires_a_path() {
        let e = validate_custom(r#"{ "experimental": { "cache_file": { "enabled": true } } }"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("path"), "要给出修法:{e}");
        assert!(e.contains("/var/lib/sbx-agent"), "要说清写到哪:{e}");

        // 写了就放行;enabled 不开(或写 false)时 path 无关紧要,不强制。
        validate_custom(
            r#"{ "experimental": { "cache_file": { "enabled": true, "path": "/var/lib/sbx-agent/cache.db" } } }"#,
        )
        .unwrap();
        validate_custom(r#"{ "experimental": { "cache_file": { "enabled": false } } }"#).unwrap();
        validate_custom(r#"{ "experimental": { "cache_file": {} } }"#).unwrap();
    }

    /// **`experimental` 放进来了,但不能靠它开管理端口。**
    ///
    /// `clash_api` 默认不带鉴权,相当于在落地机上开一个能改路由的匿名接口 ——
    /// 而「agent 不开放管理端口,管理面只有它主动连主控的 WebSocket」是硬约束。
    /// 报错要同时告诉人 `cache_file` 是可以用的,否则他会以为整个 experimental 都不行。
    #[test]
    fn experimental_may_not_open_a_management_port() {
        for bad in [
            r#"{ "experimental": { "clash_api": { "external_controller": "127.0.0.1:9090" } } }"#,
            r#"{ "experimental": { "v2ray_api": { "listen": "127.0.0.1:8080" } } }"#,
        ] {
            let e = validate_custom(bad).unwrap_err().to_string();
            assert!(e.contains("管理端口"), "要说出理由:{e}");
            assert!(e.contains("cache_file"), "要说清 experimental 里什么能用:{e}");
        }
    }

    /// **`endpoints` 要说清是构建里没编 WireGuard,不是白名单小气。**
    ///
    /// 它是 sing-box 的真字段(1.11 之后接 WARP 就走它),所以人会写。但 agent 只带
    /// `with_quic,with_utls` 两个 build tag —— 就算放进去,sing-box 也会报
    /// `WireGuard is not included in this build`。实测确认过。
    ///
    /// 一句「不允许的顶层字段」会让人去想办法绕过白名单(比如手改库),
    /// 而那条路的尽头是一样的报错。
    #[test]
    fn endpoints_explains_the_missing_build_tag() {
        let e = validate_custom(r#"{ "endpoints": [{ "type": "wireguard" }] }"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("WireGuard"), "{e}");
        assert!(e.contains("with_wireguard"), "要给出真正的出路:{e}");
    }

    /// 自定义里再出一个 `direct` 要当场拦住。
    ///
    /// 不拦的后果不是“不能用”而是“错误位置不对”:sing-box 会报重复 tag,
    /// 而那句话看不出重复的那一个是人写的还是主控加的。
    #[test]
    fn custom_config_may_not_shadow_the_direct_outbound() {
        let e = validate_custom(r#"{ "outbounds": [{ "type": "direct", "tag": "direct" }] }"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("direct"), "{e}");
        // 别的 tag 当然可以。
        assert!(
            validate_custom(r#"{ "outbounds": [{ "type": "direct", "tag": "warp" }] }"#).is_ok()
        );
    }

    /// **自定义的 outbound 是追加,`direct` 必须还在。**
    ///
    /// 节点默认走直连,`route` 里没匀到的流量也靠它。抹掉 `direct` 的表现不是报错,
    /// 而是「部分流量无法出站」—— 那种故障会被归因到完全无关的地方。
    #[tokio::test]
    async fn a_custom_outbound_is_appended_not_replacing_direct() {
        let p = pool().await;
        let (agent_id, _) = agent_with_node(&p, "vless-reality").await;
        crate::db::agent_repo::set_custom_config(
            &p,
            agent_id,
            Some(
                r#"{
                    // 走 warp
                    "outbounds": [{ "type": "direct", "tag": "warp" }],
                    "route": { "rules": [{ "domain_suffix": [".openai.com"], "outbound": "warp" }] },
                }"#,
            ),
        )
        .await
        .unwrap();

        let cfg = build_agent_config(&p, agent_id).await.unwrap();
        let tags: Vec<&str> =
            cfg["outbounds"].as_array().unwrap().iter().filter_map(|o| o["tag"].as_str()).collect();
        assert!(tags.contains(&"direct"), "主控的 direct 被抹掉了:{tags:?}");
        assert!(tags.contains(&"warp"), "自定义的 outbound 没并进去:{tags:?}");
        // **主控的 direct 必须仍在第一位。**不是美观问题:主控从不写
        // `route.final`,而 sing-box 在 final 缺省时把**第一个出站**当默认出站
        // (adapter/outbound/manager.go:303)。哪天这里从 extend 改成往前插,人写的
        // 第一个自定义出站就会**静默地**变成这台机器的默认出口 ——
        // 没规则匹配的流量全走它,一句报错都没有。
        assert_eq!(tags[0], "direct", "direct 得排第一 —— final 缺省时它就是默认出站:{tags:?}");
        assert!(cfg["route"]["rules"].is_array(), "route.rules 没并进去:{cfg}");
        // inbounds 仍然是主控算出来的那一份。
        assert_eq!(cfg["inbounds"].as_array().unwrap().len(), 1);
        assert_eq!(cfg["inbounds"][0]["tag"], "in-1");
    }

    /// **自定义里的 `log.level` 要盖掉主控那个 `warn`。**
    ///
    /// `log` 是主控自己先写了的 key(不像 `http_clients` 那样只有人写),
    /// 所以并法必须是**逐 key 盖**而不是追加也不是整个替换:
    /// 盖不上就成了“界面收下了、机器上还是 warn”—— 人会去查一个根本没变的日志。
    #[tokio::test]
    async fn a_custom_log_level_overrides_the_master_default() {
        let p = pool().await;
        let (agent_id, _) = agent_with_node(&p, "vless-ws").await;
        crate::db::agent_repo::set_custom_config(
            &p,
            agent_id,
            Some(r#"{ "log": { "level": "info" } }"#),
        )
        .await
        .unwrap();

        let cfg = build_agent_config(&p, agent_id).await.unwrap();
        assert_eq!(cfg["log"]["level"], "info", "主控的 warn 没被盖掉:{cfg}");
    }

    /// **自定义写了 `default_domain_resolver` 时,`[o]` 的策略让位。**
    ///
    /// 两边写的是同一个字段 —— `[o]` 本质上就是自定义配置的一个预设。
    /// 无条件覆盖会静默盖掉人手写的那份,而界面上 `[o]` 还显示着一个
    /// 看上去生效的值。让位之后由 `has_custom_resolver` 告知界面。
    #[tokio::test]
    async fn a_custom_resolver_wins_over_the_outbound_strategy() {
        let p = pool().await;
        let (agent_id, _) = agent_with_node(&p, "vless-reality").await;
        crate::db::agent_repo::set_outbound_strategy(
            &p,
            agent_id,
            crate::model::outbound::OutboundStrategy::Ipv4Only,
        )
        .await
        .unwrap();

        // 先确认没有自定义时策略确实写进去了 —— 否则下面那条断言测了个寂寞。
        let cfg = build_agent_config(&p, agent_id).await.unwrap();
        assert_eq!(cfg["route"]["default_domain_resolver"]["strategy"], "ipv4_only");

        crate::db::agent_repo::set_custom_config(
            &p,
            agent_id,
            Some(
                r#"{ "dns": { "servers": [{ "type": "udp", "tag": "mine", "server": "1.1.1.1" }] },
                     "route": { "default_domain_resolver": { "server": "mine", "strategy": "prefer_ipv6" } } }"#,
            ),
        )
        .await
        .unwrap();

        let cfg = build_agent_config(&p, agent_id).await.unwrap();
        assert_eq!(
            cfg["route"]["default_domain_resolver"]["strategy"], "prefer_ipv6",
            "策略把人手写的 resolver 盖掉了:{cfg}"
        );
        assert_eq!(cfg["route"]["default_domain_resolver"]["server"], "mine");
        // 且不能凭空多插一个 `local` DNS server。
        let dns_tags: Vec<&str> = cfg["dns"]["servers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["tag"].as_str())
            .collect();
        assert_eq!(dns_tags, vec!["mine"], "多插了 DNS server:{dns_tags:?}");
    }

    /// 恢复默认 = 存 `None`。库里回到 NULL,组装结果逐字节等于从没设过。
    #[tokio::test]
    async fn clearing_the_custom_config_restores_the_default_byte_for_byte() {
        let p = pool().await;
        let (agent_id, _) = agent_with_node(&p, "vless-reality").await;
        let before = build_agent_config(&p, agent_id).await.unwrap();

        crate::db::agent_repo::set_custom_config(
            &p,
            agent_id,
            Some(r#"{ "outbounds": [{ "type": "direct", "tag": "warp" }] }"#),
        )
        .await
        .unwrap();
        assert_ne!(before, build_agent_config(&p, agent_id).await.unwrap(), "该变了");

        crate::db::agent_repo::set_custom_config(&p, agent_id, None).await.unwrap();
        assert_eq!(before, build_agent_config(&p, agent_id).await.unwrap(), "恢复得不完全");
        assert!(crate::db::agent_repo::custom_config(&p, agent_id).await.unwrap().is_none());
    }

    /// 存进去的是**原文**,注释要能原样读回来。
    ///
    /// 下次打开编辑器时那些注释必须还在 —— 否则「解释这条规则为何存在」
    /// 的信息每存一次丢一次,而那恰好是自定义路由里最难重建的部分。
    #[tokio::test]
    async fn the_stored_custom_config_keeps_its_comments() {
        let p = pool().await;
        let (agent_id, _) = agent_with_node(&p, "vless-reality").await;
        let raw = "{\n  // 这条是 2026-09 为了绕开机房封锁加的\n  \"outbounds\": []\n}";
        crate::db::agent_repo::set_custom_config(&p, agent_id, Some(raw)).await.unwrap();
        assert_eq!(
            crate::db::agent_repo::custom_config(&p, agent_id).await.unwrap().as_deref(),
            Some(raw),
            "存回来的不是原文"
        );
    }

    /// 修改自定义配置要推进 `config_revision`,否则在线的机器永远不会收到它。
    #[tokio::test]
    async fn changing_the_custom_config_bumps_the_revision() {
        let p = pool().await;
        let (agent_id, _) = agent_with_node(&p, "vless-reality").await;
        let rev0: i64 = sqlx::query_scalar("SELECT config_revision FROM agents WHERE id = ?")
            .bind(agent_id)
            .fetch_one(&p)
            .await
            .unwrap();
        let rev1 =
            crate::db::agent_repo::set_custom_config(&p, agent_id, Some("{}")).await.unwrap();
        assert!(rev1 > rev0, "{rev0} -> {rev1}");
        let rev2 = crate::db::agent_repo::set_custom_config(&p, agent_id, None).await.unwrap();
        assert!(rev2 > rev1, "清空也是一次配置变更:{rev1} -> {rev2}");
    }

    /// 造一台 agent + 一个 vless-reality 节点,返回 (agent_id, node_id)。
    async fn agent_with_node(p: &SqlitePool, proto: &str) -> (i64, i64) {
        let (agent_id, _) = crate::db::agent_repo::create(p, "a", 0).await.unwrap();
        let params = serde_json::json!({
            "server_name": "www.example.com",
            "private_key": "PRIV",
            "short_id": "abcd"
        });
        let node_id: i64 = sqlx::query_scalar(
            "INSERT INTO nodes (agent_id, tag, protocol, listen_port, params_json)
             VALUES (?, 'in-1', ?, 8443, ?) RETURNING id",
        )
        .bind(agent_id)
        .bind(proto)
        .bind(params.to_string())
        .fetch_one(p)
        .await
        .unwrap();
        (agent_id, node_id)
    }

    async fn add_user(p: &SqlitePool, name: &str, node_id: Option<i64>, enabled: bool) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO users (name, uuid, password, sub_token, enabled, created_at)
             VALUES (?, ?, 'pw', ?, ?, 0) RETURNING id",
        )
        .bind(name)
        .bind(format!("uuid-{name}"))
        .bind(format!("tok-{name}"))
        .bind(enabled as i64)
        .fetch_one(p)
        .await
        .unwrap();
        if let Some(n) = node_id {
            sqlx::query("INSERT INTO user_nodes (user_id, node_id) VALUES (?, ?)")
                .bind(id)
                .bind(n)
                .execute(p)
                .await
                .unwrap();
        }
        id
    }

    /// 出站策略要真的落进下发给 agent 的那份配置里。
    ///
    /// 这条走的是**完整组装路径**(建 agent → 改策略 → build_agent_config),
    /// 而不是只测 `model::outbound::apply`。中间任何一环忘了读那一列,
    /// 界面上都会显示「已改」而机器上什么都没发生。
    #[tokio::test]
    async fn the_outbound_strategy_reaches_the_generated_config() {
        use crate::model::outbound::OutboundStrategy;
        let pool = pool().await;
        let (agent_id, _) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        // 默认 auto:不该出现 route/dns。
        let cfg = build_agent_config(&pool, agent_id).await.unwrap();
        assert!(cfg.get("route").is_none(), "auto 不该写 route:{cfg}");
        assert!(cfg.get("dns").is_none(), "auto 不该写 dns:{cfg}");

        crate::db::agent_repo::set_outbound_strategy(&pool, agent_id, OutboundStrategy::PreferIpv6)
            .await
            .unwrap();
        let cfg = build_agent_config(&pool, agent_id).await.unwrap();
        assert_eq!(cfg["route"]["default_domain_resolver"]["strategy"], "prefer_ipv6");
        // resolver 指向的 tag 必须真的存在,否则 sing-box 起不来。
        let tag = cfg["route"]["default_domain_resolver"]["server"].as_str().unwrap();
        assert!(cfg["dns"]["servers"].as_array().unwrap().iter().any(|s| s["tag"] == tag), "{cfg}");
        // 1.14.0 已移除的那个字段,一个字都不能出现。
        assert!(!cfg.to_string().contains("domain_strategy"), "{cfg}");
    }

    #[tokio::test]
    async fn config_contains_one_inbound_per_node_with_its_users() {
        let p = pool().await;
        let (agent_id, node_id) = agent_with_node(&p, "vless-reality").await;
        add_user(&p, "alice", Some(node_id), true).await;
        add_user(&p, "bob", Some(node_id), true).await;

        let cfg = build_agent_config(&p, agent_id).await.unwrap();
        let inbounds = cfg["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 1);
        assert_eq!(inbounds[0]["tag"], "in-1");
        assert_eq!(inbounds[0]["type"], "vless");
        assert_eq!(inbounds[0]["listen_port"], 8443);

        let users = inbounds[0]["users"].as_array().unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0]["name"], "alice");
        assert_eq!(users[0]["uuid"], "uuid-alice");

        // reality 参数应从 params_json 取出
        assert_eq!(inbounds[0]["tls"]["reality"]["private_key"], "PRIV");
        assert_eq!(inbounds[0]["tls"]["server_name"], "www.example.com");
        assert_eq!(cfg["outbounds"][0]["type"], "direct");
    }

    /// **被禁用的用户仍然留在配置里**(§7.5)。
    /// 禁用走 tracker 的内存标记,不走配置变更——否则每次配额超限都会重建 box。
    #[tokio::test]
    async fn disabled_users_remain_in_the_inbound() {
        let p = pool().await;
        let (agent_id, node_id) = agent_with_node(&p, "vless-reality").await;
        add_user(&p, "alice", Some(node_id), true).await;
        add_user(&p, "banned", Some(node_id), false).await;

        let cfg = build_agent_config(&p, agent_id).await.unwrap();
        let users = cfg["inbounds"][0]["users"].as_array().unwrap();
        assert_eq!(users.len(), 2, "禁用用户不该从配置里消失(§7.5)");
    }

    /// 没有分配任何用户的节点仍然要生成 inbound(端口要监听着),
    /// 只是 users 为空。
    #[tokio::test]
    async fn node_without_users_still_yields_an_inbound() {
        let p = pool().await;
        let (agent_id, _) = agent_with_node(&p, "vless-reality").await;
        let cfg = build_agent_config(&p, agent_id).await.unwrap();
        assert_eq!(cfg["inbounds"].as_array().unwrap().len(), 1);
        assert_eq!(cfg["inbounds"][0]["users"].as_array().unwrap().len(), 0);
    }

    /// 别的 agent 的节点不该出现在本 agent 的配置里。
    #[tokio::test]
    async fn config_is_scoped_to_one_agent() {
        let p = pool().await;
        let (a1, n1) = agent_with_node(&p, "vless-reality").await;
        let (a2, _) = crate::db::agent_repo::create(&p, "b", 0).await.unwrap();
        sqlx::query(
            "INSERT INTO nodes (agent_id, tag, protocol, listen_port, params_json)
             VALUES (?, 'other', 'vless-reality', 9443, '{}')",
        )
        .bind(a2)
        .execute(&p)
        .await
        .unwrap();
        add_user(&p, "alice", Some(n1), true).await;

        let cfg = build_agent_config(&p, a1).await.unwrap();
        let inbounds = cfg["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 1);
        assert_eq!(inbounds[0]["tag"], "in-1", "不该包含另一台 agent 的节点");
    }

    /// 缺密钥材料时应当在**主控侧**明确失败,而不是下发一个残缺配置
    /// 让 agent 的 box.New 报一条难以归因的错。
    #[tokio::test]
    async fn missing_key_material_fails_with_a_clear_message() {
        let p = pool().await;
        // agent_with_node 塞的是 reality 的参数,trojan 需要的是证书 —— 必然缺。
        let (agent_id, _) = agent_with_node(&p, "trojan").await;
        let err = build_agent_config(&p, agent_id).await.unwrap_err().to_string();
        assert!(err.contains("in-1"), "错误信息应指出是哪个节点: {err}");
        assert!(err.contains("trojan"), "应指出是哪个协议: {err}");
        assert!(err.contains("cert_pem"), "应指出缺的是哪个字段: {err}");
    }

    #[tokio::test]
    async fn unknown_protocol_is_rejected() {
        let p = pool().await;
        let (agent_id, _) = agent_with_node(&p, "wireguard-plus").await;
        assert!(build_agent_config(&p, agent_id).await.is_err());
    }

    #[tokio::test]
    async fn disabled_list_has_only_disabled_users_sorted() {
        let p = pool().await;
        add_user(&p, "zoe", None, false).await;
        add_user(&p, "alice", None, true).await;
        add_user(&p, "bob", None, false).await;

        assert_eq!(disabled_users(&p).await.unwrap(), vec!["bob", "zoe"]);
    }

    #[tokio::test]
    async fn empty_disabled_list_is_not_an_error() {
        let p = pool().await;
        add_user(&p, "alice", None, true).await;
        assert!(disabled_users(&p).await.unwrap().is_empty());
    }

    // ───────────────── 八协议:golden 配置(§9.1)─────────────────
    //
    // 这组测试把每个协议的 inbound 落到 `master/testdata/inbounds/<proto>.json`,
    // 由 **agent 侧的 Go 测试**(`agent/boxctl` 的 TestMasterGoldenConfigsAreAccepted)
    // 喂给真正的 sing-box 校验。
    //
    // 为什么要跨语言对一遍:主控生成配置、agent 消费配置,两边对字段名和结构的理解
    // 只在运行时才碰面。少一个必填字段、多一个 sing-box 不认的字段(比如把 reality 的
    // public_key 写进 inbound),表现都是「agent 回一条 config.apply 失败」——
    // 而那时节点已经在线上了。这组 golden 把那个时刻提前到 CI。
    //
    // 输入固定(密钥材料是写死的常量 + 一份提交进仓库的自签证书),
    // 所以输出是确定的,可以逐字节比对。

    fn testdata_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
    }

    /// 固定的 reality 私钥(43 字符 base64url-nopad),仅用于测试。
    const FIXTURE_REALITY_PRIV: &str = "SDNoQP-vFjV1FQ7lQFBhE_-8mZBnkvHnBBWEqhOpU2s";
    const FIXTURE_SHORT_ID: &str = "0123abcd";
    const FIXTURE_SS_PASSWORD: &str = "YWJjZGVmZ2hpamtsbW5vcA==";

    /// 读取(首次运行时生成并写入)测试用的自签证书。
    ///
    /// 证书本身带随机序列号和签发时间,每次生成都不同 —— 所以生成一次就提交进仓库,
    /// 之后一直读它。否则 golden 文件每跑一次测试就变一次,diff 里全是噪声。
    fn fixture_cert() -> (String, String) {
        let dir = testdata_dir();
        let cert_path = dir.join("fixture-cert.pem");
        let key_path = dir.join("fixture-key.pem");
        if let (Ok(c), Ok(k)) =
            (std::fs::read_to_string(&cert_path), std::fs::read_to_string(&key_path))
        {
            return (c, k);
        }
        std::fs::create_dir_all(&dir).unwrap();
        let (c, k) = crate::secrets::self_signed("golden.example.com").unwrap();
        std::fs::write(&cert_path, &c).unwrap();
        std::fs::write(&key_path, &k).unwrap();
        (c, k)
    }

    fn fixture_params(proto: Protocol) -> serde_json::Value {
        let (cert, key) = fixture_cert();
        let mut p = crate::model::node::NodeParams::default();
        // 先让 secrets 填默认值(server_name / path / ss_method 之类),
        // 再把随机的那几项换成固定值 —— 这样 golden 覆盖的是真实的默认值组合。
        crate::secrets::fill(proto, &mut p).unwrap();
        p.private_key = p.private_key.map(|_| FIXTURE_REALITY_PRIV.to_string());
        p.public_key = None; // 不该出现在 inbound 里,这里也不给它机会
        p.short_id = p.short_id.map(|_| FIXTURE_SHORT_ID.to_string());
        p.ss_password = p.ss_password.map(|_| FIXTURE_SS_PASSWORD.to_string());
        p.cert_pem = p.cert_pem.map(|_| cert.clone());
        p.key_pem = p.key_pem.map(|_| key.clone());
        serde_json::to_value(p).unwrap()
    }

    fn golden_users() -> Vec<(String, String, String)> {
        vec![
            (
                "alice".to_string(),
                "b831381d-6324-4d53-ad4f-8cda48b30811".to_string(),
                "alice-password".to_string(),
            ),
            (
                "bob".to_string(),
                "0f2d7b4a-1c3e-4a5b-8d9f-2e1a3b4c5d6e".to_string(),
                "bob-password".to_string(),
            ),
        ]
    }

    /// 生成八个协议的 golden 配置并与仓库里的文件比对。
    ///
    /// 文件不存在就写一份(首次引入协议时的引导路径);存在但不一致则**失败**,
    /// 同时把新结果写成 `<proto>.json.actual` 方便 diff。
    /// 不静默覆盖 —— 静默覆盖等于这组测试永远通过。
    #[test]
    fn eight_protocols_match_golden_configs() {
        let dir = testdata_dir().join("inbounds");
        std::fs::create_dir_all(&dir).unwrap();
        let users = golden_users();
        let mut drift = Vec::new();

        for &proto in Protocol::all() {
            let params = fixture_params(proto);
            let inbound = build_inbound("node-1", proto, 8443, &params, &users)
                .unwrap_or_else(|e| panic!("{proto} 生成失败: {e}"));
            let pretty = serde_json::to_string_pretty(&inbound).unwrap() + "\n";

            let path = dir.join(format!("{proto}.json"));
            match std::fs::read_to_string(&path) {
                Ok(existing) if existing.replace("\r\n", "\n") == pretty => {}
                Ok(_) => {
                    std::fs::write(path.with_extension("json.actual"), &pretty).unwrap();
                    drift.push(proto.as_str());
                }
                Err(_) => std::fs::write(&path, &pretty).unwrap(),
            }
        }

        assert!(
            drift.is_empty(),
            "以下协议的生成结果与 testdata/inbounds/ 里的 golden 不一致:{drift:?}\n\
             确认改动是有意的之后,用同目录下的 .actual 覆盖 .json 并提交。\n\
             注意 agent 侧的 Go 测试会拿这些文件喂真正的 sing-box —— 改完记得跑一遍。"
        );
    }

    /// 五个出站策略各落一份**完整配置**到 `testdata/outbound/`,由 agent 侧的
    /// Go 测试喂给真正的 sing-box 校验(`TestOutboundStrategyConfigsAreAccepted`)。
    ///
    /// 这组 golden 与上面那组的区别是**范围**:上面只有 inbound,而出站策略写的是
    /// 顶层的 `route` / `dns`,不进 inbound。少了这一组,`default_domain_resolver`
    /// 的字段名、`{ "type": "local" }` 的新式写法、以及 server tag 指得到不到,
    /// 全都要等到线上 `config.apply` 失败才知道 —— 而那时候整台机器的代理已经停了。
    ///
    /// 尤其要盯 1.14.0 移除的 `domain_strategy`:写它的表现不是「不生效」,
    /// 是 `box.New()` 直接失败,而错误只说「配置无效」。
    /// **自定义片段合并后的结果也要过真 sing-box。**
    ///
    /// 这一组 golden 守的是前两组都盖不到的东西:合并**本身**。
    /// 人写的 `route.rules` 里引用了一个自定义 outbound tag,而那个 tag 必须真的
    /// 追加进了 `outbounds` —— 合并写错一行(比如把 `outbounds` 覆盖而不是追加),
    /// 表现就是 `box.New()` 报一个指不到的 tag。而那时候这台机器已经在线上了。
    ///
    /// 写的是 `build_agent_config` 的真实输出(含真节点),不是手拼的片段 ——
    /// 要验的就是那条组装路径。
    #[tokio::test]
    async fn a_merged_custom_config_matches_its_golden() {
        let p = pool().await;
        let (agent_id, _) = agent_with_node(&p, "vless-ws").await;
        crate::db::agent_repo::set_outbound_strategy(
            &p,
            agent_id,
            crate::model::outbound::OutboundStrategy::PreferIpv4,
        )
        .await
        .unwrap();
        crate::db::agent_repo::set_custom_config(
            &p,
            agent_id,
            Some(
                r#"{
                    "outbounds": [
                        { "type": "socks", "tag": "upstream", "server": "127.0.0.1", "server_port": 1080 }
                    ],
                    "route": {
                        "rules": [
                            { "domain_suffix": [".openai.com", ".claude.ai"], "outbound": "upstream" }
                        ]
                    },
                }"#,
            ),
        )
        .await
        .unwrap();

        let cfg = build_agent_config(&p, agent_id).await.unwrap();
        let pretty = serde_json::to_string_pretty(&cfg).unwrap() + "\n";

        let dir = testdata_dir().join("custom");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("outbound-and-route.json");
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing.replace("\r\n", "\n") == pretty => {}
            Ok(_) => {
                std::fs::write(path.with_extension("json.actual"), &pretty).unwrap();
                panic!(
                    "合并结果与 testdata/custom/ 里的 golden 不一致。\n\
                     确认改动是有意的之后,用同目录下的 .actual 覆盖 .json 并提交。\n\
                     agent 侧的 Go 测试会拿它喂真正的 sing-box —— 改完记得跑一遍。"
                );
            }
            Err(_) => std::fs::write(&path, &pretty).unwrap(),
        }

        // 顺带钉两条不靠 golden 的性质:引用得到的 tag、且 direct 还在。
        let tags: Vec<&str> =
            cfg["outbounds"].as_array().unwrap().iter().filter_map(|o| o["tag"].as_str()).collect();
        assert!(tags.contains(&"direct") && tags.contains(&"upstream"), "{tags:?}");
        let rule_out = cfg["route"]["rules"][0]["outbound"].as_str().unwrap();
        assert!(tags.contains(&rule_out), "route 引用的 {rule_out} 不在 outbounds 里");
    }

    /// **人真会写的那种形状:远程 rule-set + http_clients + cache_file。**
    ///
    /// 与上一条 golden 的分工:那一条验的是 `outbounds` 追加与 `route` 并入;
    /// 这一条验的是 **1.14 那套新字段能不能穿过整条链** —— `http_clients` 与
    /// `experimental` 不走 `route`/`dns` 那条逐 key 并入的路(它们是数组/对象两种形状),
    /// 而且它们是后来才补进白名单的。
    ///
    /// `rule_set.tag` 写成数组 + url 里用 `{tag}` 占位符是 1.14 的写法 ——
    /// 这个形状也要真 sing-box 点头,光看文档不算。
    #[tokio::test]
    async fn a_remote_ruleset_config_matches_its_golden() {
        let p = pool().await;
        // 用 vless-ws 而不是 reality:**golden 必须是确定的**。reality 节点的私钥是
        // 建节点时随机生成的(§9.1),写进 golden 之后下一次跑就对不上了。
        // 第一版这么写了,表现是 Go 侧报 `invalid private key` —— 正好被
        // 跨语言那一步拓住了。本条要验的是合并,跟协议无关。
        let (agent_id, _) = agent_with_node(&p, "vless-ws").await;
        crate::db::agent_repo::set_custom_config(
            &p,
            agent_id,
            Some(
                r#"{
  // 让 AI 站点走 IPv6 出去
  "dns": { "servers": [{ "type": "local", "tag": "local" }] },
  "http_clients": [{ "tag": "fetch" }],
  "outbounds": [
    { "type": "direct", "tag": "direct-v6",
      "domain_resolver": { "server": "local", "strategy": "prefer_ipv6" } }
  ],
  "route": {
    "default_http_client": "fetch",
    "rule_set": [
      { "tag": ["ai"], "type": "remote", "format": "binary",
        "url": "https://github.com/DustinWin/ruleset_geodata/releases/download/sing-box-ruleset/{tag}.srs",
        "http_client": "fetch" }
    ],
    "rules": [
      { "rule_set": ["ai"], "action": "route", "outbound": "direct-v6" }
    ],
    "final": "direct",
  },
  "experimental": { "cache_file": { "enabled": true, "path": "/var/lib/sbx-agent/cache.db" } },
}"#,
            ),
        )
        .await
        .unwrap();

        let cfg = build_agent_config(&p, agent_id).await.unwrap();
        let pretty = serde_json::to_string_pretty(&cfg).unwrap() + "\n";

        let dir = testdata_dir().join("custom");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("remote-ruleset.json");
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing.replace("\r\n", "\n") == pretty => {}
            Ok(_) => {
                std::fs::write(path.with_extension("json.actual"), &pretty).unwrap();
                panic!(
                    "合并结果与 testdata/custom/remote-ruleset.json 不一致。\n\
                     确认改动是有意的之后,用同目录下的 .actual 覆盖 .json 并提交。"
                );
            }
            Err(_) => std::fs::write(&path, &pretty).unwrap(),
        }

        // 不靠 golden 的两条:两个新字段真的到位了,而不是被静默丢掉。
        assert_eq!(cfg["http_clients"][0]["tag"], "fetch", "http_clients 丢了:{cfg}");
        assert_eq!(cfg["experimental"]["cache_file"]["enabled"], true, "cache_file 丢了:{cfg}");
    }

    #[test]
    fn outbound_strategies_match_golden_configs() {
        use crate::model::outbound::{apply, OutboundStrategy};

        let dir = testdata_dir().join("outbound");
        std::fs::create_dir_all(&dir).unwrap();
        let mut drift = Vec::new();

        for &st in OutboundStrategy::all() {
            // 一份最小但**完整**的配置:sing-box 要能整份装配起来才算过。
            let mut cfg = serde_json::json!({
                "log": { "level": "error" },
                "inbounds": [],
                "outbounds": [{ "type": "direct", "tag": "direct" }],
            });
            apply(&mut cfg, st);
            let pretty = serde_json::to_string_pretty(&cfg).unwrap() + "\n";

            let path = dir.join(format!("{}.json", st.key()));
            match std::fs::read_to_string(&path) {
                Ok(existing) if existing.replace("\r\n", "\n") == pretty => {}
                Ok(_) => {
                    std::fs::write(path.with_extension("json.actual"), &pretty).unwrap();
                    drift.push(st.key());
                }
                Err(_) => std::fs::write(&path, &pretty).unwrap(),
            }
        }

        assert!(
            drift.is_empty(),
            "以下策略的生成结果与 testdata/outbound/ 里的 golden 不一致:{drift:?}\n\
             确认改动是有意的之后,用同目录下的 .actual 覆盖 .json 并提交。\n\
             agent 侧的 Go 测试会拿这些文件喂真正的 sing-box —— 改完记得跑一遍。"
        );
    }

    /// reality 的 public_key **不能**出现在 inbound 里:
    /// sing-box 的 reality inbound 没有这个字段,多给会被拒。
    #[test]
    fn reality_inbound_never_leaks_public_key() {
        let mut p = crate::model::node::NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut p).unwrap();
        let pub_key = p.public_key.clone().unwrap();
        let params = serde_json::to_value(&p).unwrap();

        let inbound =
            build_inbound("n", Protocol::VlessReality, 443, &params, &golden_users()).unwrap();
        let text = inbound.to_string();
        assert!(!text.contains(&pub_key), "public_key 泄进了 inbound: {text}");
        assert!(inbound["tls"]["reality"]["public_key"].is_null());
    }

    /// 只有 reality 的 vless 才带 flow —— 明文 ws 上带 vision 会握手失败,
    /// 而报错与 flow 毫无关系,极难归因。
    #[test]
    fn flow_is_only_set_for_reality() {
        let reality = build_inbound(
            "n",
            Protocol::VlessReality,
            443,
            &fixture_params(Protocol::VlessReality),
            &golden_users(),
        )
        .unwrap();
        assert_eq!(reality["users"][0]["flow"], "xtls-rprx-vision");

        let ws = build_inbound(
            "n",
            Protocol::VlessWs,
            443,
            &fixture_params(Protocol::VlessWs),
            &golden_users(),
        )
        .unwrap();
        assert!(ws["users"][0]["flow"].is_null(), "ws 上不该有 flow");
    }

    /// shadowsocks 2022:服务端密钥与每个用户的密钥是两套东西,都必须在。
    #[test]
    fn shadowsocks_has_both_server_and_per_user_keys() {
        let ib = build_inbound(
            "n",
            Protocol::Shadowsocks,
            443,
            &fixture_params(Protocol::Shadowsocks),
            &golden_users(),
        )
        .unwrap();
        assert_eq!(ib["password"], FIXTURE_SS_PASSWORD);
        assert_eq!(ib["method"], crate::secrets::SS_DEFAULT_METHOD);
        let u = ib["users"].as_array().unwrap();
        assert_eq!(u.len(), 2);
        // 用户密钥由 uuid 派生,必须是 base64(16B)。
        for entry in u {
            let pw = entry["password"].as_str().unwrap();
            use base64::Engine as _;
            assert_eq!(base64::engine::general_purpose::STANDARD.decode(pw).unwrap().len(), 16);
        }
    }

    /// TLS 系协议必须走**内联 PEM**,不能是路径 ——
    /// 主控和 agent 不在同一台机器上,路径在这边有意义、在那边没有。
    #[test]
    fn tls_protocols_inline_the_certificate_instead_of_a_path() {
        for proto in [Protocol::Trojan, Protocol::Tuic, Protocol::Anytls, Protocol::Hysteria2] {
            let ib =
                build_inbound("n", proto, 443, &fixture_params(proto), &golden_users()).unwrap();
            let tls = &ib["tls"];
            assert!(
                tls["certificate"].as_str().unwrap_or_default().contains("BEGIN CERTIFICATE"),
                "{proto} 的 certificate 不是内联 PEM"
            );
            assert!(tls["certificate_path"].is_null(), "{proto} 不该用 certificate_path");
            assert!(tls["key_path"].is_null(), "{proto} 不该用 key_path");
        }
    }

    /// 两个 revision 必须独立递增(§4.1)。
    ///
    /// 走的是**生产路径**(`node_repo` 的写函数),不是某个只有测试调用的辅助函数——
    /// 这条不变量的价值在于「真正改配置的那些操作各自推进对的那个计数器」。
    #[tokio::test]
    async fn revisions_advance_independently() {
        use crate::model::node::{NodeParams, Protocol};

        let p = pool().await;
        let (id, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        add_user(&p, "alice", None, true).await;

        // 加节点 → 只推进 config_revision。
        crate::db::node_repo::add_node(
            &p,
            id,
            "in-1",
            Protocol::VlessReality,
            8443,
            &NodeParams::default(),
        )
        .await
        .unwrap();
        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert_eq!(a.config_revision, 1);
        assert_eq!(a.user_state_revision, 0, "加节点不该动 user_state");

        // 停用用户 → 只推进 user_state_revision。
        crate::db::node_repo::set_user_enabled(&p, "alice", false).await.unwrap();
        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert_eq!(a.config_revision, 1, "停用用户不该动 config —— 它不重建 box(§4.2)");
        assert_eq!(a.user_state_revision, 1);
    }

    /// 禁用名单变化要让**所有** agent 的 user_state_revision 前进(§6.3),
    /// 包括离线的那些——它们重连时才补齐。
    #[tokio::test]
    async fn disabling_a_user_covers_every_agent_including_offline_ones() {
        let p = pool().await;
        let (a1, _) = crate::db::agent_repo::create(&p, "online", 0).await.unwrap();
        let (a2, _) = crate::db::agent_repo::create(&p, "offline", 0).await.unwrap();
        sqlx::query("UPDATE agents SET status = 'online' WHERE id = ?")
            .bind(a1)
            .execute(&p)
            .await
            .unwrap();
        add_user(&p, "alice", None, true).await;

        crate::db::node_repo::set_user_enabled(&p, "alice", false).await.unwrap();

        for id in [a1, a2] {
            let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
            assert_eq!(a.user_state_revision, 1, "agent {id} 的 revision 应前进");
        }
    }
}
