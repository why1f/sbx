package master

import (
	"context"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"github.com/sagernet/sing-box/adapter"

	"github.com/yourorg/sbx-agent/boxctl"
	"github.com/yourorg/sbx-agent/config"
	"github.com/yourorg/sbx-agent/state"
	"github.com/yourorg/sbx-agent/tracker"
)

// 这组测试用一个**假主控**跑真实的 Conn。它守的是 conn.go 里三条最容易悄悄坏掉的约定:
//
//  1. 每一条 req 都有 resp —— 漏一条,真主控的 catch_up 会卡到 30s RPC 超时;
//  2. 信封版本不符必须断开 —— 与 Rust 侧 recv_loop 对称;
//  3. user.state 不重建 box,只换 tracker 的名单。
//
// 它**不**启动真的 sing-box:box 的装配路径由 spike/ 覆盖(§12.0),
// 这里只走 boxctl 的失败分支,免得单测去抢端口。

type fakeMaster struct {
	srv   *httptest.Server
	conns chan *websocket.Conn
	done  chan struct{}
}

func startFakeMaster(t *testing.T) *fakeMaster {
	t.Helper()
	fm := &fakeMaster{
		conns: make(chan *websocket.Conn, 8),
		done:  make(chan struct{}),
	}
	up := websocket.Upgrader{}
	mux := http.NewServeMux()
	// 路径必须是 /ws —— 真主控就挂在这里(server.rs 的 route("/ws", ...))。
	mux.HandleFunc("/ws", func(w http.ResponseWriter, r *http.Request) {
		ws, err := up.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		fm.conns <- ws
		<-fm.done // 让 handler 活着,否则 httptest 会在它返回时关掉连接
	})
	fm.srv = httptest.NewServer(mux)
	t.Cleanup(func() {
		close(fm.done)
		fm.srv.Close()
	})
	return fm
}

func (fm *fakeMaster) accept(t *testing.T) *websocket.Conn {
	t.Helper()
	select {
	case ws := <-fm.conns:
		return ws
	case <-time.After(5 * time.Second):
		t.Fatal("agent 没有连上来")
		return nil
	}
}

func (fm *fakeMaster) wsURL() string {
	return "ws://" + strings.TrimPrefix(fm.srv.URL, "http://") + "/ws"
}

func readEnv(t *testing.T, ws *websocket.Conn) *Envelope {
	t.Helper()
	_ = ws.SetReadDeadline(time.Now().Add(5 * time.Second))
	mt, data, err := ws.ReadMessage()
	if err != nil {
		t.Fatalf("读消息: %v", err)
	}
	if mt != websocket.TextMessage {
		t.Fatalf("期望 text 帧,得到 %d", mt)
	}
	var env Envelope
	if err := json.Unmarshal(data, &env); err != nil {
		t.Fatalf("解析信封 %s: %v", data, err)
	}
	if err := env.validate(); err != nil {
		t.Fatalf("agent 发来的信封没通过校验(真主控会直接断开): %v — %s", err, data)
	}
	return &env
}

// readUntil 跳过周期性上报,等一条指定 method 的消息。
func readUntil(t *testing.T, ws *websocket.Conn, method string) *Envelope {
	t.Helper()
	deadline := time.Now().Add(8 * time.Second)
	for time.Now().Before(deadline) {
		env := readEnv(t, ws)
		if env.Method == method {
			return env
		}
	}
	t.Fatalf("等不到 %s", method)
	return nil
}

func writeEnv(t *testing.T, ws *websocket.Conn, env *Envelope) {
	t.Helper()
	data, err := json.Marshal(env)
	if err != nil {
		t.Fatal(err)
	}
	_ = ws.SetWriteDeadline(time.Now().Add(5 * time.Second))
	if err := ws.WriteMessage(websocket.TextMessage, data); err != nil {
		t.Fatalf("写消息: %v", err)
	}
}

// handshakeAs 扮演主控完成握手,返回 agent 发来的 hello。
func handshakeAs(t *testing.T, ws *websocket.Conn) *AgentHello {
	t.Helper()
	env := readEnv(t, ws)
	if env.Method != MethodAgentHello || env.Kind != KindReq {
		t.Fatalf("第一条消息应是 agent.hello 的 req,得到 %s/%s", env.Kind, env.Method)
	}
	var hello AgentHello
	if err := json.Unmarshal(env.Payload, &hello); err != nil {
		t.Fatalf("解析 hello: %v", err)
	}
	ack := AgentHelloAck{
		AgentID:            1,
		ServerTime:         time.Now().Unix(),
		HeartbeatSecs:      1,
		ReportIntervalSecs: 1,
		ConfigRevision:     hello.ConfigRevision,
		UserStateRevision:  hello.UserStateRevision,
	}
	// 复用 hello 的 id —— 真主控就是这么回的(server.rs 的 resp_ok(hello_id, ...))。
	writeEnv(t, ws, respOK(env.ID, MethodAgentHelloAck, ack))
	return &hello
}

type harness struct {
	fm      *fakeMaster
	ws      *websocket.Conn
	conn    *Conn
	tracker *tracker.Tracker
	dir     string
	cancel  context.CancelFunc
}

func newHarness(t *testing.T, st *state.LastApplied) *harness {
	t.Helper()
	fm := startFakeMaster(t)
	dir := t.TempDir()
	cfg := &config.Config{Server: fm.wsURL(), Token: "test-token", StateDir: dir}
	tr := tracker.New()
	if st == nil {
		st = &state.LastApplied{}
	}
	conn := NewConn(cfg, tr, boxctl.New(tr), st, "epoch-under-test")
	// 关掉公网地址自探。它在拨号**之前**跑,真去打外网会让这些测试
	// 随外网状况随机超时 —— 而这一组测试要验的是 WS 协议,不是 IP 探测。
	conn.sys.SetEndpoints(nil, nil)

	ctx, cancel := context.WithCancel(context.Background())
	go func() { _ = conn.Run(ctx) }()
	t.Cleanup(cancel)

	return &harness{fm: fm, ws: fm.accept(t), conn: conn, tracker: tr, dir: dir, cancel: cancel}
}

func TestHandshakeSendsRequiredFields(t *testing.T) {
	h := newHarness(t, &state.LastApplied{ConfigRevision: 3, UserStateRevision: 9})
	hello := handshakeAs(t, h.ws)

	if hello.Token != "test-token" {
		t.Errorf("token = %q", hello.Token)
	}
	if hello.ProtoVersion != ProtoVersion {
		t.Errorf("proto_version = %d, 期望 %d(不匹配主控会直接拒绝握手)", hello.ProtoVersion, ProtoVersion)
	}
	// 两个 revision 必须来自 last-applied.json,而且各报各的。
	if hello.ConfigRevision != 3 || hello.UserStateRevision != 9 {
		t.Errorf("revision = (%d,%d), 期望 (3,9)", hello.ConfigRevision, hello.UserStateRevision)
	}
	if hello.BootID == "" {
		t.Error("boot_id 为空 —— 它是网卡计数器的 epoch,少了主控算不出增量")
	}
	if hello.SingboxVersion == "" {
		t.Error("singbox_version 为空")
	}
	if hello.OS == "" || hello.Arch == "" {
		t.Error("os/arch 为空")
	}
}

// 最重要的一条:主控的 catch_up 是串行的,漏回一条 resp 它就卡死。
func TestEveryRequestGetsAResponse(t *testing.T) {
	h := newHarness(t, nil)
	handshakeAs(t, h.ws)

	cases := []struct {
		id      string
		method  string
		payload any
		wantErr bool
	}{
		{"r1", MethodUserState, UserState{UserStateRevision: 1, Disabled: []string{"alice"}}, false},
		{"r2", MethodBoxStatus, struct{}{}, false},
		{"r3", MethodStatsPull, struct{}{}, false},
		{"r4", MethodSysinfoPull, struct{}{}, false},
		{"r5", MethodPing, map[string]int64{"ts": 42}, false},
		// 没有已应用配置时 restart 无从下手,但**仍然必须回一条 resp**。
		{"r6", MethodBoxRestart, struct{}{}, true},
		// 未知 method 也要回,否则主控那侧的 rpc 槽位会挂到超时。
		{"r7", "no.such.method", struct{}{}, true},
	}
	for _, tc := range cases {
		writeEnv(t, h.ws, req(tc.id, tc.method, tc.payload))
		var got *Envelope
		for {
			got = readEnv(t, h.ws)
			if got.Kind == KindResp {
				break
			}
			// 期间夹着的 stats.report / pong 事件跳过。
		}
		if got.ID != tc.id {
			t.Errorf("%s: resp id = %q, 期望 %q", tc.method, got.ID, tc.id)
		}
		if got.Method != tc.method {
			t.Errorf("%s: resp method = %q(应原样回显)", tc.method, got.Method)
		}
		if tc.wantErr && got.Error == "" {
			t.Errorf("%s: 期望带 error 的应答", tc.method)
		}
		if !tc.wantErr && got.Error != "" {
			t.Errorf("%s: 意外的错误应答: %s", tc.method, got.Error)
		}
	}
}

func TestUserStateAppliesWithoutRebuildingBox(t *testing.T) {
	h := newHarness(t, nil)
	handshakeAs(t, h.ws)

	writeEnv(t, h.ws, req("u1", MethodUserState,
		UserState{UserStateRevision: 12, Disabled: []string{"alice", "bob"}}))
	resp := readUntil(t, h.ws, MethodUserState)
	if resp.Error != "" {
		t.Fatalf("user.state 应答带错误: %s", resp.Error)
	}

	// 名单进了 tracker:被禁用的用户新连接会被拒(§7.5)。
	waitFor(t, "禁用名单生效", func() bool { return isDisabled(h.tracker, "alice") })
	if isDisabled(h.tracker, "carol") {
		t.Error("carol 不该被禁用 —— user.state 是全量替换,不是并集")
	}

	// 而且落了盘:重启后不必等第一条 user.state 就能挡住。
	waitFor(t, "落盘 user_state_revision", func() bool {
		la, err := state.Load(h.dir)
		return err == nil && la.UserStateRevision == 12 && len(la.Disabled) == 2
	})
	// config_revision 不该被 user.state 碰到 —— 两条通道是独立的(§4.1)。
	la, _ := state.Load(h.dir)
	if la.ConfigRevision != 0 {
		t.Errorf("user.state 动了 config_revision: %d", la.ConfigRevision)
	}
}

// 配置构建失败时:回 error(主控据此**不推进 revision**),且不落盘。
func TestBadConfigIsRejectedAndNotPersisted(t *testing.T) {
	h := newHarness(t, nil)
	handshakeAs(t, h.ws)

	writeEnv(t, h.ws, req("c1", MethodConfigApply, ConfigApply{
		Revision: 77,
		Options:  json.RawMessage(`{"inbounds":[{"type":"no-such-protocol","tag":"x"}]}`),
	}))
	resp := readUntil(t, h.ws, MethodConfigApply)
	if resp.Error == "" {
		t.Fatal("非法配置应当回 error,否则主控会推进到一个根本没生效的 revision")
	}
	la, _ := state.Load(h.dir)
	if la.ConfigRevision != 0 || len(la.Options) != 0 {
		t.Errorf("失败的 config.apply 不该落盘: %+v", la)
	}
}

func TestPeriodicReportsAndHeartbeat(t *testing.T) {
	h := newHarness(t, nil)
	handshakeAs(t, h.ws)

	seen := map[string]bool{}
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) && !(seen[MethodStatsReport] && seen[MethodSysinfoReport] && seen[MethodPong]) {
		env := readEnv(t, h.ws)
		// 上报和心跳都必须是 event(不带 id):主控的 dispatch 先把 resp 交给
		// rpc.resolve,发成 resp 会被当成迟到的应答丢掉。
		if env.Kind != KindEvent {
			continue
		}
		seen[env.Method] = true
		if env.Method == MethodStatsReport {
			var r struct {
				CounterEpoch string          `json:"counter_epoch"`
				Users        json.RawMessage `json:"users"`
			}
			if err := json.Unmarshal(env.Payload, &r); err != nil {
				t.Fatalf("解析 stats.report: %v", err)
			}
			if r.CounterEpoch != "epoch-under-test" {
				t.Errorf("counter_epoch = %q,应是进程级常量", r.CounterEpoch)
			}
			if string(r.Users) == "null" {
				t.Error("users 是 null —— Rust 侧是 Vec,解不出来整条上报会被丢弃")
			}
		}
	}
	for _, m := range []string{MethodStatsReport, MethodSysinfoReport, MethodPong} {
		if !seen[m] {
			t.Errorf("10 秒内没收到 %s", m)
		}
	}
}

// 版本不符必须断开,不做协商、不做兼容层(§4)。
func TestProtocolVersionMismatchDisconnects(t *testing.T) {
	h := newHarness(t, nil)
	handshakeAs(t, h.ws)

	bad := req("v1", MethodBoxStatus, struct{}{})
	bad.V = ProtoVersion + 1
	writeEnv(t, h.ws, bad)

	// agent 应当关闭连接,而不是回一条 resp。
	_ = h.ws.SetReadDeadline(time.Now().Add(5 * time.Second))
	for {
		_, _, err := h.ws.ReadMessage()
		if err != nil {
			return // 期望路径:连接断了
		}
	}
}

// 解不出来的消息只丢这一条,不断开 —— 与主控侧 recv_loop 的宽严尺度一致。
func TestGarbageFrameDoesNotKillTheConnection(t *testing.T) {
	h := newHarness(t, nil)
	handshakeAs(t, h.ws)

	_ = h.ws.SetWriteDeadline(time.Now().Add(5 * time.Second))
	if err := h.ws.WriteMessage(websocket.TextMessage, []byte("{not json")); err != nil {
		t.Fatal(err)
	}
	// 连接还活着:随后的 req 依然能拿到应答。
	writeEnv(t, h.ws, req("g1", MethodBoxStatus, struct{}{}))
	resp := readUntil(t, h.ws, MethodBoxStatus)
	if resp.Error != "" {
		t.Errorf("box.status 应答带错误: %s", resp.Error)
	}
}

// 断线后必须自己重连,否则主控重启一次就得逐台去拉 agent。
func TestReconnectsAfterDisconnect(t *testing.T) {
	h := newHarness(t, nil)
	handshakeAs(t, h.ws)
	_ = h.ws.Close()

	ws2 := h.fm.accept(t)
	handshakeAs(t, ws2)
}

// ─────────────────────────── 小工具 ───────────────────────────

func waitFor(t *testing.T, what string, ok func() bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if ok() {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("等不到:%s", what)
}

// isDisabled 用 tracker 的**实际行为**反推禁用状态:走一遍 RoutedConnection,
// 被禁用的用户拿到的是一条已经关掉的 conn(§7.5)。
// 这样测的是真正会发生的事,也省得为测试在生产代码上开一个访问器。
func isDisabled(tr *tracker.Tracker, user string) bool {
	a, b := net.Pipe()
	defer a.Close()
	defer b.Close()
	go func() { _, _ = io.Copy(io.Discard, b) }()

	out := tr.RoutedConnection(context.Background(), a,
		adapter.InboundContext{User: user, Inbound: "probe-in"}, nil, nil)
	_ = out.SetWriteDeadline(time.Now().Add(time.Second))
	_, err := out.Write([]byte("x"))
	return err != nil
}
