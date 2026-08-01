package tracker

import "testing"

// 这些测试只依赖 Tracker 自己的逻辑,不碰 sing-box 的数据路径 ——
// 真实流量是否记到账由 `spike/` 验(§12.0),那需要一条完整的代理链路。
// 两边分工不同,都要保留。

func TestCounterKeyIncludesTag(t *testing.T) {
	tr := New()
	// 同一用户、两个不同 inbound tag —— 必须是两条独立记录。
	// 这是「同一 agent 上一个用户多个节点」的场景(§4.3),
	// 只按用户名记账会让它们塌成一个数字。
	a := tr.counter(ctrKey{user: "alice", tag: "vless-in"})
	b := tr.counter(ctrKey{user: "alice", tag: "trojan-in"})
	if a == b {
		t.Fatal("同一用户的不同 tag 必须是不同的计数器")
	}
	a.up.Add(100)
	b.up.Add(7)

	snap := tr.Snapshot()
	if len(snap) != 2 {
		t.Fatalf("应有 2 条记录,得到 %d", len(snap))
	}
	byTag := map[string]int64{}
	for _, s := range snap {
		if s.Name != "alice" {
			t.Errorf("用户名应为 alice,得到 %q", s.Name)
		}
		byTag[s.Tag] = s.Up
	}
	if byTag["vless-in"] != 100 || byTag["trojan-in"] != 7 {
		t.Errorf("计数不对: %v", byTag)
	}
}

func TestCounterIsStableAcrossLookups(t *testing.T) {
	tr := New()
	k := ctrKey{user: "bob", tag: "in"}
	if tr.counter(k) != tr.counter(k) {
		t.Fatal("同一 key 必须返回同一个计数器实例")
	}
}

func TestSnapshotDoesNotReset(t *testing.T) {
	// §5.3:agent 上报单调累计值,永不 reset。delta 由主控算。
	tr := New()
	tr.counter(ctrKey{user: "a", tag: "in"}).up.Add(50)

	first := tr.Snapshot()
	second := tr.Snapshot()
	if len(first) != 1 || len(second) != 1 {
		t.Fatalf("快照条数不对: %d / %d", len(first), len(second))
	}
	if first[0].Up != 50 || second[0].Up != 50 {
		t.Errorf("快照不该清零计数器: %d then %d", first[0].Up, second[0].Up)
	}
}

func TestSetDisabledReplacesWholeList(t *testing.T) {
	// §4.2:传全量名单,不是增量。幂等。
	tr := New()
	tr.SetDisabled([]string{"alice", "bob"})
	if !tr.isDisabled("alice") || !tr.isDisabled("bob") {
		t.Fatal("两个用户都该被禁用")
	}

	// 新的全量名单只含 bob → alice 应自动解禁
	tr.SetDisabled([]string{"bob"})
	if tr.isDisabled("alice") {
		t.Error("alice 不在新名单里,应已解禁")
	}
	if !tr.isDisabled("bob") {
		t.Error("bob 仍在名单里,应保持禁用")
	}

	// 空名单 = 全部解禁
	tr.SetDisabled(nil)
	if tr.isDisabled("bob") {
		t.Error("空名单应解禁所有人")
	}
}

func TestDisablingDoesNotTouchCounters(t *testing.T) {
	// 禁用只挡新连接,**流量仍照记**(§7.5)——
	// 已建立的连接继续跑,账不能错。
	tr := New()
	c := tr.counter(ctrKey{user: "alice", tag: "in"})
	c.up.Add(10)
	tr.SetDisabled([]string{"alice"})
	c.up.Add(5) // 已建立的连接继续传输

	snap := tr.Snapshot()
	if len(snap) != 1 || snap[0].Up != 15 {
		t.Errorf("禁用不该影响计数: %+v", snap)
	}
}
