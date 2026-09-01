#!/usr/bin/env bash
# DESIGN.md §13.2 / §13.3 的端到端验证,**真跑**,不只编译。
#
# 在这个脚本存在之前,`e2e/` 在 CI 里只有 `go build` —— 也就是说这条链路
#
#     主控下发配置 → agent 装配出真能用的 inbound → 真流量 → tracker 记账
#                  → 上报 → 跨 agent 求和
#
# 没有任何自动化守着。它断掉的症状是最难发现的那种:**单元测试全绿、clippy 干净、
# 产品不工作**。golden 配置能证明「下发的 JSON 长这样」,证明不了「sing-box 吃了它
# 真起得来」;`spike/` 在一个进程里自己扮演两端,验的是 tracker 本身,不是这条缝。
#
# 断言用的字节数就是 DESIGN.md §13.2 / §13.3 里记下的那几个数 —— 那次是 2026-08-01
# 在 Oracle ARM 上手工跑出来的。于是这个脚本的作用不止「跑一遍」,而是**让 CI 反复
# 重新推导文档里声称的数字**。文档和现实分叉时,红的是 CI,不是用户。
#
# 用法:
#   e2e/run.sh              # 建 + 跑,退出码非 0 即失败
#   E2E_KEEP=1 e2e/run.sh   # 失败后保留工作目录与日志
#
# 依赖:Linux(agent 读 /proc/net/dev)、cargo、go、sqlite3。
set -euo pipefail

# ── 端口。固定值,不随机:随机端口在 CI 上撞了会变成偶发失败,而偶发失败比
#    固定失败难查得多。这些都在 39xxx,和常见服务不重叠。
MASTER_PORT=39443   # 主控 WS
SUB_PORT=39081      # 订阅(只为让 daemon 完整起来,不参与断言)
NODE1_PORT=39501    # tokyo 的 vless-ws
NODE2_PORT=39502    # osaka 的 vless-ws
ECHO_PORT=39600     # 驱动的 echo 服务端
SOCKS1_PORT=39701
SOCKS2_PORT=39702

# ── §13.2 的流量。**刻意不对称**:上下行相等的话「方向接反」这个 bug
#    在断言里看不出来(驱动的注释也写了同一件事)。
T_UP=262144;  T_DOWN=4194304   # tokyo
O_UP=131072;  O_DOWN=1048576   # osaka
# ── §13.3 重启后再推的那一段。
R_UP=100000;  R_DOWN=500000

# 驱动自己的 8 字节头(上行长度 + 下行长度)也算在上行里 —— §13.2 记的
# `262152 = 262144 + 8` 就是它。记账发生在**内层已解出的流**上,所以与
# VLESS/WS 的帧头无关。
HDR=8
# 允许的额外开销上限,取的是 `spike/main.go` 里同一个 `slack` 值。
#
# 为何不直接断言相等:§13.2 那次手工跑出来的确实是严格的 `+8`,
# 但把它钉成等式 = 把 CI 的红绿绑在 sing-box 内部分片/缓冲的实现细节上,
# 上游改一个字节就是一次不是 bug 的失败。区间下界才是真正要守的东西
# (丢字节、漏记账),上界守的是「重复计」—— 32 KiB 远小于任何一段流量的
# 一倍,所以照样拦得住。这也是 `spike/` 已经在用的口径。
SLACK=$((32 * 1024))

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
W="$(mktemp -d)"
DB="$W/sbx.db"
PIDS=()
FAILED=0

log()  { printf '\n\033[1;36m── %s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
die()  { printf '  \033[31m✗ %s\033[0m\n' "$*" >&2; FAILED=1; exit 1; }

cleanup() {
  local rc=$?
  # 先收进程,再决定日志留不留 —— 反过来的话 kill 的输出会插在日志中间。
  for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  if [ "$rc" -ne 0 ] || [ "$FAILED" -ne 0 ]; then
    printf '\n\033[31m===== 失败,下面是各进程日志 =====\033[0m\n' >&2
    for f in "$W"/*.log; do
      [ -f "$f" ] || continue
      printf '\n\033[33m--- %s(末 40 行)---\033[0m\n' "$(basename "$f")" >&2
      tail -40 "$f" >&2
    done
    printf '\n\033[33m--- agent_events ---\033[0m\n' >&2
    sqlite3 "$DB" "SELECT at, agent_id, kind, message FROM agent_events ORDER BY id" 2>/dev/null >&2 || true
    # 流量现状也要印:卡在「等上报齐」的超时上时,差多少比「超时了」有用得多。
    printf '\n\033[33m--- user_traffic ---\033[0m\n' >&2
    sqlite3 -header -column "$DB" \
      "SELECT node_id, cycle_up, cycle_down, total_up, total_down, counter_epoch
         FROM user_traffic ORDER BY node_id" 2>/dev/null >&2 || true
  fi
  if [ "${E2E_KEEP:-0}" = "1" ]; then
    printf '\n工作目录保留在 %s\n' "$W" >&2
  else
    rm -rf "$W"
  fi
}
trap cleanup EXIT

q() { sqlite3 "$DB" "$1"; }

# 轮询等一个条件成立。**不用固定 sleep**:固定 sleep 要么慢要么偶发失败,
# 而这里等的每一件事都有可观测的成立信号。
wait_for() {
  local what="$1" deadline="$2" cmd="$3" t0
  t0=$(date +%s)
  while ! eval "$cmd" >/dev/null 2>&1; do
    if [ $(( $(date +%s) - t0 )) -ge "$deadline" ]; then
      die "等「$what」超过 ${deadline}s 还没成立"
    fi
    sleep 0.3
  done
  ok "$what"
}

expect_eq() {
  local what="$1" got="$2" want="$3" hint="${4:-}"
  if [ "$got" != "$want" ]; then
    printf '  \033[31m✗ %s:实得 %s,应为 %s\033[0m\n' "$what" "$got" "$want" >&2
    [ -n "$hint" ] && printf '    \033[33m%s\033[0m\n' "$hint" >&2
    FAILED=1
    return 1
  fi
  ok "$what = $got"
}

# 字节数落在 [want, want+SLACK] 里 —— 口径与 `spike/main.go` 一致。
expect_bytes() {
  local what="$1" got="$2" want="$3" hint="${4:-}"
  if [ "$got" -lt "$want" ]; then
    printf '  \033[31m✗ %s:实得 %s,少于下界 %s —— 流量丢了\033[0m\n' "$what" "$got" "$want" >&2
    [ -n "$hint" ] && printf '    \033[33m%s\033[0m\n' "$hint" >&2
    FAILED=1
    return 1
  fi
  if [ "$got" -gt "$((want + SLACK))" ]; then
    printf '  \033[31m✗ %s:实得 %s,超过上界 %s —— 多记了\033[0m\n' \
      "$what" "$got" "$((want + SLACK))" >&2
    [ -n "$hint" ] && printf '    \033[33m%s\033[0m\n' "$hint" >&2
    FAILED=1
    return 1
  fi
  ok "$what = $got(下界 $want)"
}

# ─────────────────────────────────────────────────────────────────────
log "1/8 构建三个产物"
cd "$ROOT"
cargo build --locked --bin sbx 2>&1 | tail -2
SBX_BIN="$ROOT/target/debug/sbx"
( cd agent && go build -tags with_quic,with_utls -o "$W/sbx-agent" ./cmd/sbx-agent )
( cd e2e   && go build -tags with_quic,with_utls -o "$W/e2e-driver" . )
ok "sbx / sbx-agent / e2e-driver"

# ─────────────────────────────────────────────────────────────────────
log "2/8 主控配置与库"
cat > "$W/config.toml" <<EOF
[db]
path = "$DB"

[cluster]
listen    = "127.0.0.1:$MASTER_PORT"
tls       = true
cert_path = "$W/cert.pem"
key_path  = "$W/key.pem"
# 心跳与上报都压到 2s:默认 10/30 会让这个脚本大部分时间在等。
# 压短了不改变任何被验证的语义 —— 上报间隔只决定「多久看到」,不决定数字。
heartbeat_secs       = 2
report_interval_secs = 2

[subscription]
listen  = "127.0.0.1:$SUB_PORT"
enabled = true

[telegram]
enabled = false
EOF
SBX="$SBX_BIN --config $W/config.toml"
$SBX init-db >/dev/null
# fingerprint 要在 agent-add 之前:证书是它顺手生成的,没有证书的话
# agent-add 打出来的那条命令里 SBX_FINGERPRINT 是个占位提示而不是指纹。
FP="$($SBX fingerprint 2>/dev/null | grep -o 'sha256:[0-9a-f]*')"
[ -n "$FP" ] || die "取不到证书指纹"
ok "schema $(q 'PRAGMA user_version') / 指纹 ${FP:0:20}…"

# ─────────────────────────────────────────────────────────────────────
log "3/8 两台 agent、两个节点、一个用户"
# 明文 token 只在 agent-add 的输出里出现一次(库里只有 hash),所以在这里抠出来。
TOK1="$($SBX agent-add tokyo 2>/dev/null | grep -o "SBX_TOKEN='[^']*'" | cut -d"'" -f2)"
TOK2="$($SBX agent-add osaka 2>/dev/null | grep -o "SBX_TOKEN='[^']*'" | cut -d"'" -f2)"
[ -n "$TOK1" ] && [ -n "$TOK2" ] || die "取不到 agent token"
A1="$(q "SELECT id FROM agents WHERE name='tokyo'")"
A2="$(q "SELECT id FROM agents WHERE name='osaka'")"

$SBX node-add "$A1" tokyo-ws "$NODE1_PORT" --protocol vless-ws --path /e2e >/dev/null
$SBX node-add "$A2" osaka-ws "$NODE2_PORT" --protocol vless-ws --path /e2e >/dev/null
N1="$(q "SELECT id FROM nodes WHERE tag='tokyo-ws'")"
N2="$(q "SELECT id FROM nodes WHERE tag='osaka-ws'")"

$SBX user-add alice --quota-gb 0 >/dev/null
$SBX user-assign alice "$N1" >/dev/null
$SBX user-assign alice "$N2" >/dev/null
# UUID 直接读库:此刻 agent 还没上报过公网地址,`user-sub --links` 导不出链接。
UUID="$(q "SELECT uuid FROM users WHERE name='alice'")"
[ -n "$UUID" ] || die "取不到 alice 的 uuid"
ok "agent #$A1/#$A2、节点 #$N1/#$N2、alice=${UUID:0:8}…"

# ─────────────────────────────────────────────────────────────────────
log "4/8 起 daemon 与两台 agent"
$SBX daemon > "$W/daemon.log" 2>&1 &
PIDS+=($!)
wait_for "daemon 在听 :$MASTER_PORT" 30 "ss -ltn | grep -q ':$MASTER_PORT '"

start_agent() {
  local name="$1" tok="$2"
  mkdir -p "$W/$name"
  cat > "$W/$name.toml" <<EOF
server      = "wss://127.0.0.1:$MASTER_PORT/ws"
token       = "$tok"
fingerprint = "$FP"
insecure    = false
state_dir   = "$W/$name"
EOF
  chmod 600 "$W/$name.toml"
  "$W/sbx-agent" "$W/$name.toml" > "$W/$name.log" 2>&1 &
  PIDS+=($!)
}
start_agent tokyo "$TOK1"
start_agent osaka "$TOK2"

# 两台都上线 = 库里两行 last_seen 都不空。
wait_for "两台 agent 都握手了" 60 \
  "[ \"\$(sqlite3 '$DB' 'SELECT COUNT(*) FROM agents WHERE last_seen IS NOT NULL')\" = 2 ]"
# **真正的就绪信号是 sing-box 端口在听**,不是「agent 连上了」——
# 配置下发、装配、启动都在握手之后,这一步才是「主控下发的配置真能跑」。
wait_for "tokyo 的 sing-box 在听 :$NODE1_PORT" 60 "ss -ltn | grep -q ':$NODE1_PORT '"
wait_for "osaka 的 sing-box 在听 :$NODE2_PORT" 60 "ss -ltn | grep -q ':$NODE2_PORT '"

# ─────────────────────────────────────────────────────────────────────
log "5/8 推真流量(§13.2)"
"$W/e2e-driver" -serve-echo -echo "$ECHO_PORT" > "$W/echo.log" 2>&1 &
PIDS+=($!)
wait_for "echo 服务器在听 :$ECHO_PORT" 15 "ss -ltn | grep -q ':$ECHO_PORT '"

drive() { # 目标端口 socks端口 上行 下行
  "$W/e2e-driver" -uuid "$UUID" -path /e2e -echo "$ECHO_PORT" \
    -target "$1" -socks "$2" -up "$3" -down "$4" 2>&1 | sed 's/^/    /'
}
drive "$NODE1_PORT" "$SOCKS1_PORT" "$T_UP" "$T_DOWN"
drive "$NODE2_PORT" "$SOCKS2_PORT" "$O_UP" "$O_DOWN"

# ─────────────────────────────────────────────────────────────────────
log "6/8 核对跨 agent 求和"
node_traffic() { q "SELECT COALESCE(cycle_up,0)||' '||COALESCE(cycle_down,0)
                      FROM user_traffic WHERE node_id=$1"; }
# 等的不是「有流量」而是「流量足额」。agent 每 2s 上报一次,所以完全可能
# 捕到一份**传输中途的部分值** —— `cycle_down > 0` 对部分值也成立,于是下面的
# 下界断言会偷偷变成一个看天时的偶发失败。直接等到位。
#
# 只等下行就够:echo 服务端是**读完全部上行才开始发下行**的,
# 下行到位蕴含上行已经到位。
wait_for "两个节点的流量都上报齐了" 60 \
  "[ \"\$(sqlite3 '$DB' 'SELECT COUNT(*) FROM user_traffic
       WHERE (node_id=$N1 AND cycle_down >= $T_DOWN)
          OR (node_id=$N2 AND cycle_down >= $O_DOWN)')\" = 2 ]"

read -r t_up t_down <<<"$(node_traffic "$N1")"
read -r o_up o_down <<<"$(node_traffic "$N2")"
read -r v_up v_down <<<"$(q "SELECT cycle_up||' '||cycle_down
                              FROM user_traffic_total WHERE name='alice'")"

MOVED="记账层动了或驱动改了。核实后同步更新 DESIGN.md §13.2 与本脚本的常量。"
expect_bytes "tokyo 上行" "$t_up"   "$((T_UP + HDR))" "$MOVED" || true
expect_bytes "tokyo 下行" "$t_down" "$T_DOWN"         "$MOVED" || true
expect_bytes "osaka 上行" "$o_up"   "$((O_UP + HDR))" "$MOVED" || true
expect_bytes "osaka 下行" "$o_down" "$O_DOWN"         "$MOVED" || true

# 这两条是 §13.1 的核心契约。它们用 `expect_eq` 而不是区间:视图就是一条
# `SUM()`,和 sing-box 版本、开销、分片全部无关 —— 它永远该是个恒等式,
# 给它留容差等于不验。
expect_eq "视图上行 = 两节点之和" "$v_up"   "$((t_up + o_up))"     || true
expect_eq "视图下行 = 两节点之和" "$v_down" "$((t_down + o_down))" || true

# 方向没接反:驱动刻意让下行远大于上行(tokyo 16:1、osaka 8:1),
# 所以 4 倍这个阀值对两边都安全 —— 口径同 `spike/main.go`。
[ "$t_down" -gt "$((t_up * 4))" ] || die "tokyo 的上下行反了(up=$t_up down=$t_down)"
[ "$o_down" -gt "$((o_up * 4))" ] || die "osaka 的上下行反了(up=$o_up down=$o_down)"
ok "上下行方向正确"
[ "$FAILED" -eq 0 ] || die "§13.2 断言失败"

# ─────────────────────────────────────────────────────────────────────
log "7/8 断连恢复(§13.3):kill -9 tokyo 再拉起"
before_up="$(q "SELECT total_up   FROM user_traffic WHERE node_id=$N1")"
before_down="$(q "SELECT total_down FROM user_traffic WHERE node_id=$N1")"
epoch_before="$(q "SELECT counter_epoch FROM user_traffic WHERE node_id=$N1")"
resets_before="$(q "SELECT COUNT(*) FROM agent_events WHERE kind='counter_reset'")"

# kill -9:不给它清理的机会。优雅退出走的是另一条路,验不到 epoch 变更。
pkill -9 -f "$W/tokyo.toml" || die "杀不掉 tokyo agent"
wait_for "tokyo 的 sing-box 端口已释放" 30 "! ss -ltn | grep -q ':$NODE1_PORT '"
# 日志重开(`>` 而不是 `>>`),下面的冷启动断言看的就是这一次的日志。
start_agent tokyo "$TOK1"
wait_for "tokyo 重新装配好 :$NODE1_PORT" 60 "ss -ltn | grep -q ':$NODE1_PORT '"

# §13.3 第一件事:**冷启动不等握手**。agent 先按 last-applied.json 把 box 拉起来,
# 再去连主控 —— 这正是「主控离线时重启被控机,代理服务照样能用」的依据(§1.2 / §4.1)。
# 反了的话平时看不出来:主控在线时两种顺序都能把 box 起来。
wait_for "tokyo 日志里两条启动记录都到了" 30 \
  "grep -q '已按 last-applied.json 启动 box' '$W/tokyo.log' && grep -q '已连上主控' '$W/tokyo.log'"
boxline=$(grep -n '已按 last-applied.json 启动 box' "$W/tokyo.log" | head -1 | cut -d: -f1)
connline=$(grep -n '已连上主控' "$W/tokyo.log" | head -1 | cut -d: -f1)
[ "$boxline" -lt "$connline" ] \
  || die "冷启动顺序反了:第 $connline 行先连上主控,第 $boxline 行才拉起 box"
ok "冷启动先拉 box(行 $boxline)后连主控(行 $connline)"

drive "$NODE1_PORT" "$SOCKS1_PORT" "$R_UP" "$R_DOWN"

want_up=$((before_up + R_UP + HDR))
want_down=$((before_down + R_DOWN))
wait_for "重启后的那一段也上报了" 60 \
  "[ \"\$(sqlite3 '$DB' 'SELECT total_down FROM user_traffic WHERE node_id=$N1')\" -ge $want_down ]"

after_up="$(q "SELECT total_up   FROM user_traffic WHERE node_id=$N1")"
after_down="$(q "SELECT total_down FROM user_traffic WHERE node_id=$N1")"
epoch_after="$(q "SELECT counter_epoch FROM user_traffic WHERE node_id=$N1")"
resets_after="$(q "SELECT COUNT(*) FROM agent_events WHERE kind='counter_reset'")"

# 不丢、不重复:正好是两段之和。做差(而不是当全量)会得到负数,
# 当全量但漏掉重启前那段则会少 262152/4194304,重复计会多出一整段。
# 三种错法全都落在 [want, want+SLACK] 之外。
expect_bytes "重启后累计上行" "$after_up"   "$want_up"   "既不能丢也不能重复计" || true
expect_bytes "重启后累计下行" "$after_down" "$want_down" "既不能丢也不能重复计" || true

[ "$epoch_after" != "$epoch_before" ] \
  || die "counter_epoch 没变($epoch_after)—— agent 进程重启必须换 epoch"
ok "counter_epoch 变了:${epoch_before:0:8}… → ${epoch_after:0:8}…"

[ "$resets_after" -gt "$resets_before" ] \
  || die "agent_events 里没有新的 counter_reset(§5.4 要求写审计)"
ok "写了 counter_reset 审计事件($resets_before → $resets_after)"
[ "$FAILED" -eq 0 ] || die "§13.3 断言失败"

# ─────────────────────────────────────────────────────────────────────
log "8/8 结论"
printf '  tokyo  %s / %s\n  osaka  %s / %s\n  视图   %s / %s\n' \
  "$t_up" "$t_down" "$o_up" "$o_down" "$v_up" "$v_down"
printf '  重启后 tokyo 累计 %s / %s\n' "$after_up" "$after_down"
printf '\n\033[1;32m✓ §13.2 与 §13.3 全部通过\033[0m\n'
