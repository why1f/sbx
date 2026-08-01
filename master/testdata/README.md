# testdata

`fixture-cert.pem` / `fixture-key.pem` 是**一次性的自签测试证书**,和任何部署无关。

它进版本库是必需的:`inbounds/*.json` 那八份 golden 配置里内联了这张证书的 PEM,
而 golden 是逐字节比对的。证书带随机序列号和签发时间,每次生成都不同 ——
不提交的话,每跑一次测试 golden 就变一次,diff 里全是噪声。

**这把私钥没有任何安全价值**:它只用来生成 golden、以及被 agent 侧的 Go 测试
喂给真 sing-box 做校验。密钥扫描器会标记它,那是误报。

`inbounds/*.json` 由 `service::tests::eight_protocols_match_golden_configs` 生成,
由 `agent/boxctl` 的 `TestMasterGoldenConfigsAreAccepted` 喂给真正的 sing-box 校验。
改了 `build_inbound` 之后:先跑 `cargo test` 看 diff,确认改动是有意的,
再用同目录下的 `.actual` 覆盖 `.json`,最后跑一遍 agent 侧的 Go 测试。
