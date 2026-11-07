package master

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/why1f/sbx/agent/boxctl"
	"github.com/why1f/sbx/agent/config"
	"github.com/why1f/sbx/agent/state"
	"github.com/why1f/sbx/agent/tracker"
)

func TestUserStatePersistenceFailureKeepsDurableRevisionAndRetries(t *testing.T) {
	dir := t.TempDir()
	original := &state.LastApplied{ConfigRevision: 1, UserStateRevision: 1}
	if err := state.Save(dir, original); err != nil {
		t.Fatal(err)
	}
	blocked := filepath.Join(dir, "blocked")
	if err := os.WriteFile(blocked, []byte("block"), 0600); err != nil {
		t.Fatal(err)
	}
	tr := tracker.New()
	cfg := &config.Config{StateDir: blocked}
	c := NewConn(cfg, tr, boxctl.New(tr), original, "epoch")
	update := UserState{UserStateRevision: 2, Disabled: []string{"alice"}}
	if _, err := c.applyUserState(update); err == nil {
		t.Fatal("must report persistence failure")
	}
	if c.st.UserStateRevision != 1 {
		t.Fatal("handshake must retain durable revision")
	}
	cfg.StateDir = dir
	if _, err := c.applyUserState(update); err != nil {
		t.Fatal(err)
	}
	saved, err := state.Load(dir)
	if err != nil {
		t.Fatal(err)
	}
	if saved.UserStateRevision != 2 || len(saved.Disabled) != 1 || saved.Disabled[0] != "alice" {
		t.Fatalf("retry failed to persist the ban: %+v", saved)
	}
}

func TestConfigPersistenceRetryDoesNotRestartRunningBox(t *testing.T) {
	dir := t.TempDir()
	blocked := filepath.Join(dir, "blocked")
	if err := os.WriteFile(blocked, []byte("block"), 0600); err != nil {
		t.Fatal(err)
	}
	tr := tracker.New()
	bc := &persistenceBox{Controller: boxctl.New(tr)}
	cfg := &config.Config{StateDir: blocked}
	c := NewConn(cfg, tr, bc.Controller, &state.LastApplied{}, "epoch")
	c.box = bc
	update := ConfigApply{Revision: 2, Options: json.RawMessage(`{"log":{"disabled":true},"inbounds":[],"outbounds":[{"type":"direct","tag":"direct"}]}`)}
	if _, err := c.applyConfig(update); err == nil {
		t.Fatal("must report persistence failure")
	}
	if bc.applies != 1 || c.st.ConfigRevision != 0 {
		t.Fatal("running config must not be acknowledged as durable")
	}
	cfg.StateDir = dir
	if _, err := c.applyConfig(update); err != nil {
		t.Fatal(err)
	}
	if bc.applies != 1 {
		t.Fatal("persistence retry restarted box")
	}
	saved, err := state.Load(dir)
	if err != nil || saved.ConfigRevision != 2 {
		t.Fatalf("retry did not persist: %+v %v", saved, err)
	}
}

// Keep this test under -race without starting sing-box's upstream network monitor.
type persistenceBox struct {
	*boxctl.Controller
	applies int
}

func (b *persistenceBox) Apply(json.RawMessage) error {
	b.applies++
	return nil
}

func (b *persistenceBox) Matches(json.RawMessage) bool { return b.applies > 0 }
