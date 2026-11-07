package sysinfo

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

type fakeNetworkEvent struct {
	changed bool
	err     error
}
type fakeSubscription struct {
	events chan fakeNetworkEvent
	closed chan struct{}
	once   sync.Once
}

func newFakeSubscription() *fakeSubscription {
	return &fakeSubscription{events: make(chan fakeNetworkEvent, 4), closed: make(chan struct{})}
}
func (s *fakeSubscription) Receive() (bool, error) {
	select {
	case e := <-s.events:
		return e.changed, e.err
	case <-s.closed:
		return false, errors.New("closed")
	}
}
func (s *fakeSubscription) Close() error { s.once.Do(func() { close(s.closed) }); return nil }

func TestNetworkWatchResubscribesAndRequestsResync(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	first, second := newFakeSubscription(), newFakeSubscription()
	var attempts atomic.Int64
	changes := watchNetworkChanges(ctx, func() (networkSubscription, error) {
		switch attempts.Add(1) {
		case 1:
			return first, nil
		case 2:
			return nil, errors.New("temporary subscription failure")
		default:
			return second, nil
		}
	}, time.Millisecond)
	if attempts.Load() != 1 {
		t.Fatal("initial subscription must be synchronous")
	}
	first.events <- fakeNetworkEvent{changed: true}
	select {
	case <-changes:
	case <-time.After(time.Second):
		t.Fatal("event lost")
	}
	first.events <- fakeNetworkEvent{err: errors.New("receive overflow")}
	select {
	case <-changes:
	case <-time.After(time.Second):
		t.Fatal("resubscription did not request full resync")
	}
	if attempts.Load() != 3 {
		t.Fatal("failed subscription was not retried")
	}
	select {
	case <-first.closed:
	default:
		t.Fatal("old subscription leaked")
	}
	cancel()
	select {
	case _, ok := <-changes:
		if ok {
			t.Fatal("unexpected event")
		}
	case <-time.After(time.Second):
		t.Fatal("watcher did not stop")
	}
	select {
	case <-second.closed:
	default:
		t.Fatal("cancel did not close the socket")
	}
}
