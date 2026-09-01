//! Telegram Bot(DESIGN.md §9.1,移植自旧项目 `service/tg_service.rs`)。
//!
//! 三个后台循环:
//!   * `poll_loop`     —— 长轮询取 update,分发命令与按钮回调;
//!   * `schedule_loop` —— 每 30 秒检查一次定时播报是否到点;
//!   * `lease_loop`    —— 续租,续不上就让所有循环停下来。
//!
//! 外加一条 mpsc:`supervisor` 每轮巡检把用量百分比丢进来,由这里做**档位去重**
//! 后推送(§6.3 的配额判定与通知是两件事,判定在那边,通知在这边)。
//!
//! ## 为什么要租约
//!
//! 同一个 `bot_token` 只允许一个 `getUpdates` 长轮询。两个进程一起轮询时
//! Telegram 会对其中一个返回 409,而且**两边都会随机丢 update** ——
//! 表现是「命令时灵时不灵」,极难归因。
//!
//! 目前只有 `daemon` 会起 bot。租约挡的是**两个 daemon 共用一个库**的情况
//! (滚动重启时新旧进程重叠、或者有人手工又开了一个),也给将来让 TUI 起 bot
//! 留好了位置 —— `start()` 的 `mode` 参数就是为此存在的。
//!
//! ## 相对旧项目省掉的部分
//!
//! 旧版有一整套管理员改用户配额/到期的 inline 表单。sbx 里那些操作在 CLI 和
//! TUI 上都有,而且改配额要推进 revision、下发配置 —— 放在一个可能因为
//! 网络抖动重放的聊天界面上并不合适。这里只保留**只读查询 + 通知开关**。

mod api;
// `pub(crate)`:TUI 的「网卡重置时区」表单要用 `fmt::parse_timezone` / `format_offset`
// 解析同一种 `±HH:MM` 写法。宁可把这个模块开放给 crate 内部,也不要为了「模块边界好看」
// 复制一份解析器 —— 两份解析器迟早会在某个边角上分叉,而那种分叉查起来很痛。
pub(crate) mod fmt;
mod repo;
mod ui;

use anyhow::{anyhow, Result};
use chrono::{DateTime, FixedOffset, Utc};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Semaphore};

use crate::config::TelegramConfig;

/// 外部丢进来的事件。目前只有配额告警。
#[derive(Debug, Clone)]
pub enum Event {
    QuotaAlert { username: String, percent: f64 },
}

/// 租约多久没心跳算失效。明显大于心跳周期,免得 GC 卡顿之类的抖动导致误抢。
const LEASE_STALE_SECS: i64 = 90;
const LEASE_HEARTBEAT: Duration = Duration::from_secs(25);
/// 一览里最多列多少个用户。Telegram 单条消息上限 4096 字符。
const USAGE_LIST_LIMIT: usize = 25;

#[derive(Clone)]
struct Ctx {
    pool: SqlitePool,
    cfg: Arc<TelegramConfig>,
    /// 订阅地址的前缀,来自 `[subscription] public_base`。空则只给路径。
    public_base: String,
    api: api::Api,
    offset: FixedOffset,
    /// 等待用户输入的下一条消息(改播报时间)。key 是 chat_id。
    pending: Arc<Mutex<HashMap<i64, Pending>>>,
    lease_owner: String,
    /// 租约还在自己手上。失去后各循环据此自行退出,避免两个实例同时推消息。
    active: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum Pending {
    UserSchedule,
    AdminSchedule,
}

impl Ctx {
    fn alive(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
    fn now(&self) -> DateTime<FixedOffset> {
        Utc::now().with_timezone(&self.offset)
    }
    fn is_admin(&self, chat_id: i64) -> bool {
        self.cfg.admin_chat_ids.contains(&chat_id)
    }
    fn default_user_times(&self) -> Vec<String> {
        fmt::normalize_schedule(&self.cfg.default_schedule_times)
    }
    fn default_admin_times(&self) -> Vec<String> {
        fmt::normalize_schedule(&self.cfg.admin_schedule_times)
    }
}

/// 启动 bot。
///
/// * `Ok(None)` —— 另一个进程已经在跑 bot,本次主动跳过。**这不是错误。**
/// * `Ok(Some(tx))` —— 拿到租约,把 tx 交给 supervisor 用于推送配额告警。
pub async fn start(
    pool: SqlitePool,
    cfg: TelegramConfig,
    public_base: String,
    mode: &str,
) -> Result<Option<mpsc::Sender<Event>>> {
    if !cfg.enabled {
        return Err(anyhow!("telegram.enabled = false"));
    }
    if cfg.bot_token.trim().is_empty() {
        return Err(anyhow!("telegram.bot_token 为空"));
    }

    let lease_owner = format!("{}@{mode}", std::process::id());
    let now = Utc::now().timestamp();
    if !repo::try_acquire_lease(&pool, &lease_owner, LEASE_STALE_SECS, now).await? {
        let holder =
            repo::lease_holder(&pool).await.ok().flatten().unwrap_or_else(|| "未知进程".into());
        tracing::info!(
            %holder,
            "另一个进程已在运行 Telegram Bot,本进程跳过(同一 bot_token 只能有一个长轮询)"
        );
        return Ok(None);
    }

    let api = api::Api::new(&cfg.bot_token, cfg.request_timeout_secs)?;
    let offset = fmt::parse_timezone(&cfg.timezone).unwrap_or_else(|| {
        tracing::warn!(
            timezone = %cfg.timezone,
            "无法解析时区,回落到 +08:00。支持 ±HH:MM 与**不走夏令时**的 IANA 别名;\
             会 DST 的时区(Europe/London、America/* 等)请填显式偏移"
        );
        FixedOffset::east_opt(8 * 3600).expect("8 小时偏移恒定有效")
    });

    let ctx = Ctx {
        pool,
        cfg: Arc::new(cfg),
        public_base,
        api,
        offset,
        pending: Arc::new(Mutex::new(HashMap::new())),
        lease_owner,
        active: Arc::new(AtomicBool::new(true)),
    };

    // 管理员偏好行按需建。INSERT OR IGNORE,不会覆盖管理员自己改过的设置。
    let admin_times = serde_json::to_string(&ctx.default_admin_times())?;
    for id in &ctx.cfg.admin_chat_ids {
        repo::ensure_admin_prefs(
            &ctx.pool,
            *id,
            ctx.cfg.admin_notify_quota,
            ctx.cfg.admin_schedule_enabled,
            &admin_times,
        )
        .await?;
    }
    // 命令菜单注册失败不致命:没有 / 下拉提示,命令本身照样能用。
    if let Err(e) = ctx.api.register_commands().await {
        tracing::warn!(error = %e, "注册 Telegram 命令菜单失败(不致命)");
    }

    let (tx, rx) = mpsc::channel::<Event>(64);
    tokio::spawn(event_loop(ctx.clone(), rx));
    tokio::spawn(poll_loop(ctx.clone()));
    tokio::spawn(schedule_loop(ctx.clone()));
    tokio::spawn(lease_loop(ctx));

    tracing::info!("Telegram Bot 已启动");
    Ok(Some(tx))
}

async fn lease_loop(ctx: Ctx) {
    let mut iv = tokio::time::interval(LEASE_HEARTBEAT);
    iv.tick().await;
    loop {
        iv.tick().await;
        if !ctx.alive() {
            return;
        }
        match repo::renew_lease(&ctx.pool, &ctx.lease_owner, Utc::now().timestamp()).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(owner = %ctx.lease_owner, "Telegram Bot 租约被接管,本实例停止");
                ctx.active.store(false, Ordering::Relaxed);
                return;
            }
            // 续租失败多半是 DB 临时忙。保持活跃等下一轮 —— 租约过期前还有余量。
            Err(e) => tracing::warn!(error = %e, "续租 Telegram Bot 失败,稍后重试"),
        }
    }
}

async fn event_loop(ctx: Ctx, mut rx: mpsc::Receiver<Event>) {
    while let Some(ev) = rx.recv().await {
        if !ctx.alive() {
            continue; // 失去租约后不再推送,但要把 channel 排空,免得发送方阻塞
        }
        match ev {
            Event::QuotaAlert { username, percent } => {
                if let Err(e) = handle_quota_alert(&ctx, &username, percent).await {
                    tracing::warn!(user = %username, error = %e, "配额告警推送失败");
                }
            }
        }
    }
}

async fn poll_loop(ctx: Ctx) {
    let mut offset = 0i64;
    let mut backoff = 1u64;
    let idle = ctx.cfg.poll_interval_secs.max(1);
    // 并发处理 update,但设上限:一次 getUpdates 可能返回上百条,
    // 无限 spawn 会把数据库连接池打满。
    let slots = Arc::new(Semaphore::new(8));

    loop {
        if !ctx.alive() {
            tracing::info!("Telegram Bot 已失去租约,停止轮询");
            return;
        }
        match ctx.api.get_updates(offset).await {
            Ok(updates) => {
                backoff = 1;
                let empty = updates.is_empty();
                for update in updates {
                    offset = update.update_id + 1;
                    let ctx = ctx.clone();
                    let slots = slots.clone();
                    tokio::spawn(async move {
                        let Ok(_permit) = slots.acquire_owned().await else { return };
                        let id = update.update_id;
                        // 单条 update 卡住不能拖垮整个 bot。
                        match tokio::time::timeout(
                            Duration::from_secs(60),
                            handle_update(&ctx, update),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                tracing::warn!(update_id = id, error = %e, "处理 update 失败")
                            }
                            Err(_) => tracing::warn!(update_id = id, "处理 update 超时"),
                        }
                    });
                }
                if empty {
                    tokio::time::sleep(Duration::from_secs(idle)).await;
                }
            }
            Err(e) if api::is_conflict(&e) => {
                // 租约挡得住同机多开,挡不住「另一台机器用了同一个 token」。
                // 继续轮询只会两边互抢 update,不如停下并给出明确指引。
                tracing::error!(
                    "Telegram 返回 409:同一个 bot_token 在别处也在轮询。\
                     请确认没有第二处部署(或本地调试进程)用了同一个 token。本实例已停止轮询"
                );
                ctx.active.store(false, Ordering::Relaxed);
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, backoff, "getUpdates 失败,退避后重试");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

// ─────────────────────────── 分发 ───────────────────────────

async fn handle_update(ctx: &Ctx, update: api::Update) -> Result<()> {
    if let Some(msg) = update.message {
        if let Some(text) = msg.text {
            handle_message(ctx, msg.chat.id, text.trim()).await?;
        }
    }
    if let Some(cb) = update.callback_query {
        // 先应答,让客户端上的转圈停下来。失败无所谓,它纯粹是 UI 反馈。
        let _ = ctx.api.answer_callback(&cb.id).await;
        if let (Some(data), Some(msg)) = (cb.data, cb.message) {
            handle_callback(ctx, msg.chat.id, msg.message_id, &data).await?;
        }
    }
    Ok(())
}

async fn handle_message(ctx: &Ctx, chat_id: i64, text: &str) -> Result<()> {
    // 命令永远打断「等输入」状态 —— 否则用户在填时间的中途想 /start 就卡住了。
    if text.starts_with('/') {
        ctx.pending.lock().await.remove(&chat_id);
    } else if let Some(p) = ctx.pending.lock().await.remove(&chat_id) {
        return handle_pending(ctx, chat_id, text, p).await;
    }

    if let Some(code) = text.strip_prefix("/bind") {
        return bind(ctx, chat_id, code.trim()).await;
    }
    match text {
        "/start" | "/menu" => send_home(ctx, chat_id, None).await,
        "/usage" => send_usage(ctx, chat_id, None).await,
        "/sub" => send_sub(ctx, chat_id, None).await,
        "/usages" => send_all_usages(ctx, chat_id, None).await,
        _ => {
            ctx.api
                .send_html(chat_id, "可用命令:/start  /usage  /sub  /bind &lt;绑定码&gt;", None)
                .await
        }
    }
}

async fn handle_pending(ctx: &Ctx, chat_id: i64, text: &str, p: Pending) -> Result<()> {
    let times = match fmt::parse_schedule_input(text) {
        Ok(t) => t,
        Err(e) => {
            // 解析失败时**把 pending 放回去**,让用户直接重填,
            // 而不是要他重新点一遍菜单。
            ctx.pending.lock().await.insert(chat_id, p);
            return ctx
                .api
                .send_html(
                    chat_id,
                    &format!(
                        "时间格式无效:{}\n请输入 <code>HH:MM,HH:MM</code>,例如 <code>09:00,21:30</code>",
                        fmt::h(&e.to_string())
                    ),
                    None,
                )
                .await;
        }
    };
    let json = serde_json::to_string(&times)?;
    match p {
        Pending::UserSchedule => {
            let Some(u) = repo::user_by_chat(&ctx.pool, chat_id).await? else {
                return ctx.api.send_html(chat_id, &ui::unbound_text(), None).await;
            };
            repo::set_notify_settings(
                &ctx.pool,
                &u.name,
                u.notify_80,
                u.notify_90,
                u.notify_100,
                u.schedule_enabled,
                &json,
            )
            .await?;
            send_user_settings(ctx, chat_id, None).await
        }
        Pending::AdminSchedule => {
            if !ctx.is_admin(chat_id) {
                return Ok(());
            }
            let prefs = admin_prefs(ctx, chat_id).await?;
            repo::set_admin_prefs(
                &ctx.pool,
                chat_id,
                prefs.notify_quota,
                prefs.schedule_enabled,
                &json,
            )
            .await?;
            send_admin_settings(ctx, chat_id, None).await
        }
    }
}

async fn handle_callback(ctx: &Ctx, chat_id: i64, msg_id: i64, data: &str) -> Result<()> {
    let edit = Some(msg_id);
    match data {
        "u:home" => send_home(ctx, chat_id, edit).await,
        "u:usage" => send_usage(ctx, chat_id, edit).await,
        "u:sub" => send_sub(ctx, chat_id, edit).await,
        "u:sub_links" => send_sub_links(ctx, chat_id).await,
        "u:sub_b64" => send_sub_b64(ctx, chat_id).await,
        "u:settings" => send_user_settings(ctx, chat_id, edit).await,
        "u:t80" | "u:t90" | "u:t100" | "u:sched" => {
            toggle_user_setting(ctx, chat_id, data, edit).await
        }
        "u:sched_time" => {
            ctx.pending.lock().await.insert(chat_id, Pending::UserSchedule);
            ctx.api
                .send_html(chat_id, "请发送新的播报时间,逗号分隔:\n<code>09:00,21:30</code>", None)
                .await
        }

        // 管理员侧。每一条都先查权限 —— callback_data 是客户端发来的,
        // 谁都能构造一条 `a:usages` 发过来。
        _ if data.starts_with("a:") => {
            if !ctx.is_admin(chat_id) {
                return ctx.api.send_html(chat_id, "⛔ 需要管理员权限。", None).await;
            }
            match data {
                "a:home" => send_admin_home(ctx, chat_id, edit).await,
                "a:usages" => send_all_usages(ctx, chat_id, edit).await,
                "a:settings" => send_admin_settings(ctx, chat_id, edit).await,
                "a:quota" | "a:sched" => toggle_admin_setting(ctx, chat_id, data, edit).await,
                "a:sched_time" => {
                    ctx.pending.lock().await.insert(chat_id, Pending::AdminSchedule);
                    ctx.api
                        .send_html(chat_id, "请发送新的汇总时间:\n<code>09:00,21:30</code>", None)
                        .await
                }
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

// ─────────────────────────── 各个视图 ───────────────────────────

/// 取当前 chat 绑定的用户。没绑就发引导并返回 `None`。
async fn bound_user(ctx: &Ctx, chat_id: i64, edit: Option<i64>) -> Result<Option<repo::TgUser>> {
    match repo::user_by_chat(&ctx.pool, chat_id).await? {
        Some(u) => Ok(Some(u)),
        None => {
            ctx.api.send_or_edit(chat_id, edit, &ui::unbound_text(), None).await?;
            Ok(None)
        }
    }
}

async fn send_home(ctx: &Ctx, chat_id: i64, edit: Option<i64>) -> Result<()> {
    // 管理员没绑用户时给管理员主页,而不是「请先绑定」——
    // 管理员多半不是这套系统的用户。
    match repo::user_by_chat(&ctx.pool, chat_id).await? {
        Some(u) => {
            ctx.api
                .send_or_edit(
                    chat_id,
                    edit,
                    &ui::user_home_text(&u),
                    Some(ui::user_home_keyboard()),
                )
                .await
        }
        None if ctx.is_admin(chat_id) => send_admin_home(ctx, chat_id, edit).await,
        None => ctx.api.send_or_edit(chat_id, edit, &ui::unbound_text(), None).await,
    }
}

async fn send_usage(ctx: &Ctx, chat_id: i64, edit: Option<i64>) -> Result<()> {
    let Some(u) = bound_user(ctx, chat_id, edit).await? else { return Ok(()) };
    ctx.api
        .send_or_edit(chat_id, edit, &ui::user_home_text(&u), Some(ui::user_back_keyboard()))
        .await
}

fn sub_url(ctx: &Ctx, token: &str, clash: bool) -> String {
    let base = ctx.public_base.trim().trim_end_matches('/');
    let suffix = if clash { "?type=clash" } else { "" };
    if base.is_empty() {
        format!("/sub/{token}{suffix}")
    } else {
        format!("{base}/sub/{token}{suffix}")
    }
}

async fn send_sub(ctx: &Ctx, chat_id: i64, edit: Option<i64>) -> Result<()> {
    let Some(u) = bound_user(ctx, chat_id, edit).await? else { return Ok(()) };
    let mut body = format!(
        "🔗 <b>订阅地址</b>\n\n\
         sing-box / v2rayN:\n<code>{}</code>\n\n\
         mihomo / Clash Meta:\n<code>{}</code>",
        fmt::h(&sub_url(ctx, &u.sub_token, false)),
        fmt::h(&sub_url(ctx, &u.sub_token, true)),
    );
    if ctx.public_base.trim().is_empty() {
        // 只有路径的话用户没法直接用,得说清为什么。
        body.push_str(
            "\n\n⚠️ 主控未配置 <code>subscription.public_base</code>,上面只是路径,请自行补全域名。",
        );
    }
    ctx.api.send_or_edit(chat_id, edit, &body, Some(ui::user_sub_keyboard())).await
}

/// 查出该用户可导出的链接。与订阅 HTTP 服务走**同一套**生成逻辑(§10),
/// 不另写一份 —— 两份实现迟早会给出不同的链接。
async fn user_links(ctx: &Ctx, u: &repo::TgUser) -> Result<Vec<crate::sub::ShareLink>> {
    let nodes = crate::sub::export_nodes(&ctx.pool, u.id).await?;
    let opts = crate::sub::ExportOptions {
        use_public_base_as_server: false,
        public_base: &ctx.public_base,
        request_host: None,
    };
    let su = crate::sub::SubUser {
        name: u.name.clone(),
        uuid: user_uuid(&ctx.pool, u.id).await?,
        password: user_password(&ctx.pool, u.id).await?,
    };
    Ok(crate::sub::generate_links(&su, &nodes, &opts))
}

async fn user_uuid(pool: &SqlitePool, id: i64) -> Result<String> {
    Ok(sqlx::query_scalar("SELECT uuid FROM users WHERE id = ?").bind(id).fetch_one(pool).await?)
}
async fn user_password(pool: &SqlitePool, id: i64) -> Result<String> {
    Ok(sqlx::query_scalar("SELECT password FROM users WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await?)
}

async fn send_sub_links(ctx: &Ctx, chat_id: i64) -> Result<()> {
    let Some(u) = bound_user(ctx, chat_id, None).await? else { return Ok(()) };
    let links = user_links(ctx, &u).await?;
    if links.is_empty() {
        return ctx
            .api
            .send_html(chat_id, "暂无可用节点,请联系管理员分配。", Some(ui::user_back_keyboard()))
            .await;
    }
    let body = links.iter().map(|l| l.link.as_str()).collect::<Vec<_>>().join("\n\n");
    ctx.api
        .send_code_block(chat_id, "📋 <b>全部节点链接</b>", &body, Some(ui::user_back_keyboard()))
        .await
}

async fn send_sub_b64(ctx: &Ctx, chat_id: i64) -> Result<()> {
    let Some(u) = bound_user(ctx, chat_id, None).await? else { return Ok(()) };
    let links = user_links(ctx, &u).await?;
    let b64 = crate::sub::subscription_b64(&links);
    if b64.is_empty() {
        return ctx
            .api
            .send_html(chat_id, "暂无可用节点,请联系管理员分配。", Some(ui::user_back_keyboard()))
            .await;
    }
    ctx.api
        .send_code_block(chat_id, "🧩 <b>base64 订阅内容</b>", &b64, Some(ui::user_back_keyboard()))
        .await
}

async fn send_user_settings(ctx: &Ctx, chat_id: i64, edit: Option<i64>) -> Result<()> {
    let Some(u) = bound_user(ctx, chat_id, edit).await? else { return Ok(()) };
    let body = ui::user_settings_text(&u, &ctx.default_user_times());
    ctx.api.send_or_edit(chat_id, edit, &body, Some(ui::user_settings_keyboard(&u))).await
}

async fn toggle_user_setting(ctx: &Ctx, chat_id: i64, data: &str, edit: Option<i64>) -> Result<()> {
    let Some(u) = bound_user(ctx, chat_id, edit).await? else { return Ok(()) };
    let (mut n80, mut n90, mut n100, mut sched) =
        (u.notify_80, u.notify_90, u.notify_100, u.schedule_enabled);
    match data {
        "u:t80" => n80 = !n80,
        "u:t90" => n90 = !n90,
        "u:t100" => n100 = !n100,
        "u:sched" => sched = !sched,
        _ => return Ok(()),
    }
    repo::set_notify_settings(&ctx.pool, &u.name, n80, n90, n100, sched, &u.schedule_times_json)
        .await?;
    send_user_settings(ctx, chat_id, edit).await
}

async fn send_admin_home(ctx: &Ctx, chat_id: i64, edit: Option<i64>) -> Result<()> {
    ctx.api.send_or_edit(chat_id, edit, "🛠 <b>管理员</b>", Some(ui::admin_home_keyboard())).await
}

async fn send_all_usages(ctx: &Ctx, chat_id: i64, edit: Option<i64>) -> Result<()> {
    if !ctx.is_admin(chat_id) {
        return ctx.api.send_html(chat_id, "⛔ 需要管理员权限。", None).await;
    }
    let users = repo::list_users(&ctx.pool).await?;
    let body = ui::all_usages_text(&users, USAGE_LIST_LIMIT);
    ctx.api.send_or_edit(chat_id, edit, &body, Some(ui::admin_back_keyboard())).await
}

async fn admin_prefs(ctx: &Ctx, chat_id: i64) -> Result<repo::AdminPrefs> {
    repo::list_admin_prefs(&ctx.pool, &[chat_id])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("管理员偏好未初始化"))
}

async fn send_admin_settings(ctx: &Ctx, chat_id: i64, edit: Option<i64>) -> Result<()> {
    let p = admin_prefs(ctx, chat_id).await?;
    let body = ui::admin_settings_text(&p, &ctx.default_admin_times());
    ctx.api.send_or_edit(chat_id, edit, &body, Some(ui::admin_settings_keyboard(&p))).await
}

async fn toggle_admin_setting(
    ctx: &Ctx,
    chat_id: i64,
    data: &str,
    edit: Option<i64>,
) -> Result<()> {
    let p = admin_prefs(ctx, chat_id).await?;
    let (mut q, mut s) = (p.notify_quota, p.schedule_enabled);
    match data {
        "a:quota" => q = !q,
        "a:sched" => s = !s,
        _ => return Ok(()),
    }
    repo::set_admin_prefs(&ctx.pool, chat_id, q, s, &p.schedule_times_json).await?;
    send_admin_settings(ctx, chat_id, edit).await
}

async fn bind(ctx: &Ctx, chat_id: i64, code: &str) -> Result<()> {
    if code.is_empty() {
        return ctx.api.send_html(chat_id, "用法:<code>/bind 你的绑定码</code>", None).await;
    }
    match repo::bind(&ctx.pool, code, chat_id).await? {
        Some(name) => {
            tracing::info!(user = %name, chat_id, "Telegram 账号已绑定");
            ctx.api
                .send_html(chat_id, &format!("✅ 已绑定到 <b>{}</b>", fmt::h(&name)), None)
                .await?;
            send_home(ctx, chat_id, None).await
        }
        // 不区分「码不存在」和「码已用过」—— 两者可区分的话,
        // 这个接口就成了一个绑定码存在性预言机。
        None => ctx.api.send_html(chat_id, "❌ 绑定码无效或已被使用。", None).await,
    }
}

// ─────────────────────────── 告警与定时 ───────────────────────────

/// 处理一条配额告警。**去重在这里**:只在档位上升时推送。
async fn handle_quota_alert(ctx: &Ctx, username: &str, percent: f64) -> Result<()> {
    let level = fmt::quota_level(percent);
    if level == 0 {
        return Ok(());
    }
    let Some(u) = repo::get_user(&ctx.pool, username).await? else {
        return Ok(()); // 用户刚被删掉,告警作废
    };
    if i64::from(level) <= u.last_quota_level {
        return Ok(()); // 这一档已经通知过了
    }

    if u.is_bound() && u.threshold_enabled(level) {
        // 用户侧发送失败(被拉黑、注销)不能吞掉管理员通知,也不能阻止档位落库 ——
        // 否则每 30 秒重试一次,永远推不成也永远不停。
        if let Err(e) = ctx
            .api
            .send_html(u.chat_id, &ui::quota_alert_text(&u, level), Some(ui::user_back_keyboard()))
            .await
        {
            tracing::warn!(user = %u.name, error = %e, "配额告警推给用户失败");
        }
    }

    for admin in repo::list_admin_prefs(&ctx.pool, &ctx.cfg.admin_chat_ids)
        .await?
        .into_iter()
        .filter(|a| a.notify_quota)
    {
        if let Err(e) =
            ctx.api.send_html(admin.chat_id, &ui::admin_quota_alert_text(&u, level), None).await
        {
            tracing::warn!(chat_id = admin.chat_id, error = %e, "配额告警推给管理员失败");
        }
    }

    repo::set_last_quota_level(&ctx.pool, &u.name, i64::from(level)).await?;
    Ok(())
}

async fn schedule_loop(ctx: Ctx) {
    let mut iv = tokio::time::interval(Duration::from_secs(30));
    loop {
        iv.tick().await;
        if !ctx.alive() {
            return;
        }
        if let Err(e) = reset_fallen_levels(&ctx).await {
            tracing::warn!(error = %e, "刷新配额告警档位失败");
        }
        if let Err(e) = run_due_schedules(&ctx).await {
            tracing::warn!(error = %e, "执行定时播报失败");
        }
    }
}

/// 百分比掉回低档时把去重游标降下来(月重置、管理员调高配额)。
///
/// 没有这一步,一个到过 100% 的用户在重置之后**永远不会再收到告警** ——
/// 因为 `last_quota_level` 一直卡在 100。
async fn reset_fallen_levels(ctx: &Ctx) -> Result<()> {
    for u in repo::list_users(&ctx.pool).await? {
        let current = i64::from(fmt::quota_level(u.percent()));
        if current < u.last_quota_level {
            repo::set_last_quota_level(&ctx.pool, &u.name, current).await?;
        }
    }
    Ok(())
}

/// 逐用户 / 逐管理员发送定时播报。
///
/// 单个 chat 发送失败(被拉黑、账号注销)只记 warn 并跳过,**不能用 `?` 中断整轮** ——
/// 否则排在后面的人当分钟收不到,而且失败者的日期没落库,每 30 秒重试一次,
/// 持续阻塞所有人。
async fn run_due_schedules(ctx: &Ctx) -> Result<()> {
    let now = ctx.now();
    let today = now.format("%Y-%m-%d").to_string();
    let stamp = now.format("%Y-%m-%d %H:%M").to_string();
    let users = repo::list_users(&ctx.pool).await?;

    for u in users.iter().filter(|u| u.is_bound() && u.schedule_enabled) {
        let times = {
            let own = fmt::normalize_schedule(&u.schedule_times());
            if own.is_empty() {
                ctx.default_user_times()
            } else {
                own
            }
        };
        let mut dates = u.last_schedule_dates();
        let due = fmt::due_times(&now, &times, &dates);
        if due.is_empty() {
            continue;
        }
        if let Err(e) = ctx
            .api
            .send_html(
                u.chat_id,
                &ui::scheduled_user_text(&stamp, u),
                Some(ui::user_home_keyboard()),
            )
            .await
        {
            tracing::warn!(user = %u.name, error = %e, "定时播报失败,跳过该用户");
            continue;
        }
        for item in due {
            dates.insert(item, today.clone());
        }
        if let Err(e) =
            repo::set_last_schedule_dates(&ctx.pool, &u.name, &serde_json::to_string(&dates)?).await
        {
            tracing::warn!(user = %u.name, error = %e, "记录播报日期失败");
        }
    }

    for p in repo::list_admin_prefs(&ctx.pool, &ctx.cfg.admin_chat_ids)
        .await?
        .into_iter()
        .filter(|p| p.schedule_enabled)
    {
        let times = {
            let own = fmt::normalize_schedule(&p.schedule_times());
            if own.is_empty() {
                ctx.default_admin_times()
            } else {
                own
            }
        };
        let mut dates = p.last_schedule_dates();
        let due = fmt::due_times(&now, &times, &dates);
        if due.is_empty() {
            continue;
        }
        let body = ui::scheduled_admin_text(&stamp, &users, USAGE_LIST_LIMIT);
        if let Err(e) = ctx.api.send_html(p.chat_id, &body, Some(ui::admin_home_keyboard())).await {
            tracing::warn!(chat_id = p.chat_id, error = %e, "管理员定时汇总失败,跳过");
            continue;
        }
        for item in due {
            dates.insert(item, today.clone());
        }
        if let Err(e) = repo::set_admin_last_schedule_dates(
            &ctx.pool,
            p.chat_id,
            &serde_json::to_string(&dates)?,
        )
        .await
        {
            tracing::warn!(chat_id = p.chat_id, error = %e, "记录汇总日期失败");
        }
    }
    Ok(())
}

/// 给用户生成一个绑定码(CLI / TUI 用)。
pub async fn issue_bind_code(pool: &SqlitePool, name: &str) -> Result<String> {
    repo::regenerate_bind_token(pool, name).await
}

/// 解除绑定(CLI / TUI 用)。
pub async fn unbind_user(pool: &SqlitePool, name: &str) -> Result<()> {
    repo::unbind(pool, name).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let path = std::env::temp_dir().join(format!("sbx-tgmod-{}.db", uuid::Uuid::new_v4()));
        crate::db::init_pool(path.to_string_lossy().as_ref()).await.unwrap()
    }

    fn ctx(pool: SqlitePool) -> Ctx {
        Ctx {
            pool,
            cfg: Arc::new(TelegramConfig { admin_chat_ids: vec![999], ..Default::default() }),
            public_base: "https://sub.example.com".into(),
            api: api::Api::new("test:token", 5).unwrap(),
            offset: FixedOffset::east_opt(8 * 3600).unwrap(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            lease_owner: "test".into(),
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    #[tokio::test]
    async fn sub_url_handles_missing_public_base() {
        let c = ctx(pool().await);
        assert_eq!(sub_url(&c, "tok", false), "https://sub.example.com/sub/tok");
        assert_eq!(sub_url(&c, "tok", true), "https://sub.example.com/sub/tok?type=clash");

        let mut c2 = ctx(pool().await);
        c2.public_base = String::new();
        assert_eq!(sub_url(&c2, "tok", false), "/sub/tok");

        // 尾斜杠不该拼出 //sub/
        let mut c3 = ctx(pool().await);
        c3.public_base = "https://s/".into();
        assert_eq!(sub_url(&c3, "tok", false), "https://s/sub/tok");
    }

    #[tokio::test]
    async fn admin_check_reads_the_configured_list() {
        let c = ctx(pool().await);
        assert!(c.is_admin(999));
        assert!(!c.is_admin(1));
    }

    /// 档位只在**上升**时推送。这条不成立的话,80% 的告警会每 30 秒重发一次。
    #[tokio::test]
    async fn quota_alert_fires_once_per_level() {
        let p = pool().await;
        let c = ctx(p.clone());
        crate::db::node_repo::add_user(&p, "alice", 100 * 1_073_741_824, 0).await.unwrap();

        // 没绑定、也没有管理员偏好行时不会真的发消息(API 是假 token),
        // 但去重游标该推进的还是要推进。
        handle_quota_alert(&c, "alice", 85.0).await.unwrap();
        let u = repo::get_user(&p, "alice").await.unwrap().unwrap();
        assert_eq!(u.last_quota_level, 80);

        // 同一档再来一次:不动。
        handle_quota_alert(&c, "alice", 88.0).await.unwrap();
        assert_eq!(repo::get_user(&p, "alice").await.unwrap().unwrap().last_quota_level, 80);

        // 升档:推进。
        handle_quota_alert(&c, "alice", 95.0).await.unwrap();
        assert_eq!(repo::get_user(&p, "alice").await.unwrap().unwrap().last_quota_level, 90);
    }

    /// 低于 80% 不告警 —— 那是正常状态。
    #[tokio::test]
    async fn no_alert_below_the_first_threshold() {
        let p = pool().await;
        let c = ctx(p.clone());
        crate::db::node_repo::add_user(&p, "alice", 100 * 1_073_741_824, 0).await.unwrap();
        handle_quota_alert(&c, "alice", 50.0).await.unwrap();
        assert_eq!(repo::get_user(&p, "alice").await.unwrap().unwrap().last_quota_level, 0);
    }

    /// 月重置之后档位要降回去,否则那个用户**再也收不到告警**。
    #[tokio::test]
    async fn levels_fall_back_after_a_reset() {
        let p = pool().await;
        let c = ctx(p.clone());
        let uid =
            crate::db::node_repo::add_user(&p, "alice", 100 * 1_073_741_824, 0).await.unwrap();
        repo::set_last_quota_level(&p, "alice", 100).await.unwrap();

        // 用量是 0(刚重置),巡检应当把游标降到 0。
        reset_fallen_levels(&c).await.unwrap();
        assert_eq!(repo::get_user(&p, "alice").await.unwrap().unwrap().last_quota_level, 0);

        // 降下来之后重新涨上去会再次告警。
        let _ = uid;
        handle_quota_alert(&c, "alice", 82.0).await.unwrap();
        assert_eq!(repo::get_user(&p, "alice").await.unwrap().unwrap().last_quota_level, 80);
    }

    /// 用户被删掉之后迟到的告警不该报错 —— supervisor 与 bot 之间是异步的。
    #[tokio::test]
    async fn alert_for_a_deleted_user_is_a_noop() {
        let p = pool().await;
        let c = ctx(p.clone());
        assert!(handle_quota_alert(&c, "ghost", 99.0).await.is_ok());
    }

    /// 已用满但**关掉了 100% 提醒**的用户:不推给他,但档位仍要落库,
    /// 否则每轮巡检都会重来一次。
    #[tokio::test]
    async fn disabled_threshold_still_advances_the_cursor() {
        let p = pool().await;
        let c = ctx(p.clone());
        crate::db::node_repo::add_user(&p, "alice", 100 * 1_073_741_824, 0).await.unwrap();
        repo::set_notify_settings(&p, "alice", false, false, false, true, "[]").await.unwrap();

        handle_quota_alert(&c, "alice", 100.0).await.unwrap();
        assert_eq!(repo::get_user(&p, "alice").await.unwrap().unwrap().last_quota_level, 100);
    }
}
