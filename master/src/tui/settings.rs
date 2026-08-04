//! 设置页(DESIGN.md §8.4)。
//!
//! 目的很直接:常改的那几项不该逼人去 ssh 里编辑 `/etc/sbx/config.toml`。
//!
//! **写回是行级替换**(`config::set_value`),不是「反序列化 → 改 → 写回」——
//! 后者一次保存就把配置文件里那几十行解释性注释全抹掉,而那些注释是唯一
//! 说明「这个端口为什么只听本地」的地方。
//!
//! **这里改的东西 daemon 不会热加载。** 它在启动时读一次配置,之后不再看。
//! 所以每一项都必须说清「什么时候生效」,页面顶上也常驻一句提醒 ——
//! 「改了但没变」是这个界面最容易造出来的困惑。
//!
//! 有意**没有**放进来的:`db.path`(改了等于换一个库,不是一次设置),
//! 以及证书路径(改完还要重新分发指纹,属于运维流程不是开关)。

use crate::config::{self, Config};

/// 一项设置。
pub struct Setting {
    pub section: &'static str,
    pub key: &'static str,
    pub label: String,
    /// 当前值的显示形式。凭据类的在这里就已经被打码。
    pub shown: String,
    pub kind: Kind,
    /// 灰字说明,写清「什么时候生效」。
    pub note: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// 布尔项按一下就切,不开表单 —— 为一个 true/false 弹个框太重了。
    Bool(bool),
    Int,
    /// 逗号分隔的整数列表(Telegram 管理员 chat id)。
    IntList,
    /// 凭据:显示时打码,编辑时从空白开始输(不预填旧值)。
    Secret,
}

impl Setting {
    /// 编辑表单里预填什么。
    ///
    /// 凭据**不预填** —— 把 bot_token 明文铺在一个输入框里,等于让它出现在
    /// 任何一次截图和终端回滚缓冲里,而这个框的用途本来只是「换一个新的」(§11.3)。
    pub fn edit_value(&self) -> String {
        match self.kind {
            Kind::Secret => String::new(),
            _ => self.shown.clone(),
        }
    }

    /// 把人填的东西转成 TOML 字面量。返回 `Err` 表示没通过校验。
    pub fn to_toml(&self, input: &str) -> Result<String, String> {
        let s = input.trim();
        match self.kind {
            Kind::Bool(b) => Ok((!b).to_string()),
            Kind::Int => {
                let v: u64 = s.parse().map_err(|_| format!("要填一个非负整数,收到:{s}"))?;
                Ok(v.to_string())
            }
            Kind::IntList => {
                if s.is_empty() {
                    return Ok("[]".into());
                }
                let mut ids = Vec::new();
                for part in s.split(&[',', ',', ' '][..]).filter(|p| !p.trim().is_empty()) {
                    let v: i64 = part
                        .trim()
                        .parse()
                        .map_err(|_| format!("chat id 要是数字,收到:{}", part.trim()))?;
                    ids.push(v.to_string());
                }
                Ok(format!("[{}]", ids.join(", ")))
            }
            Kind::Text | Kind::Secret => Ok(config::toml_string(s)),
        }
    }
}

/// 凭据的显示形式:只给前 6 位。**绝不回显全文**(§11.3)。
fn mask(s: &str) -> String {
    if s.trim().is_empty() {
        return "(未配置)".into();
    }
    let head: String = s.chars().take(6).collect();
    format!("{head}…({} 字符)", s.chars().count())
}

/// 当前配置对应的设置列表。每次渲染重新生成 —— 改完就能看到新值。
pub fn all(cfg: &Config) -> Vec<Setting> {
    let s = |section, key, label: &str, shown: String, kind, note: &str| Setting {
        section,
        key,
        label: label.into(),
        shown,
        kind,
        note: note.into(),
    };
    vec![
        s(
            "subscription",
            "public_base",
            "订阅对外地址",
            cfg.subscription.public_base.clone(),
            Kind::Text,
            "形如 https://sub.example.com。它同时决定新增 agent 时的主控地址,重启 daemon 生效",
        ),
        s(
            "subscription",
            "listen",
            "订阅监听",
            cfg.subscription.listen.clone(),
            Kind::Text,
            "默认只听 127.0.0.1,前面挂 nginx 终结 TLS。重启 daemon 生效",
        ),
        s(
            "subscription",
            "enabled",
            "订阅服务",
            on_off(cfg.subscription.enabled),
            Kind::Bool(cfg.subscription.enabled),
            "关掉之后订阅地址全部 404。重启 daemon 生效",
        ),
        s(
            "subscription",
            "use_public_base_as_server",
            "订阅按对外地址导出节点",
            on_off(cfg.subscription.use_public_base_as_server),
            Kind::Bool(cfg.subscription.use_public_base_as_server),
            "开启后链接里的服务器地址一律用对外地址,而不是各 agent 自己的 IP",
        ),
        s(
            "cluster",
            "listen",
            "集群监听",
            cfg.cluster.listen.clone(),
            Kind::Text,
            "agent 要能连到,必须对外。改完记得同步防火墙,并重发接入命令。重启 daemon 生效",
        ),
        s(
            "cluster",
            "tls",
            "集群 TLS",
            on_off(cfg.cluster.tls),
            Kind::Bool(cfg.cluster.tls),
            "关掉就是明文 ws://,agent 侧要配 insecure。已接入的 agent 需要重发接入命令",
        ),
        s(
            "cluster",
            "heartbeat_secs",
            "心跳间隔(秒)",
            cfg.cluster.heartbeat_secs.to_string(),
            Kind::Int,
            "主控多久 ping 一次 agent。重启 daemon 生效",
        ),
        s(
            "cluster",
            "report_interval_secs",
            "上报间隔(秒)",
            cfg.cluster.report_interval_secs.to_string(),
            Kind::Int,
            "agent 多久上报一次流量与主机指标。调小会放大 DB 写入,握手时下发给 agent",
        ),
        s(
            "telegram",
            "enabled",
            "Telegram 机器人",
            on_off(cfg.telegram.enabled),
            Kind::Bool(cfg.telegram.enabled),
            "开启后 daemon 会长轮询 bot。重启 daemon 生效",
        ),
        s(
            "telegram",
            "bot_token",
            "Telegram bot_token",
            mask(&cfg.telegram.bot_token),
            Kind::Secret,
            "凭据,界面只显示前 6 位。填新值即替换,留空则清掉。重启 daemon 生效",
        ),
        s(
            "telegram",
            "admin_chat_ids",
            "Telegram 管理员",
            cfg.telegram
                .admin_chat_ids
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            Kind::IntList,
            "逗号分隔的 chat id。只有这些人能用管理员命令。重启 daemon 生效",
        ),
        s(
            "telegram",
            "timezone",
            "Telegram 时区",
            cfg.telegram.timezone.clone(),
            Kind::Text,
            "形如 Asia/Shanghai。定时推送按这个时区算点。重启 daemon 生效",
        ),
    ]
}

fn on_off(b: bool) -> String {
    if b { "开" } else { "关" }.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_says_when_it_takes_effect() {
        let cfg = Config::default();
        for s in all(&cfg) {
            assert!(!s.note.is_empty(), "{} 没有说明", s.key);
            assert!(!s.label.is_empty());
        }
    }

    /// 凭据**绝不回显全文**(§11.3):列表里打码,编辑框里也不预填。
    #[test]
    fn the_bot_token_is_never_echoed_in_full() {
        let mut cfg = Config::default();
        cfg.telegram.bot_token = "1234567890:AAHverySecretTokenValue".into();
        let s = all(&cfg).into_iter().find(|s| s.key == "bot_token").unwrap();
        assert!(!s.shown.contains("verySecret"), "{}", s.shown);
        assert!(s.shown.starts_with("123456"), "前 6 位要给,好对号:{}", s.shown);
        assert_eq!(s.edit_value(), "", "编辑框不该预填旧凭据");

        cfg.telegram.bot_token = String::new();
        let s = all(&cfg).into_iter().find(|s| s.key == "bot_token").unwrap();
        assert_eq!(s.shown, "(未配置)");
    }

    /// 布尔项按一下就是取反,不该走「填 true/false」那条路。
    #[test]
    fn a_bool_setting_flips() {
        let cfg = Config::default();
        let s = all(&cfg).into_iter().find(|s| s.key == "tls").unwrap();
        assert!(matches!(s.kind, Kind::Bool(true)), "默认应当是开");
        assert_eq!(s.to_toml("").unwrap(), "false");
    }

    #[test]
    fn ints_are_validated() {
        let cfg = Config::default();
        let s = all(&cfg).into_iter().find(|s| s.key == "heartbeat_secs").unwrap();
        assert_eq!(s.to_toml("30").unwrap(), "30");
        assert!(s.to_toml("-1").is_err());
        assert!(s.to_toml("很快").is_err());
    }

    /// chat id 列表容忍中文逗号和空格 —— 从聊天软件里复制出来常常带这些。
    #[test]
    fn chat_ids_accept_loose_separators() {
        let cfg = Config::default();
        let s = all(&cfg).into_iter().find(|s| s.key == "admin_chat_ids").unwrap();
        assert_eq!(s.to_toml("1, 2,3").unwrap(), "[1, 2, 3]");
        assert_eq!(s.to_toml("1,2").unwrap(), "[1, 2]");
        assert_eq!(s.to_toml(" ").unwrap(), "[]");
        assert!(s.to_toml("abc").is_err());
    }

    /// 每一项都要能真的写回去,并且写完还能解析出**改后的值**。
    /// 这条把设置项定义与 `config::replace_in_toml` 串起来 ——
    /// section/key 拼错的话只有走一遍才发现。
    #[test]
    fn every_setting_round_trips_through_the_file() {
        let cfg = Config::default();
        let mut text = String::new();
        for s in all(&cfg) {
            let v = match s.kind {
                Kind::Bool(_) => s.to_toml("").unwrap(),
                Kind::Int => s.to_toml("42").unwrap(),
                Kind::IntList => s.to_toml("7").unwrap(),
                Kind::Text | Kind::Secret => s.to_toml("x").unwrap(),
            };
            text = config::replace_in_toml(&text, s.section, s.key, &v);
        }
        let back = Config::parse(&text).expect("写回去的东西必须还能解析");
        assert!(!back.cluster.tls, "布尔应当被翻转了");
        assert_eq!(back.cluster.heartbeat_secs, 42);
        assert_eq!(back.telegram.admin_chat_ids, vec![7]);
        assert_eq!(back.subscription.public_base, "x");
        assert_eq!(back.telegram.bot_token, "x");
    }
}
