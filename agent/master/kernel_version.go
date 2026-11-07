package master

import (
	"runtime/debug"
	"strings"

	C "github.com/sagernet/sing-box/constant"
)

var currentKernelVersion = func() string {
	info, _ := debug.ReadBuildInfo()
	return kernelVersion(C.Version, info)
}()

func kernelVersion(linked string, info *debug.BuildInfo) string {
	if linked != "" && linked != "unknown" {
		return strings.TrimPrefix(linked, "v")
	}
	if info != nil {
		for _, dependency := range info.Deps {
			if dependency.Path == "github.com/sagernet/sing-box" {
				if dependency.Replace != nil {
					dependency = dependency.Replace
				}
				if strings.HasPrefix(dependency.Version, "v") {
					return strings.TrimPrefix(dependency.Version, "v")
				}
			}
		}
	}
	return "unknown"
}
