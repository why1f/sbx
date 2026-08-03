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

**master 侧完整可用:CLI、daemon(TLS)、握手补齐、配额自动化 均已实现并通过测试。**
**agent 侧已实现并通过测试:WS 客户端、box 热重载、tracker、sysinfo 采集。**
剩下的是 TUI(§8)、订阅导出(§10)、以及 `build_inbound` 里其余 7 个协议。

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
| TUI(§8) | ✅ 已完成,两行式 agent 列表 + 自绘渐变进度条 + token 管理 |
| 流量统计页(`stats_html`) | ✅ 已完成,浏览器打开订阅地址即是它 |
| Telegram 通知(§9.1) | ✅ 已完成,单实例租约 + 阈值告警去重 + 定时播报 |
| §13.2 / §13.3 端到端 | ✅ **已在真实 ARM Linux 上跑通**(1 主控 + 2 agent + 真流量) |

Rust 侧 290 个测试通过(`cargo test`),
Go 侧 46 个测试通过(`cd agent && go test -tags with_quic,with_utls ./...`,
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
> 仍未实测的只剩 **agent 自升级**(`agent.upgrade`:下载、校验 sha256、
> rename 覆盖自己再退出)—— 它需要一个真实的发布产物才能验。

## 安装

```sh
# 被控机
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | sh -s -- agent
# 主控机
curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | sh -s -- master
```

**管道形式必须写 `sh -s -- <目标>`。** 少了 `-s --` 的话,`sh master` 会把 `master`
当成脚本文件名去打开,报 `cannot open master`。嫌绕就用环境变量,哪种调用形式都行:

```sh
curl -fsSL .../install.sh | SBX_TARGET=agent sh
```

不带参数重跑就是**升级**:认出本机已装的部分,已是最新版就什么都不做。
下载后强制校验 sha256,取不到校验和宁可拒装。部署细节见 [`DEPLOY.md`](DEPLOY.md)。

## 已经能跑的东西

```sh
cargo test                      # 290 个测试
cargo build --release           # 产物 target/release/sbx

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

# 或者直接开界面(§8):服务管理 / 节点 / 用户 三页,Tab 切换
./target/release/sbx --config c.toml tui
```

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

握手协议(§4.1)已经可用:token 认证、两个 revision 的分别比对、
同 agent 重连驱逐旧连接。agent 端也已实现,但**两端还没有在真实节点上对接过**——
目前的验证是各自的测试(master 的端到端测试走真实 socket,agent 对着假主控跑完整握手)。

## 构建

```sh
# 主控(Rust)
cargo build --release          # 产物 target/release/sbx
cargo test                     # 全部测试

# agent(Go,独立 module)
# -tags with_quic,with_utls 是**必需**的:reality 要 uTLS,hysteria2/tuic 要 QUIC。
# 漏了会被 boxctl/buildtags.go 的编译期哨兵挡住(不会编出一个跑不了那些协议的二进制)。
cd agent && go build -tags with_quic,with_utls ./...   # 需要 Go 1.24.7+(见 agent/go.mod)
cd agent && go test  -tags with_quic,with_utls ./...   # 46 个测试;boxctl 那组会起真的 sing-box 实例

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
