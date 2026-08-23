package master

import (
	"encoding/json"
	"testing"
)

// 这组测试守的是**线格式**,不是 Go 侧的行为。
// 协议两边各写了一份 struct(见包注释),对齐只能靠断言字节。
// Rust 侧对应的守卫是 proto.rs 的 none_fields_are_omitted_on_the_wire。

func TestEventOmitsIDAndError(t *testing.T) {
	data, err := json.Marshal(event(MethodPong, Pong{EchoTS: 1234}))
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]json.RawMessage
	if err := json.Unmarshal(data, &m); err != nil {
		t.Fatal(err)
	}
	if _, ok := m["id"]; ok {
		// Rust 的 validate() 见到带 id 的 event 会直接断开连接。
		t.Errorf("event 不该带 id: %s", data)
	}
	if _, ok := m["error"]; ok {
		t.Errorf("无错误时不该出现 error 字段: %s", data)
	}
	if _, ok := m["payload"]; !ok {
		t.Errorf("payload 必须始终存在: %s", data)
	}
}

func TestReqAndRespCarryID(t *testing.T) {
	for _, env := range []*Envelope{
		req("7", MethodAgentHello, AgentHello{}),
		respOK("7", MethodConfigApply, okPayload{}),
		respErr("7", MethodConfigApply, "boom"),
	} {
		data, _ := json.Marshal(env)
		var m map[string]json.RawMessage
		_ = json.Unmarshal(data, &m)
		if _, ok := m["id"]; !ok {
			t.Errorf("%s 少了 id: %s", env.Kind, data)
		}
		if err := env.validate(); err != nil {
			t.Errorf("自己构造的信封没通过校验: %v", err)
		}
	}
}

func TestRespErrCarriesErrorAndNullPayload(t *testing.T) {
	data, _ := json.Marshal(respErr("1", MethodConfigApply, "配置解析失败"))
	var m map[string]json.RawMessage
	_ = json.Unmarshal(data, &m)
	if string(m["error"]) != `"配置解析失败"` {
		t.Errorf("error 字段不对: %s", data)
	}
	if string(m["payload"]) != "null" {
		t.Errorf("错误应答的 payload 应为 null: %s", data)
	}
}

func TestValidateRejectsStructurallyBadEnvelopes(t *testing.T) {
	cases := []struct {
		name string
		env  Envelope
		want error
	}{
		{"版本不符", Envelope{V: ProtoVersion + 1, Kind: KindEvent, Method: "x"}, errProtoVersion},
		{"req 缺 id", Envelope{V: ProtoVersion, Kind: KindReq, Method: "x"}, errMissingID},
		{"resp 缺 id", Envelope{V: ProtoVersion, Kind: KindResp, Method: "x"}, errMissingID},
		{"event 带 id", Envelope{V: ProtoVersion, ID: "1", Kind: KindEvent, Method: "x"}, errEventHasID},
		{"未知 kind", Envelope{V: ProtoVersion, Kind: "Req", Method: "x"}, errBadKind},
	}
	for _, tc := range cases {
		if got := tc.env.validate(); got != tc.want {
			t.Errorf("%s: validate() = %v, 期望 %v", tc.name, got, tc.want)
		}
	}
	ok := Envelope{V: ProtoVersion, ID: "1", Kind: KindReq, Method: "x"}
	if err := ok.validate(); err != nil {
		t.Errorf("合法信封被拒: %v", err)
	}
}

// kind 在 Rust 侧是 #[serde(rename_all = "lowercase")] 的 enum,
// 大写会解不出来,而信封解不出来在 recv_loop 里只是 warn —— 消息静默丢失。
func TestKindConstantsAreLowercase(t *testing.T) {
	for _, k := range []string{KindReq, KindResp, KindEvent} {
		for _, r := range k {
			if r >= 'A' && r <= 'Z' {
				t.Errorf("kind %q 含大写字母", k)
			}
		}
	}
}

func TestHelloOmitsNilIPs(t *testing.T) {
	data, _ := json.Marshal(AgentHello{Token: "t", ProtoVersion: ProtoVersion})
	var m map[string]json.RawMessage
	_ = json.Unmarshal(data, &m)
	// 主控的 mark_online 用 `ipv4 = COALESCE(?, ipv4)`:
	// 字段缺失 → None → null → 保留库里已有的值。发空串会把手工设的地址冲掉。
	if _, ok := m["ipv4"]; ok {
		t.Errorf("探测失败时 ipv4 不该上线: %s", data)
	}
	if _, ok := m["ipv6"]; ok {
		t.Errorf("探测失败时 ipv6 不该上线: %s", data)
	}
	// 两个 revision 反过来:它们在 Rust 侧是 i64 + #[serde(default)],
	// 0 是有意义的值(从未落过盘),必须上线。
	if _, ok := m["config_revision"]; !ok {
		t.Errorf("config_revision 必须上线,哪怕是 0: %s", data)
	}
	if _, ok := m["user_state_revision"]; !ok {
		t.Errorf("user_state_revision 必须上线,哪怕是 0: %s", data)
	}
}

func TestParseFingerprint(t *testing.T) {
	const hexDigest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
	good := []string{
		"sha256:" + hexDigest,
		hexDigest,
		"SHA256:" + hexDigest, // 大小写不敏感
		// openssl x509 -fingerprint 输出的带冒号写法,方便直接粘贴
		"01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:" +
			"01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef",
	}
	for _, fp := range good {
		raw, err := parseFingerprint(fp)
		if err != nil {
			t.Errorf("%q 应被接受: %v", fp, err)
			continue
		}
		if len(raw) != 32 {
			t.Errorf("%q 解出 %d 字节", fp, len(raw))
		}
	}
	bad := []string{"", "sha256:", "sha256:zz", hexDigest[:60], hexDigest + "00"}
	for _, fp := range bad {
		if _, err := parseFingerprint(fp); err == nil {
			t.Errorf("%q 应被拒绝", fp)
		}
	}
}

func TestMustJSONNeverReturnsEmpty(t *testing.T) {
	// payload 在 Rust 侧不是 Option,空字节会让整条信封解不出来。
	if got := string(mustJSON(nil)); got != "{}" {
		t.Errorf("nil → %q, 期望 {}", got)
	}
	if got := string(mustJSON(okPayload{})); got != "{}" {
		t.Errorf("okPayload{} → %q, 期望 {}", got)
	}
	// 不可序列化的值不该让消息发不出去。
	if got := string(mustJSON(make(chan int))); got != "{}" {
		t.Errorf("不可序列化值 → %q, 期望降级为 {}", got)
	}
	if got := string(mustJSON(json.RawMessage(`{"a":1}`))); got != `{"a":1}` {
		t.Errorf("RawMessage 应原样透传,得到 %q", got)
	}
}

// 空的用户列表必须序列化成 [] 而不是 null:
// Rust 侧是 Vec<UserCounter>(非 Option),null 会让整条 stats.report 解码失败。
func TestStatsReportEmptyUsersIsArray(t *testing.T) {
	data, _ := json.Marshal(StatsReport{CounterEpoch: "e", Users: []UserCounter{}})
	var m map[string]json.RawMessage
	_ = json.Unmarshal(data, &m)
	if string(m["users"]) != "[]" {
		t.Errorf("users = %s, 期望 []", m["users"])
	}
}

// utc_offset_secs **不带 omitempty**,所以 0 也必须出现在线上。
//
// 省掉它的话主控会读成「老 agent 没报」并回落到自己的时区 ——
// 于是一台真在 UTC 的机器会跟着主控的时区翻月,而不是自己的。
// 与上面两个 revision 完全同一个理由。
func TestHelloAlwaysCarriesTheOffset(t *testing.T) {
	data, err := json.Marshal(AgentHello{Token: "t", ProtoVersion: ProtoVersion})
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]json.RawMessage
	if err := json.Unmarshal(data, &m); err != nil {
		t.Fatal(err)
	}
	raw, ok := m["utc_offset_secs"]
	if !ok {
		t.Fatalf("utc_offset_secs 必须上线,哪怕是 0: %s", data)
	}
	if string(raw) != "0" {
		t.Errorf("零值该序列化成 0,得到 %s", raw)
	}
}
