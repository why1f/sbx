#!/bin/sh
# install.sh 的单元测试。
#
#   sh packaging/test-install.sh
#
# 为什么需要它:`install.sh` 是一条 `download | sh` 的脚本,过去唯一的验证方式是
# **在一台真机上跑一遍**。v0.1.2 的回归(`sh -s -- master` 直接报错)正是这样溜出去的
# —— 当时我"验证"时拉的是 GitHub 上的旧脚本,等于用旧代码验证新改动。
#
# 这里靠 `SBX_SOURCE_ONLY=1` 把脚本当函数库 source 进来,再把 BIN_DIR / CONF_DIR /
# UNIT_DIR 指到临时目录上,于是**不碰系统、不联网**也能测到:参数解析、目标选择、
# 配置生成、以及那几条「填错了要拦下来」的规则。
#
# 网络相关的部分(真实下载、sha256 校验)测不到;init 检测与服务调用用 fake 命令测。

set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
INSTALL_SH="$SCRIPT_DIR/install.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT INT TERM

PASS=0
FAIL=0

ok() { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
no() { FAIL=$((FAIL + 1)); printf '  FAIL %s\n     %s\n' "$1" "$2"; }

check() { # check <说明> <期望> <实际>
    if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "期望 [$2],得到 [$3]"; fi
}

contains() { # contains <说明> <针> <干草堆>
    case "$3" in
        *"$2"*) ok "$1" ;;
        *) no "$1" "输出里找不到 [$2]:$3" ;;
    esac
}

lacks() { # lacks <说明> <针> <干草堆>
    case "$3" in
        *"$2"*) no "$1" "输出里不该有 [$2]:$3" ;;
        *) ok "$1" ;;
    esac
}

# 在一个干净的子 shell 里 source 脚本,回显某个表达式的值。
# 每次都是新的子 shell —— 上一条用例设的环境变量不会漏到下一条。
run() { # run <环境赋值...> -- <要 eval 的表达式>
    env SBX_SOURCE_ONLY=1 \
        SBX_BIN_DIR="$TMP/bin" SBX_CONF_DIR="$TMP/etc" SBX_UNIT_DIR="$TMP/unit" \
        SBX_INIT_DIR="$TMP/init" \
        "$@" 2>&1
}

echo "── POSIX sh 语法 ──────────────────────────"
if sh -n "$INSTALL_SH"; then ok "install.sh 通过 sh -n"; else no "install.sh 通过 sh -n" "语法错误"; fi
if sh -n "$SCRIPT_DIR/sbx-agent.openrc"; then ok "OpenRC service 通过 sh -n"; else no "OpenRC service 通过 sh -n" "语法错误"; fi

echo "── 目标选择 ────────────────────────────────"

# 裸命令 = 装主控(v0.1.3 起)。
out=$(run sh -c ". '$INSTALL_SH'; init_targets_from_env; parse_args; printf '%s' \"\$TARGETS\"")
check "不带参数不带环境变量 → 空(main 里再回落到 master)" "" "$out"

out=$(run sh -c ". '$INSTALL_SH'; init_targets_from_env; parse_args master; printf '%s' \"\$TARGETS\"")
check "命令行 master" "master" "$out"

# v0.1.2 的回归锚点:拼接出前导空格,后面 case 精确匹配 master 就对不上。
out=$(run sh -c ". '$INSTALL_SH'; init_targets_from_env; parse_args master; printf '[%s]' \"\$TARGETS\"")
check "TARGETS 不能有前导空格(v0.1.2 回归)" "[master]" "$out"

out=$(run sh -c ". '$INSTALL_SH'; init_targets_from_env; parse_args all; printf '%s' \"\$TARGETS\"")
check "all = 两个都装" "master agent" "$out"

out=$(run sh -c ". '$INSTALL_SH'; init_targets_from_env; parse_args agent agent; printf '%s' \"\$TARGETS\"")
check "重复的目标只算一次" "agent" "$out"

out=$(run SBX_TARGET=agent sh -c ". '$INSTALL_SH'; init_targets_from_env; parse_args; printf '%s' \"\$TARGETS\"")
check "SBX_TARGET=agent" "agent" "$out"

out=$(run SBX_TARGET=nonsense sh -c ". '$INSTALL_SH'; init_targets_from_env" || true)
contains "SBX_TARGET 填错要报错" "只能是 master / agent / all" "$out"

# 给了 token 就一定是在装被控 —— 主控不需要 token 去连谁。
out=$(run SBX_TOKEN=tok sh -c ". '$INSTALL_SH'; init_targets_from_env; parse_args; printf '%s' \"\$TARGETS\"")
check "有 SBX_TOKEN → 默认装 agent" "agent" "$out"

out=$(run SBX_TOKEN=tok SBX_TARGET=all sh -c ". '$INSTALL_SH'; init_targets_from_env; printf '%s' \"\$TARGETS\"")
check "显式 all + token 不会把 agent 加两遍" "master agent" "$out"

out=$(run sh -c ". '$INSTALL_SH'; parse_args --nope" || true)
contains "不认识的参数要报错" "不认识的参数" "$out"

out=$(run sh -c ". '$INSTALL_SH'; parse_args --version" || true)
contains "--version 少了值要报错" "后面要跟版本号" "$out"

out=$(run sh -c ". '$INSTALL_SH'; parse_args --version v1.2.3; printf '%s' \"\$WANT_VERSION\"")
check "--version 去掉前导 v" "1.2.3" "$out"

echo "── 提示语跟着调用方式走 ────────────────────"

# `curl … | sh` 时 \$0 是 "sh",这时提示 `sh master` 会被当成脚本文件名去打开
# (报 cannot open master)—— v0.1.2 修的就是这个。
out=$(run sh -c ". '$INSTALL_SH'; invocation")
contains "管道调用使用 POSIX sh" "sh -s --" "$out"
contains "管道调用带 wget 回退" "wget -qO-" "$out"

out=$(run sh -c ". '$INSTALL_SH'; usage")
contains "--help 说明 SBX_TOKEN 的用法" "SBX_TOKEN" "$out"
contains "--help 提到 TUI 会给出整条命令" "sbx tui" "$out"

echo "── init 自动检测与服务文件 ─────────────────"

fake_systemd="$TMP/fake-systemd"
fake_openrc="$TMP/fake-openrc"
empty_path="$TMP/empty-path"
mkdir -p "$fake_systemd" "$fake_openrc" "$empty_path"
printf '#!/bin/sh\nexit 0\n' > "$fake_systemd/systemctl"
for x in rc-service rc-update openrc-run; do
    printf '#!/bin/sh\nexit 0\n' > "$fake_openrc/$x"
done
chmod +x "$fake_systemd/systemctl" "$fake_openrc"/*

out=$(run PATH="$fake_systemd" /bin/sh -c ". '$INSTALL_SH'; detect_init_system; printf '%s' \"\$INIT_SYSTEM\"")
contains "有 systemctl → systemd" "systemd" "$out"
out=$(run PATH="$fake_openrc" /bin/sh -c ". '$INSTALL_SH'; detect_init_system; printf '%s' \"\$INIT_SYSTEM\"")
contains "有 rc-service/rc-update/openrc-run → openrc" "openrc" "$out"
out=$(run PATH="$empty_path" /bin/sh -c ". '$INSTALL_SH'; detect_init_system; printf '%s' \"\$INIT_SYSTEM\"")
contains "两种 init 都没有 → none" "none" "$out"

out=$(run sh -c ". '$INSTALL_SH'; source_asset_url 1.2.3 agent.example.toml")
check "示例配置按版本 tag 取" \
    "https://raw.githubusercontent.com/why1f/sbx/v1.2.3/packaging/agent.example.toml" "$out"

# service 文件是脚本自带的,不走网络。这两条 golden 用例是那份自带副本和
# packaging/ 下真文件之间的唯一约束 —— 改了一边忘了另一边,这里就挂。
run sh -c ". '$INSTALL_SH'; write_agent_unit_systemd '$TMP/emb.service'" >/dev/null
if diff -u "$SCRIPT_DIR/sbx-agent.service" "$TMP/emb.service" >"$TMP/emb.diff" 2>&1; then
    ok "install.sh 内嵌的 sbx-agent.service 与源文件逐字节一致"
else
    no "install.sh 内嵌的 sbx-agent.service 与源文件逐字节一致" "$(cat "$TMP/emb.diff")"
fi
run sh -c ". '$INSTALL_SH'; write_agent_unit_openrc '$TMP/emb.openrc'" >/dev/null
if diff -u "$SCRIPT_DIR/sbx-agent.openrc" "$TMP/emb.openrc" >"$TMP/emb.diff" 2>&1; then
    ok "install.sh 内嵌的 sbx-agent.openrc 与源文件逐字节一致"
else
    no "install.sh 内嵌的 sbx-agent.openrc 与源文件逐字节一致" "$(cat "$TMP/emb.diff")"
fi

# **v0.4.18 回归锚点。** 当时 unit 文件改成从 raw.githubusercontent.com 按 tag 取,
# 那台主机在国内 VPS 上时常连不上;取不到时脚本只说一句「跳过」,于是机器上根本
# 没有 service,agent 只能手工跑 —— 关机再开机就再也不上线,而主控只显示离线。
# 所以这里让 fetch_asset 全线失败,unit 仍然必须落地。
rm -rf "$TMP/init" "$TMP/etc" "$TMP/unit"
mkdir -p "$TMP/init" "$TMP/etc" "$TMP/unit"
run sh -c "
    . '$INSTALL_SH'
    INIT_SYSTEM=openrc
    fetch_asset() { return 1; }
    install_agent_assets 1.2.3
" >/dev/null
[ -x "$TMP/init/sbx-agent" ] && ok "网络全断时 OpenRC service 仍然安装为可执行文件" \
    || no "网络全断时 OpenRC service 仍然安装为可执行文件" "文件不存在或不可执行"
contains "OpenRC unit 使用 supervise-daemon" 'supervisor="supervise-daemon"' "$(cat "$TMP/init/sbx-agent")"
contains "OpenRC unit 自升级后无限拉起" 'respawn_max=0' "$(cat "$TMP/init/sbx-agent")"
contains "OpenRC unit 修正配置权限" 'checkpath --file --mode 0600 /etc/sbx/agent.toml' "$(cat "$TMP/init/sbx-agent")"

run /bin/sh -c "
    . '$INSTALL_SH'
    INIT_SYSTEM=systemd
    fetch_asset() { return 1; }
    install_agent_assets 1.2.3
" >/dev/null
[ -f "$TMP/unit/sbx-agent.service" ] && ok "网络全断时 sbx-agent.service 仍然落地" \
    || no "网络全断时 sbx-agent.service 仍然落地" "文件不存在"
contains "unit 里有 Restart=always(自升级的前提)" "Restart=always" "$(cat "$TMP/unit/sbx-agent.service")"

# 已有的 unit 不能被覆盖:人可能改过 LimitNOFILE 或加过代理环境变量。
printf 'CUSTOM\n' > "$TMP/unit/sbx-agent.service"
run /bin/sh -c "
    . '$INSTALL_SH'
    INIT_SYSTEM=systemd
    fetch_asset() { return 1; }
    install_agent_assets 1.2.3
" >/dev/null
check "已存在的 unit 不被覆盖" "CUSTOM" "$(cat "$TMP/unit/sbx-agent.service")"
rm -f "$TMP/unit/sbx-agent.service"

mkdir -p "$TMP/etc"
run sh -c "
    . '$INSTALL_SH'
    INIT_SYSTEM=openrc
    fetch_asset() {
        case \"\$3\" in
            agent.example.toml) cp '$SCRIPT_DIR/agent.example.toml' \"\$2\" ;;
            *) return 1 ;;
        esac
    }
    install_agent_assets 1.2.3
" >/dev/null
[ -f "$TMP/etc/agent.example.toml" ] && ok "agent.example.toml 从源码 tag 安装" || no "agent.example.toml 从源码 tag 安装" "文件不存在"

# 用 fake OpenRC 记录 start/restart/rc-update 调用,不碰本机 init。
#
# rc-update 是**有状态**的:`add` 往 $ENABLED_DB 里写一行,`show default` 再读出来。
# ensure_boot_autostart 在 enable 之后会重新查一次确认(enable 报成功但其实没生效
# 是真会发生的事),假命令必须能反映这一点,否则测的就不是同一条路径。
fake_service="$TMP/fake-service"
calls="$TMP/service-calls"
enabled_db="$TMP/openrc-enabled"
mkdir -p "$fake_service"
cat > "$fake_service/rc-service" <<'EOF'
#!/bin/sh
printf 'rc-service %s\n' "$*" >> "$CALLS"
if [ "${2:-}" = "status" ]; then
    [ "${FAKE_ACTIVE:-0}" = "1" ]
    exit $?
fi
exit 0
EOF
cat > "$fake_service/rc-update" <<'EOF'
#!/bin/sh
printf 'rc-update %s\n' "$*" >> "$CALLS"
case "${1:-}" in
    add)
        [ "${FAKE_ENABLE_FAILS:-0}" = "1" ] && exit 1
        printf '%s\n' "${2:-}" >> "$ENABLED_DB"
        ;;
    show)
        [ -f "$ENABLED_DB" ] && awk '{ printf "   %s | default\n", $1 }' "$ENABLED_DB"
        ;;
esac
exit 0
EOF
chmod +x "$fake_service"/*

: > "$calls"
rm -f "$enabled_db"
run PATH="$fake_service:$PATH" CALLS="$calls" ENABLED_DB="$enabled_db" SBX_TOKEN=tok FAKE_ACTIVE=0 \
    sh -c ". '$INSTALL_SH'; INIT_SYSTEM=openrc; start_agent" >/dev/null
body=$(cat "$calls")
contains "OpenRC 首次接入加入 default runlevel" "rc-update add sbx-agent default" "$body"
contains "OpenRC 未运行时 start" "rc-service sbx-agent start" "$body"

: > "$calls"
run PATH="$fake_service:$PATH" CALLS="$calls" ENABLED_DB="$enabled_db" SBX_TOKEN=tok FAKE_ACTIVE=1 \
    sh -c ". '$INSTALL_SH'; INIT_SYSTEM=openrc; start_agent" >/dev/null
contains "OpenRC 已运行且换 token 时 restart" "rc-service sbx-agent restart" "$(cat "$calls")"

# 上一条已经把 sbx-agent 加进 default 了,这一条不该再 add 一遍。
: > "$calls"
run PATH="$fake_service:$PATH" CALLS="$calls" ENABLED_DB="$enabled_db" SBX_TOKEN=tok FAKE_ACTIVE=1 \
    sh -c ". '$INSTALL_SH'; INIT_SYSTEM=openrc; start_agent" >/dev/null
lacks "已在 default runlevel 里就不重复 add" "rc-update add" "$(cat "$calls")"

: > "$calls"
run PATH="$fake_service:$PATH" CALLS="$calls" ENABLED_DB="$enabled_db" FAKE_ACTIVE=1 \
    sh -c ". '$INSTALL_SH'; INIT_SYSTEM=openrc; start_agent" >/dev/null
contains "只升级二进制时,原服务在跑才 restart" "rc-service sbx-agent restart" "$(cat "$calls")"

# **这条是这次故障的正面用例。** 一台在跑但没设开机自启的机器(v0.4.18~v0.4.22
# 取不到 unit、只能手工跑起来的那些)跑一次升级命令,就该把开机自启补上 ——
# 不然它下一次重启还是回不来,而人以为「升级过了应该没问题」。
: > "$calls"
rm -f "$enabled_db"
out=$(run PATH="$fake_service:$PATH" CALLS="$calls" ENABLED_DB="$enabled_db" FAKE_ACTIVE=1 \
    sh -c ". '$INSTALL_SH'; INIT_SYSTEM=openrc; restart_if_running sbx-agent")
contains "升级时给在跑但没自启的服务补上开机自启" "rc-update add sbx-agent default" "$(cat "$calls")"
contains "补上之后要说出来" "已设置 sbx-agent 开机自启" "$out"

# enable 本身失败时不能报成功 —— 那台机器重启后是真的回不来,必须留一句话。
: > "$calls"
rm -f "$enabled_db"
out=$(run PATH="$fake_service:$PATH" CALLS="$calls" ENABLED_DB="$enabled_db" \
        FAKE_ACTIVE=1 FAKE_ENABLE_FAILS=1 \
    sh -c ". '$INSTALL_SH'; INIT_SYSTEM=openrc; restart_if_running sbx-agent")
contains "设不上开机自启要明确警告" "重启后不会自动上线" "$out"
contains "警告里带可照抄的命令" "rc-update add sbx-agent default" "$out"

: > "$calls"
run PATH="$fake_service:$PATH" CALLS="$calls" ENABLED_DB="$enabled_db" SBX_TOKEN=tok FAKE_ACTIVE=1 \
    sh -c ". '$INSTALL_SH'; INIT_SYSTEM=openrc; NO_RESTART=1; start_agent" >/dev/null
check "--no-restart 不调用 OpenRC" "" "$(cat "$calls")"

# systemd 路径保留原来的接入/仅升级语义,也不让主控误走 OpenRC。
fake_systemctl_calls="$TMP/systemctl-calls"
systemd_enabled_db="$TMP/systemd-enabled"
cat > "$fake_systemd/systemctl" <<'EOF'
#!/bin/sh
printf 'systemctl %s\n' "$*" >> "$CALLS"
case "${1:-}" in
    is-active)
        [ "${FAKE_ACTIVE:-0}" = "1" ]
        exit $?
        ;;
    is-enabled)
        [ -f "$ENABLED_DB" ]
        exit $?
        ;;
    enable)
        [ "${FAKE_ENABLE_FAILS:-0}" = "1" ] && exit 1
        : > "$ENABLED_DB"
        ;;
esac
exit 0
EOF
chmod +x "$fake_systemd/systemctl"
mkdir -p "$TMP/unit"
: > "$TMP/unit/sbx-agent.service"
: > "$fake_systemctl_calls"
rm -f "$systemd_enabled_db"
run PATH="$fake_systemd" CALLS="$fake_systemctl_calls" ENABLED_DB="$systemd_enabled_db" \
        SBX_TOKEN=tok FAKE_ACTIVE=1 \
    /bin/sh -c ". '$INSTALL_SH'; INIT_SYSTEM=systemd; start_agent" >/dev/null
body=$(cat "$fake_systemctl_calls")
contains "systemd 接入时 enable" "systemctl enable sbx-agent" "$body"
contains "systemd 接入时 restart" "systemctl restart sbx-agent" "$body"

# 起不来的时候必须说「没起来」,不能照样打一句成功。
: > "$fake_systemctl_calls"
rm -f "$systemd_enabled_db"
out=$(run PATH="$fake_systemd" CALLS="$fake_systemctl_calls" ENABLED_DB="$systemd_enabled_db" \
        SBX_TOKEN=tok FAKE_ACTIVE=0 \
    /bin/sh -c ". '$INSTALL_SH'; INIT_SYSTEM=systemd; start_agent")
contains "systemd 启动失败要当场报出来" "sbx-agent 没起来" "$out"
contains "失败提示带查日志的命令" "journalctl -u sbx-agent" "$out"
lacks "失败时不能同时说已启动" "sbx-agent 已启动" "$out"

: > "$fake_systemctl_calls"
run PATH="$fake_systemd" CALLS="$fake_systemctl_calls" ENABLED_DB="$systemd_enabled_db" FAKE_ACTIVE=1 \
    /bin/sh -c ". '$INSTALL_SH'; INIT_SYSTEM=systemd; start_agent" >/dev/null
contains "systemd 无 token 且 active 时 restart" "systemctl restart sbx-agent" "$(cat "$fake_systemctl_calls")"
: > "$fake_systemctl_calls"
run PATH="$fake_systemd" CALLS="$fake_systemctl_calls" ENABLED_DB="$systemd_enabled_db" \
        SBX_TOKEN=tok FAKE_ACTIVE=1 \
    /bin/sh -c ". '$INSTALL_SH'; INIT_SYSTEM=systemd; NO_RESTART=1; start_agent" >/dev/null
check "--no-restart 不调用 systemd" "" "$(cat "$fake_systemctl_calls")"
: > "$calls"
run PATH="$fake_service" CALLS="$calls" ENABLED_DB="$enabled_db" \
    /bin/sh -c ". '$INSTALL_SH'; restart_master_if_running"
check "主控仍只使用 systemd" "" "$(cat "$calls")"

out=$(run SBX_TOKEN=tok sh -c ". '$INSTALL_SH'; INIT_SYSTEM=none; start_agent")
contains "无 supervisor 明确提示自升级不会拉起" "自升级退出后不会自动拉起" "$out"

echo "── 写 agent.toml ───────────────────────────"

conf="$TMP/etc/agent.toml"

# 没给 token = 只升级二进制,绝不能碰人家已经在用的配置。
mkdir -p "$TMP/etc"
printf 'server = "wss://old/ws"\ntoken = "OLD"\n' > "$conf"
run sh -c ". '$INSTALL_SH'; write_agent_config" >/dev/null
contains "没有 SBX_TOKEN 时不动已有配置" 'token = "OLD"' "$(cat "$conf")"
rm -f "$conf" "$conf.bak"

out=$(run SBX_TOKEN=tok sh -c ". '$INSTALL_SH'; write_agent_config" || true)
contains "有 token 没 server 要报错" "必须给 SBX_SERVER" "$out"

out=$(run SBX_TOKEN=tok SBX_SERVER=wss://1.2.3.4:18443/ws \
        sh -c ". '$INSTALL_SH'; write_agent_config" || true)
contains "wss 缺指纹要报错" "SBX_FINGERPRINT" "$out"
[ -f "$conf" ] && no "报错时不该留下半截配置" "$conf 被创建了" || ok "报错时不该留下半截配置"

run SBX_TOKEN=tok SBX_SERVER=wss://1.2.3.4:18443/ws SBX_FINGERPRINT=sha256:aabb \
    sh -c ". '$INSTALL_SH'; write_agent_config" >/dev/null
body=$(cat "$conf")
contains "写入 server" 'server      = "wss://1.2.3.4:18443/ws"' "$body"
contains "写入 token" 'token       = "tok"' "$body"
contains "写入 fingerprint" 'fingerprint = "sha256:aabb"' "$body"
contains "写入默认 state_dir" 'state_dir   = "/var/lib/sbx-agent"' "$body"
lacks "给了指纹就不该有 insecure" "insecure" "$body"
# 里面是明文 token(§8.1),权限必须是 0600。
# Windows 上的 Git Bash 不做真正的 chmod,那里跳过这一条而不是给一条假的失败。
probe="$TMP/.mode-probe"
: > "$probe"
chmod 600 "$probe"
if [ "$(ls -l "$probe" | cut -c1-10)" = "-rw-------" ]; then
    mode=$(ls -l "$conf" | cut -c1-10)
    check "agent.toml 权限 0600" "-rw-------" "$mode"
else
    printf '  skip agent.toml 权限 0600(这个文件系统不支持 chmod,去 Linux 上跑)\n'
fi
rm -f "$probe"

# 轮换 token 之后重跑同一条命令:要覆盖,但旧的先备份。
run SBX_TOKEN=NEWTOK SBX_SERVER=ws://1.2.3.4:18443/ws SBX_INSECURE=1 \
    sh -c ". '$INSTALL_SH'; write_agent_config" >/dev/null
contains "带 token 重跑会覆盖(轮换后的正当用法)" 'token       = "NEWTOK"' "$(cat "$conf")"
contains "旧配置留了备份" 'token       = "tok"' "$(cat "$conf.bak")"
contains "明文 ws + SBX_INSECURE=1" "insecure    = true" "$(cat "$conf")"

rm -f "$conf" "$conf.bak"
run SBX_TOKEN=tok SBX_SERVER=ws://1.2.3.4:18443/ws SBX_STATE_DIR=/opt/sbx \
    sh -c ". '$INSTALL_SH'; write_agent_config" >/dev/null
contains "明文 ws 不强制要指纹" 'server      = "ws://1.2.3.4:18443/ws"' "$(cat "$conf")"
contains "SBX_STATE_DIR 生效" 'state_dir   = "/opt/sbx"' "$(cat "$conf")"

echo "── 版本比较 ────────────────────────────────"

out=$(run FORCE=0 sh -c ". '$INSTALL_SH'; FORCE=0; should_install '' 1.0.0 x && echo YES || echo NO")
contains "没装过 → 装" "YES" "$out"
out=$(run sh -c ". '$INSTALL_SH'; FORCE=0; should_install 1.0.0 1.0.0 x && echo YES || echo NO")
contains "已是最新 → 跳过" "NO" "$out"
out=$(run sh -c ". '$INSTALL_SH'; FORCE=1; should_install 1.0.0 1.0.0 x && echo YES || echo NO")
contains "--force → 重装" "YES" "$out"
out=$(run sh -c ". '$INSTALL_SH'; FORCE=0; should_install unknown 1.0.0 x && echo YES || echo NO")
contains "报不出版本 → 按升级处理" "YES" "$out"
out=$(run sh -c ". '$INSTALL_SH'; FORCE=0; should_install 0.9.0 1.0.0 x && echo YES || echo NO")
contains "版本不同 → 升级" "YES" "$out"

echo
printf '通过 %d,失败 %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
