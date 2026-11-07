//go:build !linux

package sysinfo

func subscribeNetwork() (networkSubscription, error) {
	return nil, errNetworkWatchUnsupported
}
