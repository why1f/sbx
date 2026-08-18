# sbx —— 主控(Rust)+ 多被控 agent(Go 内嵌 sing-box)集群管理 TUI

> 本文件记录**当前仍生效的架构约束与正确性理由**,不是实施待办。
> §0 与 §1 保存了已经通过源码阅读、真实 sing-box 与跨机测试验证的选型结论。
> 修改流量采集、配置权威、许可证边界或 TLS 模型前,先读对应理由。

---

## 0. 背景与已验证的前置结论

### 0.0 项目定位

sbx 解决的是**一台主控管理多台 agent**的问题:

1. 主控与 agent 通过 WebSocket 通信；
2. 一个用户可分配来自不同 agent 的多个节点,用量跨 agent 求和；
3. 主控 TUI 管理 agent、节点、用户、订阅、通知与升级；
4. agent 上报 `/proc/net/dev` 网卡累计与 per-user/inbound 流量；
5. 主控 Rust、agent Go 并把 sing-box 作为库静态内嵌。

主控/agent 的语言边界不是偏好:per-user 累计依赖进程内 `adapter.Router` tracker,
Rust 主控无法访问 Go 对象。完整推论见 §0.2 / §0.3。

### 0.1 sing-box 的四个**外部** API 面(已逐一核实)

这一节的结论是:**四个外部 API 面没有一个适合本项目。** 它保留在这里是为了防止有人重新提议它们。
真正采用的方案在 §0.2——它不走任何 API 面。

| API 面 | 形态 | 默认构建包含 | 能否提供 per-user **累计**流量 |
|---|---|---|---|
| **Clash API**(experimental) | HTTP REST + WebSocket | ✅ 有 | ❌ `/connections` 只返回**当前活跃连接**。两次轮询之间建立又关闭的短连接完全丢失 → 不可用于计费级记账 |
| **V2Ray API**(experimental) | gRPC | ❌ 需 `with_v2ray_api` 构建标记(官方文档原文:「默认安装不包含 V2Ray API」) | ✅ 计数在进程生命周期内单调累计。统计名格式 `user>>>NAME>>>traffic>>>uplink\|downlink` |
| **API service**(`services` 里 `"type": "api"`,1.14.0 新增) | gRPC + gRPC-Web + `@improbable-eng/grpc-web` WS 传输 | ✅ 有 | ❌ 1.14.0 changelog 与配置文档里**没有任何 statistics / traffic metrics 条目** |
| **SSM API**(`"type": "ssm-api"`,1.12.0 新增) | **RESTful**(不是 gRPC) | ✅ 有 | △ 确实有 per-user 流量,但**仅限 Shadowsocks**,且目标 SS inbound 需开 `managed`。8 协议里只覆盖 1 个 → 不满足需求 |

补充事实(供后续判断,不必重新验证):

- **V2Ray API 没有被废弃。** 1.14.0 changelog 中不含任何废弃声明。它只是不在默认构建里。
- **API service 的配置面**只有:`secret`(默认空 = 不鉴权;鉴权方式为 `Authorization: Bearer <secret>`)、
  `access_control_allow_origin`(默认 `*`)、`access_control_allow_private_network`(默认 false)、
  `dashboard`{`enabled`, `path`, `download_url`, `http_client`, `update_interval` 默认 `1d`}、`tls`。
  它暴露的能力是:服务状态、日志、出站分组、Clash mode、连接追踪、网络质量/STUN/Tailscale 操作。**没有统计。**

### 0.2 采用的方案:`Router().AppendTracker()`(读源码核实)

> 核实基准:`github.com/SagerNet/sing-box` tag **`v1.14.0-beta.3`**,commit `1569c529`,
> 关键行在默认分支 **`testing`** 上逐条复核一致。
> **该仓库没有 `main` 也没有 `dev-next` 分支**,默认分支是 `testing`;别再去找 `main`。

**统计不走任何 API 面,而是用 sing-box 的公开接口把自己的计数器挂到数据路径上。**

```go
// adapter/router.go:29 —— AppendTracker 挂在导出的 Router 接口上
type Router interface {
    AppendTracker(tracker ConnectionTracker)
    ...
}

// adapter/router.go:104 —— 只有三个方法
type ConnectionTracker interface {
    RoutedConnection(ctx, conn net.Conn, metadata InboundContext, matchedRule Rule, matchOutbound Outbound) net.Conn
    RoutedPacketConnection(ctx, conn N.PacketConn, metadata InboundContext, matchedRule Rule, matchOutbound Outbound) N.PacketConn
    RoutedFlow(ctx, metadata InboundContext, matchedRule Rule, matchOutbound Outbound) tun.FlowTracker
}

// box.go:661 —— Router 可从 Box 取到
func (s *Box) Router() adapter.Router { return s.router }
```

三条决定性的事实:

1. **调用点无条件编译。** `route/route.go:166`(以及 `:294` 收包、`:510-513` TUN flow)所在文件
   **没有任何 `//go:build` 行**。`route/` 与 `common/trafficcontrol/` 下仅有的构建约束是
   `darwin`/`linux` 的 GOOS 限定。所以自己注册的 tracker **一定收得到流量**。
2. **用户名在数据路径上直接可得。** `adapter.InboundContext.User string`(`adapter/inbound.go:54`),
   由 inbound 鉴权后填入。已确认覆盖 vless / vmess / trojan / hysteria2 / tuic / anytls /
   shadowsocks-multi,以及 http / mixed / socks / naive / snell / shadowtls。
3. **不需要预先声明用户名单。** 上游 v2rayapi 的实现只统计 `stats.users` 里列出的用户
   (`experimental/v2rayapi/stats.go:64` 的 `countUser := user != "" && s.users[user]`);
   自己的实现可以无条件对所有 `User != ""` 建账 → 旧项目 `sync_v2ray_api_users` 那套名单同步逻辑**整个不需要**。

于是全部满足:**零 fork、零构建标记、零配置项、零监听端口。**

**为什么不用 V2Ray API(即使它能给出正确的数字):**

- `box.go:162` 是硬 gate:`experimentalOptions.V2RayAPI != nil && ...Listen != ""` 才会构造 V2RayServer,
  且 `box.go:431-440` 还要 `v2rayServer.StatsService() != nil` 才挂 tracker。
  也就是**必须在配置里填一个 listen 地址**,哪怕实现根本不打算监听。`AppendTracker` 没有这个 gate。
- 要维护 `stats.users` 名单(见上第 3 点)。
- 走的是 `experimental` 子系统;`AppendTracker` 走的是 `adapter.Router` 公开接口——后者更稳定,不是更不稳定。
- 最根本的:**V2Ray API 解决的是「跨进程取统计」,而 sbx 里 sing-box 和 agent 是同一个进程。**
  给同一个进程内的自己搭一套 gRPC 再连回自己,没有意义。

> 另一条能走通但更差的路(记录备查,**不要采用**):`experimental.RegisterV2RayServerConstructor` 是导出函数,
> 且 `include/v2rayapi_stub.go` 的 stub 是在 `init()` 里注册到一个**单槽变量、后写覆盖**(无锁、无重复注册报错),
> 所以第三方可以在 `main()` 里覆盖它。但它同样吃 `box.go:162` 的 listen gate,相比 `AppendTracker` 没有任何好处。

### 0.3 由 §0.1 + §0.2 推出的三条结论

**结论一:agent 用 Go 写,把 sing-box 作为 Go 库静态内嵌。** 于是:

- 不需要 `with_v2ray_api` 构建标记,但发行 agent 时必须带 `with_quic,with_utls`:
  reality 依赖 uTLS,Hysteria2/TUIC 依赖 QUIC。
- 不需要 gRPC(统计对象就在同一块内存里,不起任何监听)
- 不需要自编译内核 CI、内核分发仓库、TUI 里的内核安装/升级按钮
- 不需要旧项目 `core/singbox.rs`(810 行)的**外部进程管理**(启停、配置文件落盘给别的进程读、`sing-box check` 子进程)
- 不需要旧项目 `core/grpc.rs` + `build.rs` + `proto/` + `tonic`/`prost`/`protoc-bin-vendored` 依赖

sbx 的主控 `Cargo.toml` 相对旧项目**删掉** `tonic`、`prost`、`[build-dependencies]` 整段,以及 `build.rs` 和 `proto/` 目录。

**结论二:「Rust agent + sing-box 独立进程」已被排除。**
注意排除它的理由**不是**「维护内核 CI 太麻烦」——CI 无论如何都要建(§11.1),那条论证站不住,别再用它。
真正的理由是:

| | Rust agent + 独立 sing-box 进程 | Go agent 内嵌 |
|---|---|---|
| **取统计** | **拿不到 `adapter.Router`,只能退回 V2Ray API + gRPC**,吃尽 §0.2 列出的全部代价 | 内存读取 |
| 产物 | 2 个二进制,每台机要版本配对 | 1 个 |
| 进程管理 | `core/singbox.rs` 810 行原样回来 | 无 |
| 配置下发 | 必须落盘给另一个进程读,§4.2 的原子 apply 更复杂 | 内存里传 `option.Options` |
| 依赖与攻击面 | `tonic`/`prost`/`protoc` + 本机 gRPC 监听端口 | 无 |
| **动态改用户**(§7.5) | 得先给内核补进一套管理 RPC 才能驱动 | 一次 Go 方法调用 |

**结论三:不 fork sing-box、不打补丁。** sing-box 版本就是 `go.mod` 里的一行。
八种协议的运行时改用户能力确实被挡在未导出字段后面(§7.5 有完整核实结果),
但绕开它的代价比 fork 小得多。触发重新考虑的条件写在 §7.5 末尾。

---

## 1. 三个结构性决策(已定,可推翻,但推翻前请读理由)

### 1.1 许可证:双许可,边界在 WebSocket 上

- `agent/` 是**独立的 Go module,GPLv3**。静态链接 sing-box 使 agent 构成其衍生作品。
- `master/` 与 `shared/` 保持 **MIT**。它们与 agent 之间**只有 WebSocket 网络边界**,不构成链接,GPL 不传染。
- README 顶部**显式标注**双许可及这条边界的理由。这是一个必须写出来的事实,不是可选的礼节。

### 1.2 配置权威:主控是唯一真源

agent 本地**只保留一份 `last-applied.json` 快照**,用途**仅限**:主控离线时重启 agent,仍能拉起同样的代理服务。

- 快照**不接受本地编辑**。启动时若发现快照被改动,只记一条 warn 并照用,**不上报为冲突**、不尝试合并。
- 理由:需求 §0.0(3) 允许一个用户跨 agent 拥有多个节点。一旦配置分权到 agent,「用户 → 节点」这个关系就**无处仲裁**——
  两个 agent 各自认为自己持有该用户的权威配置时,没有任何一方有全局视图。配置权威必须唯一,且必须在主控。
- 这条决策的直接收益见 §6.3(配额超限时的处理路径)。

### 1.3 WS 安全:TOFU 指纹固定,不做 CA 体系

- agent 侧默认 `wss://` + `Authorization: Bearer <token>`。
- 主控内置 rustls **自签证书**;agent **首次连接时固定证书指纹**(TOFU pinning)。
  `token` 与 `fingerprint` 一起写进 agent 配置文件。
- **证书由主控自己生成,不需要用户准备。** daemon 启动时若 `cert_path` / `key_path`(§11.2)不存在,
  就用 `rcgen` 生成一张长有效期自签证书写进去,权限 `0600`;已存在则直接加载。
  SAN 取 `cluster.listen` 的地址加 `localhost`——**但 agent 侧只校验指纹、不校验 SAN/CN**,
  所以主控改域名或 IP 都不需要重签(TOFU 的本意就是把信任锚定在密钥而非名字上)。
- 主控启动后计算证书 DER 的 **SHA-256** 作为指纹(`sha256:<hex>`),
  §8.1 新增 agent 的弹窗里直接把它填进一键安装命令的 `SBX_FINGERPRINT`。
  `sbx fingerprint` 子命令也打印它,便于手工排查。
- 同时支持 `ws://` 明文模式,**仅当** agent 配置里显式写 `insecure = true`(留给「nginx 已做 TLS 终止」的部署)。
- **不做 CA 体系**、不做证书轮换流程、不做双向 mTLS。规模不匹配。
  换证书的流程就是删掉这两个文件重启,然后更新各 agent 的 `fingerprint`——低频操作,不值得为它建体系。

---

## 2. 非目标(明确不做)

沿用旧项目 `ROADMAP.md` 的边界,并按用户确认加强:

- **不做 Web 面板。** 唯一的例外是**订阅 HTTP 监听**(§10)——它是订阅链接必须的,不构成面板。
  没有管理 UI 的 HTTP 端点、没有前端资源、没有 REST 管理 API。管理入口只有 CLI 与 TUI。
- **不做复杂 RBAC / 多租户。** 主控只有一个操作者。
- **不做平台化后端。** 不做插件系统、不做多内核抽象层。
- **不做「花哨但收益低」的 TUI 鼠标交互。** 键盘优先。
- **不做 agent 侧的业务判断。** agent 不做删档、不做月重置、不做配额判定,只上报原始累计值(§5.3)。

定位:可发行 / 可自用小规模 / CLI + TUI + supervisor 运维闭环为核心。
主控仍只支持 systemd；agent 支持 systemd 与 OpenRC。

---

## 3. 工作区布局

| 路径 | 当前职责 |
|---|---|
| `master/` | Rust 主控:CLI、daemon、TUI、SQLite、订阅、Telegram、升级、doctor |
| `agent/` | Go agent:WebSocket 客户端、内嵌 sing-box、tracker、sysinfo、自升级 |
| `shared/` | Rust 协议信封与 payload 类型 |
| `packaging/` | 安装脚本、示例配置、systemd/OpenRC service 与离线测试 |
| `master/testdata/` | 八协议与出站策略 golden,由真 sing-box 校验 |
| `spike/` | tracker 的真实 sing-box 流量/拒绝行为回归 |
| `e2e/` | 跨 agent 求和与断线恢复驱动 |
| `.github/workflows/` | Rust/Go/安装/端到端 CI 与双架构发布 |

二进制名:主控 `sbx`,agent `sbx-agent`。`agent/` 不允许出现 `replace`、`patches/`
或内核构建脚本；sing-box 版本只由 `agent/go.mod` 的普通依赖决定。

## 4. WS 协议

传输:WebSocket **text frame,JSON**。信封:

```json
{ "v": 1, "id": "seq-或-uuid", "kind": "req|resp|event",
  "method": "config.apply", "payload": {}, "error": null }
```

- `kind: "req"` **必须**带 `id`;对端回同 `id` 的 `resp`。
- `kind: "event"` 单向,**无** `id`,不需要回应。
- `v` 不匹配即**断开连接**,并在主控 TUI 标红「协议版本不符」。不做协议协商、不做向下兼容层。

`shared/` crate 定义信封与全部 payload struct(serde),master 直接依赖它;
agent 侧手写对应的 Go struct(两边各一份,靠 §12 的联调测试保证一致)。

### 4.1 握手

agent 连上后的**第一帧必须**是 `agent.hello`;10 秒内没收到就断开。

| 方向 | method | payload |
|---|---|---|
| A→M | `agent.hello` | `token`, `agent_version`, `proto_version`, `os`, `arch`, `hostname`, `boot_id`, `singbox_version`, **`config_revision`**, **`user_state_revision`**(后两者取自本地 `last-applied.json`,从未落过盘时为 0) |
| M→A | `agent.hello_ack` | `agent_id`, `server_time`, `heartbeat_secs`, `report_interval_secs`, `config_revision`, `user_state_revision` |

**两个 revision 分别比对、分别补齐**(缺一不可):

| 比对项 | 不一致时主控立刻下发 | 是否重建 box |
|---|---|---|
| `config_revision` | `config.apply` | 是 |
| `user_state_revision` | `user.state` | 否,只翻内存标记 |

这就是 §1.2「配置权威归主控」在重连路径上的落地点——agent 离线期间错过的变更,在握手时自动补齐。

**这两个计数器必须独立递增,不能合并成一个。**
若把禁用状态挤进 `config_revision`:「离线期间某用户超配额」会让 agent 重连时走一次全量 `config.apply`,
§6.3 / §7.5 辛苦换来的「不重建 box」在**最频繁的那条路径上**又丢掉了。
若只比对 `config_revision` 而不管禁用状态:离线期间的禁用**永远不会生效**——
超配额用户重连后继续跑,这是计费漏洞,且不会有任何报错提示你。

**重复连接的驱逐:** 同一 `agent_id` 的新连接握手成功时,`registry.rs` **立刻关闭并丢弃旧的 `AgentConn`**
(last-write-wins),再登记新连接。`kill -9` 后重连(§13.3)与网络分区恢复都会走这条路径:
旧连接的 TCP 可能几分钟后才超时,不主动驱逐就会出现「两个 conn 同时在 registry 里,下发命令随机走一条」。
被驱逐的旧连接**不写 `agent_events`**——这是正常现象,不是故障。

### 4.2 主控 → agent

| method | payload | resp |
|---|---|---|
| `config.apply` | `revision`, `options`(完整 sing-box 配置) | `ok` / `error`(check 失败原文) |
| `config.check` | `options` | 同上,**不落盘** |
| **`user.state`** | **`user_state_revision`, `disabled: [name, ...]`(禁用用户全量名单)** | **`ok`** |
| `box.restart` | — | `ok` |
| `box.status` | — | `running`, `since`, `pid_rss` |
| `stats.pull` | — | 同 `stats.report` 的 payload |
| `sysinfo.pull` | — | 同 `sysinfo.report` 的 payload |
| `agent.upgrade` | `url`, `sha256` | `ok`(随后 agent 主动断连重启) |

`stats.pull` **没有 `reset` 参数**,这是刻意的——理由见 §5.3。

**`config.apply` 语义(严格按此顺序):**
`box.New()` 构造成功(= 校验通过,§7.4)→ 写 `last-applied.json.tmp` → `rename` 原子替换 → 切换到新 box。
任一步失败:**保持旧配置继续运行**并回 `error`;主控保留旧 revision,**不标记为成功**。

**不存在「配置半应用」状态,但存在一个亚秒级的服务中断窗口**——
因为新旧 box 抢同一批监听端口,必须先 `Close()` 旧的再 `Start()` 新的(§7.4)。

**`user.state` 语义:** 传**全量**禁用名单(不是增量),agent 直接替换 `Tracker.disabled` 集合,
连同 `user_state_revision` 一起写进 `last-applied.json`——**只改这两个字段,不动 `options`、不重建 box**(§7.5)。
全量的理由:幂等,丢一条消息不会导致状态永久偏移;名单规模是用户数量级,开销可忽略。
**不影响其他用户的连接**(§7.5)。

`user_state_revision` 与 `config_revision` 是**两个独立递增的计数器**,握手时分别比对(§4.1)。

### 4.3 agent → 主控

| method | payload |
|---|---|
| `stats.report` | `counter_epoch`, `users: [{name, tag, up, down}]` |
| `sysinfo.report` | `boot_id`, `nic: {rx, tx}`, `cpu_pct`, `mem_used`, `mem_total`, `load1`, `uptime_secs` |
| `box.event` | `state`, `message` |
| `log` | `level`, `line`(节流,仅 warn 以上) |
| `pong` | `echo_ts` |

上报周期默认 **30s**(沿用旧项目 `stats.sync_interval_secs`),心跳 **10s**。

`stats.report` 的每条记录由 **`(name, tag)` 二元组**标识:`name` 是代理用户名,`tag` 是承载它的 inbound tag。
主控据此定位 `user_traffic` 的 `(user_id, node_id)` 主键(§6.1)。
**只报 `name` 是不够的**——§0.0(3) 允许一个用户在**同一台** agent 上拥有多个节点,
少了 `tag`,这些节点的流量会塌成一个数字,按节点归属入账就永久做不到了。

**ingest 必须容忍未知条目。** tracker 的计数表只增不减(§7.1),所以用户或节点在主控侧被删除后,
agent 仍会继续上报它们,直到 agent 进程重启为止。
`cluster/ingest.rs` 对**匹配不到 `(user_id, node_id)` 的条目直接丢弃**:不建行、不报错、也不记 warn
(否则每 30s 刷一条日志)。

**agent 侧不做任何删档 / 月重置 / 配额判断,只报原始累计值。**

---

## 5. 计数器重置处理(核心正确性,不要简化)

### 5.1 问题

累计计数会归零的情况:

1. ~~sing-box 热重载(`box.New()` 重建)~~ ← **不再归零,见 §5.2 的说明**
2. agent 进程重启
3. 机器重启(网卡计数器清零)

如果主控天真地做 `delta = new - last`,以上任一情况都会得到**负数或巨大的错误增量**。

### 5.2 算法

```
epoch 相同 且 new >= last   →   delta = new - last
其他任何情况                →   delta = new        (视作新纪元的首次上报)
```

- **用户流量**的 epoch 是 `counter_epoch`:**agent 进程启动时生成一个 UUID,进程存活期间不变**。
- **网卡流量**的 epoch 是 `boot_id`:取自 `/proc/sys/kernel/random/boot_id`。

> **为什么 `counter_epoch` 不跟着 `box.New()` 走**(这是 §0.2 的直接收益,别改回去):
> 计数器活在 **agent 自己的 tracker 对象**里,不归 box 所有(§7.1)。
> box 重建时把**同一个 tracker 实例**重新 `AppendTracker` 上去即可,那张 `map[user]*counter` 原样存活。
> 于是配置变更、加删用户、热重载**都不会**造成计数跳变——
> 而这些恰恰是本系统里最频繁的操作。epoch 变更被压缩到只剩「进程重启」和「机器重启」两种真实事件。

### 5.3 为什么 agent 不做 reset

**agent 上报单调累计值,永不 reset;delta 由主控计算。**

这是对旧项目的一处**有意的修正**。旧项目 `core/grpc.rs::query_all_traffic(reset = true)` 在读取的同时清零内核计数器——
一旦这个响应在网络上丢失并重传,**被清掉的那一段流量就永久丢失了**。
单机时这是小概率;N 个 agent 跨公网时,这个概率乘以 N 且持续存在。

agent 保持无状态更安全:主控没收到就重算,agent 那边什么都没变。

(注:sbx 的 tracker 是自己实现的,`reset` 这个参数从一开始就不存在。这一节保留是为了防止有人「优化」出一个 reset 语义。)

### 5.4 epoch 变更必须可见

主控收到 epoch 变更时,**写一条 `agent_events` 审计记录**,TUI 显示「计数器已重置」,
**而不是静默吞掉**。旧项目在 gRPC 重连的状态一致性上踩过同类问题——静默的状态跃迁会让「流量数字不对」这类
bug 变得无法归因。

因为 §5.2 已经把 epoch 变更压到只剩进程/机器重启,这条审计记录现在是**低频且高信息量**的:
它出现就意味着真的发生了重启。若发现它频繁出现,说明 §7.1 的 tracker 生命周期写错了(被 box 连带重建了),这是个 bug 信号。

---

## 6. SQLite 表结构

迁移方式沿用旧项目:**线性迁移 + `PRAGMA user_version`,每版一个事务,
且必须在建连接池之前用一条独立连接跑完**。
(理由:池化连接会缓存 ALTER 之前的 schema。这是旧项目 `src/db/mod.rs` 已经处理好的坑,照搬。)

### 6.1 建表

```sql
-- 001_init.sql
CREATE TABLE agents (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  token_hash TEXT NOT NULL,          -- sha256(token) 十六进制;明文只在生成时显示一次(§8.1)
  token_prefix TEXT NOT NULL,        -- 前 8 位,列表里给人识别 + 校验时索引定位
  status TEXT NOT NULL DEFAULT 'never',   -- never|online|offline
  last_seen INTEGER,
  agent_version TEXT, singbox_version TEXT,
  os TEXT, arch TEXT, hostname TEXT,
  ipv4 TEXT, ipv6 TEXT,              -- agent 自探上报,主控可手工覆盖
  nic_quota_bytes INTEGER,           -- NULL = 不限流量
  nic_reset_day INTEGER,             -- NULL = 无需重置;否则 1..31
  config_revision INTEGER NOT NULL DEFAULT 0,      -- 配置(options)版本
  user_state_revision INTEGER NOT NULL DEFAULT 0,  -- 禁用名单版本,独立递增(§4.1)
  created_at INTEGER NOT NULL
);

CREATE TABLE agent_nic_traffic (
  agent_id INTEGER PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
  boot_id TEXT,
  last_rx INTEGER NOT NULL DEFAULT 0, last_tx INTEGER NOT NULL DEFAULT 0,
  cycle_rx INTEGER NOT NULL DEFAULT 0, cycle_tx INTEGER NOT NULL DEFAULT 0,
  cycle_start INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE nodes (
  id INTEGER PRIMARY KEY,
  agent_id INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  tag TEXT NOT NULL,
  protocol TEXT NOT NULL,
  listen_port INTEGER NOT NULL,
  params_json TEXT NOT NULL,         -- server_name / path / port_reuse / ipv6 / relay
  UNIQUE(agent_id, tag)
);

CREATE TABLE users (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  uuid TEXT NOT NULL, password TEXT NOT NULL,
  quota_bytes INTEGER NOT NULL DEFAULT 0,      -- 0 = 不限
  traffic_multiplier REAL NOT NULL DEFAULT 1.0,
  expire_at INTEGER,
  enabled INTEGER NOT NULL DEFAULT 1,
  reset_day INTEGER,
  sub_token TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL
);

-- 一个用户 ↔ 多个节点;节点可来自不同 agent
CREATE TABLE user_nodes (
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  PRIMARY KEY(user_id, node_id)
);

-- 每 (用户, 节点) 一行;跨 agent 求和得总量
CREATE TABLE user_traffic (
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  counter_epoch TEXT,
  last_up INTEGER NOT NULL DEFAULT 0, last_down INTEGER NOT NULL DEFAULT 0,
  cycle_up INTEGER NOT NULL DEFAULT 0, cycle_down INTEGER NOT NULL DEFAULT 0,
  total_up INTEGER NOT NULL DEFAULT 0, total_down INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY(user_id, node_id)
);
CREATE INDEX idx_user_traffic_user ON user_traffic(user_id);

CREATE TABLE agent_events (
  id INTEGER PRIMARY KEY,
  agent_id INTEGER,
  kind TEXT NOT NULL,
  message TEXT NOT NULL,
  at INTEGER NOT NULL
);
```

### 6.2 跨 agent 的用户总流量

这是需求「该用户下各节点流量统计之和」的落地:

```sql
CREATE VIEW user_traffic_total AS
SELECT u.id AS user_id, u.name,
       COALESCE(SUM(t.cycle_up),   0) AS cycle_up,
       COALESCE(SUM(t.cycle_down), 0) AS cycle_down,
       COALESCE(SUM(t.total_up),   0) AS total_up,
       COALESCE(SUM(t.total_down), 0) AS total_down
FROM users u
LEFT JOIN user_traffic t ON t.user_id = u.id
GROUP BY u.id;
```

`LEFT JOIN` + `COALESCE` 是必须的:没有任何流量记录的用户也要出现在结果里(值为 0),否则用户列表会漏行。

### 6.3 配额与到期

配额判定用 `(cycle_up + cycle_down) * traffic_multiplier` 对比 `quota_bytes`
(复用旧 `model/user.rs::used_total_bytes` / `quota_used_percent`)。

超额或到期时,主控**只改 `users.enabled`**,再向**所有承载该用户节点的 agent** 下发 **`user.state`**
(全量禁用名单,§4.2)——**不是 `config.apply`**。
某个 agent 此刻离线,就把它的 revision 留在旧值,等它重连握手时(§4.1)自动补齐。

这条路径**不重建 box**,所以:不掉其他用户的连接、不产生 epoch 变更、不写「计数器已重置」审计。
考虑到这是全系统触发最频繁的自动化路径(每 30s 同步都可能命中),这个差别是实质性的。

**这就是 §1.2 配置权威归主控的直接收益**——禁用一个跨 3 台服务器的用户是一次数据库写 + N 次下发,
且对离线节点天然收敛;如果配置分权在 agent,这个操作没有一致的完成时刻。

写并发沿用旧项目的 `BEGIN IMMEDIATE` 作跨进程写锁(daemon 与 TUI 会同时在跑)。

### 6.4 网卡流量与配额(与用户流量互不相干)

`agent_nic_traffic` 走完全独立的一套:

- 数据来自 agent 的 `sysinfo.report`(§7.2),epoch 是 `boot_id`
- delta 算法与用户流量**同一个**(§5.2),累进 `cycle_rx` / `cycle_tx`
- 配额在 `agents.nic_quota_bytes`,`NULL` = 不限流量;重置日在 `agents.nic_reset_day`,`NULL` = 无需重置
- **口径是整机物理网卡,与 VPS 服务商面板对齐**,和 `user_traffic` 不是同一套数字(§7.2 的警告)

#### 记账口径:四选一,而���可以随时改回去

厂商按什么收钱各家不同,所以「本周期用了多少」由 `agents.nic_accounting_mode` 决定
(迁移 010,默认 `sum` = 升级前的行为)。**方向站在被控机看**,与 `/proc/net/dev` 一致:
出站 = Transmit = 机器**发出**去的字节(代理场景里就是「服务器 → 客户端」那一段,
也就是客户端那边看到的下载);入站 = Receive = 机器收进来的字节。

| 取值 | 界面上的说法 | 用量 | 典型场景 |
|---|---|---|---|
| `sum` | 入出总计 | `rx + tx` | 双向都计费 |
| `outbound` | 仅出站(TX) | `tx` | 只按出网计费(多数云厂商) |
| `inbound` | 仅入站(RX) | `rx` | 只按入网计费 |
| `max` | 入出取大 | `max(rx, tx)` | 按较大的那个方向计费 |

**入库的永远是两个方向的原始累计值,模式只在读取时投影。**
这条不能倒过来 —— 在 ingest 里按模式先算一遍的话:

1. 改一次模式就要重算历史,而历史已经被合并成一个数,算不回来了;
2. `max` 会退化成「逐次上报取大再求和」,那和「整周期累计取大」是两个不同的数
   (方向优势在周期中途反转时就会分叉)。

投影放在读取侧,切换模式就只是换一个算式:当前周期立刻按新口径重算,
原始计数、`cycle_start`、`last_reset_ym` 一个都不动,改错了切回去即可。

模式**不推进任何 revision**:它不进 sing-box 配置,agent 根本不需要知道。
月重置照旧只清零两个原始 `cycle_*`,保留 delta 基线与 `boot_id`(§5.2);清零之后
四种口径自然都是 0。网速仍按原始方向显示,不受口径影响 —— 它回答的是
「这台机器现在忙不忙」,不是「这个月要付多少钱」。

绑定了网卡的订阅(§10.3)用**同一份**投影,逐台算完再汇总:
`sum` → `(upload=tx, download=rx)`,`outbound` → `(tx, 0)`,`inbound` → `(0, rx)`,
`max` → 把较大的那个方向留在它自己的位置、另一个归零。于是每台的
`upload + download` 恒等于它计入的用量,几台混着不同口径也能直接相加。


**网速不落库。** 由主控在内存里用相邻两次 `sysinfo.report` 的 rx/tx 差值 ÷ 时间间隔算出(§8.2)。
`boot_id` 变化的那一次显示 `--`,否则会算出一个爆炸数字。

---

## 7. agent 侧实现

### 7.1 per-user 累计统计(`AppendTracker`)

机制与核实依据见 §0.2。agent 持有**一个长生命周期的 tracker 对象**,它**不归 box 所有**:

```go
// key 必须是 (用户, inbound tag) 二元组,不能只用用户名 —— 理由见下方「为什么 key 要带 tag」
type ctrKey struct{ user, tag string }

type Tracker struct {
    mu       sync.RWMutex
    counters map[ctrKey]*counter    // (username, inbound tag) -> 计数器
    disabled map[string]struct{}    // 被主控禁用的用户(§7.5);禁用是按用户,与 tag 无关
}
type counter struct{ up, down atomic.Int64 }

func (t *Tracker) RoutedConnection(ctx context.Context, conn net.Conn,
        md adapter.InboundContext, rule adapter.Rule, out adapter.Outbound) net.Conn {
    if md.User == "" { return conn }                  // 未鉴权 inbound,不建账
    if t.isDisabled(md.User) { return rejected(conn) } // §7.5
    c := t.counter(ctrKey{md.User, md.Inbound})        // 无条件建账,不需要预列名单
    return bufio.NewInt64CounterConn(conn, []*atomic.Int64{&c.up}, []*atomic.Int64{&c.down})
}
// RoutedPacketConnection 同理;RoutedFlow 非 tun 场景返回 nil
```

**为什么 key 要带 tag:** §0.0(3) 允许一个用户拥有多个节点,**且其中几个可以在同一台 agent 上**。
若 key 只有用户名,同机多节点的流量会合并成一个数字,而 `user_traffic` 的主键是 `(user_id, node_id)`(§6.1)、
`stats.report` 的记录是 `(name, tag, up, down)`(§4.3)——**填不进去**。
这不是能事后补救的取舍:合并发生在计数阶段,数据一旦塌掉就无法拆回。

> ✅ **`md.Inbound` 已由 §12.0 的 spike 在 sing-box v1.14.0-beta.3 上实测确认。**
> 已核实的是 `adapter.InboundContext.User string`(`adapter/inbound.go:54`);
> inbound tag 的字段名确实是 `md.Inbound`,且填的是**配置里的 tag 字符串**
> （实测 `md.Inbound == "vless-in"`，与配置一致；另有 `md.InboundType == "vless"` 是协议名，不是 tag）。
> 同一次 spike 还确认了方向：`NewInt64CounterConn(conn, read, write)` 的 read 是上行、write 是下行，
> 与上游 `service/ssmapi/traffic.go:116` 的用法一致。

挂载(**顺序不能变**):

```go
ctx := include.Context(context.Background())   // ← 少了这行 box.New 直接报 "missing inbound registry in context"
b, err := box.New(box.Options{Context: ctx, Options: opts})
b.Router().AppendTracker(tracker)              // 必须在 Start() 之前
b.Start()
```

要点:

- **只增不减**,在 **agent 进程**生命周期内单调(不是 box 生命周期,§5.2)。
  **删除的用户/节点不会从表里消失**,agent 会一直报到进程重启为止 → 主控侧丢弃未知条目(§4.3)
- `stats.report` 直接快照全表,**不 reset**(§5.3)
- box 重建时**复用同一个 tracker 实例**——这是 §5.2 成立的前提
- `AppendTracker` 内部是 `r.trackers = append(...)`,**无锁**(`route/router.go:272`)。
  所以只能在 `Start()` 前调用,**绝不能**在有流量时并发调用
- `bufio.NewInt64CounterConn` 收 `[]*atomic.Int64`,所以计数器用 `atomic.Int64` 而非 `Uint64`

预估约 **140 行**。

### 7.2 网卡流量(`/proc/net/dev`)

**与 sing-box 完全无关**,读的是内核 `/proc`,所以 §0 的任何决策都碰不到它。
把旧项目 `src/core/sysinfo.rs::read_net()` 的语义移植到 Go:

1. 读 `/proc/net/dev`,**跳过表头两行**
2. 按 `:` 切分出接口名与数值区
3. **过滤虚拟网卡**,前缀黑名单:
   `lo` `docker` `veth` `br-` `virbr` `tun` `tap` `wg` `sing-box` `utun` `kube` `cni` `flannel` `tailscale` `zt`
4. 累加数值区的 **field 0(RX bytes)** 与 **field 8(TX bytes)**

过滤理由(照抄旧项目的注释语义):**容器网桥会让同一份流量在物理网卡和虚拟网卡上各记一次**,
不过滤就会双计。这直接决定了口径能否与 VPS 服务商面板对上。

> **网卡流量与用户代理流量是两套不同的数字,不要试图对齐。**
> 用户跑 10 GB 代理流量,网卡上大约会记到 20 GB(进站一次 + 出站一次)。
> 前者用于给用户计费(`user_traffic`),后者用于对齐 VPS 服务商的流量配额(`agent_nic_traffic`)。

### 7.3 其它系统信息

- CPU:采样 `/proc/stat`,相邻两次差分算占用百分比
- 内存:`/proc/meminfo`
- `boot_id`:`/proc/sys/kernel/random/boot_id`(§5.2 的网卡 epoch)
- 公网 IP 自探:把旧项目 `node_service.rs::get_server_ips()` 那套搬到 agent 侧——
  `api4.ipify.org` / `api6.ipify.org` 并发探测、3s 超时、`ifconfig.me` 兜底、600s 缓存。
  结果随 `agent.hello` 上报,主控写入 `agents.ipv4/ipv6`,**并允许手工覆盖**。

### 7.4 sing-box 生命周期

`boxctl/`:用 `option.Options` 类型化结构体构建配置 → `box.New()` → `Start()` / `Close()`。

**热重载顺序(不能写成「先起新的再关旧的」——两个 box 抢同一批监听端口,必然 EADDRINUSE):**

```
box.New(新配置)                       ← 构造期校验,不绑端口。这是 `sing-box check` 子进程的进程内替代
  ├ 失败 → 旧 box 原样跑着,回 error,主控保留旧 revision   ✅ §4.2 的回滚语义在这里保住
  └ 成功 → old.Close()
           → new.Router().AppendTracker(同一个 tracker 实例)   ← 别忘了,忘了统计就断了
           → new.Start()
                ├ 成功 → 落盘 last-applied.json
                └ 失败 → 用旧配置重新 New + AppendTracker + Start,回 error
```

`counter_epoch` **不在这里生成**——它在 agent 进程启动时生成一次(§5.2)。

`Close()` 有幂等保护,第二次调用返回 `os.ErrClosed`。

### 7.5 用户启停:走内存,不重建 box

**已核实的事实(v1.14.0-beta.3 / `testing`):sing-box 不支持运行时改用户,八个协议里只有 Shadowsocks 多用户例外。**

| 协议 | inbound 上有导出的 `UpdateUsers`? | 底层库支持? |
|---|---|---|
| vless-reality / vless-ws | ❌ | ✅ `vless.Service[T].UpdateUsers` |
| vmess-ws | ❌ | ✅ |
| trojan | ❌ | ✅ |
| **shadowsocks(multi)** | **✅ `MultiInbound.UpdateUsers`(`inbound_multi.go:125`)** | ✅ |
| hysteria2 / tuic / anytls | ❌ | ✅ |

模式完全一致:每个协议都在 `NewInbound` 构造函数里调用一次 `service.UpdateUsers(...)`,
然后把 service 存进**小写的未导出字段,没有 accessor**。
唯一的通用接口 `adapter.ManagedSSMServer`(`adapter/ssm.go`)全仓库只有 `*MultiInbound` 一个实现者,
签名带 `uPSKs` 是 SS2022 形状的,不通用。`service/` 下也没有任何通用 user manager
(`ccm`/`ocm` 里的 `UserManager` 管的是控制平面 token,不是代理用户)。

**结论:把「用户管理」拆成两半,高频的那一半不需要碰 sing-box。**

| 操作 | 频率 | 路径 | 中断 | epoch |
|---|---|---|---|---|
| 配额超限 / 到期 / 手动启停 | **高,自动触发** | `user.state` 事件 → 翻 `Tracker.disabled` 内存标记 | 无 | 不变 |
| 加删用户、改 UUID/密码、增删节点 | 低,管理员手动 | `config.apply` → box 重建(§7.4) | 该 agent 亚秒级 | 不变 |

tracker 已经在数据路径上且拿得到 `md.User`,所以禁用只需在 `RoutedConnection` 里返回一个立刻出错的 conn。
主控下发一条轻量事件,**不重建 box、不下发 config.apply、不影响其他用户的连接**。

**禁用只挡新连接,不掐已建立的连接。** 这是 `RoutedConnection` 这个挂载点的固有语义——
它在连接**建立时**被调用一次,此后数据在 `CounterConn` 里流动,tracker 不再介入。
后果要正视:超配额用户如果正挂着一条长连接(大文件下载、视频流、长连 tunnel),
**`user.state` 到达后它可以继续无限期跑下去**,直到那条连接自己结束。
流量仍然照记(计数器不受 `disabled` 影响),所以账不会错,**只是止损不及时**。

对本项目取这个语义是**可接受的**:配额判定每 30s 跑一次(§6.3),真实用户绝大多数连接是短连接,
超限后新连接立刻被拒,吞吐在几十秒内就掉到接近零。

若将来需要「立即掐断」:让 `Tracker` 额外持有 `map[string]map[*trackedConn]struct{}`,
`user.state` 到达时遍历该用户的活跃连接逐个 `Close()`。
代价是要在 conn 关闭时从集合里摘除(否则内存泄漏),`Tracker` 从「只写计数」变成「管理连接生命周期」。
**现在不做**,等有实际需求再加——它是纯增量,不影响其它任何设计。

> ✅ **`rejected(conn)` 的拒绝时机已实测。** tracker 的调用点确实在**选定 outbound 之后**,
> 所以拒绝发生在上游已经拨号之后,会浪费一次 dial。但整个过程在 3ms 内结束,
> 客户端看到的是一次普通的连接中断,不泄露账户状态(spike 对错误文本做了泄漏扫描:
> 不含 disabl/quota/expire/ban/suspend 任一词根)。
> **代价可接受,「禁用也走 box 重建」那条退路不需要启用。**

**内存态必须能重建。** 否则 agent 重启后被禁用的用户会复活。做法:
`disabled` 集合作为 `last-applied.json` 的一个独立字段,`user.state` 只改这个字段并落盘,
**不动 `options`、不重建 box**。§1.2「主控是唯一真源」不受影响。

**什么时候重新考虑打补丁:** 若上游运行时用户更新能力仍不可用、且重建 box 的代价变得不可接受,
再评估为相关协议补导出接口。当前条件不成立,所以继续保持普通依赖、零补丁。
但它把 CI 从一条 `go build` 变成 clone 上游 + apply patch + `go mod edit -replace` + 每日 cron 追补丁腐烂(§11)。
**触发条件:当「加用户导致同 agent 其他用户掉线」成为实际投诉时。**
届时 WS 协议、主控、数据库一行不用改,只是 `config.apply` 多一条「仅用户变更」的快路径。

---

## 8. TUI:五页界面与两行式服务器列表

页签是 `仪表盘[1] │ 服务管理[2] │ 节点[3] │ 用户[4] │ 设置[5]`,样式与旧项目
`tui/widgets/tab_bar.rs` 一致。**方括号里的序号就是能直接按的键**;
Tab / `h` / `l` 循环。五页里跳来跳去时,只有 Tab 的话去最后一页要按四下,
而人心里想的是「去第 5 页」。

`R` 是立刻刷新(常规是每秒一次)。**大写是刻意的**:小写 `r` 在服务管理页是
「轮换 token」,那是一个不可撤销的动作,不能和刷新只差一个 Shift 却又长得像。

### 8.0 仪表盘

只读,回答打开界面第一秒想知道的三件事:

| 面板 | 内容 |
|---|---|
| 概况 | agent 在线/离线/从未连接、节点数、用户启停分布、当前总网速、本周期用量(↑/↓ 分开) |
| 上行 / 下行 | 盲文点阵折线图,全集群合计速率 |
| 用量 Top | 按计费用量排序的用户,↑/↓ 分列 + 渐变进度条 + 快到期提醒 |
| 被控服务器 | 每台两行:流量与配额条 / CPU、内存、负载、运行时长 |

**折线图的一个点 = 一轮上报(30s),不是一次刷新。** TUI 每秒刷新,每次都记的话
一轮上报会被复制成 30 个一样的点,横轴的含义就从「时间」变成了「刷新次数」
(`data::SpeedTracker::observe`)。120 个点 ≈ 一小时。矮终端(< 24 行)上整块让掉:
一条挤成两行的曲线不如把地方给下面的条形和数字。

**主机指标是补上的一条数据通路。** `sysinfo.report` 里的 `cpu_pct` / `mem_used` /
`mem_total` / `load1` / `uptime_secs` 每 30 秒就到主控一次(§7.2),但入库时
只取了 nic 那一段,其余原地扔掉 —— 于是概览页想显示「哪台机器忙成什么样」时
无米下锅,而数据其实早就在线上了。现在它们落在 `agents` 表上(`004` 迁移),
**存最新一次即可**:它们和网卡计数不是一类东西,网卡是累加的账、必须一笔不漏,
CPU 只有「现在多少」有意义,留历史等于给一张每 30 秒 × N 台 agent 增长的表,
而没有任何一处会去查它。

同一张迁移里加了 `sysinfo_at`,而且它不是可选的:少了它,一台离线三天的机器会
一直挂着三天前那个「CPU 3%」,看起来和一台闲着的在线机器一模一样。
超过 90 秒(上报周期 30s 的三倍余量)就整行显示 `--`。

「本周期用量」同时给**网卡**与**计费**两个数字,并在旁边写清口径
(网卡 = 机器进出总量;计费 = 各用户用量 × 倍率)。这两个数字天生对不上(§6.4 / §7.2),
不写清楚就会被当成 bug 反复来问。

#### 网速为什么必须「留住上一个读数」

TUI 每秒刷新,agent 每 30 秒上报一次 —— 两次上报之间的 29 次刷新都做不出差
(`agent_nic_traffic.updated_at` 没动)。早先那一版在做不出差时直接给 `None`,
于是界面上的网速**只在上报到达后的那一帧闪一下**,其余时间全是 `--`。

这不是「没有数据」,是「数据还是上一次那个」。这个数字本来就是 30 秒平均值(§8.2),
它在一个周期内保持不变才是对的。所以 `SpeedTracker` 缓存上一次算出的速率,
只有在**超过 90 秒**(上报周期的三倍)没有新读数时才退回 `--` ——
那时是真的掉线了,而不是刚好卡在两次上报之间。

#### 网卡流量:第一次上报只建基线

`ingest_sysinfo` 对**第一次见到的 agent** 只记基线、不入账。

通用的 epoch 规则(§5.2)是「epoch 变了 → delta = new」,那对**用户流量**是对的:
agent 的 tracker 从进程启动才开始数,`new` 就是这段时间真实发生的量。
但网卡计数读的是 `/proc/net/dev`,它从**机器开机**就在数 —— 一台已经跑了三个月的
机器接进来,照那条规则第一次上报就会把这三个月整个搬进本周期用量,界面上凭空
多出几十上百 GB,而这台机器在本集群里一个字节都还没跑。

注意这**不影响 boot_id 变更**那条路:机器重启后计数器确实是从 0 开始的,那时
`delta = new` 才是对的。区别在「有没有基线」,不在「epoch 变没变」,两者不能混。

### 8.4 设置页

常改的那几项(订阅对外地址、监听、TLS、上报间隔、Telegram)可以在界面里改,
不用去 ssh 里编辑 `/etc/sbx/config.toml`。

写回是**行级替换**(`config::replace_in_toml`),不是「反序列化 → 改字段 → 写回」:
后者一次保存就把配置文件里那几十行解释性注释全抹掉,而那些注释是这个项目里
唯一说明「这个端口为什么只听本地」的地方。实现上找到 `[section]`,在它的范围内
找 `key =`,只换等号右边;**跳过被注释掉的样例行**(`# public_base = "..."`),
改到那上面等于什么都没改而界面会显示「已保存」。写临时文件再 rename,
写到一半断电留下的是旧配置而不是半个配置。

两条约束:

- **daemon 不热加载配置**,它启动时读一次就不再看。所以页面顶上常驻一句
  「改完要重启 daemon」,每一项的说明里也各写一遍生效时机 ——
  「改了但没变」是这个页面最容易造出来的困惑。
- **凭据不回显**(§11.3)。`bot_token` 在列表里只显示前 6 位,编辑框里
  **不预填旧值**:把它明文铺在一个输入框里,等于让它出现在任何一次截图里,
  而这个框的用途本来只是「换一个新的」。

### 8.4.1 页签为什么占两行

`Tabs` 加了 `Borders::BOTTOM`,那条下边框自己要占一行。布局里只给一行的话,
边框把那唯一一行吃掉,**整条页签消失**,屏幕上只剩一条横线 ——
界面「看起来只是少了点东西」,而实际是导航条整个没了。
`tabs_are_actually_visible` 断言的是**渲染出来的字符**,不是布局参数:
参数对不对只有画出来才知道。

### 8.5 用量明细(二级页面)

节点页 / 用户页按 `Enter`(或 `v`)打开一张只读明细:

- 节点 → 这个节点上**各用户**用了多少。排查「某台机器流量异常」的第一个问题。
- 用户 → 这个用户在**各节点**上用了多少。跨 agent 的用户在列表里只有一个合计,
  分不出是哪台机器在承载。

两个方向共用一套查询与渲染(`data::BreakdownRow`):它们读的是 `user_traffic`
的同一张表,只是分组的那一维不同。`LEFT JOIN` 是必须的 ——
分配了但还没跑过流量的关系要显示成全 0,而「分配了没生效」正是最需要看见的一种情况。

明细里同时给**本周期**和**累计**:本周期是计费口径(会被月重置清零),
累计是从建账起的总量。只给其中一个,总会有人拿它去回答另一个问题。

### 8.1 token 管理与一键接入

- **`[a]` 新增 agent**:填名称、网卡月配额、重置日 → 主控生成 **32 字节随机 token**(`rand::rngs::OsRng`),
  base64url 编码,**只在弹窗里显示这一次**;库里只存 `token_hash` 与 `token_prefix`(前 8 位)。
  弹窗给出的不是一份要人手抄的 `agent.toml`,而是**一条可以整条复制去跑的命令**:

      (curl -fsSL .../install.sh 2>/dev/null || wget -qO- .../install.sh) | SBX_SERVER='wss://…/ws' SBX_TOKEN='…' SBX_FINGERPRINT='sha256:…' sh

  走环境变量而不是 `--server` 这类参数,是因为管道形式下传参要写 `sh -s -- …`,
  而这条命令已经够长了;`SBX_TOKEN` 非空本身就足以让脚本判定「这是在装被控」,
  于是连 `SBX_TARGET=agent` 都不用带(`packaging/install.sh`)。
  脚本收到这三个值会写好 `/etc/sbx/agent.toml`(**0600**,里面是明文 token),然后按当前机器
  的 supervisor 接入:systemd `enable` + `restart`,OpenRC 加入 `default` runlevel 并启动,
  最后回读一次运行状态(§11.4)。
  没有 supervisor 时只安装并打印手动命令,不会把一次性后台进程伪装成可靠服务。
  **主控地址不问人**(`install::resolve_host`),按这个次序定:

  1. `cluster.public_host` —— 人明确指定过就不再猜。
  2. `subscription.public_base` 的主机名 —— 域名比 IP 好:换机器时被控端不用重配,
     TLS 那一套也才说得通。
  3. **问外面「你看到我是谁」**(`probe_public_ip`,与 agent 侧 `sysinfo.PublicIPs`
     打的是同一组 endpoint)。
  4. 本机出口地址(`UdpSocket::connect`,只让内核选路由,不发包)。
  5. 占位符,并在提示里说清要换掉。

  **第 3 和第 4 的次序不能反。** 云主机网卡拿到的是**厂商给的内网地址**
  (Oracle / 阿里云 / AWS 都是 NAT 到公网 IP),`UdpSocket` 读出来的正是那个内网地址 ——
  拿它当主控地址,被控机根本连不上,而报错是「连接超时」:一个指向网络问题、
  其实是地址错了的错误。所以必须先问外面,拿不到才退回本机视角。

  探测在 **TUI 启动时丢到后台**跑,不在按下 `a` 的那一刻同步去打 ——
  后者会把界面冻住好几秒。CLI(`agent-add`)是一次性命令,同步等就行。
- **`[i]` 重新查看接入命令**:token 那一格是占位符 —— 明文早就没了,只能提示去轮换。
- 命令那一行在弹窗里**按 `y` 一键复制**,走 **OSC 52**。
  不用 `arboard` 之类的原生剪贴板库是有原因的:主控几乎总是跑在一台没有图形界面的
  Linux 服务器上,而人是从自己的电脑 ssh 进去的 —— 原生库操作的是**服务器那一侧**的
  X11 剪贴板,无头机器上直接报错,即使有也复制到了一个没人看得见的地方。
  OSC 52 把内容发给**终端模拟器**,由终端写进用户本地的剪贴板,天然穿过 ssh。
  代价是**没有回执**:终端不支持(或 tmux 没开 `set-clipboard`)就什么都不会发生,
  程序这边看不出区别 —— 所以弹窗里那句回执必须如实说这一点,不能写成「已复制」了事。
  另有一条一定管用的退路:`sbx agent-add` / `agent-rotate` 在**普通终端**里
  也打同一条命令,那里用鼠标选中复制没有任何依赖。
- **`[r]` 轮换 token**:新 token 使旧的立即失效。**在线连接不立刻踢**,下次重连时生效。
  弹窗直接给出带新 token 的完整命令 —— 轮换之后要做的事恰好就是「在那台机器上重跑一遍」。
- **`[E]` 编辑 agent**:名称、网卡月配额、配额重置日、网卡记账口径(§6.4)。
  口径是 `Select`(←/→ 循环)而不是手打:它只有四个取值,打错一个字母的反馈会是
  提交之后一句「无法识别」,而正确的四个一个都没显示出来(§8.1.1 的通则)。
  **IP 不在可改之列** ——
  它每次 `sysinfo.report` 都会被 agent 自探的值覆盖(§7.3),
  一个「改了但过一会儿自己变回去」的输入框比没有这个输入框更糟。
- **`[d]` 删除 agent**:级联删 `nodes`,`user_nodes` 随之清理。
  删除前弹确认,并**提示会影响的用户数**。

token 校验:**按 `token_prefix` 索引定位到候选行 → SHA-256 → 恒定时间比较 hash**。
失败的连接记 `agent_events`,但**不向连接方回显具体失败原因**(避免枚举)。

**不要用 argon2 之类的慢 KDF。** 慢 KDF 是为**低熵人类密码**设计的;
这里的 token 是 32 字节 `OsRng`(256 位熵),爆破在物理上不可能,慢哈希**换不到任何安全性**。
反过来它有实际代价:argon2 每行独立 salt → 无法用 hash 索引 → 校验一个进来的 token 要**遍历全表逐行跑 KDF**,
于是任意**未认证**的连接都能放大成 N 次昂贵 KDF,是一个白送的 DoS 面。
SHA-256 单次即可,且 `token_prefix` 让定位是 O(1)。

### 8.1.1 表单:字段有类型,而且会随取值增减

`master/src/tui/modal.rs` 是一套小表单引擎,三种字段:

| 类型 | 操作 | 用在哪 |
|---|---|---|
| `Text` | 打字 / Backspace | tag、端口、SNI、日期 |
| `Select` | `←` `→` 循环 | 协议(八选一)、所属 agent |
| `Toggle` | 空格 / `←` `→` | 订阅优先 IPv6 |

排版**照旧项目 `src/tui/forms.rs` 的样子**:一个字段两行 —— 标签与取值同一行
(标签列按最长的可见标签对齐,所有取值落在同一竖线上),下面空一行。
聚焦的取值用反白底。装不进标签括号的说明落到弹窗底部的说明区。

早先每个字段是「标签 / 灰色提示 / 空行」三行,九个字段就把弹窗撑到快满屏,
读起来是散的:视线要跨过两行才能从一个输入框走到下一个。

弹窗的**宽高都按内容算**,不用百分比:百分比在宽屏上拉出一个空荡荡的大框,
在窄屏上又把说明和底部按键提示切掉。宽度取「字段行 / 最长说明 / 按键提示」三者的最大值。
说明区**自己折行**(`theme::wrap`),不交给 ratatui 的 `Wrap` —— 后者折出来的行数
算不进高度,底下几条会被静默裁掉,而且续行顶到最左边,看起来像另起了一条说明。

两条不能退回去的规矩:

- **取值来自有限集合的字段必须是 `Select`。** 协议曾经是手打的,打错一个字母的反馈是
  提交之后一句「无法识别的协议」,而正确的八个值一个都没显示出来。
- **字段按协议显隐**(`Form::visible`)。`server_name` 只对 reality/trojan/tuic/anytls 有意义,
  `path` 只对两个 ws 协议有意义 —— 判定在 `tui/forms.rs::uses_sni` / `uses_path`,
  与 `secrets::fill`、`service::build_inbound` 是同一份事实。
  让 shadowsocks 的表单上摆着一个填了不生效的 `path` 框,比不显示它更容易出错。
  焦点必须跟着走:协议一换,原本聚焦的字段可能就地消失,不处理的话下一次按键会改一个
  屏幕上根本看不见的框。

**编辑节点时 tag / 协议 / 所属机器不可改**,只在标题行里只读显示:
改 tag 会让历史流量和新流量记到两个互不相认的账上(§7.1 的记账口径),
改协议等于换一整套密钥材料 —— 那就该走「删掉重建」,而不是伪装成一次编辑。
同理,编辑走的是**在原 `params` 上改**再写回,不是构造一个新的:
`params` 里有 reality 密钥对、自签证书、ss 服务端密钥,清掉等于客户端静默全部失联(§9.1)。

用户的节点分配是一个**多选框**(`Picker`),打开时已分配的节点是勾上的,
一次提交整体替换(`node_repo::set_user_nodes`)。之前那版要人先去另一页把 `node_id` 记下来,
一次只能加一个,取消分配还得走另一条路。

### 8.2 两行式列表

用 ratatui `Table` + `Row::new(cells).height(2)`,每个 cell 用
`Cell::from(Text::from(vec![Line, Line]))`,列宽用 `Constraint::Length`。
这与旧项目 `src/tui/pages/nodes.rs` 是**同一套写法**,唯一区别是行高从 1 变 2。

| 列 | 第 1 行 | 第 2 行 |
|---|---|---|
| 名称/状态 | agent 名 + 在线圆点 | `agent_version` |
| IP 地址 | IPv4 | IPv6(超宽度截断,尾部 `…`) |
| 网速 | `↑ 12.4 MB/s`(绿) | `↓ 3.1 MB/s`(蓝) |
| 流量 | `34.29 GB / 500.00 GB` 或 `63.63 GB · 不限流量` | 渐变进度条 + `每月 22 日重置` / `无需重置` |
| 主机 | `CPU 37%`(颜色走同一条渐变) | `内存 38%` |

「主机」列只在**宽到有余量**时出现(约 130 列以上)。它是最锦上添花的一列,
不该从流量或重置日那里抢地方 —— 而且它的门槛必须**按前面几列已经定下来的宽度算**,
不能像出站/重置那样各判各的:独立判断会让它在 121 列凭空出现,总宽一次撑过边框
8 格,而 ratatui 会静默压缩各列。表现是「终端明明拉宽了,每一列反而更挤」。

流量列没有配额时,第二行画一条**暗色 `—`** 占位而不是留空:留空会让那一台
看起来只有一行,和上下两台黏成一片;缺 IPv6 早就是这么占位的,两处要一致。

**相邻两台之间空一行。** 一台占两行,而终端画不出半行,所以一条空行就是最小的
那一档间距。两个约束:

- 间隔是一条**独立的空 `Row`**,不是给上一行加 `bottom_margin`。margin 属于它所在
  的那一行,选中态的底色会连着盖过去 —— 于是恰恰在正看着的那一台下面,间距被
  填成一整块实心底色,白加一行还看不出分隔。
- **只有全部机器都放得下时才加**(`3n - 1 ≤ 可用行数`)。这张表没有滚动视口,
  行数超了是从底下静默裁掉的;无条件加空行等于凭空少放三分之一的机器,
  而被裁掉的那台看不出来。放不下时退回原来的紧凑布局。


细则:

- **网速**由相邻两次 `sysinfo.report` 的 rx/tx 差除以时间间隔算出,**只在内存里,不落库**。
  epoch(`boot_id`)变化时,该次速度显示 `--` 而不是一个爆炸的数字。
  **注意口径:上报周期是 30s,所以这个数字是 30 秒平均值,不是瞬时速率**——
  突发流量会被抹平,数字变化也会显得很钝(每 30s 才跳一次)。
  这是刻意接受的:该列的用途是「这台机器现在忙不忙」,不是抓包级的速率监控;
  为了让它更灵敏而把上报周期压到几秒,会让 N 台 agent 的上报开销和 DB 写入放大一个数量级。
  若确实需要更灵敏,正确的做法是给 `sysinfo.report` 单独一个更短的周期(与 `stats.report` 解耦),
  而不是整体加快上报。
- **渐变进度条自绘**:按格数生成 `Vec<Span>`,每格一个 `█`,
  颜色沿多段停靠点(绿 `#3ddc84` → 黄 `#f5c451` → 橙 `#f08c3a` → 红 `#e5484d`)
  按当前格位置**线性插值**成 `Color::Rgb`。未填充部分用暗灰 `░`。
  **不能用 ratatui 的 `Gauge`——它只支持单色。**
- `nic_quota_bytes IS NULL` 时**不画进度条**,只显示累计值 + `不限流量` / `无需重置`。
- 「已用」按这台机器的**网卡记账口径**算(§6.4),不再写死 RX+TX。口径没有自己的列
  —— 这张表已经够挤 —— 它出现在底部选中项摘要和 `Enter` 的网卡明细里;
  后者同时给原始 `↑TX` / `↓RX` 和「计入(口径)」那个数,两边对得上才查得动账。
- 文本格式化复用旧 `model/user.rs::format_bytes`。
- IPv6 显示前走 `node_service.rs::normalize_server_ip` 的**同族逻辑**,但注意:
  **只有 URL 场景才加方括号,列表里不加。**
- **IP 列不留列内空白。** 分隔由 ratatui 的 `column_spacing`(默认 1)负责;
  再自己减一格是白花的,而它让整张表多占一列 —— 146 列的终端上正好卡在
  「七列全在 + 完整 39 字符 IPv6」的门槛下面一格,地址从尾巴少两位。

### 8.3 窄终端:宁可砍整列,不要压窄每一列

节点页与用户页的列**按可用宽度挑**(`pages.rs::pick`):列表按重要性排,
放不下就从尾巴上砍掉整列(用户页的顺序是 倍率 → 重置日 → 上/下行 → 进度条 → 到期)。

理由是**砍掉一整列是看得见的,压窄一列是看不见的**。早先固定列宽的版本在 80 列下
把「每月 22 日重置」切成了「每 」,而界面看起来完全正常 —— 只是少了信息。
根因是列宽没算 ratatui 在列之间插的间隔,总宽超出后各列被静默压缩。

配套的两件事:

- 宽度一律按**显示列数**算,不是字符数(`theme::cols` / `truncate` / `pad`)。
  一个汉字占两列,`format!("{:<12}", "计数器重置")` 会在一个已经占了 10 列的串后面
  再补 7 个空格,后面的列全部错位。
- 列里放不下的东西(完整的 SNI、中转落点、订阅地址)放在页面底部的**详情面板**里。
  详情面板只渲染人填的那几项 —— `params` 里的密钥材料不得出现在界面上(§11.3),
  这一条有专门的回归测试盯着。

---

---

## 9. 从旧实现保留下来的契约

### 9.1 必须保留

- 八协议链接与配置生成语义；golden 在 `master/testdata/`,由 `agent/boxctl` 喂给真 sing-box。
- SQLite 线性迁移:`include_str!` + `PRAGMA user_version`,每版一事务。
- 用户配额/到期/倍率、Telegram 阈值去重、订阅和只读统计页。
- Reality、自签 TLS、Shadowsocks 服务端密钥只生成一次并持久化。
- TUI 的显示宽度必须使用 `theme::{cols,pad,truncate}`,不能用 Rust 字符宽度。

### 9.2 明确移除且不得重新引入

- V2Ray gRPC API、protobuf 与 `with_v2ray_api`；统计来自进程内 tracker。
- 外部 sing-box 进程管理、内核安装/升级页与内核分发仓库。
- fork / patch sing-box；CI 拒绝 `agent/go.mod` 的 `replace` 和 `agent/patches/`。
- agent 管理 HTTP/gRPC 端口；agent 只主动连接主控 WebSocket。

因此 master 不依赖 `tonic`/`prost`,agent 只允许 `with_quic,with_utls` 两个 build tag。

## 10. 订阅导出

节点地址来自 `agents.ipv4 / agents.ipv6`,保留中转覆盖与
`use_public_base_as_server` 优先级链。

IPv6 必须按输出上下文处理:
- URI authority 加方括号:`@[2001:db8::1]:443`；
- Clash/Mihomo YAML 与 VMess JSON 的 `server/add/host` 使用不带框的裸地址。

方括号属于 URI 语法,不能在 endpoint 层提前写进地址。

一个用户的订阅 = 它在 `user_nodes` 里的**全部节点,跨 agent 拼在一起**。

订阅 HTTP 监听(旧 `sub_server.rs`)是 §2「不做 Web 面板」的**唯一例外**:
它只吐订阅内容与 `stats_html`,不提供任何管理能力。

---

## 10.3 订阅按网卡流量报(给管理员用)

用户可以绑定一台或多台 agent 的**网卡流量**(用户页 `[b]`,与分配节点同一套多选框)。
绑定之后,**只有订阅响应头 `subscription-userinfo` 里的三个数字**换成这些机器的
网卡用量之和:

```
upload   = Σ 该机口径投影出的出站部分
download = Σ 该机口径投影出的入站部分
total    = Σ agents.nic_quota_bytes    （见下面那条例外）
expire   照旧
```

投影按**每台自己的** `nic_accounting_mode` 做,做完再汇总(§6.4 那张表):
`sum` → `(tx, rx)`,`outbound` → `(tx, 0)`,`inbound` → `(0, rx)`,
`max` → 较大的那个方向留在原位、另一个归零。于是每台的 `upload + download`
恒等于服务管理页上那个「已用」,几台混着不同口径也能直接相加 ——
两处给出两个不同的数字,才是这个功能最难查的失败形态。

**为什么要有这个。** VPS 厂商按**网卡**计费,而用户流量是 sing-box 记的账,
两者天生对不上(§6.4 / §7.2)。绑一下之后,管理员用任何一个代理客户端打开订阅
就能看到「这几台机器这个月烧了多少」,不用 ssh 上去查 —— 客户端本来就会把
这个响应头显示成一个进度条。

**边界要守死:它只改这个响应头。** 订阅内容、用户自己的计费与停用判定、
界面上的用户用量,全都走原来的路。一个只影响「显示」的功能不该能把人停掉,
这一点有专门的回归测试(`a_binding_never_touches_the_users_own_accounting`)。

两条具体规则:

- **不乘 `traffic_multiplier`。** 网卡流量是厂商的账,倍率是给用户计费用的,
  混在一起会给出一个哪边都对不上的数字。
- **只要有一台没设配额,`total` 就报 0(不限)。** 把设了配额的几台加起来当上限,
  会给出一个「看起来精确但根本不是上限」的数字,而客户端会拿它去算百分比 ——
  那比不给更糟。

绑定关系存在 `user_nic_bindings`(`005` 迁移),**不推进任何 revision**:
它不进 sing-box 配置,agent 根本不需要知道它。

界面上必须标出来:用户列表有一列「订阅口径」,绑了的显示 `网卡×N`;
详情面板再写一句「订阅响应头报的是 N 台机器的网卡用量之和,不是上面这个用户用量」。
不标的话,那是一条永远查不明白的「客户端和后台对不上」。

---

## 11. 构建、发布与配置文件

### 11.1 CI:两个普通构建,没有内核构建

**因为不 fork sing-box(§0.3 结论三),agent 的 CI 就是一条普通的 `go build`——不需要 clone 上游、
不需要打补丁、不需要 `go mod edit -replace`、不需要每日 cron 追上游。**
sing-box 版本是 `agent/go.mod` 里的一行 require,升级 = `go get -u` + 跑测试 + 提一个 commit。

`release.yml` 出两组产物:

| 产物 | 语言 | target | 备注 |
|---|---|---|---|
| `sbx` | Rust | `x86_64-unknown-linux-musl`、`aarch64-unknown-linux-musl` | 沿用旧 `release.yml`:x86 用 musl-tools 原生编,arm64 用 `cross`(避开不稳定的 ARM runner) |
| `sbx-agent` | Go | `linux/amd64`、`linux/arm64` | `CGO_ENABLED=0` 交叉编译,单个 job 就能出两个 arch |

agent 侧构建的全部内容:

```yaml
- uses: actions/setup-go@v5
  with: { go-version: stable, cache-dependency-path: agent/go.sum }
- working-directory: agent
  env: { CGO_ENABLED: "0", GOOS: linux, GOARCH: "${{ matrix.goarch }}" }
  run: |
    go build -trimpath -tags with_quic,with_utls \
      -ldflags "-s -w -buildid= -X main.Version=${GITHUB_REF_NAME#v}" \
      -o sbx-agent ./cmd/sbx-agent
```

**`-tags` 只有 `with_quic,with_utls` 这两个,而且是必需的:**
reality 依赖 uTLS、hysteria2 / tuic 依赖 QUIC(§9.1 的跨语言 golden 测试测出来的)。
漏掉会被 `agent/boxctl/buildtags.go` 的编译期哨兵挡在 CI 里。
**多了就有问题** —— 如果将来有人在这里加了 `-tags with_v2ray_api`,说明 §0.2 被误读了:
流量统计走 ConnectionTracker,与 v2ray API 无关。

沿用旧 `release.yml` 的两条做法:
- **发布前校验元数据**:tag、`Cargo.toml` version、`CHANGELOG.md` 里的 `## v<x>` 标题三者一致,不一致就 fail
- **每个产物配一个独立的 `.sha256`**,`agent.upgrade`(§4.2)按 `<asset>.sha256` 下载校验

Release 共八项:主控双架构 tar.gz 与各自校验和、agent 双架构裸二进制与各自校验和。
`agent.example.toml` 不重复上传,安装脚本从目标版本源码 tag 获取；systemd/OpenRC
service 内嵌在 `install.sh` 里(§11.4),既不占资产也不依赖网络。
主控 `config.example.toml` 与 `sbx.service` 仍留在已校验的 tar.gz 内。

### 11.2 主控配置文件

沿用旧项目的分节 TOML + `#[serde(default)]` 模式(旧 `model/config.rs` 是范本)。分节:

```toml
[db]
path = "/etc/sbx/sbx.db"

[cluster]
listen = "0.0.0.0:18443"
tls = true                    # false 则明文 ws(agent 侧也要 insecure = true)
cert_path = "/etc/sbx/tls/cert.pem"
key_path  = "/etc/sbx/tls/key.pem"
heartbeat_secs = 10
report_interval_secs = 30

[subscription]
listen = "127.0.0.1:18081"
public_base = ""
use_public_base_as_server = false
enabled = true

[telegram]
# 沿用旧 TelegramConfig 全部字段与默认值
```

**没有 `[kernel]` 段。** 旧项目的 `KernelConfig { update_repo, ... }` 整个不存在(§9.2)。

### 11.3 agent 配置文件

agent 配置(`/etc/sbx/agent.toml` 或纯 flag/env):

```toml
server      = "wss://master.example.com:18443/ws"
token       = "..."          # 主控生成,只显示一次
fingerprint = "sha256:..."   # TOFU 固定
insecure    = false          # true 时允许 ws:// 明文
```

**agent 不监听任何管理端口。** 它是 WebSocket **客户端**,主动拨向主控。
被控服务器上对外开放的端口只有 sing-box 自己的代理 inbound——
没有 gRPC 18080(旧项目要开)、没有管理 HTTP。
需要暴露管理端口的只有主控一台(`cluster.listen`)。N 台被控的管理面攻击面是零。

**凭据处理:** `token`、TLS 私钥、`telegram.bot_token` 属于凭据,
日志与 TUI 中一律不回显完整值(token 只显示 `token_prefix` 8 位)。

### 11.4 agent 进程监督约束

agent 不要求 systemd 专属 API；它只要求部署环境有一个能在进程退出后重新启动它的
supervisor。Linux 安装脚本按命令是否真实存在分流:

- systemd 使用 `sbx-agent.service` 的 `Restart=always`；
- OpenRC 使用 `/etc/init.d/sbx-agent` 的 `supervise-daemon`、3 秒延迟和无限重试；
- 没有任一 supervisor 的容器仍可手动 `exec /usr/local/bin/sbx-agent /etc/sbx/agent.toml`,
  但崩溃或 `agent.upgrade` 主动退出后不会自动恢复。

**service 文件由 `install.sh` 自带,不从网络获取。** v0.4.18 曾改成按 tag 从
`raw.githubusercontent.com` 拉取；那台主机在部分网络里不可达,取不到时脚本只提示
「跳过」,于是机器上根本没有 service,agent 只能手工前台运行 —— 表现是「装完能用,
关机再开机就再也不上线」,而主控侧只看到一盏灭掉的灯。两个 service 文件是静态文本,
不该让部署能否熬过一次重启依赖第二个必须可达的域名。`packaging/test-install.sh` 有
golden 用例保证内嵌副本与 `packaging/` 下的文件逐字节一致。仅 `agent.example.toml`
仍按 `v${VERSION}` tag 获取:它只是参考样本,取不到不影响运行。

**开机自启是部署的一部分,不是「现在要不要提供服务」的策略选择。** 一台在跑但没
enable 的 agent 从外面完全正常,直到下一次重启才失联,而主控无法区分「机器关着」和
「服务没设自启」。因此每条安装/升级路径都调用 `ensure_boot_autostart`(已设好时是空
操作),设不上时打印可照抄的命令并继续 —— 不能因为一条警告让整条安装命令带着 `set -e`
退出,那会把「二进制已经换好了」也一起吞掉。启动之后回读 `is-active`:起不来要当场
说,不能照样打一句成功提示。

主控运行管理不随 agent 分流改变,仍由 systemd unit 负责。

---

## 12. 真实 sing-box 验证基线

`spike/` 已在 sing-box v1.14.0-beta.3 上验证并进入 CI:

1. per-user / inbound tag 流量真实到账,read=上行、write=下行；
2. 禁用用户的新连接在毫秒级被拒,错误文本不泄露 quota/expire/ban 状态；
3. inbound tag 来自 `md.Inbound`,不是协议名 `InboundType`；
4. `include.Context()` 与 `AppendTracker` 的安装时序不可改变。

`agent/boxctl` 另行验证同端口热重载、失败回滚与八协议跨语言 golden。
Reality 需要 `with_utls`,Hysteria2/TUIC 需要 `with_quic`;编译期哨兵阻止漏 tag。
修改 `agent/tracker` 时必须同步修改并运行 `spike/`。

## 13. 验证方式

### 13.1 单元测试契约

以下两块是 sbx 的核心正确性,必须持续保留:

- **epoch / delta 增量算法**:喂三类输入 —— 正常递增、epoch 变更、`new < last` 回绕。
- **跨 agent 求和视图**:造 2 个 agent × 3 个节点的数据,验证 `user_traffic_total` 的结果
  等于逐节点手算之和,且无流量记录的用户也出现(值为 0)。

### 13.2 端到端

> ✅ **已跑通**(Oracle ARM / Ubuntu 22.04 aarch64,2026-08-01)。可重复执行的驱动在 `e2e/`。

本机起 master daemon,同机跑**两个 agent 实例**(不同 token、不同 sing-box 监听端口)。
加一个用户、给它分配两个 agent 各一个节点,用真客户端连上跑流量,
确认 `user_traffic_total` **等于两个节点之和**。

实测逐字节对上:tokyo 记 `262152 / 4194304`,osaka 记 `131080 / 1048576`,
视图合计 `393232 / 5242880` —— 等于逐节点手算之和(`+8` 是驱动自己的协议头)。

### 13.3 断连恢复

> ✅ **已跑通**,同上。

跑流量期间 `kill -9` 一个 agent 再拉起。确认:

- 主控写了 epoch 变更事件(§5.4)—— `agent_events` 里出现 `counter_reset`
- 增量**不重复计**
- **不丢失**重启前已上报的部分

实测:重启前 `total = 262152 / 4194304`,重启后再推 `100000 / 500000`,
结果 `362160 / 4694304` —— 正好是两段之和,一字节不差。
`counter_epoch` 变了,主控把上报值当**全量**而不是做差(做差会得到负数)。

顺带验到:agent 冷启动时先按 `last-applied.json` 把 box 拉起来再连主控(§4.1),
订阅链接里的地址是 agent 自探到的公网 IP 而不是 `127.0.0.1`(§7.3)。

### 13.4 TUI 宽度与 Unicode

窄/宽终端均由 TestBackend 回归。重点覆盖:

- 完整 39 字符 IPv6、IPv4/IPv6 中转与端口；
- 中文 label/note 按终端显示列宽对齐；
- quota NULL / 50% / 100% 的进度条；
- 列宽总和不得越过边框,确实放不下时必须显示 `…` 或砍掉整列。

---

## 14. 维护约束速查

- §1 的双许可 / 配置权威 / TOFU 是默认架构,调整前必须更新理由与部署流程。
- 记账键是 `(用户, inbound tag)`,不是用户名。
- `config_revision` 与 `user_state_revision` 独立:前者重建 box,后者只更新内存名单。
- 改 `agent/tracker` 同步改 `spike/`;改主控配置生成同步改 golden。
- URI 与结构化订阅的 IPv6 形状不同,不能共享带框后的 host。
- TUI 中文宽度使用 `theme::cols/pad/truncate`,不得用 `format!("{:<n}")` 对齐。
- 发布前 tag、Cargo 版本、CHANGELOG 标题必须一致,并等待 CI 全绿。
