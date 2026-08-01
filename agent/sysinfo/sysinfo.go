// Package sysinfo 采集主机指标,对应 shared/src/proto.rs 的 SysinfoReport(DESIGN.md §7.2)。
//
// 全部走 /proc,不引第三方采集库:要的七个数字都是一行文本,
// 而 gopsutil 那类库会把 cgo、Windows API 和一堆平台分支拖进来。
// 非 Linux 上这些读取会失败,Collect 返回零值而不是报错 —— agent 的部署形态是 Linux,
// 在别的系统上跑只是为了编译和本地调试。
package sysinfo

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Report 是一次采集的结果,字段与 proto.rs 的 SysinfoReport 一一对应。
type Report struct {
	BootID     string  `json:"boot_id"`
	NIC        NIC     `json:"nic"`
	CPUPct     float64 `json:"cpu_pct"`
	MemUsed    int64   `json:"mem_used"`
	MemTotal   int64   `json:"mem_total"`
	Load1      float64 `json:"load1"`
	UptimeSecs int64   `json:"uptime_secs"`
}

// NIC 是全部物理网卡的累计收发字节数。
//
// 上报的是**累计值**而非增量:主控按 boot_id 做 epoch 判断,
// 机器重启(boot_id 变了)时整段当新计数处理,不会把归零当成负增长(§5.2)。
type NIC struct {
	RX int64 `json:"rx"`
	TX int64 `json:"tx"`
}

// nicSkip 是要排除的网卡名前缀(§7.2)。
//
// 排除的都是**不代表出网流量**的口:回环、容器网桥/veth 对、虚拟机网桥、
// 隧道口(含 sing-box 自己的 tun),以及各类 overlay。留下来的才是账单上那条线。
// 判断用前缀而不是全等,因为 veth3a2f1c / docker0 / br-1a2b3c 都是带随机后缀的。
var nicSkip = []string{
	"lo", "docker", "veth", "br-", "virbr", "tun", "tap", "wg",
	"sing-box", "utun", "kube", "cni", "flannel", "tailscale", "zt",
}

func skipNIC(name string) bool {
	for _, p := range nicSkip {
		if strings.HasPrefix(name, p) {
			return true
		}
	}
	return false
}

// Collector 持有跨采样的状态:CPU 占用率必须由两次快照相减得到,
// 单次读 /proc/stat 只能算出**开机以来**的平均值,那个数字没有意义。
type Collector struct {
	mu       sync.Mutex
	lastIdle int64
	lastAll  int64

	ipMu      sync.Mutex
	ipv4      *string
	ipv6      *string
	ipFetched time.Time
	ipTTL     time.Duration
	// v4Endpoints / v6Endpoints 是自探要打的地址,按顺序试,第一个成功的即采用。
	// 留成字段而不是写死,有两个用处:单测把它清空就不碰网络(否则 WS 客户端的
	// 测试会依赖外网可达,在受限的 CI runner 上随机超时);受限网络里的部署
	// 也可以指向自己的 echo 服务。
	v4Endpoints []string
	v6Endpoints []string
}

func NewCollector() *Collector {
	return &Collector{
		v4Endpoints: []string{"https://api4.ipify.org", "https://ifconfig.me/ip"},
		v6Endpoints: []string{"https://api6.ipify.org", "https://ifconfig.me/ip"},
	}
}

// SetEndpoints 覆盖自探地址。传 nil 表示**不探测那一路**,直接报 nil。
func (c *Collector) SetEndpoints(v4, v6 []string) {
	c.ipMu.Lock()
	defer c.ipMu.Unlock()
	c.v4Endpoints = v4
	c.v6Endpoints = v6
	// 换了地址就作废缓存,否则改完还得等 TTL 到期才生效。
	c.ipFetched = time.Time{}
}

// Collect 采一次。任何单项失败都只让那一项留零值,不影响其余字段 ——
// 上报比精确更重要,一台读不到 /proc/loadavg 的机器不该整个从监控里消失。
func (c *Collector) Collect() Report {
	rx, tx := readNetDev()
	memUsed, memTotal := readMemInfo()
	return Report{
		BootID:     BootID(),
		NIC:        NIC{RX: rx, TX: tx},
		CPUPct:     c.cpuPct(),
		MemUsed:    memUsed,
		MemTotal:   memTotal,
		Load1:      readLoad1(),
		UptimeSecs: readUptime(),
	}
}

// ─────────────────────────── /proc 读取 ───────────────────────────

func readNetDev() (rx, tx int64) {
	f, err := os.Open("/proc/net/dev")
	if err != nil {
		return 0, 0
	}
	defer f.Close()
	return parseNetDev(f)
}

// parseNetDev 与 readNetDev 分开,是为了能拿真实的 /proc/net/dev 样本做单元测试 ——
// 这段解析的坑(前导空格、冒号粘连、字段偏移)全在格式上,不在 IO 上。
func parseNetDev(r io.Reader) (rx, tx int64) {
	sc := bufio.NewScanner(r)
	// 前两行是表头(Inter-|Receive... / face |bytes...),不是数据。
	for i := 0; i < 2 && sc.Scan(); i++ {
	}
	for sc.Scan() {
		line := sc.Text()
		// 网卡名和数字之间用冒号分隔,且名字可能带前导空格。
		// 不能直接 Fields 切 —— 名字长的时候冒号会和第一个数字连在一起
		// (内核用 %6s: 对齐,超过 6 字符就没有空格了)。
		colon := strings.IndexByte(line, ':')
		if colon < 0 {
			continue
		}
		name := strings.TrimSpace(line[:colon])
		if skipNIC(name) {
			continue
		}
		fields := strings.Fields(line[colon+1:])
		if len(fields) < 9 {
			continue
		}
		rx += parseInt(fields[0]) // Receive bytes
		tx += parseInt(fields[8]) // Transmit bytes
	}
	return rx, tx
}

func (c *Collector) cpuPct() float64 {
	idle, all := readCPUTimes()
	return c.pctFrom(idle, all)
}

// pctFrom 把一次绝对读数换算成占用率,并把它存成下次的基准。
// 单独拆出来是为了能在测试里喂两拍固定样本 —— 这段的正确性全在减法上。
func (c *Collector) pctFrom(idle, all int64) float64 {
	if all == 0 {
		return 0
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	lastIdle, lastAll := c.lastIdle, c.lastAll
	c.lastIdle, c.lastAll = idle, all
	if lastAll == 0 || all <= lastAll {
		// 第一次采样没有基准;计数器回绕/重启时也走这条路。
		return 0
	}
	dAll := float64(all - lastAll)
	dIdle := float64(idle - lastIdle)
	pct := (1 - dIdle/dAll) * 100
	if pct < 0 {
		return 0
	}
	if pct > 100 {
		return 100
	}
	return pct
}

// readCPUTimes 返回 (空闲时间, 总时间)。
// 空闲含 iowait —— 那段时间 CPU 确实没在算,算成占用会让 IO 密集的机器看着永远满载。
func readCPUTimes() (idle, all int64) {
	f, err := os.Open("/proc/stat")
	if err != nil {
		return 0, 0
	}
	defer f.Close()
	return parseCPUTimes(f)
}

func parseCPUTimes(r io.Reader) (idle, all int64) {
	sc := bufio.NewScanner(r)
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		// 只认汇总行 "cpu",不要 "cpu0"/"cpu1" —— 那些会被重复计入。
		if len(fields) < 5 || fields[0] != "cpu" {
			continue
		}
		for i, v := range fields[1:] {
			n := parseInt(v)
			all += n
			if i == 3 || i == 4 { // idle, iowait
				idle += n
			}
		}
		return idle, all
	}
	return 0, 0
}

func readMemInfo() (used, total int64) {
	f, err := os.Open("/proc/meminfo")
	if err != nil {
		return 0, 0
	}
	defer f.Close()
	return parseMemInfo(f)
}

func parseMemInfo(r io.Reader) (used, total int64) {
	var avail int64
	sc := bufio.NewScanner(r)
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		if len(fields) < 2 {
			continue
		}
		// 单位是 kB,统一换成字节。
		switch fields[0] {
		case "MemTotal:":
			total = parseInt(fields[1]) * 1024
		case "MemAvailable:":
			// 用 MemAvailable 而不是 MemFree:后者把 page cache 算成「已用」,
			// 一台正常干活的机器会常年显示 95% 内存占用。
			avail = parseInt(fields[1]) * 1024
		}
	}
	if total > avail {
		used = total - avail
	}
	return used, total
}

func readLoad1() float64 {
	data, err := os.ReadFile("/proc/loadavg")
	if err != nil {
		return 0
	}
	fields := strings.Fields(string(data))
	if len(fields) == 0 {
		return 0
	}
	v, _ := strconv.ParseFloat(fields[0], 64)
	return v
}

func readUptime() int64 {
	data, err := os.ReadFile("/proc/uptime")
	if err != nil {
		return 0
	}
	fields := strings.Fields(string(data))
	if len(fields) == 0 {
		return 0
	}
	v, _ := strconv.ParseFloat(fields[0], 64)
	return int64(v)
}

func parseInt(s string) int64 {
	v, _ := strconv.ParseInt(s, 10, 64)
	return v
}

// SelfRSS 返回本进程的常驻内存字节数,用于 box.status 的 pid_rss。
//
// 读不到时返回 nil 而不是 0 —— proto.rs 那边是 Option<i64>,
// 「没测到」和「测到 0」是两回事,后者在监控图上是一条误导性的曲线。
func SelfRSS() *int64 {
	data, err := os.ReadFile("/proc/self/statm")
	if err != nil {
		return nil
	}
	fields := strings.Fields(string(data))
	if len(fields) < 2 {
		return nil
	}
	// 第 2 个字段是 resident,单位是**页**不是字节。
	rss := parseInt(fields[1]) * int64(os.Getpagesize())
	return &rss
}

// ─────────────────────────── boot_id ───────────────────────────

var (
	bootOnce sync.Once
	bootID   string
)

// BootID 是本次开机的唯一标识,用作网卡计数器的 epoch(§5.2)。
//
// /proc/sys/kernel/random/boot_id 每次开机变一次,正好是 /proc/net/dev
// 计数器归零的时刻。读不到时退回一个**进程级**随机值:那会让 agent 重启
// 也被当成机器重启,代价是丢一个上报周期的网卡增量,不会算出负数。
func BootID() string {
	bootOnce.Do(func() {
		if data, err := os.ReadFile("/proc/sys/kernel/random/boot_id"); err == nil {
			if s := strings.TrimSpace(string(data)); s != "" {
				bootID = s
				return
			}
		}
		bootID = RandomUUID()
	})
	return bootID
}

// RandomUUID 生成一个 v4 UUID。
//
// 自己拼而不是引 google/uuid:整个 agent 只有两处要它(boot_id 兜底、
// §5.2 的 counter_epoch),不值得为二十行代码多一条依赖。
func RandomUUID() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		// crypto/rand 失败在 Linux 上意味着系统已经不可用了,
		// 但这里返回错误会污染上层所有签名,退回时间戳即可。
		return fmt.Sprintf("%016x-fallback", time.Now().UnixNano())
	}
	b[6] = (b[6] & 0x0f) | 0x40 // version 4
	b[8] = (b[8] & 0x3f) | 0x80 // variant 10
	h := hex.EncodeToString(b[:])
	return h[0:8] + "-" + h[8:12] + "-" + h[12:16] + "-" + h[16:20] + "-" + h[20:32]
}

// ─────────────────────────── 公网地址自探 ───────────────────────────

const (
	ipCacheTTL = 600 * time.Second
	// 探测失败时的负缓存。没有它,一台没有出网权限的机器每次重连都要
	// 白等一轮探测超时;而重连本身就是在网络不好的时候最频繁。
	ipFailTTL = 60 * time.Second
)

// PublicIPs 返回本机的公网 v4/v6 地址,探测失败的那一路返回 nil。
//
// **失败必须是 nil 而不是空串。** 主控的 mark_online 用的是
// `ipv4 = COALESCE(?, ipv4)`(server.rs:487):传 null 保留库里已有的值,
// 传空串会把管理员手工填的地址冲掉,订阅链接随即全部指向空地址。
//
// 调用方要注意:这个方法**会阻塞**,最坏情况是 ctx 的超时。所以它必须在
// 拨号**之前**调用 —— 主控的握手窗口只有 10 秒,从连上那一刻开始算,
// 把探测排在连上之后会让没有出网的节点永远握手超时。
func (c *Collector) PublicIPs(ctx context.Context) (*string, *string) {
	c.ipMu.Lock()
	defer c.ipMu.Unlock()
	if !c.ipFetched.IsZero() && time.Since(c.ipFetched) < c.ipTTL {
		return c.ipv4, c.ipv6
	}
	// 两路并行:串行的话 v6 探测要排在 v4 的超时之后,最坏耗时翻倍。
	var wg sync.WaitGroup
	var v4, v6 *string
	v4eps, v6eps := c.v4Endpoints, c.v6Endpoints
	wg.Add(2)
	go func() {
		defer wg.Done()
		v4 = probe(ctx, "tcp4", v4eps)
	}()
	go func() {
		defer wg.Done()
		v6 = probe(ctx, "tcp6", v6eps)
	}()
	wg.Wait()

	c.ipv4, c.ipv6, c.ipFetched = v4, v6, time.Now()
	if v4 != nil || v6 != nil {
		c.ipTTL = ipCacheTTL
	} else {
		c.ipTTL = ipFailTTL
	}
	return v4, v6
}

// probe 依次试每个 endpoint,第一个成功的即返回。空列表 = 不探测。
// network 固定 tcp4/tcp6,否则 Go 会按系统偏好自己选,v6 那一路可能又走回 v4。
func probe(ctx context.Context, network string, endpoints []string) *string {
	if len(endpoints) == 0 {
		return nil
	}
	dialer := &net.Dialer{Timeout: 3 * time.Second}
	client := &http.Client{
		Timeout: 3 * time.Second,
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, addr string) (net.Conn, error) {
				return dialer.DialContext(ctx, network, addr)
			},
		},
	}
	defer client.CloseIdleConnections()

	for _, url := range endpoints {
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
		if err != nil {
			continue
		}
		resp, err := client.Do(req)
		if err != nil {
			continue
		}
		body, err := io.ReadAll(io.LimitReader(resp.Body, 128))
		resp.Body.Close()
		if err != nil || resp.StatusCode != http.StatusOK {
			continue
		}
		s := strings.TrimSpace(string(body))
		// 校验一遍再信:被劫持的 DNS 会返回一整页 HTML,
		// 那玩意儿写进 agents.ipv4 就成了订阅里的乱码。
		ip := net.ParseIP(s)
		if ip == nil {
			continue
		}
		if (network == "tcp4") != (ip.To4() != nil) {
			continue
		}
		return &s
	}
	return nil
}
