package boxctl

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"io"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	M "github.com/sagernet/sing/common/metadata"
	"github.com/why1f/sbx/agent/tracker"
)

// The server and the URI/YAML are produced by the actual Rust generators.
// Exercise the selected algorithm, user identity, TCP/UDP accounting, and bans.
func TestShadowsocksAlgorithmsTransferAndEnforceUserState(t *testing.T) {
	files, err := filepath.Glob("../../master/testdata/shadowsocks/*.json")
	if err != nil || len(files) != 5 {
		t.Fatalf("expected five algorithm fixtures: %v %v", files, err)
	}
	for _, file := range files {
		t.Run(strings.TrimSuffix(filepath.Base(file), ".json"), func(t *testing.T) {
			raw, err := os.ReadFile(file)
			if err != nil {
				t.Fatal(err)
			}
			var fixture struct {
				Inbound      map[string]any `json:"inbound"`
				EmptyInbound map[string]any `json:"empty_inbound"`
				URI          string         `json:"uri"`
				Clash        string         `json:"clash"`
			}
			if err := json.Unmarshal(raw, &fixture); err != nil {
				t.Fatal(err)
			}
			uri, err := url.Parse(fixture.URI)
			if err != nil {
				t.Fatal(err)
			}
			method, password := uri.User.Username(), ""
			if decoded, ok := uri.User.Password(); ok {
				password = decoded // SS2022: URL-escaped method:server-key:user-key.
			} else {
				decoded, err := base64.RawURLEncoding.DecodeString(method)
				if err != nil {
					t.Fatal(err)
				}
				var ok bool
				method, password, ok = strings.Cut(string(decoded), ":")
				if !ok {
					t.Fatal("invalid SIP002 userinfo")
				}
			}
			if method != fixture.Inbound["method"] {
				t.Fatal("server/subscription algorithms differ")
			}
			// Check the YAML credential mapping without adding a runtime YAML dependency.
			for _, line := range strings.Split(fixture.Clash, "\n") {
				if value, ok := strings.CutPrefix(line, "    password: "); ok {
					if strings.HasPrefix(value, `"`) {
						if err := json.Unmarshal([]byte(value), &value); err != nil {
							t.Fatal(err)
						}
					}
					if value != password {
						t.Fatal("URI and Clash passwords differ")
					}
				}
			}
			port := freePort(t)
			fixture.Inbound["listen"], fixture.Inbound["listen_port"] = "127.0.0.1", port
			tr := tracker.New()
			server := New(tr)
			defer server.Close()
			cfg, _ := json.Marshal(map[string]any{
				"log": map[string]any{"disabled": true}, "inbounds": []any{fixture.Inbound},
				"outbounds": []any{map[string]any{"type": "direct", "tag": "direct"}},
			})
			if err := server.Apply(cfg); err != nil {
				t.Fatal(err)
			}
			cfg, _ = json.Marshal(map[string]any{
				"log": map[string]any{"disabled": true},
				"outbounds": []any{map[string]any{"type": "shadowsocks", "tag": "proxy", "server": "127.0.0.1",
					"server_port": port, "method": method, "password": password}},
			})
			client, err := build(cfg)
			if err != nil {
				t.Fatal(err)
			}
			defer client.Close()
			if err := client.Start(); err != nil {
				t.Fatal(err)
			}
			outbound, _ := client.Outbound().Outbound("proxy")
			echo, err := net.Listen("tcp", "127.0.0.1:0")
			if err != nil {
				t.Fatal(err)
			}
			defer echo.Close()
			go func() {
				for {
					c, err := echo.Accept()
					if err != nil {
						return
					}
					go func() { defer c.Close(); _, _ = io.Copy(c, c) }()
				}
			}()
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			defer cancel()
			target := M.ParseSocksaddr(echo.Addr().String())
			conn, err := outbound.DialContext(ctx, "tcp", target)
			if err != nil {
				t.Fatal(err)
			}
			_ = conn.SetDeadline(time.Now().Add(3 * time.Second))
			payload := []byte("shadowsocks user traffic round trip")
			if _, err := conn.Write(payload); err != nil {
				conn.Close()
				t.Fatal(err)
			}
			received := make([]byte, len(payload))
			_, err = io.ReadFull(conn, received)
			conn.Close()
			if err != nil || !bytes.Equal(received, payload) {
				t.Fatalf("TCP: %q %v", received, err)
			}

			udp, err := net.ListenPacket("udp", "127.0.0.1:0")
			if err != nil {
				t.Fatal(err)
			}
			defer udp.Close()
			go func() {
				buf := make([]byte, 2048)
				for {
					n, from, err := udp.ReadFrom(buf)
					if err != nil {
						return
					}
					_, _ = udp.WriteTo(buf[:n], from)
				}
			}()
			packet, err := outbound.ListenPacket(ctx, M.ParseSocksaddr(udp.LocalAddr().String()))
			if err != nil {
				t.Fatal(err)
			}
			_ = packet.SetDeadline(time.Now().Add(3 * time.Second))
			if _, err := packet.WriteTo(payload, udp.LocalAddr()); err != nil {
				packet.Close()
				t.Fatal(err)
			}
			received = make([]byte, 2048)
			n, _, err := packet.ReadFrom(received)
			packet.Close()
			if err != nil || !bytes.Equal(received[:n], payload) {
				t.Fatalf("UDP: %q %v", received[:n], err)
			}
			counts := tr.Snapshot()
			if len(counts) != 1 || counts[0].Name != "alice" || counts[0].Tag != "node-1" || counts[0].Up == 0 || counts[0].Down == 0 {
				t.Fatalf("user accounting lost: %+v", counts)
			}
			tr.SetDisabled([]string{"alice"})
			denied, err := outbound.DialContext(ctx, "tcp", target)
			if err == nil {
				_ = denied.SetDeadline(time.Now().Add(3 * time.Second))
				_, err = denied.Write(payload)
				if err == nil {
					_, err = denied.Read(make([]byte, 1))
				}
				denied.Close()
			}
			if err == nil {
				t.Fatal("disabled user still received data")
			}

			fixture.EmptyInbound["listen"], fixture.EmptyInbound["listen_port"] = "127.0.0.1", port
			cfg, _ = json.Marshal(map[string]any{
				"log": map[string]any{"disabled": true}, "inbounds": []any{fixture.EmptyInbound},
				"outbounds": []any{map[string]any{"type": "direct", "tag": "direct"}},
			})
			if err := server.Apply(cfg); err != nil {
				t.Fatal(err)
			}
			cfg, _ = json.Marshal(map[string]any{
				"log": map[string]any{"disabled": true},
				"outbounds": []any{map[string]any{"type": "shadowsocks", "tag": "proxy", "server": "127.0.0.1",
					"server_port": port, "method": method, "password": fixture.EmptyInbound["password"]}},
			})
			rootClient, err := build(cfg)
			if err != nil {
				t.Fatal(err)
			}
			defer rootClient.Close()
			if err := rootClient.Start(); err != nil {
				t.Fatal(err)
			}
			rootOutbound, _ := rootClient.Outbound().Outbound("proxy")
			denied, err = rootOutbound.DialContext(ctx, "tcp", target)
			if err == nil {
				_ = denied.SetDeadline(time.Now().Add(3 * time.Second))
				_, err = denied.Write(payload)
				if err == nil {
					_, err = denied.Read(make([]byte, 1))
				}
				denied.Close()
			}
			if err == nil {
				t.Fatal("empty user list allowed access with the server key")
			}
		})
	}
}
