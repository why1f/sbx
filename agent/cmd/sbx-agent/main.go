// Package main 是 sbx-agent 的主程序入口(DESIGN.md §7)。
//
// agent 的职责(§0.3):
//  1. 内嵌 sing-box,**通过 box.New() / Start() / Close() API 控制它**;
//  2. 挂载一个 tracker,拦截数据路径记录 per-(user,tag) 流量(§7.1);
//  3. 连 master 的 WS,收命令(config.apply / user.state)、定期上报(stats / sysinfo);
//  4. 落盘 last-applied.json(两个 revision + options + 禁用名单),用于握手比对(§4.1)。
//
// 启动顺序有意是「先起 box,再连主控」:last-applied.json 里的配置就是**冷启动配置源**,
// 主控挂着的时候节点照样服务。反过来先等握手的话,主控一次维护窗口就等于全网停服。
package main

import (
	"context"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/yourorg/sbx-agent/boxctl"
	"github.com/yourorg/sbx-agent/config"
	"github.com/yourorg/sbx-agent/master"
	"github.com/yourorg/sbx-agent/state"
	"github.com/yourorg/sbx-agent/sysinfo"
	"github.com/yourorg/sbx-agent/tracker"
)

// Version 由 release 构建注入:`-ldflags "-X main.Version=1.2.3"`(§11.1)。
// 它会随握手报给主控,显示在节点列表里。
var Version = "dev"

func main() {
	master.AgentVersion = Version

	if len(os.Args) < 2 {
		log.Fatal("用法: sbx-agent <config.toml>")
	}
	cfg, err := config.Load(os.Args[1])
	if err != nil {
		log.Fatalf("读配置失败: %v", err)
	}

	// 1. 初始化 tracker(§7.1)。
	//    它比 box 活得久 —— 计数器活在这里,box 重建不清零(§5.2)。
	tr := tracker.New()

	// counter_epoch **每进程生成一次**,不随 box 重建变化。
	// 主控靠「epoch 相同且新值 ≥ 旧值 → 取差值,否则取全量」来算增量(§5.2);
	// 让它跟着 box 走的话,每次改配置都会被算成一次计数器归零,流量凭空翻倍。
	counterEpoch := sysinfo.RandomUUID()

	// 2. 读上次落盘的状态,并据此冷启动。
	st, err := state.Load(cfg.StateDir)
	if err != nil {
		log.Fatalf("读 %s 失败: %v", cfg.StateDir, err)
	}
	tr.SetDisabled(st.Disabled)

	bc := boxctl.New(tr)
	if len(st.Options) > 0 {
		if err := bc.Apply(st.Options); err != nil {
			// 起不来不退出:主控随后的 config.apply 可能正好是修好这个问题的那一版。
			log.Printf("按 last-applied.json 启动 box 失败(等主控下发新配置): %v", err)
		} else {
			log.Printf("已按 last-applied.json 启动 box(config revision %d)", st.ConfigRevision)
		}
	} else {
		log.Println("没有 last-applied.json,等待主控首次 config.apply")
	}

	// 3. 连 master,收发循环(内部自带断线重连)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	conn := master.NewConn(cfg, tr, bc, st, counterEpoch)
	go func() {
		if err := conn.Run(ctx); err != nil {
			log.Printf("master 连接异常: %v", err)
		}
	}()

	// 4. 等退出信号,或 agent 自升级完成后的主动退出
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
	select {
	case <-sig:
		log.Println("收到退出信号,停止 box")
	case <-conn.Shutdown():
		log.Println("agent 请求退出,停止 box")
	}
	cancel()
	if err := bc.Close(); err != nil {
		log.Printf("停止 box: %v", err)
	}
}
