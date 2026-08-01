//! 信封与全部 payload struct(DESIGN.md §4)。

use serde::{Deserialize, Serialize};

/// 消息类型。`req` 必须带 `id`,`event` 必须不带(§4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Req,
    Resp,
    Event,
}

/// 线信封。
///
/// ```json
/// { "v": 1, "id": "…", "kind": "req", "method": "config.apply", "payload": {}, "error": null }
/// ```
///
/// `payload` 保持为 `serde_json::Value` 而不是泛型 enum:
/// 解码分两步(先看 `method`,再按 method 解 payload)让**未知 method 可以被优雅拒绝**,
/// 而不是让整个信封解码失败——后者会使一条无法识别的消息毒死整个连接。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    /// `kind: Req` / `Resp` 必须有;`Event` 必须没有。用 `validate()` 检查。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: Kind,
    pub method: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Envelope {
    pub fn req(id: impl Into<String>, method: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            v: crate::PROTO_VERSION,
            id: Some(id.into()),
            kind: Kind::Req,
            method: method.into(),
            payload,
            error: None,
        }
    }

    pub fn resp_ok(id: impl Into<String>, method: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            v: crate::PROTO_VERSION,
            id: Some(id.into()),
            kind: Kind::Resp,
            method: method.into(),
            payload,
            error: None,
        }
    }

    pub fn resp_err(id: impl Into<String>, method: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            v: crate::PROTO_VERSION,
            id: Some(id.into()),
            kind: Kind::Resp,
            method: method.into(),
            payload: serde_json::Value::Null,
            error: Some(error.into()),
        }
    }

    pub fn event(method: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            v: crate::PROTO_VERSION,
            id: None,
            kind: Kind::Event,
            method: method.into(),
            payload,
            error: None,
        }
    }

    /// 结构性校验。返回 `Err` 的信封**必须**导致断开连接(§4)。
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.v != crate::PROTO_VERSION {
            return Err("协议版本不符");
        }
        match self.kind {
            Kind::Req | Kind::Resp if self.id.is_none() => Err("req/resp 缺少 id"),
            Kind::Event if self.id.is_some() => Err("event 不应带 id"),
            _ => Ok(()),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

// ─────────────────────────── method 名字常量 ───────────────────────────
// 用常量而不是散落的字符串字面量：method 名同时出现在 master 的发送端、
// master 的路由 match、以及 agent 的 Go 侧。前两个至少能靠常量对齐。

pub mod method {
    // 握手
    pub const AGENT_HELLO: &str = "agent.hello";
    pub const AGENT_HELLO_ACK: &str = "agent.hello_ack";
    // 主控 → agent
    pub const CONFIG_APPLY: &str = "config.apply";
    pub const CONFIG_CHECK: &str = "config.check";
    pub const USER_STATE: &str = "user.state";
    pub const BOX_RESTART: &str = "box.restart";
    pub const BOX_STATUS: &str = "box.status";
    pub const STATS_PULL: &str = "stats.pull";
    pub const SYSINFO_PULL: &str = "sysinfo.pull";
    pub const AGENT_UPGRADE: &str = "agent.upgrade";
    pub const PING: &str = "ping";
    // agent → 主控
    pub const STATS_REPORT: &str = "stats.report";
    pub const SYSINFO_REPORT: &str = "sysinfo.report";
    pub const BOX_EVENT: &str = "box.event";
    pub const LOG: &str = "log";
    pub const PONG: &str = "pong";
}

// ─────────────────────────── 握手(§4.1)───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHello {
    pub token: String,
    pub agent_version: String,
    pub proto_version: u32,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    /// `/proc/sys/kernel/random/boot_id` —— 网卡流量的 epoch(§5.2)
    pub boot_id: String,
    pub singbox_version: String,
    /// 取自本地 `last-applied.json`;从未落过盘时为 0。
    #[serde(default)]
    pub config_revision: i64,
    /// **与 `config_revision` 独立**。合并二者会造成 §4.1 描述的两种 bug 之一。
    #[serde(default)]
    pub user_state_revision: i64,
    /// agent 自探的公网 IP(§7.3),主控写入 `agents.ipv4/ipv6` 且允许手工覆盖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHelloAck {
    pub agent_id: i64,
    pub server_time: i64,
    pub heartbeat_secs: u64,
    pub report_interval_secs: u64,
    pub config_revision: i64,
    pub user_state_revision: i64,
}

// ─────────────────────────── 主控 → agent(§4.2)───────────────────────────

/// `config.apply` —— `options` 是完整的 sing-box 配置。
///
/// 语义严格按 §4.2:`box.New()` 成功 → 写 tmp → rename → 切换。
/// 任一步失败保持旧配置运行并回 error,主控**不推进 revision**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigApply {
    pub revision: i64,
    /// 完整 sing-box 配置。主控侧不解析它的内部结构,原样透传给 agent。
    pub options: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigCheck {
    pub options: serde_json::Value,
}

/// `user.state` —— **全量**禁用名单,不是增量(§4.2)。
///
/// 全量的理由:幂等。丢一条消息不会让状态永久偏移。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserState {
    pub user_state_revision: i64,
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentUpgrade {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxStatus {
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_rss: Option<i64>,
}

/// 空 payload 的 resp(`ok`)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ok {}

// ─────────────────────────── agent → 主控(§4.3)───────────────────────────

/// 一条 per-(用户, inbound tag) 的累计计数。
///
/// **`tag` 不是可选的。** §0.0(3) 允许一个用户在同一台 agent 上有多个节点;
/// 少了 tag,这些节点的流量会塌成一个数字,而 `user_traffic` 的主键是
/// `(user_id, node_id)`,填不进去(§4.3 / §7.1)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCounter {
    pub name: String,
    /// 承载该用户的 inbound tag。对应 `nodes.tag`。
    pub tag: String,
    pub up: i64,
    pub down: i64,
}

/// `stats.report` —— 单调累计值,**永不 reset**(§5.3)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsReport {
    /// agent **进程**启动时生成的 UUID,进程存活期间不变。
    /// 注意:**不跟 box 重建走**——计数器活在 agent 自己的 tracker 里(§5.2)。
    pub counter_epoch: String,
    pub users: Vec<UserCounter>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NicCounter {
    pub rx: i64,
    pub tx: i64,
}

/// `sysinfo.report`。网卡流量口径是**整机物理网卡 RX+TX**,
/// 与用户代理流量是两套不同的数字,不要试图对齐(§7.2)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysinfoReport {
    /// 网卡流量的 epoch(§5.2)。
    pub boot_id: String,
    pub nic: NicCounter,
    pub cpu_pct: f64,
    pub mem_used: i64,
    pub mem_total: i64,
    pub load1: f64,
    pub uptime_secs: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxEvent {
    pub state: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub level: String,
    pub line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pong {
    pub echo_ts: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_rejects_version_mismatch() {
        let mut e = Envelope::event(method::PONG, serde_json::json!({}));
        e.v = PROTO_VERSION_FOR_TEST_MISMATCH;
        assert_eq!(e.validate(), Err("协议版本不符"));
    }
    const PROTO_VERSION_FOR_TEST_MISMATCH: u32 = crate::PROTO_VERSION + 1;

    #[test]
    fn envelope_requires_id_on_req_and_forbids_on_event() {
        let ok = Envelope::req("1", method::BOX_STATUS, serde_json::Value::Null);
        assert!(ok.validate().is_ok());

        let mut bad = ok.clone();
        bad.id = None;
        assert_eq!(bad.validate(), Err("req/resp 缺少 id"));

        let mut bad_event = Envelope::event(method::PONG, serde_json::json!({}));
        bad_event.id = Some("nope".into());
        assert_eq!(bad_event.validate(), Err("event 不应带 id"));
    }

    /// `id` 与 `error` 在 None 时不应出现在线格式里——agent 侧是手写的 Go struct,
    /// 多余的 `"error": null` 会让它的解码更难写。
    #[test]
    fn none_fields_are_omitted_on_the_wire() {
        let e = Envelope::event(method::PONG, serde_json::json!({"echo_ts": 1}));
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("\"id\""), "event 不该序列化出 id: {s}");
        assert!(!s.contains("\"error\""), "无错误时不该序列化出 error: {s}");
        assert!(s.contains("\"kind\":\"event\""), "kind 应为小写: {s}");
    }

    /// 记账口径的回归锚点:`tag` 必须在线格式里存在。
    /// 若有人「简化」掉它,这个测试会失败——而不是等到流量数字塌掉才发现。
    #[test]
    fn stats_report_carries_tag_per_record() {
        let r = StatsReport {
            counter_epoch: "epoch-1".into(),
            users: vec![
                UserCounter { name: "alice".into(), tag: "vless-in".into(), up: 1, down: 2 },
                UserCounter { name: "alice".into(), tag: "trojan-in".into(), up: 3, down: 4 },
            ],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: StatsReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back.users.len(), 2);
        // 同一用户的两个 tag 必须是两条独立记录,不能合并
        assert_ne!(back.users[0].tag, back.users[1].tag);
        assert_eq!(back.users[0].name, back.users[1].name);
    }

    /// hello 的两个 revision 必须分别独立地出现在线格式里(§4.1)。
    #[test]
    fn hello_carries_two_independent_revisions() {
        let h = AgentHello {
            token: "t".into(),
            agent_version: "0.1.0".into(),
            proto_version: crate::PROTO_VERSION,
            os: "linux".into(),
            arch: "amd64".into(),
            hostname: "h".into(),
            boot_id: "b".into(),
            singbox_version: "1.14.0".into(),
            config_revision: 7,
            user_state_revision: 3,
            ipv4: None,
            ipv6: None,
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["config_revision"], 7);
        assert_eq!(v["user_state_revision"], 3);
    }

    /// 老 agent(或 stub)不带 revision 字段时应解成 0,而不是解码失败。
    #[test]
    fn hello_revisions_default_to_zero_when_absent() {
        let h: AgentHello = serde_json::from_value(serde_json::json!({
            "token": "t", "agent_version": "0.1.0", "proto_version": 1,
            "os": "linux", "arch": "amd64", "hostname": "h",
            "boot_id": "b", "singbox_version": "1.14.0"
        }))
        .unwrap();
        assert_eq!(h.config_revision, 0);
        assert_eq!(h.user_state_revision, 0);
    }
}
