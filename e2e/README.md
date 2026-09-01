# §13.2 / §13.3 端到端验证

**已在真实机器上跑通**(Oracle ARM,Ubuntu 22.04 aarch64,2026-08-01)。
这是 `DESIGN.md` §13 里最后两条验收标准 —— 也是唯一能真正验证
「跨 agent 流量求和」与「计数器重置处理」(§5.2,标注为**核心正确性,不要简化**)的方式。

单元测试和 `spike/` 各自只覆盖一侧:主控的端到端测试对面是测试代码,
agent 对着假主控跑握手。**只有把两半真的连起来,那条增量算法才被执行过一次。**

这个目录不属于 sbx 工作区(自己一个 `go.mod`),和 `spike/` 一样。

## 跑法

```sh
e2e/run.sh              # 建 + 起 + 推流量 + 核对,退出码非 0 即失败
E2E_KEEP=1 e2e/run.sh   # 失败后保留工作目录与各进程日志
```

需要一台 Linux(agent 要读 `/proc`,sing-box 要监听端口),外加 `cargo`、`go`、`sqlite3`。
**CI 每次 push 都真跑这一条**(`ci.yml` 的 `e2e` job),不再只是 `go build`。

`run.sh` 自己起主控 daemon、两个 agent、echo 服务器,推完流量查库断言,
最后无论成败都清理进程。它断言的字节数就是下面两节记下的那几个数 ——
所以 CI 的作用不止「跑一遍」,而是**反复重新推导这份文档声称的数字**。
文档和现实分叉时,红的是 CI,不是用户。

## 手工分解(排查时用)

`run.sh` 挂在某一步时,照下面这些命令一步步来更容易看清卡在哪。
拓扑:

```
driver --socks5--> [客户端 box] --vless-ws--> [agent 的 sing-box] --direct--> echo
                                                      ↑
                                            主控下发的配置在这里生效
```

```sh
# 1. 主控:建库、加两台 agent、各建一个节点、分配给同一个用户
sbx --config master.toml init-db
sbx --config master.toml agent-add tokyo-e2e     # 记下 token
sbx --config master.toml agent-add osaka-e2e
sbx --config master.toml node-add 1 tokyo-in 39501 --protocol vless-ws --path /e2e
sbx --config master.toml node-add 2 osaka-in 39502 --protocol vless-ws --path /e2e
sbx --config master.toml user-add alice --quota-gb 100
sbx --config master.toml user-assign alice 1
sbx --config master.toml user-assign alice 2

# 2. 起主控与两个 agent(agent 配置里填 `sbx fingerprint` 打印的指纹)
sbx --config master.toml daemon &
sbx-agent agent-tokyo.toml &
sbx-agent agent-osaka.toml &

# 3. echo 服务器 + 分别经两台 agent 推不对称流量
go build -tags with_quic,with_utls -o driver .
./driver -serve-echo -echo 39600 &
./driver -socks 39701 -target 39501 -uuid "$UUID" -up 262144 -down 4194304
./driver -socks 39702 -target 39502 -uuid "$UUID" -up 131072 -down 1048576
```

**端口挑高位的。** 这类机器上通常已经跑着 nginx / docker / 别的代理,
撞上去的表现是「agent 起不来」或者更糟 —— 把别人的服务挤掉。

## §13.2:跨 agent 求和

流量落定后(等一个 `report_interval_secs`)查库:

```sql
SELECT ut.node_id, n.tag, ut.cycle_up, ut.cycle_down
  FROM user_traffic ut JOIN nodes n ON n.id = ut.node_id;
SELECT * FROM user_traffic_total;
```

实测结果(逐字节对上,`+8` 是 driver 自己的协议头):

| 节点 | 上行 | 下行 |
|---|---|---|
| tokyo-in | 262152 = 262144+8 | 4194304 |
| osaka-in | 131080 = 131072+8 | 1048576 |
| **视图合计** | **393232** | **5242880** |

两台 agent 各自记各自的账,`user_traffic_total` 等于逐节点手算之和。

## §13.3:断连恢复(epoch / delta)

跑流量期间 `kill -9` 一个 agent 再拉起,验三件事:

1. **冷启动**:agent 起来后先按 `last-applied.json` 把 box 拉起来,**不等握手** ——
   日志里应当先出现 `已按 last-applied.json 启动 box`,再出现 `已连上主控`。
2. **主控记录 epoch 变更**(§5.4):`agent_events` 里出现 `counter_reset`。
3. **不重复计、不丢失**:agent 的 tracker 随进程重启归零,新 `counter_epoch`
   让主控把上报值当**全量**而不是做差(做差会得到负数)。

实测:重启前 `total_up=262152 / total_down=4194304`,重启后再推 `100000 / 500000`:

```
362160 = 262152 + 100008   ✓
4694304 = 4194304 + 500000 ✓
```

一字节不差。`counter_epoch` 从 `f24f3520…` 变成 `0525dee9…`,
`agent_events` 里有对应的 `counter_reset` 记录。

## 顺带验到的

- **TOFU 握手**:agent 只校验指纹、不校验 SAN/CN,wss 连接建立正常(§1.3)。
- **补齐**:agent 带 `revision=0` 连上来,主控下发 `config.apply`,box 起来(§4.1)。
- **公网 IP 自探**:订阅链接里的地址是 agent 自己探到的公网 IP(§7.3),不是 `127.0.0.1`。
- **订阅三种格式**:base64 / Clash YAML / 统计页,在真实部署上都是 200。
- **`/proc` 采集**:`agent_nic_traffic` 里是真实的网卡累计值,`boot_id` 取自
  `/proc/sys/kernel/random/boot_id`(两个 agent 在同一台机器上,所以 boot_id 相同 —— 符合预期)。
