#!/usr/bin/env bash
# Two-process throughput benchmark for Bluefin.
#
# Spawns one `server` and N `client --task <ix>` processes (default N=2).
# Each client gets its own Tokio runtime, so they cannot starve each other
# inside a shared scheduler. This is the recommended way to measure peak
# multi-connection throughput.
#
# Usage:
#   ./bench_two_process.sh                  # 2 connections (default)
#   ./bench_two_process.sh -n 5             # 5 connections
#   ./bench_two_process.sh -n 2 -t 60       # 2 connections, 60s timeout
#   ./bench_two_process.sh --stagger 1.0    # 1s gap between spawning clients
#   ./bench_two_process.sh --retry 3        # auto-retry up to 3 times if a handshake fails
#   ./bench_two_process.sh --skip-build     # reuse existing release binaries
#
# The handshake-race retry is needed because the Bluefin server has a known
# race (see docs/archive/BINARY_RACE_CONDITIONS.md) where a client hello can
# arrive before the server's next accept() slot is ready. The 500ms default
# stagger avoids it most of the time on loopback; --retry catches the rest.
#
# Output goes to ./bench_logs/<timestamp>/{server,client_<ix>}.log
# A short summary (FINAL lines + recent inst lines) is printed at the end.

set -euo pipefail

# --- defaults --------------------------------------------------------------
NUM_CONNS=2
CLIENT_TIMEOUT=120          # hard cap on how long any client may run
SETTLE_AFTER_BUILD=2        # seconds to wait for server bind() to complete
STAGGER=0.5                 # seconds between spawning successive clients
RETRY=2                     # retry attempts on handshake failure (0 = no retry)
SKIP_BUILD=0
PORTS=(1320 1322 1323 1324 1325)   # must match DEFAULT_PORTS in client.rs

# --- arg parsing -----------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        -n|--num-connections)
            NUM_CONNS="$2"
            shift 2
            ;;
        -t|--timeout)
            CLIENT_TIMEOUT="$2"
            shift 2
            ;;
        --stagger)
            STAGGER="$2"
            shift 2
            ;;
        --retry)
            RETRY="$2"
            shift 2
            ;;
        --skip-build)
            SKIP_BUILD=1
            shift
            ;;
        -h|--help)
            sed -n '2,24p' "$0"
            exit 0
            ;;
        *)
            echo "unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

if (( NUM_CONNS < 1 || NUM_CONNS > ${#PORTS[@]} )); then
    echo "error: -n must be between 1 and ${#PORTS[@]} (matches DEFAULT_PORTS in client.rs)" >&2
    exit 2
fi

# --- locations -------------------------------------------------------------
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

SERVER_BIN="./target/release/server"
CLIENT_BIN="./target/release/client"

LOG_DIR="bench_logs/$(date +%Y%m%d_%H%M%S)"
mkdir -p "$LOG_DIR"
# Single-line marker so external drivers (e.g. bench_ci.sh) can pick up the
# per-invocation log directory without racing against `ls -t bench_logs/`.
echo "[log-dir] $LOG_DIR"

# --- macOS prefers `gtimeout` (coreutils); fall back to `timeout` ----------
if command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN="gtimeout"
elif command -v timeout >/dev/null 2>&1; then
    TIMEOUT_BIN="timeout"
else
    TIMEOUT_BIN=""   # no timeout available; rely on client's own exit
fi

# --- pid tracking + cleanup ------------------------------------------------
SERVER_PID=""
CLIENT_PIDS=()

cleanup() {
    local exit_code=$?
    echo
    echo "[cleanup] stopping background processes..."
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
    fi
    for pid in "${CLIENT_PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    # give them a moment, then SIGKILL anything still alive
    sleep 0.5
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -9 "$SERVER_PID" 2>/dev/null || true
    fi
    for pid in "${CLIENT_PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null || true
        fi
    done
    exit "$exit_code"
}
trap cleanup EXIT INT TERM

# --- 1. build (once, outside the retry loop) -------------------------------
if (( SKIP_BUILD == 0 )); then
    echo "[build] release binaries..."
    cargo build --release --bin server --bin client \
        2>&1 | grep -E "(Compiling|Finished|error)" || true
else
    echo "[build] --skip-build set; reusing existing binaries"
fi

if [[ ! -x "$SERVER_BIN" ]]; then
    echo "error: $SERVER_BIN not found or not executable" >&2
    exit 1
fi
if [[ ! -x "$CLIENT_BIN" ]]; then
    echo "error: $CLIENT_BIN not found or not executable" >&2
    exit 1
fi

export RUST_BACKTRACE=1

# Run one full attempt (server + N clients + summary). Echoes "ATTEMPT_OK" or
# "ATTEMPT_HANDSHAKE_FAIL" on its last line so the caller can decide to retry.
run_attempt() {
    local attempt_ix="$1"
    local attempt_log_dir="$2"

    SERVER_PID=""
    CLIENT_PIDS=()

    # --- 2. kill any stale benchmark processes -----------------------------
    echo "[attempt $attempt_ix | step 1/4] killing stale server/client processes..."
    pkill -9 -f "$SERVER_BIN" 2>/dev/null || true
    pkill -9 -f "$CLIENT_BIN" 2>/dev/null || true
    sleep 0.5

    # --- 3. start server ---------------------------------------------------
    local server_log="$attempt_log_dir/server.log"
    echo "[attempt $attempt_ix | step 2/4] starting server -> $server_log"
    # Pass NUM_CONNS through so the server accepts exactly that many handshakes
    # before starting recv loops. Without this it hardcodes 2 and any
    # `-n` other than 2 hangs (server waits forever on a 3rd accept, or
    # exits early before the 3rd client finishes its handshake).
    "$SERVER_BIN" "$NUM_CONNS" >"$server_log" 2>&1 &
    SERVER_PID=$!
    echo "       server pid: $SERVER_PID"

    sleep "$SETTLE_AFTER_BUILD"
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "error: server died during startup. Last 20 lines:" >&2
        tail -20 "$server_log" >&2
        return 2
    fi

    # --- 4. spawn N clients, one per task ix -------------------------------
    echo "[attempt $attempt_ix | step 3/4] spawning $NUM_CONNS client process(es) (stagger=${STAGGER}s)..."
    for ((ix = 0; ix < NUM_CONNS; ix++)); do
        local client_log="$attempt_log_dir/client_${ix}.log"
        if [[ -n "$TIMEOUT_BIN" ]]; then
            "$TIMEOUT_BIN" "$CLIENT_TIMEOUT" "$CLIENT_BIN" --task "$ix" \
                >"$client_log" 2>&1 &
        else
            "$CLIENT_BIN" --task "$ix" >"$client_log" 2>&1 &
        fi
        CLIENT_PIDS+=($!)
        echo "       client #$ix pid: ${CLIENT_PIDS[$ix]} -> $client_log"
        # Stagger the next client so the server's accept() slot is ready
        # before its hello arrives. See docs/archive/BINARY_RACE_CONDITIONS.md
        # for the underlying protocol race this papers over.
        if (( ix + 1 < NUM_CONNS )); then
            sleep "$STAGGER"
        fi
    done

    # --- 5. wait for clients, then drain server ----------------------------
    echo "[attempt $attempt_ix | step 4/4] waiting for clients to finish..."
    CLIENT_EXIT_CODES=()
    for pid in "${CLIENT_PIDS[@]}"; do
        set +e
        wait "$pid"
        local code=$?
        set -e
        CLIENT_EXIT_CODES+=("$code")
    done

    local handshake_failed=0
    for ((ix = 0; ix < NUM_CONNS; ix++)); do
        local code="${CLIENT_EXIT_CODES[$ix]}"
        local note=""
        if [[ "$code" -eq 124 ]]; then
            note=" (hit ${CLIENT_TIMEOUT}s timeout)"
        elif [[ "$code" -ne 0 ]]; then
            note=" (NON-ZERO -- check log)"
            # Detect the known handshake race so the caller can retry.
            if grep -q "Failed to read from handshake connection buffer" \
                "$attempt_log_dir/client_${ix}.log"; then
                handshake_failed=1
                note="$note  [HANDSHAKE RACE]"
            fi
        fi
        echo "       client #$ix exit=$code$note"
    done

    # Server exits naturally only after `RECV_IDLE_TIMEOUT_SECS` of no
    # incoming bytes. Default is 2 s; CI bumps it to 10 s. Our grace must
    # always be at least that long + a small safety margin, otherwise we
    # SIGTERM the server before it prints its FINAL lines and the bench
    # parser sees zero measurements.
    local recv_idle_secs="${BLUEFIN_RECV_IDLE_TIMEOUT_SECS:-2}"
    if ! [[ "$recv_idle_secs" =~ ^[0-9]+$ ]] || (( recv_idle_secs <= 0 )); then
        recv_idle_secs=2
    fi
    local grace_secs=$(( recv_idle_secs + 3 ))
    local grace_iters=$(( grace_secs * 10 ))
    echo "       waiting up to ${grace_secs}s for server to print FINAL lines and exit..."
    for _ in $(seq 1 "$grace_iters"); do
        if ! kill -0 "$SERVER_PID" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done
    if kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "       server still running after grace; sending SIGTERM"
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    SERVER_PID=""

    if (( handshake_failed )); then
        return 3
    fi
    return 0
}

# Print summary for one attempt's log directory.
print_summary() {
    local d="$1"
    local server_log="$d/server.log"

    echo
    echo "============================================================"
    echo " summary  (logs in $d)"
    echo "============================================================"

    local server_lines
    server_lines=$(wc -l <"$server_log" | tr -d ' ')
    echo
    echo "--- server log: $server_lines line(s) ---"
    if [[ "$server_lines" -eq 0 ]]; then
        echo "  WARNING: server log is empty."
        echo "  Most likely cause: server is stuck in server.accept().await waiting"
        echo "  for a connection that never arrived (a client failed to handshake)."
        echo "  Check the client logs below for panics or missing summary lines."
    fi

    echo
    echo "--- server FINAL lines ---"
    grep -E "FINAL|idle for" "$server_log" || echo "  (none found)"

    echo
    echo "--- server last 5 inst lines per connection ---"
    for ((ix = 0; ix < NUM_CONNS; ix++)); do
        echo "  conn #$ix:"
        local matches
        matches=$(grep -E "^${ix} " "$server_log" | tail -5 || true)
        if [[ -z "$matches" ]]; then
            echo "    (no inst lines for this connection)"
        else
            echo "$matches" | sed 's/^/    /'
        fi
    done

    echo
    echo "--- per-client report ---"
    for ((ix = 0; ix < NUM_CONNS; ix++)); do
        local log="$d/client_${ix}.log"
        local lines
        lines=$(wc -l <"$log" | tr -d ' ')
        local code="${CLIENT_EXIT_CODES[$ix]}"
        echo "  client #$ix  exit=$code  log=$lines line(s)"
        if [[ "$lines" -eq 0 ]]; then
            echo "    WARNING: log is empty -- client crashed before any output"
            echo "    or its connect() failed."
        else
            if grep -q "client #" "$log"; then
                grep "client #" "$log" | sed 's/^/    /'
            else
                echo "    (no \"client #\" summary line; tail of log:)"
                tail -10 "$log" | sed 's/^/    | /'
            fi
        fi
    done

    echo
    echo "Done. Full logs:"
    ls -1 "$d"
}

# --- retry loop -----------------------------------------------------------
final_status=1
for ((attempt = 1; attempt <= RETRY + 1; attempt++)); do
    ATTEMPT_DIR="$LOG_DIR/attempt_${attempt}"
    mkdir -p "$ATTEMPT_DIR"

    set +e
    run_attempt "$attempt" "$ATTEMPT_DIR"
    rc=$?
    set -e

    if [[ "$rc" -eq 0 ]]; then
        print_summary "$ATTEMPT_DIR"
        final_status=0
        break
    elif [[ "$rc" -eq 3 ]]; then
        # Handshake race: known protocol bug, retry transparently.
        echo
        echo ">>> attempt $attempt hit the known handshake race (see docs/archive/BINARY_RACE_CONDITIONS.md)."
        if (( attempt < RETRY + 1 )); then
            echo ">>> retrying ($((attempt + 1)) of $((RETRY + 1)))..."
            continue
        else
            echo ">>> exhausted $((RETRY + 1)) attempts; giving up."
            print_summary "$ATTEMPT_DIR"
            final_status=3
            break
        fi
    else
        # Hard failure (server died, etc) — don't retry.
        print_summary "$ATTEMPT_DIR"
        final_status="$rc"
        break
    fi
done

exit "$final_status"

