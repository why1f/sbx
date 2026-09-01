//! WS 端点:握手、认证、连接生命周期(DESIGN.md §4.1)。
//!
//! 一条连接的完整生命周期:
//!
//! ```text
//! upgrade → 等第一帧(10s 超时)→ 必须是 agent.hello
//!   → 校 token(失败:记 agent_events,回一个不透露原因的 error,断开)
//!   → registry.register(驱逐同 id 旧连接)
//!   → 回 agent.hello_ack(带两个 revision)
//!   → 比对两个 revision,不一致的分别补齐(config.apply / user.state)
//!   → 收发循环
//!   → 断开:registry.unregister(带自己的 epoch)、标 offline
//! ```
//!
//! **TLS 不在这里。** §1.3 的自签证书 + TOFU 由 daemon 装配时套在这个 router 外面。

use crate::cluster::Registry;
use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use sbx_shared::{method, AgentHello, AgentHelloAck, Envelope, Kind};
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// 握手超时:agent 连上后的第一帧必须在这个时间内到达(§4.1)。
const HELLO_TIMEOUT_SECS: u64 = 10;

#[derive(Clone)]
pub struct ServerState {
    pub pool: SqlitePool,
    /// `Registry` 是同步结构(内部只有 HashMap 操作,不 await),
    /// 所以用 `tokio::sync::Mutex` 短暂持有即可,不需要 RwLock。
    pub registry: Arc<Mutex<Registry>>,
    pub rpc: Arc<crate::cluster::Rpc>,
    /// 每台 agent 最近一次 sysinfo 的 (时刻, rx 增量, tx 增量),用于算网速。
    ///
    /// **只在内存里,不落库**(§8.2)。注意它是 30s 平均值而不是瞬时速率——
    /// 上报周期就是 30s,所以这个数字会显得很钝。
    pub speed: Arc<Mutex<std::collections::HashMap<i64, SpeedSample>>>,
    pub heartbeat_secs: u64,
    pub report_interval_secs: u64,
    /// 一条连接最长可以多久不出声。超过就判成半开、断开并 `mark_offline`。
    ///
    /// 由 `idle_limit(heartbeat_secs)` 在启动时算好存下来,而不是每次现算:
    /// 测试要塞一个很小的值(真实值有 30s 下限,照那个跑一条用例要半分钟)。
    pub idle_limit: Duration,
}

/// 一次网速采样。`boot_id` 变化的那一次不产生采样(否则会算出爆炸数字,§8.2)。
///
/// 两个速率字段目前只写不读 —— 它们是给 TUI(§8.2 的两行式 agent 列表)准备的,
/// 那一页会把它们画成实时速率。在 TUI 落地之前保留写入,是因为采样必须**连续**:
/// 断一段时间再接上,第一拍会拿一个跨越缺口的差值算出假的高速。
#[derive(Debug, Clone, Copy)]
pub struct SpeedSample {
    pub at: i64,
    #[allow(dead_code)]
    pub rx_per_sec: f64,
    #[allow(dead_code)]
    pub tx_per_sec: f64,
}

pub fn router(state: ServerState) -> Router {
    Router::new().route("/ws", get(upgrade)).with_state(state)
}

async fn upgrade(ws: WebSocketUpgrade, State(state): State<ServerState>) -> Response {
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = serve(socket, state).await {
            // 连接级错误是常态(网络抖动、agent 重启),记 debug 不记 warn,
            // 否则日志会被正常的断连刷满。真正的异常(认证失败)在 handshake 里单独记。
            tracing::debug!(error = %e, "agent 连接结束");
        }
    })
}

async fn serve(socket: WebSocket, state: ServerState) -> Result<()> {
    let (mut sink, mut stream) = socket.split();

    // ── 握手 ──
    let hello = match handshake(&mut stream, &state).await {
        Ok(h) => h,
        Err(HandshakeError::Rejected(reason)) => {
            // 不向连接方回显具体原因(避免枚举,§8.1),只回一个笼统的 error。
            let env = Envelope::resp_err("0", method::AGENT_HELLO_ACK, "握手被拒绝");
            let _ = sink.send(Message::Text(serde_json::to_string(&env)?)).await;
            tracing::warn!(reason, "拒绝 agent 握手");
            return Ok(());
        }
        Err(HandshakeError::Fatal(e)) => return Err(e),
    };
    let Authenticated { agent_id, hello, hello_id } = hello;

    // ── 登记(驱逐同 id 旧连接,§4.1)──
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Envelope>();
    let registered = state.registry.lock().await.register(agent_id, tx);
    if registered.evicted_previous {
        // 正常现象,不写 agent_events(§4.1),但值得留一条 debug:
        // 若它高频出现,说明 agent 在反复重连。
        tracing::debug!(agent_id, "同 agent 的新连接驱逐了旧连接");
    }

    let loop_result =
        serve_registered(sink, &mut stream, rx, agent_id, hello, hello_id, &state).await;

    // ── 清理 ──
    //
    // **必须无条件执行。** 早先这一段和上面那些 `?` 在同一个函数里,于是
    // `load_revisions` / `mark_online` / 发 ack 任一失败都会带着一条**已经登记**的
    // 连接直接 return:registry 里留下一个接收端已 drop 的幽灵条目
    // (`is_online()` 是 true、`send()` 却永远 false),`mark_offline` 不执行,
    // 挂在这台 agent 上的 pending RPC 也不取消、只能干等 30 秒超时。
    // 它会在 agent 下次重连时被 `register` 驱逐掉,所以能自愈 —— 但在那之前
    // 仪表盘的在线数是虚高的,而这种「数字不对但没人报错」最难查。
    //
    // 拆成两个函数就是为了让这件事由**结构**保证,而不是靠每次改动都记得
    // 「别在这中间加 `?`」。
    let cleanup_result = cleanup_connection(&state, agent_id, registered.epoch).await;
    tracing::info!(agent_id, "agent 断开");

    // 收尾的错误**不能盖掉 `loop_result`**。后者说的是「连接为什么断的」,
    // 那才是排查时想看的那一个;而 `mark_offline` 失败(磁盘忙、库被关掉)只是
    // 没收干净 —— 真正要紧的注销已经在 `cleanup_connection` 里先做完了,
    // 而状态字段下一次握手就会被改对。
    //
    // 改之前这里是 `cleanup_connection(...).await?`,于是一次收尾失败会把断开原因
    // 整个丢掉。两个都失败时留 `loop_result`,收尾那个单独记一条 warn。
    match (loop_result, cleanup_result) {
        (Err(e), Err(c)) => {
            tracing::warn!(agent_id, error = %c, "断开收尾也失败了");
            Err(e)
        }
        (Err(e), Ok(())) => Err(e),
        (Ok(()), c) => c,
    }
}

/// 登记之后的全部工作:补齐 revision、回 ack、跑收发循环。
///
/// **它怎么失败都不影响清理** —— 调用方拿到 `Result` 之后才走清理路径(见 `serve`)。
/// 这里可以放心用 `?`,那正是把它拆出来的目的。
#[allow(clippy::too_many_arguments)]
async fn serve_registered(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Envelope>,
    agent_id: i64,
    hello: AgentHello,
    hello_id: String,
    state: &ServerState,
) -> Result<()> {
    let now = chrono::Local::now().timestamp();
    let db_state = load_revisions(&state.pool, agent_id).await?;
    mark_online(&state.pool, agent_id, &hello, now).await?;

    // ── 回 hello_ack ──
    let ack = AgentHelloAck {
        agent_id,
        server_time: now,
        heartbeat_secs: state.heartbeat_secs,
        report_interval_secs: state.report_interval_secs,
        config_revision: db_state.config_revision,
        user_state_revision: db_state.user_state_revision,
    };
    sink.send(Message::Text(serde_json::to_string(&Envelope::resp_ok(
        hello_id,
        method::AGENT_HELLO_ACK,
        serde_json::to_value(&ack)?,
    ))?))
    .await?;

    // ── 两个 revision 分别比对、分别补齐(§4.1)──
    //
    // 顺序上先 config 后 user_state:若 config.apply 会重建 box,
    // 让禁用名单在那之后到达,避免刚设好的内存标记被一次重建洗掉。
    // (实际上 §7.5 保证 disabled 是独立字段、不随重建丢失,但顺序无成本,取更稳的那个。)
    // 补齐必须在**收发循环启动之后**才能做:它要发 req 并等 resp,
    // 而 resp 只有 recv_loop 跑起来才会被 rpc.resolve 唤醒。
    // 所以这里 spawn 一个任务,让它与 recv_loop 并行。
    let catchup_state = state.clone();
    let catchup = tokio::spawn(async move {
        catch_up(
            agent_id,
            &catchup_state,
            hello.config_revision,
            hello.user_state_revision,
            db_state,
        )
        .await;
    });

    // ── 收发循环 ──
    //
    // 发送侧单独一个任务:registry 里的 `ConnTx` 被丢弃(驱逐)时 `rx.recv()` 返回 None,
    // 这个任务随即退出——这就是驱逐旧连接的实际生效路径。
    let send_task = tokio::spawn(async move {
        while let Some(env) = rx.recv().await {
            let text = match serde_json::to_string(&env) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "序列化下发消息失败");
                    continue;
                }
            };
            if sink.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    let loop_result = recv_loop(stream, agent_id, state).await;
    catchup.abort(); // 连接断了,补齐没意义了
    send_task.abort();
    loop_result
}

/// 一条连接的收尾:注销、取消它的 pending RPC、标离线。
///
/// **注销排在最前面,而且它的成败不依赖数据库。** 顺序反过来的话,一次
/// `mark_offline` 失败(磁盘忙、库被关掉)会让 registry 里留下一个幽灵条目,
/// 而那比「状态字段没更新」难查得多:状态字段下一次握手就会被改对,
/// 幽灵条目却会让仪表盘的在线数一直虚高。
async fn cleanup_connection(state: &ServerState, agent_id: i64, epoch: u64) -> Result<()> {
    // 带上自己的 epoch:若这条连接已被更新的连接驱逐,这次注销会被忽略,
    // 不会把新连接踢下线(registry 的 stale-unregister 测试覆盖了这条)。
    let still_mine = state.registry.lock().await.unregister(agent_id, epoch);
    if !still_mine {
        return Ok(());
    }
    // 这条 agent 上还挂着的 pending RPC 立刻失败,不必干等 30 秒超时。
    //
    // **必须在 still_mine 里面。** 若这条连接已被更新的连接驱逐,
    // cancel_agent 是按 agent_id 清的,会把新连接正在等的 RPC 一起打掉。
    let cancelled = state.rpc.cancel_agent(agent_id).await;
    if cancelled > 0 {
        tracing::debug!(agent_id, cancelled, "断开时取消了 pending RPC");
    }
    mark_offline(&state.pool, agent_id, chrono::Local::now().timestamp()).await
}

/// 握手后补齐 agent 错过的变更(§4.1)。
///
/// 两个 revision **分别比对、分别补齐**:
///   * `config_revision` 不一致 → `config.apply`(会重建 box)
///   * `user_state_revision` 不一致 → `user.state`(只翻内存标记,不重建)
///
/// 顺序上先 config 后 user_state:若 config.apply 重建了 box,
/// 让禁用名单在那之后到达更稳(§7.5 保证 disabled 是独立字段不随重建丢失,
/// 但这个顺序零成本,取更稳的那个)。
///
/// **失败不重试。** 下次握手会再比一次 revision,自然重来(§4.2:
/// 失败时主控保留旧 revision,不标记为成功)。
async fn catch_up(
    agent_id: i64,
    state: &ServerState,
    agent_config_rev: i64,
    agent_user_rev: i64,
    master: Revisions,
) {
    if agent_config_rev == master.config_revision {
        // 版本已经一致,不用发。但要把这个事实记在连接上 ——
        // 巡检拿 `sent_config_rev` 和库里的值比,留着 `None` 会被当成
        // 「这条连接还没下发过」,于是白发一次(重建 box 是有代价的)。
        state.registry.lock().await.mark_config_sent(agent_id, agent_config_rev);
    } else {
        tracing::info!(
            agent_id,
            agent_rev = agent_config_rev,
            master_rev = master.config_revision,
            "配置版本不一致,下发 config.apply"
        );
        match crate::service::build_agent_config(&state.pool, agent_id).await {
            Ok(options) => {
                let payload = serde_json::json!({
                    "revision": master.config_revision,
                    "options": options,
                });
                match state
                    .rpc
                    .call_default(&state.registry, agent_id, method::CONFIG_APPLY, payload)
                    .await
                {
                    Ok(_) => {
                        // 记在连接上,否则巡检看不到「已经发过了」,会每 30 秒
                        // 重发同一个 revision —— 那等于让在线的机器不停重建 box。
                        state
                            .registry
                            .lock()
                            .await
                            .mark_config_sent(agent_id, master.config_revision);
                        tracing::info!(
                            agent_id,
                            rev = master.config_revision,
                            "config.apply 已生效"
                        );
                    }
                    Err(e) => {
                        // agent 回的 error 里带着 box.New 的失败原文(§4.2),值得留痕:
                        // 「配置下发失败」是运维要立刻知道的事。
                        tracing::error!(agent_id, error = %e, "config.apply 失败");
                        let now = chrono::Local::now().timestamp();
                        let _ = crate::db::agent_repo::log_event(
                            &state.pool,
                            Some(agent_id),
                            "config_apply_failed",
                            &e.to_string(),
                            now,
                        )
                        .await;
                    }
                }
            }
            Err(e) => {
                // 组装阶段就失败(节点缺必填参数、或协议名本版本读不懂),根本没发出去。
                tracing::error!(agent_id, error = %e, "组装配置失败,未下发");
                let now = chrono::Local::now().timestamp();
                let _ = crate::db::agent_repo::log_event(
                    &state.pool,
                    Some(agent_id),
                    "config_build_failed",
                    &e.to_string(),
                    now,
                )
                .await;
            }
        }
    }

    if agent_user_rev == master.user_state_revision {
        // 已经一致,记在连接上,免得巡检把它当成「没下发过」白发一次(理由同上)。
        state.registry.lock().await.mark_user_sent(agent_id, agent_user_rev);
    } else {
        tracing::info!(
            agent_id,
            agent_rev = agent_user_rev,
            master_rev = master.user_state_revision,
            "禁用名单版本不一致,下发 user.state"
        );
        match crate::service::disabled_users(&state.pool).await {
            Ok(disabled) => {
                let payload = serde_json::json!({
                    "user_state_revision": master.user_state_revision,
                    "disabled": disabled,
                });
                match state
                    .rpc
                    .call_default(&state.registry, agent_id, method::USER_STATE, payload)
                    .await
                {
                    Ok(_) => {
                        state
                            .registry
                            .lock()
                            .await
                            .mark_user_sent(agent_id, master.user_state_revision);
                        tracing::info!(
                            agent_id,
                            rev = master.user_state_revision,
                            count = disabled.len(),
                            "user.state 已生效"
                        );
                    }
                    Err(e) => tracing::error!(agent_id, error = %e, "user.state 失败"),
                }
            }
            Err(e) => tracing::error!(agent_id, error = %e, "查询禁用名单失败"),
        }
    }
}

struct Authenticated {
    agent_id: i64,
    hello: AgentHello,
    /// 握手帧的 id,`hello_ack` 要用同一个回。
    hello_id: String,
}

enum HandshakeError {
    /// 认证/协议层面的拒绝。**不向连接方透露原因**(§8.1)。
    Rejected(&'static str),
    /// 传输层错误,直接向上抛。
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for HandshakeError {
    fn from(e: anyhow::Error) -> Self {
        HandshakeError::Fatal(e)
    }
}

async fn handshake(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    state: &ServerState,
) -> Result<Authenticated, HandshakeError> {
    // 第一帧必须在 10 秒内到达(§4.1)。没有这个超时,半开连接会一直占着一个任务。
    let first =
        tokio::time::timeout(std::time::Duration::from_secs(HELLO_TIMEOUT_SECS), stream.next())
            .await
            .map_err(|_| HandshakeError::Rejected("握手超时"))?
            .ok_or(HandshakeError::Rejected("连接在握手前关闭"))?
            .map_err(|e| HandshakeError::Fatal(e.into()))?;

    let Message::Text(text) = first else {
        return Err(HandshakeError::Rejected("第一帧必须是 text"));
    };

    let env: Envelope =
        serde_json::from_str(&text).map_err(|_| HandshakeError::Rejected("第一帧不是合法信封"))?;

    // 版本不匹配即断开,不做协商(§4)。
    env.validate().map_err(HandshakeError::Rejected)?;

    if env.method != method::AGENT_HELLO || env.kind != Kind::Req {
        return Err(HandshakeError::Rejected("第一帧必须是 agent.hello 请求"));
    }
    let hello_id = env.id.clone().ok_or(HandshakeError::Rejected("hello 缺少 id"))?;
    let hello: AgentHello = serde_json::from_value(env.payload)
        .map_err(|_| HandshakeError::Rejected("hello payload 不合法"))?;

    if hello.proto_version != sbx_shared::PROTO_VERSION {
        return Err(HandshakeError::Rejected("协议版本不符"));
    }

    let agent = crate::db::agent_repo::find_by_token(&state.pool, &hello.token)
        .await
        .map_err(HandshakeError::Fatal)?;

    let Some(agent) = agent else {
        // 认证失败要留痕(§8.1),但记的是「有人用错 token 连过来」,
        // 不记 token 本身(凭据不进日志,§11.3)。
        let now = chrono::Local::now().timestamp();
        let msg = format!(
            "token 认证失败(hostname={}, prefix={}…)",
            hello.hostname,
            crate::cluster::token::prefix_of(&hello.token)
        );
        let _ = crate::db::agent_repo::log_event(&state.pool, None, "auth_failed", &msg, now).await;
        return Err(HandshakeError::Rejected("token 认证失败"));
    };

    Ok(Authenticated { agent_id: agent.id, hello, hello_id })
}

/// 判定「半开连接」的静默上限:心跳的 3 倍,不低于 30 秒。
///
/// **与 agent 侧的 `readLoop` 用同一个公式**(`conn.go`:`idle := 3*heartbeat`,
/// 下限 30s)。两边取同一个值,才不会出现「一边已经认定断了、另一边还以为连着」
/// 的一段窗口。3 倍是为了让**一次丢包不断线** —— agent 每 heartbeat_secs 发一条
/// pong,连丢两条才会触发。
pub fn idle_limit(heartbeat_secs: u64) -> Duration {
    Duration::from_secs((heartbeat_secs.saturating_mul(3)).max(30))
}

async fn recv_loop(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    agent_id: i64,
    state: &ServerState,
) -> Result<()> {
    // **读超时。少了它,机器突然消失时这条连接会永远挂着。**
    //
    // TCP 只在对端发了 FIN/RST 时才让读返回。断电、拔网线、网络分区、
    // NAT/防火墙把连接表项丢掉、虚拟机挂起 —— 这些情况下什么都不会到达,
    // `stream.next()` 就一直 pending:agent 早已不在,而主控这边
    // `recv_loop` 不返回 → 清理不跑 → `mark_offline` 不执行 →
    // **服务管理页上那盏绿灯一直亮着**,直到 daemon 重启或者 OS 的 TCP
    // keepalive 兜底(Linux 默认 2 小时 11 分)。
    //
    // agent 侧一直有这道防线(`conn.go` 的 `idle`),主控侧没有 —— 这次补上,
    // 两边用同一个公式。dispatch 里 PONG 分支说的「超时判定由外层的心跳巡检做」
    // 指的就是这里。
    let idle = state.idle_limit;
    loop {
        let msg = match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(msg)) => msg,
            // 对端正常关闭:走正常退出,不是错误。
            Ok(None) => return Ok(()),
            Err(_) => anyhow::bail!("{}s 没收到任何帧(心跳都停了),判定为半开连接", idle.as_secs()),
        };
        let Message::Text(text) = msg? else {
            // 二进制/ping/pong 帧忽略。协议是 text frame JSON(§4)。
            continue;
        };
        let env: Envelope = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(e) => {
                // 单条无法解析的消息不该毒死整个连接——记一条就继续。
                tracing::warn!(agent_id, error = %e, "收到无法解析的信封,已忽略");
                continue;
            }
        };
        if let Err(reason) = env.validate() {
            // 版本不匹配必须断开(§4)。
            anyhow::bail!("信封校验失败:{reason}");
        }
        // **上报之前先确认这个 agent 还在库里。**
        //
        // TUI 与 daemon 是两个进程,只共享库。在 TUI 里删掉一台 agent 不会动
        // daemon 内存里那条 WebSocket —— 它会继续上报,而上报路径只按 agent_id
        // 做 UPSERT,不查 `agents` 表。于是那些数字落到一个已经不存在的 agent
        // 头上;更糟的是 id 一旦被复用(009 之前 agents.id 没有 AUTOINCREMENT),
        // 它们会落进**下一台新加的机器**的行里 ——
        // 现场表现是「新机器还没装,页面上流量就在涨」。
        //
        // 只在**上报**这一步查,不是每条消息都查:上报按固定间隔来(默认 30s),
        // 一次主键命中的开销可以忽略;resp / log 这些消息不该为此多查一次库。
        if matches!(env.method.as_str(), method::STATS_REPORT | method::SYSINFO_REPORT)
            && !agent_exists(&state.pool, agent_id).await
        {
            anyhow::bail!("agent 已从库里删除,断开这条连接");
        }
        dispatch(env, agent_id, state).await;
    }
}

/// 这个 agent 还在库里吗。
///
/// **查库失败时返回 `true`(当作还在)。** 一次磁盘忙不该把一台正常的机器
/// 踢下线 —— 它下一个上报周期还会再来。反过来(失败即断开)会让数据库的
/// 短暂抖动变成一场集体掉线。
async fn agent_exists(pool: &SqlitePool, agent_id: i64) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT 1 FROM agents WHERE id = ?")
        .bind(agent_id)
        .fetch_optional(pool)
        .await
        .map(|r| r.is_some())
        .unwrap_or(true)
}

/// 分发一条已校验的信封。
///
/// **单条消息的处理失败不该断开连接。** 一次 ingest 因为磁盘忙而失败,
/// 下一次 30s 后的上报会带着同样的累计值再来一遍(§5.3 的直接收益:
/// agent 上报单调累计值,主控没记上就重算)。所以这里所有错误都是记日志然后继续。
async fn dispatch(env: Envelope, agent_id: i64, state: &ServerState) {
    // resp 先于 method 判断:它要按 id 关联回等待方,method 只是给日志看的。
    if env.kind == Kind::Resp {
        if !state.rpc.resolve(env).await {
            // 通常是已超时请求的迟到回应。频繁出现说明超时设得太短。
            tracing::trace!(agent_id, "收到无人等待的 resp,已丢弃");
        }
        return;
    }

    let now = chrono::Local::now().timestamp();
    match env.method.as_str() {
        method::STATS_REPORT => match serde_json::from_value(env.payload) {
            Ok(r) => ingest_stats(agent_id, state, &r, now).await,
            Err(e) => tracing::warn!(agent_id, error = %e, "stats.report payload 不合法"),
        },
        method::SYSINFO_REPORT => match serde_json::from_value(env.payload) {
            Ok(r) => ingest_sysinfo(agent_id, state, &r, now).await,
            Err(e) => tracing::warn!(agent_id, error = %e, "sysinfo.report payload 不合法"),
        },
        method::BOX_EVENT => {
            let state_str = env.payload.get("state").and_then(|v| v.as_str()).unwrap_or("?");
            let message = env.payload.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let msg = format!("box {state_str}: {message}");
            tracing::info!(agent_id, "{msg}");
            let _ = crate::db::agent_repo::log_event(
                &state.pool,
                Some(agent_id),
                "box_event",
                &msg,
                now,
            )
            .await;
        }
        method::LOG => {
            // agent 侧已节流(仅 warn 以上,§4.3)。不落库——日志量不可控,
            // 落库会让 agent_events 表被日志淹没,而它是审计表。
            let level = env.payload.get("level").and_then(|v| v.as_str()).unwrap_or("info");
            let line = env.payload.get("line").and_then(|v| v.as_str()).unwrap_or("");
            tracing::info!(agent_id, level, "agent: {line}");
        }
        method::PONG => {
            // 心跳记账:只更新 last_seen。**超时判定在 `recv_loop` 的读超时那里**
            // (`idle_limit`)—— 连着 idle 那么久一条 pong 都没来,那条连接
            // 就会被判成半开并走正常的清理路径(注销 + mark_offline)。
            let _ = sqlx::query("UPDATE agents SET last_seen = ? WHERE id = ?")
                .bind(now)
                .bind(agent_id)
                .execute(&state.pool)
                .await;
        }
        other => {
            // 未知 method 不断开连接(§4 只要求版本不符才断)。
            // agent 比主控新时会出现这种情况,忽略比崩掉好。
            tracing::warn!(agent_id, method = other, "未知 method,已忽略");
        }
    }
}

async fn ingest_stats(agent_id: i64, state: &ServerState, r: &sbx_shared::StatsReport, now: i64) {
    match crate::cluster::ingest::ingest_stats(&state.pool, agent_id, r, now).await {
        Ok(out) => {
            if out.epoch_changed {
                // §5.4:这条记录是低频且高信息量的——它出现就意味着真的发生了重启。
                // 若频繁出现,说明 §7.1 的 tracker 生命周期写错了(被 box 连带重建了)。
                tracing::info!(agent_id, "用户流量计数器已重置(agent 进程重启)");
            }
            tracing::debug!(agent_id, applied = out.applied, unknown = out.unknown, "stats 已入库");
        }
        Err(e) => tracing::error!(agent_id, error = %e, "stats 入库失败(下次上报会重来)"),
    }
}

async fn ingest_sysinfo(
    agent_id: i64,
    state: &ServerState,
    r: &sbx_shared::SysinfoReport,
    now: i64,
) {
    match crate::cluster::ingest::ingest_sysinfo(&state.pool, agent_id, r, now).await {
        Ok(out) => {
            if out.epoch_changed {
                tracing::info!(agent_id, "网卡计数器已重置(机器重启)");
                // boot_id 变化的那一次**不产生网速采样**,否则会算出一个爆炸数字(§8.2)。
                state.speed.lock().await.remove(&agent_id);
            } else {
                update_speed(agent_id, state, &out, now).await;
            }
        }
        Err(e) => tracing::error!(agent_id, error = %e, "sysinfo 入库失败"),
    }
}

/// 用相邻两次上报的增量 ÷ 时间间隔算网速,只存内存(§8.2)。
///
/// 得到的是**上报周期的平均值**(默认 30s),不是瞬时速率。
async fn update_speed(
    agent_id: i64,
    state: &ServerState,
    out: &crate::cluster::ingest::SysinfoOutcome,
    now: i64,
) {
    let mut map = state.speed.lock().await;
    let elapsed = match map.get(&agent_id) {
        Some(prev) => now - prev.at,
        // 首次采样没有前一个时刻,用配置的上报周期作估计值。
        None => state.report_interval_secs as i64,
    };
    // 时钟回拨或同一秒内两次上报时,除以 0 会得到 inf。取 1 秒兜底。
    let secs = elapsed.max(1) as f64;
    map.insert(
        agent_id,
        SpeedSample {
            at: now,
            rx_per_sec: out.rx_delta as f64 / secs,
            tx_per_sec: out.tx_delta as f64 / secs,
        },
    );
}

struct Revisions {
    config_revision: i64,
    user_state_revision: i64,
}

async fn load_revisions(pool: &SqlitePool, agent_id: i64) -> Result<Revisions> {
    let row: (i64, i64) =
        sqlx::query_as("SELECT config_revision, user_state_revision FROM agents WHERE id = ?")
            .bind(agent_id)
            .fetch_one(pool)
            .await?;
    Ok(Revisions { config_revision: row.0, user_state_revision: row.1 })
}

async fn mark_online(pool: &SqlitePool, agent_id: i64, hello: &AgentHello, now: i64) -> Result<()> {
    // ipv4/ipv6 用 COALESCE(?, 旧值):agent 探测失败时上报 None,
    // 不该把库里已有的(可能是管理员手工填的)地址擦掉(§7.3「允许手工覆盖」)。
    //
    // reported_utc_offset_secs **不用 COALESCE**,故意与上面两列不同:IP 缺失是
    // 「这次探测失败了」,保留旧值是对的;而偏移缺失是「这个 agent 老得不会报」,
    // 留着一个没人再声称过的值只会骗人 —— 界面会显示「agent 上报 -07:00」,
    // 而那台机器几个月前就不这么说了。这一列的语义是「它最后一次告诉我们什么」。
    sqlx::query(
        "UPDATE agents SET
           status = 'online', last_seen = ?,
           agent_version = ?, singbox_version = ?, os = ?, arch = ?, hostname = ?,
           ipv4 = COALESCE(?, ipv4), ipv6 = COALESCE(?, ipv6),
           reported_utc_offset_secs = ?
         WHERE id = ?",
    )
    .bind(now)
    .bind(&hello.agent_version)
    .bind(&hello.singbox_version)
    .bind(&hello.os)
    .bind(&hello.arch)
    .bind(&hello.hostname)
    .bind(&hello.ipv4)
    .bind(&hello.ipv6)
    .bind(sane_offset(agent_id, hello.utc_offset_secs))
    .bind(agent_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 收下 agent 报的 UTC 偏移,离谱的值丢掉并留一条日志。
///
/// `FixedOffset` 的上限是 ±24h,超出的值存进去只会在 `model::agent::reset_offset`
/// 那边被静默丢弃 —— 那时已经离现场很远了。在入口挡住并说出来,坏掉的 agent
/// 才能在日志里被找到,而不是表现成「这台机器的重置边界莫名回落到主控时区」。
fn sane_offset(agent_id: i64, secs: Option<i64>) -> Option<i64> {
    let s = secs?;
    if i32::try_from(s).is_ok_and(|s| chrono::FixedOffset::east_opt(s).is_some()) {
        Some(s)
    } else {
        tracing::warn!(agent_id, secs = s, "agent 上报的 UTC 偏移越界,已忽略");
        None
    }
}

async fn mark_offline(pool: &SqlitePool, agent_id: i64, now: i64) -> Result<()> {
    sqlx::query("UPDATE agents SET status = 'offline', last_seen = ? WHERE id = ?")
        .bind(now)
        .bind(agent_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-server-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    /// 只为直接调 `cleanup_connection` 这类函数用:不起监听、不接真 socket。
    fn state_with(pool: SqlitePool) -> ServerState {
        ServerState {
            pool,
            registry: Arc::new(Mutex::new(Registry::new())),
            rpc: Arc::new(crate::cluster::Rpc::new()),
            speed: Arc::new(Mutex::new(std::collections::HashMap::new())),
            heartbeat_secs: 10,
            report_interval_secs: 30,
            idle_limit: idle_limit(10),
        }
    }

    fn hello(token: &str) -> AgentHello {
        AgentHello {
            token: token.into(),
            agent_version: "0.1.0".into(),
            proto_version: sbx_shared::PROTO_VERSION,
            os: "linux".into(),
            arch: "amd64".into(),
            hostname: "vps-1".into(),
            boot_id: "boot-abc".into(),
            singbox_version: "1.14.0".into(),
            config_revision: 0,
            user_state_revision: 0,
            ipv4: Some("203.0.113.7".into()),
            ipv6: None,
            utc_offset_secs: None,
        }
    }

    /// 被删掉的 agent 上再来的上报要被认出来(连接随即断开)。
    ///
    /// 这是「删掉一台机器、重新添加同一台」那个 bug 的另一半:TUI 删行时
    /// 动不了 daemon 内存里的 WebSocket,那台 VPS 上的老 agent 进程还在跑、
    /// 还在按原来的 agent_id 上报。009 让 id 不再复用之后,这些上报会落到
    /// 一个不存在的 id 上;这里负责把连接断掉,而不是静默写进流量表。
    #[tokio::test]
    async fn reports_from_a_deleted_agent_are_refused() {
        let p = pool().await;
        let (id, _) = crate::db::agent_repo::create(&p, "gone", 0).await.unwrap();
        assert!(agent_exists(&p, id).await, "刚建好的 agent 该在");

        crate::db::agent_repo::delete(&p, id).await.unwrap();
        assert!(!agent_exists(&p, id).await, "删掉之后就不该认它了");
    }

    /// 查库失败时**当作还在**。
    ///
    /// 反过来(失败即断开)会把数据库的一次短暂抖动放大成一场集体掉线,
    /// 而这些 agent 本来什么毛病都没有。
    #[tokio::test]
    async fn a_broken_database_does_not_kick_everyone_offline() {
        let p = pool().await;
        let (id, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        p.close().await; // 池关掉 = 之后每次查询都失败
        assert!(agent_exists(&p, id).await, "查不动库时不该把 agent 判死");
    }

    /// **清理必须先注销,再动数据库。**
    ///
    /// 顺序反过来的话,一次 `mark_offline` 失败(磁盘忙、库被关掉)会让 registry 里
    /// 留下一个幽灵条目:`is_online()` 是 true、`send()` 却永远 false。那比
    /// 「状态字段没更新」难查得多 —— 状态字段下一次握手就会被改对,而幽灵条目会让
    /// 仪表盘的在线数一直虚高,直到那台 agent 重连把它驱逐掉。
    #[tokio::test]
    async fn cleanup_unregisters_even_when_the_database_is_gone() {
        let p = pool().await;
        let (id, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let state = state_with(p.clone());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let epoch = state.registry.lock().await.register(id, tx).epoch;
        assert!(state.registry.lock().await.is_online(id));

        p.close().await; // 之后每次查询都失败
        let err = cleanup_connection(&state, id, epoch).await;
        assert!(err.is_err(), "标离线该报错报出来,不该悄悄吞掉");
        assert!(
            !state.registry.lock().await.is_online(id),
            "库挂了也必须先把连接注销掉,不能留幽灵条目"
        );
    }

    /// 陈旧的清理**不能**把已经接上的新连接踢掉。
    ///
    /// 与 registry 的 stale-unregister 测试是同一条性质,但守的是 `cleanup_connection`
    /// 这一层:它除了注销还会 `cancel_agent`,而那是按 agent_id 清的 ——
    /// 漏掉 epoch 判断就会把新连接正在等的 RPC 一起打掉。
    #[tokio::test]
    async fn a_stale_cleanup_leaves_the_live_connection_alone() {
        let p = pool().await;
        let (id, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let state = state_with(p.clone());
        let (tx1, _rx1) = tokio::sync::mpsc::unbounded_channel();
        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        let old = state.registry.lock().await.register(id, tx1).epoch;
        state.registry.lock().await.register(id, tx2); // 新连接驱逐旧的
        mark_online(&p, id, &hello("t"), 100).await.unwrap();

        cleanup_connection(&state, id, old).await.unwrap();

        assert!(state.registry.lock().await.is_online(id), "新连接必须还在线");
        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert!(a.is_online(), "陈旧的清理不该把状态改成离线");
    }

    #[tokio::test]
    async fn mark_online_records_hello_fields() {
        let p = pool().await;
        let (id, tok) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        mark_online(&p, id, &hello(&tok), 1_700_000_000).await.unwrap();

        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert!(a.is_online());
        assert_eq!(a.last_seen, Some(1_700_000_000));
        assert_eq!(a.hostname.as_deref(), Some("vps-1"));
        assert_eq!(a.ipv4.as_deref(), Some("203.0.113.7"));
        assert_eq!(a.singbox_version.as_deref(), Some("1.14.0"));
    }

    /// agent 上报的 UTC 偏移要落进库里 —— 网卡月重置的边界默认按它算(§6.4)。
    #[tokio::test]
    async fn mark_online_records_the_reported_utc_offset() {
        let p = pool().await;
        let (id, tok) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let mut h = hello(&tok);
        h.utc_offset_secs = Some(-7 * 3600);
        mark_online(&p, id, &h, 1).await.unwrap();

        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert_eq!(a.reported_utc_offset_secs, Some(-25200));
        assert_eq!(a.nic_reset_offset_secs, None, "握手不该动人手工填的那一列");
    }

    /// **这一列故意不用 COALESCE**,与 ipv4/ipv6 相反。
    ///
    /// IP 缺失是「这次探测失败了」,保留旧值是对的;偏移缺失是「这个 agent 老得
    /// 不认识这个字段」,留着一个它已经不再声称的值只会骗人 —— 界面会显示
    /// 「agent 上报 -07:00」,而那台机器降级之后早就不这么说了。
    #[tokio::test]
    async fn a_hello_without_an_offset_clears_the_reported_column() {
        let p = pool().await;
        let (id, tok) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        sqlx::query("UPDATE agents SET reported_utc_offset_secs = -25200 WHERE id = ?")
            .bind(id)
            .execute(&p)
            .await
            .unwrap();

        let mut h = hello(&tok);
        h.utc_offset_secs = None; // 老版本 agent
        mark_online(&p, id, &h, 1).await.unwrap();

        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert_eq!(a.reported_utc_offset_secs, None, "老 agent 连上后该清成「没报过」");
    }

    /// 越界的偏移在入口就挡掉并留日志。存进去只会在 `reset_offset` 那边被静默丢弃,
    /// 那时离现场已经很远,表现成「这台机器的边界莫名回落到主控时区」。
    #[tokio::test]
    async fn an_absurd_reported_offset_is_dropped_not_stored() {
        let p = pool().await;
        let (id, tok) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        let mut h = hello(&tok);
        h.utc_offset_secs = Some(99 * 3600);
        mark_online(&p, id, &h, 1).await.unwrap();

        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert_eq!(a.reported_utc_offset_secs, None);
        // 边界值必须收下:parse_timezone 能接受到 ±14:59。
        h.utc_offset_secs = Some(14 * 3600 + 59 * 60);
        mark_online(&p, id, &h, 2).await.unwrap();
        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert_eq!(a.reported_utc_offset_secs, Some(53_940));
    }

    /// agent 探测失败(ipv4 = None)时不能擦掉库里已有的地址——
    /// 那可能是管理员手工填的(§7.3)。
    #[tokio::test]
    async fn mark_online_does_not_clobber_manually_set_ips() {
        let p = pool().await;
        let (id, tok) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        sqlx::query("UPDATE agents SET ipv4 = '198.51.100.1', ipv6 = '2001:db8::1' WHERE id = ?")
            .bind(id)
            .execute(&p)
            .await
            .unwrap();

        let mut h = hello(&tok);
        h.ipv4 = None; // 探测失败
        h.ipv6 = None;
        mark_online(&p, id, &h, 1).await.unwrap();

        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert_eq!(a.ipv4.as_deref(), Some("198.51.100.1"), "不该被 None 擦掉");
        assert_eq!(a.ipv6.as_deref(), Some("2001:db8::1"));
    }

    #[tokio::test]
    async fn offline_marking_keeps_last_seen() {
        let p = pool().await;
        let (id, tok) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        mark_online(&p, id, &hello(&tok), 100).await.unwrap();
        mark_offline(&p, id, 200).await.unwrap();

        let a = crate::db::agent_repo::get(&p, id).await.unwrap().unwrap();
        assert!(!a.is_online());
        assert_eq!(a.last_seen, Some(200));
    }

    /// **静默上限的公式要和 agent 侧一模一样。**
    ///
    /// agent 的 `readLoop` 用 `3*heartbeat`、下限 30s。两边取值不同的话,
    /// 中间会有一段「一边认定断了、另一边还以为连着」的窗口 ——
    /// 那期间主控会把 config.apply 发给一个已经在重连的 agent。
    #[test]
    fn the_idle_limit_matches_the_agent_side_formula() {
        assert_eq!(idle_limit(10), Duration::from_secs(30), "默认心跳 10s → 下限 30s");
        assert_eq!(idle_limit(1), Duration::from_secs(30), "3 倍不足 30s 时取下限");
        assert_eq!(idle_limit(20), Duration::from_secs(60), "超过下限之后按 3 倍走");
        // heartbeat_secs 是配置项,填 0 也不该让上限变成 0(那会秒断所有连接)。
        assert_eq!(idle_limit(0), Duration::from_secs(30), "0 也要落到下限");
    }

    /// **机器突然消失时,那盏绿灯必须自己灭掉。**
    ///
    /// 断电、拔网线、网络分区、NAT 丢表项 —— 这些情况��不会有 FIN/RST 到达,
    /// `stream.next()` 永远 pending。在补上读超时之前,`recv_loop` 不返回 →
    /// 清理不跑 → `mark_offline` 不执行 → 服务管理页上一直是 online,
    /// 直到 daemon 重启或者 OS 的 TCP keepalive 兜底(Linux 默认 2 小时 11 分)。
    ///
    /// 这里用「连上、握完手、然后一个字都不发」来模拟那台消失的机器 ——
    /// **不是 drop 连接**:drop 会发 FIN,那正是原来就能处理的那条路径,
    /// 测不到这个 bug。
    #[tokio::test]
    async fn a_silently_vanished_agent_is_marked_offline() {
        let (url, pool) = spawn_server_with_idle(Duration::from_millis(300)).await;
        let (id, token) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        let mut ws = connect(&url).await;
        assert!(do_hello(&mut ws, &hello(&token)).await.is_ok());
        assert!(
            crate::db::agent_repo::get(&pool, id).await.unwrap().unwrap().is_online(),
            "握手之后该是 online"
        );

        // 握着这条连接,什么都不发 —— 机器还在,但已经不说话了。
        wait_for("静默超时之后该被判成 offline", || async {
            !crate::db::agent_repo::get(&pool, id).await.unwrap().unwrap().is_online()
        })
        .await;

        // 连接还在我们手里(没 drop),证明它是被**读超时**判掉的,
        // 而不是靠对端关闭那条老路径。
        drop(ws);
    }

    /// 反过来:**还在发心跳的 agent 不能被误判**。
    ///
    /// 读超时设太紧或者刷新点漏了,表现就是好好的机器每隔一会儿掉一次线;
    /// 那比「绿灯不灭」更糟 —— 每次重连都会触发一次 catch_up。
    #[tokio::test]
    async fn a_heartbeating_agent_is_never_reaped() {
        let (url, pool) = spawn_server_with_idle(Duration::from_millis(400)).await;
        let (id, token) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        let mut ws = connect(&url).await;
        assert!(do_hello(&mut ws, &hello(&token)).await.is_ok());

        // 按远快于上限的节奏发 pong,持续到超过上限好几倍。
        for _ in 0..12 {
            let env = Envelope::event(method::PONG, serde_json::json!({ "echo_ts": 1 }));
            ws.send(tungstenite::Message::Text(serde_json::to_string(&env).unwrap()))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                crate::db::agent_repo::get(&pool, id).await.unwrap().unwrap().is_online(),
                "还在发心跳的 agent 不该被判成离线"
            );
        }
    }

    /// 两个 revision 必须能分别读出来(§4.1)。
    #[tokio::test]
    async fn revisions_load_independently() {
        let p = pool().await;
        let (id, _) = crate::db::agent_repo::create(&p, "a", 0).await.unwrap();
        sqlx::query("UPDATE agents SET config_revision = 5, user_state_revision = 9 WHERE id = ?")
            .bind(id)
            .execute(&p)
            .await
            .unwrap();

        let r = load_revisions(&p, id).await.unwrap();
        assert_eq!(r.config_revision, 5);
        assert_eq!(r.user_state_revision, 9);
    }

    // ───────────────── 端到端:真的起一个 WS 服务端连上去 ─────────────────
    //
    // 这些测试走完整路径:TcpListener → axum → upgrade → handshake → registry。
    // 比起单测 `mark_online` 之类的辅助函数,它们能抓到「握手顺序写错」
    // 「payload 字段名对不上」这类只在真实收发时才暴露的问题。

    use tokio_tungstenite::tungstenite;

    /// 等一个条件成立,而不是睡一个固定时长。
    ///
    /// 这几个测试要等的都是**别的任务写完库**(驱逐清理、断连标记、组装失败记审计)。
    /// 早先的写法是 `sleep(200ms)` 再断言,那有两个方向的毛病:
    ///   * 机器一忙(CI 上并发跑、本地刚编译完)就可能还没写完 —— 假失败。
    ///     v0.3.4 打 tag 那次的 `1 failed` 大概率就是这么来的,而它之后 47 次
    ///     重跑全绿,于是「红了就重跑」变成习惯,下次真回归也会被同样放过去。
    ///   * 反过来,条件一毫秒就满足时也要白等 200ms。
    ///
    /// 轮询到条件成立就立刻返回,超时才失败 —— 快路径更快,慢机器上更稳。
    /// 上限给到 5 秒:比任何一次正常的库写入都长得多,真卡住了也能在测试里
    /// 报出一句人话,而不是一个「断言失败」的空壳。
    async fn wait_for<F, Fut>(what: &str, mut cond: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if cond().await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("等了 5 秒也没等到:{what}");
    }

    /// 在一个窗口内反复断言条件**一直**成立。
    ///
    /// 给「某件事不要发生」用。这类断言不能用 `wait_for`:条件一开始就是真的,
    /// 轮询会立刻返回,而那条可能出问题的路径还没来得及跑 —— 测试永远绿,
    /// 却什么都没验证。这里反过来:在窗口里反复查,只要塌过一次就抓到。
    ///
    /// 窗口固定 300ms。这一条确实要花掉这点时间(不像 `wait_for` 有快路径),
    /// 但它是**唯一**能测「不该发生的事没发生」的办法。
    async fn stays_true<F, Fut>(what: &str, mut cond: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < deadline {
            assert!(cond().await, "条件中途塌了:{what}");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// 起一个 server,返回 (ws_url, pool)。测试结束时 server 随 runtime 一起销毁。
    async fn spawn_server() -> (String, SqlitePool) {
        // 真实公式给的是 30s。绝大多数用例在这个窗口里跑完,不受影响;
        // 专门验半开检测的那条用自己的短值(`spawn_server_with_idle`)。
        spawn_server_with_idle(idle_limit(10)).await
    }

    async fn spawn_server_with_idle(idle: Duration) -> (String, SqlitePool) {
        let pool = pool().await;
        let state = ServerState {
            pool: pool.clone(),
            registry: Arc::new(Mutex::new(Registry::new())),
            rpc: Arc::new(crate::cluster::Rpc::new()),
            speed: Arc::new(Mutex::new(std::collections::HashMap::new())),
            heartbeat_secs: 10,
            report_interval_secs: 30,
            idle_limit: idle,
        };
        // 端口 0 = 让内核分配一个空闲端口,避免测试并发时抢端口。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });
        (format!("ws://{addr}/ws"), pool)
    }

    async fn connect(
        url: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        ws
    }

    /// 发一个 agent.hello 并等回应。返回 (是否成功, 回应信封)。
    async fn do_hello(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        h: &AgentHello,
    ) -> Envelope {
        let req = Envelope::req("h1", method::AGENT_HELLO, serde_json::to_value(h).unwrap());
        ws.send(tungstenite::Message::Text(serde_json::to_string(&req).unwrap())).await.unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("等回应超时")
            .expect("连接已关闭")
            .unwrap();
        let tungstenite::Message::Text(t) = msg else {
            panic!("回应不是 text 帧: {msg:?}");
        };
        serde_json::from_str(&t).unwrap()
    }

    #[tokio::test]
    async fn valid_token_completes_handshake_and_marks_online() {
        let (url, pool) = spawn_server().await;
        let (id, token) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        let mut ws = connect(&url).await;
        let resp = do_hello(&mut ws, &hello(&token)).await;

        assert!(resp.is_ok(), "握手应成功,得到 error: {:?}", resp.error);
        assert_eq!(resp.method, method::AGENT_HELLO_ACK);
        assert_eq!(resp.id.as_deref(), Some("h1"), "resp 必须回同一个 id(§4)");

        let ack: AgentHelloAck = serde_json::from_value(resp.payload).unwrap();
        assert_eq!(ack.agent_id, id);
        assert_eq!(ack.heartbeat_secs, 10);
        assert_eq!(ack.report_interval_secs, 30);

        // 库里应已标 online
        let a = crate::db::agent_repo::get(&pool, id).await.unwrap().unwrap();
        assert!(a.is_online(), "握手成功后应标记为 online");
        assert_eq!(a.hostname.as_deref(), Some("vps-1"));
    }

    /// 错 token 必须被拒,且**不透露具体原因**(§8.1 避免枚举)。
    #[tokio::test]
    async fn bad_token_is_rejected_without_revealing_why() {
        let (url, pool) = spawn_server().await;
        crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        let mut ws = connect(&url).await;
        let resp = do_hello(&mut ws, &hello("wrong-token")).await;

        assert!(!resp.is_ok(), "错误 token 必须被拒");
        let err = resp.error.unwrap();
        assert_eq!(err, "握手被拒绝", "不该透露失败细节,得到: {err}");
        assert!(!err.contains("token"), "错误信息不该提到 token: {err}");

        // 认证失败要留痕(§8.1)
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE kind = 'auth_failed'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 1, "认证失败应记一条 agent_events");
    }

    /// 认证失败的审计记录里**不能有 token 明文**(§11.3 凭据处理)。
    #[tokio::test]
    async fn auth_failure_audit_does_not_leak_the_token() {
        let (url, pool) = spawn_server().await;
        crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        let secret = "SuperSecretTokenValue123456789012345678";
        let mut ws = connect(&url).await;
        let _ = do_hello(&mut ws, &hello(secret)).await;

        let msg: String =
            sqlx::query_scalar("SELECT message FROM agent_events WHERE kind = 'auth_failed'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!msg.contains(secret), "审计记录泄露了 token 明文: {msg}");
        // 只该有前 8 位用于排查
        assert!(msg.contains("SuperSec"), "应保留 prefix 便于排查: {msg}");
    }

    /// 协议版本不符必须被拒(§4:不做协商、不做向下兼容层)。
    #[tokio::test]
    async fn protocol_version_mismatch_is_rejected() {
        let (url, pool) = spawn_server().await;
        let (_, token) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        let mut h = hello(&token);
        h.proto_version = sbx_shared::PROTO_VERSION + 1;
        let mut ws = connect(&url).await;
        let resp = do_hello(&mut ws, &h).await;
        assert!(!resp.is_ok(), "版本不符必须被拒");
    }

    /// 第一帧不是 agent.hello 时必须被拒(§4.1)。
    #[tokio::test]
    async fn first_frame_must_be_hello() {
        let (url, _pool) = spawn_server().await;
        let mut ws = connect(&url).await;

        let req = Envelope::req("x", method::BOX_STATUS, serde_json::Value::Null);
        ws.send(tungstenite::Message::Text(serde_json::to_string(&req).unwrap())).await.unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("等回应超时")
            .expect("连接已关闭")
            .unwrap();
        let tungstenite::Message::Text(t) = msg else { panic!("不是 text 帧") };
        let resp: Envelope = serde_json::from_str(&t).unwrap();
        assert!(!resp.is_ok(), "第一帧不是 hello 应被拒");
    }

    /// §4.1 的驱逐语义走完整路径:第二条连接接上后,第一条应被踢掉,
    /// 且 agent 最终仍是 online(不能因为旧连接的清理而变成 offline)。
    #[tokio::test]
    async fn reconnect_evicts_old_connection_and_stays_online() {
        let (url, pool) = spawn_server().await;
        let (id, token) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        let mut ws1 = connect(&url).await;
        assert!(do_hello(&mut ws1, &hello(&token)).await.is_ok());

        // 第二条连接(模拟 kill -9 后重连:旧 TCP 还没超时)
        let mut ws2 = connect(&url).await;
        assert!(do_hello(&mut ws2, &hello(&token)).await.is_ok());

        // 旧连接被驱逐后主动断开,它的清理**不能**把新连接标成 offline。
        drop(ws1);
        // 这一条等的是「某件事**不要**发生」,所以不能像别处那样轮询到条件成立就返回
        // —— `is_online()` 一开始就是 true,那样会立刻通过,而根本没给那条
        // 有 bug 的清理路径留出跑的时间。这里改成**在一个窗口内反复断言它一直成立**:
        // 清理若真的把新连接标成 offline,总会在某一次检查里被抓到。
        stays_true("新连接在旧连接清理期间一直在线", || async {
            crate::db::agent_repo::get(&pool, id).await.unwrap().unwrap().is_online()
        })
        .await;

        let a = crate::db::agent_repo::get(&pool, id).await.unwrap().unwrap();
        assert!(
            a.is_online(),
            "旧连接的清理不该把 agent 标成 offline —— 这正是 stale-unregister 要防的"
        );
    }

    /// §4.1 的补齐路径:agent 带着落后的 revision 连上来,
    /// 应当收到 `config.apply` 和 `user.state` 两条 req。
    ///
    /// 这是「加了节点但 agent 上没有」这类 bug 的回归锚点——
    /// 补齐链路断在任何一环(revision 没推进、catch_up 没 spawn、rpc 没发出)
    /// 都会让这个测试失败。
    #[tokio::test]
    async fn stale_agent_receives_config_apply_and_user_state() {
        let (url, pool) = spawn_server().await;
        let (agent_id, token) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        // 建一个节点 + 一个分配给它的、被禁用的用户。
        // 这会把 config_revision 推到 2、user_state_revision 推到 1。
        //
        // params 要经 `secrets::fill` —— reality 缺私钥时 build_agent_config 会失败,
        // 于是 catch_up 根本发不出 config.apply,这个测试会退化成「等超时」。
        let mut params = crate::model::node::NodeParams {
            server_name: Some("www.example.com".into()),
            ..Default::default()
        };
        crate::secrets::fill(crate::model::node::Protocol::VlessReality, &mut params).unwrap();
        let (node_id, _) = crate::db::node_repo::add_node(
            &pool,
            agent_id,
            "vless-in",
            crate::model::node::Protocol::VlessReality,
            8443,
            &params,
        )
        .await
        .unwrap();
        let uid = crate::db::node_repo::add_user(&pool, "banned", 0, 0).await.unwrap();
        crate::db::node_repo::assign_node(&pool, uid, node_id).await.unwrap();
        crate::db::node_repo::set_user_enabled(&pool, "banned", false).await.unwrap();

        // agent 带 revision 0 连上来 —— 两个都落后
        let mut ws = connect(&url).await;
        let ack = do_hello(&mut ws, &hello(&token)).await;
        assert!(ack.is_ok());
        let ack: AgentHelloAck = serde_json::from_value(ack.payload).unwrap();
        assert_eq!(ack.config_revision, 2, "hello_ack 应带主控侧的 revision");
        assert_eq!(ack.user_state_revision, 1);

        // 收接下来的两条 req(顺序:先 config 后 user_state)。
        //
        // **每条都必须回 resp。** `catch_up` 是顺序的:它等 `config.apply` 的回应
        // 才发 `user.state`(§4.2 要求 config 先落地)。不回的话第二条要等到
        // rpc 的 30s 超时才会发,测试会在这里卡住——这正是本测试第一版失败的原因。
        let mut got: Vec<(String, serde_json::Value)> = Vec::new();
        for i in 0..2 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
                .await
                .unwrap_or_else(|_| panic!("等第 {} 条补齐消息超时", i + 1))
                .expect("连接已关闭")
                .unwrap();
            let tungstenite::Message::Text(t) = msg else { panic!("不是 text 帧") };
            let env: Envelope = serde_json::from_str(&t).unwrap();
            assert_eq!(env.kind, Kind::Req, "补齐应当是 req(要等 resp)");
            let id = env.id.clone().expect("req 必须带 id(§4)");

            // 扮演 agent 回 ok
            let ok = Envelope::resp_ok(id, env.method.as_str(), serde_json::json!({}));
            ws.send(tungstenite::Message::Text(serde_json::to_string(&ok).unwrap())).await.unwrap();

            got.push((env.method, env.payload));
        }

        assert_eq!(got[0].0, method::CONFIG_APPLY, "应先下发 config.apply");
        assert_eq!(got[0].1["revision"], 2);
        let inbounds = got[0].1["options"]["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 1);
        assert_eq!(inbounds[0]["tag"], "vless-in");
        // 被禁用的用户仍然在 inbound 里(§7.5)
        assert_eq!(inbounds[0]["users"][0]["name"], "banned");

        assert_eq!(got[1].0, method::USER_STATE, "再下发 user.state");
        assert_eq!(got[1].1["user_state_revision"], 1);
        assert_eq!(got[1].1["disabled"][0], "banned", "禁用名单应含该用户");
    }

    /// revision 一致时**不该**下发任何东西 —— 否则每次重连都白重建一次 box。
    #[tokio::test]
    async fn up_to_date_agent_receives_nothing() {
        let (url, pool) = spawn_server().await;
        let (_, token) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        // 主控与 agent 都是 revision 0
        let mut ws = connect(&url).await;
        assert!(do_hello(&mut ws, &hello(&token)).await.is_ok());

        // 等一小会儿,确认没有后续消息
        let next = tokio::time::timeout(std::time::Duration::from_millis(400), ws.next()).await;
        assert!(next.is_err(), "revision 一致时不该收到任何补齐消息");
    }

    /// 组装配置失败时应记审计,且**不断开连接**。
    ///
    /// 触发方式是把一个已建好的节点的 protocol 改成本版本不认识的字符串。
    /// 这不是杜撰的场景:节点行由更新版本的主控写进去、或者有人手改过库,
    /// 都会长成这样。`build_inbound` 对 `Protocol::Unknown` 是**硬拒绝**
    /// (不给未知协议兜底 inbound —— 否则一个打错的协议名会静默生成一个
    /// 开放的直连入站),所以这个触发条件是设计上永久成立的。
    ///
    /// 早先这里用的是「trojan + 空参数」,靠的其实是缺 cert_pem/key_pem 而不是
    /// 注释里写的「trojan 尚未实现」—— 八个协议早就都实现了。那种写法的问题是:
    /// 哪天给 trojan 加了自签证书兜底,这条测试会在无人察觉的情况下
    /// 不再测它声称要测的东西。
    #[tokio::test]
    async fn config_build_failure_is_audited_and_does_not_kill_the_connection() {
        let (url, pool) = spawn_server().await;
        let (agent_id, token) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();
        crate::db::node_repo::add_node(
            &pool,
            agent_id,
            "trojan-in",
            crate::model::node::Protocol::Trojan,
            8443,
            &crate::model::node::NodeParams::default(),
        )
        .await
        .unwrap();
        // add_node 会挡 Protocol::Unknown(建节点时本就该挡),所以绕开它直接改库,
        // 模拟「库里已经躺着一个本版本读不懂的节点」。
        sqlx::query("UPDATE nodes SET protocol = 'wireguard-plus' WHERE agent_id = ?")
            .bind(agent_id)
            .execute(&pool)
            .await
            .unwrap();

        let mut ws = connect(&url).await;
        assert!(do_hello(&mut ws, &hello(&token)).await.is_ok());
        // 等审计行落库。catch_up 是 spawn 出去的,握手返回时它还没跑完。
        wait_for("config_build_failed 审计落库", || async {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM agent_events WHERE kind = 'config_build_failed'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            n > 0
        })
        .await;

        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE kind = 'config_build_failed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "组装失败应写审计记录");

        // 钉死**为什么**失败。少了这一条,将来任何一个别的组装错误
        // (缺参数、端口冲突……)都能让这条测试继续绿,而「未知协议不给兜底
        // inbound」这条保证就没人盯着了。
        let detail: String = sqlx::query_scalar(
            "SELECT message FROM agent_events WHERE kind = 'config_build_failed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(detail.contains("无法识别"), "审计详情该说清是协议读不懂,实际:{detail}");

        // 连接仍然可用:agent 还是 online
        let a = crate::db::agent_repo::get(&pool, agent_id).await.unwrap().unwrap();
        assert!(a.is_online(), "组装失败不该断开连接");
    }

    /// 正常断开后应标 offline。
    #[tokio::test]
    async fn clean_disconnect_marks_offline() {
        let (url, pool) = spawn_server().await;
        let (id, token) = crate::db::agent_repo::create(&pool, "tokyo", 0).await.unwrap();

        let mut ws = connect(&url).await;
        assert!(do_hello(&mut ws, &hello(&token)).await.is_ok());
        drop(ws);
        // 断开后的 offline 标记也是异步的:drop 只是关掉了 socket,
        // 服务端那边还要跑完清理才写库。
        wait_for("断开后被标成 offline", || async {
            !crate::db::agent_repo::get(&pool, id).await.unwrap().unwrap().is_online()
        })
        .await;

        let a = crate::db::agent_repo::get(&pool, id).await.unwrap().unwrap();
        assert!(!a.is_online(), "断开后应标 offline");
        assert!(a.last_seen.is_some());
    }
}
