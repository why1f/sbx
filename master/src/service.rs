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

    Ok(serde_json::json!({
        "log": { "level": "warn" },
        "inbounds": inbounds,
        // 出站只要 direct。agent 是落地机,不做链式代理——
        // 中转由 §10 的 RelaySetting 在订阅侧处理,不在 sing-box 配置里。
        "outbounds": [{ "type": "direct", "tag": "direct" }],
    }))
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
                if m.is_empty() { secrets::SS_DEFAULT_METHOD } else { m }
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
    users
        .iter()
        .map(|(name, _, pw)| serde_json::json!({ "name": name, "password": pw }))
        .collect()
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
        if let (Ok(c), Ok(k)) = (std::fs::read_to_string(&cert_path), std::fs::read_to_string(&key_path)) {
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

    /// reality 的 public_key **不能**出现在 inbound 里:
    /// sing-box 的 reality inbound 没有这个字段,多给会被拒。
    #[test]
    fn reality_inbound_never_leaks_public_key() {
        let mut p = crate::model::node::NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut p).unwrap();
        let pub_key = p.public_key.clone().unwrap();
        let params = serde_json::to_value(&p).unwrap();

        let inbound = build_inbound("n", Protocol::VlessReality, 443, &params, &golden_users()).unwrap();
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
            assert_eq!(
                base64::engine::general_purpose::STANDARD.decode(pw).unwrap().len(),
                16
            );
        }
    }

    /// TLS 系协议必须走**内联 PEM**,不能是路径 ——
    /// 主控和 agent 不在同一台机器上,路径在这边有意义、在那边没有。
    #[test]
    fn tls_protocols_inline_the_certificate_instead_of_a_path() {
        for proto in [Protocol::Trojan, Protocol::Tuic, Protocol::Anytls, Protocol::Hysteria2] {
            let ib = build_inbound("n", proto, 443, &fixture_params(proto), &golden_users()).unwrap();
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
