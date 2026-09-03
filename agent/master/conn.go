// Package master 处理与主控的 WebSocket 连接(DESIGN.md §7)。
//
// 一次会话的形状:
//
//	拨号(wss + 指纹校验) → 发 agent.hello(req) → 收 agent.hello_ack(resp)
//	  → 读循环(逐条应答主控的 req) ∥ 定时器(stats/sysinfo 上报 + 心跳)
//	  → 任一侧出错 → 关连接 → 退避重连(1s 起,翻倍到 60s 封顶)
//
// 三条约束决定了下面的写法:
//
//  1. **每一条 req 都必须回一条 resp。** 主控的 catch_up 是**串行**的 ——
//     它用 rpc.call_default 发 config.apply 并阻塞等 resp,拿到之后才发 user.state。
//     漏回一条,主控就会卡到 30 秒 RPC 超时,整个补发流程跟着停。
//  2. **req 在读循环里同步处理,不开 goroutine。** 顺序是语义的一部分:
//     config.apply 必须先于随后的 user.state 生效。并发处理会让禁用名单
//     落在旧 box 上,然后被重建冲掉。
//  3. **信封校验失败 = 断开。** 与 Rust 侧 recv_loop 对称(server.rs:340)。
package master

import (
	"context"
	"crypto/sha256"
	"crypto/subtle"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"

	"github.com/gorilla/websocket"
	C "github.com/sagernet/sing-box/constant"

	"github.com/why1f/sbx/agent/boxctl"
	"github.com/why1f/sbx/agent/config"
	"github.com/why1f/sbx/agent/state"
	"github.com/why1f/sbx/agent/sysinfo"
	"github.com/why1f/sbx/agent/tracker"
)

// AgentVersion 出现在握手里,进而出现在主控的节点列表上。
// 由 `main` 在启动时赋值,值来自 ldflags 注入的 `main.Version`(§11.1):
//
//	go build -ldflags "-X main.Version=1.2.3" ./cmd/sbx-agent
//
// 不写死在源码里 —— 让一个忘了改的常量去代表「线上跑的是哪个版本」比没有版本号更糟。
var AgentVersion = "dev"

const (
	// helloTimeout 对应主控侧的 HELLO_TIMEOUT_SECS。
	helloTimeout = 10 * time.Second
	// 退避区间(§7.3)。上限 60s:主控重启通常在这个量级内完成,
	// 再长就变成「主控好了但节点要等几分钟才回来」。
	backoffMin = 1 * time.Second
	backoffMax = 60 * time.Second

	writeTimeout = 10 * time.Second

	// ipProbeBudget 是公网地址自探的总预算(两路并行)。探不到就报 nil,
	// 主控侧 COALESCE 会保留已有地址 —— 拿不到地址不该拖累上线。
	ipProbeBudget = 5 * time.Second
)

type Conn struct {
	cfg     *config.Config
	tracker *tracker.Tracker
	box     *boxctl.Controller
	sys     *sysinfo.Collector

	// counterEpoch 是 agent **进程**级的 UUID,由 main 生成后传进来。
	// 它不跟 box 重建走 —— 计数器活在 tracker 里,box 重建不清零(§5.2)。
	counterEpoch string

	mu sync.Mutex
	st *state.LastApplied

	wsMu sync.Mutex
	ws   *websocket.Conn

	shutdown     chan struct{}
	shutdownOnce sync.Once
}

func NewConn(
	cfg *config.Config,
	tr *tracker.Tracker,
	bc *boxctl.Controller,
	st *state.LastApplied,
	counterEpoch string,
) *Conn {
	return &Conn{
		cfg:          cfg,
		tracker:      tr,
		box:          bc,
		sys:          sysinfo.NewCollector(),
		counterEpoch: counterEpoch,
		st:           st,
		shutdown:     make(chan struct{}),
	}
}

// Shutdown 在 agent 需要自行退出时关闭(目前只有 agent.upgrade 会触发)。
// main select 它和信号,两条路走同一套收尾逻辑。
func (c *Conn) Shutdown() <-chan struct{} { return c.shutdown }

func (c *Conn) triggerShutdown() {
	c.shutdownOnce.Do(func() { close(c.shutdown) })
}

// Run 是连接主循环:握手 → 收发 → 断线重连。ctx 取消时返回 nil。
func (c *Conn) Run(ctx context.Context) error {
	backoff := backoffMin
	for {
		if ctx.Err() != nil {
			return nil
		}
		handshaked, err := c.session(ctx)
		if ctx.Err() != nil {
			return nil
		}
		if err != nil {
			log.Printf("主控连接断开: %v", err)
		}
		if handshaked {
			// 握手成功过说明地址、token、指纹都是对的,这次断开多半是网络抖动 ——
			// 从最短退避重来。只有连不上或被拒才让退避涨上去,免得一台配错 token 的
			// agent 每秒敲一次主控。
			backoff = backoffMin
		}
		select {
		case <-ctx.Done():
			return nil
		case <-time.After(backoff):
		}
		if backoff < backoffMax {
			backoff *= 2
			if backoff > backoffMax {
				backoff = backoffMax
			}
		}
	}
}

// ─────────────────────────── 一次会话 ───────────────────────────

// session 跑完整的一次连接。第一个返回值表示**是否完成过握手**,只用来决定退避。
func (c *Conn) session(parent context.Context) (bool, error) {
	tlsCfg, err := c.tlsConfig()
	if err != nil {
		return false, err
	}
	// 公网地址探测放在**拨号之前**。主控的握手窗口(HELLO_TIMEOUT_SECS = 10s)
	// 从连上那一刻开始计时,把一次可能超时的外部 HTTP 请求排在连上之后,
	// 会让没有出网权限的节点永远卡在握手超时上 —— 而那种节点恰恰最需要连上来报错。
	probeCtx, cancelProbe := context.WithTimeout(parent, ipProbeBudget)
	v4, v6 := c.sys.PublicIPs(probeCtx)
	cancelProbe()

	dialer := &websocket.Dialer{
		TLSClientConfig:  tlsCfg,
		HandshakeTimeout: helloTimeout,
	}
	dialCtx, cancelDial := context.WithTimeout(parent, helloTimeout+5*time.Second)
	ws, resp, err := dialer.DialContext(dialCtx, c.cfg.Server, nil)
	cancelDial()
	if err != nil {
		if resp != nil {
			return false, fmt.Errorf("拨号 %s: %w(HTTP %d)", c.cfg.Server, err, resp.StatusCode)
		}
		return false, fmt.Errorf("拨号 %s: %w", c.cfg.Server, err)
	}
	defer ws.Close()
	// gorilla 默认不限单帧大小,一整帧会先完整缓冲进内存。主控下发的最大消息是
	// config.apply(几十 KB 量级),8 MiB 绰绰有余;信任锚在钉住的证书上,这一条
	// 只是给「锚不在」(insecure)或主控被攻破的情况兜底,别让 agent 被一帧撑爆。
	ws.SetReadLimit(8 << 20)

	c.wsMu.Lock()
	c.ws = ws
	c.wsMu.Unlock()
	defer func() {
		c.wsMu.Lock()
		c.ws = nil
		c.wsMu.Unlock()
	}()

	ack, err := c.handshake(ws, v4, v6)
	if err != nil {
		return false, err
	}
	log.Printf("已连上主控:agent_id=%d,上报间隔 %ds,心跳 %ds",
		ack.AgentID, ack.ReportIntervalSecs, ack.HeartbeatSecs)

	c.mu.Lock()
	localCfgRev, localUserRev := c.st.ConfigRevision, c.st.UserStateRevision
	c.mu.Unlock()
	if ack.ConfigRevision != localCfgRev || ack.UserStateRevision != localUserRev {
		// 只记日志,不主动拉取:补发是主控的活(server.rs 的 catch_up),
		// 两边都发会变成重复下发。
		log.Printf("revision 与主控不一致(本地 config=%d user=%d,主控 config=%d user=%d),等待补发",
			localCfgRev, localUserRev, ack.ConfigRevision, ack.UserStateRevision)
	}

	ctx, cancel := context.WithCancel(parent)
	defer cancel()

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		c.timers(ctx, ws, ack)
	}()

	err = c.readLoop(ws, ack)
	cancel()
	// 主动关一下,让还堵在 WriteMessage 上的定时器协程立刻拿到错误退出。
	_ = ws.Close()
	wg.Wait()
	return true, err
}

// handshake 发 agent.hello 并等 agent.hello_ack。
//
// hello 是 **req**(带 id),主控用同一个 id 回 resp(server.rs:114)。
// 被拒时主控回的 id 是字面量 "0" 而不是我们的 id —— 那是故意的(不给枚举线索),
// 所以这里认 method 而不是认 id。
func (c *Conn) handshake(ws *websocket.Conn, v4, v6 *string) (*AgentHelloAck, error) {
	hostname, _ := os.Hostname()

	c.mu.Lock()
	hello := AgentHello{
		Token:             c.cfg.Token,
		AgentVersion:      AgentVersion,
		ProtoVersion:      ProtoVersion,
		OS:                runtime.GOOS,
		Arch:              runtime.GOARCH,
		Hostname:          hostname,
		BootID:            sysinfo.BootID(),
		SingboxVersion:    C.Version,
		ConfigRevision:    c.st.ConfigRevision,
		UserStateRevision: c.st.UserStateRevision,
		IPv4:              v4,
		IPv6:              v6,
		UTCOffsetSecs:     sysinfo.UTCOffsetSecs(),
	}
	c.mu.Unlock()

	if err := c.send(req("1", MethodAgentHello, hello)); err != nil {
		return nil, fmt.Errorf("发 agent.hello: %w", err)
	}

	_ = ws.SetReadDeadline(time.Now().Add(helloTimeout))
	for {
		mt, data, err := ws.ReadMessage()
		if err != nil {
			return nil, fmt.Errorf("等 agent.hello_ack: %w", err)
		}
		if mt != websocket.TextMessage {
			continue
		}
		var env Envelope
		if err := json.Unmarshal(data, &env); err != nil {
			return nil, fmt.Errorf("握手应答不是合法信封: %w", err)
		}
		if err := env.validate(); err != nil {
			return nil, fmt.Errorf("握手应答校验失败: %w", err)
		}
		if env.Method != MethodAgentHelloAck {
			continue
		}
		if env.Error != "" {
			// 主控不会说明拒绝原因(避免 token 枚举),这里也只能原样转述。
			return nil, fmt.Errorf("主控拒绝握手: %s", env.Error)
		}
		var ack AgentHelloAck
		if err := json.Unmarshal(env.Payload, &ack); err != nil {
			return nil, fmt.Errorf("解析 agent.hello_ack: %w", err)
		}
		if ack.HeartbeatSecs == 0 {
			ack.HeartbeatSecs = 10
		}
		if ack.ReportIntervalSecs == 0 {
			ack.ReportIntervalSecs = 30
		}
		return &ack, nil
	}
}

// readLoop 收消息直到出错。返回的错误就是断开原因。
func (c *Conn) readLoop(ws *websocket.Conn, ack *AgentHelloAck) error {
	// 读超时取心跳的 3 倍:一次丢包不该断线,但半开连接必须在一分钟内被发现。
	// 刷新点有两个 —— 收到任何帧,以及收到 WS 层的 pong(见 timers 里的 ping)。
	idle := 3 * time.Duration(ack.HeartbeatSecs) * time.Second
	if idle < 30*time.Second {
		idle = 30 * time.Second
	}
	_ = ws.SetReadDeadline(time.Now().Add(idle))
	ws.SetPongHandler(func(string) error {
		return ws.SetReadDeadline(time.Now().Add(idle))
	})

	for {
		mt, data, err := ws.ReadMessage()
		if err != nil {
			return err
		}
		_ = ws.SetReadDeadline(time.Now().Add(idle))
		if mt != websocket.TextMessage {
			continue
		}
		var env Envelope
		if err := json.Unmarshal(data, &env); err != nil {
			// 解不出信封只丢这一条 —— 与主控侧 recv_loop 的处理对称:
			// 一条坏消息不该毒死整条连接。
			log.Printf("收到无法解析的消息,已忽略: %v", err)
			continue
		}
		if err := env.validate(); err != nil {
			// 但**结构性非法**必须断开(§4)。
			return fmt.Errorf("信封校验失败: %w", err)
		}
		switch env.Kind {
		case KindReq:
			c.handleReq(&env)
		case KindResp, KindEvent:
			// agent 侧除握手外不发 req,也不订阅事件。收到就忽略,不断开 ——
			// 与主控对未知 method 的处理保持同一个宽严尺度。
		}
		_ = ws.SetReadDeadline(time.Now().Add(idle))
	}
}

// timers 负责周期性上报和心跳。
func (c *Conn) timers(ctx context.Context, ws *websocket.Conn, ack *AgentHelloAck) {
	report := time.NewTicker(time.Duration(ack.ReportIntervalSecs) * time.Second)
	defer report.Stop()
	beat := time.NewTicker(time.Duration(ack.HeartbeatSecs) * time.Second)
	defer beat.Stop()

	// 先采一次 CPU 做基准:cpu_pct 是两次采样相减出来的,
	// 不预热的话第一条上报永远是 0。
	c.sys.Collect()

	for {
		select {
		case <-ctx.Done():
			return
		case <-report.C:
			if err := c.sendReports(); err != nil {
				log.Printf("上报失败: %v", err)
				return
			}
		case <-beat.C:
			// 应用层心跳:主控收到 pong 就更新 agents.last_seen(server.rs 的 dispatch)。
			// **是 agent 主动发,不是应答主控的 ping** —— 主控侧没有 ping 发送方。
			if err := c.send(event(MethodPong, Pong{EchoTS: time.Now().Unix()})); err != nil {
				log.Printf("心跳失败: %v", err)
				return
			}
			// 同时打一个 WS 层 ping,用它的 pong 刷新读超时:
			// 应用层心跳是单向的,只有它探不出「主控还在听吗」。
			c.wsMu.Lock()
			err := ws.WriteControl(websocket.PingMessage, nil, time.Now().Add(writeTimeout))
			c.wsMu.Unlock()
			if err != nil {
				log.Printf("WS ping 失败: %v", err)
				return
			}
		}
	}
}

func (c *Conn) sendReports() error {
	if err := c.send(event(MethodStatsReport, StatsReport{
		CounterEpoch: c.counterEpoch,
		Users:        c.tracker.Snapshot(),
	})); err != nil {
		return err
	}
	return c.send(event(MethodSysinfoReport, c.sys.Collect()))
}

// ─────────────────────────── req 分发 ───────────────────────────

func (c *Conn) handleReq(env *Envelope) {
	payload, err := c.invoke(env)
	if err != nil {
		log.Printf("处理 %s 失败: %v", env.Method, err)
		if sendErr := c.send(respErr(env.ID, env.Method, err.Error())); sendErr != nil {
			log.Printf("回错误应答失败: %v", sendErr)
		}
		return
	}
	if err := c.send(respOK(env.ID, env.Method, payload)); err != nil {
		log.Printf("回应答失败: %v", err)
	}
}

// okPayload 对应 proto.rs 的 `Ok {}`。
type okPayload struct{}

func (c *Conn) invoke(env *Envelope) (any, error) {
	switch env.Method {
	case MethodConfigApply:
		var p ConfigApply
		if err := json.Unmarshal(env.Payload, &p); err != nil {
			return nil, fmt.Errorf("解析 config.apply: %w", err)
		}
		return c.applyConfig(p)

	case MethodConfigCheck:
		var p ConfigCheck
		if err := json.Unmarshal(env.Payload, &p); err != nil {
			return nil, fmt.Errorf("解析 config.check: %w", err)
		}
		if err := c.box.Check(p.Options); err != nil {
			return nil, err
		}
		return okPayload{}, nil

	case MethodUserState:
		var p UserState
		if err := json.Unmarshal(env.Payload, &p); err != nil {
			return nil, fmt.Errorf("解析 user.state: %w", err)
		}
		return c.applyUserState(p)

	case MethodBoxRestart:
		if err := c.box.Restart(); err != nil {
			c.emitBoxEvent("failed", err.Error())
			return nil, err
		}
		c.emitBoxEvent("running", "restart")
		return okPayload{}, nil

	case MethodBoxStatus:
		running, since := c.box.Running()
		st := BoxStatus{Running: running, PidRSS: sysinfo.SelfRSS()}
		if running {
			st.Since = &since
		}
		return st, nil

	case MethodStatsPull:
		return StatsReport{CounterEpoch: c.counterEpoch, Users: c.tracker.Snapshot()}, nil

	case MethodSysinfoPull:
		return c.sys.Collect(), nil

	case MethodAgentUpgrade:
		var p AgentUpgrade
		if err := json.Unmarshal(env.Payload, &p); err != nil {
			return nil, fmt.Errorf("解析 agent.upgrade: %w", err)
		}
		if err := c.upgrade(p); err != nil {
			return nil, err
		}
		// 应答先发出去,再退出 —— 反过来的话主控看到的是连接断开而不是成功,
		// 会把一次正常升级记成一次失败。
		go func() {
			time.Sleep(500 * time.Millisecond)
			log.Println("升级完成,退出等待 supervisor 拉起新版本")
			c.triggerShutdown()
		}()
		return okPayload{}, nil

	case MethodPing:
		// 主控目前没有 ping 的发送方,这里是照协议留的应答面。
		// echo_ts 若能从 payload 里取到就原样回,取不到用本地时间。
		var p struct {
			TS     int64 `json:"ts"`
			EchoTS int64 `json:"echo_ts"`
		}
		_ = json.Unmarshal(env.Payload, &p)
		ts := p.TS
		if ts == 0 {
			ts = p.EchoTS
		}
		if ts == 0 {
			ts = time.Now().Unix()
		}
		return Pong{EchoTS: ts}, nil

	default:
		return nil, fmt.Errorf("未知 method: %s", env.Method)
	}
}

func (c *Conn) applyConfig(p ConfigApply) (any, error) {
	if err := c.box.Apply(p.Options); err != nil {
		c.emitBoxEvent("failed", err.Error())
		// 主控收到 error 就不推进 revision(§4.1),下次会重发同一份配置。
		return nil, err
	}
	c.mu.Lock()
	c.st.ConfigRevision = p.Revision
	c.st.Options = p.Options
	snap := *c.st
	c.mu.Unlock()

	// 落盘失败**不回错误**:box 已经在跑新配置了,回错误会让主控保留旧 revision
	// 并立刻重发,于是磁盘满的机器会陷入「重建 box → 落盘失败 → 重发」的循环。
	// 报 ok 的代价只是重启后按旧 revision 握手、主控补发一次 —— 自愈的那条路。
	if err := state.Save(c.cfg.StateDir, &snap); err != nil {
		log.Printf("警告:last-applied.json 落盘失败(配置已生效,重启后会退回旧配置): %v", err)
	}
	c.emitBoxEvent("running", fmt.Sprintf("config revision %d", p.Revision))
	return okPayload{}, nil
}

func (c *Conn) applyUserState(p UserState) (any, error) {
	// **不重建 box。** 只换 tracker 里的禁用名单,已建立的连接跑完为止(§7.5)。
	c.tracker.SetDisabled(p.Disabled)

	c.mu.Lock()
	c.st.UserStateRevision = p.UserStateRevision
	c.st.Disabled = p.Disabled
	snap := *c.st
	c.mu.Unlock()

	if err := state.Save(c.cfg.StateDir, &snap); err != nil {
		log.Printf("警告:last-applied.json 落盘失败(禁用名单已生效): %v", err)
	}
	return okPayload{}, nil
}

func (c *Conn) emitBoxEvent(stateName, message string) {
	if err := c.send(event(MethodBoxEvent, BoxEvent{State: stateName, Message: message})); err != nil {
		log.Printf("发 box.event 失败: %v", err)
	}
}

// ─────────────────────────── 收发 ───────────────────────────

func req(id, method string, payload any) *Envelope {
	return &Envelope{V: ProtoVersion, ID: id, Kind: KindReq, Method: method, Payload: mustJSON(payload)}
}

func respOK(id, method string, payload any) *Envelope {
	return &Envelope{V: ProtoVersion, ID: id, Kind: KindResp, Method: method, Payload: mustJSON(payload)}
}

func respErr(id, method, msg string) *Envelope {
	return &Envelope{V: ProtoVersion, ID: id, Kind: KindResp, Method: method,
		Payload: json.RawMessage("null"), Error: msg}
}

func event(method string, payload any) *Envelope {
	return &Envelope{V: ProtoVersion, Kind: KindEvent, Method: method, Payload: mustJSON(payload)}
}

// mustJSON 把 payload 编成 RawMessage。编码失败只可能是我们自己传了不可序列化的值
// (chan、func、循环引用),那是编码 bug 而不是运行时状况 —— 退化成 `{}` 让消息还能发出去,
// 比整条连接卡住强。
func mustJSON(v any) json.RawMessage {
	if v == nil {
		return json.RawMessage("{}")
	}
	if raw, ok := v.(json.RawMessage); ok {
		return raw
	}
	data, err := json.Marshal(v)
	if err != nil {
		log.Printf("内部错误:payload 无法序列化: %v", err)
		return json.RawMessage("{}")
	}
	return data
}

func (c *Conn) send(env *Envelope) error {
	data, err := json.Marshal(env)
	if err != nil {
		return err
	}
	c.wsMu.Lock()
	defer c.wsMu.Unlock()
	if c.ws == nil {
		return errors.New("尚未连接到主控")
	}
	_ = c.ws.SetWriteDeadline(time.Now().Add(writeTimeout))
	return c.ws.WriteMessage(websocket.TextMessage, data)
}

// ─────────────────────────── TLS 指纹钉扎(§1.3)───────────────────────────

// tlsConfig 构造只认指纹的 TLS 配置。
//
// **信任锚定在密钥而非名字上**:主控用的是自签证书,没有 CA 可验,SAN/CN 也没意义
// (IP 会变、域名可能压根没有)。所以关掉标准校验,换成对**叶子证书 DER 的
// SHA-256** 做常量时间比较 —— 与 master/src/tls.rs 生成指纹的算法逐字节一致,
// 也与 `openssl x509 -fingerprint -sha256` 的结果一致。
//
// InsecureSkipVerify: true 在这里不是「不校验」,而是「换一种校验」。
// 少了它,自签证书会先在标准链校验那一步就被拒,根本走不到 VerifyPeerCertificate。
func (c *Conn) tlsConfig() (*tls.Config, error) {
	if !strings.HasPrefix(c.cfg.Server, "wss://") {
		// 明文 ws://,gorilla 会忽略 TLS 配置。说一声:token 在这条连接上没有任何保护。
		log.Println("警告:server 是明文 ws://,token 与配置在网络上不加密,也不校验主控身份")
		return nil, nil
	}
	if c.cfg.Insecure {
		// 仅开发用。生产上没有指纹就该拒绝启动,而不是悄悄降级成不校验。
		log.Println("警告:insecure = true,不校验主控证书指纹")
		return &tls.Config{InsecureSkipVerify: true}, nil
	}
	want, err := parseFingerprint(c.cfg.Fingerprint)
	if err != nil {
		return nil, err
	}
	return &tls.Config{
		InsecureSkipVerify: true,
		VerifyPeerCertificate: func(rawCerts [][]byte, _ [][]*x509.Certificate) error {
			if len(rawCerts) == 0 {
				return errors.New("主控没有出示证书")
			}
			got := sha256.Sum256(rawCerts[0])
			if subtle.ConstantTimeCompare(got[:], want) != 1 {
				return fmt.Errorf("主控证书指纹不符:期望 %s,实际 sha256:%s",
					c.cfg.Fingerprint, hex.EncodeToString(got[:]))
			}
			return nil
		},
	}, nil
}

func parseFingerprint(fp string) ([]byte, error) {
	s := strings.TrimSpace(strings.ToLower(fp))
	if s == "" {
		return nil, errors.New("配置缺少 fingerprint(主控 `sbx cert show` 可以拿到)")
	}
	s = strings.TrimPrefix(s, "sha256:")
	// 允许粘贴 openssl 那种带冒号的写法。
	s = strings.ReplaceAll(s, ":", "")
	raw, err := hex.DecodeString(s)
	if err != nil {
		return nil, fmt.Errorf("fingerprint 不是合法的十六进制: %w", err)
	}
	if len(raw) != sha256.Size {
		return nil, fmt.Errorf("fingerprint 长度应为 %d 字节,实际 %d", sha256.Size, len(raw))
	}
	return raw, nil
}

// ─────────────────────────── 自升级(§11.2)───────────────────────────

// upgrade 下载新二进制、校验 sha256、原子替换当前可执行文件。
//
// 替换成功后**不 exec 新进程**,而是退出让 systemd 把它拉起来:
// 自己 exec 要处理已打开的 fd、信号处理器、以及「新版本起不来时谁来兜底」,
// 而 supervisor 天生就干这个。Restart=always 是部署的前提(§11.2)。
func (c *Conn) upgrade(p AgentUpgrade) error {
	exe, err := os.Executable()
	if err != nil {
		return fmt.Errorf("定位当前可执行文件: %w", err)
	}
	return replaceExecutable(resolveExecutable(exe), p.URL, p.SHA256)
}

// resolveExecutable 解一次符号链接:很多部署把 /usr/local/bin/sbx-agent 指向
// 带版本号的实际文件,覆盖软链本身会把版本管理搞乱 —— 目录里会多出一个普通文件,
// 而下次 systemd 起的还是同一个路径,从外面看不出区别。
// 解不开(不是软链、或者断链)就原样返回,交给后面的 rename 去报错。
func resolveExecutable(exe string) string {
	if resolved, err := filepath.EvalSymlinks(exe); err == nil {
		return resolved
	}
	return exe
}

// replaceExecutable 把 exe 换成 url 指向的内容,前提是下载下来的 sha256 等于 wantHex。
//
// 从 upgrade 里拆出来是为了**能测**:upgrade 拿的是 os.Executable(),
// 在 go test 里那就是测试二进制自己 —— 覆盖它验证不了任何东西,
// 只会把正在跑的这轮测试搞坏。拆开之后 exe 是个参数,测试传临时目录里的假文件。
func replaceExecutable(exe, url, wantHex string) error {
	want, err := hex.DecodeString(strings.ToLower(strings.TrimSpace(wantHex)))
	if err != nil || len(want) != sha256.Size {
		return fmt.Errorf("agent.upgrade 的 sha256 非法")
	}

	// 临时文件必须和目标**同目录** —— 跨文件系统 rename 不是原子的。
	tmp, err := os.CreateTemp(filepath.Dir(exe), ".sbx-agent.new-*")
	if err != nil {
		return fmt.Errorf("创建临时文件: %w", err)
	}
	tmpName := tmp.Name()
	defer os.Remove(tmpName) // rename 成功后是 no-op

	client := &http.Client{Timeout: 10 * time.Minute}
	resp, err := client.Get(url)
	if err != nil {
		tmp.Close()
		return fmt.Errorf("下载 %s: %w", url, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		tmp.Close()
		return fmt.Errorf("下载 %s: HTTP %d", url, resp.StatusCode)
	}

	h := sha256.New()
	if _, err := io.Copy(io.MultiWriter(tmp, h), resp.Body); err != nil {
		tmp.Close()
		return fmt.Errorf("写入下载内容: %w", err)
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	// 先校验再赋予可执行位:顺序反过来的话,一个被中途截断的文件会有一瞬间
	// 是可执行的,而 rename 之后就来不及了。
	if subtle.ConstantTimeCompare(h.Sum(nil), want) != 1 {
		return fmt.Errorf("下载内容 sha256 不符,已丢弃")
	}
	if err := os.Chmod(tmpName, 0o755); err != nil {
		return err
	}
	// Linux 上 rename 覆盖**正在运行**的可执行文件是允许的:
	// 老的 inode 会一直活到本进程退出(ETXTBSY 只挡以写方式 open,不挡 rename)。
	if err := os.Rename(tmpName, exe); err != nil {
		return fmt.Errorf("替换 %s: %w", exe, err)
	}
	return nil
}
