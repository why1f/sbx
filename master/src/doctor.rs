//! `sbx doctor` —— 一条命令看清主控跑不起来是缺了什么(DEPLOY.md 的验证清单)。
//!
//! **这个命令全程只读,不改变被诊断的系统。** 两个具体后果:
//!
//!   * 开库用 `mode=ro` 而不是 `init_pool` 的 `mode=rwc` —— 后者会**把库建出来**
//!     并跑迁移,那样「数据库缺失」这一条永远报不出来(查之前就已经被创建了);
//!   * 证书用 `tls::fingerprint` 而不是 `tls::ensure_cert` —— 后者在证书缺失时
//!     会**生成**一张。诊断命令生成了证书,下一次再跑就看不见问题了。
//!
//! 采集与呈现分开:`collect` 依赖真实环境(文件系统、systemctl、端口),
//! 没法单测;`render` / `exit_code` 是纯函数,行为锚点打在那两个上。

use crate::config::Config;
use std::path::Path;

/// 一项检查的结论。
///
/// `Skip` 与 `Ok` **必须分开**:「这台机器上没装 systemd,所以没查」和
/// 「查了,是好的」是两回事。混成一个的话,汇总行里「跳过了 3 项」会伪装成
/// 「3 项正常」—— 那正是自检工具最不该犯的错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Err,
    Skip,
}

impl Level {
    /// 方括号里那四个字符。**等宽**(都是 4 列)——
    /// 不等宽的话后面的标签列会跟着错开,而这一列每行都在。
    fn tag(self) -> &'static str {
        match self {
            Level::Ok => " OK ",
            Level::Warn => "WARN",
            Level::Err => "ERR ",
            Level::Skip => " -- ",
        }
    }

    /// ANSI 前景色。绿 / 黄 / 红 / 灰,与 TUI 那套的语义对齐
    /// (`theme::ONLINE` / `ACCENT` / `OFFLINE` / `INACTIVE`)。
    fn color(self) -> &'static str {
        match self {
            Level::Ok => "\x1b[32m",
            Level::Warn => "\x1b[33m",
            Level::Err => "\x1b[31m",
            Level::Skip => "\x1b[90m",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    /// 左边那个标签,如「数据库」。
    pub name: String,
    pub level: Level,
    /// 右边那句话。**位置要写进去** —— 「配置文件缺失」帮不上忙,
    /// 「/etc/sbx/config.toml 不存在」才能直接去建。
    pub detail: String,
}

impl Check {
    fn new(name: &str, level: Level, detail: impl Into<String>) -> Self {
        Self { name: name.into(), level, detail: detail.into() }
    }
}

/// 标签列宽度。按**显示列宽**算(中文两列),取最长的那个标签
/// (「systemd 单元」= 12 列)再留两格。
const NAME_COLS: usize = 14;

/// 渲染成最终要打印的那几行。`color` 为 false 时**一个转义序列都不出现** ——
/// `sbx doctor > report.txt` 存进一堆 `\x1b[32m` 是没法看的。
pub fn render(checks: &[Check], color: bool) -> String {
    let mut out = String::new();
    for c in checks {
        let (on, off) = if color { (c.level.color(), "\x1b[0m") } else { ("", "") };
        // 标签用 `theme::pad` 而不是 `{:<14}`:格式化宽度按**字符数**算,
        // 而「数据库」是 3 个字符 6 列。混排中英文时说明列会参差不齐。
        out.push_str(&format!(
            "[{on}{}{off}] {} {}\n",
            c.level.tag(),
            crate::tui::theme::pad(&c.name, NAME_COLS),
            c.detail
        ));
    }
    let (ok, warn, err, skip) = tally(checks);
    out.push_str(&format!("\n汇总: {ok} OK / {warn} WARN / {err} ERR"));
    // 跳过的项**单独报**,不并进 OK。见 `Level` 的说明。
    if skip > 0 {
        out.push_str(&format!(" / {skip} 跳过"));
    }
    out.push('\n');
    out
}

fn tally(checks: &[Check]) -> (usize, usize, usize, usize) {
    let count = |l: Level| checks.iter().filter(|c| c.level == l).count();
    (count(Level::Ok), count(Level::Warn), count(Level::Err), count(Level::Skip))
}

/// 退出码。**只有 ERR 才非 0。**
///
/// WARN 不算失败:「订阅没配 public_base」「daemon 还没起来」都是真实存在、
/// 但不该让部署脚本整个红掉的状态。把 WARN 也算进去的话,人会很快学会
/// 忽略这个命令的退出码,那它就白做了。
pub fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| c.level == Level::Err))
}

/// 跑全部检查。顺序是「从最底层往上」:二进制 → 配置 → 目录 → 库 → 服务 → 网络,
/// 这样第一个红的那一项通常就是根因,底下的红多半是它的连带后果。
pub async fn collect(cfg_path: &str) -> Vec<Check> {
    let mut v = Vec::new();

    v.push(check_binary());

    // 配置要先读出来,后面好几项(库路径、监听地址、证书路径)都从它来。
    // 读不出来时用默认值继续 —— 那正是 daemon 的行为,doctor 要照着它演。
    let (cfg_check, cfg) = check_config(cfg_path);
    v.push(cfg_check);

    v.push(check_data_dir(&cfg));
    let (db_check, db_ok) = check_database(&cfg).await;
    v.push(db_check);
    v.push(check_db_contents(&cfg, db_ok).await);
    v.push(check_systemd_unit());
    v.push(check_cluster_listen(&cfg).await);
    v.push(check_tls(&cfg));
    v.push(check_subscription(&cfg));
    v.push(check_telegram(&cfg));
    v.push(check_colocated_agent());

    v
}

// ── 1. 二进制 ────────────────────────────────────────────────────────────

fn check_binary() -> Check {
    let ver = env!("CARGO_PKG_VERSION");
    match std::env::current_exe() {
        Ok(p) => {
            let path = p.display().to_string();
            // 装在别处不是错(开发时就是从 target/ 跑的),但要说出来 ——
            // 「我明明升级了怎么还是老版本」十有八九是 systemd 拉的是另一个文件。
            let note =
                if path.contains("target") { "(不是安装路径,开发构建?)" } else { "" };
            Check::new("sbx 二进制", Level::Ok, format!("{path} (v{ver}) {note}").trim_end())
        }
        // 真拿不到路径也不影响诊断别的项,降级成 WARN 继续往下走。
        Err(e) => {
            Check::new("sbx 二进制", Level::Warn, format!("取不到自身路径: {e}(版本 v{ver})"))
        }
    }
}

// ── 2. 配置文件 ──────────────────────────────────────────────────────────

fn check_config(path: &str) -> (Check, Config) {
    match std::fs::read_to_string(path) {
        Ok(text) => match Config::parse(&text) {
            Ok(c) => (Check::new("配置文件", Level::Ok, format!("已读取 {path}")), c),
            // 解析失败是**真的起不来**:daemon 在这一步会直接退出。
            Err(e) => (
                Check::new("配置文件", Level::Err, format!("{path} 解析失败: {e}")),
                Config::default(),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            Check::new(
                "配置文件",
                Level::Warn,
                format!("{path} 不存在,当前是全默认配置(cp config.example.toml 过去)"),
            ),
            Config::default(),
        ),
        // 读不了通常是权限:/etc/sbx 是 0750,非 root 跑就会撞上。
        Err(e) => {
            (Check::new("配置文件", Level::Err, format!("读 {path} 失败: {e}")), Config::default())
        }
    }
}

// ── 3. 数据目录 ──────────────────────────────────────────────────────────

fn check_data_dir(cfg: &Config) -> Check {
    let dir = Path::new(&cfg.db.path).parent().unwrap_or(Path::new("."));
    let shown = dir.display().to_string();
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => {
            Check::new("数据目录", Level::Ok, format!("{shown}{}", mode_note(&m)))
        }
        Ok(_) => Check::new("数据目录", Level::Err, format!("{shown} 存在但不是目录")),
        Err(e) => Check::new("数据目录", Level::Err, format!("{shown} 不可用: {e}")),
    }
}

/// 权限位那一小段说明。**只在 Unix 上有**,Windows 上返回空串。
///
/// 关心的是 other 位:证书私钥和整个库都在这个目录里,给同机其他用户开着
/// 等于把 token hash 和 reality 私钥摊开(§11.3)。
#[cfg(unix)]
fn mode_note(m: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = m.permissions().mode() & 0o777;
    if mode & 0o007 != 0 {
        format!(" (权限 {mode:04o} —— 同机其他用户可访问,建议 chmod 750)")
    } else {
        format!(" (权限 {mode:04o})")
    }
}

#[cfg(not(unix))]
fn mode_note(_m: &std::fs::Metadata) -> String {
    String::new()
}

// ── 4. 数据库 ────────────────────────────────────────────────────────────

/// 返回 (检查结论, 库是否可用)。后者给「库内容」那一项做前置判断 ——
/// 库都打不开时再去 `SELECT COUNT(*)` 只会多报一条同源的错。
async fn check_database(cfg: &Config) -> (Check, bool) {
    let path = &cfg.db.path;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (
                Check::new("数据库", Level::Err, format!("{path} 不存在(跑一次 sbx init-db)")),
                false,
            );
        }
        Err(e) => return (Check::new("数据库", Level::Err, format!("{path} 不可用: {e}")), false),
    };

    // 大小要把 -wal 算进去:WAL 模式下刚写入的数据还在那个文件里,
    // 只报主库大小会在「刚导入一批用户」之后显得完全没变。
    let mut size = meta.len();
    let mut wal = 0u64;
    for suffix in ["-wal", "-shm"] {
        if let Ok(m) = std::fs::metadata(format!("{path}{suffix}")) {
            size += m.len();
            wal += m.len();
        }
    }
    let size_txt = crate::model::user::User::format_bytes(size as i64);
    let wal_txt = if wal > 0 {
        format!(",含 WAL {}", crate::model::user::User::format_bytes(wal as i64))
    } else {
        String::new()
    };

    let writable = !meta.permissions().readonly();
    let rw = if writable { "可读写" } else { "只读" };

    // **`mode=ro`。** 用 rwc 的话,库不存在时这一步会把它建出来 ——
    // 上面那条「不存在」的判断就永远不会再触发第二次。
    let url = format!("sqlite://{path}?mode=ro");
    let ver: Option<i64> = match sqlx::SqlitePool::connect(&url).await {
        Ok(pool) => {
            let v = sqlx::query_scalar::<_, i64>("PRAGMA user_version").fetch_one(&pool).await.ok();
            pool.close().await;
            v
        }
        Err(e) => {
            return (
                Check::new("数据库", Level::Err, format!("{path}({size_txt})打不开: {e}")),
                false,
            );
        }
    };

    let want = crate::db::schema_version();
    let (level, schema) = match ver {
        Some(v) if v == want => (Level::Ok, format!("schema v{v}")),
        // 落后不是错:daemon / init-db 启动时会自动迁移上来。
        Some(v) if v < want => {
            (Level::Warn, format!("schema v{v},程序期望 v{want}(启动时会自动迁移)"))
        }
        // 比程序新 = 有人把二进制降级了。继续跑会撞上不认识的列。
        Some(v) => (Level::Err, format!("schema v{v} 比程序期望的 v{want} 还新(二进制被降级了?)")),
        None => (Level::Warn, "读不到 schema 版本".to_string()),
    };
    let level = if writable { level } else { Level::Err.min_of(level) };

    (Check::new("数据库", level, format!("{rw} {path} ({size_txt}{wal_txt}, {schema})")), true)
}

impl Level {
    /// 两个结论取更严重的那个。库只读时,即使 schema 对也得报 ERR ——
    /// daemon 起来第一件事就是写。
    fn min_of(self, other: Level) -> Level {
        let rank = |l: Level| match l {
            Level::Err => 3,
            Level::Warn => 2,
            Level::Ok => 1,
            Level::Skip => 0,
        };
        if rank(self) >= rank(other) {
            self
        } else {
            other
        }
    }
}

// ── 5. 库内容 ────────────────────────────────────────────────────────────

async fn check_db_contents(cfg: &Config, db_ok: bool) -> Check {
    if !db_ok {
        return Check::new("库内容", Level::Skip, "库打不开,跳过");
    }
    let url = format!("sqlite://{}?mode=ro", cfg.db.path);
    let Ok(pool) = sqlx::SqlitePool::connect(&url).await else {
        return Check::new("库内容", Level::Skip, "库打不开,跳过");
    };
    let count = |t: &'static str| {
        let p = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {t}")).fetch_one(&p).await
        }
    };
    let agents = count("agents").await;
    let nodes = count("nodes").await;
    let users = count("users").await;
    pool.close().await;

    match (agents, nodes, users) {
        (Ok(a), Ok(n), Ok(u)) => {
            let detail = format!("{a} 台被控 / {n} 个节点 / {u} 个用户");
            // 一台被控都没有 = 装完了但还没接机器,多半是没走完部署。
            let level = if a == 0 { Level::Warn } else { Level::Ok };
            let hint =
                if a == 0 { "(还没添加被控服务器:sbx agent-add <名称>)" } else { "" };
            Check::new("库内容", level, format!("{detail} {hint}").trim_end())
        }
        // 表都读不出来说明迁移没跑完,而不是「库是空的」。
        _ => Check::new("库内容", Level::Err, "读不到 agents/nodes/users 表(迁移没跑完?)"),
    }
}

// ── 6. systemd 单元 ──────────────────────────────────────────────────────

const UNIT_PATH: &str = "/etc/systemd/system/sbx.service";

fn check_systemd_unit() -> Check {
    if !has_systemctl() {
        return Check::new("systemd 单元", Level::Skip, "这台机器上没有 systemctl");
    }
    let unit_exists = Path::new(UNIT_PATH).exists();
    let enabled = systemctl_says(&["is-enabled", "sbx"]);
    let active = systemctl_says(&["is-active", "sbx"]);

    match (unit_exists, enabled.as_deref(), active.as_deref()) {
        (false, _, _) => Check::new(
            "systemd 单元",
            Level::Warn,
            format!("{UNIT_PATH} 不存在(手动跑 sbx daemon 也行,但开机不会自启)"),
        ),
        (true, Some("enabled"), Some("active")) => {
            Check::new("systemd 单元", Level::Ok, format!("已启用且运行中 {UNIT_PATH}"))
        }
        (true, Some("enabled"), Some(s)) => Check::new(
            "systemd 单元",
            Level::Warn,
            format!("已启用但当前 {s} —— systemctl status sbx 看原因"),
        ),
        (true, Some(e), Some(s)) => Check::new(
            "systemd 单元",
            Level::Warn,
            format!("{UNIT_PATH} 在,但 is-enabled={e} is-active={s}(systemctl enable --now sbx)"),
        ),
        (true, _, _) => {
            Check::new("systemd 单元", Level::Warn, format!("{UNIT_PATH} 在,但问不出状态"))
        }
    }
}

fn has_systemctl() -> bool {
    std::process::Command::new("systemctl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// 跑一条 `systemctl <args>` 取第一行输出。
///
/// **不看退出码。** `is-enabled` 对一个 disabled 的单元返回非 0,但它照样
/// 在 stdout 上打印 `disabled` —— 那正是我们要的答案。
fn systemctl_says(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("systemctl").args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s.lines().next().unwrap_or_default().to_string())
    }
}

// ── 7. 集群监听 ──────────────────────────────────────────────────────────

/// daemon 到底起来没有。**发一次真的 TCP connect**,而不是查配置里写了什么 ——
/// 「配置里写着 18443」和「18443 上真有人在听」是两回事,后者才是 agent 关心的。
async fn check_cluster_listen(cfg: &Config) -> Check {
    let listen = &cfg.cluster.listen;
    // 0.0.0.0 / :: 是**通配监听地址**,不能直接拿去 connect ——
    // 得换成回环地址才连得上自己。
    let target = match listen.rsplit_once(':') {
        Some((host, port)) => {
            let h = host.trim_matches(['[', ']']);
            let h = if h.is_empty() || h == "0.0.0.0" || h == "*" {
                "127.0.0.1"
            } else if h == "::" {
                "[::1]"
            } else {
                h
            };
            format!("{h}:{port}")
        }
        None => return Check::new("集群监听", Level::Warn, format!("listen 不像地址: {listen}")),
    };

    let probe = tokio::time::timeout(
        std::time::Duration::from_millis(1200),
        tokio::net::TcpStream::connect(&target),
    )
    .await;

    match probe {
        Ok(Ok(_)) => Check::new("集群监听", Level::Ok, format!("{listen} 已在监听")),
        // 连不上 = daemon 没跑。是 WARN 不是 ERR:装完还没启动是正常中间态,
        // 而 doctor 本身经常就是在「还没起来」的时候跑的。
        Ok(Err(e)) => {
            Check::new("集群监听", Level::Warn, format!("{listen} 连不上({e})—— daemon 没在跑?"))
        }
        // 超时和拒绝对使用者是同一件事(「没人在听」),所以给同一句提示。
        // 分开只在于:超时那条还可能是本机防火墙把 SYN 丢了。
        Err(_) => Check::new(
            "集群监听",
            Level::Warn,
            format!("{listen} 无响应 —— daemon 没在跑?(或本机防火墙丢了 SYN)"),
        ),
    }
}

// ── 8. TLS 证书 ──────────────────────────────────────────────────────────

fn check_tls(cfg: &Config) -> Check {
    if !cfg.cluster.tls {
        return Check::new(
            "TLS 证书",
            Level::Ok,
            "cluster.tls = false,明文 ws(agent 侧需要 insecure = true)",
        );
    }
    let (c, k) = (&cfg.cluster.cert_path, &cfg.cluster.key_path);
    let (ce, ke) = (Path::new(c).exists(), Path::new(k).exists());
    match (ce, ke) {
        (true, true) => match crate::tls::fingerprint(c) {
            // **只算指纹,不调 ensure_cert** —— 后者在缺文件时会生成一张,
            // 那样这一项下次就永远是绿的了。
            Ok(fp) => Check::new("TLS 证书", Level::Ok, format!("{c}  {fp}")),
            Err(e) => Check::new("TLS 证书", Level::Err, format!("{c} 解析失败: {e}")),
        },
        (false, false) => Check::new(
            "TLS 证书",
            Level::Warn,
            format!("{c} 与密钥都不存在(首次启动 daemon 时会自签生成)"),
        ),
        // 半套是**坏状态**:拿旧证书配新私钥,握手报的错和真正的原因毫无关系。
        _ => Check::new(
            "TLS 证书",
            Level::Warn,
            format!(
                "只有一半:cert {} / key {}(daemon 启动时会重新生成一整套,指纹会变)",
                if ce { "在" } else { "缺" },
                if ke { "在" } else { "缺" }
            ),
        ),
    }
}

// ── 9. 订阅服务 ──────────────────────────────────────────────────────────

fn check_subscription(cfg: &Config) -> Check {
    let s = &cfg.subscription;
    if !s.enabled {
        return Check::new("订阅服务", Level::Ok, "已关闭");
    }
    if s.public_base.trim().is_empty() {
        // 订阅链接是拿 public_base 拼的,空的话发给用户的地址是残的。
        return Check::new(
            "订阅服务",
            Level::Warn,
            format!("已开启({}),但 public_base 为空 —— 订阅链接不可用", s.listen),
        );
    }
    Check::new("订阅服务", Level::Ok, format!("{} → {}", s.listen, s.public_base))
}

// ── 10. Telegram ─────────────────────────────────────────────────────────

fn check_telegram(cfg: &Config) -> Check {
    let t = &cfg.telegram;
    if !t.enabled {
        return Check::new("Telegram", Level::Ok, "已关闭");
    }
    if t.bot_token.trim().is_empty() {
        // 开着却没 token:bot 起不来,而通知会静默地一条都不发。
        return Check::new("Telegram", Level::Err, "已开启但 bot_token 为空,通知发不出去");
    }
    let admins = t.admin_chat_ids.len();
    let note = if admins == 0 { ",没有管理员 chat_id(只有用户侧通知)" } else { "" };
    // token 是凭据,只回显长度(§11.3)。
    Check::new(
        "Telegram",
        Level::Ok,
        format!("已开启,token 已配置({} 位),{admins} 个管理员{note}", t.bot_token.len()),
    )
}

// ── 11. 同机 agent ───────────────────────────────────────────────────────

/// install.sh 支持在同一台机器上同时装主控和 agent,所以这台机器上**可能**有。
/// 没有就整条跳过 —— 绝大多数主控机上不该有 agent,报「缺失」是误导。
fn check_colocated_agent() -> Check {
    let unit = "/etc/systemd/system/sbx-agent.service";
    let bin = "/usr/local/bin/sbx-agent";
    let (ue, be) = (Path::new(unit).exists(), Path::new(bin).exists());
    if !ue && !be {
        return Check::new("同机 agent", Level::Skip, "未安装(主控机上通常也不需要)");
    }
    let active = if has_systemctl() {
        systemctl_says(&["is-active", "sbx-agent"]).unwrap_or_else(|| "?".into())
    } else {
        "?".into()
    };
    let level = if active == "active" { Level::Ok } else { Level::Warn };
    Check::new("同机 agent", level, format!("已安装 {bin},服务 {active}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, level: Level) -> Check {
        Check::new(name, level, "说明")
    }

    /// **只有 ERR 才让退出码非 0。**
    ///
    /// WARN 也算失败的话,「订阅没配 public_base」「daemon 还没起来」这些
    /// 真实但无害的状态会让部署脚本整个红掉 —— 人很快就学会忽略这个退出码,
    /// 那这个命令就白做了。
    #[test]
    fn only_errors_make_the_exit_code_nonzero() {
        assert_eq!(exit_code(&[]), 0, "什么都没查也不算失败");
        assert_eq!(exit_code(&[c("a", Level::Ok), c("b", Level::Ok)]), 0);
        assert_eq!(exit_code(&[c("a", Level::Ok), c("b", Level::Warn)]), 0, "WARN 不该让脚本失败");
        assert_eq!(exit_code(&[c("a", Level::Skip)]), 0, "跳过更不该");
        assert_eq!(exit_code(&[c("a", Level::Ok), c("b", Level::Err)]), 1);
    }

    /// **跳过的项不能并进 OK。**
    ///
    /// 「这台机器上没装 systemd,所以没查」和「查了,是好的」是两回事。
    /// 混在一起的话,一份「11 OK」的报告里可能有 3 项根本没查过 ——
    /// 那是自检工具最不该犯的错。
    #[test]
    fn skipped_checks_are_counted_separately() {
        let checks = [c("a", Level::Ok), c("b", Level::Skip), c("c", Level::Skip)];
        let out = render(&checks, false);
        assert!(out.contains("1 OK"), "只有一项是真的 OK:\n{out}");
        assert!(out.contains("2 跳过"), "跳过要单独报出来:\n{out}");
        assert_eq!(tally(&checks), (1, 0, 0, 2));
    }

    /// 一项都没跳过时不显示「0 跳过」—— 那一截是噪音。
    #[test]
    fn a_clean_run_does_not_mention_skips() {
        let out = render(&[c("a", Level::Ok)], false);
        assert!(out.contains("汇总: 1 OK / 0 WARN / 0 ERR"), "{out}");
        assert!(!out.contains("跳过"), "没有跳过项就别提它:\n{out}");
    }

    /// **中英文标签混排时,说明列要从同一列开始。**
    ///
    /// `{:<14}` 是按**字符数**补的,而「数据库」是 3 个字符 6 列 ——
    /// 用它的话中文标签那几行会往右串出好几格。必须走按显示列宽算的 `pad`。
    #[test]
    fn the_detail_column_lines_up_across_scripts() {
        let checks = [
            Check::new("数据库", Level::Ok, "详情"),
            Check::new("Telegram", Level::Ok, "详情"),
            Check::new("systemd 单元", Level::Ok, "详情"),
        ];
        let out = render(&checks, false);
        let starts: Vec<usize> = out
            .lines()
            .filter(|l| l.contains("详情"))
            .map(|l| crate::tui::theme::cols(&l[..l.find("详情").unwrap()]))
            .collect();
        assert_eq!(starts.len(), 3);
        assert!(
            starts.iter().all(|s| *s == starts[0]),
            "说明列没对齐,各行起始列 = {starts:?}\n{out}"
        );
    }

    /// 重定向到文件时**一个转义序列都不许有**。
    /// `sbx doctor > report.txt` 存进一堆 `\x1b[32m` 是没法看的。
    #[test]
    fn without_a_tty_there_are_no_escape_sequences() {
        let out = render(&[c("a", Level::Ok), c("b", Level::Err)], false);
        assert!(!out.contains('\x1b'), "无 TTY 时不该有颜色:\n{out:?}");
        // 反过来,开了颜色就该有 —— 否则上面那条断言可能只是因为压根没接线。
        let colored = render(&[c("a", Level::Ok)], true);
        assert!(colored.contains('\x1b'), "开了颜色却没上色:{colored:?}");
    }

    /// 四个等级的标签**必须等宽**,否则后面整列跟着错开。
    #[test]
    fn every_level_tag_is_the_same_width() {
        for l in [Level::Ok, Level::Warn, Level::Err, Level::Skip] {
            assert_eq!(l.tag().len(), 4, "{l:?} 的标签宽度不是 4");
        }
    }

    /// **doctor 不得改变被诊断的系统。**
    ///
    /// 这条钉的是 `mode=ro`:写成 `init_pool` 的 `mode=rwc` 的话,
    /// 库会在「检查它存不存在」的过程中被创建出来 ——
    /// 于是「数据库缺失」这一条永远只报得出一次,第二次跑就绿了。
    #[tokio::test]
    async fn collecting_never_creates_the_database() {
        let dir = std::env::temp_dir().join(format!("sbx-doctor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sbx.db");
        let cfg_path = dir.join("config.toml");
        std::fs::write(
            &cfg_path,
            format!("[db]\npath = \"{}\"\n", db.display().to_string().replace('\\', "/")),
        )
        .unwrap();

        let checks = collect(cfg_path.to_string_lossy().as_ref()).await;

        assert!(!db.exists(), "doctor 把库建出来了 —— 它必须只读");
        let dbc = checks.iter().find(|c| c.name == "数据库").expect("该有数据库这一项");
        assert_eq!(dbc.level, Level::Err, "库不存在该报 ERR:{}", dbc.detail);
        assert_eq!(exit_code(&checks), 1, "有 ERR 时退出码该是 1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真造一个库,断言它报得出**大小**和 schema 版本 —— 这两样是这条命令
    /// 被要求做的核心信息。
    #[tokio::test]
    async fn a_real_database_reports_its_size_and_schema() {
        let dir = std::env::temp_dir().join(format!("sbx-doctor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("sbx.db");
        let db_str = db.display().to_string().replace('\\', "/");
        crate::db::init_pool(&db_str).await.unwrap().close().await;

        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, format!("[db]\npath = \"{db_str}\"\n")).unwrap();

        let checks = collect(cfg_path.to_string_lossy().as_ref()).await;
        let dbc = checks.iter().find(|c| c.name == "数据库").unwrap();
        assert_eq!(dbc.level, Level::Ok, "刚建好的库该是 OK:{}", dbc.detail);
        assert!(dbc.detail.contains(&db_str), "要写出位置:{}", dbc.detail);
        assert!(
            dbc.detail.contains(&format!("schema v{}", crate::db::schema_version())),
            "要写出 schema 版本:{}",
            dbc.detail
        );
        assert!(
            dbc.detail.contains("KB") || dbc.detail.contains("MB") || dbc.detail.contains("B"),
            "要写出文件大小:{}",
            dbc.detail
        );

        // 空库:被控数为 0,该提示还没接机器。
        let contents = checks.iter().find(|c| c.name == "库内容").unwrap();
        assert_eq!(contents.level, Level::Warn, "空库该提示:{}", contents.detail);
        assert!(contents.detail.contains("0 台被控"), "{}", contents.detail);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配置文件不存在只是 WARN(daemon 会退默认值),但**解析失败是 ERR** ——
    /// 后者 daemon 起不来。两者不该同级。
    #[test]
    fn a_broken_config_is_worse_than_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("sbx-doctor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("nope.toml");
        let (chk, _) = check_config(missing.to_string_lossy().as_ref());
        assert_eq!(chk.level, Level::Warn, "缺文件退默认值,不是致命错");
        assert!(chk.detail.contains("不存在"), "{}", chk.detail);

        let broken = dir.join("broken.toml");
        std::fs::write(&broken, "[db\npath = ").unwrap();
        let (chk, _) = check_config(broken.to_string_lossy().as_ref());
        assert_eq!(chk.level, Level::Err, "解析失败 daemon 就起不来");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 通配监听地址要换成回环地址才连得上自己。
    /// 直接拿 `0.0.0.0:18443` 去 connect 在多数平台上是连不通的 ——
    /// 那会让一个**正在正常监听**的 daemon 被报成「没在跑」。
    #[tokio::test]
    async fn a_wildcard_listen_address_is_probed_on_loopback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let mut cfg = Config::default();
        cfg.cluster.listen = format!("0.0.0.0:{port}");
        let chk = check_cluster_listen(&cfg).await;
        assert_eq!(chk.level, Level::Ok, "通配地址该换成回环再探:{}", chk.detail);

        // 没人听的端口报 WARN 而不是 ERR —— 「还没启动」是正常中间态。
        drop(listener);
        let mut cfg2 = Config::default();
        cfg2.cluster.listen = format!("127.0.0.1:{port}");
        let chk2 = check_cluster_listen(&cfg2).await;
        assert_eq!(chk2.level, Level::Warn, "连不上是 WARN 不是 ERR");
    }

    /// Telegram 开着却没填 token 是 ERR:bot 起不来,而通知会**静默**地一条都不发。
    /// 关着则是正常状态。
    #[test]
    fn telegram_enabled_without_a_token_is_an_error() {
        let mut cfg = Config::default();
        cfg.telegram.enabled = false;
        assert_eq!(check_telegram(&cfg).level, Level::Ok);

        cfg.telegram.enabled = true;
        cfg.telegram.bot_token = String::new();
        assert_eq!(check_telegram(&cfg).level, Level::Err);

        cfg.telegram.bot_token = "123456:abcdef".into();
        let chk = check_telegram(&cfg);
        assert_eq!(chk.level, Level::Ok);
        assert!(!chk.detail.contains("abcdef"), "token 是凭据,不该回显原文:{}", chk.detail);
    }
}
