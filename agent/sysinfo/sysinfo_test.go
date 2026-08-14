package sysinfo

import (
	"context"
	"net"
	"strings"
	"testing"
	"time"
)

// 真实的 /proc/net/dev 样本。三处刻意保留了原始格式:
// 表头两行、名字右对齐带前导空格、以及一个 8 字符网卡名(冒号紧贴数字)。
const netDevSample = `Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 9999999   1000    0    0    0     0          0         0  8888888    900    0    0    0     0       0          0
  eth0: 1000000    500    0    0    0     0          0         0   200000    300    0    0    0     0       0          0
  eth1:  500000    250    0    0    0     0          0         0   100000    150    0    0    0     0       0          0
docker0: 7777777    700    0    0    0     0          0         0  6666666    600    0    0    0     0       0          0
veth9a1b2c: 5555555 500   0    0    0     0          0         0  4444444    400    0    0    0     0       0          0
`

func TestParseNetDevSumsOnlyPhysicalNICs(t *testing.T) {
	rx, tx := parseNetDev(strings.NewReader(netDevSample))
	// 只有 eth0 + eth1 该被计入;lo / docker0 / veth* 都在黑名单里。
	if want := int64(1500000); rx != want {
		t.Errorf("rx = %d, 期望 %d(eth0+eth1,不含 lo/docker0/veth)", rx, want)
	}
	if want := int64(300000); tx != want {
		t.Errorf("tx = %d, 期望 %d", tx, want)
	}
}

// docker0 那一行没有前导空格,冒号直接跟在名字后面 —— 用 Fields 先切会把
// "docker0:" 当成第一个字段,名字匹配不上黑名单,于是容器网桥的流量被算进账单。
func TestParseNetDevHandlesLongNameWithoutLeadingSpace(t *testing.T) {
	line := "docker0: 7777777 700 0 0 0 0 0 0 6666666 600 0 0 0 0 0 0\n"
	rx, tx := parseNetDev(strings.NewReader("h1\nh2\n" + line))
	if rx != 0 || tx != 0 {
		t.Errorf("docker0 应被跳过,却得到 rx=%d tx=%d", rx, tx)
	}
}

func TestSkipNIC(t *testing.T) {
	skip := []string{"lo", "docker0", "veth9a1b", "br-1a2b3c", "virbr0", "tun0", "wg0", "tailscale0"}
	keep := []string{"eth0", "ens18", "enp3s0", "wlan0", "bond0"}
	for _, n := range skip {
		if !skipNIC(n) {
			t.Errorf("%s 应被跳过", n)
		}
	}
	for _, n := range keep {
		if skipNIC(n) {
			t.Errorf("%s 不该被跳过 —— 跳掉物理网卡等于整机流量归零", n)
		}
	}
}

const statSample = `cpu  100 20 30 800 50 0 0 0 0 0
cpu0 50 10 15 400 25 0 0 0 0 0
intr 12345
`

func TestParseCPUTimesUsesAggregateLineOnly(t *testing.T) {
	idle, all := parseCPUTimes(strings.NewReader(statSample))
	// idle = idle(800) + iowait(50)
	if idle != 850 {
		t.Errorf("idle = %d, 期望 850(含 iowait)", idle)
	}
	// all = 100+20+30+800+50 = 1000;cpu0 那行不能重复计入
	if all != 1000 {
		t.Errorf("all = %d, 期望 1000(只算 \"cpu\" 汇总行)", all)
	}
}

func TestCPUPctNeedsTwoSamples(t *testing.T) {
	c := NewCollector()
	// 第一次没有基准,必须是 0 而不是「开机以来的平均值」。
	if got := c.pctFrom(0, 0); got != 0 {
		t.Errorf("首次采样应为 0,得到 %v", got)
	}
	c.lastIdle, c.lastAll = 850, 1000
	// 下一拍总时间 +100,其中空闲 +25 → 占用 75%
	if got := c.pctFrom(875, 1100); got != 75 {
		t.Errorf("cpu_pct = %v, 期望 75", got)
	}
}

const memInfoSample = `MemTotal:       16384000 kB
MemFree:          512000 kB
MemAvailable:    8192000 kB
Buffers:          100000 kB
`

func TestParseMemInfoUsesAvailableNotFree(t *testing.T) {
	used, total := parseMemInfo(strings.NewReader(memInfoSample))
	if want := int64(16384000) * 1024; total != want {
		t.Errorf("total = %d, 期望 %d", total, want)
	}
	// 用 MemAvailable:16384000-8192000 = 8192000 kB。
	// 若误用 MemFree 会得到 15872000 kB —— 一台正常机器立刻显示 97% 内存占用。
	if want := int64(8192000) * 1024; used != want {
		t.Errorf("used = %d, 期望 %d(应基于 MemAvailable)", used, want)
	}
}

func TestRandomUUIDShape(t *testing.T) {
	seen := make(map[string]bool)
	for i := 0; i < 100; i++ {
		u := RandomUUID()
		if len(u) != 36 || u[8] != '-' || u[13] != '-' || u[18] != '-' || u[23] != '-' {
			t.Fatalf("不是 UUID 形状: %q", u)
		}
		if u[14] != '4' {
			t.Fatalf("version nibble 应为 4: %q", u)
		}
		if !strings.ContainsRune("89ab", rune(u[19])) {
			t.Fatalf("variant nibble 应为 8/9/a/b: %q", u)
		}
		if seen[u] {
			t.Fatalf("生成了重复的 UUID: %q —— counter_epoch 撞了会让主控错算增量", u)
		}
		seen[u] = true
	}
}

// 关掉自探地址后**不能碰网络**,而且要立刻返回。
//
// 这不只是测试便利:WS 客户端在拨号前调它(主控的握手窗口只有 10 秒),
// 探测在受限网络里会拖满预算。这个开关是那条路径的兜底。
func TestPublicIPsSkipsProbingWhenEndpointsAreEmpty(t *testing.T) {
	c := NewCollector()
	c.SetEndpoints(nil, nil)

	start := time.Now()
	v4, v6 := c.PublicIPs(context.Background())
	elapsed := time.Since(start)

	if v4 != nil || v6 != nil {
		t.Errorf("不该探到地址: v4=%v v6=%v", v4, v6)
	}
	// 真去打网络最少也要几十毫秒;这里应当是纯内存操作。
	if elapsed > 100*time.Millisecond {
		t.Errorf("耗时 %v,看起来仍在访问网络", elapsed)
	}
}

// 探测失败要用**负缓存**,不能每次重连都重试一轮 ——
// 而重连恰恰在网络最差的时候最频繁。
func TestFailedProbeIsNegativelyCached(t *testing.T) {
	c := NewCollector()
	c.SetEndpoints(nil, nil)
	c.PublicIPs(context.Background())

	c.ipMu.Lock()
	ttl := c.ipTTL
	fetched := c.ipFetched
	c.ipMu.Unlock()

	if ttl != ipFailTTL {
		t.Errorf("失败后的 TTL = %v,期望 %v(短负缓存)", ttl, ipFailTTL)
	}
	if fetched.IsZero() {
		t.Error("失败也要记时间戳,否则下次调用又会重试一轮")
	}
}

// isGloballyRoutable 挡掉的每一类都有具体后果:这些地址写进 agents.ipv4/ipv6
// 就成了订阅链接里的死地址,而主控那边 COALESCE 会用它冲掉管理员手填的正确值。
func TestOnlyGloballyRoutableAddressesAreReported(t *testing.T) {
	cases := []struct {
		ip   string
		want bool
		why  string
	}{
		// 这台机器实际的两个地址,都该算数。
		{"2600:1700:3a90:c620:be24:11ff:febb:5ad5", true, "SLAAC 出来的全球 v6"},
		{"2a06:9801:1e::1c1", true, "另一段的全球 v6"},
		{"76.9.111.80", true, "公网 v4"},

		{"fe80::be24:11ff:febb:5ad5", false, "链路本地只在同网段有意义"},
		{"fc00::1", false, "ULA 是 v6 的内网段"},
		{"fd12:3456::1", false, "ULA 的另一半(fd00::/8)"},
		{"::1", false, "回环"},
		{"::", false, "未指定"},
		{"ff02::1", false, "组播"},

		{"10.0.0.5", false, "RFC1918"},
		{"172.16.0.1", false, "RFC1918 下界"},
		{"172.31.255.254", false, "RFC1918 上界"},
		{"172.32.0.1", true, "172.32 已经出了 RFC1918"},
		{"192.168.1.1", false, "RFC1918"},
		{"100.64.0.1", false, "CGNAT:看着像公网,外面连不进来"},
		{"100.127.255.255", false, "CGNAT 上界"},
		{"100.128.0.1", true, "100.128 已经出了 CGNAT"},
		{"127.0.0.1", false, "回环"},
		{"169.254.1.1", false, "v4 链路本地"},
	}
	for _, c := range cases {
		ip := net.ParseIP(c.ip)
		if ip == nil {
			t.Fatalf("测试用例里的地址解析不了: %s", c.ip)
		}
		if got := isGloballyRoutable(ip); got != c.want {
			t.Errorf("isGloballyRoutable(%s) = %v, 想要 %v —— %s", c.ip, got, c.want, c.why)
		}
	}
}

// 本机源地址那条路**不发包也不依赖外部服务**,所以它必须是瞬时的。
//
// 这正是它存在的理由:原先只有 HTTP 探测一条路,超时 3 秒,碰上响应慢的
// endpoint 就两个连着超时 —— 一台明明有全球 IPv6 的机器界面上一直是空的。
func TestLocalSourceIPDoesNotBlock(t *testing.T) {
	start := time.Now()
	for _, network := range []string{"udp4", "udp6"} {
		got := localSourceIP(network)
		// 返回什么取决于跑测试的这台机器有没有对应的默认路由,两种都合法;
		// 但**只要返回了就必须是全球可达地址**。
		if got != nil {
			ip := net.ParseIP(*got)
			if ip == nil {
				t.Fatalf("%s 返回了解析不了的地址: %q", network, *got)
			}
			if !isGloballyRoutable(ip) {
				t.Errorf("%s 返回了不该报的地址 %s", network, *got)
			}
		}
	}
	// 两路加起来远不该到一秒 —— 它只做路由查找,不发包。
	if d := time.Since(start); d > time.Second {
		t.Errorf("本机源地址查询耗时 %v,它不该阻塞", d)
	}
}
