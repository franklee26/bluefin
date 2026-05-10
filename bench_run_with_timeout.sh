#!/usr/bin/env bash
#
# Single-shot bench runner with a hard wall-clock timeout. Used by the
# 10-iteration loop when neither `gtimeout` nor `timeout` is installed
# (the existing `bench_two_process.sh` only enforces a per-client cap when
# one of those is on PATH; on a stock macOS install they aren't).
#
# Behaviour:
#   - Starts server + 2 clients, captures logs to bench_logs/<ts>/
#   - Hard-kills the whole group after $WALLCLOCK seconds (default 30)
#   - On timeout, flushes pending stderr from clients (panic backtraces
#     survive SIGTERM if the client uses eprintln; SIGKILL loses them).
#   - Always prints server FINAL lines and per-client `sent ...` lines so
#     the parent loop can grep them.
#
# Usage: ./bench_run_with_timeout.sh [wallclock_seconds]
set -uo pipefail
cd "$(dirname "$0")"

WALLCLOCK="${1:-30}"
LOG_DIR="bench_logs/$(date +%Y%m%d_%H%M%S_to)"
mkdir -p "$LOG_DIR"

pkill -9 -f ./target/release/server 2>/dev/null || true
pkill -9 -f ./target/release/client 2>/dev/null || true
sleep 0.3

export RUST_BACKTRACE=full

./target/release/server >"$LOG_DIR/server.log" 2>&1 &
SVR=$!
sleep 1.5

./target/release/client --task 0 >"$LOG_DIR/c0.log" 2>&1 &
C0=$!
sleep 0.1
./target/release/client --task 1 >"$LOG_DIR/c1.log" 2>&1 &
C1=$!

# Wall-clock watchdog. SIGTERM first so client/server panic handlers get
# a chance to flush stderr; SIGKILL after a short grace.
(
    sleep "$WALLCLOCK"
    for p in "$C0" "$C1" "$SVR"; do
        kill -0 "$p" 2>/dev/null && kill -SIGTERM "$p" 2>/dev/null
    done
    sleep 1
    for p in "$C0" "$C1" "$SVR"; do
        kill -0 "$p" 2>/dev/null && kill -9 "$p" 2>/dev/null
    done
) &
WATCHDOG=$!

wait "$C0" 2>/dev/null
C0_EXIT=$?
wait "$C1" 2>/dev/null
C1_EXIT=$?
# Server prints `FINAL` after 2 seconds of recv-idle on each connection.
# Give it up to 6s to do that on its own; only SIGTERM if it's still
# running after the watchdog grace.
for _ in $(seq 1 60); do
    if ! kill -0 "$SVR" 2>/dev/null; then break; fi
    sleep 0.1
done
kill -SIGTERM "$SVR" 2>/dev/null || true
wait "$SVR" 2>/dev/null
SVR_EXIT=$?
kill -9 "$WATCHDOG" 2>/dev/null || true
wait "$WATCHDOG" 2>/dev/null || true

# Tag for the parent loop's stats parser.
HUNG=0
if [[ "$C0_EXIT" -eq 143 || "$C0_EXIT" -eq 137 || "$C1_EXIT" -eq 143 || "$C1_EXIT" -eq 137 ]]; then
    HUNG=1
fi
if (( HUNG )); then
    echo "WALLCLOCK_TIMEOUT after ${WALLCLOCK}s (c0=$C0_EXIT c1=$C1_EXIT svr=$SVR_EXIT)"
fi

# Surface the lines the loop greps for. Server may have printed FINAL
# even on SIGTERM if it had time; client may not have flushed stdout if
# it was hung in flush().await — that's fine, the parent loop tolerates
# missing rows.
grep -hE "FINAL|sent [0-9]+ bytes|sendmsg_x writer" \
    "$LOG_DIR/server.log" "$LOG_DIR/c0.log" "$LOG_DIR/c1.log" 2>/dev/null || true
