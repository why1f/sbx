# CHANGELOG

版本号只有一个:`master/Cargo.toml` 的 `version`。tag、它、以及本文件里的
`## v<x>` 标题三者必须一致 —— `release.yml` 会在打 tag 时校验,不一致直接 fail。
agent 是同一个版本号,通过 `-ldflags "-X main.Version=…"` 注入(§11.1)。

## v0.1.2

### 修

- **`install.sh` 的错误提示会误导管道用户。** 没装过又不指定目标时,它一律提示
  `install.sh agent` —— 但 `curl … | sh` 的场景下照抄过去写成 `sh master`,
  shell 会把 `master` 当成**脚本文件名**去打开,报 `cannot open master`。
  现在提示按**实际调用方式**生成:管道就给 `curl … | sh -s -- agent`,
  当文件跑就给 `./install.sh agent`。
- 顺带支持 `SBX_TARGET=agent`,任何调用形式都能用,不必记 `-s --`。

## v0.1.1

### 新增

- **一键安装 / 升级脚本** `packaging/install.sh`(POSIX sh,被控机上可能只有 dash)。

      curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | sh

  不带参数时**按本机已装的东西升级**:装了什么就升什么,一个都没装则报错并让你
  显式选 —— 免得在一台只跑 agent 的机器上莫名多出一个主控。
  已是最新版就什么都不做;下载后**强制校验 sha256**,取不到校验和宁可拒装;
  替换走同目录 tmp + `mv`(跨文件系统的 mv 不是原子的);
  只在单元**本来就在跑**时才重启它。

- `sbx-agent --version`。v0.1.0 没有这个,导致升级脚本认不出已装版本、每次都重装。
  输出走 stdout 且不带日志前缀,脚本可以直接取。

### 修

- 新版 clippy 的 `some_filter` lint(`Some(x).filter(|_| cond)` → `cond.then_some(x)`)。

## v0.1.0

首个版本。

### 主控(Rust,MIT)

- WS 集群服务端:token 握手、同 agent 重连驱逐旧连接、心跳与 `last_seen` 记账。
- 握手补齐(§4.1):`config_revision` 与 `user_state_revision` **分别**比对,
  各自触发 `config.apply` / `user.state`。两者是独立计数器,合并会造成
  「离线期间的禁用永久失效」或「高频路径退化成全量 box 重建」二者之一。
- 上报入库走 epoch/delta(§5.2):agent 报单调累计值,主控按 epoch 判断
  取差值还是取全量。agent 重启、box 重建、机器重启三种情况分别有对应输入的测试。
- TLS(§1.3):证书不存在时自签一张,启动打印 SHA-256 指纹供 agent TOFU 固定。
  指纹算法与 `openssl x509 -fingerprint -sha256` 交叉验证过。
- 配额与到期巡检(§6.3):月重置(含短月)、配额超限、到期、跨 agent 流量求和。
- CLI:agent / node / user 的增删改查,`agent-add` 生成 token(只显示一次),
  `user-sub` 打印订阅地址与分享链接。
- 订阅导出(§10):base64 分享链接 + Clash/Mihomo YAML,由 `/sub/:token` 提供。
  节点地址取自**该节点所属 agent** 的 `ipv4/ipv6`(旧项目是本机 IP 探测),
  保留中转覆盖与 `use_public_base_as_server` 的优先级链。
  订阅服务是「不做 Web 面板」的唯一例外,且只读:没有写接口、没有登录。
- TUI(§8,`sbx tui`):服务管理 / 节点 / 用户三页。
  agent 列表是**两行式**的,带自绘的多段渐变进度条(ratatui 的 `Gauge` 只支持单色,用不了)。
  网速由 TUI 自己按刷新节奏对 `agent_nic_traffic` 做差算出 —— 它与 daemon 是
  两个进程,读不到那边内存里的采样;`boot_id` 变化时显示 `--` 而不是一个爆炸的数字。
  **不限流量的行不画进度条**:一根满条读起来是「用完了」,与「不限」正好相反
  (这一处与参考图 `1.png` 不同,是有意的)。
- 配置组装:**八个协议全部支持**(§9.1)。密钥材料(reality 密钥对、自签证书、
  shadowsocks 服务端密钥)在**建节点时**生成一次并存进 `params_json` ——
  下发时现生成会让每次 `config.apply` 都换一套密钥,客户端静默全部失联。
  证书走**内联 PEM** 而不是路径:主控与 agent 不在同一台机器上。
  生成结果有跨语言 golden 测试:Rust 生成八份配置,agent 侧用**真 sing-box** 校验。

### agent(Go,GPLv3)

- 内嵌 sing-box(v1.14.0-beta.3),通过 `box.New()` / `Start()` / `Close()` API 控制,
  **不 fork、不打补丁**。构建必须带 `-tags with_quic,with_utls`
  (reality 依赖 uTLS,hysteria2 / tuic 依赖 QUIC);漏了会被编译期哨兵挡住,
  而不是编出一个跑不了那几个协议的二进制。
- `ConnectionTracker` 按 **(用户, inbound tag)** 二元组记账(§7.1)。
  计数器活在 tracker 里而非 box 里,所以 box 重建不清零 —— 这是 §5.2 的前提。
- 热重载按 §7.4 的顺序:`box.New` → `old.Close()` → `AppendTracker` → `Start()`,
  Start 失败回滚到旧配置。同端口重载与回滚都有跑真实实例的测试。
- `user.state` 只换禁用名单,**不重建 box**;禁用只挡新连接(§7.5)。
- WS 客户端:指纹钉扎的 wss、指数退避重连(1→60s)、对每一条 req 都回 resp
  (漏一条会让主控的串行补发卡到 RPC 超时)。
- `last-applied.json` 原子落盘,兼作冷启动配置源 —— 主控挂着时节点照常服务。

- 流量统计页(`stats_html`):浏览器打开订阅地址时返回它 —— 用量进度条、
  每个节点的分享链接 + 内联 SVG 二维码。**仍然只读**,没有任何管理能力。
  页面上每段动态文本都做了三种上下文的转义(HTML 文本 / 属性 / onclick 里的 JS 字符串)。
- Telegram 通知(§9.1):80/90/100% 三档阈值告警 + 定时流量播报,
  用户可在 bot 里自助查用量、取订阅、改通知开关。
  **告警去重按档位而不是百分比**:库里记「已通知到哪一档」,只在上升时推 ——
  按百分比去重会让 80.1% → 80.2% 也算一次变化,每 30 秒推一条。
  月重置后档位会被降回去,否则那个用户再也收不到告警。
  单实例租约(`tg_bot_lease`)保证同一个 bot_token 只有一个长轮询:
  多开时 Telegram 会 409,而且两边都随机丢 update,表现成「命令时灵时不灵」。

### 已知不完整

- **agent 自升级**(`agent.upgrade`)没有实测过 —— 它要一个真实的发布产物才能验。
- TUI 的交互只在无头渲染(`TestBackend`)和按键单测里覆盖过,
  没有人在真实终端里从头点到尾。
- Telegram bot 的收发只对着 Telegram API 的错误路径验过(假 token → 401 → 退避),
  没有对着真 bot 跑过一遍完整会话。相对旧项目也**刻意少了**管理员在聊天里
  改用户配额/到期的那套表单 —— 那些操作要推进 revision、下发配置,
  放在一个可能因为网络抖动重放的界面上并不合适,CLI 和 TUI 上都有。

### 已验证

在一台 Oracle ARM(Ubuntu 22.04 aarch64)上跑通了 §13.2 / §13.3:
1 个主控 + 2 个 agent + 真客户端流量,跨 agent 求和与断连恢复都逐字节对上。
驱动程序与复跑步骤在 `e2e/`。`-race` 也在那台机器上跑过,干净。
