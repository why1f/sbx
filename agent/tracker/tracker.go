// Package tracker 实现 sing-box 的 ConnectionTracker,记录 per-(用户, inbound tag) 累计流量。
//
// 机制与核实依据见 DESIGN.md §0.2 / §7.1。三条要点:
//
//   - **计数器活在这里,不归 box 所有。** box 重建时复用同一个 Tracker 实例,
//     那张 counters map 原样存活 —— 这是 §5.2「counter_epoch 不跟着 box.New() 走」的前提。
//   - **key 是 (用户, tag) 二元组,不是用户名。** 一个用户可以在同一台 agent 上有多个节点,
//     只按用户名记账会让它们塌成一个数字,而 master 的 user_traffic 主键是 (user_id, node_id)。
//   - **只增不减。** 删掉的用户/节点会继续上报到进程重启为止,master 侧丢弃未知条目(§4.3)。
//
// §12.0 的 spike 已在 sing-box v1.14.0-beta.3 上实测通过,本文件的两个待确认项都已落定:
//  1. inbound tag 的字段名确实是 `md.Inbound`,且填的是**配置里的 tag 字符串**
//     (实测 md.Inbound == "vless-in",与配置一致;另有 md.InboundType == "vless");
//  2. rejected(conn) 3ms 内生效,客户端看到的是普通的连接中断,不泄露账户状态。
//
// 实测同时确认了 up/down 的方向:`NewInt64CounterConn(conn, read, write)` 的
// read 是上行、write 是下行(与上游 service/ssmapi/traffic.go:116 的用法一致)。
package tracker

import (
	"context"
	"net"
	"sync"
	"sync/atomic"

	"github.com/sagernet/sing-box/adapter"
	tun "github.com/sagernet/sing-tun"
	"github.com/sagernet/sing/common/bufio"
	N "github.com/sagernet/sing/common/network"
)

// ctrKey 是记账口径:(用户名, inbound tag)。
// 少了 tag 就填不进 master 的 user_traffic(user_id, node_id) —— 见包注释。
type ctrKey struct {
	user string
	tag  string
}

type counter struct {
	up   atomic.Int64
	down atomic.Int64
}

// Snapshot 是一条上报记录,对应 shared/src/proto.rs 的 UserCounter。
type Snapshot struct {
	Name string `json:"name"`
	Tag  string `json:"tag"`
	Up   int64  `json:"up"`
	Down int64  `json:"down"`
}

type Tracker struct {
	mu       sync.RWMutex
	counters map[ctrKey]*counter
	// disabled 是被主控禁用的用户(§7.5)。按**用户**禁用,与 tag 无关。
	disabled map[string]struct{}
}

func New() *Tracker {
	return &Tracker{
		counters: make(map[ctrKey]*counter),
		disabled: make(map[string]struct{}),
	}
}

// 编译期断言:必须满足 sing-box 的 ConnectionTracker 接口(adapter/router.go:104)。
// 若上游改了接口,这一行会在编译时报错,而不是在运行时静默不记账。
var _ adapter.ConnectionTracker = (*Tracker)(nil)

func (t *Tracker) RoutedConnection(
	ctx context.Context,
	conn net.Conn,
	md adapter.InboundContext,
	rule adapter.Rule,
	out adapter.Outbound,
) net.Conn {
	if md.User == "" {
		// 未鉴权的 inbound(如纯 direct 的 http/socks),不建账。
		return conn
	}
	if t.isDisabled(md.User) {
		return rejected(conn)
	}
	c := t.counter(ctrKey{user: md.User, tag: md.Inbound})
	// NewInt64CounterConn 收 []*atomic.Int64,所以计数器用 atomic.Int64 而非 Uint64(§7.1)。
	return bufio.NewInt64CounterConn(conn, []*atomic.Int64{&c.up}, []*atomic.Int64{&c.down})
}

func (t *Tracker) RoutedPacketConnection(
	ctx context.Context,
	conn N.PacketConn,
	md adapter.InboundContext,
	rule adapter.Rule,
	out adapter.Outbound,
) N.PacketConn {
	if md.User == "" {
		return conn
	}
	if t.isDisabled(md.User) {
		return rejectedPacket(conn)
	}
	c := t.counter(ctrKey{user: md.User, tag: md.Inbound})
	// 五个参数:(conn, read, readPacket, write, writePacket)。
	// 包数用不上,传 nil —— 与上游 experimental/v2rayapi/stats.go:119 的做法一致。
	return bufio.NewInt64CounterPacketConn(conn,
		[]*atomic.Int64{&c.up}, nil, []*atomic.Int64{&c.down}, nil)
}

// RoutedFlow 只在 TUN 场景有意义。agent 不跑 TUN inbound,返回 nil(§7.1)。
//
// 返回类型必须是 tun.FlowTracker —— 写成 any 编译不过(接口签名精确匹配)。
func (t *Tracker) RoutedFlow(
	ctx context.Context,
	md adapter.InboundContext,
	rule adapter.Rule,
	out adapter.Outbound,
) tun.FlowTracker {
	return nil
}

// counter 取或建一个计数器。**无条件建账**,不需要预先声明用户名单
// —— 这是相对上游 v2rayapi 实现的简化(§0.2 第 3 点)。
func (t *Tracker) counter(k ctrKey) *counter {
	t.mu.RLock()
	c, ok := t.counters[k]
	t.mu.RUnlock()
	if ok {
		return c
	}
	t.mu.Lock()
	defer t.mu.Unlock()
	// 双检:两条连接同时首次出现时,只有一个应当建账。
	if c, ok := t.counters[k]; ok {
		return c
	}
	c = &counter{}
	t.counters[k] = c
	return c
}

func (t *Tracker) isDisabled(user string) bool {
	t.mu.RLock()
	defer t.mu.RUnlock()
	_, ok := t.disabled[user]
	return ok
}

// SetDisabled 替换整个禁用名单。
//
// 收的是**全量**名单而不是增量(§4.2):幂等,丢一条消息不会让状态永久偏移。
// **不重建 box、不影响其他用户已建立的连接。**
//
// 注意语义边界:这只挡**新**连接。已建立的连接会继续跑到自己结束为止(§7.5)。
func (t *Tracker) SetDisabled(users []string) {
	next := make(map[string]struct{}, len(users))
	for _, u := range users {
		next[u] = struct{}{}
	}
	t.mu.Lock()
	t.disabled = next
	t.mu.Unlock()
}

// Snapshot 快照全部计数器,**不 reset**(§5.3)。
//
// master 侧按 (name, tag) 定位 user_traffic 的行;匹配不到的条目直接丢弃(§4.3)。
func (t *Tracker) Snapshot() []Snapshot {
	t.mu.RLock()
	defer t.mu.RUnlock()
	out := make([]Snapshot, 0, len(t.counters))
	for k, c := range t.counters {
		out = append(out, Snapshot{
			Name: k.user,
			Tag:  k.tag,
			Up:   c.up.Load(),
			Down: c.down.Load(),
		})
	}
	return out
}
