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
# --retry catches transient failures (e.g. CI runner noise).
#
# The server-side hello queue (HelloState) eliminates the previous
# handshake race. Stagger defaults to 0; --stagger is preserved for
# manual experimentation.
#
# Output goes to ./bench_logs/<timestamp>/{server,client_<ix>}.log
# A short summary (FINAL lines + recent inst lines) is printed at the end.

set -euo pipefail

# --- defaults --------------------------------------------------------------
NUM_CONNS=2
CLIENT_TIMEOUT=120          # hard cap on how long any client may run
SETTLE_AFTER_BUILD=2        # seconds to wait for server bind() to complete
STAGGER=0                   # seconds between spawning successive clients (0 = no stagger)
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
# DO NOT change the prefix — it's the contract between this script and
# `bench_ci.sh` (see skills/bluefin-ci/SKILL.md §"The [log-dir] contract").
echo "[log-dir] $LOG_DIR"

# --- pretty output helpers ------------------------------------------------
# ANSI styling is enabled only when stdout is a TTY, so CI logs (and any
# captured stdout consumed by `bench_ci.sh`) stay free of escape codes.
if [[ -t 1 ]]; then
    C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'; C_RESET=$'\033[0m'
    C_BLUE=$'\033[38;5;39m'; C_GREEN=$'\033[38;5;42m'
    C_YELLOW=$'\033[38;5;221m'; C_RED=$'\033[38;5;203m'
    C_CYAN=$'\033[38;5;87m'; C_GREY=$'\033[38;5;245m'
else
    C_BOLD=''; C_DIM=''; C_RESET=''
    C_BLUE=''; C_GREEN=''; C_YELLOW=''; C_RED=''; C_CYAN=''; C_GREY=''
fi

# 70-char rule made of light box-drawing horizontals.
_RULE='──────────────────────────────────────────────────────────────────────'

banner() {
    # banner "Section title"
    local title="$1"
    local pad=$(( 70 - ${#title} - 6 ))
    (( pad < 1 )) && pad=1
    printf '\n%s┌─[ %s%s%s ]%s%s\n' \
        "$C_BOLD$C_BLUE" "$C_RESET$C_BOLD" "$title" "$C_RESET$C_BOLD$C_BLUE" \
        "${_RULE:0:$pad}" "$C_RESET"
}
endbanner() {
    printf '%s└%s%s\n' "$C_BOLD$C_BLUE" "${_RULE}" "$C_RESET"
}
section() {
    printf '\n  %s%s%s\n  %s%s%s\n' \
        "$C_BOLD" "$1" "$C_RESET" \
        "$C_DIM" "${_RULE:0:${#1}}" "$C_RESET"
}
step()    { printf '  %s▸%s %s\n' "$C_CYAN" "$C_RESET" "$*"; }
ok()      { printf '      %s✓%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn()    { printf '      %s!%s %s\n' "$C_YELLOW" "$C_RESET" "$*"; }
fail()    { printf '      %s✗%s %s\n' "$C_RED" "$C_RESET" "$*"; }
info()    { printf '      %s%s%s\n' "$C_GREY" "$*" "$C_RESET"; }
kv()      { # kv "key" "value"
    printf '  %s%-13s%s %s%s%s\n' "$C_DIM" "$1" "$C_RESET" "$C_BOLD" "$2" "$C_RESET"
}

# --- live progress bar ----------------------------------------------------
# Spawned as a backgrounded subshell during the "waiting for clients"
# step. Tails the in-progress server log (which the per-conn task writes
# as `<ix> avg <…> | inst <X> [kg]b/s | …` every ~3500 iters) and
# refreshes ONE line in place using \r + ANSI clear-to-EOL, rendering
# ONE small bar per connection side-by-side. Disabled automatically on
# non-TTY stdout so CI logs stay clean.
#
# **CI contract** (do not remove the `[[ -t 1 ]]` guard): `bench_ci.sh`
# runs this script through `... | tee "$run_stdout"` and greps the
# captured stdout for `^\[log-dir\] `. The tee pipe is not a TTY, so the
# monitor short-circuits there and emits no `\r` / ANSI bytes. Same
# protection covers GitHub-hosted runners (no PTY) and any user who
# pipes the script (`./bench_two_process.sh | grep ...`).
#
# Per-conn progress is estimated from the server log's running average:
# `bytes_so_far ≈ avg_rate × elapsed_seconds`, compared against the
# expected payload (`BLUEFIN_NUM_SENDS × 1500 B` per conn, with the
# default matching the client binary). It's an estimate, not exact -- a
# starved conn whose `avg` rate hasn't built up yet will show low pct
# even after a while. That's fine and informative: it tells you which
# conn is the slow one. Bars cap at 100 % so a finished conn just stays
# pegged until the monitor is killed by `stop_progress_monitor`.
PROGRESS_PID=""
PROGRESS_BAR_WIDTH=10            # per-conn bar width
start_progress_monitor() {
    [[ -t 1 ]] || return 0
    local server_log="$1"
    local start_ts="$2"
    shift 2
    local ixs=("$@")
    # Expected payload per conn = num_sends × 1500 B (matches
    # client.rs:DEFAULT_NUM_SENDS and the BLUEFIN_NUM_SENDS override).
    local num_sends="${BLUEFIN_NUM_SENDS:-10000000}"
    [[ "$num_sends" =~ ^[0-9]+$ ]] || num_sends=10000000
    local expected_bytes=$(( num_sends * 1500 ))
    (
        # Subshell -- our own trap so cleanup() doesn't kill our parent's
        # state. Exit silently when signalled.
        trap 'exit 0' TERM INT
        # Cap refresh at ~4 Hz; faster wastes CPU and flickers more.
        local interval=0.25
        while true; do
            local now elapsed segment ix latest avg_val avg_unit inst_val inst_unit
            local avg_bps bytes_so_far pct filled empty bar line=""
            now="$(date +%s)"
            elapsed=$(( now - start_ts ))
            (( elapsed < 0 )) && elapsed=0
            for ix in "${ixs[@]}"; do
                # Pull the last server-log line for this conn.
                latest="$(grep -E "^${ix} " "$server_log" 2>/dev/null | tail -n 1)"
                pct=0
                inst_val=""; inst_unit=""
                if [[ -n "$latest" ]]; then
                    if [[ "$latest" =~ avg\ ([0-9]+\.[0-9]+)\ ([kmg]b/s) ]]; then
                        avg_val="${BASH_REMATCH[1]}"
                        avg_unit="${BASH_REMATCH[2]}"
                        # Convert to bytes/sec via the unit.
                        case "$avg_unit" in
                            gb/s) avg_bps="$(awk -v v="$avg_val" 'BEGIN{printf "%.0f", v*1e9}')" ;;
                            mb/s) avg_bps="$(awk -v v="$avg_val" 'BEGIN{printf "%.0f", v*1e6}')" ;;
                            kb/s) avg_bps="$(awk -v v="$avg_val" 'BEGIN{printf "%.0f", v*1e3}')" ;;
                            *)    avg_bps=0 ;;
                        esac
                        bytes_so_far=$(( avg_bps * elapsed ))
                        if (( expected_bytes > 0 )); then
                            pct=$(( bytes_so_far * 100 / expected_bytes ))
                            (( pct > 100 )) && pct=100
                            (( pct < 0 )) && pct=0
                        fi
                    fi
                    if [[ "$latest" =~ inst\ ([0-9]+\.[0-9]+)\ ([kmg]b/s) ]]; then
                        inst_val="${BASH_REMATCH[1]}"
                        inst_unit="${BASH_REMATCH[2]}"
                    fi
                fi
                filled=$(( pct * PROGRESS_BAR_WIDTH / 100 ))
                empty=$(( PROGRESS_BAR_WIDTH - filled ))
                bar="$(printf '█%.0s' $(seq 1 $filled 2>/dev/null) 2>/dev/null)$(printf '·%.0s' $(seq 1 $empty 2>/dev/null) 2>/dev/null)"
                if [[ -n "$inst_val" ]]; then
                    segment="$(printf '%sc%d%s %s[%s]%s %3d%% %s%s %s%s' \
                        "$C_BOLD" "$ix" "$C_RESET" \
                        "$C_CYAN" "$bar" "$C_RESET" \
                        "$pct" \
                        "$C_DIM" "$inst_val $inst_unit" "$C_RESET" "")"
                else
                    segment="$(printf '%sc%d%s %s[%s]%s %3d%% %s%s%s' \
                        "$C_BOLD" "$ix" "$C_RESET" \
                        "$C_CYAN" "$bar" "$C_RESET" \
                        "$pct" \
                        "$C_DIM" "(warming up)" "$C_RESET")"
                fi
                if [[ -z "$line" ]]; then
                    line="$segment"
                else
                    line="$line   $segment"
                fi
            done
            printf '\r      %s\033[K' "$line"
            sleep "$interval"
        done
    ) &
    PROGRESS_PID=$!
}
stop_progress_monitor() {
    if [[ -n "$PROGRESS_PID" ]]; then
        kill "$PROGRESS_PID" 2>/dev/null || true
        wait "$PROGRESS_PID" 2>/dev/null || true
        PROGRESS_PID=""
        # Wipe the live line so subsequent ok/warn output starts clean.
        [[ -t 1 ]] && printf '\r\033[K'
    fi
}

# --- inst-throughput line chart ------------------------------------------
# At end-of-run, render an ASCII line chart of each conn's instantaneous
# throughput over time. TTY-gated for the same CI-safety reasons as the
# progress monitor. Resolution intentionally low (60 cols × 10 rows) --
# the goal is "relative trends across conns" not a precise plot.
#
# Series source: per-conn `^<ix> ... inst <V> <unit>` lines in the
# server log (one print every ~3500 iters, see server.rs:print_throughput).
# Implementation in portable awk; markers are the bare conn ix digit
# (`0`/`1`/...) with `*` for collisions between conns at the same cell.
CHART_WIDTH=60
CHART_HEIGHT=10
render_inst_chart() {
    [[ -t 1 ]] || return 0
    local server_log="$1"
    [[ -s "$server_log" ]] || return 0
    awk -v W="$CHART_WIDTH" -v H="$CHART_HEIGHT" '
    function fmt(v,    s) {
        if (v >= 1000) { s = sprintf("%.2f gb/s", v/1000); return s }
        if (v >= 1)    { s = sprintf("%.1f mb/s", v);     return s }
        s = sprintf("%.0f kb/s", v*1000); return s
    }
    # Server prints fields:
    #   $1=<ix>  $2=avg  $3=<v>  $4=<unit>  $5=|  $6=inst  $7=<v>  $8=<unit>  ...
    $1 ~ /^[0-9]+$/ && $6 == "inst" {
        ix = $1 + 0
        v  = $7 + 0
        u  = $8
        if (u == "gb/s")      v = v * 1000   # store everything as mb/s
        else if (u == "kb/s") v = v / 1000
        n = ++counts[ix]
        samples[ix SUBSEP n] = v
        if (v > vmax)   vmax = v
        if (ix > max_ix) max_ix = ix
    }
    END {
        if (max_ix < 0 || vmax <= 0) {
            print "      (no inst samples in server log -- nothing to plot)"
            exit
        }
        # Resample each conn to W columns.
        for (ix = 0; ix <= max_ix; ix++) {
            n = counts[ix] + 0
            if (n == 0) continue
            for (c = 0; c < W; c++) {
                if (n == 1) {
                    series[ix SUBSEP c] = samples[ix SUBSEP 1]
                    has[ix SUBSEP c] = 1
                } else if (n <= W) {
                    src  = c * (n - 1) / (W - 1)
                    lo   = int(src); hi = lo + 1
                    if (hi >= n) hi = n - 1
                    frac = src - lo
                    series[ix SUBSEP c] = samples[ix SUBSEP (lo+1)] * (1 - frac) \
                                        + samples[ix SUBSEP (hi+1)] * frac
                    has[ix SUBSEP c] = 1
                } else {
                    s = int(c * n / W) + 1
                    e = int((c+1) * n / W)
                    if (e < s) e = s
                    if (e > n) e = n
                    sum = 0; k = 0
                    for (i = s; i <= e; i++) { sum += samples[ix SUBSEP i]; k++ }
                    if (k > 0) {
                        series[ix SUBSEP c] = sum / k
                        has[ix SUBSEP c] = 1
                    }
                }
            }
        }
        # Build the 2-D grid (row 0 = top of y-axis).
        for (r = 0; r < H; r++) for (c = 0; c < W; c++) grid[r SUBSEP c] = " "
        for (ix = 0; ix <= max_ix; ix++) {
            for (c = 0; c < W; c++) {
                if (!has[ix SUBSEP c]) continue
                y = (series[ix SUBSEP c] / vmax) * (H - 1)
                r = (H - 1) - int(y + 0.5)
                if (r < 0)  r = 0
                if (r >= H) r = H - 1
                cur = grid[r SUBSEP c]
                if (cur == " ")                grid[r SUBSEP c] = ix
                else if (cur != (ix ""))       grid[r SUBSEP c] = "*"
            }
        }
        # Render.
        for (r = 0; r < H; r++) {
            if (r % 2 == 0) label = fmt(vmax * (H - 1 - r) / (H - 1))
            else            label = ""
            printf "      %10s │", label
            for (c = 0; c < W; c++) printf "%s", grid[r SUBSEP c]
            printf "\n"
        }
        # X-axis.
        printf "      %10s └", ""
        for (c = 0; c < W; c++) printf "─"
        printf "\n"
        # X-axis labels.
        end_label = "end"
        pad = W - length("start") - length(end_label)
        if (pad < 1) pad = 1
        printf "      %10s   start", ""
        for (i = 0; i < pad; i++) printf " "
        printf "%s\n", end_label
        # Legend.
        printf "      %10s   ", ""
        for (ix = 0; ix <= max_ix; ix++) {
            if (counts[ix] + 0 == 0) continue
            if (ix > 0) printf "  "
            printf "%d = conn #%d", ix, ix
        }
        if (max_ix >= 1) printf "    * = overlap"
        printf "\n"
    }
    ' "$server_log"
}

# --- top banner -----------------------------------------------------------
banner "Bluefin two-process throughput benchmark"
kv "connections" "$NUM_CONNS"
kv "timeout"     "${CLIENT_TIMEOUT}s"
kv "stagger"     "${STAGGER}s"
kv "retry"       "$RETRY"
kv "log-root"    "$LOG_DIR"
endbanner

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
    printf '\n'
    # Kill the live progress monitor first so it stops repainting over
    # subsequent cleanup output.
    stop_progress_monitor
    step "cleanup: stopping background processes"
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
banner "Build"
if (( SKIP_BUILD == 0 )); then
    step "compiling release binaries (server + client)"
    cargo build --release --bin server --bin client \
        2>&1 | grep -E "(Compiling|Finished|error)" \
        | sed "s/^/      /" || true
    ok "build complete"
else
    step "--skip-build set; reusing existing binaries"
fi
endbanner

if [[ ! -x "$SERVER_BIN" ]]; then
    fail "$SERVER_BIN not found or not executable"
    exit 1
fi
if [[ ! -x "$CLIENT_BIN" ]]; then
    fail "$CLIENT_BIN not found or not executable"
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

    banner "Attempt $attempt_ix of $((RETRY + 1))"

    # --- 2. kill any stale benchmark processes -----------------------------
    step "killing stale server/client processes"
    pkill -9 -f "$SERVER_BIN" 2>/dev/null || true
    pkill -9 -f "$CLIENT_BIN" 2>/dev/null || true
    sleep 0.5

    # --- 3. start server ---------------------------------------------------
    local server_log="$attempt_log_dir/server.log"
    step "starting server"
    # Pass NUM_CONNS through so the server accepts exactly that many handshakes
    # before starting recv loops. Without this it hardcodes 2 and any
    # `-n` other than 2 hangs (server waits forever on a 3rd accept, or
    # exits early before the 3rd client finishes its handshake).
    "$SERVER_BIN" "$NUM_CONNS" >"$server_log" 2>&1 &
    SERVER_PID=$!
    info "pid $SERVER_PID  →  $server_log"

    sleep "$SETTLE_AFTER_BUILD"
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        fail "server died during startup"
        info "last 20 lines of server log:"
        tail -20 "$server_log" | sed 's/^/        /' >&2
        endbanner
        return 2
    fi
    ok "server up"

    # --- 4. spawn N clients, one per task ix -------------------------------
    step "spawning $NUM_CONNS client process(es) (stagger ${STAGGER}s)"
    for ((ix = 0; ix < NUM_CONNS; ix++)); do
        local client_log="$attempt_log_dir/client_${ix}.log"
        if [[ -n "$TIMEOUT_BIN" ]]; then
            "$TIMEOUT_BIN" "$CLIENT_TIMEOUT" "$CLIENT_BIN" --task "$ix" \
                >"$client_log" 2>&1 &
        else
            "$CLIENT_BIN" --task "$ix" >"$client_log" 2>&1 &
        fi
        CLIENT_PIDS+=($!)
        info "client #$ix  pid ${CLIENT_PIDS[$ix]}  →  $client_log"
        if (( ix + 1 < NUM_CONNS )) && [[ "$STAGGER" != "0" ]]; then
            sleep "$STAGGER"
        fi
    done

    # --- 5. wait for clients, then drain server ----------------------------
    step "waiting for clients to finish"
    local start_wait_ts ixs=()
    start_wait_ts="$(date +%s)"
    for ((ix = 0; ix < NUM_CONNS; ix++)); do ixs+=("$ix"); done
    start_progress_monitor "$attempt_log_dir/server.log" "$start_wait_ts" "${ixs[@]}"
    CLIENT_EXIT_CODES=()
    for pid in "${CLIENT_PIDS[@]}"; do
        set +e
        wait "$pid"
        local code=$?
        set -e
        CLIENT_EXIT_CODES+=("$code")
    done
    stop_progress_monitor

    local handshake_failed=0
    for ((ix = 0; ix < NUM_CONNS; ix++)); do
        local code="${CLIENT_EXIT_CODES[$ix]}"
        if [[ "$code" -eq 0 ]]; then
            ok "client #$ix  exit $code"
        elif [[ "$code" -eq 124 ]]; then
            warn "client #$ix  exit $code  (hit ${CLIENT_TIMEOUT}s timeout)"
        else
            if grep -q "Failed to read from handshake connection buffer" \
                "$attempt_log_dir/client_${ix}.log"; then
                handshake_failed=1
                fail "client #$ix  exit $code  [HANDSHAKE FAIL]"
            else
                fail "client #$ix  exit $code  (check log)"
            fi
        fi
    done

    # Healthy server exits ~immediately after every connected client has
    # sent its `Fin`: `recv_bytes` returns `Ok(0)` (EOF), the per-conn
    # task prints `FINAL` and the join_set drains. The `BLUEFIN_RECV_IDLE_TIMEOUT_SECS`
    # window is now a safety net for crashed/SIGKILL'd clients only; we
    # still wait `recv_idle + 3` seconds so that if a client *did* die
    # without closing we don't SIGTERM the server before its idle-fallback
    # path prints FINAL.
    local recv_idle_secs="${BLUEFIN_RECV_IDLE_TIMEOUT_SECS:-2}"
    if ! [[ "$recv_idle_secs" =~ ^[0-9]+$ ]] || (( recv_idle_secs <= 0 )); then
        recv_idle_secs=2
    fi
    local grace_secs=$(( recv_idle_secs + 3 ))
    local grace_iters=$(( grace_secs * 10 ))
    step "awaiting server FINAL lines (up to ${grace_secs}s)"
    for _ in $(seq 1 "$grace_iters"); do
        if ! kill -0 "$SERVER_PID" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done
    if kill -0 "$SERVER_PID" 2>/dev/null; then
        warn "server still running after grace; sending SIGTERM"
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    else
        ok "server exited cleanly"
    fi
    SERVER_PID=""

    endbanner

    if (( handshake_failed )); then
        return 3
    fi
    return 0
}

# Print summary for one attempt's log directory.
print_summary() {
    local d="$1"
    local server_log="$d/server.log"

    banner "Summary"
    kv "log dir" "$d"

    local server_lines
    server_lines=$(wc -l <"$server_log" | tr -d ' ')
    kv "server log" "$server_lines line(s)"
    if [[ "$server_lines" -eq 0 ]]; then
        warn "server log is empty"
        info "Most likely cause: server is stuck in server.accept().await"
        info "waiting for a connection that never arrived (a client failed"
        info "to handshake). Check client logs below."
    fi

    section "Server FINAL lines"
    local final_lines
    final_lines=$(grep -E "FINAL|idle for" "$server_log" || true)
    if [[ -z "$final_lines" ]]; then
        info "(none found)"
    else
        printf '%s\n' "$final_lines" | sed "s/^/    ${C_GREEN}•${C_RESET} /"
    fi

    section "Per-connection tail (last 5 inst lines)"
    for ((ix = 0; ix < NUM_CONNS; ix++)); do
        printf '    %sconn #%d%s\n' "$C_BOLD" "$ix" "$C_RESET"
        local matches
        matches=$(grep -E "^${ix} " "$server_log" | tail -5 || true)
        if [[ -z "$matches" ]]; then
            printf '      %s(no inst lines for this connection)%s\n' "$C_DIM" "$C_RESET"
        else
            printf '%s\n' "$matches" | sed "s/^/      ${C_GREY}│${C_RESET} /"
        fi
    done

    # TTY-only: ASCII line graph of instantaneous throughput trends across
    # the whole run. Skipped on CI / piped stdout to keep logs clean.
    if [[ -t 1 ]]; then
        section "Instantaneous throughput trend"
        render_inst_chart "$server_log"
    fi

    section "Per-client report"
    for ((ix = 0; ix < NUM_CONNS; ix++)); do
        local log="$d/client_${ix}.log"
        local lines
        lines=$(wc -l <"$log" | tr -d ' ')
        local code="${CLIENT_EXIT_CODES[$ix]}"
        local mark color
        if [[ "$code" -eq 0 ]]; then
            mark="✓"; color="$C_GREEN"
        elif [[ "$code" -eq 124 ]]; then
            mark="!"; color="$C_YELLOW"
        else
            mark="✗"; color="$C_RED"
        fi
        printf '    %s%s%s client #%d  exit %s  log %s line(s)\n' \
            "$color" "$mark" "$C_RESET" "$ix" "$code" "$lines"
        if [[ "$lines" -eq 0 ]]; then
            printf '      %s⚠  log is empty -- client crashed before any output or its connect() failed%s\n' \
                "$C_YELLOW" "$C_RESET"
        else
            if grep -q "client #" "$log"; then
                grep "client #" "$log" | sed "s/^/      ${C_GREY}│${C_RESET} /"
            else
                printf '      %s(no "client #" summary line; tail of log:)%s\n' "$C_DIM" "$C_RESET"
                tail -10 "$log" | sed "s/^/      ${C_GREY}│${C_RESET} /"
            fi
        fi
    done

    section "Files"
    ls -1 "$d" | sed "s/^/    ${C_GREY}·${C_RESET} /"

    endbanner
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
        # Handshake failure, retry transparently.
        printf '\n'
        warn "attempt $attempt hit a handshake failure"
        if (( attempt < RETRY + 1 )); then
            info "retrying ($((attempt + 1)) of $((RETRY + 1)))..."
            continue
        else
            fail "exhausted $((RETRY + 1)) attempts; giving up"
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

