package boxctl

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// 跨语言契约测试(§9.1)。
//
// 主控(Rust)生成 sing-box 配置,agent(Go)消费它。两边对字段名和结构的理解
// 只在**运行时**才碰面:少一个必填字段、多一个 sing-box 不认的字段
// (典型的是把 reality 的 public_key 写进 inbound),表现都是
// 「agent 回一条 config.apply 失败」—— 而那时节点已经在线上了。
//
// 这组测试把 master/testdata/inbounds/*.json 喂给**真正的 sing-box**
// (agent/go.mod 里 require 的那个版本),把那个时刻提前到 CI。
//
// golden 由 Rust 侧的 `service::tests::eight_protocols_match_golden_configs` 生成。
// 那边改了 build_inbound 而没更新 golden 会先失败;golden 更新了但 sing-box 不认,
// 会在这里失败。两个方向都堵上了。
const goldenDir = "../../master/testdata/inbounds"

func TestMasterGoldenConfigsAreAccepted(t *testing.T) {
	entries, err := os.ReadDir(goldenDir)
	if err != nil {
		// 只在 sbx 仓库里跑得起来。单独 clone agent/ 的场景下跳过而不是失败。
		t.Skipf("读不到 %s(不在完整仓库里?): %v", goldenDir, err)
	}

	c := New(nil)
	found := 0
	for _, e := range entries {
		name := e.Name()
		if !strings.HasSuffix(name, ".json") {
			continue // 跳过 .json.actual 之类的中间产物
		}
		found++
		t.Run(strings.TrimSuffix(name, ".json"), func(t *testing.T) {
			raw, err := os.ReadFile(filepath.Join(goldenDir, name))
			if err != nil {
				t.Fatal(err)
			}
			var inbound json.RawMessage = raw
			cfg, err := json.Marshal(map[string]any{
				"log":       map[string]any{"level": "error"},
				"inbounds":  []json.RawMessage{inbound},
				"outbounds": []map[string]any{{"type": "direct", "tag": "direct"}},
			})
			if err != nil {
				t.Fatal(err)
			}
			// Check 走的是 box.New 的完整解析与装配,只是不 Start ——
			// 所以不占端口,八个协议可以在同一个测试进程里连着验。
			if err := c.Check(cfg); err != nil {
				t.Errorf("sing-box 不接受主控生成的 %s 配置: %v\n配置:%s", name, err, raw)
			}
		})
	}

	// 少了文件比测试通过更危险:一个空目录会让这组测试静默变成 no-op。
	if found != 8 {
		t.Errorf("期望 8 个协议的 golden,实际找到 %d 个 —— §9.1 要求八个协议都能生成配置", found)
	}
}

// 主控绝不该把 reality 的 public_key 放进 inbound:sing-box 的 reality
// inbound 没有这个字段。Rust 侧有对应的断言,这里从**消费端**再确认一次。
func TestRealityGoldenHasNoPublicKey(t *testing.T) {
	raw, err := os.ReadFile(filepath.Join(goldenDir, "vless-reality.json"))
	if err != nil {
		t.Skipf("读不到 golden: %v", err)
	}
	var ib struct {
		TLS struct {
			Reality map[string]json.RawMessage `json:"reality"`
		} `json:"tls"`
	}
	if err := json.Unmarshal(raw, &ib); err != nil {
		t.Fatal(err)
	}
	if _, ok := ib.TLS.Reality["public_key"]; ok {
		t.Error("reality inbound 里出现了 public_key —— 它只属于客户端侧(订阅)")
	}
	if _, ok := ib.TLS.Reality["private_key"]; !ok {
		t.Error("reality inbound 缺 private_key")
	}
}

// 出站地址族策略的 golden(§9.1 同一条契约,范围不同)。
//
// 上面那组只有 inbound;这组是**整份配置** —— 出站策略写的是顶层 route / dns。
// 要盯的三件事都只有真 sing-box 能答:
//  1. `route.default_domain_resolver` 的字段名对不对(1.12.0 引入);
//  2. 它 `server` 指向的 tag 在 dns.servers 里存不存在(指不到就起不来);
//  3. `dns.servers` 用的是 1.12+ 的 `{"type":"local"}` 而不是已移除的
//     `{"address":"local"}`。
//
// 另外直接断言配置里没有 `domain_strategy`。那个字段 1.14.0 已移除,而这里
// require 的就是 1.14.0-beta.3 —— 但它**移除得没有声音**:实测 Check 会照常
// 返回 nil,字段被静默丢掉。所以这一条不能指望 sing-box 帮我们发现,
// 只能在这里自己按字符串挡:线上写错了是不会有任何报错的。
const outboundGoldenDir = "../../master/testdata/outbound"

func TestOutboundStrategyConfigsAreAccepted(t *testing.T) {
	entries, err := os.ReadDir(outboundGoldenDir)
	if err != nil {
		t.Skipf("读不到 %s(不在完整仓库里?): %v", outboundGoldenDir, err)
	}

	c := New(nil)
	found := 0
	for _, e := range entries {
		name := e.Name()
		if !strings.HasSuffix(name, ".json") {
			continue // 跳过 .json.actual 之类的中间产物
		}
		found++
		t.Run(strings.TrimSuffix(name, ".json"), func(t *testing.T) {
			raw, err := os.ReadFile(filepath.Join(outboundGoldenDir, name))
			if err != nil {
				t.Fatal(err)
			}
			if strings.Contains(string(raw), "domain_strategy") {
				t.Fatalf("%s 里出现了 domain_strategy —— 该字段 1.14.0 已移除,"+
					"且是被静默丢掉(Check 仍然通过),线上不会有任何报错,"+
					"表现只是「策略改了但没生效」", name)
			}
			// 这里的 golden 已经是整份配置(带 log/inbounds/outbounds),
			// 不像 inbound 那组还要现包一层。
			if err := c.Check(raw); err != nil {
				t.Fatalf("sing-box 拒绝了 %s:%v\n%s", name, err, raw)
			}
		})
	}
	if found == 0 {
		t.Fatal("一个 golden 都没读到 —— 先在 master 侧跑 outbound_strategies_match_golden_configs")
	}
}
