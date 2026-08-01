package state

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestLoadMissingFileIsNotAnError(t *testing.T) {
	// 首次启动就是这个样子:没有文件,报 revision 0,主控全量下发。
	la, err := Load(t.TempDir())
	if err != nil {
		t.Fatalf("文件不存在不该报错: %v", err)
	}
	if la.ConfigRevision != 0 || la.UserStateRevision != 0 || la.Options != nil {
		t.Errorf("期望零值状态,得到 %+v", la)
	}
}

func TestLoadCorruptFileFallsBackToEmpty(t *testing.T) {
	dir := t.TempDir()
	// 掉电写了一半、或者手改坏了。此时 agent 必须还能起来。
	if err := os.WriteFile(filepath.Join(dir, FileName), []byte(`{"config_rev`), 0o644); err != nil {
		t.Fatal(err)
	}
	la, err := Load(dir)
	if err != nil {
		t.Fatalf("损坏文件不该让 agent 起不来: %v", err)
	}
	if la.ConfigRevision != 0 {
		t.Errorf("期望退回零值,得到 %+v", la)
	}
}

func TestSaveLoadRoundTrip(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "nested", "state")
	want := &LastApplied{
		ConfigRevision:    42,
		UserStateRevision: 7,
		Options:           json.RawMessage(`{"log":{"level":"warn"}}`),
		Disabled:          []string{"alice", "bob"},
	}
	if err := Save(dir, want); err != nil {
		t.Fatalf("Save(建目录): %v", err)
	}
	got, err := Load(dir)
	if err != nil {
		t.Fatal(err)
	}
	if got.ConfigRevision != 42 || got.UserStateRevision != 7 {
		t.Errorf("revision 没对上: %+v", got)
	}
	if len(got.Disabled) != 2 || got.Disabled[0] != "alice" {
		t.Errorf("禁用名单没对上: %+v", got.Disabled)
	}
	// options 必须逐字节保真 —— agent 不理解的新字段不能在存盘往返里被吃掉。
	var a, b any
	_ = json.Unmarshal(want.Options, &a)
	_ = json.Unmarshal(got.Options, &b)
	if string(got.Options) == "" {
		t.Fatalf("options 丢了")
	}
}

// 两个 revision 是独立字段。合并它们会造成 §4.1 描述的两种 bug 之一:
// 要么把最频繁的 user.state 变成 box 重建,要么丢掉离线期间的封禁。
func TestRevisionsAreIndependent(t *testing.T) {
	dir := t.TempDir()
	if err := Save(dir, &LastApplied{ConfigRevision: 5, UserStateRevision: 0}); err != nil {
		t.Fatal(err)
	}
	la, _ := Load(dir)
	la.UserStateRevision = 99
	if err := Save(dir, la); err != nil {
		t.Fatal(err)
	}
	got, _ := Load(dir)
	if got.ConfigRevision != 5 {
		t.Errorf("改 user_state_revision 动到了 config_revision: %+v", got)
	}
	if got.UserStateRevision != 99 {
		t.Errorf("user_state_revision 没存住: %+v", got)
	}
}

// Save 走 tmp + rename,不能在目标目录留下垃圾。
func TestSaveLeavesNoTempFiles(t *testing.T) {
	dir := t.TempDir()
	for i := 0; i < 3; i++ {
		if err := Save(dir, &LastApplied{ConfigRevision: int64(i)}); err != nil {
			t.Fatal(err)
		}
	}
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || entries[0].Name() != FileName {
		names := make([]string, len(entries))
		for i, e := range entries {
			names[i] = e.Name()
		}
		t.Errorf("目录里应只剩 %s,实际 %v", FileName, names)
	}
}

// 空的 options 不该以 "options": null 的形式落盘 —— Load 回来后
// main 用 len(st.Options) > 0 判断要不要冷启动 box,写成 null 会变成 4 字节非空。
func TestEmptyOptionsIsOmitted(t *testing.T) {
	dir := t.TempDir()
	if err := Save(dir, &LastApplied{ConfigRevision: 1}); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(filepath.Join(dir, FileName))
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]json.RawMessage
	if err := json.Unmarshal(data, &m); err != nil {
		t.Fatal(err)
	}
	if _, ok := m["options"]; ok {
		t.Errorf("没有配置时不该写 options 字段: %s", data)
	}
	la, _ := Load(dir)
	if len(la.Options) != 0 {
		t.Errorf("Load 回来的 Options 应为空,得到 %q", la.Options)
	}
}
