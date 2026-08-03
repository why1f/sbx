#!/bin/sh
# sbx 一键安装 / 升级脚本。
#
#   # 装主控(不带参数就是这个)
#   curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | bash
#
#   # 装被控 agent
#   curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh | bash -s -- agent
#
#   # 装被控 agent 并**直接接入某台主控**(主控 TUI 的「新增被控服务器」会吐出整条命令)
#   curl -fsSL .../install.sh | SBX_SERVER='wss://1.2.3.4:18443/ws' \
#       SBX_TOKEN='…' SBX_FINGERPRINT='sha256:…' bash
#
# 选目标的优先级:
#   1. 命令行参数 / SBX_TARGET
#   2. 给了 SBX_TOKEN —— 那只可能是在装被控
#   3. 本机**已经装了什么**就升什么 —— 一台只跑 agent 的机器上跑裸命令
#      应该升那个 agent,而不是莫名多出一个主控
#   4. 都没有 → 装主控
#
# 已是最新版就什么都不做。下载后**强制校验 sha256**,取不到校验和宁可拒装。
#
# 用 POSIX sh 写(`| sh` 和 `| bash` 都能跑):被控机上可能只有 dash/busybox。
#
# ── 关于 `curl | bash` ──
# 整个脚本包在函数里,最后一行才 `main "$@"`。这样连接中断导致下载不完整时,
# shell 读到的是一堆没被调用的函数定义,不会执行到一半就动你的系统。

set -eu

REPO="why1f/sbx"
BIN_DIR="${SBX_BIN_DIR:-/usr/local/bin}"
# 配置目录。改它就得自己改 unit 里的 ExecStart —— sbx-agent.service 写死了
# /etc/sbx/agent.toml。留这个口子主要是为了能在真机之外测这个脚本。
CONF_DIR="${SBX_CONF_DIR:-/etc/sbx}"
UNIT_DIR="${SBX_UNIT_DIR:-/etc/systemd/system}"
API="https://api.github.com/repos/$REPO/releases/latest"
DL="https://github.com/$REPO/releases/download"
RAW="https://raw.githubusercontent.com/$REPO/main/packaging/install.sh"

AGENT_CONF="$CONF_DIR/agent.toml"

WANT_VERSION=""     # 空 = 用 latest
FORCE=0
NO_RESTART=0
TARGETS=""          # master / agent,空 = 自动判断

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
        *) printf 'curl -fsSL %s | bash -s --' "$RAW" ;;
    esac
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "缺少 $1,请先安装(apt install $2 / yum install $2)"
}

usage() {
    _i=$(invocation)
    cat <<EOF
用法: $_i [master|agent|all] [选项]

  不带目标时:本机装过什么就升什么;都没装过则装**主控**。

选项:
  --version <X.Y.Z>   装指定版本(默认最新)
  --force             即使已是该版本也重新装一遍
  --no-restart        替换二进制后不重启 systemd 单元
  --bin-dir <目录>    安装目录(默认 /usr/local/bin,也可用 SBX_BIN_DIR)
  -h, --help          这段

环境变量(自动化场景比传参省事,任何调用形式都能用):
  SBX_TARGET=agent    等价于把 agent 作为参数
  SBX_BIN_DIR=/opt/bin

被控机接入(给了 SBX_TOKEN 就默认装 agent,并写好配置直接起服务):
  SBX_SERVER=wss://主控:18443/ws   主控 WS 地址
  SBX_TOKEN=<agent-add 给的 token>
  SBX_FINGERPRINT=sha256:…         主控证书指纹(TLS 时必填)
  SBX_INSECURE=1                   主控是明文 ws:// 时用这个替代指纹
  这几个值不用自己拼:主控上 \`sbx tui\` → 服务管理页 → [a] 新增,会直接给出整条命令。
EOF
}

# 往 TARGETS 里加一个目标,不产生前导空格。
#
# 之前直接 `TARGETS="$TARGETS $1"` 会得到 " master",而后面用 case 精确匹配
# `master` 就对不上 —— v0.1.2 的回归正是这么来的。
add_target() {
    case " $TARGETS " in
        *" $1 "*) ;;                                    # 已经有了,别加两遍
        *) TARGETS="${TARGETS:+$TARGETS }$1" ;;
    esac
}

# 环境变量形式的目标。**在这里校验**,而不是等和命令行参数合并之后 ——
# 合并之后就分不清「用户填错了 SBX_TARGET」和「parse_args 自己拼出来的值」了。
init_targets_from_env() {
    case "${SBX_TARGET:-}" in
        '') ;;
        all) add_target master; add_target agent ;;
        master|agent) add_target "$SBX_TARGET" ;;
        *) die "SBX_TARGET 只能是 master / agent / all,收到:${SBX_TARGET}" ;;
    esac
    # 给了 token 就只可能是在装被控 —— 主控不需要 token 去连谁。
    # 有了这条,主控 TUI 吐出来的命令就不必再带一个 SBX_TARGET=agent,
    # 而那条命令本来就已经够长了。
    [ -n "${SBX_TOKEN:-}" ] && add_target agent
    return 0
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            master|agent) add_target "$1" ;;
            all) add_target master; add_target agent ;;
            --version) [ $# -ge 2 ] || die "--version 后面要跟版本号"; WANT_VERSION="${2#v}"; shift ;;
            --force) FORCE=1 ;;
            --no-restart) NO_RESTART=1 ;;
            --bin-dir) [ $# -ge 2 ] || die "--bin-dir 后面要跟目录"; BIN_DIR="$2"; shift ;;
            -h|--help) usage; exit 0 ;;
            *) die "不认识的参数: $1($(invocation) --help 看用法)" ;;
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

# 把主控给的接入信息写成 /etc/sbx/agent.toml。没给 SBX_TOKEN 就什么都不做 ——
# 那是「只升级二进制」的情形,绝不能顺手覆盖人家已经在用的配置。
#
# 给了 token 就**覆盖**(旧的先备份):这条命令是主控 TUI 在「新增 agent」或
# 「轮换 token」之后吐出来的,人跑它的意图正是「用这份新凭据接入」。
write_agent_config() {
    [ -n "${SBX_TOKEN:-}" ] || return 0
    [ -n "${SBX_SERVER:-}" ] || die "给了 SBX_TOKEN 就必须给 SBX_SERVER(形如 wss://主控地址:18443/ws)"

    # wss 却既没有指纹也没说要跳过校验 —— 这种配置 agent 起不来,
    # 与其装完再让人去看日志,不如现在就说清楚(§1.3 的 TOFU 固定)。
    case "$SBX_SERVER" in
        wss://*)
            if [ -z "${SBX_FINGERPRINT:-}" ] && [ "${SBX_INSECURE:-0}" != "1" ]; then
                die "wss:// 需要 SBX_FINGERPRINT=sha256:…(主控上跑 \`sbx fingerprint\` 可以打印),
       或者显式 SBX_INSECURE=1 跳过校验(只建议在内网调试时用)"
            fi
            ;;
    esac

    [ -d "$CONF_DIR" ] || install -d -m750 "$CONF_DIR"
    if [ -f "$AGENT_CONF" ]; then
        cp -p "$AGENT_CONF" "$AGENT_CONF.bak"
        info "原配置已备份为 $AGENT_CONF.bak"
    fi

    # 先写临时文件再 mv:中途失败不会留下一个半截的配置,
    # 而半截配置会让 agent 反复起-崩,比根本没配还难查。
    #
    # umask 要**存下来再改回去**:这个函数不是脚本的最后一步,
    # 让 077 漏到后面会顺手改掉别的文件的权限。
    _conf_tmp="$AGENT_CONF.new.$$"
    trap 'rm -f "$_conf_tmp"' EXIT INT TERM
    _old_umask=$(umask)
    umask 077
    printf 'server      = "%s"\n' "$SBX_SERVER"  > "$_conf_tmp"
    printf 'token       = "%s"\n' "$SBX_TOKEN"  >> "$_conf_tmp"
    if [ -n "${SBX_FINGERPRINT:-}" ]; then
        printf 'fingerprint = "%s"\n' "$SBX_FINGERPRINT" >> "$_conf_tmp"
    fi
    if [ "${SBX_INSECURE:-0}" = "1" ]; then
        printf 'insecure    = true\n' >> "$_conf_tmp"
    fi
    printf 'state_dir   = "%s"\n' "${SBX_STATE_DIR:-/var/lib/sbx-agent}" >> "$_conf_tmp"
    umask "$_old_umask"

    # 0600:里面是明文 token(§8.1)。
    chmod 600 "$_conf_tmp"
    mv -f "$_conf_tmp" "$AGENT_CONF"
    trap - EXIT INT TERM
    info "已写入 $AGENT_CONF(0600)"
}

# 从 release 取一个附属文件(unit、示例配置)。失败**要说出来**:
# 早先这里是一条 `curl … && systemctl …` 的 && 链,curl 失败时 set -e
# 会让整个脚本一声不响地退出,人看到的是「装了一半」。
fetch_asset() {
    _url="$1"; _dest="$2"; _what="$3"
    _asset_tmp="$_dest.new.$$"
    if curl -fsSL --retry 3 -o "$_asset_tmp" "$_url" && [ -s "$_asset_tmp" ]; then
        mv -f "$_asset_tmp" "$_dest"
        return 0
    fi
    rm -f "$_asset_tmp"
    info "取不到 $_what($_url),跳过"
    return 1
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
    # (config.example.toml 是例外:它按定义就是「最新版本长什么样」的样本,
    #  真正的配置是同目录的 config.toml,那个我们碰都不碰。)
    [ -d "$CONF_DIR" ] || install -d -m750 "$CONF_DIR"
    install -m640 "$_src/config.example.toml" "$CONF_DIR/config.example.toml"
    if [ ! -f "$UNIT_DIR/sbx.service" ]; then
        install -m644 "$_src/sbx.service" "$UNIT_DIR/sbx.service"
        command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload
        info "已放置 sbx.service(未启用;systemctl enable --now sbx 启动)"
    fi
    rm -rf "$_tmpd"; trap - EXIT INT TERM
    restart_if_running sbx
}

install_agent() {
    _new="$1"
    # 注意这里**不能**在版本相同时提前 return:带着 SBX_TOKEN 重跑
    # (轮换 token 之后)是正当用法,那时二进制本来就该是最新的,
    # 要做的事情全在下面写配置那一段。
    if should_install "$(installed_version sbx-agent)" "$_new" "sbx-agent"; then
        _f="sbx-agent-v$_new-linux-$GO_ARCH"
        fetch_verify_install "$DL/v$_new/$_f" "$DL/v$_new/$_f.sha256" "$BIN_DIR/sbx-agent"
    fi

    [ -d "$CONF_DIR" ] || install -d -m750 "$CONF_DIR"
    if [ ! -f "$UNIT_DIR/sbx-agent.service" ]; then
        if fetch_asset "$DL/v$_new/sbx-agent.service" "$UNIT_DIR/sbx-agent.service" "sbx-agent.service"; then
            command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload || true
            info "已放置 sbx-agent.service"
        fi
    fi
    if [ ! -f "$CONF_DIR/agent.example.toml" ]; then
        fetch_asset "$DL/v$_new/agent.example.toml" "$CONF_DIR/agent.example.toml" "agent.example.toml" || true
    fi

    write_agent_config
    start_agent
}

# 装完之后要不要把 agent 跑起来。
#
# 分两种情况,差别是**意图**:
#   * 这次带了接入信息(SBX_TOKEN)—— 人明确说了「这台机器要接进集群」,
#     那就 enable --now,装完即用;
#   * 没带 —— 只是升级二进制,沿用「本来在跑才重启」的规矩:
#     替一台没在服务的机器决定「你现在开始提供服务」不是安装脚本该干的事。
start_agent() {
    if [ "$NO_RESTART" -ne 0 ]; then
        info "跳过启动/重启(--no-restart),记得手动 systemctl restart sbx-agent"
        return 0
    fi
    if [ -z "${SBX_TOKEN:-}" ]; then
        restart_if_running sbx-agent
        return 0
    fi
    command -v systemctl >/dev/null 2>&1 || {
        info "没有 systemd。手动跑:$BIN_DIR/sbx-agent $AGENT_CONF"
        return 0
    }
    [ -f "$UNIT_DIR/sbx-agent.service" ] || {
        info "没有 sbx-agent.service,不启动。手动跑:$BIN_DIR/sbx-agent $AGENT_CONF"
        return 0
    }
    if systemctl enable --now sbx-agent 2>/dev/null; then
        # 已经在跑的进程读的是旧配置(比如旧 token),必须重启才会用新的。
        systemctl restart sbx-agent || true
        info "sbx-agent 已启用并启动。看日志:journalctl -u sbx-agent -f"
    else
        info "systemctl enable --now sbx-agent 失败,自己看一眼:systemctl status sbx-agent"
    fi
}

main() {
    init_targets_from_env
    parse_args "$@"
    need curl curl
    need sha256sum coreutils
    need tar tar
    detect_arch

    [ -w "$BIN_DIR" ] || [ "$(id -u)" = "0" ] || die "$BIN_DIR 不可写,请用 root 运行(或 --bin-dir 指定别处)"
    [ -d "$BIN_DIR" ] || install -d -m755 "$BIN_DIR"

    # 没显式指定目标时:先看本机已经装了什么。
    #
    # 「已装什么就升什么」必须排在「默认装主控」前面 —— 一台只跑 agent 的
    # 被控机上跑裸命令,意图显然是升级那个 agent,而不是给它安一个主控。
    if [ -z "$TARGETS" ]; then
        [ -x "$BIN_DIR/sbx" ] && add_target master
        [ -x "$BIN_DIR/sbx-agent" ] && add_target agent
        if [ -n "$TARGETS" ]; then
            info "检测到已安装:$TARGETS"
        else
            TARGETS="master"
            info "本机还没装过,默认装主控(要装被控端就加 agent:$(invocation) agent)"
        fi
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

# `SBX_SOURCE_ONLY=1` 只定义函数、不动系统 —— `test-install.sh` 靠它逐个测函数。
# 判断本身仍然是**最后一行**,所以「下载不完整 → 什么都不执行」的性质没变:
# 截断的脚本读到的依旧只是一堆没被调用的函数定义。
[ "${SBX_SOURCE_ONLY:-0}" = "1" ] || main "$@"
