// Package master 的线协议定义。
//
// 这份文件是 shared/src/proto.rs 的 Go 镜像,**必须逐字段对齐**。
// 两边各写一份而不是靠代码生成:整个协议只有一个消费者(Go agent)和
// 一个生产者(Rust master),十几个 struct 的重复成本远低于引一套 IDL 工具链。
// 代价是改协议要动两处 —— 所以 proto.rs 那边有一条 `none_fields_are_omitted_on_the_wire`
// 测试,专门守住「Option 为 None 时不上线」这个约定,让这边的 omitempty 能对上。
package master

import (
	"encoding/json"
	"errors"
)

// 信封结构性错误。这四个都是**断开连接**级别的,不是「忽略这条消息」级别的。
var (
	errProtoVersion = errors.New("协议版本不符")
	errMissingID    = errors.New("req/resp 缺少 id")
	errEventHasID   = errors.New("event 不应带 id")
	errBadKind      = errors.New("未知 kind")
)

// ProtoVersion 对应 shared/src/version.rs 的 PROTO_VERSION。
//
// **不匹配即断开连接**,不做协议协商、不做向下兼容层(DESIGN.md §4)。
// 改了这个常量必须同步改 Rust 那边,否则整个集群握不上手 —— 这是有意的:
// 版本漂移应该在第一秒就炸,而不是在某条罕见消息上才暴露。
const ProtoVersion = 1

// Kind 的三个取值。Rust 侧是 `#[serde(rename_all = "lowercase")]` 的 enum,
// 线上就是这三个小写字符串。
const (
	KindReq   = "req"
	KindResp  = "resp"
	KindEvent = "event"
)

// method 名字常量,与 proto.rs 的 `pub mod method` 一一对应。
const (
	MethodAgentHello    = "agent.hello"
	MethodAgentHelloAck = "agent.hello_ack"

	// 主控 → agent
	MethodConfigApply  = "config.apply"
	MethodConfigCheck  = "config.check"
	MethodUserState    = "user.state"
	MethodBoxRestart   = "box.restart"
	MethodBoxStatus    = "box.status"
	MethodStatsPull    = "stats.pull"
	MethodSysinfoPull  = "sysinfo.pull"
	MethodAgentUpgrade = "agent.upgrade"
	MethodPing         = "ping"

	// agent → 主控
	MethodStatsReport   = "stats.report"
	MethodSysinfoReport = "sysinfo.report"
	MethodBoxEvent      = "box.event"
	MethodLog           = "log"
	MethodPong          = "pong"
)

// Envelope 是线信封。
//
// id / error 用 omitempty:Rust 侧是 Option + skip_serializing_if,
// 缺字段才是「没有」,空串会被解成 Some("")。payload 反过来 —— Rust 侧
// 没有 skip_serializing_if,而且带 `#[serde(default)]`,所以发送时始终填,
// 至少填 `{}`。
type Envelope struct {
	V       uint32          `json:"v"`
	ID      string          `json:"id,omitempty"`
	Kind    string          `json:"kind"`
	Method  string          `json:"method"`
	Payload json.RawMessage `json:"payload"`
	Error   string          `json:"error,omitempty"`
}

// validate 是 proto.rs::Envelope::validate 的镜像。
//
// **返回非 nil 必须导致断开连接**(§4)。这条不是洁癖:一个结构上非法的信封
// 说明对面不是我们以为的那个东西(版本漂移、中间件改写、串号),
// 继续收下去只会把错误变成难查的数据问题。
func (e *Envelope) validate() error {
	if e.V != ProtoVersion {
		return errProtoVersion
	}
	switch e.Kind {
	case KindReq, KindResp:
		if e.ID == "" {
			return errMissingID
		}
	case KindEvent:
		if e.ID != "" {
			return errEventHasID
		}
	default:
		return errBadKind
	}
	return nil
}

// ─────────────────────────── 握手(§4.1)───────────────────────────

type AgentHello struct {
	Token          string `json:"token"`
	AgentVersion   string `json:"agent_version"`
	ProtoVersion   uint32 `json:"proto_version"`
	OS             string `json:"os"`
	Arch           string `json:"arch"`
	Hostname       string `json:"hostname"`
	BootID         string `json:"boot_id"`
	SingboxVersion string `json:"singbox_version"`
	// 两个 revision 取自本地 last-applied.json,**各自独立**(§4.1)。
	ConfigRevision    int64 `json:"config_revision"`
	UserStateRevision int64 `json:"user_state_revision"`
	// 自探公网地址。探不到时是 nil,序列化后字段整个消失 ——
	// 主控的 mark_online 用 COALESCE,null 保留库里已有的值,空串会把它冲掉。
	IPv4 *string `json:"ipv4,omitempty"`
	IPv6 *string `json:"ipv6,omitempty"`
	// 本机当前的 UTC 偏移秒数。主控拿它当网卡月重置边界的默认时区 ——
	// 厂商按自己机房的本地日界翻月。报偏移而不是时区名:主控不引 tzdata。
	// 夏令时靠每次握手重报跟随,所以这是「现在多少」而不是「属于哪个时区」。
	//
	// **不加 omitempty**:0 是有意义的值(机器就在 UTC),省掉它主控会当成
	// 「老 agent 什么都没报」而回落到自己的时区 —— 与上面两个 revision 同一个理由。
	UTCOffsetSecs int32 `json:"utc_offset_secs"`
}

type AgentHelloAck struct {
	AgentID            int64  `json:"agent_id"`
	ServerTime         int64  `json:"server_time"`
	HeartbeatSecs      uint64 `json:"heartbeat_secs"`
	ReportIntervalSecs uint64 `json:"report_interval_secs"`
	ConfigRevision     int64  `json:"config_revision"`
	UserStateRevision  int64  `json:"user_state_revision"`
}

// ─────────────────────────── 主控 → agent(§4.2)───────────────────────────

type ConfigApply struct {
	Revision int64           `json:"revision"`
	Options  json.RawMessage `json:"options"`
}

type ConfigCheck struct {
	Options json.RawMessage `json:"options"`
}

// UserState 是**全量**禁用名单,不是增量(§4.2)。
type UserState struct {
	UserStateRevision int64    `json:"user_state_revision"`
	Disabled          []string `json:"disabled"`
}

type AgentUpgrade struct {
	URL    string `json:"url"`
	SHA256 string `json:"sha256"`
}

type BoxStatus struct {
	Running bool   `json:"running"`
	Since   *int64 `json:"since,omitempty"`
	PidRSS  *int64 `json:"pid_rss,omitempty"`
}

// ─────────────────────────── agent → 主控(§4.3)───────────────────────────

// UserCounter 与 tracker.Snapshot 的 json tag 完全一致,所以上报时
// 可以直接复用 tracker 的切片,不用再抄一遍字段。
type UserCounter struct {
	Name string `json:"name"`
	Tag  string `json:"tag"`
	Up   int64  `json:"up"`
	Down int64  `json:"down"`
}

type StatsReport struct {
	CounterEpoch string `json:"counter_epoch"`
	Users        any    `json:"users"`
}

type BoxEvent struct {
	State   string `json:"state"`
	Message string `json:"message"`
}

type LogLine struct {
	Level string `json:"level"`
	Line  string `json:"line"`
}

type Pong struct {
	EchoTS int64 `json:"echo_ts"`
}
