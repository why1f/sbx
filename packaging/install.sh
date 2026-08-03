#!/bin/sh
# sbx 一键安装 / 升级脚本。
#
#   curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | sh
#   ./install.sh agent          # 只装 agent
#   ./install.sh master         # 只装主控
#   ./install.sh --version 0.1.0
#
# 不带参数时**按已安装的东西自动升级**:装了什么就升什么,
# 一个都没装就报错并让你显式选 —— 免得在一台只跑 agent 的机器上莫名多出一个主控。
#
# 行为:
#   * 已是最新版 → 什么都不做(除非 --force)
#   * 版本不一致 → 下载、**校验 sha256**、原子替换、必要时重启 systemd 单元
#
# 用 POSIX sh 而不是 bash:被控机上可能只有 dash/busybox。
#
# ── 关于 `curl | sh` ──
# 整个脚本包在函数里,最后一行才 `main "$@"`。这样连接中断导致下载不完整时,
# sh 读到的是一堆没被调用的函数定义,不会执行到一半就动你的系统。

set -eu

REPO="why1f/sbx"
BIN_DIR="${SBX_BIN_DIR:-/usr/local/bin}"
API="https://api.github.com/repos/$REPO/releases/latest"
DL="https://github.com/$REPO/releases/download"
RAW="https://raw.githubusercontent.com/$REPO/main/packaging/install.sh"

WANT_VERSION=""     # 空 = 用 latest
FORCE=0
NO_RESTART=0
TARGETS="${SBX_TARGET:-}"   # master / agent / all,空 = 自动判断

die() { printf 'sbx-install: %s\n' "$*" >&2; exit 1; }
info() { printf '  %s\n' "$*"; }

# 按**当前的调用方式**给出可以照抄的命令前缀。
#
# `curl … | sh` 的时候 $0 是 "sh",此时 `sh master` 会把 master 当成脚本文件名
# 去打开(报 "cannot open master"),必须写 `sh -s -- master`。
# 提示里如果一律写 "install.sh agent",管道用户照抄就一定踩坑 —— 踩过。
invocation() {
    case "$0" in
        *install.sh) printf '%s' "$0" ;;
        *) printf 'curl -fsSL %s | sh -s --' "$RAW" ;;
    esac
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "缺少 $1,请先安装(apt install $2 / yum install $2)"
}

usage() {
    _i=$(invocation)
    cat <<EOF
用法: $_i [master|agent|all] [选项]

  不带目标时:升级本机已安装的部分;一个都没装则报错。

选项:
  --version <X.Y.Z>   装指定版本(默认最新)
  --force             即使已是该版本也重新装一遍
  --no-restart        替换二进制后不重启 systemd 单元
  --bin-dir <目录>    安装目录(默认 /usr/local/bin,也可用 SBX_BIN_DIR)
  -h, --help          这段

环境变量(自动化场景比传参省事,任何调用形式都能用):
  SBX_TARGET=agent    等价于把 agent 作为参数
  SBX_BIN_DIR=/opt/bin
EOF
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            master|agent) TARGETS="$TARGETS $1" ;;
            all) TARGETS="master agent" ;;
            --version) [ $# -ge 2 ] || die "--version 后面要跟版本号"; WANT_VERSION="${2#v}"; shift ;;
            --force) FORCE=1 ;;
            --no-restart) NO_RESTART=1 ;;
            --bin-dir) [ $# -ge 2 ] || die "--bin-dir 后面要跟目录"; BIN_DIR="$2"; shift ;;
            -h|--help) usage; exit 0 ;;
            *) die "不认识的参数: $1(--help 看用法)" ;;
        esac
        shift
    done
}

# 把 uname -m 映射成 release 里的两套命名。
detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)  RUST_TARGET="x86_64-unknown-linux-musl";  GO_ARCH="amd64" ;;
        aarch64|arm64) RUST_TARGET="aarch64-unknown-linux-musl"; GO_ARCH="arm64" ;;
        *) die "不支持的架构 $(uname -m);release 只出 x86_64 与 aarch64" ;;
    esac
    [ "$(uname -s)" = "Linux" ] || die "只支持 Linux(当前 $(uname -s))"
}

latest_version() {
    # 只要 tag_name。不引 jq —— 被控机上通常没有。
    v=$(curl -fsSL "$API" 2>/dev/null | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1)
    [ -n "$v" ] || die "取不到最新版本号。GitHub API 可能被限流(未认证 60 次/小时)或网络不通;
       可以用 --version 显式指定,例如 --version 0.1.0"
    printf '%s' "$v"
}

# 已装版本。没装 → 空;装了但报不出版本 → "unknown"。
#
# 两者要分开:v0.1.0 的 sbx-agent 还没有 --version,对它说「全新安装」是假话,
# 而且会让人以为脚本没认出自己刚装过的东西。
installed_version() {
    _bin="$BIN_DIR/$1"
    [ -x "$_bin" ] || return 0
    # sbx --version → "sbx 0.1.0";sbx-agent --version → "sbx-agent 0.1.0"
    _v=$("$_bin" --version 2>/dev/null | awk 'NR==1{print $NF}')
    case "$_v" in
        ''|*[!0-9.]*) printf 'unknown' ;;   # 报不出、或者报出来的不像版本号
        *) printf '%s' "$_v" ;;
    esac
}

# 该不该动手:没装、版本不同、报不出版本、或者 --force。
should_install() {
    _cur="$1"; _new="$2"; _name="$3"
    if [ -z "$_cur" ]; then
        info "$_name 全新安装 $_new"; return 0
    fi
    if [ "$_cur" = "unknown" ]; then
        info "$_name 已装但报不出版本(旧版本没有 --version),按升级到 $_new 处理"; return 0
    fi
    if [ "$_cur" = "$_new" ] && [ "$FORCE" -eq 0 ]; then
        info "$_name 已是 $_new,跳过"; return 1
    fi
    [ "$_cur" = "$_new" ] && info "$_name 重装 $_new(--force)" || info "$_name $_cur → $_new"
    return 0
}

# 下载 + 校验 sha256 + 原子替换。
#
# 临时文件必须和目标**同目录**:跨文件系统的 mv 不是原子的,会退化成 copy,
# 中途断电就留下一个截断的可执行文件。
fetch_verify_install() {
    _url="$1"; _sum_url="$2"; _dest="$3"
    _tmp="$_dest.new.$$"
    _sum="$_tmp.sha256"
    # 无论从哪条路径退出都别留垃圾。
    trap 'rm -f "$_tmp" "$_sum"' EXIT INT TERM

    curl -fsSL --retry 3 -o "$_tmp" "$_url" || die "下载失败: $_url"
    if curl -fsSL --retry 3 -o "$_sum" "$_sum_url" 2>/dev/null; then
        # .sha256 里记的是发布时的文件名,和我们的临时名对不上,
        # 所以只取哈希值自己比,不用 `sha256sum -c`。
        _want=$(awk '{print $1}' "$_sum")
        _got=$(sha256sum "$_tmp" | awk '{print $1}')
        [ "$_want" = "$_got" ] || die "sha256 不符,已丢弃
       期望 $_want
       实际 $_got"
        info "sha256 校验通过"
    else
        die "取不到 .sha256,拒绝安装未校验的二进制"
    fi

    chmod 755 "$_tmp"
    mv -f "$_tmp" "$_dest"
    trap - EXIT INT TERM
    rm -f "$_sum"
}

# 单元在跑才重启。没跑的话装完不该顺手把它拉起来 ——
# 那等于替用户决定「这台机器现在开始提供服务」。
restart_if_running() {
    _unit="$1"
    [ "$NO_RESTART" -eq 0 ] || { info "跳过重启($_unit),记得手动 systemctl restart $_unit"; return 0; }
    command -v systemctl >/dev/null 2>&1 || return 0
    if systemctl is-active --quiet "$_unit" 2>/dev/null; then
        systemctl restart "$_unit" && info "已重启 $_unit"
    else
        info "$_unit 未在运行,只替换了二进制"
    fi
}

install_master() {
    _new="$1"
    should_install "$(installed_version sbx)" "$_new" "sbx" || return 0

    _name="sbx-v$_new-$RUST_TARGET.tar.gz"
    _tmpd=$(mktemp -d)
    # mktemp -d 的目录也要保证清掉,否则反复升级会在 /tmp 里堆一堆。
    trap 'rm -rf "$_tmpd"' EXIT INT TERM
    curl -fsSL --retry 3 -o "$_tmpd/$_name" "$DL/v$_new/$_name" || die "下载失败: $_name"
    curl -fsSL --retry 3 -o "$_tmpd/$_name.sha256" "$DL/v$_new/$_name.sha256" \
        || die "取不到 .sha256,拒绝安装未校验的产物"
    _want=$(awk '{print $1}' "$_tmpd/$_name.sha256")
    _got=$(sha256sum "$_tmpd/$_name" | awk '{print $1}')
    [ "$_want" = "$_got" ] || die "sha256 不符,已丢弃"
    info "sha256 校验通过"

    tar -xzf "$_tmpd/$_name" -C "$_tmpd"
    _src="$_tmpd/sbx-v$_new-$RUST_TARGET"
    install -m755 "$_src/sbx" "$BIN_DIR/sbx"

    # 示例配置与 unit 只在**不存在时**放过去,绝不覆盖已有的 ——
    # 覆盖 /etc/sbx/config.toml 等于把人家的部署配置冲掉。
    [ -d /etc/sbx ] || install -d -m750 /etc/sbx
    [ -f /etc/sbx/config.example.toml ] && :
    install -m640 "$_src/config.example.toml" /etc/sbx/config.example.toml
    if [ ! -f /etc/systemd/system/sbx.service ]; then
        install -m644 "$_src/sbx.service" /etc/systemd/system/sbx.service
        command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload
        info "已放置 sbx.service(未启用;systemctl enable --now sbx 启动)"
    fi
    rm -rf "$_tmpd"; trap - EXIT INT TERM
    restart_if_running sbx
}

install_agent() {
    _new="$1"
    should_install "$(installed_version sbx-agent)" "$_new" "sbx-agent" || return 0

    _f="sbx-agent-v$_new-linux-$GO_ARCH"
    fetch_verify_install "$DL/v$_new/$_f" "$DL/v$_new/$_f.sha256" "$BIN_DIR/sbx-agent"

    [ -d /etc/sbx ] || install -d -m750 /etc/sbx
    if [ ! -f /etc/systemd/system/sbx-agent.service ]; then
        curl -fsSL -o /etc/systemd/system/sbx-agent.service "$DL/v$_new/sbx-agent.service" \
            && command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload
        info "已放置 sbx-agent.service(未启用;需要先写 /etc/sbx/agent.toml)"
    fi
    if [ ! -f /etc/sbx/agent.example.toml ]; then
        curl -fsSL -o /etc/sbx/agent.example.toml "$DL/v$_new/agent.example.toml" || true
    fi
    restart_if_running sbx-agent
}

main() {
    parse_args "$@"
    need curl curl
    need sha256sum coreutils
    need tar tar
    detect_arch

    [ -w "$BIN_DIR" ] || [ "$(id -u)" = "0" ] || die "$BIN_DIR 不可写,请用 root 运行(或 --bin-dir 指定别处)"
    [ -d "$BIN_DIR" ] || install -d -m755 "$BIN_DIR"

    # SBX_TARGET 走的是环境变量,没经过 parse_args 的校验和 all 展开,这里补上。
    case "$TARGETS" in
        '') ;;
        all) TARGETS="master agent" ;;
        master|agent|'master agent'|'agent master') ;;
        *) die "SBX_TARGET 只能是 master / agent / all,收到:$TARGETS" ;;
    esac

    # 没显式指定目标时,按本机已装的东西决定升谁。
    if [ -z "$TARGETS" ]; then
        [ -x "$BIN_DIR/sbx" ] && TARGETS="$TARGETS master"
        [ -x "$BIN_DIR/sbx-agent" ] && TARGETS="$TARGETS agent"
        [ -n "$TARGETS" ] || die "本机还没装过 sbx。首次安装请显式指定装哪个:

       $(invocation) agent     # 被控机
       $(invocation) master    # 主控机

       (或者用环境变量:SBX_TARGET=agent)"
        info "检测到已安装:$(printf '%s' "$TARGETS" | tr -s ' ')"
    fi

    if [ -n "$WANT_VERSION" ]; then
        VERSION="$WANT_VERSION"
        info "指定版本 $VERSION"
    else
        VERSION=$(latest_version)
        info "最新版 $VERSION"
    fi

    for t in $TARGETS; do
        case "$t" in
            master) install_master "$VERSION" ;;
            agent)  install_agent  "$VERSION" ;;
        esac
    done
    printf '完成。\n'
}

main "$@"
