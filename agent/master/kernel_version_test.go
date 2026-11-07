package master

import (
	"runtime/debug"
	"testing"
)

func TestKernelVersionIdentifiesEmbeddedDependencyAndExplicitVariant(t *testing.T) {
	info := &debug.BuildInfo{Deps: []*debug.Module{{Path: "github.com/sagernet/sing-box", Version: "v1.14.0"}}}
	if got := kernelVersion("unknown", info); got != "1.14.0" {
		t.Fatal(got)
	}
	if got := kernelVersion("1.15.0-alpha.13", info); got != "1.15.0-alpha.13" {
		t.Fatal(got)
	}
	info.Deps[0].Replace = &debug.Module{Path: "../local"}
	if got := kernelVersion("unknown", info); got != "unknown" {
		t.Fatal(got)
	}
}
