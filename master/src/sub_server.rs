//! 订阅 HTTP 服务(DESIGN.md §10)。移植自旧项目 `service/sub_server.rs`。
//!
//! **这是 §2「不做 Web 面板」的唯一例外**,而且是刻意收窄的例外:
//! 它只吐订阅内容,**没有任何管理能力** —— 没有写接口、没有登录、没有会话。
//! 唯一的凭据是 URL 里的 `sub_token`,泄露它的后果上限是「别人拿到你的节点链接」,
//! 不是「别人能改配置」。
//!
//! 默认只听 127.0.0.1(`subscription.listen`),TLS 由前面的 nginx 终结。
//!
//! 路由:
//!   * `GET /sub/:token`                        按 User-Agent 自动选格式
//!   * `GET /sub/:token?type=base64|sing-box`   强制 base64 链接列表
//!   * `GET /sub/:token?type=clash|mihomo|yaml` 强制 Clash YAML
//!   * `GET /sub/:token?type=stats|html`        强制流量统计页
//!   * `GET /healthz`                           探活

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::net::SocketAddr;

use crate::config::SubscriptionConfig;
use crate::sub;

#[derive(Clone)]
struct SubState {
    pool: SqlitePool,
    cfg: SubscriptionConfig,
}

#[derive(Deserialize)]
struct SubQuery {
    #[serde(rename = "type")]
    ty: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Format {
    Stats,
    Yaml,
    Base64,
}

pub fn router(pool: SqlitePool, cfg: SubscriptionConfig) -> Router {
    Router::new()
        .route("/sub/:token", get(handle_sub))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(SubState { pool, cfg })
}

pub async fn run(pool: SqlitePool, cfg: SubscriptionConfig) -> Result<()> {
    let addr: SocketAddr = cfg
        .listen
        .parse()
        .with_context(|| format!("解析 subscription.listen 失败: {}", cfg.listen))?;
    let app = router(pool, cfg);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("订阅服务绑定 {addr} 失败"))?;
    tracing::info!(%addr, "订阅 HTTP 服务已启动");
    axum::serve(listener, app).await.context("订阅服务异常退出")
}

/// token 的形状校验。**在查库之前做**:
/// 它挡掉的是路径穿越、超长字符串这类明显不是 token 的输入,
/// 顺便让绝大多数扫描流量不落到数据库上。
fn token_looks_valid(token: &str) -> bool {
    (16..=64).contains(&token.len())
        && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn handle_sub(
    State(s): State<SubState>,
    Path(token): Path<String>,
    Query(q): Query<SubQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let fmt = pick_format(q.ty.as_deref(), ua);

    if !token_looks_valid(&token) {
        return (StatusCode::NOT_FOUND, HeaderMap::new(), String::new());
    }
    // 找不到 token 与 token 格式非法返回**同样的** 404:
    // 两者可区分的话,这个接口就成了一个 token 存在性预言机。
    let found = match sub::find_by_sub_token(&s.pool, &token).await {
        Ok(Some(v)) => v,
        Ok(None) => return (StatusCode::NOT_FOUND, HeaderMap::new(), String::new()),
        Err(e) => {
            tracing::warn!(error = %e, "查订阅用户失败");
            return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), String::new());
        }
    };
    let (user_id, user) = found;

    let nodes = match sub::export_nodes(&s.pool, user_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "查订阅节点失败");
            return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), String::new());
        }
    };

    let request_host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let opts = sub::ExportOptions {
        use_public_base_as_server: s.cfg.use_public_base_as_server,
        public_base: &s.cfg.public_base,
        request_host,
    };

    let (body, ctype) = match fmt {
        Format::Stats => {
            let base = resolve_base_url(&s.cfg.public_base, &headers);
            match stats_view(&s.pool, user_id, &user.name).await {
                Ok(view) => (
                    crate::stats_html::render(&view, &sub::generate_links(&user, &nodes, &opts), &base),
                    "text/html; charset=utf-8",
                ),
                Err(e) => {
                    tracing::warn!(error = %e, "渲染统计页失败");
                    return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), String::new());
                }
            }
        }
        Format::Yaml => (
            sub::generate_clash_yaml(&user, &nodes, &opts),
            "text/yaml; charset=utf-8",
        ),
        Format::Base64 => (
            sub::subscription_b64(&sub::generate_links(&user, &nodes, &opts)),
            "text/plain; charset=utf-8",
        ),
    };

    let mut out = HeaderMap::new();
    out.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    // 客户端拿它显示「已用 / 总量 / 到期」。查失败就不发这个头 ——
    // 少一个头只是少显示一行,而让整个订阅 500 会让用户直接断网。
    match usage_header(&s.pool, user_id).await {
        Ok(v) => {
            if let Ok(hv) = HeaderValue::from_str(&v) {
                out.insert(HeaderName::from_static("subscription-userinfo"), hv);
            }
        }
        Err(e) => tracing::warn!(error = %e, "算 subscription-userinfo 失败,已跳过该头"),
    }
    out.insert(
        HeaderName::from_static("profile-update-interval"),
        HeaderValue::from_static("6"),
    );

    (StatusCode::OK, out, body)
}

/// 显式 `?type=` > UA 嗅探 > 默认 base64。
fn pick_format(ty: Option<&str>, ua: &str) -> Format {
    if let Some(t) = ty {
        return match t.to_ascii_lowercase().as_str() {
            "clash" | "mihomo" | "yaml" => Format::Yaml,
            "stats" | "html" => Format::Stats,
            _ => Format::Base64,
        };
    }
    // 主流代理客户端的 UA 都带自己的关键字;浏览器几乎必然以 Mozilla 开头。
    // 认不出来就给 base64 —— 那是通用格式,给错了顶多用户手动加 ?type=,
    // 不会拿到一份解析失败的配置。
    let u = ua.to_ascii_lowercase();
    if u.contains("clash") || u.contains("mihomo") || u.contains("stash") {
        return Format::Yaml;
    }
    // 浏览器判定放在客户端关键字**之后**:有些客户端的 UA 也带 Mozilla 前缀,
    // 但它们同时带自己的关键字,上面那一条会先命中。
    if u.starts_with("mozilla/") {
        return Format::Stats;
    }
    Format::Base64
}

/// 统计页里拼完整 URL 用的 base:`public_base` 优先,否则从 Host 头回退。
///
/// 回退时 scheme 取 `X-Forwarded-Proto`,没有就假定 https ——
/// 这个服务默认只听 127.0.0.1,前面必然有一层反代(§10)。
fn resolve_base_url(public_base: &str, headers: &HeaderMap) -> String {
    let pb = public_base.trim().trim_end_matches('/');
    if !pb.is_empty() {
        return pb.to_string();
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    if host.is_empty() {
        return String::new();
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    format!("{scheme}://{host}")
}

/// 统计页要用的用户信息。
async fn stats_view(pool: &SqlitePool, user_id: i64, name: &str) -> Result<crate::stats_html::StatsView> {
    let (enabled, auto_disabled, quota_bytes, mult, expire_at, reset_day, sub_token): (
        bool,
        bool,
        i64,
        f64,
        Option<i64>,
        Option<i64>,
        String,
    ) = sqlx::query_as(
        "SELECT enabled, auto_disabled, quota_bytes, traffic_multiplier,
                expire_at, reset_day, sub_token
           FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    let (up, down): (i64, i64) =
        sqlx::query_as("SELECT cycle_up, cycle_down FROM user_traffic_total WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or((0, 0));

    Ok(crate::stats_html::StatsView {
        name: name.to_string(),
        enabled,
        auto_disabled,
        quota_bytes,
        cycle_up: up,
        cycle_down: down,
        traffic_multiplier: mult,
        expire_at,
        reset_day,
        sub_token,
    })
}

/// `subscription-userinfo: upload=X; download=Y; total=Z; expire=T`。
///
/// 流量取**周期内**用量并乘以倍率 —— 与 §6.3 的配额判定同一个口径,
/// 否则客户端显示的百分比和实际被停用的时机对不上。
async fn usage_header(pool: &SqlitePool, user_id: i64) -> Result<String> {
    let (quota, mult, expire_at): (i64, f64, Option<i64>) =
        sqlx::query_as("SELECT quota_bytes, traffic_multiplier, expire_at FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_one(pool)
            .await?;
    let (up, down): (i64, i64) =
        sqlx::query_as("SELECT cycle_up, cycle_down FROM user_traffic_total WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or((0, 0));

    let m = mult.max(0.0);
    let upload = (up.max(0) as f64 * m) as u64;
    let download = (down.max(0) as f64 * m) as u64;
    // total = 0 时客户端一般显示「不限」,正好对上 quota_bytes = 0 的语义。
    Ok(format!(
        "upload={upload}; download={download}; total={}; expire={}",
        quota.max(0),
        expire_at.unwrap_or(0)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_type_beats_user_agent() {
        assert_eq!(pick_format(Some("clash"), "v2rayN"), Format::Yaml);
        assert_eq!(pick_format(Some("base64"), "clash.meta"), Format::Base64);
        assert_eq!(pick_format(Some("stats"), "clash.meta"), Format::Stats);
        // 认不出的 type 落到 base64,而不是报错。
        assert_eq!(pick_format(Some("nonsense"), "clash"), Format::Base64);
    }

    /// 浏览器打开订阅地址时给统计页,不是一坨 base64。
    #[test]
    fn browsers_get_the_stats_page() {
        for ua in [
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            "mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)",
        ] {
            assert_eq!(pick_format(None, ua), Format::Stats, "{ua}");
        }
    }

    /// 带 Mozilla 前缀的**客户端** UA 仍然要拿配置,不是网页 ——
    /// 所以客户端关键字必须先于浏览器判定命中。
    #[test]
    fn clash_wins_over_the_mozilla_prefix() {
        assert_eq!(
            pick_format(None, "Mozilla/5.0 clash-verge/1.5"),
            Format::Yaml
        );
    }

    #[test]
    fn base_url_prefers_public_base_then_host() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("sub.example.com"));
        assert_eq!(resolve_base_url("https://cfg.example.com/", &h), "https://cfg.example.com");
        assert_eq!(resolve_base_url("", &h), "https://sub.example.com");

        h.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        assert_eq!(resolve_base_url("", &h), "http://sub.example.com");
        assert_eq!(resolve_base_url("", &HeaderMap::new()), "");
    }

    #[test]
    fn clash_family_user_agents_get_yaml() {
        for ua in ["clash.meta/1.18", "mihomo/1.0", "Stash/2.0", "ClashX Pro"] {
            assert_eq!(pick_format(None, ua), Format::Yaml, "{ua}");
        }
    }

    #[test]
    fn unknown_user_agents_get_base64() {
        for ua in ["v2rayN/6.0", "sing-box", "", "curl/8.0"] {
            assert_eq!(pick_format(None, ua), Format::Base64, "{ua}");
        }
    }

    /// token 形状校验挡掉明显不是 token 的输入 —— 包括路径穿越尝试。
    #[test]
    fn token_shape_is_validated_before_hitting_the_database() {
        assert!(token_looks_valid("abcdefghijklmnop"));
        assert!(token_looks_valid("A-B_c1234567890xyz"));

        assert!(!token_looks_valid(""), "空 token");
        assert!(!token_looks_valid("short"), "太短");
        assert!(!token_looks_valid(&"x".repeat(65)), "太长");
        assert!(!token_looks_valid("../../etc/passwd"), "路径穿越");
        assert!(!token_looks_valid("abcdefghijklmnop'--"), "SQL 元字符");
    }

    // ───────────────── 端到端:请求直接喂进 Router ─────────────────
    //
    // 用 ServiceExt::oneshot 而不是起真实监听:订阅服务没有握手之类的
    // 协议状态,走 Router 已经覆盖了路由、提取器、状态码和响应头的全部逻辑,
    // 而不占端口的测试可以随便并发跑。

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-subsrv-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    fn cfg() -> SubscriptionConfig {
        SubscriptionConfig::default()
    }

    /// 造一个有节点的用户,返回它的 sub_token。
    async fn user_with_node(pool: &SqlitePool) -> String {
        use crate::model::node::{NodeParams, Protocol};
        let (agent_id, _) = crate::db::agent_repo::create(pool, "tokyo", 0).await.unwrap();
        sqlx::query("UPDATE agents SET ipv4 = '203.0.113.7' WHERE id = ?")
            .bind(agent_id)
            .execute(pool)
            .await
            .unwrap();
        let mut params = NodeParams::default();
        crate::secrets::fill(Protocol::VlessReality, &mut params).unwrap();
        let (node_id, _) =
            crate::db::node_repo::add_node(pool, agent_id, "in-1", Protocol::VlessReality, 8443, &params)
                .await
                .unwrap();
        let uid = crate::db::node_repo::add_user(pool, "alice", 10 * 1_073_741_824, 0)
            .await
            .unwrap();
        crate::db::node_repo::assign_node(pool, uid, node_id).await.unwrap();
        sqlx::query_scalar("SELECT sub_token FROM users WHERE id = ?")
            .bind(uid)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn get(pool: &SqlitePool, uri: &str, ua: &str) -> (StatusCode, HeaderMap, String) {
        let app = router(pool.clone(), cfg());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::USER_AGENT, ua)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn valid_token_returns_base64_links() {
        let p = pool().await;
        let token = user_with_node(&p).await;
        let (status, headers, body) = get(&p, &format!("/sub/{token}"), "v2rayN/6.0").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        let decoded = String::from_utf8(
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &body).unwrap(),
        )
        .unwrap();
        assert!(decoded.starts_with("vless://"), "{decoded}");
        assert!(decoded.contains("203.0.113.7:8443"), "地址该来自 agent: {decoded}");
    }

    #[tokio::test]
    async fn clash_user_agent_gets_yaml() {
        let p = pool().await;
        let token = user_with_node(&p).await;
        let (status, headers, body) = get(&p, &format!("/sub/{token}"), "clash.meta/1.18").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/yaml; charset=utf-8");
        assert!(body.contains("proxies:"), "{body}");
    }

    /// 浏览器打开订阅地址应当看到统计页,而不是一屏 base64。
    #[tokio::test]
    async fn browser_gets_the_stats_page() {
        let p = pool().await;
        let token = user_with_node(&p).await;
        let (status, headers, body) =
            get(&p, &format!("/sub/{token}"), "Mozilla/5.0 (Windows NT 10.0)").await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
        assert!(body.starts_with("<!doctype html>"), "{}", &body[..80.min(body.len())]);
        assert!(body.contains("alice"), "应当显示用户名");
        assert!(body.contains("vless://"), "应当列出节点链接");
        assert!(body.contains("<svg"), "应当有二维码");
        // 订阅地址两种格式都要给出来。
        assert!(body.contains(&format!("/sub/{token}")));
        assert!(body.contains("?type=clash"));
        // 服务端凭据一个都不能出现。
        assert!(!body.contains("BEGIN PRIVATE KEY"));
        let params: String = sqlx::query_scalar("SELECT params_json FROM nodes LIMIT 1")
            .fetch_one(&p)
            .await
            .unwrap();
        let np: crate::model::node::NodeParams = serde_json::from_str(&params).unwrap();
        assert!(!body.contains(&np.private_key.unwrap()), "reality 私钥泄进了统计页");
    }

    #[tokio::test]
    async fn explicit_type_overrides_user_agent() {
        let p = pool().await;
        let token = user_with_node(&p).await;
        let (_, headers, _) = get(&p, &format!("/sub/{token}?type=clash"), "v2rayN").await;
        assert_eq!(headers[header::CONTENT_TYPE], "text/yaml; charset=utf-8");
    }

    /// 未知 token 与格式非法的 token 返回**同样的** 404、同样的空 body。
    /// 两者可区分的话,这个接口就成了 token 存在性预言机。
    #[tokio::test]
    async fn unknown_and_malformed_tokens_are_indistinguishable() {
        let p = pool().await;
        user_with_node(&p).await;

        let (s1, _, b1) = get(&p, "/sub/aaaaaaaaaaaaaaaaaaaa", "curl").await; // 形状合法但不存在
        let (s2, _, b2) = get(&p, "/sub/../../etc/passwd", "curl").await; // 形状非法
        assert_eq!(s1, StatusCode::NOT_FOUND);
        assert_eq!(s2, StatusCode::NOT_FOUND);
        assert_eq!(b1, b2);
        assert!(b1.is_empty());
    }

    /// 订阅正文与响应头都不该出现 token 之外的凭据 ——
    /// 尤其是 reality 私钥和证书私钥(§11.3)。
    #[tokio::test]
    async fn response_never_leaks_server_side_secrets() {
        let p = pool().await;
        let token = user_with_node(&p).await;
        let priv_key: String = sqlx::query_scalar("SELECT params_json FROM nodes LIMIT 1")
            .fetch_one(&p)
            .await
            .unwrap();
        let params: crate::model::node::NodeParams = serde_json::from_str(&priv_key).unwrap();
        let secret = params.private_key.unwrap();

        for ty in ["base64", "clash"] {
            let (_, headers, body) = get(&p, &format!("/sub/{token}?type={ty}"), "curl").await;
            let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &body)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_else(|_| body.clone());
            assert!(!decoded.contains(&secret), "{ty} 泄露了 reality 私钥");
            assert!(!decoded.contains("BEGIN PRIVATE KEY"), "{ty} 泄露了证书私钥");
            assert!(!format!("{headers:?}").contains(&secret), "{ty} 的响应头泄露了私钥");
        }
    }

    /// 客户端靠 subscription-userinfo 显示用量。倍率要算进去 ——
    /// 与 §6.3 的配额判定同一个口径,否则显示的百分比和实际停用时机对不上。
    #[tokio::test]
    async fn userinfo_header_reports_quota_and_multiplied_usage() {
        let p = pool().await;
        let token = user_with_node(&p).await;
        let uid: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name = 'alice'")
            .fetch_one(&p)
            .await
            .unwrap();
        let nid: i64 = sqlx::query_scalar("SELECT id FROM nodes LIMIT 1").fetch_one(&p).await.unwrap();
        sqlx::query("UPDATE users SET traffic_multiplier = 2.0, expire_at = 1893456000 WHERE id = ?")
            .bind(uid)
            .execute(&p)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO user_traffic (user_id, node_id, cycle_up, cycle_down, updated_at)
             VALUES (?, ?, 100, 200, 0)",
        )
        .bind(uid)
        .bind(nid)
        .execute(&p)
        .await
        .unwrap();

        let (_, headers, _) = get(&p, &format!("/sub/{token}"), "curl").await;
        let info = headers["subscription-userinfo"].to_str().unwrap();
        assert!(info.contains("upload=200"), "倍率没算进去: {info}");
        assert!(info.contains("download=400"), "{info}");
        assert!(info.contains(&format!("total={}", 10u64 * 1_073_741_824)), "{info}");
        assert!(info.contains("expire=1893456000"), "{info}");
    }

    /// 没有任何节点的用户拿到的是**空订阅**,不是 500 ——
    /// 报错会让客户端把上一次的可用配置也丢掉。
    #[tokio::test]
    async fn user_without_nodes_gets_an_empty_subscription() {
        let p = pool().await;
        let uid = crate::db::node_repo::add_user(&p, "lonely", 0, 0).await.unwrap();
        let token: String = sqlx::query_scalar("SELECT sub_token FROM users WHERE id = ?")
            .bind(uid)
            .fetch_one(&p)
            .await
            .unwrap();

        let (status, _, body) = get(&p, &format!("/sub/{token}"), "curl").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_empty(), "空订阅就该是空 body: {body:?}");

        let (status, _, yaml) = get(&p, &format!("/sub/{token}?type=clash"), "curl").await;
        assert_eq!(status, StatusCode::OK);
        assert!(yaml.contains("proxies:\n  []"), "{yaml}");
    }

    #[tokio::test]
    async fn healthz_and_unknown_paths() {
        let p = pool().await;
        let (status, _, body) = get(&p, "/healthz", "curl").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");

        // 订阅服务**没有任何管理能力**(§2 的例外要收窄到只读订阅)。
        for path in ["/", "/api/users", "/admin"] {
            let (status, _, _) = get(&p, path, "curl").await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path} 不该存在");
        }
    }
}
