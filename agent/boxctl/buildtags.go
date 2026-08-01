//go:build !with_quic || !with_utls

// sbx-agent **必须**带 `-tags with_quic,with_utls` 构建。
//
// 这两个 tag 不是可选优化,少了任何一个都会让部分协议在运行时才炸:
//
//	with_utls  reality 依赖 uTLS 做指纹伪装。少了它,vless-reality 节点在
//	           box.New 时报 "uTLS, which is required by reality is not included
//	           in this build" —— 而 vless-reality 是本项目的默认协议。
//	with_quic  hysteria2 / tuic 是 QUIC 协议。少了它报
//	           "QUIC is not included in this build"。
//
// 失败的时机很糟:主控已经把配置下发出去了,agent 回一条 config.apply 失败,
// 主控保留旧 revision 并重试 —— 表现是「加了节点但一直不生效」,
// 而错误信息藏在 agent 的日志里。所以把它提前到编译期。
//
// **这是允许的两个 build tag 的全部**(DESIGN.md §12.0 预见到了这一点)。
// 出现别的 tag —— 尤其是 `with_v2ray_api` —— 说明 §0.2 被误读了:
// 流量统计走的是 ConnectionTracker,不需要 v2ray API(§7.1)。
package boxctl

// 下面这行引用了一个不存在的标识符,于是编译直接失败,
// 错误信息 "undefined: sbx_agent_must_be_built_with_tags_with_quic_and_with_utls"
// 本身就是修复说明。
//
// 正确的构建命令:
//
//	go build -tags with_quic,with_utls ./cmd/sbx-agent
var _ = sbx_agent_must_be_built_with_tags_with_quic_and_with_utls
