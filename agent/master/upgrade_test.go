package master

import (
	"crypto/sha256"
	"encoding/hex"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
)

// 这组测试守的是自升级(§11.2)那条路径上唯一真正危险的一步:
// **覆盖一个正在被 supervisor 管着的可执行文件**。
//
// 它值得单独测,是因为失败模式不对称 —— 升级失败没关系(老版本继续跑,
// 主控看到一条错误),但「把 sbx-agent 换成了一个下了一半的文件」
// 会让这台机器在 systemd 重启它的那一刻永久掉线,而且现场只剩一个
// Exec format error。所以下面每一条失败用例都断言**老文件原样还在**。
//
// upgrade 本身拿的是 os.Executable(),在 go test 里那是测试二进制自己;
// 真正做事的 replaceExecutable 把它作为参数收下,测试传临时目录里的假文件。

// binServer 起一个假的下载源,顺带记有没有真的被请求过。
type binServer struct {
	srv  *httptest.Server
	hits atomic.Int32
}

// newBinServer 提供 /ok(返回 body)、/404、/truncated(声明长度但只写一半)。
func newBinServer(t *testing.T, body []byte) *binServer {
	t.Helper()
	bs := &binServer{}
	mux := http.NewServeMux()
	mux.HandleFunc("/ok", func(w http.ResponseWriter, r *http.Request) {
		bs.hits.Add(1)
		w.Write(body)
	})
	mux.HandleFunc("/404", func(w http.ResponseWriter, r *http.Request) {
		bs.hits.Add(1)
		w.WriteHeader(http.StatusNotFound)
	})
	// 声明 Content-Length 是全长,却只写一半然后掐断连接 —— 这是「下到一半
	// 网断了」在客户端看到的样子,io.Copy 会返回 ErrUnexpectedEOF。
	mux.HandleFunc("/truncated", func(w http.ResponseWriter, r *http.Request) {
		bs.hits.Add(1)
		w.Header().Set("Content-Length", strconv.Itoa(len(body)))
		w.Write(body[:len(body)/2])
		if fl, ok := w.(http.Flusher); ok {
			fl.Flush()
		}
		panic(http.ErrAbortHandler) // 不写完就断,且不打一屏栈
	})
	bs.srv = httptest.NewServer(mux)
	t.Cleanup(bs.srv.Close)
	return bs
}

func sha256hex(b []byte) string {
	sum := sha256.Sum256(b)
	return hex.EncodeToString(sum[:])
}

// fakeBinary 在一个临时目录里放一个「当前版本」,返回它的路径。
func fakeBinary(t *testing.T, content string) string {
	t.Helper()
	dir := t.TempDir()
	exe := filepath.Join(dir, "sbx-agent")
	if err := os.WriteFile(exe, []byte(content), 0o755); err != nil {
		t.Fatal(err)
	}
	return exe
}

// assertUntouched 断言老文件一个字节都没变。
func assertUntouched(t *testing.T, exe, want string) {
	t.Helper()
	got, err := os.ReadFile(exe)
	if err != nil {
		t.Fatalf("老文件读不到了(不该被删):%v", err)
	}
	if string(got) != want {
		t.Fatalf("老文件被改了\n want %q\n  got %q", want, string(got))
	}
}

// assertNoLeftovers 断言目录里没留下 .sbx-agent.new-* 临时文件。
// 漏删的话,每失败一次就在 /usr/local/bin 里攒一个几十 MB 的垃圾。
func assertNoLeftovers(t *testing.T, exe string) {
	t.Helper()
	ents, err := os.ReadDir(filepath.Dir(exe))
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range ents {
		if strings.HasPrefix(e.Name(), ".sbx-agent.new-") {
			t.Fatalf("留下了临时文件 %s", e.Name())
		}
	}
}

func TestUpgradeReplacesTheBinary(t *testing.T) {
	newBin := []byte("\x7fELF 这是新版本")
	bs := newBinServer(t, newBin)
	exe := fakeBinary(t, "老版本")

	if err := replaceExecutable(exe, bs.srv.URL+"/ok", sha256hex(newBin)); err != nil {
		t.Fatalf("升级应当成功:%v", err)
	}

	got, err := os.ReadFile(exe)
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != string(newBin) {
		t.Fatalf("内容没换成新版本:%q", string(got))
	}
	assertNoLeftovers(t, exe)

	// 可执行位是 rename 之前打上的。Windows 上没有这个概念,跳过。
	if runtime.GOOS != "windows" {
		fi, err := os.Stat(exe)
		if err != nil {
			t.Fatal(err)
		}
		if fi.Mode().Perm() != 0o755 {
			t.Fatalf("权限应为 0755,实际 %o —— 换上去的文件必须还能被 exec", fi.Mode().Perm())
		}
	}
}

// sha256 大小写和前后空白都该被容忍:主控那边是从 .sha256 文件里读的,
// 那文件的格式(大小写、行尾)不归 agent 管。
func TestUpgradeToleratesUppercaseAndPaddedHash(t *testing.T) {
	newBin := []byte("新版本")
	bs := newBinServer(t, newBin)
	exe := fakeBinary(t, "老版本")

	padded := "  " + strings.ToUpper(sha256hex(newBin)) + "\n"
	if err := replaceExecutable(exe, bs.srv.URL+"/ok", padded); err != nil {
		t.Fatalf("大写 + 空白的 sha256 应当被接受:%v", err)
	}
	if got, _ := os.ReadFile(exe); string(got) != string(newBin) {
		t.Fatalf("内容没换成新版本:%q", string(got))
	}
}

func TestUpgradeKeepsTheOldBinaryWhenTheHashDoesNotMatch(t *testing.T) {
	bs := newBinServer(t, []byte("其实是别的东西"))
	exe := fakeBinary(t, "老版本")

	err := replaceExecutable(exe, bs.srv.URL+"/ok", sha256hex([]byte("我以为会下到这个")))
	if err == nil {
		t.Fatal("sha256 不符必须报错 —— 这是防「下到了错的/被掉包的二进制」的唯一一道闸")
	}
	if !strings.Contains(err.Error(), "sha256") {
		t.Fatalf("错误里该点名 sha256,实际:%v", err)
	}
	assertUntouched(t, exe, "老版本")
	assertNoLeftovers(t, exe)
}

// 下到一半断线:文件已经写进临时文件了,但它是残的。
// 这一条和上一条的区别在于失败发生在 io.Copy 里而不是比对时。
func TestUpgradeKeepsTheOldBinaryWhenTheDownloadIsCutShort(t *testing.T) {
	full := []byte(strings.Repeat("A", 4096))
	bs := newBinServer(t, full)
	exe := fakeBinary(t, "老版本")

	err := replaceExecutable(exe, bs.srv.URL+"/truncated", sha256hex(full))
	if err == nil {
		t.Fatal("下载被掐断必须报错,绝不能把半个文件 rename 上去")
	}
	assertUntouched(t, exe, "老版本")
	assertNoLeftovers(t, exe)
}

func TestUpgradeKeepsTheOldBinaryOnHTTPError(t *testing.T) {
	bs := newBinServer(t, []byte("无所谓"))
	exe := fakeBinary(t, "老版本")

	err := replaceExecutable(exe, bs.srv.URL+"/404", sha256hex([]byte("无所谓")))
	if err == nil {
		t.Fatal("HTTP 404 必须报错")
	}
	if !strings.Contains(err.Error(), "404") {
		t.Fatalf("错误里该带状态码,实际:%v", err)
	}
	assertUntouched(t, exe, "老版本")
	assertNoLeftovers(t, exe)
}

// 非法 sha256 要在**发请求之前**就挡掉:一个 26 MB 的下载不该为一条
// 明显写错的指令白跑一趟。
func TestUpgradeRejectsAMalformedHashWithoutDownloading(t *testing.T) {
	bs := newBinServer(t, []byte("新版本"))
	exe := fakeBinary(t, "老版本")

	for _, bad := range []string{"", "not-hex", "abcd", strings.Repeat("ab", 33)} {
		err := replaceExecutable(exe, bs.srv.URL+"/ok", bad)
		if err == nil {
			t.Fatalf("sha256 %q 非法,应当报错", bad)
		}
	}
	if n := bs.hits.Load(); n != 0 {
		t.Fatalf("非法 sha256 不该产生任何下载,实际请求了 %d 次", n)
	}
	assertUntouched(t, exe, "老版本")
	assertNoLeftovers(t, exe)
}

// 部署常把 /usr/local/bin/sbx-agent 指到带版本号的实际文件上。
// 升级要换的是**指向的那个**,不是软链自己 —— 覆盖软链会把版本管理搞乱,
// 而且下次 systemd 起的还是同一个路径,看不出区别,直到有人去看目录。
func TestUpgradeFollowsTheSymlinkToTheRealBinary(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Windows 上建软链要特权")
	}
	dir := t.TempDir()
	real := filepath.Join(dir, "sbx-agent-0.3.1")
	link := filepath.Join(dir, "sbx-agent")
	if err := os.WriteFile(real, []byte("老版本"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(real, link); err != nil {
		t.Fatal(err)
	}

	resolved := resolveExecutable(link)
	if resolved == link {
		t.Fatal("软链没被解开 —— 升级会把软链本身覆盖成一个普通文件")
	}

	newBin := []byte("新版本")
	bs := newBinServer(t, newBin)
	if err := replaceExecutable(resolved, bs.srv.URL+"/ok", sha256hex(newBin)); err != nil {
		t.Fatalf("升级应当成功:%v", err)
	}

	if fi, err := os.Lstat(link); err != nil {
		t.Fatal(err)
	} else if fi.Mode()&os.ModeSymlink == 0 {
		t.Fatal("软链被替换成了普通文件")
	}
	if got, _ := os.ReadFile(real); string(got) != string(newBin) {
		t.Fatalf("软链指向的文件没被换:%q", string(got))
	}
}

// 目标目录不可写(比如只读挂载的 /usr/local/bin)时要干净地失败,
// 而不是 panic,也不该把老文件先删了再发现写不进去。
func TestUpgradeFailsCleanlyWhenTheTargetDirIsMissing(t *testing.T) {
	bs := newBinServer(t, []byte("新版本"))
	exe := filepath.Join(t.TempDir(), "no-such-dir", "sbx-agent")

	err := replaceExecutable(exe, bs.srv.URL+"/ok", sha256hex([]byte("新版本")))
	if err == nil {
		t.Fatal("目录不存在时应当报错")
	}
	if n := bs.hits.Load(); n != 0 {
		t.Fatalf("临时文件都建不出来,不该开始下载,实际请求了 %d 次", n)
	}
}
