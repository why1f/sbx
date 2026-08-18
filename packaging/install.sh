#!/bin/sh
# sbx 一键安装 / 升级脚本。
#
#   # 装主控(不带参数就是这个)
#   (curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh 2>/dev/null \
#     || wget -qO- https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh) | sh
#
#   # 装被控 agent
#   (curl -fsSL https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh 2>/dev/null \
#     || wget -qO- https://raw.githubusercontent.com/why1f/sbx/main/packaging/install.sh) | sh -s -- agent
#
#   # 装被控 agent 并**直接接入某台主控**(主控 TUI 的「新增被控服务器」会吐出整条命令)
#   curl -fsSL .../install.sh | SBX_SERVER='wss://1.2.3.4:18443/ws' \
#       SBX_TOKEN='…' SBX_FINGERPRINT='sha256:…' sh
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
# ── 关于 `download | sh` ──
# 整个脚本包在函数里,最后一行才 `main "$@"`。这样连接中断导致下载不完整时,
# shell 读到的是一堆没被调用的函数定义,不会执行到一半就动你的系统。

set -eu

REPO="why1f/sbx"
BIN_DIR="${SBX_BIN_DIR:-/usr/local/bin}"
# 配置目录。改它就得自己改 service 里的启动参数 —— systemd/OpenRC 文件都写死了
# /etc/sbx/agent.toml。留这个口子主要是为了能在真机之外测这个脚本。
CONF_DIR="${SBX_CONF_DIR:-/etc/sbx}"
# systemd 与 OpenRC 的服务文件分别落在这两处。环境变量只用于离线测试/非标准布局。
UNIT_DIR="${SBX_UNIT_DIR:-/etc/systemd/system}"
INIT_DIR="${SBX_INIT_DIR:-/etc/init.d}"
INIT_SYSTEM="none"
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
        *) printf '(curl -fsSL %s 2>/dev/null || wget -qO- %s) | sh -s --' "$RAW" "$RAW" ;;
    esac
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "缺少 $1,请先安装(apt/yum: $2; Alpine: apk add $2)"
}

need_downloader() {
    command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 ||
        die "缺少下载工具,请先安装 curl 或 wget(Alpine: apk add curl)"
}

usage() {
    _i=$(invocation)
    cat <<EOF
用法: $_i [master|agent|all] [选项]

  不带目标时:本机装过什么就升什么;都没装过则装**主控**。

选项:
  --version <X.Y.Z>   装指定版本(默认最新)
  --force             即使已是该版本也重新装一遍
  --no-restart        替换二进制后不重启 systemd/OpenRC 服务
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

# 不按发行版名字猜 init。Alpine 容器也有 /etc/alpine-release,但 PID 1 通常不是
# OpenRC;硬看发行版会装出一个永远没人拉起的 service。命令真实存在才采用。
detect_init_system() {
    if command -v systemctl >/dev/null 2>&1; then
        INIT_SYSTEM="systemd"
    elif command -v rc-service >/dev/null 2>&1 &&
         command -v rc-update >/dev/null 2>&1 &&
         command -v openrc-run >/dev/null 2>&1; then
        INIT_SYSTEM="openrc"
    else
        INIT_SYSTEM="none"
    fi
    info "init 系统:$INIT_SYSTEM"
}

# agent.example.toml 属于源码,不重复占 GitHub Release 资产。按**目标版本 tag**取,
# 不能固定 main:用 --version 装旧二进制时示例配置也该是同版本的。
#
# 只有这个示例文件走网络。**service 文件不走** —— 它们是部署能不能熬过一次重启的
# 前提,不能挂在第二个必须可达的域名上(见 write_agent_unit_systemd 上面那段)。
# 这个取不到只是少一份参考样本,不影响 agent 运行。
source_asset_url() {
    printf 'https://raw.githubusercontent.com/%s/v%s/packaging/%s' "$REPO" "$1" "$2"
}

latest_version() {
    # 只要 tag_name。不引 jq —— 被控机上通常没有。
    v=$(http_body "$API" 2>/dev/null | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1)
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
# 从网络取文件。curl 优先,Alpine/BusyBox 没有 curl 时走 wget。
download() {
    _u="$1"; _o="$2"
    _n=0
    while :; do
        if command -v curl >/dev/null 2>&1; then
            curl -fsSL --retry 3 -o "$_o" "$_u" && return 0
        else
            wget -q -O "$_o" "$_u" && return 0
        fi
        _n=$((_n + 1))
        [ "$_n" -ge 3 ] && return 1
        sleep 3
    done
}

# 取 API/文本响应到 stdout。不能用 `$(download ...)` —— download 的语义是写文件。
http_body() {
    _u="$1"
    _n=0
    while :; do
        if command -v curl >/dev/null 2>&1; then
            curl -fsSL --retry 3 "$_u" && return 0
        else
            wget -q -O - "$_u" && return 0
        fi
        _n=$((_n + 1))
        [ "$_n" -ge 3 ] && return 1
        sleep 3
    done
}

fetch_verify_install() {
    _url="$1"; _sum_url="$2"; _dest="$3"
    _tmp="$_dest.new.$$"
    _sum="$_tmp.sha256"
    # 无论从哪条路径退出都别留垃圾。
    trap 'rm -f "$_tmp" "$_sum"' EXIT INT TERM

    download "$_url" "$_tmp" || die "下载失败: $_url"
    if download "$_sum_url" "$_sum"; then
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

manual_restart_command() {
    case "$INIT_SYSTEM" in
        systemd) printf 'systemctl restart %s' "$1" ;;
        openrc)  printf 'rc-service %s restart' "$1" ;;
        *)       printf '%s/%s %s' "$BIN_DIR" "$1" "$AGENT_CONF" ;;
    esac
}

manual_enable_command() {
    case "$INIT_SYSTEM" in
        systemd) printf 'systemctl enable --now %s' "$1" ;;
        openrc)  printf 'rc-update add %s default && rc-service %s start' "$1" "$1" ;;
        *)       printf '这台机器没有 systemd/OpenRC,开机自启得自己接一个 supervisor' ;;
    esac
}

service_is_active() {
    case "$INIT_SYSTEM" in
        systemd) systemctl is-active --quiet "$1" 2>/dev/null ;;
        openrc)  rc-service "$1" status >/dev/null 2>&1 ;;
        *)       return 1 ;;
    esac
}

# 开机自启是否已经设好。
#
# OpenRC 侧不用 `grep sbx-agent` —— `rc-update show default` 的输出是
# `   sbx-agent | default` 这种带分隔符的表格,子串匹配会被别的服务名蹭上
# (比如某天多一个 sbx-agent-exporter)。用 awk 精确比第一个字段。
service_is_enabled() {
    case "$INIT_SYSTEM" in
        systemd) systemctl is-enabled --quiet "$1" 2>/dev/null ;;
        openrc)  rc-update show default 2>/dev/null |
                     awk -v n="$1" '$1 == n { hit = 1 } END { exit !hit }' ;;
        *)       return 1 ;;
    esac
}

service_enable() {
    case "$INIT_SYSTEM" in
        # 只 enable,不 --now:「开机要不要起」和「现在要不要起」是两件事,
        # 调用方各自决定后者(接入时 restart,升级时沿用原来的运行状态)。
        systemd) systemctl enable "$1" >/dev/null 2>&1 ;;
        openrc)  rc-update add "$1" default >/dev/null 2>&1 ;;
        *)       return 1 ;;
    esac
}

# 确保 agent 开机自启。**每条安装/升级路径都要过一遍。**
#
# 这不是「要不要现在开始提供服务」那类策略选择,而是部署的一部分:一台在跑但没
# enable 的 agent 从外面看完全正常,直到下一次重启 —— 它再也不上线,而主控只显示
# 一盏灭掉的灯,不会告诉你「这台机器的服务没设开机自启」。v0.4.18~v0.4.22 取不到
# unit 文件的机器就是这样:靠手工命令跑着,重启即失联。所以升级时顺手补上,
# 已经 enable 的是无副作用的空操作。
ensure_boot_autostart() {
    _unit="$1"
    case "$INIT_SYSTEM" in
        systemd|openrc) ;;
        *) return 0 ;;
    esac
    service_is_enabled "$_unit" && return 0
    if service_enable "$_unit" && service_is_enabled "$_unit"; then
        info "已设置 $_unit 开机自启($INIT_SYSTEM)"
        return 0
    fi
    info "警告:$_unit 没设开机自启,这台机器重启后不会自动上线。手动执行:$(manual_enable_command "$_unit")"
    return 1
}

service_restart() {
    case "$INIT_SYSTEM" in
        systemd) systemctl restart "$1" ;;
        openrc)  rc-service "$1" restart ;;
        *)       return 1 ;;
    esac
}

# 主控只支持 systemd,不能跟着 agent 的 OpenRC 分流走。保持原有语义:
# systemd 单元在跑才重启,没有 systemctl 或未运行都只替换文件。
restart_master_if_running() {
    if [ "$NO_RESTART" -ne 0 ]; then
        info "跳过重启(sbx),需要时手动执行:systemctl restart sbx"
        return 0
    fi
    command -v systemctl >/dev/null 2>&1 || return 0
    if systemctl is-active --quiet sbx 2>/dev/null; then
        systemctl restart sbx && info "已重启 sbx(systemd)"
    else
        info "sbx 未在运行,只替换了二进制"
    fi
}

# agent 单元在跑才重启。没跑的话装完不该顺手把它拉起来 ——
# 那等于替用户决定「这台机器现在开始提供服务」。systemd/OpenRC 语义一致。
#
# 但**开机自启要补**:在跑说明这台机器就是要提供服务的,那它重启后也应该回来。
restart_if_running() {
    _unit="$1"
    if [ "$NO_RESTART" -ne 0 ]; then
        info "跳过重启($_unit),需要时手动执行:$(manual_restart_command "$_unit")"
        return 0
    fi
    if service_is_active "$_unit"; then
        # `|| true`:设不上开机自启只是一条警告,不该让整条安装命令带着 set -e 退出
        # —— 那会把「二进制已经换好了」这件事也一起吞掉。
        ensure_boot_autostart "$_unit" || true
        service_restart "$_unit" && info "已重启 $_unit($INIT_SYSTEM)"
    else
        info "$_unit 未在运行,只替换了二进制"
        info "要让它现在启动并开机自启:$(manual_enable_command "$_unit")"
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
    if download "$_url" "$_asset_tmp" && [ -s "$_asset_tmp" ]; then
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
    download "$DL/v$_new/$_name" "$_tmpd/$_name" || die "下载失败: $_name"
    download "$DL/v$_new/$_name.sha256" "$_tmpd/$_name.sha256" \
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
    restart_master_if_running
}

# ── 服务文件由脚本自带,不从网络取 ──
#
# v0.4.18 把 unit / OpenRC 文件从 Release 资产改成按 tag 去
# raw.githubusercontent.com 取。那台主机在不少网络里(尤其是国内 VPS)会被
# DNS 污染或 TLS 重置;取不到时 fetch_asset 只说一句「跳过」,紧接着
# start_agent 因为找不到 unit 而退化成「你自己手动跑一下」——
# **手动跑起来的进程熬不过一次重启**。现场症状是「装完能用,关机再开机就
# 再也不上线了」,而且从主控那边只看到一盏灭掉的灯,完全看不出跟网络有关。
#
# 这两个文件是几十行静态文本,本来就不该多依赖一个必须可达的域名。写死在
# 这里之后接入流程只需要 GitHub Release 一个下载源;
# `test-install.sh` 有一条 golden 用例保证它们和 packaging/ 下的文件逐字节一致,
# 改了那边忘了改这边会在 CI 里挂掉,而不是等下一次重启才在机房里暴露。
write_agent_unit_systemd() {
    cat > "$1" <<'SBX_AGENT_SERVICE_EOF'
[Unit]
Description=sbx agent (embedded sing-box, managed by sbx master)
Documentation=https://github.com/why1f/sbx
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/sbx-agent /etc/sbx/agent.toml

# Restart=always 是 §11.2 自升级的**前提**,不是可选的加固项:
# agent.upgrade 的收尾动作是「替换掉自己的二进制,然后退出」——
# 靠 supervisor 拉起新版本。没有它,一次升级等于一次永久下线。
Restart=always
RestartSec=3

# last-applied.json 落在 /var/lib/sbx-agent(agent.toml 里的 state_dir)。
# StateDirectory 让 systemd 建好目录并设属主。
StateDirectory=sbx-agent
StateDirectoryMode=0750
ConfigurationDirectory=sbx
ConfigurationDirectoryMode=0750

# sing-box 要监听特权端口(443 等),但不需要 root 的其余能力。
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

# 自升级要用 rename 覆盖 /usr/local/bin/sbx-agent,所以那条路径必须可写 ——
# 这是 ProtectSystem=strict 下唯一需要额外开口的地方。
# 不用自升级的部署可以删掉这一行,那时 agent.upgrade 会明确报错而不是静默失败。
ProtectSystem=strict
ReadWritePaths=/usr/local/bin

NoNewPrivileges=true
PrivateTmp=true
ProtectHome=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictNamespaces=true
LockPersonality=true

# 代理进程的连接数很容易顶到默认的 1024。
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
SBX_AGENT_SERVICE_EOF
}

write_agent_unit_openrc() {
    cat > "$1" <<'SBX_AGENT_OPENRC_EOF'
#!/sbin/openrc-run
# sbx-agent OpenRC service.
#
# `supervise-daemon` is deliberate: agent.upgrade replaces the running binary and
# exits so it can be re-execed. OpenRC must bring it back just like systemd's
# `Restart=always` does on the other supported Linux distributions.

name="sbx-agent"
description="sbx agent (embedded sing-box, managed by sbx master)"

command="/usr/local/bin/sbx-agent"
command_args="/etc/sbx/agent.toml"
command_user="root:root"

supervisor="supervise-daemon"
respawn_delay=3
respawn_max=0

output_log="/var/log/sbx-agent.log"
error_log="/var/log/sbx-agent.log"

depend() {
    need net
    after firewall
}

start_pre() {
    if [ ! -r /etc/sbx/agent.toml ]; then
        eerror "缺少 /etc/sbx/agent.toml;请先运行主控生成的一键接入命令"
        return 1
    fi
    checkpath --file --mode 0600 /etc/sbx/agent.toml
    checkpath --directory --mode 0750 /var/lib/sbx-agent
    checkpath --file --mode 0640 /var/log/sbx-agent.log
}
SBX_AGENT_OPENRC_EOF
}

install_agent_assets() {
    _asset_ver="$1"
    [ -d "$CONF_DIR" ] || install -d -m750 "$CONF_DIR"
    case "$INIT_SYSTEM" in
        systemd)
            [ -d "$UNIT_DIR" ] || install -d -m755 "$UNIT_DIR"
            # 已有的 unit 绝不覆盖 —— 人可能改过 LimitNOFILE、加过代理环境变量。
            if [ ! -f "$UNIT_DIR/sbx-agent.service" ]; then
                write_agent_unit_systemd "$UNIT_DIR/sbx-agent.service"
                chmod 644 "$UNIT_DIR/sbx-agent.service"
                systemctl daemon-reload 2>/dev/null || true
                info "已放置 sbx-agent.service"
            fi
            ;;
        openrc)
            [ -d "$INIT_DIR" ] || install -d -m755 "$INIT_DIR"
            if [ ! -f "$INIT_DIR/sbx-agent" ]; then
                write_agent_unit_openrc "$INIT_DIR/sbx-agent"
                chmod 755 "$INIT_DIR/sbx-agent"
                info "已放置 OpenRC service:$INIT_DIR/sbx-agent"
            fi
            ;;
        none)
            info "没有 systemd/OpenRC supervisor,agent 可手动运行但崩溃/自升级后不会自动拉起"
            ;;
    esac
    if [ ! -f "$CONF_DIR/agent.example.toml" ]; then
        fetch_asset "$(source_asset_url "$_asset_ver" agent.example.toml)" \
            "$CONF_DIR/agent.example.toml" "agent.example.toml" || true
    fi
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

    install_agent_assets "$_new"

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
        info "跳过启动/重启(--no-restart),需要时手动执行:$(manual_restart_command sbx-agent)"
        return 0
    fi
    if [ -z "${SBX_TOKEN:-}" ]; then
        restart_if_running sbx-agent
        return 0
    fi

    case "$INIT_SYSTEM" in
        systemd)
            [ -f "$UNIT_DIR/sbx-agent.service" ] || {
                info "没有 sbx-agent.service,不启动。手动跑:$BIN_DIR/sbx-agent $AGENT_CONF"
                info "注意:这样跑起来的进程重启后不会自动回来"
                return 0
            }
            ensure_boot_autostart sbx-agent || true
            # 已经在跑的进程读的是旧配置(比如旧 token),必须重启才会用新的。
            # restart 对没在跑的单元等价于 start,所以这一条同时覆盖首次接入。
            systemctl restart sbx-agent 2>/dev/null || true
            ;;
        openrc)
            [ -x "$INIT_DIR/sbx-agent" ] || {
                info "没有 OpenRC service,不启动。手动跑:$BIN_DIR/sbx-agent $AGENT_CONF"
                info "注意:这样跑起来的进程重启后不会自动回来"
                return 0
            }
            ensure_boot_autostart sbx-agent || true
            if rc-service sbx-agent status >/dev/null 2>&1; then
                rc-service sbx-agent restart || true
            else
                rc-service sbx-agent start || true
            fi
            ;;
        none)
            info "没有 systemd/OpenRC,手动跑:$BIN_DIR/sbx-agent $AGENT_CONF"
            info "没有 supervisor 时 agent 自升级退出后不会自动拉起"
            return 0
            ;;
    esac

    # **起来了没有,要当场说。** 之前这里只报告「已启用并启动」,
    # 那句话在单元起不来时同样会打出来,人看到成功提示就走了,等主控那盏灯
    # 一直不亮才回来查 —— 而那时早就不记得装的时候有没有异常了。
    if service_is_active sbx-agent; then
        case "$INIT_SYSTEM" in
            systemd) info "sbx-agent 已启动。看日志:journalctl -u sbx-agent -f" ;;
            openrc)  info "sbx-agent 已启动。看日志:tail -f /var/log/sbx-agent.log" ;;
        esac
    else
        case "$INIT_SYSTEM" in
            systemd) info "警告:sbx-agent 没起来。看原因:systemctl status sbx-agent;journalctl -u sbx-agent -n 30 --no-pager" ;;
            openrc)  info "警告:sbx-agent 没起来。看原因:rc-service sbx-agent status;tail -n 30 /var/log/sbx-agent.log" ;;
        esac
    fi
}

main() {
    init_targets_from_env
    parse_args "$@"
    need_downloader
    need sha256sum coreutils
    need tar tar
    detect_arch
    detect_init_system

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
