# sbx

一台 Rust 主控通过 WebSocket 管理多台 Go agent；agent 内嵌 sing-box。
提供 CLI、TUI、订阅、流量统计、Telegram 通知、配额与到期管理。

- 部署与跨机验证：[`DEPLOY.md`](DEPLOY.md)
- 架构约束与正确性理由：[`DESIGN.md`](DESIGN.md)
- 版本记录：[`CHANGELOG.md`](CHANGELOG.md)

## 功能

- 多 agent / 多节点 / 多用户管理
- 八种协议：VLESS Reality、VLESS WS、VMess WS、Shadowsocks、Trojan、Hysteria2、TUIC、AnyTLS
- 主控是 sing-box 配置的唯一真源；agent 离线时按最后一次配置冷启动
- 用户配额、到期、周期重置、流量倍率、手动/自动停用
- agent 网卡流量、CPU、内存、地址和在线状态
- Base64 分享链接、Clash/Mihomo YAML、浏览器流量页
- Telegram 绑定、用量查询、阈值告警和定时播报
- 主控与 agent 自升级，发布资产自动校验 SHA-256
- `sbx doctor` 检查二进制、配置、数据库、systemd、监听、证书和可选服务

核心路径已有单元、真实 socket、真实 sing-box、跨语言 golden 和跨机流量测试：

- master：509 个测试
- shared：6 个测试
- agent：57 个顶层测试函数；Linux CI 额外运行 `go test -race`
- `spike/`：真实 sing-box 流量与拒绝行为
- `e2e/`：两台 agent 的跨机求和、断线重连与 epoch/delta

## 安装与升级

### 主控

```sh
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | bash
cp -n /etc/sbx/config.example.toml /etc/sbx/config.toml
systemctl enable --now sbx
sbx --config /etc/sbx/config.toml doctor
```

脚本安装：

- `/usr/local/bin/sbx`
- `/etc/sbx/config.example.toml`
- `/etc/systemd/system/sbx.service`

不会覆盖已有的 `/etc/sbx/config.toml`。重跑安装命令就是升级；新主控二进制需要重启 daemon，
正在运行的 TUI 需要退出后重新进入。

### 被控机

推荐从主控 TUI 生成完整命令：进入“服务管理”页，按 `[a]` 新增 agent，复制弹窗中的命令到被控机执行。
命令会包含主控地址、一次性 token 和 TLS 指纹，例如：

```sh
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh \
  | SBX_SERVER='wss://主控:18443/ws' SBX_TOKEN='…' SBX_FINGERPRINT='sha256:…' bash
```

它会安装 `sbx-agent`、写入 `/etc/sbx/agent.toml`（0600）并执行
`systemctl enable --now sbx-agent`。轮换 token 后，把 TUI 新生成的命令再运行一次即可；旧配置会备份为
`agent.toml.bak`。

单独安装 agent 也可以：

```sh
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | bash -s -- agent
```

## 快速使用

```sh
# 只读自检；有 ERR 时退出码为 1，WARN 不算失败
sbx --config /etc/sbx/config.toml doctor

# CLI
sbx --config /etc/sbx/config.toml init-db
sbx --config /etc/sbx/config.toml agent-add tokyo
sbx --config /etc/sbx/config.toml agent-list
sbx --config /etc/sbx/config.toml node-add 1 tokyo-reality 443 --protocol vless-reality
sbx --config /etc/sbx/config.toml user-add alice --quota-gb 100
sbx --config /etc/sbx/config.toml user-assign alice 1
sbx --config /etc/sbx/config.toml user-sub alice

# TUI
sbx --config /etc/sbx/config.toml tui
```

`doctor` 全程只读：不会创建数据库、执行迁移或生成证书。数据库一项显示文件位置、大小（含 WAL）和 schema 版本。

## TUI

全局按键：`1-5` / `Tab` 切页，`↑↓` / `jk` 选择，`R` 刷新，`U` 升级主控，`q` 退出。

| 页 | 主要操作 |
|---|---|
| 仪表盘 | 集群概况、网速曲线、用户/节点用量排行；`←/→` 换栏，`Enter` 看明细 |
| 服务管理 | `a` 新增、`E` 编辑、`Enter` 网卡明细、`c` 查看完整 sing-box 配置、`o` 出站策略、`i` 接入命令、`u` 升级 agent、`r` 轮换 token、`d` 删除 |
| 节点 | `a` 新增、`E` 编辑、`Enter` 用户明细、`d` 删除 |
| 用户 | `a` 新增、`E` 编辑、`n` 分配节点、`b` 绑定网卡用量、`T` token、`r` 重置、`t` 启停、`s` 订阅、`d` 删除 |
| 设置 | `Enter` 修改配置文件；修改后重启 daemon |

说明：

- `[c]` 显示主控现场组装、实际下发的完整配置，**包含原始凭据且不脱敏**；不要截图或外传。
- `[o]` 按 agent 设置自动、优先 IPv4、优先 IPv6、仅 IPv4、仅 IPv6。
- `[u]` 可升级当前 agent 或全部在线 agent；daemon 负责真正下发。
- TUI 与 daemon 是两个独立进程，只通过 SQLite 交换状态。

## 订阅

默认订阅监听为 `127.0.0.1:18081`，建议由 nginx/caddy 提供外部 TLS：

```text
GET /sub/<token>              按 User-Agent 选择格式
GET /sub/<token>?type=clash   Clash/Mihomo YAML
GET /sub/<token>?type=stats   浏览器流量页
```

这是唯一的 HTTP 界面，而且只读，不提供管理 API。

IPv6 输出规则：

- URI authority 必须带方括号：`vless://uuid@[2001:db8::1]:443`
- Clash/Mihomo、VMess JSON 等结构化 `server` / `add` / `host` 字段必须使用裸地址：
  `server: "2001:db8::1"`

方括号属于 URI 语法，不是 IPv6 地址内容。

## 关键运行语义

- agent 每 `heartbeat_secs` 主动发送心跳；主控静默超过 `max(3×heartbeat, 30s)` 判定掉线。
- IPv6 由 agent 先询问内核的 RFC 6724 源地址选择；无全球地址时才查询外部服务。
- 用户流量按 `(用户, inbound tag)` 记账，不能只按用户名。
- `config_revision` 与 `user_state_revision` 独立：前者重建 box，后者只更新内存禁用名单。
- Reality、自签 TLS、Shadowsocks 等密钥在创建节点时生成一次并持久化，后续下发不得重新生成。
- agent 不开放管理端口；管理面只有 agent 主动连接主控的 WebSocket。

## 构建与测试

```sh
# Rust 主控
cargo build --release
cargo test --all
cargo clippy --all-targets -- -D warnings

# Go agent；两个 build tag 都是必需的
cd agent
go build -tags with_quic,with_utls ./...
go vet -tags with_quic,with_utls ./...
go test -tags with_quic,with_utls ./...

# tracker / 真流量回归
cd ../spike && go run .
```

CI 还检查：

- `gofmt`
- Linux race detector
- amd64 / arm64 交叉编译
- `packaging/install.sh` 在 dash 与 bash 下的行为
- 禁止 `agent/go.mod` 出现 `replace` 或新增 `agent/patches/`

发布由 `v*` tag 触发。tag、`master/Cargo.toml` 的版本和 `CHANGELOG.md` 标题必须一致。
主控发布为 musl 静态 `.tar.gz`；agent 发布为裸二进制和独立 `.sha256`。

## 仓库布局

| 路径 | 用途 |
|---|---|
| `master/` | Rust 主控、CLI、TUI、数据库、订阅和通知 |
| `agent/` | Go agent，内嵌 sing-box |
| `shared/` | Rust 协议类型 |
| `packaging/` | 安装脚本、配置示例、systemd unit |
| `master/testdata/` | 八协议及出站策略 golden 配置，测试必需 |
| `spike/` | 真实 sing-box tracker 回归，CI 必需 |
| `e2e/` | 跨 agent 记账与断连恢复驱动，CI 编译检查 |

这些目录均被构建、测试、CI 或发布流程使用，不是可删除的样例文件。

## 许可证

本仓库采用双许可，边界在 WebSocket：

| 目录 | 许可证 | 原因 |
|---|---|---|
| `master/`、`shared/` | MIT（[`LICENSE-MIT`](LICENSE-MIT)） | 不链接 GPL 代码 |
| `agent/` | GPLv3（[`agent/LICENSE`](agent/LICENSE)） | 静态链接 sing-box |

master 与 agent 是两个独立进程，只通过网络协议通信。跨边界的功能必须继续通过 WebSocket，
不要把 agent 的 GPL 代码直接链接进 master。
