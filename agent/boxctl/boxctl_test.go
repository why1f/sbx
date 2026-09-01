package boxctl

import (
	"encoding/json"
	"fmt"
	"net"
	"testing"
	"time"

	"github.com/why1f/sbx/agent/tracker"
)

// 这组测试跑的是**真的 sing-box 实例**。它守的是 §7.4 的热重载顺序 ——
// 那个顺序的正确性没法靠读代码确认:新旧两个 box 监听同一组端口,
// 「先起新的再停旧的」在纸面上更安全,实际会直接 EADDRINUSE。
//
// 与 spike/ 的分工:spike 验的是**流量记到没记到**(需要真实代理链路),
// 这里验的是**实例切换与回滚**(只需要端口)。

func freePort(t *testing.T) int {
	t.Helper()
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port
}

func configOn(port int) json.RawMessage {
	return json.RawMessage(fmt.Sprintf(`{
		"log": {"level": "error"},
		"inbounds": [{"type": "mixed", "tag": "socks-in",
		              "listen": "127.0.0.1", "listen_port": %d}],
		"outbounds": [{"type": "direct", "tag": "direct"}]
	}`, port))
}

func canConnect(port int) bool {
	c, err := net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", port), time.Second)
	if err != nil {
		return false
	}
	_ = c.Close()
	return true
}

func waitPort(t *testing.T, port int, want bool) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if canConnect(port) == want {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("端口 %d 期望 %s,超时", port, map[bool]string{true: "可连", false: "不可连"}[want])
}

func TestApplyStartsAndStops(t *testing.T) {
	port := freePort(t)
	c := New(tracker.New())
	t.Cleanup(func() { _ = c.Close() })

	if running, _ := c.Running(); running {
		t.Error("还没 Apply 就报在跑")
	}
	if err := c.Apply(configOn(port)); err != nil {
		t.Fatalf("Apply: %v", err)
	}
	running, since := c.Running()
	if !running || since == 0 {
		t.Errorf("Running() = (%v, %d)", running, since)
	}
	waitPort(t, port, true)

	if err := c.Close(); err != nil {
		t.Errorf("Close: %v", err)
	}
	if running, _ := c.Running(); running {
		t.Error("Close 之后仍报在跑")
	}
	waitPort(t, port, false)
}

// 热重载到**同一个端口**。这是最常见的一次改配置(改用户、改证书),
// 也是「先起后停」会立刻失败的那个场景。
func TestHotReloadOnSamePort(t *testing.T) {
	port := freePort(t)
	c := New(tracker.New())
	t.Cleanup(func() { _ = c.Close() })

	if err := c.Apply(configOn(port)); err != nil {
		t.Fatalf("首次 Apply: %v", err)
	}
	waitPort(t, port, true)

	for i := 0; i < 3; i++ {
		if err := c.Apply(configOn(port)); err != nil {
			t.Fatalf("第 %d 次热重载失败(端口没被旧实例先释放?): %v", i+1, err)
		}
	}
	waitPort(t, port, true)
}

// 配置本身构建不出来时,旧实例**完全不受影响** —— 这一步在 box.New 就失败了,
// 根本走不到 Close 旧实例那一行。改错一个字段不该让节点下线。
func TestBadConfigLeavesOldInstanceRunning(t *testing.T) {
	port := freePort(t)
	c := New(tracker.New())
	t.Cleanup(func() { _ = c.Close() })

	if err := c.Apply(configOn(port)); err != nil {
		t.Fatalf("首次 Apply: %v", err)
	}
	waitPort(t, port, true)

	bad := json.RawMessage(`{"inbounds":[{"type":"no-such-protocol","tag":"x"}]}`)
	if err := c.Apply(bad); err == nil {
		t.Fatal("非法配置应当报错")
	}
	if running, _ := c.Running(); !running {
		t.Error("配置构建失败后旧实例应继续在跑")
	}
	waitPort(t, port, true)
}

// 新配置能构建但**起不来**(端口被别人占了)时,必须回滚到旧配置。
// 这条路径最容易写错:此时旧实例已经被 Close 了,不主动重建就是彻底下线。
func TestStartFailureRollsBackToOldConfig(t *testing.T) {
	oldPort := freePort(t)
	c := New(tracker.New())
	t.Cleanup(func() { _ = c.Close() })

	if err := c.Apply(configOn(oldPort)); err != nil {
		t.Fatalf("首次 Apply: %v", err)
	}
	waitPort(t, oldPort, true)

	// 占住一个端口,让新配置在 Start 阶段(而不是 New 阶段)失败。
	squatted, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer squatted.Close()
	takenPort := squatted.Addr().(*net.TCPAddr).Port

	err = c.Apply(configOn(takenPort))
	if err == nil {
		t.Fatal("端口被占时 Apply 应当报错")
	}
	if running, _ := c.Running(); !running {
		t.Fatal("Start 失败后应回滚到旧配置并保持在跑")
	}
	// 回滚回的是**旧端口**,不是新端口。
	waitPort(t, oldPort, true)
}

// Check 只验配置,不接管当前实例,也不占端口。
func TestCheckDoesNotTouchRunningInstance(t *testing.T) {
	port := freePort(t)
	other := freePort(t)
	c := New(tracker.New())
	t.Cleanup(func() { _ = c.Close() })

	if err := c.Apply(configOn(port)); err != nil {
		t.Fatalf("Apply: %v", err)
	}
	waitPort(t, port, true)

	if err := c.Check(configOn(other)); err != nil {
		t.Errorf("Check 合法配置报错: %v", err)
	}
	if canConnect(other) {
		t.Error("Check 不该真的把端口监听起来")
	}
	if err := c.Check(json.RawMessage(`{"inbounds":[{"type":"nope"}]}`)); err == nil {
		t.Error("Check 非法配置应当报错")
	}
	waitPort(t, port, true)
}

func TestRestartWithoutConfigIsAnError(t *testing.T) {
	c := New(tracker.New())
	if err := c.Restart(); err == nil {
		t.Error("没有已应用配置时 Restart 应当报错,而不是静默成功")
	}
}

func TestCloseIsIdempotent(t *testing.T) {
	port := freePort(t)
	c := New(tracker.New())
	if err := c.Apply(configOn(port)); err != nil {
		t.Fatalf("Apply: %v", err)
	}
	if err := c.Close(); err != nil {
		t.Errorf("首次 Close: %v", err)
	}
	// 第二次 Close 走的是 box == nil 分支,不该把 os.ErrClosed 冒到调用方。
	if err := c.Close(); err != nil {
		t.Errorf("重复 Close 应当无害,得到: %v", err)
	}
}
