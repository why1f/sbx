package sysinfo

import (
	"context"
	"errors"
	"net"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"
)

func TestSlowProbeDoesNotBlockSystemReports(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		close(started)
		<-release
		_, _ = w.Write([]byte("198.51.100.1"))
	}))
	defer srv.Close()
	defer close(release)
	c := NewCollector()
	c.SetEndpoints([]string{srv.URL}, nil)
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	go c.RefreshIPs(ctx)
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("local probe did not start")
	}
	reported := make(chan struct{})
	go func() { c.Collect(); close(reported) }()
	select {
	case <-reported:
	case <-time.After(500 * time.Millisecond):
		t.Fatal("system reporting waited for the HTTP probe")
	}
}

func TestRefreshIPv6AfterAttachedAddressRemoved(t *testing.T) {
	c := NewCollector()
	c.SetEndpoints(nil, []string{"invalid-url"})
	selected := "2600:1700::1"
	c.sourceIP = func(string) *string { value := selected; return &value }
	c.interfaceAddrs = func() ([]net.Addr, error) {
		return []net.Addr{&net.IPAddr{IP: net.ParseIP(selected)}}, nil
	}
	_, first := c.PublicIPs(context.Background())
	if first == nil || *first != selected {
		t.Fatalf("initial address: %v", first)
	}
	selected = "2a06:9801::1"
	_, cached := c.PublicIPs(context.Background())
	if *cached != "2600:1700::1" {
		t.Fatal("expected reconnect cache for control")
	}
	c.RefreshIPs(context.Background())
	report := c.Collect()
	if report.Addresses == nil || report.Addresses.IPv6 == nil || *report.Addresses.IPv6 != selected {
		t.Fatalf("periodic report did not replace ATT IPv6: %+v", report.Addresses)
	}
	if list := report.Addresses.IPv6Local; list == nil || len(*list) != 1 || (*list)[0] != selected {
		t.Fatalf("removed address survived inventory: %v", list)
	}
}

func TestAddressRemovalAndProbeFailureAreDifferent(t *testing.T) {
	c := NewCollector()
	c.SetEndpoints([]string{"invalid-url"}, []string{"invalid-url"})
	c.sourceIP = func(string) *string { return nil }
	c.interfaceAddrs = func() ([]net.Addr, error) {
		return []net.Addr{&net.IPAddr{IP: net.ParseIP("10.0.0.1")}, &net.IPAddr{IP: net.ParseIP("2a06:9801::1")}}, nil
	}
	c.RefreshIPs(context.Background())
	u := c.AddressSnapshot()
	if u.IPv4 != nil || u.IPv6 != nil || u.IPv6Local == nil || len(*u.IPv6Local) != 1 {
		t.Fatalf("probe failure must retain evidence of configured addresses: %+v", u)
	}
	c.interfaceAddrs = func() ([]net.Addr, error) { return nil, nil }
	c.RefreshIPs(context.Background())
	u = c.AddressSnapshot()
	if u.IPv4 == nil || *u.IPv4 != "" || u.IPv6Local == nil || len(*u.IPv6Local) != 0 {
		t.Fatalf("confirmed removal must be explicit: %+v", u)
	}
	c.interfaceAddrs = func() ([]net.Addr, error) { return nil, errors.New("enumeration failed") }
	c.RefreshIPs(context.Background())
	u = c.AddressSnapshot()
	if u.IPv4 != nil || u.IPv6Local != nil {
		t.Fatalf("inventory failure must not clear: %+v", u)
	}
}

func TestPeriodicIPv4ProbeBypassesCache(t *testing.T) {
	var current atomic.Value
	current.Store("198.51.100.1")
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		_, _ = w.Write([]byte(current.Load().(string)))
	}))
	defer srv.Close()
	c := NewCollector()
	c.SetEndpoints([]string{srv.URL}, nil)
	c.interfaceAddrs = func() ([]net.Addr, error) { return []net.Addr{&net.IPAddr{IP: net.ParseIP("10.0.0.1")}}, nil }
	c.PublicIPs(context.Background())
	current.Store("198.51.100.2")
	c.RefreshIPs(context.Background())
	if v4 := c.AddressSnapshot().IPv4; v4 == nil || *v4 != "198.51.100.2" {
		t.Fatalf("stale IPv4: %v", v4)
	}
}

func TestExhaustedRetryHintStaysClearedWhileReusingCachedIPs(t *testing.T) {
	c := NewCollector()
	c.SetEndpoints([]string{"invalid-url"}, nil)
	c.sourceIP = func(string) *string { return nil }
	c.interfaceAddrs = func() ([]net.Addr, error) {
		return []net.Addr{&net.IPAddr{IP: net.ParseIP("10.0.0.1")}}, nil
	}
	if c.RefreshIPs(context.Background()) || !c.NeedsIPRetry() {
		t.Fatal("failed query must request retries")
	}
	snapshot := c.AddressSnapshot()
	c.StopIPRetry()
	c.PublicIPs(context.Background()) // Reconnect reuses the failed-query cache.
	if c.NeedsIPRetry() {
		t.Fatal("cached result must not revive exhausted retries")
	}
	if c.AddressSnapshot() != snapshot {
		t.Fatal("retiring retries must not discard the snapshot")
	}
	c.RefreshIPs(context.Background()) // A real new trigger permits another cycle.
	if !c.NeedsIPRetry() {
		t.Fatal("new failed query must have its own retry cycle")
	}
}
