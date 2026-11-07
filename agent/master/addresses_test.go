package master

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"testing/synctest"
	"time"

	"github.com/why1f/sbx/agent/sysinfo"
)

func TestIPQueryIntervalDistinguishesMissingAndZero(t *testing.T) {
	for _, tc := range []struct {
		json string
		want time.Duration
		bad  bool
	}{
		{`{}`, 10 * time.Minute, false},
		{`{"ip_query_interval_secs":0}`, 0, false},
		{`{"ip_query_interval_secs":300}`, 5 * time.Minute, false},
		{`{"ip_query_interval_secs":9223372037}`, 0, true},
	} {
		var ack AgentHelloAck
		if err := json.Unmarshal([]byte(tc.json), &ack); err != nil {
			t.Fatal(err)
		}
		got, err := ack.ipQueryInterval()
		if (err != nil) != tc.bad || got != tc.want {
			t.Fatalf("%s: got %s, %v", tc.json, got, err)
		}
	}
}

func TestZeroIntervalKeepsStartupQueryAndImmediateNetworkUpdates(t *testing.T) {
	var current atomic.Value
	current.Store("198.51.100.1")
	var requests atomic.Int64
	subscribed := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		select {
		case <-subscribed:
		default:
			t.Error("IP snapshot ran before network subscription")
		}
		requests.Add(1)
		_, _ = w.Write([]byte(current.Load().(string)))
	}))
	defer srv.Close()
	events := make(chan struct{}, 32)
	h := newHarnessWithSetup(t, nil, func(c *Conn) {
		c.sys.SetEndpoints([]string{srv.URL}, nil)
		c.networkChanges = func(context.Context) <-chan struct{} { close(subscribed); return events }
	})
	defer h.ws.Close()
	zero := uint64(0)
	// A one-hour system-report interval proves the event is sent immediately.
	hello := handshakeWithIPInterval(t, h.ws, &zero, 3600)
	if hello.IPv4 == nil || *hello.IPv4 != "198.51.100.1" {
		t.Fatalf("startup query missing: %+v", hello)
	}
	time.Sleep(100 * time.Millisecond)
	if requests.Load() != 1 {
		t.Fatalf("unexpected periodic query at interval zero: %d", requests.Load())
	}
	current.Store("198.51.100.2")
	for i := 0; i < 25; i++ {
		events <- struct{}{}
	}
	env := readUntil(t, h.ws, MethodAddressesReport)
	var update sysinfo.Addresses
	if err := json.Unmarshal(env.Payload, &update); err != nil {
		t.Fatal(err)
	}
	if update.IPv4 == nil || *update.IPv4 != "198.51.100.2" {
		t.Fatalf("missing immediate update: %+v", update)
	}
	if requests.Load() != 2 {
		t.Fatalf("burst should produce one fresh query: %d", requests.Load())
	}
}

func TestAddressWatchIdleDisabledAndRetry(t *testing.T) {
	timing := addressWatchTiming{5 * time.Millisecond, 10 * time.Millisecond}
	for _, needsRetry := range []bool{false, true} {
		lastFailedAttempt := -1
		if needsRetry {
			lastFailedAttempt = 0
		}
		ctx, cancel := context.WithTimeout(context.Background(), 90*time.Millisecond)
		calls := 0
		err := watchAddressUpdates(ctx, 0, nil, lastFailedAttempt, func(_ int) (bool, error) {
			calls++
			return calls > 1, nil
		}, timing)
		cancel()
		want := 0
		if needsRetry {
			want = 2
		}
		if err != nil || calls != want {
			t.Fatalf("retry=%v: calls=%d, err=%v", needsRetry, calls, err)
		}
	}
}

func TestAddressWatchUsesConfiguredIntervalAfterListenerCloses(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	closed := make(chan struct{})
	close(closed)
	calls := 0
	timing := addressWatchTiming{time.Millisecond, time.Millisecond}
	err := watchAddressUpdates(ctx, 15*time.Millisecond, closed, -1, func(_ int) (bool, error) {
		calls++
		if calls == 2 {
			cancel()
		}
		return true, nil
	}, timing)
	if err != nil || calls != 2 {
		t.Fatalf("periodic query did not run: %d %v", calls, err)
	}
}

// Virtual time verifies the production 5/10/20-second delays without slow or
// timing-sensitive sleeps. Startup's initial query has already happened.
func TestAddressRetriesStopAfterThreeAndNewEventRestarts(t *testing.T) {
	for _, startup := range []bool{false, true} {
		lastFailedAttempt := -1
		if startup {
			lastFailedAttempt = 0
		}
		name := "network_event"
		if startup {
			name = "startup"
		}
		t.Run(name, func(t *testing.T) {
			synctest.Test(t, func(t *testing.T) {
				ctx, cancel := context.WithCancel(context.Background())
				defer cancel()
				events := make(chan struct{}, 1)
				type call struct {
					attempt int
					at      time.Duration
				}
				calls := make(chan call, 8)
				done := make(chan error, 1)
				start := time.Now()
				go func() {
					done <- watchAddressUpdates(ctx, 0, events, lastFailedAttempt, func(attempt int) (bool, error) {
						calls <- call{attempt, time.Since(start)}
						return false, nil
					}, addressWatchTiming{time.Second, 5 * time.Second})
				}()
				want := []call{{0, time.Second}, {1, 6 * time.Second}, {2, 16 * time.Second}, {3, 36 * time.Second}}
				if startup {
					want = []call{{1, 5 * time.Second}, {2, 15 * time.Second}, {3, 35 * time.Second}}
				} else {
					events <- struct{}{}
				}
				for _, expected := range want {
					if got := <-calls; got != expected {
						t.Fatalf("got %+v, want %+v", got, expected)
					}
				}
				time.Sleep(time.Hour)
				synctest.Wait()
				if len(calls) != 0 {
					t.Fatal("exhausted retries must remain idle with interval 0")
				}
				// A new event grants a new initial query plus its three retries.
				events <- struct{}{}
				for attempt := 0; attempt <= 3; attempt++ {
					if got := <-calls; got.attempt != attempt {
						t.Fatalf("new cycle: %+v", got)
					}
				}
				time.Sleep(time.Hour)
				synctest.Wait()
				if len(calls) != 0 {
					t.Fatal("new cycle also exceeded the retry limit")
				}
				cancel()
				if err := <-done; err != nil {
					t.Fatal(err)
				}
			})
		})
	}
}

func TestAddressRetriesStopOnSuccessOrCancellation(t *testing.T) {
	for _, cancelEarly := range []bool{false, true} {
		name := "success"
		if cancelEarly {
			name = "cancel"
		}
		t.Run(name, func(t *testing.T) {
			synctest.Test(t, func(t *testing.T) {
				ctx, cancel := context.WithCancel(context.Background())
				defer cancel()
				calls := make(chan int, 8)
				done := make(chan error, 1)
				go func() {
					done <- watchAddressUpdates(ctx, 0, nil, 0, func(attempt int) (bool, error) {
						calls <- attempt
						return attempt == 2, nil
					}, addressWatchTiming{time.Second, 5 * time.Second})
				}()
				if got := <-calls; got != 1 {
					t.Fatal(got)
				}
				if cancelEarly {
					cancel()
				} else {
					if got := <-calls; got != 2 {
						t.Fatal(got)
					}
				}
				time.Sleep(time.Hour)
				synctest.Wait()
				if len(calls) != 0 {
					t.Fatal("success/cancellation must stop pending retries")
				}
				cancel()
				if err := <-done; err != nil {
					t.Fatal(err)
				}
			})
		})
	}
}

func TestPeriodicIPQueryStartsFreshCycleAfterRetryExhaustion(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		type call struct {
			attempt int
			at      time.Duration
		}
		calls := make(chan call, 8)
		done := make(chan error, 1)
		start := time.Now()
		go func() {
			done <- watchAddressUpdates(ctx, 10*time.Minute, nil, -1, func(attempt int) (bool, error) {
				calls <- call{attempt, time.Since(start)}
				return false, nil
			}, addressWatchTiming{time.Second, 5 * time.Second})
		}()
		for _, want := range []call{{0, 600 * time.Second}, {1, 605 * time.Second}, {2, 615 * time.Second}, {3, 635 * time.Second}, {0, 1200 * time.Second}} {
			if got := <-calls; got != want {
				t.Fatalf("got %+v, want %+v", got, want)
			}
		}
		cancel()
		if err := <-done; err != nil {
			t.Fatal(err)
		}
	})
}

func TestCachedReconnectKeepsRemainingAddressRetryBudget(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		lastAttempt := 0 // Startup query failed; its snapshot is reused on reconnect.
		var attempts []int
		run := func(interrupt bool) {
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			done := make(chan error, 1)
			go func() {
				done <- watchAddressUpdates(ctx, 0, nil, lastAttempt, func(attempt int) (bool, error) {
					lastAttempt = attempt
					attempts = append(attempts, attempt)
					if interrupt {
						cancel()
					}
					return false, nil
				}, addressWatchTiming{time.Second, 5 * time.Second})
			}()
			if !interrupt {
				time.Sleep(time.Minute)
				cancel()
			}
			if err := <-done; err != nil {
				t.Fatal(err)
			}
		}
		run(true)
		run(false)
		if len(attempts) != 3 || attempts[0] != 1 || attempts[1] != 2 || attempts[2] != 3 {
			t.Fatalf("cached reconnect granted extra retries: %v", attempts)
		}
	})
}

func TestPeriodicTickDuringEventQueryDoesNotImmediatelyQueryAgain(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		events := make(chan struct{}, 1)
		calls := make(chan time.Time, 8)
		done := make(chan error, 1)
		go func() {
			done <- watchAddressUpdates(ctx, 10*time.Second, events, -1, func(int) (bool, error) {
				calls <- time.Now()
				time.Sleep(2 * time.Second)
				return true, nil
			}, addressWatchTiming{time.Second, 5 * time.Second})
		}()
		synctest.Wait()
		time.Sleep(8 * time.Second)
		events <- struct{}{}
		<-calls // Event query starts at second 9 and finishes at second 11.
		time.Sleep(3 * time.Second)
		synctest.Wait()
		extra := len(calls)
		cancel()
		if err := <-done; err != nil {
			t.Fatal(err)
		}
		if extra != 0 {
			t.Fatal("periodic tick queued during a query caused a redundant query")
		}
	})
}

func TestCoincidentEventAndPeriodicTimersShareOneQuery(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		events := make(chan struct{}, 1)
		calls := make(chan time.Time, 8)
		done := make(chan error, 1)
		start := time.Now()
		go func() {
			done <- watchAddressUpdates(ctx, 10*time.Second, events, -1, func(int) (bool, error) {
				calls <- time.Now()
				return true, nil
			}, addressWatchTiming{time.Second, 5 * time.Second})
		}()
		synctest.Wait()
		time.Sleep(9 * time.Second)
		events <- struct{}{}
		synctest.Wait() // Both timers are due at second 10.
		first := <-calls
		synctest.Wait()
		extra := len(calls)
		// The next legitimate periodic query must still run at second 20.
		second := <-calls
		cancel()
		if err := <-done; err != nil {
			t.Fatal(err)
		}
		if extra != 0 || first.Sub(start) != 10*time.Second || second.Sub(start) != 20*time.Second {
			t.Fatalf("coalescing failed: extra=%d, query times=%s/%s", extra, first.Sub(start), second.Sub(start))
		}
	})
}
