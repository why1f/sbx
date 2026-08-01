// Package config 解析 agent 配置(DESIGN.md §7.4)。
package config

import (
	"errors"
	"os"

	"github.com/BurntSushi/toml"
)

// DefaultStateDir 是 last-applied.json 的落盘目录(§4.1)。
// 部署形态是 Linux systemd 服务,所以默认值取 FHS 的 /var/lib;
// 开发时用 state_dir 覆盖即可(Windows 上跑必须覆盖)。
const DefaultStateDir = "/var/lib/sbx-agent"

type Config struct {
	Server      string `toml:"server"`      // ws://主控地址:端口/ws
	Token       string `toml:"token"`       // 连接 token(明文)
	Fingerprint string `toml:"fingerprint"` // 主控证书指纹(TOFU,§1.3)
	Insecure    bool   `toml:"insecure"`    // true = 跳过证书校验(仅开发)
	// StateDir 存 last-applied.json:两个 revision + options + disabled(§4.1)。
	// 握手时要把这里读到的 revision 报给主控,主控据此决定是否补发 —— 丢了它
	// 每次重连都会全量重下发配置,不致命但没必要。
	StateDir string `toml:"state_dir"`
}

func Load(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var cfg Config
	if err := toml.Unmarshal(data, &cfg); err != nil {
		return nil, err
	}
	if cfg.Server == "" {
		return nil, errors.New("配置缺少 server(形如 wss://host:port/ws)")
	}
	if cfg.Token == "" {
		return nil, errors.New("配置缺少 token")
	}
	if cfg.StateDir == "" {
		cfg.StateDir = DefaultStateDir
	}
	return &cfg, nil
}
