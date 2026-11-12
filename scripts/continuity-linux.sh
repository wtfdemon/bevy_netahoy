#!/usr/bin/env bash
# Headless Linux continuity harness: the example server + N windowless
# clients playing on autopilot, with the kernel's netem conditioning the link.
# Summarize runs with scripts/continuity-analyze.py <log dir>...
#
#   scripts/continuity-linux.sh <players> <seconds> <profile>
#
# Profiles (netem args applied PER DIRECTION, so a 90ms profile is ~180ms RTT):
#   clean          no qdisc at all (veth only)
#   lan            delay 5ms
#   transatlantic  delay 90ms 10ms distribution normal, loss 0.3%
#   bursty         delay 90ms, loss gemodel 2% 30% (Gilbert-Elliott bursts;
#                  per packet, so over TCP's sparse backoff sends a bad state
#                  stretches into seconds: overstates outages)
#   harsh          delay 150ms 30ms, loss 3%  (~300 ms RTT, frequent retransmit stalls)
#   clumsy15       delay 100ms, loss 15%  (as the Windows tool clumsy)
#   clumsy5        delay 100ms, loss 5%
#   brutal         delay 250ms 60ms, loss 8%, 5% reorder (~500 ms RTT; past the design point)
#   stall          delay 90ms, plus every STALL_PERIOD s the link goes 100%
#                  loss for STALL_HOLD_MS ms (both directions; logged as
#                  link_down / link_up in events.log): over TCP a
#                  hold, then the retransmits land together.
#
# Topology: the script re-execs itself in a private user + network + mount
# namespace (rootless), so parallel runs from different worktrees can't
# collide. Inside it, the server lives in network namespace $NS on the far
# end of a veth pair; clients stay in the outer private namespace. netem sits
# on the egress of BOTH veth ends, so commands (clients -> ns) and snapshots
# (ns -> clients) are each conditioned once.
#
#   private ns: $VETH_ROOT 10.200.0.1  <== veth ==>  $VETH_NS 10.200.0.2 : $NS
#               clientN --------------------------->  server :5000
#
# With no DISPLAY the run starts its own Xvfb (winit needs a display
# connection even with no window). Clients render nothing, so they run at
# their real 60 Hz; wgpu still gets an adapter from lavapipe.
#
# TRANSPORT=ws|wt (default ws, the baselines' transport) pins every client to
# WebSocket or WebTransport (TRANSPORT, read by bevy_netahoy::transport's
# client: no fallback), and is recorded in $LOG_DIR/transport. The
# server always opens both: WebTransport on UDP $WT_PORT (default 5001) and
# its offer on TCP $WT_HASH_PORT (default 5002), in the netns. A client that
# connected over anything else fails the run. TRANSPORT=auto leaves the choice
# to the clients (WebTransport first, WebSocket when it fails or UDP alone
# dies) and wants WebTransport for every session: nothing here blocks UDP
# alone, so a client on WebSocket, after a hook's whole-link outage say, chose
# wrong. A hook that blocks UDP alone reads the client logs itself.
#
# Clients run with HEADLESS=1 and AUTOPILOT=1 (examples/playground/client/
# autopilot.rs: a scripted player, so prediction and rollback really run);
# AUTOPILOT=0 leaves them idle. SERVER_ARGS / CLIENT_ARGS add CLI args; the
# rest of the environment (RUST_LOG, NETAHOY_ROLLBACK, ...) is inherited.
# SERVER_LOOP_HZ=<n> runs the server loop at n Hz (ScheduleRunner;
# examples/playground/server.rs); unset, the server runs one fixed tick per
# frame (one_tick_per_frame). It is passed to the server explicitly and
# recorded in $LOG_DIR/server_loop_hz.
# TICK_HZ=<n> runs the server and clients at n ticks a second (default 20);
# passed to both explicitly and recorded in $LOG_DIR/tick_hz.
# CARGO_PROFILE picks the cargo profile the binaries are built with and run
# from (default dev, in target/debug/). BUILD=1 builds both examples first.
# LOG_DIR picks where the logs go.
#
# HOOK=path/to/scenario.sh runs that script (sourced, in the background) once
# the clients are up, for scripted faults. It gets these helpers: `event msg`
# (timestamped line in $LOG_DIR/events.log), `link_down` / `link_up` (every
# packet dropped both directions, on top of the profile), `freeze N` / `thaw N`
# (SIGSTOP/SIGCONT client N: a hidden tab, a sleeping laptop),
# `stop_server` / `start_server`. The run still lasts <seconds>.
set -euo pipefail

PLAYERS="${1:-2}"
RUN_SECONDS="${2:-60}"
PROFILE="${3:-clean}"

[[ "$PLAYERS" =~ ^[0-9]+$ ]] && (( PLAYERS > 0 )) || { echo "players must be a positive integer"; exit 2; }
[[ "$RUN_SECONDS" =~ ^[0-9]+$ ]] || { echo "seconds must be an integer"; exit 2; }

cd "$(dirname "$0")/.."

CARGO_PROFILE="${CARGO_PROFILE:-dev}"
case "$CARGO_PROFILE" in
    dev) BIN_DIR="$PWD/target/debug/examples" ;;
    *)   BIN_DIR="$PWD/target/$CARGO_PROFILE/examples" ;;
esac
# Per checkout, so runs from different worktrees don't share logs.
RUN_ID="$(basename "$PWD")-$(pwd | md5sum | cut -c1-6)"
LOG_DIR="${LOG_DIR:-/tmp/continuity-logs-$RUN_ID}"
NS="${NS:-nahoy}"
VETH_ROOT="${VETH_ROOT:-nahoy-root}"
VETH_NS="${VETH_NS:-nahoy-srv}"
ROOT_IP="10.200.0.1"
SERVER_IP="10.200.0.2"
PORT="${PORT:-5000}"
WT_PORT="${WT_PORT:-5001}"
WT_HASH_PORT="${WT_HASH_PORT:-5002}"
TRANSPORT="${TRANSPORT:-ws}"
case "$TRANSPORT" in
    ws) WANT_TRANSPORT=websocket ;;
    wt | auto) WANT_TRANSPORT=webtransport ;;
    *) echo "unknown TRANSPORT '$TRANSPORT' (ws|wt|auto)"; exit 2 ;;
esac
STALL_PERIOD="${STALL_PERIOD:-10}"
STALL_HOLD_MS="${STALL_HOLD_MS:-400}"
# Software Vulkan so the client gets a wgpu adapter without a GPU.
export VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}"
export WGPU_BACKEND="${WGPU_BACKEND:-vulkan}"
# Assets load from the checkout, not from beside the binaries.
export CARGO_MANIFEST_DIR="$PWD"
export RUST_LOG="${RUST_LOG:-info}"

IP=/usr/sbin/ip
TC=/usr/sbin/tc
NSX="$IP netns exec $NS"

case "$PROFILE" in
    clean)         NETEM="" ;;
    lan)           NETEM="delay 5ms" ;;
    transatlantic) NETEM="delay 90ms 10ms distribution normal loss 0.3%" ;;
    bursty)        NETEM="delay 90ms loss gemodel 2% 30%" ;;
    harsh)         NETEM="delay 150ms 30ms distribution normal loss 3%" ;;
    clumsy15)      NETEM="delay 100ms loss 15%" ;;
    clumsy5)       NETEM="delay 100ms loss 5%" ;;
    brutal)        NETEM="delay 250ms 60ms distribution normal loss 8% reorder 5%" ;;
    stall)         NETEM="delay 90ms" ;;
    *) echo "unknown profile '$PROFILE' (clean|lan|transatlantic|bursty|harsh|clumsy15|clumsy5|brutal|stall)"; exit 2 ;;
esac

if [[ -z "${NAHOY_IN_NS:-}" ]]; then
    # Build outside the namespace: inside it, loopback is private and sccache
    # can't reach its server.
    if [[ "${BUILD:-0}" == "1" ]]; then
        cargo build --profile "$CARGO_PROFILE" --example playground_server --example playground_client
    fi
    for bin in playground_server playground_client; do
        [[ -x "$BIN_DIR/$bin" ]] || { echo "missing $BIN_DIR/$bin (BUILD=1)"; exit 1; }
    done
    NAHOY_IN_NS=1 exec unshare --user --map-root-user --net --mount --fork -- "$0" "$PLAYERS" "$RUN_SECONDS" "$PROFILE"
fi
# Private /run so `ip netns add` has a writable /run/netns.
mount -t tmpfs none /run
mkdir -p /run/netns

PIDS=()
STALL_PID=""
XVFB_PID=""
cleanup() {
    local rc=$?
    trap - EXIT INT TERM
    echo ""
    echo "Shutting down..."
    [[ -n "$STALL_PID" ]] && { kill "$STALL_PID" 2>/dev/null || true; }
    [[ -n "${HOOK_PID:-}" ]] && { pkill -P "$HOOK_PID" 2>/dev/null || true; kill "$HOOK_PID" 2>/dev/null || true; }
    # A frozen client ignores TERM until it's thawed.
    pkill -CONT -f "$BIN_DIR/playground_client" 2>/dev/null || true
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    # The server runs under ip-netns-exec; kill by namespace.
    $IP netns pids "$NS" 2>/dev/null | xargs -r kill -TERM 2>/dev/null || true
    sleep 1
    $IP netns pids "$NS" 2>/dev/null | xargs -r kill -KILL 2>/dev/null || true
    for pid in "${PIDS[@]}"; do kill -KILL "$pid" 2>/dev/null || true; done
    [[ -n "$XVFB_PID" ]] && { kill "$XVFB_PID" 2>/dev/null || true; }
    $IP netns del "$NS" 2>/dev/null || true
    wait 2>/dev/null || true
    echo "Done; logs in $LOG_DIR (scripts/continuity-analyze.py $LOG_DIR)"
    exit "$rc"
}
trap cleanup EXIT INT TERM

mkdir -p "$LOG_DIR"
rm -f "$LOG_DIR"/*.log "$LOG_DIR"/*.pid "$LOG_DIR"/netem-stats.txt
echo "$RUN_SECONDS" >"$LOG_DIR/seconds"
echo "${SERVER_LOOP_HZ:-one_tick_per_frame}" >"$LOG_DIR/server_loop_hz"
echo "${TICK_HZ:-default}" >"$LOG_DIR/tick_hz"
echo "$TRANSPORT" >"$LOG_DIR/transport"

# ---- display --------------------------------------------------------------
if [[ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]]; then
    DISPLAY_FILE="$(mktemp)"
    # Several runs starting at once can trip over each other compiling the
    # keymap; another try a moment later goes through.
    for attempt in 1 2 3; do
        Xvfb -displayfd 3 -screen 0 1280x720x24 -nolisten tcp 3>"$DISPLAY_FILE" >"$LOG_DIR/xvfb.log" 2>&1 &
        XVFB_PID="$!"
        for _ in $(seq 1 40); do [[ -s "$DISPLAY_FILE" ]] && break; sleep 0.1; done
        [[ -s "$DISPLAY_FILE" ]] && break
        kill "$XVFB_PID" 2>/dev/null || true
        sleep "$attempt"
    done
    [[ -s "$DISPLAY_FILE" ]] || { echo "Xvfb didn't start; see $LOG_DIR/xvfb.log"; exit 1; }
    export DISPLAY=":$(tr -d '[:space:]' <"$DISPLAY_FILE")"
    rm -f "$DISPLAY_FILE"
    echo "Xvfb on $DISPLAY"
fi

# ---- network --------------------------------------------------------------
$IP link set lo up
$IP netns add "$NS"
$IP link add "$VETH_ROOT" type veth peer name "$VETH_NS" netns "$NS"
$IP addr add "$ROOT_IP/24" dev "$VETH_ROOT"
$IP link set "$VETH_ROOT" up
$NSX $IP addr add "$SERVER_IP/24" dev "$VETH_NS"
$NSX $IP link set "$VETH_NS" up
$NSX $IP link set lo up
# TCP congestion control on both ends, CC (default cubic, Linux's and Windows'
# default; bbr reacts to loss and outages very differently). Per namespace.
CC="${CC:-cubic}"
sysctl -qw net.ipv4.tcp_congestion_control="$CC"
$NSX sysctl -qw net.ipv4.tcp_congestion_control="$CC"
echo "$CC" >"$LOG_DIR/cc"
if [[ -n "$NETEM" ]]; then
    # shellcheck disable=SC2086
    $TC qdisc add dev "$VETH_ROOT" root netem $NETEM
    # shellcheck disable=SC2086
    $NSX $TC qdisc add dev "$VETH_NS" root netem $NETEM
    echo "netem ($PROFILE): $NETEM on $VETH_ROOT (clients->server) and $NS:$VETH_NS (server->clients)"
else
    echo "no netem ($PROFILE)"
fi
if [[ "$PROFILE" == "stall" ]]; then
    (
        while sleep "$STALL_PERIOD"; do
            echo "$(date +%s.%N) link_down ${STALL_HOLD_MS}ms" >>"$LOG_DIR/events.log"
            # shellcheck disable=SC2086
            $TC qdisc change dev "$VETH_ROOT" root netem $NETEM loss 100%
            # shellcheck disable=SC2086
            $NSX $TC qdisc change dev "$VETH_NS" root netem $NETEM loss 100%
            sleep "$(awk "BEGIN{print $STALL_HOLD_MS/1000}")"
            # shellcheck disable=SC2086
            $TC qdisc change dev "$VETH_ROOT" root netem $NETEM
            # shellcheck disable=SC2086
            $NSX $TC qdisc change dev "$VETH_NS" root netem $NETEM
            echo "$(date +%s.%N) link_up" >>"$LOG_DIR/events.log"
        done
    ) &
    STALL_PID="$!"
    echo "stall loop: ${STALL_HOLD_MS}ms blackhole every ${STALL_PERIOD}s"
fi

# ---- processes ------------------------------------------------------------
start_server() {
    # shellcheck disable=SC2086
    $NSX env SERVER_BIND_ADDR="0.0.0.0:$PORT" WT_PORT="$WT_PORT" WT_HASH_ADDR="0.0.0.0:$WT_HASH_PORT" ${SERVER_LOOP_HZ:+SERVER_LOOP_HZ="$SERVER_LOOP_HZ"} ${TICK_HZ:+TICK_HZ="$TICK_HZ"} "$BIN_DIR/playground_server" ${SERVER_ARGS:-} >>"$LOG_DIR/server.log" 2>&1 &
    PIDS+=("$!")
    for _ in $(seq 1 80); do
        bash -c "exec 3<>/dev/tcp/$SERVER_IP/$PORT && exec 4<>/dev/tcp/$SERVER_IP/$WT_HASH_PORT" 2>/dev/null && return 0
        sleep 0.25
    done
    echo "server never listened on $SERVER_IP:$PORT and :$WT_HASH_PORT; see $LOG_DIR/server.log"
    return 1
}
stop_server() {
    $IP netns pids "$NS" 2>/dev/null | xargs -r kill -TERM 2>/dev/null || true
    sleep 1
    $IP netns pids "$NS" 2>/dev/null | xargs -r kill -KILL 2>/dev/null || true
}
event() { echo "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ) $*" | tee -a "$LOG_DIR/events.log"; }
client_pid() { cat "$LOG_DIR/client$1.pid"; }
freeze() { kill -STOP "$(client_pid "$1")"; }
thaw() { kill -CONT "$(client_pid "$1")"; }
# The link goes down by a filter that drops every packet leaving either end, in front of the
# profile's netem, which stays as it is: adding `loss 100%` to a profile that has its own loss is a
# duplicate argument tc refuses, and the link stayed up. Packets netem already holds still arrive,
# as on a real link.
DROP_ALL="egress pref 1 protocol all u32 match u32 0 0 action drop"
link_down() {
    $TC qdisc replace dev "$VETH_ROOT" clsact
    # shellcheck disable=SC2086
    $TC filter replace dev "$VETH_ROOT" $DROP_ALL
    $NSX $TC qdisc replace dev "$VETH_NS" clsact
    # shellcheck disable=SC2086
    $NSX $TC filter replace dev "$VETH_NS" $DROP_ALL
}
link_up() {
    $TC qdisc del dev "$VETH_ROOT" clsact
    $NSX $TC qdisc del dev "$VETH_NS" clsact
}
# Every client on the transport asked for and nothing else: a run that measured the other one
# would read as this one.
check_transports() {
    local bad=0 id used
    for id in $(seq 1 "$PLAYERS"); do
        used="$(sed 's/\x1b\[[0-9;]*m//g' "$LOG_DIR/client$id.log" | grep -o 'connected over [a-z]*' \
            | awk '{ print $3 }' | sort -u | paste -sd ' ' -)"
        if [[ "$used" != "$WANT_TRANSPORT" ]]; then
            echo "TRANSPORT CHECK FAILED: client $id connected over '${used:-nothing}', TRANSPORT=$TRANSPORT wants $WANT_TRANSPORT"
            bad=1
        fi
    done
    return "$bad"
}

echo "Starting server in netns $NS on ws://$SERVER_IP:$PORT and udp $WT_PORT (clients: $TRANSPORT)..."
start_server || exit 1

for id in $(seq 1 "$PLAYERS"); do
    echo "Starting headless client $id..."
    # shellcheck disable=SC2086
    env HEADLESS=1 AUTOPILOT="${AUTOPILOT:-1}" SERVER_URL="ws://$SERVER_IP:$PORT" \
        TRANSPORT="$TRANSPORT" WT_HASH_URL="http://$SERVER_IP:$WT_HASH_PORT/" ${TICK_HZ:+TICK_HZ="$TICK_HZ"} \
        "$BIN_DIR/playground_client" --headless ${CLIENT_ARGS:-} >"$LOG_DIR/client$id.log" 2>&1 &
    PIDS+=("$!")
    echo "$!" >"$LOG_DIR/client$id.pid"
    sleep 1
done

HOOK_PID=""
if [[ -n "${HOOK:-}" ]]; then
    HOOK="$(realpath "$HOOK")"
    echo "Running hook $HOOK"
    # shellcheck disable=SC1090
    ( source "$HOOK" ) &
    HOOK_PID="$!"
fi

echo "Running for ${RUN_SECONDS}s..."
sleep "$RUN_SECONDS"

# ---- proof the traffic crossed the veth -----------------------------------
{
    echo "# ss (client ns) connections to the server"
    ss -tni "dport = :$PORT" || true
    ss -uni "dport = :$WT_PORT" || true
    echo "# tc -s qdisc $VETH_ROOT (client ns, clients->server)"
    $TC -s qdisc show dev "$VETH_ROOT"
    echo "# tc -s qdisc $VETH_NS ($NS, server->clients)"
    $NSX $TC -s qdisc show dev "$VETH_NS"
} >"$LOG_DIR/netem-stats.txt"

check_transports || exit 1
