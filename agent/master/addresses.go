package master

import (
	"context"
	"fmt"
	"reflect"
	"time"
)

func (ack *AgentHelloAck) ipQueryInterval() (time.Duration, error) {
	secs := uint64(600)
	if ack.IPQueryIntervalSecs != nil {
		secs = *ack.IPQueryIntervalSecs
	}
	if secs > uint64(1<<63-1)/uint64(time.Second) {
		return 0, fmt.Errorf("ip_query_interval_secs 超出可用范围")
	}
	return time.Duration(secs) * time.Second, nil
}

type addressWatchTiming struct {
	coalesce, retryMin time.Duration
}

const maxAddressRetries = 3

func (c *Conn) watchAddresses(ctx context.Context, interval time.Duration, changes <-chan struct{}) error {
	lastFailedAttempt := -1
	if c.sys.NeedsIPRetry() {
		c.mu.Lock()
		lastFailedAttempt = c.ipRetryAttempt
		c.mu.Unlock()
	}
	return watchAddressUpdates(ctx, interval, changes, lastFailedAttempt, func(attempt int) (bool, error) {
		c.mu.Lock()
		c.ipRetryAttempt = attempt
		c.mu.Unlock()
		before := c.sys.AddressSnapshot()
		probeCtx, cancel := context.WithTimeout(ctx, ipProbeBudget)
		complete := c.sys.RefreshIPs(probeCtx)
		cancel()
		if !complete && attempt == maxAddressRetries {
			// A reconnect that reuses this snapshot must not grant another retry budget.
			c.sys.StopIPRetry()
		}
		if ctx.Err() != nil {
			return true, nil
		}
		after := c.sys.AddressSnapshot()
		if !reflect.DeepEqual(before, after) {
			c.reportMu.Lock()
			defer c.reportMu.Unlock()
			// A separate event avoids concurrent sysinfo snapshots being delivered
			// out of order and incorrectly resetting NIC accounting baselines.
			if err := c.send(event(MethodAddressesReport, after)); err != nil {
				return false, err
			}
		}
		return complete, nil
	}, addressWatchTiming{time.Second, 5 * time.Second})
}

// lastFailedAttempt is -1 for no pending retry, or the cached query's last attempt.
func watchAddressUpdates(ctx context.Context, interval time.Duration, changes <-chan struct{}, lastFailedAttempt int,
	refresh func(attempt int) (bool, error), timing addressWatchTiming) error {
	var periodic <-chan time.Time
	if interval > 0 {
		ticker := time.NewTicker(interval)
		defer ticker.Stop()
		periodic = ticker.C
	}
	drainPeriodic := func() {
		select {
		case <-periodic:
		default:
		}
	}
	var timer *time.Timer
	var due <-chan time.Time
	var deadline time.Time
	clear := func() {
		if timer != nil {
			if !timer.Stop() {
				select {
				case <-timer.C:
				default:
				}
			}
		}
		timer, due = nil, nil
	}
	defer clear()
	schedule := func(delay time.Duration) {
		at := time.Now().Add(delay)
		if timer != nil && !at.Before(deadline) {
			return
		}
		clear()
		timer = time.NewTimer(delay)
		due, deadline = timer.C, at
	}
	// attempt 0 is a new query; 1..3 are its extra retries, after 5/10/20s.
	attempt := 0
	if lastFailedAttempt >= 0 && lastFailedAttempt < maxAddressRetries {
		// Resume remaining retries after reconnect instead of restarting at attempt 1.
		attempt = lastFailedAttempt + 1
		schedule(timing.retryMin * (1 << lastFailedAttempt))
	}
	for {
		select {
		case <-ctx.Done():
			return nil
		case _, ok := <-changes:
			if !ok {
				changes = nil
				continue
			}
			attempt = 0 // A new network event starts a fresh query cycle.
			schedule(timing.coalesce)
			continue
		case <-periodic:
			attempt = 0
		case <-due:
		}
		clear()
		// All triggers already queued are covered by the query we are about to run.
		drainPeriodic()
		for pending := len(changes); pending > 0; pending-- {
			select {
			case _, ok := <-changes:
				if ok {
					attempt = 0
				} else {
					changes = nil
				}
			default:
			}
		}
		if ctx.Err() != nil {
			return nil
		}
		complete, err := refresh(attempt)
		// A tick during the query must not immediately start another identical query.
		drainPeriodic()
		if err != nil {
			return err
		}
		if complete {
			attempt = 0
		} else if attempt < maxAddressRetries {
			schedule(timing.retryMin * (1 << attempt))
			attempt++
		}
	}
}
