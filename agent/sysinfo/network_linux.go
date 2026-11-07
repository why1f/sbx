//go:build linux

package sysinfo

import (
	"errors"
	"github.com/mdlayher/netlink"
	"golang.org/x/sys/unix"
)

type routeSubscription struct{ *netlink.Conn }

func subscribeNetwork() (networkSubscription, error) {
	groups := uint32(unix.RTMGRP_LINK | unix.RTMGRP_IPV4_IFADDR | unix.RTMGRP_IPV6_IFADDR |
		unix.RTMGRP_IPV4_ROUTE | unix.RTMGRP_IPV6_ROUTE | unix.RTMGRP_IPV4_RULE |
		(1 << (unix.RTNLGRP_IPV6_RULE - 1)))
	conn, err := netlink.Dial(unix.NETLINK_ROUTE, &netlink.Config{Groups: groups})
	if err != nil {
		return nil, err
	}
	return &routeSubscription{conn}, nil
}

func (s *routeSubscription) Receive() (bool, error) {
	msgs, err := s.Conn.Receive()
	if err != nil {
		return false, err
	}
	for _, msg := range msgs {
		if msg.Header.Type == netlink.Overrun {
			return false, errors.New("netlink receive overflow")
		}
		switch uint16(msg.Header.Type) {
		case unix.RTM_NEWADDR, unix.RTM_DELADDR, unix.RTM_NEWLINK, unix.RTM_DELLINK,
			unix.RTM_NEWROUTE, unix.RTM_DELROUTE, unix.RTM_NEWRULE, unix.RTM_DELRULE:
			return true, nil
		}
	}
	return false, nil
}
