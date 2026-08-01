// Package state 负责 last-applied.json 的读写(DESIGN.md §4.1)。
//
// 这份文件是 agent 重启后的记忆:没有它,agent 只能报 revision=0,
// 主控就得每次重连都全量下发一遍配置。它同时也是**冷启动配置源** ——
// 进程起来后不等握手就能把 box 拉起来,断网期间节点照样服务。
//
// 四个字段是**各自独立**的,不能合并:
//   - config_revision / user_state_revision 对应两条独立的下发通道(§4.2)。
//     合并后要么把最频繁的 user.state 变成一次 box 重建,要么丢掉离线期间的封禁 ——
//     后者是个不报错的计费漏洞。
//   - options 是 sing-box 配置原文,原样存 json.RawMessage,**不做任何解析或规范化**:
//     agent 不理解的字段(新版 sing-box 的新选项)不该在存盘时被吃掉。
//   - disabled 是禁用名单全量快照,重启后先挡住再说,不必等第一条 user.state。
package state

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
)

// FileName 是落盘文件名,放在 config.Config.StateDir 下。
const FileName = "last-applied.json"

type LastApplied struct {
	ConfigRevision    int64           `json:"config_revision"`
	UserStateRevision int64           `json:"user_state_revision"`
	Options           json.RawMessage `json:"options,omitempty"`
	Disabled          []string        `json:"disabled,omitempty"`
}

// Load 读取状态文件。文件不存在**不是错误** —— 首次启动就是这样,
// 返回一个零值(两个 revision 都是 0,Options 为 nil = 不启动 box)。
func Load(dir string) (*LastApplied, error) {
	data, err := os.ReadFile(filepath.Join(dir, FileName))
	if errors.Is(err, os.ErrNotExist) {
		return &LastApplied{}, nil
	}
	if err != nil {
		return nil, err
	}
	var la LastApplied
	if err := json.Unmarshal(data, &la); err != nil {
		// 文件损坏(掉电写了一半的旧文件、手改坏了)时不要让 agent 起不来:
		// 当成空状态,握手报 0,主控会补发全量。
		return &LastApplied{}, nil
	}
	return &la, nil
}

// Save 原子落盘:先写同目录下的临时文件,fsync,再 rename 覆盖。
//
// 必须同目录 —— 跨文件系统的 rename 不是原子的,会退化成 copy。
// 掉电时最坏情况是留下一个 .tmp 垃圾文件,而不是一个截断的 last-applied.json。
func Save(dir string, la *LastApplied) error {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(la, "", "  ")
	if err != nil {
		return err
	}
	final := filepath.Join(dir, FileName)
	tmp, err := os.CreateTemp(dir, FileName+".tmp*")
	if err != nil {
		return err
	}
	tmpName := tmp.Name()
	defer os.Remove(tmpName) // rename 成功后这一行是 no-op

	if _, err := tmp.Write(data); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmpName, final)
}
