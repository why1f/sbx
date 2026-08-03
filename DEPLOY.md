# 部署与跨机验证清单

假设:**master 在 A 机,agent 在 B 机**,两台都是 Linux。
这份清单同时是 `DESIGN.md` §13.2 / §13.3 的**跨机版本** ——
仓库里已经跑过的那次是单机 loopback(§13.2 就是那么规定的),
跨机额外能验到的是:真实网络延迟下的 wss、跨公网断线重连、以及 systemd 单元。

> 装之前想清楚端口。这类机器上通常已经有 nginx / docker / 别的代理,
> 撞上去的表现是「agent 起不来」,或者更糟——把别人的服务挤掉。
> 下面用 `18443`(集群)和 `18081`(订阅)只是示例,**先 `ss -tlnp` 确认没被占**。

---

## A 机(master)

```sh
# 1. 装
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | bash
#    脚本会放好 /usr/local/bin/sbx、/etc/sbx/config.example.toml 与 sbx.service(不启用)。
#    它**不会覆盖**已有的 /etc/sbx/config.toml。
cp -n /etc/sbx/config.example.toml /etc/sbx/config.toml

# 2. 改 /etc/sbx/config.toml —— 至少确认这两个端口没被占
#    [cluster] listen  = "0.0.0.0:18443"   ← agent 要能连到,必须对外
#    [subscription] listen = "127.0.0.1:18081"  ← 只听本地,前面挂 nginx

# 3. 起来。证书不存在时会自签一张并打印指纹
systemctl daemon-reload && systemctl enable --now sbx
journalctl -u sbx -n 20 --no-pager        # 记下 fingerprint
sbx --config /etc/sbx/config.toml fingerprint   # 或者随时再打印一次
```

**防火墙**:只需要放行集群端口。订阅端口默认只听 `127.0.0.1`,不要对外。

```sh
ufw allow 18443/tcp comment 'sbx cluster'
# 云厂商那边的安全组(Oracle VCN / 阿里云 / AWS)也要放行,两层都得开
```

> Oracle Cloud 的实例默认还有一层 VCN 安全列表,ufw 放行了不代表外面连得进来。
> 验证:在 B 机上 `nc -vz <A机IP> 18443`。

## B 机(agent)

**推荐:让主控把命令生成好。** 在 A 机上 `sbx --config /etc/sbx/config.toml tui`,
按 `2` 到服务管理页,按 `a` 新增 —— 弹窗里就是一条填好 token 与证书指纹的完整命令,
整条复制到 B 机上跑(root):

```sh
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh \
  | SBX_SERVER='wss://<A机IP>:18443/ws' SBX_TOKEN='…' SBX_FINGERPRINT='sha256:…' bash
```

它会:装 `sbx-agent`(校验 sha256)→ 放好 unit → 写 `/etc/sbx/agent.toml`(0600)
→ `enable --now sbx-agent`。给了 `SBX_TOKEN` 就一定是在装被控端,
不用再带 `SBX_TARGET=agent`。

token 丢了或要换,在 TUI 里按 `r` 轮换,把新命令在 B 机上再跑一遍就行 ——
旧配置会自动备份成 `agent.toml.bak`。

<details>
<summary>手工版(想知道它到底做了什么,或者机器连不上 GitHub)</summary>

```sh
# 1. 装。注意选对 arch,以及校验 sha256
curl -fsSLO https://github.com/why1f/sbx/releases/download/v0.2.0/sbx-agent-v0.2.0-linux-arm64
curl -fsSLO https://github.com/why1f/sbx/releases/download/v0.2.0/sbx-agent-v0.2.0-linux-arm64.sha256
sha256sum -c sbx-agent-v0.2.0-linux-arm64.sha256
install -m755 sbx-agent-v0.2.0-linux-arm64 /usr/local/bin/sbx-agent
install -d -m750 /etc/sbx && install -m644 sbx-agent.service /etc/systemd/system/

# 2. 在 A 机上生成 token
#    sbx --config /etc/sbx/config.toml agent-add tokyo    ← token 只显示这一次

cat > /etc/sbx/agent.toml <<'EOF'
server      = "wss://<A机IP>:18443/ws"
token       = "<agent-add 打印的那串>"
fingerprint = "<A 机打印的 sha256:...>"
state_dir   = "/var/lib/sbx-agent"
EOF
chmod 600 /etc/sbx/agent.toml     # 里面有 token

systemctl daemon-reload && systemctl enable --now sbx-agent
```

</details>

```sh
journalctl -u sbx-agent -f
```

期望日志:

```
没有 last-applied.json,等待主控首次 config.apply
已连上主控:agent_id=1,上报间隔 30s,心跳 10s
```

---

## 验证清单

### 1. 握手与下发

```sh
# A 机
sbx --config /etc/sbx/config.toml agent-list      # 应当 online,带 os/arch/版本
sbx --config /etc/sbx/config.toml node-add 1 tokyo-in 443 --protocol vless-reality
# 日志里应当出现「配置版本不一致,下发 config.apply」→「config.apply 已生效」
# B 机上 ss -tlnp | grep 443 应当看到 sbx-agent 在听
```

### 2. 冷启动(agent 不依赖主控)

```sh
# B 机:先停 A 机的 master,再重启 agent
systemctl restart sbx-agent
journalctl -u sbx-agent -n 5
```

期望**先**出现 `已按 last-applied.json 启动 box`,**再**是连主控失败并退避重试。
这条的意义:主控挂了,节点照常服务。

### 3. 跨公网断线重连

```sh
# A 机:停掉 master,看 B 机退避;再拉起来,看它自己回来
systemctl stop sbx && sleep 90 && systemctl start sbx
```

B 机日志应当能看到退避间隔从 1s 翻倍到 60s 封顶,主控回来后**自动重连**,
且不需要重新下发配置(revision 一致)。

### 4. 流量记账(§13.2 的跨机版)

用真客户端连 B 机的节点跑一些流量,然后在 A 机:

```sh
sqlite3 /etc/sbx/sbx.db "SELECT u.name, n.tag, t.cycle_up, t.cycle_down
  FROM user_traffic t JOIN users u ON u.id=t.user_id JOIN nodes n ON n.id=t.node_id;"
sqlite3 /etc/sbx/sbx.db "SELECT * FROM user_traffic_total;"
```

多台 agent 时,`user_traffic_total` 应当等于各节点之和。

### 5. 断连恢复(§13.3 的跨机版)

```sh
# B 机:跑流量期间
kill -9 $(pidof sbx-agent)     # systemd 的 Restart=always 会把它拉回来
```

A 机的 `agent_events` 里应当出现一条 `counter_reset`,
而 `user_traffic.total_up/down` 应当是「重启前 + 重启后」之和 ——
**既不重复计,也不丢失**。这是 §5.2 那套 epoch/delta 的核心。

### 6. 自升级(本仓库唯一未实测的功能)

`agent.upgrade` 的收尾动作是**替换掉自己的二进制然后退出**,
靠 systemd 的 `Restart=always` 拉起新版本 —— 所以这一条必须在 systemd 下测,
手动 `nohup` 起的 agent 升级完就没了。

`sbx-agent.service` 里为此专门开了 `ReadWritePaths=/usr/local/bin`
(其余路径被 `ProtectSystem=strict` 锁着)。不用自升级的话可以删掉那一行,
那时 `agent.upgrade` 会明确报错而不是静默失败。

目前主控侧还没有发起 `agent.upgrade` 的命令,要测得手工构造一条 WS 消息。

---

## 卸载

```sh
systemctl disable --now sbx-agent   # B 机
systemctl disable --now sbx         # A 机
rm -f /usr/local/bin/sbx{,-agent} /etc/systemd/system/sbx{,-agent}.service
rm -rf /etc/sbx /var/lib/sbx-agent
ufw delete allow 18443/tcp
```
