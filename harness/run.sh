#!/bin/bash
# End-to-end test of dormant against fake backends (harness/fakemc.py). Linux, bash, python3.
#
#   cargo build && harness/run.sh                 # tests target/debug/dormant
#   harness/run.sh path/to/dormant
#   BASE_PORT=40000 harness/run.sh                # default 35590; uses BASE_PORT .. BASE_PORT+3
#
# Everything listens on 127.0.0.1. The script only ever signals processes it started itself
# (by pid), and writes only below harness/work/. Takes about a minute. Exit status 0 = all passed.
set -u
H=$(cd "$(dirname "$0")" && pwd)
BIN=$(realpath "${1:-$H/../target/debug/dormant}")
[ -x "$BIN" ] || { echo "no dormant binary at $BIN (run cargo build first)"; exit 2; }
W=$H/work
rm -rf "$W"; mkdir -p "$W"
P=${BASE_PORT:-35590}; PA=$((P + 1)); PB=$((P + 2)); PF=$((P + 3))
DPID=
FAILED=0

stop_dormant() { [ -n "$DPID" ] && kill "$DPID" 2>/dev/null; wait "$DPID" 2>/dev/null; DPID=; }
cleanup() {
    stop_dormant
    for f in "$W"/*.pid; do
        [ -f "$f" ] || continue
        kill -CONT "$(cat "$f")" 2>/dev/null
        kill "$(cat "$f")" 2>/dev/null
    done
}
trap cleanup EXIT

ping() { python3 "$H/mcclient.py" ping "$P" "$1" 2>&1; }
login() { python3 "$H/mcclient.py" login "$P" "$2" "$1" 2>&1; }
alive() { [ -f "$W/$1.pid" ] && kill -0 "$(cat "$W/$1.pid")" 2>/dev/null; }
state() { awk '{print $3}' "/proc/$1/stat" 2>/dev/null; }

# expect "what" "substring" command...   (runs the command once)
expect() {
    local what=$1 want=$2; shift 2
    local got; got=$("$@")
    if [[ "$got" == *"$want"* ]]; then echo "ok    $what"; else echo "FAIL  $what: wanted '$want', got: $got"; FAILED=1; fi
}
# eventually "what" "substring" seconds command...   (retries the command every half second)
eventually() {
    local what=$1 want=$2 secs=$3; shift 3
    local got= i
    for ((i = 0; i < secs * 2; i++)); do
        got=$("$@")
        [[ "$got" == *"$want"* ]] && { echo "ok    $what"; return; }
        sleep 0.5
    done
    echo "FAIL  $what: wanted '$want' within $secs s, last got: $got"; FAILED=1
}
yesno() { if "$@"; then echo yes; else echo no; fi; }

server() { # name hosts port label
    cat <<T
[[server]]
name = "$1"
hosts = [$2]
backend = "127.0.0.1:$3"
start_cmd = "setsid nohup python3 $H/fakemc.py $3 $4 --pidfile $W/$1.pid >/dev/null 2>&1 </dev/null &"
stop_cmd = "kill \$(cat $W/$1.pid) && rm -f $W/$1.pid"
idle_secs = 6
min_up_secs = 2
poll_secs = 1
wake_secs_default = 5
wake_names = ["Steve"]
wake_limit_per_ip = 2
wake_limit_global = 0
kick_rate_limited = "slow down"
motd_sleeping = "$4 asleep (~{wake_secs} s)"
kick_starting = "$4 starting: {secs} of {wake_secs}"
cache_file = "$W/$1-status.json"
T
}

echo "##### 1. stop mode: two servers on one port, routing, wake, idle stop, rate limit"
{
    echo "listen = \"127.0.0.1:$P\""
    echo 'default = "alpha"'
    server alpha '"alpha.test", "a.test"' "$PA" ALPHA
    server beta '"beta.test"' "$PB" BETA
} > "$W/multi.toml"
expect "--check accepts the config" "2 server(s)" "$BIN" --check "$W/multi.toml"
"$BIN" "$W/multi.toml" > "$W/multi.log" 2>&1 &
DPID=$!
eventually "alpha.test shows alpha's sleeping MOTD" "ALPHA asleep (~5 s)" 5 ping alpha.test
expect "beta.test shows beta's sleeping MOTD" "BETA asleep" ping beta.test
expect "hostnames are case-insensitive, trailing dot ignored" "BETA asleep" ping BETA.TEST.
expect "an unknown hostname goes to the default server" "ALPHA asleep" ping nobody.test
expect "ping gets a pong while asleep" '"pong": true' ping beta.test
expect "a name outside wake_names cannot wake" "not allowed" login beta.test Mallory
expect "nothing was started by the refused login" "no" yesno alive beta
expect "an allowed name wakes beta and is told to retry" "BETA starting" login beta.test Steve
eventually "beta comes up and its real status is passed through" "REAL BETA" 15 ping beta.test
expect "alpha was not started" "no" yesno alive alpha
expect "alpha still shows as asleep" "ALPHA asleep" ping alpha.test
expect "a login while up reaches the backend (handshake replayed)" "backend BETA says hi: Steve" login beta.test Steve
# Wait on the process, not with pings: a ping while up is a proxied connection and counts as activity.
eventually "beta is stopped when idle" "no" 30 yesno alive beta
eventually "beta shows as asleep again" "BETA asleep" 5 ping beta.test
expect "the sleeping reply keeps the cached mod list" '"modinfo": true' ping beta.test
sleep 3 # let dormant notice that the backend is down (a login before that is queued, not started)
expect "the measured wake time was saved" "boots" cat "$W/beta-status.state.json"
expect "second wake from this address is allowed" "BETA starting" login beta.test Steve
eventually "beta comes up again" "REAL BETA" 15 ping beta.test
eventually "beta is stopped when idle again" "no" 30 yesno alive beta
sleep 3
expect "third wake from this address is rate limited" "slow down" login beta.test Steve
sleep 2
expect "the rate-limited login started nothing" "no" yesno alive beta
expect "list shows both servers" "beta" "$BIN" list "$W/multi.toml"
stop_dormant

echo "##### 2. no default server: unknown hostnames get a polite reply and wake nothing"
grep -v '^default' "$W/multi.toml" > "$W/nodefault.toml"
"$BIN" "$W/nodefault.toml" > "$W/nodefault.log" 2>&1 &
DPID=$!
eventually "unknown hostname gets the unknown-server MOTD" "Unknown server" 5 ping nobody.test
expect "unknown hostname login is refused" "Unknown server" login nobody.test Steve
expect "nothing was started" "no" yesno alive alpha
stop_dormant

echo "##### 3. freeze mode: SIGSTOP when idle, answer pings from the cache, resume and forward on login"
printf 'motd=x\nmax-tick-time=-1\n' > "$W/server.properties"
cat > "$W/freeze.toml" <<T
listen = "127.0.0.1:$P"
backend = "127.0.0.1:$PF"
start_cmd = "true"
stop_cmd = "kill \$(cat $W/frozen.pid)"
sleep_mode = "freeze"
pidfile = "$W/frozen.pid"
properties_file = "$W/server.properties"
idle_secs = 4
min_up_secs = 2
poll_secs = 1
motd_sleeping = "frozen asleep"
cache_file = "$W/frozen-status.json"
T
setsid python3 "$H/fakemc.py" "$PF" FROZEN --pidfile "$W/frozen.pid" > /dev/null 2>&1 < /dev/null &
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -f "$W/frozen.pid" ] && break; sleep 0.3; done
FP=$(cat "$W/frozen.pid")
"$BIN" "$W/freeze.toml" > "$W/freeze.log" 2>&1 &
DPID=$!
eventually "the running backend is adopted" "REAL FROZEN" 10 ping localhost
# Wait for dormant's own "frozen" log line, not just the process state: the process reaches state T a moment
# before dormant commits to the freeze, and a ping landing in that window makes it undo the freeze (by design).
eventually "dormant reports the backend frozen" "event=frozen" 20 cat "$W/freeze.log"
expect "the backend is frozen when idle (process state T)" "T" state "$FP"
expect "a ping while frozen is answered from the cache" "frozen asleep" ping localhost
expect "the ping did not resume the backend" "T" state "$FP"
expect "a login resumes the backend and is forwarded, no kick" "backend FROZEN says hi: Alex" login localhost Alex
expect "the backend is running again" "S" state "$FP"
stop_dormant

echo "##### 4. freeze is refused unless max-tick-time=-1 (falls back to stop_cmd)"
printf 'max-tick-time=60000\n' > "$W/server.properties"
"$BIN" "$W/freeze.toml" > "$W/freeze2.log" 2>&1 &
DPID=$!
eventually "the refusal is logged" "freeze_refused" 20 cat "$W/freeze2.log"
eventually "the backend was stopped instead" "no" 10 yesno alive frozen
stop_dormant

if [ "$FAILED" = 0 ]; then echo "ALL PASSED"; else echo "SOME CHECKS FAILED (logs in $W)"; fi
exit "$FAILED"
