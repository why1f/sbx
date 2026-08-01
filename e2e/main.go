// Command e2e-driver 驱动 DESIGN.md §13.2 / §13.3 的端到端验证。
//
// 它扮演**客户端**:起两个 sing-box 客户端实例,分别连两台 agent 上的 vless-ws
// 节点,把不对称的流量推过去,再由外层脚本查库核对。
//
// 拓扑:
//
//	driver --socks5--> [客户端 box] --vless-ws--> [agent 的 sing-box] --direct--> echo
//
// 与 spike/ 的区别:spike 在**一个进程里**同时扮演客户端和服务端,验的是
// tracker 本身;这里的服务端是**真正在跑的 agent**,验的是「主控下发的配置
// → agent 装配 → 流量记账 → 上报 → 跨 agent 求和」这条完整链路。
//
// 用法:
//
//	e2e-driver -port 39701 -up 262144 -down 4194304
package main

import (
	"context"
	"encoding/binary"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/netip"
	"os"
	"time"

	box "github.com/sagernet/sing-box"
	"github.com/sagernet/sing-box/include"
	"github.com/sagernet/sing-box/option"
	"github.com/sagernet/sing/common/json/badoption"
	"golang.org/x/net/proxy"
)

var (
	socksPort  = flag.Int("socks", 39701, "本地 socks 监听端口")
	targetPort = flag.Int("target", 39501, "agent 上的 vless-ws 端口")
	echoPort   = flag.Int("echo", 39600, "echo 服务器端口")
	uuid       = flag.String("uuid", "", "用户 uuid")
	path       = flag.String("path", "/e2e", "ws path")
	upBytes    = flag.Int("up", 256*1024, "上行字节数")
	downBytes  = flag.Int("down", 4*1024*1024, "下行字节数")
	serveEchoF = flag.Bool("serve-echo", false, "只起 echo 服务器,不跑流量")
)

func main() {
	flag.Parse()
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "❌ %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	if *serveEchoF {
		ln, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", *echoPort))
		if err != nil {
			return err
		}
		fmt.Printf("echo 服务器在 :%d\n", *echoPort)
		serveEcho(ln)
		return nil
	}
	if *uuid == "" {
		return errors.New("必须给 -uuid")
	}

	client, err := newClientBox(*socksPort, *targetPort, *uuid, *path)
	if err != nil {
		return fmt.Errorf("构建客户端 box: %w", err)
	}
	if err := client.Start(); err != nil {
		return fmt.Errorf("启动客户端 box: %w", err)
	}
	defer client.Close()

	socksAddr := fmt.Sprintf("127.0.0.1:%d", *socksPort)
	if err := waitPort(socksAddr, 10*time.Second); err != nil {
		return err
	}

	echoAddr := fmt.Sprintf("127.0.0.1:%d", *echoPort)
	start := time.Now()
	if err := driveTraffic(socksAddr, echoAddr, *upBytes, *downBytes); err != nil {
		return fmt.Errorf("跑流量: %w", err)
	}
	fmt.Printf("✓ 经 :%d 推送完成 上行=%d 下行=%d 耗时=%s\n",
		*targetPort, *upBytes, *downBytes, time.Since(start).Round(time.Millisecond))
	// 计数发生在 conn 关闭路径上,给它一点时间落定。
	time.Sleep(500 * time.Millisecond)
	return nil
}

func newClientBox(socks, target int, uuidStr, wsPath string) (*box.Box, error) {
	listen := badoption.Addr(netip.MustParseAddr("127.0.0.1"))
	opts := option.Options{
		Log: &option.LogOptions{Level: "error"},
		Inbounds: []option.Inbound{{
			Type: "mixed",
			Tag:  "socks-in",
			Options: &option.HTTPMixedInboundOptions{
				ListenOptions: option.ListenOptions{Listen: &listen, ListenPort: uint16(socks)},
			},
		}},
		Outbounds: []option.Outbound{{
			Type: "vless",
			Tag:  "proxy",
			Options: &option.VLESSOutboundOptions{
				ServerOptions: option.ServerOptions{Server: "127.0.0.1", ServerPort: uint16(target)},
				UUID:          uuidStr,
				// 必须与主控下发给 agent 的 transport 完全一致,否则握手失败。
				Transport: &option.V2RayTransportOptions{
					Type:             "ws",
					WebsocketOptions: option.V2RayWebsocketOptions{Path: wsPath},
				},
			},
		}},
		Route: &option.RouteOptions{Final: "proxy"},
	}
	return box.New(box.Options{Context: include.Context(context.Background()), Options: opts})
}

// ── echo 服务器:客户端发 8 字节头(上行长度 + 下行长度),然后推上行;
//    服务端读完后回下行再关闭。不对称是为了让「方向接反」变成可断言的失败。

func serveEcho(ln net.Listener) {
	for {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		go func(c net.Conn) {
			defer c.Close()
			var hdr [8]byte
			if _, err := io.ReadFull(c, hdr[:]); err != nil {
				return
			}
			up := binary.BigEndian.Uint32(hdr[0:4])
			down := binary.BigEndian.Uint32(hdr[4:8])
			if _, err := io.CopyN(io.Discard, c, int64(up)); err != nil {
				return
			}
			_, _ = io.CopyN(c, &patternReader{}, int64(down))
		}(c)
	}
}

type patternReader struct{ off int }

var pattern = []byte("sbx-e2e-payload-0123456789abcdef")

func (p *patternReader) Read(dst []byte) (int, error) {
	if p.off >= len(pattern) {
		p.off = 0
	}
	n := copy(dst, pattern[p.off:])
	p.off += n
	return n, nil
}

func driveTraffic(socksAddr, target string, up, down int) error {
	d, err := proxy.SOCKS5("tcp", socksAddr, nil, proxy.Direct)
	if err != nil {
		return err
	}
	cd, ok := d.(proxy.ContextDialer)
	if !ok {
		return errors.New("socks5 dialer 不支持 DialContext")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	conn, err := cd.DialContext(ctx, "tcp", target)
	if err != nil {
		return fmt.Errorf("经代理连 %s: %w", target, err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(60 * time.Second))

	var hdr [8]byte
	binary.BigEndian.PutUint32(hdr[0:4], uint32(up))
	binary.BigEndian.PutUint32(hdr[4:8], uint32(down))
	if _, err := conn.Write(hdr[:]); err != nil {
		return fmt.Errorf("写头: %w", err)
	}
	if _, err := io.CopyN(conn, &patternReader{}, int64(up)); err != nil {
		return fmt.Errorf("写上行 %d 字节: %w", up, err)
	}
	n, err := io.CopyN(io.Discard, conn, int64(down))
	if err != nil {
		return fmt.Errorf("读下行(收到 %d/%d): %w", n, down, err)
	}
	return nil
}

func waitPort(addr string, d time.Duration) error {
	deadline := time.Now().Add(d)
	for time.Now().Before(deadline) {
		c, err := net.DialTimeout("tcp", addr, time.Second)
		if err == nil {
			_ = c.Close()
			return nil
		}
		time.Sleep(50 * time.Millisecond)
	}
	return fmt.Errorf("%s 在 %s 内没起来", addr, d)
}
