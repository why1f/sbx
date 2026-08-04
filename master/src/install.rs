//! 被控机上的一键接入命令(DESIGN.md §8.1)。
//!
//! 放在 `tui` 外面是因为**两条路都要用它**:TUI 的「新增被控服务器」弹窗,
//! 和 CLI 的 `sbx agent-add`。后者尤其要紧 —— 终端不支持 OSC 52 复制时,
//! CLI 打在普通回滚缓冲里的那一份是唯一能用鼠标选中复制的。
//!
//! 命令走**环境变量**而不是 `--server` 这类参数:管道形式下传参要写
//! `bash -s -- …`,而这条命令已经够长了;`SBX_TOKEN` 非空本身就足以让
//! `packaging/install.sh` 判定「这是在装被控」,于是连 `SBX_TARGET=agent` 都不用带。

use crate::config::Config;

/// 一键安装脚本的地址。与 README / CHANGELOG 里那条是同一个 URL,改了要一起改。
pub const INSTALL_URL: &str = "https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh";

/// 被控机回连主控用的默认地址。
///
/// 主控只知道自己 `listen` 在 `0.0.0.0:18443`,不知道对外该用哪个地址,
/// 所以拿订阅的 `public_base` 的主机名当**猜测** —— 多数部署里订阅和 WS
/// 就在同一台机器上。猜错了人可以直接改,总比让人从零开始打强。
pub fn default_host(cfg: &Config) -> String {
    let base = cfg.subscription.public_base.trim();
    if base.is_empty() {
        return String::new();
    }
    let rest = base.split_once("://").map(|(_, r)| r).unwrap_or(base);
    let hostport = rest.split('/').next().unwrap_or(rest);
    // `[::1]:8080` → `[::1]`;`example.com:8080` → `example.com`。
    match hostport.strip_prefix('[') {
        Some(r) => r.split_once(']').map(|(h, _)| format!("[{h}]")).unwrap_or_else(|| hostport.into()),
        None => hostport.split(':').next().unwrap_or(hostport).to_string(),
    }
}

/// 从 `0.0.0.0:18443` 里取出端口。取不到就用默认值 —— 这个字符串只是拼给人看的
/// 提示,解析失败不该让「新增 agent」整个失败。
pub fn port_of(listen: &str) -> String {
    listen.rsplit_once(':').map(|(_, p)| p.to_string()).unwrap_or_else(|| "18443".into())
}

/// IPv6 字面量在 URL 里必须带方括号,否则那些冒号会被当成端口分隔符。
fn url_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// 拼出被控机上的一条命令。`token` 为 `None` 时给占位符 ——
/// 明文早就没了(§8.1),这种情况只能提示去轮换一个新的。
pub fn command(cfg: &Config, host: &str, token: Option<&str>) -> String {
    let scheme = if cfg.cluster.tls { "wss" } else { "ws" };
    let host = if host.trim().is_empty() { "<主控地址>" } else { host.trim() };
    let server = format!("{scheme}://{}:{}/ws", url_host(host), port_of(&cfg.cluster.listen));
    let token = token.unwrap_or("<token 已经看不到了,按 [r] 轮换一个新的>");

    let auth = if cfg.cluster.tls {
        // 指纹取不到(还没生成证书)时也要把命令给全,只是那一格是占位符 ——
        // 少给一段的话人会以为命令就该长这样,连上之后才发现 TOFU 没生效。
        let fp = crate::tls::fingerprint(&cfg.cluster.cert_path)
            .unwrap_or_else(|_| "<先跑一次 sbx daemon 生成证书,再取这条命令>".into());
        format!("SBX_FINGERPRINT='{fp}' ")
    } else {
        // cluster.tls = false:没有证书可钉,agent 侧必须显式 insecure。
        "SBX_INSECURE=1 ".into()
    };

    format!("curl -fsSL {INSTALL_URL} | SBX_SERVER='{server}' SBX_TOKEN='{token}' {auth}bash")
}

/// 弹窗/终端里跟着命令一起给的几句话。
pub fn notes(host: &str, has_token: bool) -> Vec<String> {
    let mut out = vec![
        "整条复制到被控机上跑(root)。脚本会装好 sbx-agent、写 /etc/sbx/agent.toml(0600)、".into(),
        "并 enable --now sbx-agent。以后重跑同一条命令就是升级。".into(),
    ];
    if host.trim().is_empty() {
        out.push(String::new());
        out.push("⚠ 主控地址是空的,命令里留了 <主控地址> 占位符 —— 换成被控机能连到的 IP 或域名。".into());
    }
    if has_token {
        out.push(String::new());
        out.push("⚠ token 明文只显示这一次,关掉就再也拿不回来了(库里只有 hash)。".into());
        out.push("  这条命令带着 token,别贴进聊天记录或工单里。丢了就轮换一个新的。".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_of_handles_ipv4_ipv6_and_garbage() {
        assert_eq!(port_of("0.0.0.0:18443"), "18443");
        assert_eq!(port_of("[::]:9443"), "9443");
        assert_eq!(port_of("nonsense"), "18443");
    }

    /// 从订阅地址里猜主控主机名。猜错了人可以改,但不能猜出一个带端口或带路径的东西 ——
    /// 那会拼出 `wss://example.com:8080:18443/ws` 这种一眼看不出错在哪的地址。
    #[test]
    fn default_host_extracts_just_the_hostname() {
        let mut cfg = Config::default();
        for (base, want) in [
            ("https://sub.example.com", "sub.example.com"),
            ("https://sub.example.com/", "sub.example.com"),
            ("https://sub.example.com:8443/x", "sub.example.com"),
            ("http://203.0.113.8:8080", "203.0.113.8"),
            ("https://[2001:db8::1]:8443", "[2001:db8::1]"),
            ("", ""),
        ] {
            cfg.subscription.public_base = base.into();
            assert_eq!(default_host(&cfg), want, "public_base = {base}");
        }
    }

    /// IPv6 主控地址必须带方括号,否则 `wss://2001:db8::1:18443/ws` 里
    /// 哪个冒号是端口分隔符谁也说不清 —— agent 侧会解析失败。
    #[test]
    fn ipv6_master_address_is_bracketed() {
        let mut cfg = Config::default();
        cfg.cluster.tls = false;
        cfg.cluster.listen = "[::]:18443".into();
        let cmd = command(&cfg, "2001:db8::1", Some("tok"));
        assert!(cmd.contains("ws://[2001:db8::1]:18443/ws"), "{cmd}");
        // 已经带方括号的不要再套一层。
        let cmd = command(&cfg, "[2001:db8::1]", Some("tok"));
        assert!(cmd.contains("ws://[2001:db8::1]:18443/ws"), "{cmd}");
    }

    /// 明文模式下没有证书可钉,命令里必须显式给 `SBX_INSECURE=1` ——
    /// 少了它 agent 会因为「配了 ws:// 却没说明为什么不校验」而拒绝启动。
    #[test]
    fn plaintext_mode_tells_the_agent_to_skip_verification() {
        let mut cfg = Config::default();
        cfg.cluster.tls = false;
        let cmd = command(&cfg, "203.0.113.8", Some("tok"));
        assert!(cmd.contains("SBX_INSECURE=1"), "{cmd}");
        assert!(cmd.contains("ws://203.0.113.8"), "{cmd}");
        assert!(!cmd.contains("wss://"), "明文模式不该给 wss: {cmd}");
        assert!(!cmd.contains("SBX_FINGERPRINT"), "没有证书就不该有指纹: {cmd}");
    }

    /// 主控地址没填时留占位符,并且**明确说出来** ——
    /// 直接给一条拼错的命令,人会照抄然后对着连不上发懵。
    #[test]
    fn missing_host_is_called_out_not_silently_wrong() {
        let mut cfg = Config::default();
        cfg.cluster.tls = false;
        let cmd = command(&cfg, "", Some("tok"));
        assert!(cmd.contains("<主控地址>"), "{cmd}");
        assert!(notes("", true).join("\n").contains("主控地址是空的"));
    }

    /// 重新查看接入命令时 token 是取不到的(库里只有 hash),
    /// 必须给一句「怎么才能拿到」而不是一个看起来能用的空值。
    #[test]
    fn reshown_command_admits_the_token_is_gone() {
        let mut cfg = Config::default();
        cfg.cluster.tls = false;
        assert!(command(&cfg, "203.0.113.8", None).contains("轮换"));
        assert!(!notes("203.0.113.8", false).join("\n").contains("只显示这一次"));
    }

    /// 接入命令必须是**一整行**。分行的命令从终端里复制会带上换行,
    /// 粘到另一个 shell 里就变成好几条互相看不见对方的命令 ——
    /// 而这条命令的全部价值就是「整条复制过去跑」。
    #[test]
    fn command_is_a_single_copyable_line() {
        let mut cfg = Config::default();
        cfg.cluster.tls = false;
        let cmd = command(&cfg, "203.0.113.8", Some("tok"));
        assert!(!cmd.contains('\n'), "{cmd}");
        assert!(cmd.starts_with("curl -fsSL "), "{cmd}");
        // 环境变量赋值必须落在 `| ` 之后、`bash` 之前 —— `curl … | VAR=x bash`
        // 是一条合法的 POSIX 简单命令,而 `VAR=x curl … | bash` 会把变量给了 curl。
        let (_, after_pipe) = cmd.split_once("| ").expect("要有管道");
        assert!(after_pipe.starts_with("SBX_SERVER="), "{cmd}");
        assert!(after_pipe.ends_with(" bash"), "{cmd}");
        // 值都用单引号包起来:token 是随机串,理论上可以出现 shell 元字符。
        assert!(cmd.contains("SBX_TOKEN='tok'"), "{cmd}");
    }
}
