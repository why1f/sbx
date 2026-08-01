# §12.0 spike

**已跑通**(sing-box v1.14.0-beta.3)。这个目录现在的身份变了:
它不再是「动手前先验一遍」的探路程序,而是一份**自断言的回归测试** ——
全部断言通过才 `exit 0`,失败 `exit 1`,150 秒看门狗超时 `exit 2`。CI 里跑的就是它。

> **改了 `agent/tracker` 就要同步改这里**,否则验的不是同一份东西。
> 这个目录不属于 sbx 工作区(自己一个 `go.mod`,不在根 `Cargo.toml` 里,
> 也不被 `agent/go.mod` 引用),所以编译器不会替你发现两边的漂移。

## 跑法

```sh
cd spike
go mod tidy
go run .        # 全自动,不需要人肉接客户端
```

## 三个验证点(全部已证实)

### 1. 统计确实到账 ✅

spike 自己起一个 vless 服务端 box 和一个客户端 box,推 128 KiB 上行 + 2 MiB 下行,
然后断言 tracker 里的计数。

- 计数器在增长,`user` 与配置里的 `spike-user` 一致;
- **up / down 方向没搞反**:16:1 的不对称是刻意选的 —— 方向反了必然失败,
  而不是勉强落在误差里。容差 32 KiB(握手与协议开销)。
- 结论:`NewInt64CounterConn(conn, read, write)` 的 **read 是上行、write 是下行**,
  与上游 `service/ssmapi/traffic.go:116` 的用法一致。

### 2. `rejected(conn)` 的客户端观感 ✅

把用户加进 `disabled` 再连一次:

- **3ms 内**收到一次普通的连接中断,不是卡住超时;
- 错误文本经过泄漏扫描(`disabl` / `quota` / `expire` / `ban` / `suspend` 五个词根),
  没有命中 —— 不向未授权方泄露账户状态;
- tracker 的调用点确实在**选定 outbound 之后**,所以会浪费一次上游拨号。
  代价可接受。

**「禁用也走 box 重建」那条退路不启用。** 它仍然有效,留作 §7.5 触发条件成立时的备选。

### 3. `InboundContext` 上 inbound tag 的字段名 ✅

字段名就是 `md.Inbound`,填的是**配置里的 tag 字符串**(实测 `"vless-in"`)。
另有 `md.InboundType == "vless"` —— 那是**协议名不是 tag**,不要用它记账。

这个字段是 `(用户, tag)` 记账口径能否成立的前提(§4.3 / §7.1)——
少了它,同一 agent 上一个用户的多个节点流量会塌成一个数字,
而 master 的 `user_traffic` 主键是 `(user_id, node_id)`,填不进去。

## 顺带确认的两件事

- `include.Context()` **不能少**。少了 `box.New` 直接报 `missing inbound registry in context`。
- `AppendTracker` **必须在 `Start()` 之前**:它内部是无锁 append,有流量时并发调用会 race。

> ⚠️ **build tag 这一项 spike 没能回答。** spike 只跑了**无 TLS 的 vless**,
> 所以它「不需要 build tag」的结论是**错的** —— 只是它的样本没碰到。
> 真正的答案来自 §9.1 的跨语言 golden 测试:
> **agent 必须带 `-tags with_quic,with_utls` 构建**
> (reality 依赖 uTLS,hysteria2 / tuic 依赖 QUIC)。
> `agent/boxctl/buildtags.go` 里有编译期哨兵,漏了直接编不出来。
>
> 这两个就是允许的全部 tag。如果有人在 `release.yml` 里加了 `-tags with_v2ray_api`,
> 说明 §0.2 被误读了。
>
> 教训值得记一笔:**一个只覆盖了一种协议的 spike,不能用来下关于八种协议的结论。**

## 与 `agent/boxctl` 测试的分工

三边都要有,验的不是一回事:

| | 验什么 | 需要什么 |
|---|---|---|
| `spike/` | 流量**记到没记到**(数据路径) | 一条完整的代理链路 |
| `agent/boxctl` 的 boxctl_test | **实例切换与回滚**(§7.4 的顺序) | 只要端口 |
| `agent/boxctl` 的 golden_test | **主控生成的八协议配置 sing-box 认不认** | 只要 `master/testdata/` |
