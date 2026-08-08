//! sbx 主控。CLI + daemon + TUI 三入口(§3)。
//!
//! 已接线:CLI(agent / node / user 管理)、daemon(wss 或明文 ws)、
//! 握手补齐(§4.1)、上报入库(§5.2)、TLS 自签证书与指纹(§1.3)、
//! 配额与到期的自动化巡检(§6.3,见 `supervisor::spawn`)、
//! 八协议配置生成(§9.1)、订阅导出与统计页(§10,见 `sub_server` / `stats_html`)、
//! TUI(§8,见 `tui`)、Telegram 通知(§9.1,见 `tg`)。
//!
//! 全部 DESIGN.md 里规划的功能都已接线。

mod cluster;
mod config;
mod db;
mod install;
mod model;
mod secrets;
mod service;
mod stats_html;
mod sub;
mod sub_server;
mod supervisor;
mod tg;
mod tls;
mod tui;
mod upgrade;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "sbx", version, about = "sing-box 集群管理(主控)")]
struct Cli {
    /// 配置文件路径。不存在时使用全默认配置。
    #[arg(long, default_value = "/etc/sbx/config.toml")]
    config: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 初始化/迁移数据库并退出。反复运行是安全的(幂等)。
    InitDb,
    /// 启动主控守护进程(WS 服务端)。
    Daemon,
    /// 打开交互式管理界面(§8)。
    ///
    /// 与 daemon 是**两个独立进程**,只通过数据库交换状态:
    /// 在这里改完配置,由正在跑的 daemon 在下次握手或下发时同步到各 agent。
    Tui,
    /// 打印主控证书指纹(agent 侧 TOFU 固定用)。证书不存在时会先生成。
    Fingerprint,
    /// 列出全部被控服务器。
    AgentList,
    /// 新增被控服务器,生成连接 token(**明文只显示这一次**)。
    AgentAdd {
        /// 服务器名称,须唯一。
        name: String,
    },
    /// 轮换某台被控服务器的 token。旧 token 立即失效,在线连接下次重连时生效。
    AgentRotate {
        /// 服务器 id(见 agent-list)。
        id: i64,
    },
    /// 删除被控服务器(级联删除其节点与分配关系)。
    AgentRemove {
        id: i64,
        /// 跳过确认。没有它时只打印将受影响的用户数,不执行删除。
        #[arg(long)]
        yes: bool,
    },

    /// 列出全部节点。
    NodeList,
    /// 在某台被控服务器上新增节点。
    NodeAdd {
        /// 被控服务器 id。
        agent_id: i64,
        /// inbound tag,在同一 agent 内唯一。也是流量记账的一半(§7.1)。
        tag: String,
        /// 监听端口。
        port: u16,
        /// 协议。八个都支持(§9.1)。
        #[arg(long, default_value = "vless-reality")]
        protocol: String,
        /// TLS 系协议的 server_name / 伪装域名。留空按协议取默认值。
        #[arg(long)]
        server_name: Option<String>,
        /// ws 系协议(vless-ws / vmess-ws)的路径。留空取 /vless 或 /vmess。
        #[arg(long)]
        path: Option<String>,
    },
    /// 删除节点。
    NodeRemove { id: i64 },

    /// 列出全部用户及其用量(跨 agent 求和)。
    UserList,
    /// 新增用户。
    UserAdd {
        name: String,
        /// 配额,单位 GB。0 = 不限。
        #[arg(long, default_value = "0")]
        quota_gb: f64,
    },
    /// 把节点分配给用户。一个用户可以有多个节点,可跨 agent。
    UserAssign { name: String, node_id: i64 },
    /// 解除节点分配。
    UserUnassign { name: String, node_id: i64 },
    /// 启用用户。
    UserEnable { name: String },
    /// 停用用户(手动停用不会被自动流程恢复)。
    UserDisable { name: String },
    /// 删除用户。
    UserRemove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// 打印用户的订阅地址与各节点的分享链接(§10)。
    UserSub {
        name: String,
        /// 同时打印每条分享链接。默认只打印订阅地址 ——
        /// 链接里含用户凭据,不该无意中留在终端回滚缓冲里。
        #[arg(long)]
        links: bool,
    },
    /// 生成 Telegram 绑定码(用户在 bot 里发 `/bind <码>` 完成绑定)。
    ///
    /// 每次调用都会作废上一个未使用的码。绑定成功后码自动失效。
    UserTgBind { name: String },
    /// 解除某用户的 Telegram 绑定。
    UserTgUnbind { name: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sbx=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = load_config(&cli.config)?;
    let pool = db::init_pool(&cfg.db.path)
        .await
        .with_context(|| format!("打开数据库 {} 失败", cfg.db.path))?;

    match cli.cmd {
        Cmd::InitDb => {
            println!("数据库已就绪:{}(schema v{})", cfg.db.path, db::schema_version());
        }
        Cmd::Daemon => run_daemon(cfg, pool).await?,
        Cmd::Tui => tui::run(pool, cfg, cli.config.clone()).await?,
        Cmd::Fingerprint => {
            if !cfg.cluster.tls {
                println!("cluster.tls = false(明文模式),没有证书指纹。");
                println!("agent 侧应配 insecure = true,并把 TLS 终止交给前面的 nginx。");
            } else {
                let fp = tls::ensure_cert(&cfg.cluster.cert_path, &cfg.cluster.key_path, &cfg.cluster.listen)?;
                println!("{fp}");
            }
        }
        Cmd::AgentList => {
            let agents = db::agent_repo::list(&pool).await?;
            if agents.is_empty() {
                println!("还没有被控服务器。用 `sbx agent-add <名称>` 添加。");
            }
            for a in agents {
                println!(
                    "{:>4}  {:<20} {:<8} token={}…  {}",
                    a.id,
                    a.name,
                    a.status,
                    a.token_prefix,
                    a.ipv4.as_deref().unwrap_or("-")
                );
            }
        }
        Cmd::AgentAdd { name } => {
            let now = chrono::Local::now().timestamp();
            let (id, token) = db::agent_repo::create(&pool, &name, now).await?;
            // 明文 token 只在这里出现一次:库里只有 hash 与 prefix(§8.1)。
            // 刻意用 println! 而不是 tracing —— 凭据不该进日志(§11.3)。
            println!("已添加被控服务器 #{id}:{name}");
            println!();
            println!("在被控机上跑这一条(root):");
            println!();
            // CLI 是一次性命令,同步等一下探测没问题(TUI 那边是后台探的,
            // 因为那里卡几秒会冻住界面)。
            let host = install::resolve_host(&cfg, install::probe_public_ip().await.as_deref());
            // 命令**顶格单独一行**:它是要被鼠标选中复制走的东西,
            // 前面加缩进会让「双击选中整行」把缩进也带上。
            println!("{}", install::command(&cfg, &host, Some(&token)));
            println!();
            for line in install::notes(&host, true) {
                println!("{line}");
            }
            println!();
            println!("(TUI 里 [2] 服务管理页按 [a] 也能拿到同一条,并且可以按 [y] 复制。)");
        }
        Cmd::AgentRotate { id } => {
            if db::agent_repo::get(&pool, id).await?.is_none() {
                anyhow::bail!("没有 id 为 {id} 的被控服务器");
            }
            let token = db::agent_repo::rotate_token(&pool, id).await?;
            println!("已轮换 #{id} 的 token(旧 token 立即失效;在线连接下次重连时生效)。");
            println!();
            println!("在那台被控机上重跑这一条,旧配置会自动备份成 agent.toml.bak:");
            println!();
            let host = install::resolve_host(&cfg, install::probe_public_ip().await.as_deref());
            println!("{}", install::command(&cfg, &host, Some(&token)));
        }
        Cmd::AgentRemove { id, yes } => {
            let Some(agent) = db::agent_repo::get(&pool, id).await? else {
                anyhow::bail!("没有 id 为 {id} 的被控服务器");
            };
            let affected = db::agent_repo::affected_user_count(&pool, id).await?;
            if !yes {
                println!("将删除 #{id} {},影响 {affected} 个用户的节点分配。", agent.name);
                println!("确认请加 --yes。");
                return Ok(());
            }
            db::agent_repo::delete(&pool, id).await?;
            println!("已删除 #{id} {}(影响 {affected} 个用户)。", agent.name);
        }

        Cmd::NodeList => {
            let nodes = db::node_repo::list_nodes(&pool).await?;
            if nodes.is_empty() {
                println!("还没有节点。用 `sbx node-add <agent_id> <tag> <port>` 添加。");
            }
            for n in nodes {
                println!(
                    "{:>4}  agent={:<4} {:<16} {:<14} :{}",
                    n.id, n.agent_id, n.tag, n.protocol, n.listen_port
                );
            }
        }
        Cmd::NodeAdd { agent_id, tag, port, protocol, server_name, path } => {
            if db::agent_repo::get(&pool, agent_id).await?.is_none() {
                anyhow::bail!("没有 id 为 {agent_id} 的被控服务器");
            }
            let proto = model::node::Protocol::parse(&protocol);
            if matches!(proto, model::node::Protocol::Unknown) {
                anyhow::bail!(
                    "无法识别的协议 {protocol};可选:{}",
                    model::node::Protocol::all()
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                );
            }
            let mut params = model::node::NodeParams { server_name, path, ..Default::default() };
            // 密钥材料在这里生成一次,存进 params_json。**不在下发时生成** ——
            // 那会让每次 config.apply 都换一套密钥,客户端静默全部失联(§9.1)。
            secrets::fill(proto, &mut params)?;
            let (id, rev) = db::node_repo::add_node(&pool, agent_id, &tag, proto, port, &params).await?;
            println!("已在 agent #{agent_id} 上新增节点 #{id} {tag}({proto} :{port})");
            // reality 的 public_key / short_id 是**客户端**要的,订阅里也会带。
            // 私钥、证书私钥、ss 服务端密钥一律不打印(§11.3)。
            if let (Some(pk), Some(sid)) = (&params.public_key, &params.short_id) {
                println!("  reality public_key = {pk}");
                println!("  reality short_id   = {sid}");
            }
            println!("该 agent 的 config_revision → {rev};在线时会在下次握手或下发时生效。");
        }
        Cmd::NodeRemove { id } => {
            let (agent_id, rev) = db::node_repo::delete_node(&pool, id).await?;
            println!("已删除节点 #{id}(agent #{agent_id} 的 config_revision → {rev})");
        }

        Cmd::UserList => {
            let users = db::node_repo::list_users(&pool).await?;
            if users.is_empty() {
                println!("还没有用户。用 `sbx user-add <名称>` 添加。");
            }
            for u in users {
                // 用量来自跨 agent 求和的视图(§6.2)
                let usage: model::user::Usage = sqlx::query_as(
                    "SELECT cycle_up, cycle_down, total_up, total_down
                       FROM user_traffic_total WHERE user_id = ?",
                )
                .bind(u.id)
                .fetch_optional(&pool)
                .await?
                .unwrap_or_default();

                let used = model::user::User::format_bytes(u.used_total_bytes(&usage));
                let quota = if u.quota_bytes == 0 {
                    "不限".to_string()
                } else {
                    format!(
                        "{} ({:.1}%)",
                        model::user::User::format_bytes(u.quota_bytes),
                        u.quota_used_percent(&usage)
                    )
                };
                println!(
                    "{:>4}  {:<20} {:<8} {} / {}",
                    u.id,
                    u.name,
                    if u.enabled { "启用" } else { "停用" },
                    used,
                    quota
                );
            }
        }
        Cmd::UserAdd { name, quota_gb } => {
            let quota_bytes = (quota_gb * 1_073_741_824.0) as i64;
            let now = chrono::Local::now().timestamp();
            let id = db::node_repo::add_user(&pool, &name, quota_bytes, now).await?;
            println!("已新增用户 #{id} {name}");
            if quota_bytes == 0 {
                println!("配额:不限");
            } else {
                println!("配额:{}", model::user::User::format_bytes(quota_bytes));
            }
            println!("用 `sbx user-assign {name} <node_id>` 给它分配节点。");
        }
        Cmd::UserAssign { name, node_id } => {
            let u = db::node_repo::get_user_by_name(&pool, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("没有名为 {name} 的用户"))?;
            let (agent_id, rev) = db::node_repo::assign_node(&pool, u.id, node_id).await?;
            println!("已把节点 #{node_id} 分配给 {name}(agent #{agent_id} 的 config_revision → {rev})");
        }
        Cmd::UserUnassign { name, node_id } => {
            let u = db::node_repo::get_user_by_name(&pool, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("没有名为 {name} 的用户"))?;
            let (agent_id, rev) = db::node_repo::unassign_node(&pool, u.id, node_id).await?;
            println!("已解除 {name} 对节点 #{node_id} 的分配(agent #{agent_id} → rev {rev})");
        }
        Cmd::UserEnable { name } => {
            db::node_repo::set_user_enabled(&pool, &name, true).await?;
            println!("已启用 {name}。各 agent 的 user_state_revision 已推进,在线的会收到 user.state。");
        }
        Cmd::UserDisable { name } => {
            db::node_repo::set_user_enabled(&pool, &name, false).await?;
            println!("已停用 {name}(手动停用,自动流程不会恢复它)。");
            println!("这条路径**不重建 box**,不影响其他用户的连接(§7.5)。");
        }
        Cmd::UserRemove { name, yes } => {
            if db::node_repo::get_user_by_name(&pool, &name).await?.is_none() {
                anyhow::bail!("没有名为 {name} 的用户");
            }
            if !yes {
                println!("将删除用户 {name} 及其全部节点分配与流量记录。确认请加 --yes。");
                return Ok(());
            }
            db::node_repo::delete_user(&pool, &name).await?;
            println!("已删除用户 {name}。");
        }
        Cmd::UserSub { name, links } => {
            let u = db::node_repo::get_user_by_name(&pool, &name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("没有名为 {name} 的用户"))?;

            let base = cfg.subscription.public_base.trim_end_matches('/');
            if base.is_empty() {
                println!("订阅路径:/sub/{}", u.sub_token);
                println!("(未配置 subscription.public_base,所以只能给出路径;");
                println!(" 完整地址取决于你前面那层 nginx 怎么反代 {})", cfg.subscription.listen);
            } else {
                println!("订阅地址:{base}/sub/{}", u.sub_token);
            }
            println!("  Clash/Mihomo:在上面的地址后加 ?type=clash(客户端 UA 也会被自动识别)");

            let nodes = sub::export_nodes(&pool, u.id).await?;
            let opts = sub::ExportOptions {
                use_public_base_as_server: cfg.subscription.use_public_base_as_server,
                public_base: &cfg.subscription.public_base,
                request_host: None,
            };
            let generated = sub::generate_links(&sub::SubUser {
                name: u.name.clone(),
                uuid: u.uuid.clone(),
                password: u.password.clone(),
            }, &nodes, &opts);

            println!("节点:{} 个已分配,{} 个可导出", nodes.len(), generated.len());
            if generated.len() < nodes.len() {
                // 差额的原因只有两种:所属 agent 没有可用地址,或节点缺密钥材料。
                println!("  (差额是「所属 agent 还没上报过公网地址」或「节点缺密钥材料」;");
                println!("   前者等 agent 连上来即可,后者要重建那个节点)");
            }
            if links {
                for l in &generated {
                    println!("  [{}] {}", l.protocol, l.link);
                }
            } else if !generated.is_empty() {
                println!("  加 --links 打印分享链接(含用户凭据,注意终端回滚缓冲)");
            }
        }
        Cmd::UserTgBind { name } => {
            if db::node_repo::get_user_by_name(&pool, &name).await?.is_none() {
                anyhow::bail!("没有名为 {name} 的用户");
            }
            let code = tg::issue_bind_code(&pool, &name).await?;
            println!("{name} 的 Telegram 绑定码:{code}");
            println!("让他在 bot 里发送:/bind {code}");
            println!("(绑定成功后该码即失效;重新生成会作废上一个)");
        }
        Cmd::UserTgUnbind { name } => {
            tg::unbind_user(&pool, &name).await?;
            println!("已解除 {name} 的 Telegram 绑定。");
        }
    }
    Ok(())
}

/// 启动 WS 服务端,收 agent 连接。
///
/// 默认是 **wss**:§1.3 的自签证书 + TOFU 指纹已接线,证书不存在时自动生成,
/// 启动时打印指纹供 agent 配 `fingerprint =`。
/// 配置里显式关掉 TLS 才会退回明文 ws——那种情况下 agent 侧必须配 `insecure = true`,
/// 或者在前面套一层 nginx 做 TLS 终止。**降级是显式的,不静默发生。**
async fn run_daemon(cfg: config::Config, pool: sqlx::SqlitePool) -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::Mutex;


    // 启动时把上次残留的 online 状态清掉:进程刚起来,谁都还没连上。
    // 不做这一步,TUI 会显示一批实际不在线的 agent 为 online。
    let reset = sqlx::query("UPDATE agents SET status = 'offline' WHERE status = 'online'")
        .execute(&pool)
        .await?;
    if reset.rows_affected() > 0 {
        tracing::debug!(count = reset.rows_affected(), "启动时重置残留的 online 状态");
    }

    let state = cluster::ServerState {
        pool,
        registry: Arc::new(Mutex::new(cluster::Registry::new())),
        rpc: Arc::new(cluster::Rpc::new()),
        speed: Arc::new(Mutex::new(std::collections::HashMap::new())),
        heartbeat_secs: cfg.cluster.heartbeat_secs,
        report_interval_secs: cfg.cluster.report_interval_secs,
    };

    // §9.1 的 Telegram Bot。拿不到租约(另一个进程已经在跑)不是错误,
    // 起不来也不该拖垮主控 —— 通知是锦上添花,agent 连不上才是全网停服。
    let tg_tx = if cfg.telegram.enabled {
        match tg::start(
            state.pool.clone(),
            cfg.telegram.clone(),
            cfg.subscription.public_base.clone(),
            "daemon",
        )
        .await
        {
            Ok(tx) => tx,
            Err(e) => {
                tracing::error!(error = %e, "Telegram Bot 未能启动(主控继续运行)");
                None
            }
        }
    } else {
        None
    };

    // §6.3 的配额/到期巡检。与 WS 服务端并行跑,周期沿用 report_interval_secs
    // ——没必要比上报更频繁,因为它判定的依据正是上报进来的用量。
    supervisor::spawn(state.clone(), cfg.cluster.report_interval_secs, tg_tx);

    // §10 的订阅 HTTP 服务。与集群端口是**两个独立监听**:
    // 集群那个要对公网开(agent 要连),订阅这个默认只听 127.0.0.1 由 nginx 反代。
    // 它起不来不该拖垮主控 —— agent 连不上是全网停服,订阅拿不到只是暂时下发不了链接。
    if cfg.subscription.enabled {
        let sub_pool = state.pool.clone();
        let sub_cfg = cfg.subscription.clone();
        tokio::spawn(async move {
            if let Err(e) = sub_server::run(sub_pool, sub_cfg).await {
                tracing::error!(error = %e, "订阅服务未能启动(主控继续运行)");
            }
        });
    } else {
        tracing::info!("subscription.enabled = false,不启动订阅服务");
    }

    let router = cluster::server::router(state);
    let addr: std::net::SocketAddr = cfg
        .cluster
        .listen
        .parse()
        .with_context(|| format!("cluster.listen = {} 不是合法的监听地址", cfg.cluster.listen))?;

    if cfg.cluster.tls {
        // rustls 0.23 起,进程级的 CryptoProvider 必须显式装一次。
        // 依赖树里同时出现 ring 与 aws-lc-rs 时它拒绝自动选,直接 panic
        // ——而且 panic 发生在**第一次握手时**,不是启动时,所以不装的话
        // 表现为「主控起来了但每个 agent 一连就崩」。
        //
        // 返回 Err 表示已经装过了(比如测试里跑过一次),忽略即可。
        let _ = rustls::crypto::ring::default_provider().install_default();

        // §1.3:证书不存在时自己生成一张自签的。
        let fp = tls::ensure_cert(&cfg.cluster.cert_path, &cfg.cluster.key_path, &cfg.cluster.listen)?;
        tracing::info!(listen = %addr, fingerprint = %fp, "主控已启动(wss),等待 agent 连接");
        tracing::info!("agent 侧需要配置这个指纹(TOFU pinning);也可用 `sbx fingerprint` 再打印一次");

        let tls_cfg = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            &cfg.cluster.cert_path,
            &cfg.cluster.key_path,
        )
        .await
        .context("加载 TLS 证书失败")?;

        // axum-server 的优雅退出走 Handle,而不是 axum::serve 的 with_graceful_shutdown。
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            // 给在途连接 10 秒收尾。agent 会自己重连,不需要等更久。
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
        });

        axum_server::bind_rustls(addr, tls_cfg)
            .handle(handle)
            .serve(router.into_make_service())
            .await
            .context("WSS 服务端异常退出")?;
    } else {
        // 明文模式只该用于「nginx 已做 TLS 终止」的部署(§1.3)。
        // agent 侧也必须显式写 insecure = true,不会静默降级。
        tracing::warn!(
            listen = %addr,
            "以**明文 ws** 启动(cluster.tls = false)。\
             仅适用于 nginx 已做 TLS 终止的部署;agent 侧需配 insecure = true。"
        );
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("监听 {addr} 失败"))?;

        // SIGTERM / Ctrl-C 时优雅退出。旧项目 `shutdown_flush()` 的语义(§9.1)
        // 在这里只需要停止接受新连接——流量数据是 agent 单调累计的,
        // 主控中途退出不会丢账(§5.3 的直接收益)。
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("WS 服务端异常退出")?;
    }

    tracing::info!("主控已停止");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("收到退出信号,停止接受新连接");
}

/// 读配置。**文件不存在不是错误**——用全默认值继续,
/// 否则第一次运行 `sbx init-db` 就会失败,而那正是还没有配置文件的时刻。
fn load_config(path: &str) -> Result<config::Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => config::Config::parse(&text).with_context(|| format!("解析配置 {path} 失败")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(path, "配置文件不存在,使用默认配置");
            Ok(config::Config::default())
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!("读取配置 {path} 失败"))),
    }
}
