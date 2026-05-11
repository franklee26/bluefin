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
# Usage: ./bench_run_with_timeout.sh [wallclock_seconds] [num_conns]
#   wallclock_seconds: hard kill grace (default 30)
#   num_conns:         how many clients to spawn (default 2; max = len(DEFAULT_PORTS) in client.rs, currently 5)
set -uo pipefail
cd "$(dirname "$0")"

WALLCLOCK="${1:-30}"
NUM_CONNS="${2:-2}"
LOG_DIR="bench_logs/$(date +%Y%m%d_%H%M%S_to)"
mkdir -p "$LOG_DIR"

pkill -9 -f ./target/release/server 2>/dev/null || true
pkill -9 -f ./target/release/client 2>/dev/null || true
sleep 0.3

export RUST_BACKTRACE=full

# Server now reads num-expected-conns from argv[1]; pass NUM_CONNS through.
./target/release/server "$NUM_CONNS" >"$LOG_DIR/server.log" 2>&1 &
SVR=$!
sleep 1.5

CLIENT_PIDS=()
for ((ix = 0; ix < NUM_CONNS; ix++)); do
    ./target/release/client --task "$ix" >"$LOG_DIR/c${ix}.log" 2>&1 &
    CLIENT_PIDS+=($!)
    # Stagger to dodge the handshake race (see live bottleneck #11). 100ms
    # is what the in-process client uses between its two tasks; matches.
    if (( ix + 1 < NUM_CONNS )); then
        sleep 0.1
    fi
done

# Wall-clock watchdog. SIGTERM first so client/server panic handlers get
# a chance to flush stderr; SIGKILL after a short grace.
(
    sleep "$WALLCLOCK"
    for p in "${CLIENT_PIDS[@]}" "$SVR"; do
        kill -0 "$p" 2>/dev/null && kill -SIGTERM "$p" 2>/dev/null
    done
    sleep 1
    for p in "${CLIENT_PIDS[@]}" "$SVR"; do
        kill -0 "$p" 2>/dev/null && kill -9 "$p" 2>/dev/null
    done
) &
WATCHDOG=$!

CLIENT_EXITS=()
for pid in "${CLIENT_PIDS[@]}"; do
    wait "$pid" 2>/dev/null
    CLIENT_EXITS+=($?)
done
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
for ec in "${CLIENT_EXITS[@]}"; do
    if [[ "$ec" -eq 143 || "$ec" -eq 137 ]]; then HUNG=1; fi
done
if (( HUNG )); then
    echo "WALLCLOCK_TIMEOUT after ${WALLCLOCK}s (clients=${CLIENT_EXITS[*]} svr=$SVR_EXIT)"
fi

# Surface the lines the loop greps for. Server may have printed FINAL
# even on SIGTERM if it had time; client may not have flushed stdout if
# it was hung in flush().await — that's fine, the parent loop tolerates
# missing rows.
CLIENT_LOGS=()
for ((ix = 0; ix < NUM_CONNS; ix++)); do
    CLIENT_LOGS+=("$LOG_DIR/c${ix}.log")
done
grep -hE "FINAL|sent [0-9]+ bytes|sendmsg_x writer" \
    "$LOG_DIR/server.log" "${CLIENT_LOGS[@]}" 2>/dev/null || true
