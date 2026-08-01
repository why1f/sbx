package tracker

import (
	"errors"
	"net"

	N "github.com/sagernet/sing/common/network"
)

// ErrUserDisabled 是被禁用用户的连接拿到的错误。
//
// **措辞刻意含糊。** 它可能经由代理协议传回客户端,不该告诉对面
// 「你被禁用了」还是「你超配额了」—— 那是账户状态,不该向未授权方泄露(§8.1 同源考虑)。
var ErrUserDisabled = errors.New("connection refused")

// rejected 返回一个立刻出错的 conn,用于被禁用用户的新连接(§7.5)。
//
// 拒绝时机已由 §12.0 的 spike 实测:tracker 的调用点在**选定 outbound 之后**,
// 所以确实会浪费一次上游拨号 —— 但整个过程在 3ms 内结束,
// 客户端看到的是一次普通的连接中断,不泄露账户状态。代价可以接受,
// 不需要退回「禁用也走 box 重建」的那条退路。
func rejected(conn net.Conn) net.Conn {
	// 关掉底层 conn 而不是只包一层:否则这条连接会挂在那里直到超时,
	// 白占一个 fd 和一个 goroutine。
	_ = conn.Close()
	return &deadConn{Conn: conn}
}

func rejectedPacket(conn N.PacketConn) N.PacketConn {
	_ = conn.Close()
	return conn
}

// deadConn 的每个操作都立刻失败。
type deadConn struct {
	net.Conn
}

func (d *deadConn) Read([]byte) (int, error)  { return 0, ErrUserDisabled }
func (d *deadConn) Write([]byte) (int, error) { return 0, ErrUserDisabled }
func (d *deadConn) Close() error              { return nil }
