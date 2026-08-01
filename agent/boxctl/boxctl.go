// Package boxctl 管 sing-box 实例的生命周期:构建、热重载、停止(DESIGN.md §7.4)。
//
// 一台 agent 上**同时只能有一个 box 在跑** —— 新旧两个 box 监听同一组端口,
// 并存必然 EADDRINUSE。所以热重载不是「起新的再停旧的」,而是下面这个顺序:
//
//	box.New(新配置)
//	  ├─ 失败 → 旧的原样继续跑,返回错误,主控保留旧 revision(§4.1)
//	  └─ 成功 → old.Close() → new.Router().AppendTracker(同一个 tracker) → new.Start()
//	                            ├─ 成功 → 落盘 last-applied.json
//	                            └─ 失败 → 用旧配置重建 + AppendTracker + Start,返回错误
//
// 顺序不能调:
//   - box.New 放最前,是为了让**配置错误**这种最常见的失败在完全没有停机的情况下被挡掉;
//   - AppendTracker 必须在 Start 之前 —— 它内部是无锁 append,box 已经在收流量时
//     并发调用会 race(§12.0 的 spike 实测确认了这个调用点)。
//
// **tracker 实例全程复用,不随 box 重建。** 那张计数器 map 活在 tracker 里,
// 这是 §5.2「counter_epoch 不跟着 box.New() 走」的前提:box 重建不该让主控
// 把累计量当成计数器归零来处理。
package boxctl

import (
	"context"
	"encoding/json"
	"fmt"
	"sync"
	"time"

	box "github.com/sagernet/sing-box"
	"github.com/sagernet/sing-box/include"
	"github.com/sagernet/sing-box/option"
	sjson "github.com/sagernet/sing/common/json"

	"github.com/yourorg/sbx-agent/tracker"
)

type Controller struct {
	mu      sync.Mutex
	tracker *tracker.Tracker
	box     *box.Box
	// current 是当前**正在跑**的配置原文,用于 Start 失败时回滚。
	// box == nil 时它也可能非 nil(上一次回滚也失败了),此时 Apply 不做回滚。
	current json.RawMessage
	since   time.Time
}

func New(tr *tracker.Tracker) *Controller {
	return &Controller{tracker: tr}
}

// parse 把主控下发的 options 解成 option.Options。
//
// **不能用 encoding/json。** option.Inbound / option.Outbound 这些类型是
// tagged union,反序列化时要从 context 里取 inbound/outbound 注册表才知道
// Type 字段对应哪个具体结构体;用标准库解出来的 Options 里所有协议选项都是 nil。
// 这里走的是 sing-box 自己 cmd/sing-box/cmd_run.go:60 的同一条路径。
func parse(raw json.RawMessage) (option.Options, error) {
	return sjson.UnmarshalExtendedContext[option.Options](include.Context(context.Background()), raw)
}

// Check 只校验配置能否构建,不接管当前实例(method: config.check)。
//
// 构建出来的 box 立刻 Close —— 它没 Start 过,不占端口,Close 只回收内部资源。
func (c *Controller) Check(raw json.RawMessage) error {
	b, err := build(raw)
	if err != nil {
		return err
	}
	_ = b.Close()
	return nil
}

func build(raw json.RawMessage) (*box.Box, error) {
	opts, err := parse(raw)
	if err != nil {
		return nil, fmt.Errorf("解析配置: %w", err)
	}
	// include.Context() 不能少 —— 少了 box.New 直接报
	// "missing inbound registry in context"(§12.0 spike 实测)。
	b, err := box.New(box.Options{
		Context: include.Context(context.Background()),
		Options: opts,
	})
	if err != nil {
		return nil, fmt.Errorf("构建 box: %w", err)
	}
	return b, nil
}

// Apply 用新配置替换当前实例,按包注释里的顺序。
//
// 返回 nil 时调用方才该落盘 last-applied.json 并向主控确认新 revision。
func (c *Controller) Apply(raw json.RawMessage) error {
	c.mu.Lock()
	defer c.mu.Unlock()

	nb, err := build(raw)
	if err != nil {
		// 旧实例完全没被碰过,继续跑。
		return err
	}

	if c.box != nil {
		// Close 是幂等的(第二次返回 os.ErrClosed),这里的错误只作日志价值,
		// 不能因为它中断切换 —— 否则端口就悬着了。
		_ = c.box.Close()
		c.box = nil
	}

	nb.Router().AppendTracker(c.tracker)
	if err := nb.Start(); err != nil {
		_ = nb.Close()
		return c.rollbackLocked(fmt.Errorf("启动新配置: %w", err))
	}

	c.box = nb
	c.current = raw
	c.since = time.Now()
	return nil
}

// rollbackLocked 在新配置起不来时把旧配置重新拉起来。调用方必须持有 c.mu。
//
// 回滚本身也可能失败(比如旧配置占的端口被别的进程抢了)。那种情况下节点是真的下线了,
// 返回的错误里两段原因都要带上 —— 只报「回滚失败」会让人去查错的那份配置。
func (c *Controller) rollbackLocked(cause error) error {
	if len(c.current) == 0 {
		return cause // 本来就没有旧配置(冷启动第一次 apply 就失败)
	}
	ob, err := build(c.current)
	if err != nil {
		return fmt.Errorf("%w;回滚重建旧配置也失败: %v", cause, err)
	}
	ob.Router().AppendTracker(c.tracker)
	if err := ob.Start(); err != nil {
		_ = ob.Close()
		return fmt.Errorf("%w;回滚启动旧配置也失败: %v", cause, err)
	}
	c.box = ob
	c.since = time.Now()
	return cause
}

// Restart 用当前配置重启(method: box.restart)。没有配置时是空操作。
func (c *Controller) Restart() error {
	c.mu.Lock()
	raw := c.current
	c.mu.Unlock()
	if len(raw) == 0 {
		return fmt.Errorf("没有已应用的配置,无从重启")
	}
	return c.Apply(raw)
}

// Running 报告 box 是否在跑,以及从何时起(method: box.status)。
func (c *Controller) Running() (bool, int64) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.box == nil {
		return false, 0
	}
	return true, c.since.Unix()
}

// Close 停掉当前实例。进程退出路径用。
func (c *Controller) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.box == nil {
		return nil
	}
	err := c.box.Close()
	c.box = nil
	return err
}
