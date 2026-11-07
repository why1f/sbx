package sysinfo

import (
	"context"
	"errors"
	"log"
	"time"
)

var errNetworkWatchUnsupported = errors.New("network event subscription is only supported on Linux")

type networkSubscription interface {
	Receive() (bool, error)
	Close() error
}

// WatchNetworkChanges subscribes before returning, so the caller can take its
// initial snapshot without missing changes between the snapshot and subscription.
// A one-element channel coalesces bursts, including changes while disconnected.
func WatchNetworkChanges(ctx context.Context) <-chan struct{} {
	return watchNetworkChanges(ctx, subscribeNetwork, 5*time.Second)
}

func watchNetworkChanges(ctx context.Context, subscribe func() (networkSubscription, error), retry time.Duration) <-chan struct{} {
	changes := make(chan struct{}, 1)
	sub, err := subscribe()
	go func() {
		defer close(changes)
		notify := func() {
			select {
			case changes <- struct{}{}:
			default:
			}
		}
		for {
			if err != nil {
				if errors.Is(err, errNetworkWatchUnsupported) {
					return
				}
				log.Printf("网络事件监听失败,稍后重新订阅: %v", err)
				timer := time.NewTimer(retry)
				select {
				case <-ctx.Done():
					timer.Stop()
					return
				case <-timer.C:
				}
				sub, err = subscribe()
				if err != nil {
					continue
				}
				// Recheck everything after recovery (including receive-buffer overflow).
				notify()
			}
			err = receiveNetworkChanges(ctx, sub, notify)
			if ctx.Err() != nil {
				return
			}
		}
	}()
	return changes
}

func receiveNetworkChanges(ctx context.Context, sub networkSubscription, notify func()) error {
	stop := context.AfterFunc(ctx, func() { _ = sub.Close() })
	defer stop()
	defer sub.Close()
	for {
		changed, err := sub.Receive()
		if err != nil {
			return err
		}
		if changed {
			notify()
		}
	}
}
