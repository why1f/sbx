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
- 网卡记账口径每台可选:入出总计 / 仅出站(TX) / 仅入站(RX) / 入出取大
- 网卡月重置按每台机器自己的时区翻月（厂商按机房当地零点计费），agent 自动上报，可手工覆盖
- Base64 分享链接、Clash/Mihomo YAML、浏览器流量页
- Telegram 绑定、用量查询、阈值告警和定时播报
- 主控与 agent 自升级，发布资产自动校验 SHA-256
- `sbx doctor` 检查二进制、配置、数据库、systemd、监听、证书和可选服务

核心路径已有单元、真实 socket、真实 sing-box、跨语言 golden 和跨机流量测试：

| 层 | 覆盖的东西 |
|---|---|
| `master/`、`shared/` | 单元 + 真实 SQLite + 真实 socket + 无头 TUI 渲染快照；测试行数约占 Rust 代码的四成 |
| `agent/` | 单元 + 真实 sing-box 装配；Linux CI 额外跑 `go test -race` |
| `master/testdata/` | 八协议与出站策略的跨语言 golden 配置 |
| `spike/` | 真实 sing-box 的流量与拒绝行为回归 |
| `e2e/` | 两台 agent 的跨机求和、断线重连与 epoch/delta；`e2e/run.sh` 在 CI 里真跑 |

**这里不写测试数量。** 手写的计数一定会漂移成谎，而这一段里别的论断读者没法当场核对，
唯一能核对的就是那几个数字——数字错了，整段话的可信度一起打折。要数量就自己跑：

```sh
cargo test --all 2>&1 | grep '^test result'
cd agent && go test -tags with_quic,with_utls ./... -v 2>&1 | grep -c '^=== RUN'
```

## 安装与升级

### 主控

```sh
(curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh 2>/dev/null \
  || wget -qO- https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh) | sh
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

### 被控机（Alpine/OpenRC 也适用）

推荐从主控 TUI 生成完整命令：进入“服务管理”页，按 `[a]` 新增 agent，复制弹窗中的命令到被控机执行。
命令会包含主控地址、一次性 token 和 TLS 指纹，例如：

```sh
(curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh 2>/dev/null \
  || wget -qO- https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh) \
  | SBX_SERVER='wss://主控:18443/ws' SBX_TOKEN='…' SBX_FINGERPRINT='sha256:…' sh
```

它会安装 `sbx-agent`、写入 `/etc/sbx/agent.toml`（0600）并加入当前机器的 supervisor：
有 systemd 时执行 `systemctl enable --now sbx-agent`，有 OpenRC 时安装 `/etc/init.d/sbx-agent`、加入 `default` runlevel 并启动。
服务文件和 `agent.example.toml` 按目标版本从源码 tag 下载，不占 GitHub Release 静态资产。
没有 supervisor 时只安装文件，不会伪造一个无法自愈的后台服务；手动启动命令为
`/usr/local/bin/sbx-agent /etc/sbx/agent.toml`，崩溃或自升级退出后不会自动拉起。
轮换 token 后，把 TUI 新生成的命令再运行一次即可；旧配置会备份为
`agent.toml.bak`。

只安装/升级而不启动服务时加 `--no-restart`；之后按提示手动执行对应的
`systemctl restart` 或 `rc-service restart`。不带 token 的二进制升级只会重启原本已经运行的 agent。

单独安装 agent 也可以：

```sh
(curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh 2>/dev/null \
  || wget -qO- https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh) | sh -s -- agent
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

# 给某台被控加一份自定义 sing-box 片段（出站 / 路由 / DNS），或清掉它
sbx --config /etc/sbx/config.toml agent-config-set 1 custom.jsonc
sbx --config /etc/sbx/config.toml agent-config-clear 1

# TUI
sbx --config /etc/sbx/config.toml tui
```

`doctor` 全程只读：不会创建数据库、执行迁移或生成证书。数据库一项显示文件位置、大小（含 WAL）和 schema 版本。

## TUI

全局按键：`1-5` / `Tab` 切页，`↑↓` / `jk` 选择，`R` 刷新，`U` 升级主控，`q` 退出。

| 页 | 主要操作 |
|---|---|
| 仪表盘 | 集群概况、网速曲线、用户/节点用量排行；`←/→` 换栏，`Enter` 看明细 |
| 服务管理 | `a` 新增、`E` 编辑（含记账口径与重置时区）、`Enter` 网卡明细、`c` 查看完整 sing-box 配置、`C` 改自定义片段、`K` 让它自己的 sing-box 校验配置、`o` 出站策略、`i` 接入命令、`u` 升级 agent、`r` 轮换 token、`d` 删除 |
| 节点 | `a` 新增、`E` 编辑、`Enter` 用户明细、`d` 删除 |
| 用户 | `a` 新增、`E` 编辑、`n` 分配节点、`b` 绑定网卡用量、`T` token、`r` 重置、`t` 启停、`s` 订阅、`d` 删除 |
| 设置 | `Enter` 修改配置文件；修改后重启 daemon |

说明：

- `[c]` 显示主控现场组装、实际下发的完整配置，**包含原始凭据且不脱敏**；不要截图或外传。
- `[o]` 按 agent 设置自动、优先 IPv4、优先 IPv6、仅 IPv4、仅 IPv6。
- `[C]` 拉 `$EDITOR` 改这台的**自定义片段**：只能写 `outbounds` / `route` / `dns` 三个顶层字段。
  注释（`//` `#` `/* */`）与尾随逗号都行，而且**原文存进库里**，下次打开还在。
  全部清空存盘 = 恢复默认。
  **`inbounds` 不在内** —— 记账键是（用户, inbound tag），改了 tag 流量会静默停止记账。
  自定义里写了 `route.default_domain_resolver` 就等于接管 `[o]`，那时摘要行会标「由自定义配置接管」。
- `[K]` 把即将下发的那份配置交给**那台机器自己的 sing-box** 试建一次（`config.check`，建完立即关掉，不接管当前实例、不占端口）。
  字段名拼错、`route` 引用了不存在的 outbound tag 这类错只有它能报 —— 主控里没有 sing-box。
  验不出**端口占用**（那要到 `Start()`，由 `config.apply` 的 build-first + 回滚那一层守）。
- `[E]` 里的「网卡记账口径」决定这台机器本周期算多少：入出总计 / 仅出站(TX) / 仅入站(RX) / 入出取大。
  方向站在被控机看：**出站 = 机器发出（服务器→客户端，也就是客户端那边的下载），入站 = 机器收到**。
  原始两个方向始终完整保存，切换口径只重算显示，不清零也不改历史；`Enter` 的网卡明细同时给出两者。
- `[E]` 里的「重置时区」决定每月**哪一刻**翻月：厂商按机房当地零点结算，而主控可能在别的时区。
  留空则跟随 agent 上报的本机偏移（新机器接入即对齐）；机器上装的时区不等于厂商计费的时区时，
  在这里填显式偏移（例 `UTC-07:00`）压过它。`Enter` 的网卡明细会写明当前用的是哪个偏移、这个值是谁给的。
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
- agent 网卡流量按 `agents.nic_accounting_mode` 投影；原始 RX/TX 分别入库，模式只在读取时生效，
  绑定网卡的订阅统计用同一份投影。
- `config_revision` 与 `user_state_revision` 独立：前者重建 box，后者只更新内存禁用名单。
- Reality、自签 TLS、Shadowsocks 等密钥在创建节点时生成一次并持久化，后续下发不得重新生成。
- agent 必须由能在进程退出后重新拉起它的 supervisor 管理；systemd 的 `Restart=always` 与 OpenRC 的 `supervise-daemon` 都满足。自升级会原子替换二进制后主动退出，没有 supervisor 就会永久离线。
- 每台 agent 可带一份自定义 sing-box 片段（`agents.custom_json`，只限 `outbounds` / `route` / `dns`）。
  它存在**主控库里**而不是 agent 上，配置权威不变；组装时先并入自定义，再叠出站策略。
  `inbounds` 不开放修改 —— 记账键是（用户, inbound tag）。
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
`agent.example.toml`、systemd/OpenRC service 由安装脚本从对应源码 tag 获取，不重复上传到 Release。

## 仓库布局

| 路径 | 用途 |
|---|---|
| `master/` | Rust 主控、CLI、TUI、数据库、订阅和通知 |
| `agent/` | Go agent，内嵌 sing-box |
| `shared/` | Rust 协议类型 |
| `packaging/` | 安装脚本、配置示例、systemd/OpenRC service |
| `master/testdata/` | 八协议、出站策略、自定义片段合并后的 golden 配置，测试必需 |
| `spike/` | 真实 sing-box tracker 回归，CI 必需 |
| `e2e/` | 跨 agent 记账与断连恢复的端到端验证，`e2e/run.sh` 在 CI 里真跑 |

这些目录均被构建、测试、CI 或发布流程使用，不是可删除的样例文件。

## 许可证

本仓库采用双许可，边界在 WebSocket：

| 目录 | 许可证 | 原因 |
|---|---|---|
| `master/`、`shared/` | MIT（[`LICENSE-MIT`](LICENSE-MIT)） | 不链接 GPL 代码 |
| `agent/` | GPLv3（[`agent/LICENSE`](agent/LICENSE)） | 静态链接 sing-box |

master 与 agent 是两个独立进程，只通过网络协议通信。跨边界的功能必须继续通过 WebSocket，
不要把 agent 的 GPL 代码直接链接进 master。
