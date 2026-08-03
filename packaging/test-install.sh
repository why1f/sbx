#!/bin/sh
# install.sh 的单元测试。
#
#   sh packaging/test-install.sh
#
# 为什么需要它:`install.sh` 是一条 `curl | bash` 的脚本,过去唯一的验证方式是
# **在一台真机上跑一遍**。v0.1.2 的回归(`sh -s -- master` 直接报错)正是这样溜出去的
# —— 当时我"验证"时拉的是 GitHub 上的旧脚本,等于用旧代码验证新改动。
#
# 这里靠 `SBX_SOURCE_ONLY=1` 把脚本当函数库 source 进来,再把 BIN_DIR / CONF_DIR /
# UNIT_DIR 指到临时目录上,于是**不碰系统、不联网**也能测到:参数解析、目标选择、
# 配置生成、以及那几条「填错了要拦下来」的规则。
#
# 网络相关的部分(下载、sha256 校验、systemd)测不到,那些仍然要在真机上过一遍。

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
        "$@" 2>&1
}

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
contains "管道调用要给 bash -s --" "bash -s --" "$out"

out=$(run sh -c ". '$INSTALL_SH'; usage")
contains "--help 说明 SBX_TOKEN 的用法" "SBX_TOKEN" "$out"
contains "--help 提到 TUI 会给出整条命令" "sbx tui" "$out"

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
