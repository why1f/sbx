# 部署与跨机验证

假设主控在 A 机、agent 在 B 机，两台均为 Linux。默认端口：

- `18443/tcp`：agent 回连主控，需允许 B → A
- `18081/tcp`：订阅 HTTP，默认只监听 `127.0.0.1`，不要直接暴露公网

安装前先用 `ss -tlnp` 确认端口没有被 nginx、Docker 或其它代理占用。

## 1. 安装主控（A 机）

```sh
(curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh 2>/dev/null \
  || wget -qO- https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh) | sh
cp -n /etc/sbx/config.example.toml /etc/sbx/config.toml
```

至少检查 `/etc/sbx/config.toml`：

```toml
[cluster]
listen = "0.0.0.0:18443"

[subscription]
listen = "127.0.0.1:18081"
```

启动：

```sh
systemctl daemon-reload
systemctl enable --now sbx
sbx --config /etc/sbx/config.toml doctor
sbx --config /etc/sbx/config.toml fingerprint
journalctl -u sbx -n 30 --no-pager
```

`doctor` 只读，不会创建数据库、执行迁移或生成证书；有 ERR 时退出码为 1。

只需对外放行集群端口：

```sh
ufw allow 18443/tcp comment 'sbx cluster'
```

云厂商安全组/VCN 也必须放行。请从 B 机验证，而不是只在 A 机本地测：

```sh
nc -vz <A机公网地址> 18443
```

## 2. 安装 agent（B 机，支持 Alpine/OpenRC）

推荐在 A 机打开 TUI，进入“服务管理”页按 `[a]`，复制生成的完整命令到 B 机执行：

```sh
(curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh 2>/dev/null \
  || wget -qO- https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh) \
  | SBX_SERVER='wss://<A机地址>:18443/ws' SBX_TOKEN='…' SBX_FINGERPRINT='sha256:…' sh
```

脚本会：

1. 下载正确架构的 `sbx-agent` 并校验 SHA-256；
2. 按命令真实存在情况选择 supervisor：systemd 安装 `/etc/systemd/system/sbx-agent.service`，
   OpenRC 安装 `/etc/init.d/sbx-agent`；
3. 写入 `/etc/sbx/agent.toml`（0600）；
4. systemd 执行 `systemctl enable --now sbx-agent`，OpenRC 执行
   `rc-update add sbx-agent default` 后启动。

`agent.example.toml` 和两种 service 文件按目标版本从源码 tag 获取；`--version 0.x.y`
不会误配 `main` 分支的新 service。主控仍只使用 systemd。

查看状态（按 B 机 init 选择一组）：

```sh
# systemd
systemctl status sbx-agent --no-pager
journalctl -u sbx-agent -n 30 --no-pager

# OpenRC
rc-service sbx-agent status
tail -n 30 /var/log/sbx-agent.log
```

期望看到：

```text
没有 last-applied.json,等待主控首次 config.apply
已连上主控:agent_id=…,上报间隔 30s,心跳 10s
```

轮换 token：在 TUI 服务管理页按 `[r]`，把新命令在 B 机再执行一次。旧配置会备份成
`/etc/sbx/agent.toml.bak`。

容器里如果既没有可用的 systemd 命令，也没有 `rc-service` / `rc-update` / `openrc-run`，
脚本仍安装二进制与配置，但必须由容器入口或现有进程管理器运行：

```sh
exec /usr/local/bin/sbx-agent /etc/sbx/agent.toml
```

没有 supervisor 时，普通崩溃和自升级后的主动退出都不会自动恢复。`--no-restart` 在
systemd/OpenRC 下也只安装文件、不启动；脚本会打印对应的手工重启命令。不带 token 的升级
只重启原本正在运行的 agent，不会擅自启动停用状态的服务。

## 3. 验证清单

### 3.1 主控自检

```sh
sbx --config /etc/sbx/config.toml doctor
```

重点应为 OK：二进制、配置文件、数据目录、数据库、systemd 单元、集群监听和 TLS 证书。
订阅或 Telegram 明确关闭时属于正常状态。

### 3.2 握手与配置下发

A 机：

```sh
sbx --config /etc/sbx/config.toml agent-list
sbx --config /etc/sbx/config.toml node-add 1 tokyo-in 443 --protocol vless-reality
```

确认：

- agent 状态为 online，带版本、架构和公网地址；
- A 机日志出现配置下发成功；
- B 机 `ss -tlnp | grep 443` 能看到节点监听。

### 3.3 IPv6

B 机：

```sh
ip -6 route show default
curl -6 -sS --max-time 5 https://api.ip.sb/ip; echo
```

agent 优先使用内核 RFC 6724 源地址选择，不依赖外部查询速度；外部查询只是兜底。
如果 TUI 仍没有 IPv6，确认默认路由、云安全组及主机防火墙。

从其它机器验证入站，而不是只验证 B 机能出站：

```sh
nc -6 -vz <B机IPv6> <节点端口>
```

订阅格式：

- 分享 URI：`@[IPv6]:端口`
- Clash/Mihomo 与 VMess JSON 的 `server` / `add` / `host`：裸 IPv6，不含方括号

### 3.4 冷启动

停掉 A 机主控，再重启 B 机 agent：

```sh
systemctl stop sbx                    # A 机
systemctl restart sbx-agent           # B 机 systemd
# 或：rc-service sbx-agent restart    # B 机 OpenRC
journalctl -u sbx-agent -n 20 --no-pager
# OpenRC 日志：tail -n 20 /var/log/sbx-agent.log
```

应先出现“已按 last-applied.json 启动 box”，随后才是连接主控失败并退避。主控不可用时，已有节点仍应服务。
验证后恢复主控：

```sh
systemctl start sbx
```

### 3.5 掉线与重连

A 机：

```sh
systemctl stop sbx
sleep 40
systemctl start sbx
```

B 机应退避重试并自动重连。主控默认在 30 秒静默后把 agent 标为离线；TUI 中掉线灯为红色。
重连后 revision 一致时不应重复重建 box。

### 3.5b 关机再开机（被控端）

这条要单独测，因为它验证的不是重连逻辑而是**开机自启**——两者在 TUI 上看起来完全一样。

```sh
# B 机
systemctl is-enabled sbx-agent        # systemd，应输出 enabled
rc-update show default | grep sbx-agent   # OpenRC，应能看到 sbx-agent
poweroff
# 从面板重新开机后
systemctl is-active sbx-agent
```

开机后 1 分钟内主控的服务管理页应重新亮起（agent 退避上限 60 秒）。

**开机后一直不上线时按这个顺序查**（都在 B 机上）：

```sh
systemctl is-enabled sbx-agent        # disabled/not-found → 就是开机自启没设上
systemctl is-active  sbx-agent        # inactive/failed    → 起来了但崩了
journalctl -u sbx-agent -n 30 --no-pager    # 崩的原因；OpenRC 看 /var/log/sbx-agent.log
ls -l /etc/systemd/system/sbx-agent.service # 文件不存在 → 当初 unit 没装上
```

`is-enabled` 报 `disabled` 或 unit 根本不存在，说明这台机器当时是被手工拉起来的
（v0.4.18~v0.4.22 取不到 service 文件时会走到那条路）。重跑一次不带 `SBX_TOKEN` 的
安装命令即可修好：它会补上 service 文件和开机自启，不动现有配置。

### 3.6 流量记账与进程重启

用真实客户端经过 B 机节点产生流量，然后在 A 机检查：

```sh
sqlite3 /etc/sbx/sbx.db "SELECT u.name,n.tag,t.cycle_up,t.cycle_down
  FROM user_traffic t
  JOIN users u ON u.id=t.user_id
  JOIN nodes n ON n.id=t.node_id;"

sqlite3 /etc/sbx/sbx.db "SELECT * FROM user_traffic_total;"
```

多台 agent 时，`user_traffic_total` 应等于各节点之和。

流量进行中重启 agent：

```sh
kill -9 $(pidof sbx-agent)
```

systemd 的 `Restart=always` 或 OpenRC `supervise-daemon` 会拉起它。重启前后的累计流量应相加且不重复，
`agent_events` 应记录计数器 epoch 变化。

### 3.7 升级

主控：

```sh
(curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh 2>/dev/null \
  || wget -qO- https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh) | sh
systemctl restart sbx
```

正在运行的 TUI 需要退出后重新进入，才能加载新二进制。

agent：在 TUI 服务管理页按 `[u]`，可升级当前 agent 或全部在线 agent。agent 下载目标版本、校验
SHA-256、原子替换自身并退出，由 systemd `Restart=always` 或 OpenRC `supervise-daemon` 拉起新版本。
若手动运行或容器没有 supervisor，自升级退出后不会自动恢复。

## 4. 关键路径与权限

| 路径 | 内容 | 建议权限 |
|---|---|---|
| `/usr/local/bin/sbx` | 主控二进制 | 0755 |
| `/usr/local/bin/sbx-agent` | agent 二进制 | 0755 |
| `/etc/sbx/config.toml` | 主控配置 | 0640 |
| `/etc/sbx/sbx.db` | 主控数据库 | 仅服务用户可读写 |
| `/etc/sbx/tls/` | 主控证书和私钥 | 目录 0750 |
| `/etc/sbx/agent.toml` | 主控地址、token、指纹 | 0600 |
| `/var/lib/sbx-agent/last-applied.json` | agent 最后成功配置 | 仅服务用户可读写 |

## 5. 卸载

以下操作会删除配置、数据库、证书和 agent 状态，先自行备份：

```sh
systemctl disable --now sbx-agent 2>/dev/null || true
rc-update del sbx-agent default 2>/dev/null || true
rc-service sbx-agent stop 2>/dev/null || true
systemctl disable --now sbx 2>/dev/null || true
rm -f /usr/local/bin/sbx /usr/local/bin/sbx-agent
rm -f /etc/systemd/system/sbx.service /etc/systemd/system/sbx-agent.service /etc/init.d/sbx-agent
rm -rf /etc/sbx /var/lib/sbx-agent
systemctl daemon-reload 2>/dev/null || true
ufw delete allow 18443/tcp
```
