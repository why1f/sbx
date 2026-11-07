package sysinfo

import (
	"context"
	"net"
	"sort"
	"sync"
	"time"
)

// Addresses accompanies sysinfo reports. nil means the probe could not determine
// the address; an empty IPv4 means the machine has no usable IPv4 interfaces.
// IPv6Local is authoritative only when interface enumeration succeeded. It lets
// the master distinguish a removed IPv6 from a temporary route/probe failure.
type Addresses struct {
	IPv4      *string   `json:"ipv4,omitempty"`
	IPv6      *string   `json:"ipv6,omitempty"`
	IPv6Local *[]string `json:"ipv6_local,omitempty"`
}

func (c *Collector) AddressSnapshot() *Addresses {
	c.ipMu.Lock()
	defer c.ipMu.Unlock()
	return c.addresses // Immutable after publication.
}

// RefreshIPs bypasses the reconnect cache. Run it outside the heartbeat/report
// goroutine: an unreachable echo service must not delay either of those tasks.
func (c *Collector) RefreshIPs(ctx context.Context) bool {
	c.probeMu.Lock()
	defer c.probeMu.Unlock()
	c.ipMu.Lock()
	v4eps := append([]string(nil), c.v4Endpoints...)
	v6eps := append([]string(nil), c.v6Endpoints...)
	c.ipMu.Unlock()
	var update Addresses
	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		update.IPv4 = probe(ctx, "tcp4", v4eps)
		if update.IPv4 == nil && len(v4eps) > 0 {
			update.IPv4 = c.sourceIP("udp4")
		}
	}()
	go func() {
		defer wg.Done()
		if len(v6eps) > 0 {
			update.IPv6 = c.sourceIP("udp6")
		}
		if update.IPv6 == nil {
			update.IPv6 = probe(ctx, "tcp6", v6eps)
		}
	}()
	wg.Wait()
	complete := true
	if len(v4eps) > 0 || len(v6eps) > 0 {
		if addrs, err := c.interfaceAddrs(); err == nil {
			has4, local6 := localAddresses(addrs)
			if len(v4eps) > 0 && !has4 && update.IPv4 == nil {
				empty := ""
				update.IPv4 = &empty
			}
			if len(v6eps) > 0 {
				update.IPv6Local = &local6
			}
		} else {
			complete = false
		}
	}
	if len(v4eps) > 0 && update.IPv4 == nil {
		complete = false
	}
	if len(v6eps) > 0 && update.IPv6 == nil &&
		(update.IPv6Local == nil || len(*update.IPv6Local) > 0) {
		complete = false
	}
	c.ipMu.Lock()
	defer c.ipMu.Unlock()
	c.ipv4, c.ipv6 = update.IPv4, update.IPv6
	c.addresses = &update
	c.ipRetry = !complete
	c.ipFetched = time.Now()
	c.ipTTL = ipFailTTL
	if update.IPv4 != nil || update.IPv6 != nil {
		c.ipTTL = ipCacheTTL
	}
	return complete
}

func (c *Collector) NeedsIPRetry() bool {
	c.ipMu.Lock()
	defer c.ipMu.Unlock()
	return c.ipRetry
}

// StopIPRetry retires only the retry hint, keeping the address snapshot/cache.
// A new actual probe (startup/cache expiry, event or periodic) can set it again.
func (c *Collector) StopIPRetry() {
	c.ipMu.Lock()
	defer c.ipMu.Unlock()
	c.ipRetry = false
}

func localAddresses(addrs []net.Addr) (bool, []string) {
	has4 := false
	v6 := make([]string, 0)
	for _, addr := range addrs {
		var ip net.IP
		switch a := addr.(type) {
		case *net.IPNet:
			ip = a.IP
		case *net.IPAddr:
			ip = a.IP
		}
		if ip == nil || !ip.IsGlobalUnicast() {
			continue
		}
		if ip.To4() != nil {
			has4 = true // Private IPv4 may have public NAT; do not clear it.
		} else if isGloballyRoutable(ip) {
			v6 = append(v6, ip.String())
		}
	}
	sort.Strings(v6)
	return has4, v6
}
