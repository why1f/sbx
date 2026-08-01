// Command spike 验证 DESIGN.md §0.2 / §12.0 的三个未实测假设。
//
// **在信任 agent/ 的任何实现之前先跑这个。** 它不属于 sbx 工作区(自己一个 go.mod)。
//
// 与最初的手工版本不同,现在是**全自动**的:进程内起两个 box(服务端 + 客户端)
// 加一个 echo 服务器,自己把流量跑起来,自己断言,exit code 即结论。
// 不需要人拿客户端 App 连上来 —— 那让「验证」变成一次性的手工操作,
// 没法在 CI 里重跑,也没法在升级 sing-box 后回归。
//
// 拓扑:
//
//	driver --socks5--> [客户端 box: mixed-in → vless-out] --vless--> [服务端 box: vless-in → direct-out] --> echo
//	                                                                        ↑
//	                                                                  tracker 挂在这里
//
// 三个验证点:
//
//  1. **统计确实到账,且方向没反。** 上下行**刻意不对称**(上行 128 KiB,下行 2 MiB,
//     16:1)。若 up/down 接反,断言会立刻抓到 —— 对称的 echo 测不出这个。
//  2. **rejected(conn) 的客户端观感。** 禁用用户后再连一次,打印客户端拿到的
//     错误与耗时,并断言错误信息**不泄露账户状态**(§8.1 同源考虑)。
//  3. **InboundContext 上 inbound tag 的字段名。** 打印整个 md,并断言拿到的
//     tag 是**配置里的 tag 字符串**而不是某种内部 ID。
//
// 下面的 tracker 是 agent/tracker 的**逐行镜像**(含 rejected/deadConn)。
// 刻意重复而不是 import —— agent/ 是 GPLv3,spike 是独立 module,
// 而且这里要验的正是「那份代码能不能编译、跑起来对不对」。
// **改了 agent/tracker 就要同步改这里,否则验的不是同一份东西。**
//
// 用法:
//
//	cd spike && go mod tidy && go run .
package main

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/netip"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	box "github.com/sagernet/sing-box"
	"github.com/sagernet/sing-box/adapter"
	"github.com/sagernet/sing-box/include"
	"github.com/sagernet/sing-box/option"
	tun "github.com/sagernet/sing-tun"
	"github.com/sagernet/sing/common/bufio"
	"github.com/sagernet/sing/common/json/badoption"
	N "github.com/sagernet/sing/common/network"
	"golang.org/x/net/proxy"
)

const (
	testUser = "spike-user"
	testUUID = "b831381d-6324-4d53-ad4f-8cda48b30811"
	testTag  = "vless-in"

	vlessPort = 5533
	socksPort = 5534
	echoPort  = 5535

	// 上下行刻意不对称,比例 16:1。方向接反时断言必然失败。
	upBytes   = 128 * 1024
	downBytes = 2 * 1024 * 1024

	// 计数允许的额外开销(协议头、分片等)。远小于 up/down 的差距,
	// 所以它松一点也不会让方向判断失效。
	slack = 32 * 1024
)

// ─────────────────────────── tracker(agent/tracker 的镜像)───────────────────────────

type ctrKey struct {
	user string
	tag  string
}

type counter struct {
	up   atomic.Int64
	down atomic.Int64
}

type Snapshot struct {
	Name string
	Tag  string
	Up   int64
	Down int64
}

type Tracker struct {
	mu       sync.RWMutex
	counters map[ctrKey]*counter
	disabled map[string]struct{}

	// 下面两个是 spike 特有的,agent 里没有:留证据用。
	dumped     atomic.Bool
	firstMeta  atomic.Value // adapter.InboundContext
	rejections atomic.Int64
}

func newTracker() *Tracker {
	return &Tracker{
		counters: make(map[ctrKey]*counter),
		disabled: make(map[string]struct{}),
	}
}

// 编译期断言 —— 这一行本身就是验证点 3 的一半:接口签名对不上就编译不过。
var _ adapter.ConnectionTracker = (*Tracker)(nil)

func (t *Tracker) RoutedConnection(
	ctx context.Context, conn net.Conn, md adapter.InboundContext,
	rule adapter.Rule, out adapter.Outbound,
) net.Conn {
	if t.dumped.CompareAndSwap(false, true) {
		t.firstMeta.Store(md)
		fmt.Printf("\n─── 验证点 3:InboundContext 实际内容 ───\n")
		fmt.Printf("md.User    = %q\n", md.User)
		fmt.Printf("md.Inbound = %q   ← §7.1 假设的 tag 字段\n", md.Inbound)
		fmt.Printf("md.InboundType = %q\n", md.InboundType)
		fmt.Printf("md.Destination = %s\n", md.Destination)
		fmt.Printf("────────────────────────────────────────\n\n")
	}

	if md.User == "" {
		return conn
	}
	if t.isDisabled(md.User) {
		t.rejections.Add(1)
		log.Printf("[tracker] 拒绝被禁用用户 %q 的新连接", md.User)
		return rejected(conn)
	}
	c := t.counter(ctrKey{user: md.User, tag: md.Inbound})
	log.Printf("[tracker] 建账 user=%q tag=%q", md.User, md.Inbound)
	return bufio.NewInt64CounterConn(conn, []*atomic.Int64{&c.up}, []*atomic.Int64{&c.down})
}

func (t *Tracker) RoutedPacketConnection(
	ctx context.Context, conn N.PacketConn, md adapter.InboundContext,
	rule adapter.Rule, out adapter.Outbound,
) N.PacketConn {
	if md.User == "" {
		return conn
	}
	if t.isDisabled(md.User) {
		t.rejections.Add(1)
		return rejectedPacket(conn)
	}
	c := t.counter(ctrKey{user: md.User, tag: md.Inbound})
	// 注意参数是 5 个:(conn, read, readPacket, write, writePacket)。
	// 包计数用不上,传 nil —— 与上游 v2rayapi/stats.go:119 的做法一致。
	return bufio.NewInt64CounterPacketConn(conn,
		[]*atomic.Int64{&c.up}, nil, []*atomic.Int64{&c.down}, nil)
}

// RoutedFlow 只在 TUN 场景有意义。agent 不跑 TUN inbound,返回 nil。
// 返回类型是 tun.FlowTracker(agent/tracker 里原本写的是 any,编译不过)。
func (t *Tracker) RoutedFlow(
	ctx context.Context, md adapter.InboundContext,
	rule adapter.Rule, out adapter.Outbound,
) tun.FlowTracker {
	return nil
}

func (t *Tracker) counter(k ctrKey) *counter {
	t.mu.RLock()
	c, ok := t.counters[k]
	t.mu.RUnlock()
	if ok {
		return c
	}
	t.mu.Lock()
	defer t.mu.Unlock()
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

func (t *Tracker) SetDisabled(users []string) {
	next := make(map[string]struct{}, len(users))
	for _, u := range users {
		next[u] = struct{}{}
	}
	t.mu.Lock()
	t.disabled = next
	t.mu.Unlock()
}

func (t *Tracker) Snapshot() []Snapshot {
	t.mu.RLock()
	defer t.mu.RUnlock()
	out := make([]Snapshot, 0, len(t.counters))
	for k, c := range t.counters {
		out = append(out, Snapshot{Name: k.user, Tag: k.tag, Up: c.up.Load(), Down: c.down.Load()})
	}
	return out
}

// ─────────────────────────── reject(agent/tracker/reject.go 的镜像)───────────────────────────

var ErrUserDisabled = errors.New("connection refused")

func rejected(conn net.Conn) net.Conn {
	_ = conn.Close()
	return &deadConn{Conn: conn}
}

func rejectedPacket(conn N.PacketConn) N.PacketConn {
	_ = conn.Close()
	return conn
}

type deadConn struct{ net.Conn }

func (d *deadConn) Read([]byte) (int, error)  { return 0, ErrUserDisabled }
func (d *deadConn) Write([]byte) (int, error) { return 0, ErrUserDisabled }
func (d *deadConn) Close() error              { return nil }

// ─────────────────────────── echo 服务器 ───────────────────────────

// 协议:客户端发 8 字节头(大端 uint32 上行长度 + uint32 下行长度),
// 然后发上行数据;服务端读完后回下行数据再关闭。
// 用不对称的长度是为了把「方向接反」变成可断言的失败。
func serveEcho(ln net.Listener) {
	for {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		go func(c net.Conn) {
			defer c.Close()
			var hdr [8]byte
			if _, err := io.ReadFull(c, hdr[:]); err != nil {
				return
			}
			up := binary.BigEndian.Uint32(hdr[0:4])
			down := binary.BigEndian.Uint32(hdr[4:8])
			if _, err := io.CopyN(io.Discard, c, int64(up)); err != nil {
				return
			}
			_, _ = io.CopyN(c, &patternReader{}, int64(down))
		}(c)
	}
}

type patternReader struct{ off int }

var pattern = []byte("sbx-spike-payload-0123456789abcdef")

func (p *patternReader) Read(dst []byte) (int, error) {
	if p.off >= len(pattern) {
		p.off = 0
	}
	n := copy(dst, pattern[p.off:])
	p.off += n
	return n, nil
}

// ─────────────────────────── 流量驱动 ───────────────────────────

func driveTraffic(socksAddr, target string, up, down int) error {
	d, err := proxy.SOCKS5("tcp", socksAddr, nil, proxy.Direct)
	if err != nil {
		return fmt.Errorf("建 socks5 dialer: %w", err)
	}
	cd, ok := d.(proxy.ContextDialer)
	if !ok {
		return errors.New("socks5 dialer 不支持 DialContext")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	conn, err := cd.DialContext(ctx, "tcp", target)
	if err != nil {
		return fmt.Errorf("经代理连 %s: %w", target, err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(30 * time.Second))

	var hdr [8]byte
	binary.BigEndian.PutUint32(hdr[0:4], uint32(up))
	binary.BigEndian.PutUint32(hdr[4:8], uint32(down))
	if _, err := conn.Write(hdr[:]); err != nil {
		return fmt.Errorf("写头: %w", err)
	}
	if _, err := io.CopyN(conn, &patternReader{}, int64(up)); err != nil {
		return fmt.Errorf("写上行 %d 字节: %w", up, err)
	}
	n, err := io.CopyN(io.Discard, conn, int64(down))
	if err != nil {
		return fmt.Errorf("读下行(收到 %d/%d 字节): %w", n, down, err)
	}
	return nil
}

// ─────────────────────────── box 装配 ───────────────────────────

func localAddr() badoption.Addr {
	return badoption.Addr(netip.MustParseAddr("127.0.0.1"))
}

func newServerBox(t *Tracker) (*box.Box, error) {
	listen := localAddr()
	opts := option.Options{
		Log: &option.LogOptions{Level: "warn"},
		Inbounds: []option.Inbound{{
			Type: "vless",
			Tag:  testTag,
			Options: &option.VLESSInboundOptions{
				ListenOptions: option.ListenOptions{Listen: &listen, ListenPort: vlessPort},
				Users:         []option.VLESSUser{{Name: testUser, UUID: testUUID}},
			},
		}},
		Outbounds: []option.Outbound{{
			Type: "direct", Tag: "direct", Options: &option.DirectOutboundOptions{},
		}},
		Route: &option.RouteOptions{Final: "direct"},
	}
	// include.Context() 不能少 —— 少了 box.New 直接报 "missing inbound registry in context"。
	b, err := box.New(box.Options{Context: include.Context(context.Background()), Options: opts})
	if err != nil {
		return nil, err
	}
	// 顺序不能变:AppendTracker 必须在 Start() 之前(§7.1)。
	// 它内部是无锁 append,有流量时并发调用会 race。
	b.Router().AppendTracker(t)
	return b, nil
}

func newClientBox() (*box.Box, error) {
	listen := localAddr()
	opts := option.Options{
		Log: &option.LogOptions{Level: "warn"},
		Inbounds: []option.Inbound{{
			Type: "mixed",
			Tag:  "socks-in",
			Options: &option.HTTPMixedInboundOptions{
				ListenOptions: option.ListenOptions{Listen: &listen, ListenPort: socksPort},
			},
		}},
		Outbounds: []option.Outbound{{
			Type: "vless",
			Tag:  "proxy",
			Options: &option.VLESSOutboundOptions{
				ServerOptions: option.ServerOptions{Server: "127.0.0.1", ServerPort: vlessPort},
				UUID:          testUUID,
			},
		}},
		Route: &option.RouteOptions{Final: "proxy"},
	}
	return box.New(box.Options{Context: include.Context(context.Background()), Options: opts})
}

func waitPort(addr string, d time.Duration) error {
	deadline := time.Now().Add(d)
	for time.Now().Before(deadline) {
		c, err := net.DialTimeout("tcp", addr, time.Second)
		if err == nil {
			_ = c.Close()
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("%s 在 %s 内没起来", addr, d)
}

// ─────────────────────────── main ───────────────────────────

func main() {
	// 看门狗:spike 是要放进 CI 的,卡住比失败更糟。
	go func() {
		time.Sleep(150 * time.Second)
		fmt.Fprintln(os.Stderr, "\n❌ 超时:spike 卡住超过 150s")
		os.Exit(2)
	}()

	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "\n❌ spike 失败:%v\n", err)
		os.Exit(1)
	}
}

func run() error {
	tracker := newTracker()

	echoLn, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", echoPort))
	if err != nil {
		return fmt.Errorf("起 echo 服务器: %w", err)
	}
	defer echoLn.Close()
	go serveEcho(echoLn)

	server, err := newServerBox(tracker)
	if err != nil {
		return fmt.Errorf("box.New(服务端): %w", err)
	}
	if err := server.Start(); err != nil {
		return fmt.Errorf("box.Start(服务端): %w", err)
	}
	defer server.Close()

	client, err := newClientBox()
	if err != nil {
		return fmt.Errorf("box.New(客户端): %w", err)
	}
	if err := client.Start(); err != nil {
		return fmt.Errorf("box.Start(客户端): %w", err)
	}
	defer client.Close()

	socksAddr := fmt.Sprintf("127.0.0.1:%d", socksPort)
	echoAddr := fmt.Sprintf("127.0.0.1:%d", echoPort)
	if err := waitPort(fmt.Sprintf("127.0.0.1:%d", vlessPort), 10*time.Second); err != nil {
		return err
	}
	if err := waitPort(socksAddr, 10*time.Second); err != nil {
		return err
	}

	// ── 验证点 1 + 3:跑流量,看账 ──
	fmt.Printf("跑流量:上行 %d 字节,下行 %d 字节(比例 %d:1)\n",
		upBytes, downBytes, downBytes/upBytes)
	if err := driveTraffic(socksAddr, echoAddr, upBytes, downBytes); err != nil {
		return fmt.Errorf("跑流量: %w", err)
	}
	// 计数发生在 conn 关闭路径上,给它一点时间落定。
	time.Sleep(500 * time.Millisecond)

	snap := tracker.Snapshot()
	fmt.Printf("\n─── 验证点 1:计数器快照 ───\n")
	for _, s := range snap {
		fmt.Printf("  user=%q tag=%q up=%d down=%d\n", s.Name, s.Tag, s.Up, s.Down)
	}
	fmt.Printf("───────────────────────────\n\n")

	if len(snap) == 0 {
		return errors.New("计数器为空 —— 统计根本没到账,§0.2 的整个方案不成立")
	}
	if len(snap) != 1 {
		return fmt.Errorf("期望恰好 1 条记账,得到 %d 条", len(snap))
	}
	s := snap[0]
	if s.Name != testUser {
		return fmt.Errorf("用户名对不上:期望 %q,得到 %q", testUser, s.Name)
	}
	// 验证点 3 的关键断言:tag 必须是**配置里的字符串**,不是内部 ID。
	if s.Tag != testTag {
		return fmt.Errorf("inbound tag 对不上:期望 %q,得到 %q —— "+
			"md.Inbound 填的不是配置里的 tag,§7.1 的记账口径要改", testTag, s.Tag)
	}

	wantUp := int64(upBytes + 8) // 8 = 协议头
	wantDown := int64(downBytes)
	if s.Up < wantUp || s.Up > wantUp+slack {
		return fmt.Errorf("上行计数 %d 不在期望区间 [%d, %d]", s.Up, wantUp, wantUp+slack)
	}
	if s.Down < wantDown || s.Down > wantDown+slack {
		return fmt.Errorf("下行计数 %d 不在期望区间 [%d, %d]", s.Down, wantDown, wantDown+slack)
	}
	// 方向断言:上下行 16:1,接反时这一条必然失败。
	if s.Down < s.Up*4 {
		return fmt.Errorf("up/down 方向接反了:up=%d down=%d,但实际跑的是上行 %d、下行 %d",
			s.Up, s.Down, upBytes, downBytes)
	}
	fmt.Printf("✓ 验证点 1:统计到账,用户名与 tag 均正确,up/down 方向正确\n")
	fmt.Printf("✓ 验证点 3:md.Inbound 确实是配置里的 tag 字符串 %q\n", testTag)

	// ── 验证点 2:禁用后的客户端观感 ──
	beforeUp, beforeDown := s.Up, s.Down
	tracker.SetDisabled([]string{testUser})
	fmt.Printf("\n已禁用 %q,再连一次观察客户端表现……\n", testUser)

	start := time.Now()
	err = driveTraffic(socksAddr, echoAddr, 1024, 1024)
	elapsed := time.Since(start)

	fmt.Printf("\n─── 验证点 2:被禁用用户的客户端观感 ───\n")
	fmt.Printf("耗时:%s\n", elapsed.Round(time.Millisecond))
	if err == nil {
		return errors.New("被禁用的用户竟然连通了 —— §7.5 的禁用路径无效")
	}
	fmt.Printf("客户端拿到的错误:%v\n", err)
	fmt.Printf("──────────────────────────────────────\n\n")

	if elapsed > 10*time.Second {
		return fmt.Errorf("拒绝耗时 %s,太久了 —— 客户端会表现为卡住而不是失败", elapsed)
	}
	// 不该泄露账户状态(§8.1 同源考虑)。
	low := strings.ToLower(err.Error())
	for _, leak := range []string{"disabl", "quota", "expire", "ban", "suspend"} {
		if strings.Contains(low, leak) {
			return fmt.Errorf("错误信息泄露了账户状态(含 %q):%v", leak, err)
		}
	}
	if tracker.rejections.Load() == 0 {
		return errors.New("tracker 没有记录到拒绝 —— 连接没走到 RoutedConnection")
	}

	after := tracker.Snapshot()
	if len(after) != 1 || after[0].Up != beforeUp || after[0].Down != beforeDown {
		return fmt.Errorf("被拒绝的连接不该产生记账:之前 up=%d down=%d,之后 %+v",
			beforeUp, beforeDown, after)
	}
	fmt.Printf("✓ 验证点 2:拒绝在 %s 内生效,错误信息不泄露账户状态,且不产生记账\n",
		elapsed.Round(time.Millisecond))

	fmt.Printf("\n✅ 三个验证点全部通过。把结论写回 DESIGN.md §12.0。\n")
	return nil
}
