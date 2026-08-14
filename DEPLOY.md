# 部署与跨机验证

假设主控在 A 机、agent 在 B 机，两台均为 Linux。默认端口：

- `18443/tcp`：agent 回连主控，需允许 B → A
- `18081/tcp`：订阅 HTTP，默认只监听 `127.0.0.1`，不要直接暴露公网

安装前先用 `ss -tlnp` 确认端口没有被 nginx、Docker 或其它代理占用。

## 1. 安装主控（A 机）

```sh
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | bash
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

## 2. 安装 agent（B 机）

推荐在 A 机打开 TUI，进入“服务管理”页按 `[a]`，复制生成的完整命令到 B 机执行：

```sh
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh \
  | SBX_SERVER='wss://<A机地址>:18443/ws' SBX_TOKEN='…' SBX_FINGERPRINT='sha256:…' bash
```

脚本会：

1. 下载正确架构的 `sbx-agent` 并校验 SHA-256；
2. 安装 `/etc/systemd/system/sbx-agent.service`；
3. 写入 `/etc/sbx/agent.toml`（0600）；
4. 执行 `systemctl enable --now sbx-agent`。

查看状态：

```sh
systemctl status sbx-agent --no-pager
journalctl -u sbx-agent -n 30 --no-pager
```

期望看到：

```text
没有 last-applied.json,等待主控首次 config.apply
已连上主控:agent_id=…,上报间隔 30s,心跳 10s
```

轮换 token：在 TUI 服务管理页按 `[r]`，把新命令在 B 机再执行一次。旧配置会备份成
`/etc/sbx/agent.toml.bak`。

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
systemctl restart sbx-agent           # B 机
journalctl -u sbx-agent -n 20 --no-pager
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

systemd 的 `Restart=always` 会拉起它。重启前后的累计流量应相加且不重复，`agent_events` 应记录计数器 epoch 变化。

### 3.7 升级

主控：

```sh
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | bash
systemctl restart sbx
```

正在运行的 TUI 需要退出后重新进入，才能加载新二进制。

agent：在 TUI 服务管理页按 `[u]`，可升级当前 agent 或全部在线 agent。agent 下载目标版本、校验
SHA-256、原子替换自身并退出，由 `sbx-agent.service` 的 `Restart=always` 拉起新版本。

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
systemctl disable --now sbx 2>/dev/null || true
rm -f /usr/local/bin/sbx /usr/local/bin/sbx-agent
rm -f /etc/systemd/system/sbx.service /etc/systemd/system/sbx-agent.service
rm -rf /etc/sbx /var/lib/sbx-agent
systemctl daemon-reload
ufw delete allow 18443/tcp
```
