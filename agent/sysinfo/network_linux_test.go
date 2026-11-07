//go:build linux

package sysinfo

import (
	"testing"
	"time"

	"github.com/mdlayher/netlink"
	"golang.org/x/sys/unix"
)

// Read-only kernel integration: a link dump generates the same RTM_NEWLINK
// messages as the subscribed multicast stream, without changing host networking.
func TestLinuxNetworkSubscriptionReceivesLinkSnapshots(t *testing.T) {
	sub, err := subscribeNetwork()
	if err != nil {
		t.Fatal(err)
	}
	defer sub.Close()
	s := sub.(*routeSubscription)
	if err := s.SetReadDeadline(time.Now().Add(3 * time.Second)); err != nil {
		t.Fatal(err)
	}
	_, err = s.Send(netlink.Message{Header: netlink.Header{Type: unix.RTM_GETLINK, Flags: netlink.Request | netlink.Dump}, Data: make([]byte, unix.SizeofIfInfomsg)})
	if err != nil {
		t.Fatal(err)
	}
	changed, err := s.Receive()
	if err != nil || !changed {
		t.Fatalf("kernel link notification: %v %v", changed, err)
	}
}
