# sbx

主控(Rust)+ 多被控 agent(Go 内嵌 sing-box)的 sing-box 集群管理工具。
一台主控通过 WebSocket 管理 N 台被控服务器,CLI + TUI 运维闭环。

完整设计见 [`DESIGN.md`](DESIGN.md);部署与跨机验证清单见 [`DEPLOY.md`](DEPLOY.md)。

## 许可证:双许可,边界在 WebSocket 上

本仓库**不是**单一许可证的项目。两部分各有各的许可证:

| 目录 | 许可证 | 原因 |
|---|---|---|
| `master/`、`shared/` | **MIT**([LICENSE-MIT](LICENSE-MIT)) | 不链接任何 GPL 代码 |
| `agent/` | **GPLv3**([agent/LICENSE](agent/LICENSE)) | 静态链接 sing-box,构成其衍生作品 |

**为什么 GPL 不会传染到 `master/`:**
`master/` 与 `agent/` 之间**只有 WebSocket 网络边界**——它们是两个独立的进程,
通过 JSON 消息通信,不共享地址空间、不互相链接、不互相调用函数。
GPL 的传染范围是「衍生作品」,而网络通信不构成链接。
因此把 sing-box 静态编进 `agent/` 使 `agent/` 成为 GPLv3 作品,但 `master/` 不受影响。

这条边界是**设计约束,不是事后解释**:如果将来有人提议把 agent 的某段逻辑
以库的形式直接链进 master(或反过来),那会立刻打破这个边界,使 master 也变成 GPL 衍生作品。
不要那样做。跨边界的一切都必须经过 WebSocket。

## 状态

**master 与 agent 都已完成并通过测试,两端在真机上对接跑通过(见下)。**

agent 自升级(`agent.upgrade`)从 v0.4.0 起整条路都通了:主控侧拼产物地址与
校验和并排进队列、daemon 侧下发、agent 侧下载校验替换退出,每一段都有测试
(其中「替换正在运行的二进制」那一步有 8 条,每条失败路径都断言老文件一字节没变)。
**仍然没有在真机上端到端跑过一次完整升级** —— 那要一台在线的被控机加一次真实发布。

| 部分 | 状态 |
|---|---|
| `shared/` 协议定义 | ✅ 已完成,6 个测试 |
| `master/` db 迁移 + model | ✅ 已完成,有测试 |
| `master/` epoch/delta 算法(§5.2) | ✅ 已完成,§13.1 要求的三类输入全覆盖 |
| `master/` token 生成与校验(§8.1) | ✅ 已完成,有测试 |
| `master/cluster/registry`(§4.1 驱逐) | ✅ 已完成,有测试 |
| `master/cluster/server` WS 握手 | ✅ 已完成,**端到端测试走真实 socket** |
| `master/cluster/{rpc,ingest}` | ✅ 已完成,rpc 关联 + 上报入库经 delta |
| `master/service` 配置组装 | ✅ 已完成,**八个协议全部支持**(§9.1),含跨语言 golden 测试 |
| `master/db/node_repo` | ✅ 已完成,CRUD 时自动推进 revision |
| CLI(agent/node/user 管理) | ✅ 已完成,可操作 |
| §4.1 握手补齐 | ✅ 已完成,有端到端测试(agent revision 落后时收到补齐消息) |
| TLS 装配(§1.3 自签证书 + TOFU) | ✅ **已完成**,含指纹计算与 openssl 交叉验证 |
| §6.3 配额/到期巡检 | ✅ **已完成**,15 个测试覆盖月重置、短月、配额、到期、跨 agent 求和 |
| §12.0 的 spike | ✅ **已跑通**(sing-box v1.14.0-beta.3),三条假设全部证实 |
| `agent/tracker`(§7.1/§7.5) | ✅ 已完成,流量方向与拒绝时机均经 spike 实测 |
| `agent/boxctl`(§7.4 热重载) | ✅ 已完成,7 个测试跑**真实 sing-box 实例**验同端口重载与回滚 |
| `agent/master` WS 客户端(§4) | ✅ 已完成,17 个测试,含对着**假主控**跑完整握手/收发 |
| `agent/{state,sysinfo,config}` | ✅ 已完成,有测试 |
| CI / release(§11.1) | ✅ 已完成,`.github/workflows/` + `packaging/` |
| 八协议配置生成(§9.1) | ✅ 已完成,golden 由 Rust 生成、**由真 sing-box 校验** |
| 订阅导出(§10) | ✅ 已完成,base64 链接 + Clash YAML + `/sub/:token` HTTP 服务 |
| TUI(§8) | ✅ 已完成,仪表盘 / 服务管理 / 节点 / 用户 / 设置 五页,数字键直达 |
| 流量统计页(`stats_html`) | ✅ 已完成,浏览器打开订阅地址即是它 |
| Telegram 通知(§9.1) | ✅ 已完成,单实例租约 + 阈值告警去重 + 定时播报 |
| §13.2 / §13.3 端到端 | ✅ **已在真实 ARM Linux 上跑通**(1 主控 + 2 agent + 真流量) |

Rust 侧 453 个测试通过(`cargo test`),
Go 侧 54 个测试通过(`cd agent && go test -tags with_quic,with_utls ./...`,
**含 `-race`**,已在 Linux ARM 上跑过)。

> **两端已经真正对接过了。** 在一台 Oracle ARM(Ubuntu 22.04 aarch64)上起 1 个主控
> + 2 个 agent,用真客户端推不对称流量,逐字节核对:
>
> * **跨 agent 求和**(§13.2):两节点分别记 `262152/4194304` 与 `131080/1048576`,
>   `user_traffic_total` 合计 `393232/5242880` —— 等于逐节点手算之和;
> * **断连恢复**(§13.3):跑流量时 `kill -9` 一个 agent 再拉起,
>   `counter_epoch` 变化被主控识别为计数器重置,增量既不重复计也不丢失,
>   `262152 + 100008 = 362160` 一字节不差;
> * 顺带验到 TOFU 握手、握手补齐、冷启动(先起 box 再连主控)、
>   公网 IP 自探、订阅三种格式在真实部署上都正常。
>
> 复跑方式见 [`e2e/README.md`](e2e/README.md)。

> **已实测的三件事**(原先是 §12.0 待验清单,现在是结论):
> ① per-(用户, inbound tag) 流量确实到账,`md.Inbound` 填的是配置里的 tag 字符串,
> `NewInt64CounterConn(conn, read, write)` 的 read 是上行、write 是下行;
> ② 被禁用用户的新连接在 **3ms 内**被拒,客户端看到的是普通连接中断,错误文本不泄露账户状态;
> ③ `include.Context()` 不能少,`AppendTracker` 必须在 `Start()` 之前。
> 复跑:`cd spike && go run .`(自断言,全过才 exit 0)。**改了 `agent/tracker` 要同步改它。**
>
> **spike 的第四条结论是错的,已更正**:它当时说「不需要 build tag」,
> 但它只跑了无 TLS 的 vless。八协议的 golden 测试测出 agent **必须**带
> `-tags with_quic,with_utls`(reality 要 uTLS,hysteria2/tuic 要 QUIC)。
> 现在 `agent/boxctl/buildtags.go` 有编译期哨兵挡着。
>
> **agent 自升级**(`agent.upgrade`:下载、校验 sha256、rename 覆盖自己再退出)
> 从 v0.4.0 起在界面上可用(服务管理页 `[u]`),各段都有单测;
> 但**没有在真机上端到端跑过一次** —— 那要一台在线的被控机加一次真实发布。

## 安装

```sh
# 主控机 —— 不带参数就是装主控
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | bash

# 被控机
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | bash -s -- agent
```

重跑同一条命令就是**升级**:本机装过什么就升什么,已是最新版则什么都不做。
下载后强制校验 sha256,取不到校验和宁可拒装。部署细节见 [`DEPLOY.md`](DEPLOY.md)。

> 管道形式要传参数必须写 `bash -s -- <目标>`。少了 `-s --` 的话,`bash agent`
> 会把 `agent` 当成脚本文件名去打开,报 `cannot open agent`。
> 嫌绕就用 `| SBX_TARGET=agent bash`,哪种调用形式都行。

被控机通常**不用记上面这条**:主控上 `sbx tui` → 服务管理页 → `[a]` 新增,
会直接吐出一条填好 token 与证书指纹的整行命令,复制过去跑完即接入:

```sh
curl -fsSL .../install.sh | SBX_SERVER='wss://主控:18443/ws' SBX_TOKEN='…' SBX_FINGERPRINT='sha256:…' bash
```

给了 `SBX_TOKEN` 就一定是在装被控端,所以不必再带 `SBX_TARGET=agent`。
脚本会写好 `/etc/sbx/agent.toml`(0600,里面是明文 token)并 `enable --now sbx-agent`。
轮换 token 之后按 `[r]` 拿到的新命令在那台机器上重跑一遍即可 —— 旧配置会自动备份成 `.bak`。

`packaging/install.sh` 有一套离线测试:`sh packaging/test-install.sh`(CI 里用 dash 和 bash 各跑一遍)。

## 已经能跑的东西

```sh
cargo test                      # 507 个测试
cargo build --release           # 产物 target/release/sbx

# 装完起不来?先跑自检 —— 二进制/配置/数据库/systemd/证书/端口逐项过一遍,
# 每项都写出实际位置,数据库另给文件大小与 schema 版本。
# 它只读,不会把库建出来、也不会生成缺失的证书。有 ERR 时退出码为 1。
./target/release/sbx --config c.toml doctor

# 建库 + 加一台被控服务器 + 启动主控
./target/release/sbx --config c.toml init-db
./target/release/sbx --config c.toml agent-add tokyo-1    # 打印 token(只此一次)
./target/release/sbx --config c.toml agent-list
./target/release/sbx --config c.toml daemon               # 默认 wss,启动时打印证书指纹

# 建节点 + 建用户 + 分配 + 取订阅(八个协议都可以)
./target/release/sbx --config c.toml node-add 1 tokyo-reality 8443 --protocol vless-reality
./target/release/sbx --config c.toml user-add alice --quota-gb 100
./target/release/sbx --config c.toml user-assign alice 1
./target/release/sbx --config c.toml user-sub alice          # 订阅地址
./target/release/sbx --config c.toml user-sub alice --links  # 连同分享链接

# 或者直接开界面(§8):仪表盘 / 服务管理 / 节点 / 用户 / 设置 五页,按 1-5 直达
./target/release/sbx --config c.toml tui
```

TUI 里能做的事。哪一页都能按的:`1-5` / `Tab` 切页、`↑↓` 或 `jk` 选行、
`R` 立刻刷新、`U` 升级主控自己、`q` 退出。

| 页 | 键 |
|---|---|
| 仪表盘 | 集群概况、上下行盲文折线图、用量 Top、节点用量。`←/→` 在两张表间换焦点,`[Enter]` 看选中那一项的用量明细 |
| 服务管理 | `[a]` 新增(给出一键接入命令) `[E]` 编辑配额 `[Enter]` 网卡明细 `[c]` 查看下发给它的 sing-box 配置 `[o]` 出站地址族策略 `[i]` 再看一次接入命令 `[u]` 升级 agent `[r]` 轮换 token `[d]` 删除 |
| 节点 | `[a]` 新增 `[E]` 编辑 `[Enter]` 这个节点上各用户的用量 `[d]` 删除 |
| 用户 | `[a]` 新增 `[E]` 编辑计费 `[Enter]` 这个用户在各节点的用量 `[n]` 分配节点(多选) `[b]` 订阅按网卡流量报 `[T]` token 管理 `[r]` 重置流量 `[t]` 启/停 `[s]` 订阅 `[d]` 删除 |
| 设置 | `[Enter]` 改这一项。改的是配置文件本身,注释与排版都保留;**改完要重启 daemon** |

`[o]` 的五个取值是「自动(跟随系统解析)/ 优先 IPv4 / 优先 IPv6 / 仅 IPv4 / 仅 IPv6」,
**按 agent 存**:一台只有 IPv4 出口的机器要「仅 IPv4」,另一台双栈的可以「优先 IPv6」,
放在全局配置里就没法表达这个差别。它落到 sing-box 的 `route.default_domain_resolver`
而**不是**已被 1.14.0 移除的 `domain_strategy` —— 后者写了不报错、只是静默失效,
所以有一条跨语言 golden 专门盯着它(`master/testdata/outbound/`,由真 sing-box 校验)。

`[c]` 显示的是主控**现场组装**的那份 sing-box 配置(不向 agent 索取):下发给它的就是
这份字节,所以两边必然一致,而且**离线的机器也能看** —— 那恰恰是最需要看的时候。
它是**原文,不脱敏**:这一页的用处就是「能不能直接 `sing-box -c` 跑起来」,
遮掉私钥的配置跑不起来。这些密钥本来就产自主控、库里也是明文(§9.1),
而它只在主控的终端上、按 `[c]` 才打开 —— 别对着屏幕录像或截图外传。

`[u]` 升级 agent 可以只升选中的这台、也可以一键升在线的全部。升级目标是**主控自己的版本**
(§11.1 两边共用一个版本号),产物按每台的架构挑,sha256 取不到就不下发。
TUI 只把指令排进队列,真正下发由 daemon 做 —— 两者是不同进程,WS 连接在 daemon 手上。

`[U]` 升级主控自己:界面临时退出去跑一键安装脚本,跑完回来。
**新二进制要退出 TUI 再进才生效**,当前这个进程还是老的。

节点表单里协议和所属机器是 `←/→` 选的,不是手打;`server_name` / `path` 只在
用得上它们的协议下出现。编辑节点时 tag 与协议不可改 —— 改 tag 会让历史流量和新流量
记到两个互不相认的账上,改协议等于换一整套密钥材料,那两件事都该走「删掉重建」。

`[b]` 那个是给管理员自己用的:VPS 厂商按**网卡**计费,而用户流量是 sing-box 记的账,
两者天生对不上。给某个用户绑上几台机器的网卡流量之后,**只有它的订阅响应头**
`subscription-userinfo` 换成那几台的网卡用量之和 —— 于是用任何一个代理客户端打开
订阅就能看到「这几台这个月烧了多少」,不用 ssh 上去查。订阅内容、用户自己的计费与
停用判定都不受影响(§10.3)。

TUI 与 daemon 是**两个独立进程**,只通过数据库交换状态:在界面里改完配置,
由正在跑的 daemon 在下次握手或下发时同步到各 agent(§4.1)。所以每个写操作
之后状态栏都会说明「什么时候生效」。

节点的密钥材料(reality 密钥对、自签证书、shadowsocks 服务端密钥)在 `node-add`
时**生成一次**存进库里,之后每次下发都用同一套 —— 下发时现生成会让客户端静默全部失联。

被控端:

```sh
cd agent && go build -tags with_quic,with_utls -o sbx-agent ./cmd/sbx-agent
cat > agent.toml <<'EOF'
server      = "ws://主控地址:18443/ws"    # TLS 时用 wss://,并填 fingerprint
token       = "上面 agent-add 打印的那串"
fingerprint = "sha256:…"                  # 主控证书指纹(TOFU,§1.3)
state_dir   = "/var/lib/sbx-agent"        # last-applied.json 落在这里
EOF
./sbx-agent agent.toml
```

订阅由主控的另一个监听提供(默认 `127.0.0.1:18081`,前面套 nginx 做 TLS):

```
GET /sub/<token>              按 User-Agent 自动选格式
GET /sub/<token>?type=clash   强制 Clash/Mihomo YAML
GET /sub/<token>?type=stats   强制流量统计页(浏览器打开时的默认)
```

这是「不做 Web 面板」(§2)的**唯一例外**,而且只读 —— 没有写接口、没有登录。

Telegram 通知(可选,`[telegram] enabled = true` 才启动):

```sh
./target/release/sbx --config c.toml user-tg-bind alice   # 生成绑定码
# 用户在 bot 里发 /bind <码>,之后可用 /usage /sub,并收到 80/90/100% 阈值告警与定时播报
```

同一个 `bot_token` 只能有一个长轮询,所以 bot 靠数据库里的租约做单实例互斥:
第二个进程会打一行日志然后跳过,不会两边互抢 update。

握手协议(§4.1):token 认证、两个 revision 的分别比对、同 agent 重连驱逐旧连接。
两端已经在真实节点上对接跑通(见上面 §13.2 / §13.3 那段)。

## 构建

```sh
# 主控(Rust)
cargo build --release          # 产物 target/release/sbx
cargo test                     # 全部测试

# agent(Go,独立 module)
# -tags with_quic,with_utls 是**必需**的:reality 要 uTLS,hysteria2/tuic 要 QUIC。
# 漏了会被 boxctl/buildtags.go 的编译期哨兵挡住(不会编出一个跑不了那些协议的二进制)。
cd agent && go build -tags with_quic,with_utls ./...   # 需要 Go 1.24.7+(见 agent/go.mod)
cd agent && go test  -tags with_quic,with_utls ./...   # 54 个测试;boxctl 那组会起真的 sing-box 实例

# §12.0 spike —— 改了 agent/tracker 就要重跑
cd spike && go mod tidy && go run .
```

CI(`.github/workflows/ci.yml`)跑的就是上面这些,外加 `clippy -D warnings`、
`gofmt -l`、两个 arch 的交叉编译,以及 `go test -race`(需要 cgo,只在 Linux runner 上跑)。
另有两个守卫:`agent/go.mod` 里出现 `replace`、或 `agent/patches/` 出现,直接 fail ——
那意味着 §0.3 结论三(不 fork sing-box)被打破了。

发布(`release.yml`)在打 `v*` tag 时触发,产物:

| 产物 | 形态 |
|---|---|
| `sbx` | `.tar.gz`,含 `packaging/config.example.toml` 与 `sbx.service` |
| `sbx-agent` | 裸二进制 + 独立 `.sha256`(`agent.upgrade` 按这个格式下载校验) |

打 tag 前:tag、`master/Cargo.toml` 的 `version`、`CHANGELOG.md` 的 `## v<x>` 三者必须一致。

部署用的示例配置与 systemd unit 在 [`packaging/`](packaging/)。
`sbx-agent.service` 里的 `Restart=always` **不是可选加固** ——
自升级的收尾动作是「替换自己的二进制然后退出」,没有它一次升级等于一次永久下线。
