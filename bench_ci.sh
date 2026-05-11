#!/usr/bin/env bash
# CI throughput regression check for Bluefin.
#
# Drives `bench_two_process.sh` N times, parses the per-connection FINAL
# lines from each run's server.log, aggregates stats, and asserts they meet
# floor thresholds. Designed to fail PR CI on catastrophic regressions
# without being so tight that virtualised-runner noise causes false alarms.
#
# Usage:
#   ./bench_ci.sh                       # 5 runs, 2 conns, default floors
#   ./bench_ci.sh -r 5 -n 2             # explicit
#   ./bench_ci.sh --skip-build          # reuse existing release binaries
#
# Floors (env vars; override in CI YAML or shell):
#   BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS   default 0.05  (50 MB/s)
#   BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS   default 0.10  (100 MB/s)
#   BLUEFIN_BENCH_FLOOR_GOOD_CONNS      default 6     (of N_RUNS * N_CONNS)
#
# A "good conn" is one that emitted a FINAL line AND delivered at least
# GOOD_CONN_MIN_BYTES (default 14e9 = ~93% of the 15 GB target). This
# distinguishes a successful run from a connection that was truncated by
# the bench's idle timeout (the bilateral-failure pattern).
#
# Exit codes:
#   0   all floors met
#   1   one or more floors missed
#   2   bench harness error (server died, no FINAL lines parsed, etc.)

set -euo pipefail

# --- defaults --------------------------------------------------------------
N_RUNS=5
N_CONNS=2
SKIP_BUILD=0
RETRY=3                 # tolerate handshake race up to 3 retries per run
CLIENT_TIMEOUT=120

FLOOR_MEAN_AVG_GBPS="${BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS:-0.05}"
FLOOR_MAX_PEAK_GBPS="${BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS:-0.10}"
FLOOR_GOOD_CONNS="${BLUEFIN_BENCH_FLOOR_GOOD_CONNS:-6}"
GOOD_CONN_MIN_BYTES="${BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES:-14000000000}"

# --- arg parsing -----------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        -r|--runs)        N_RUNS="$2"; shift 2 ;;
        -n|--num-conns)   N_CONNS="$2"; shift 2 ;;
        --skip-build)     SKIP_BUILD=1; shift ;;
        --retry)          RETRY="$2"; shift 2 ;;
        -t|--timeout)     CLIENT_TIMEOUT="$2"; shift 2 ;;
        -h|--help)        sed -n '2,28p' "$0"; exit 0 ;;
        *)                echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

if [[ ! -x ./bench_two_process.sh ]]; then
    echo "error: ./bench_two_process.sh not found or not executable" >&2
    exit 2
fi

# --- build once outside the loop ------------------------------------------
if (( SKIP_BUILD == 0 )); then
    echo "[ci-bench] building release binaries..."
    cargo build --release --bin server --bin client \
        2>&1 | grep -E "(Compiling|Finished|error)" || true
fi

# --- per-run accumulators -------------------------------------------------
TOTAL_CONNS=$(( N_RUNS * N_CONNS ))
GOOD_CONNS=0
ALL_AVGS=()      # gb/s, every conn that emitted a FINAL line
ALL_PEAKS=()    # gb/s, ditto
RUN_LOG_DIRS=()
RUN_RC=()
# Parallel arrays keyed by run index (0-based). Each slot is a space-joined
# string for that run; empty if no FINAL lines were parsed.
RUN_AVGS=()      # avg gb/s per conn, space-joined
RUN_PEAKS=()    # peak gb/s per conn, space-joined
RUN_GOOD=()     # good-conn count for this run

echo
echo "=========================================================================="
echo " bluefin CI bench: $N_RUNS run(s) x $N_CONNS conn(s) = $TOTAL_CONNS conn-trials"
echo "=========================================================================="
echo " floors:"
echo "   mean avg gb/s : >= $FLOOR_MEAN_AVG_GBPS"
echo "   max peak gb/s : >= $FLOOR_MAX_PEAK_GBPS"
echo "   good conns    : >= $FLOOR_GOOD_CONNS / $TOTAL_CONNS"
echo "   good-conn floor bytes: $GOOD_CONN_MIN_BYTES"
echo "=========================================================================="

# --- run loop -------------------------------------------------------------
for ((run = 1; run <= N_RUNS; run++)); do
    echo
    echo "--- run $run/$N_RUNS ---"

    run_stdout="$(mktemp -t bluefin-bench.XXXXXX)"
    set +e
    ./bench_two_process.sh \
        -n "$N_CONNS" \
        -t "$CLIENT_TIMEOUT" \
        --retry "$RETRY" \
        --skip-build \
        2>&1 | tee "$run_stdout"
    rc="${PIPESTATUS[0]}"
    set -e
    RUN_RC+=("$rc")

    # Pull the log dir the bench script printed.
    log_dir=$(grep -m1 '^\[log-dir\] ' "$run_stdout" | awk '{print $2}')
    rm -f "$run_stdout"

    if [[ -z "$log_dir" || ! -d "$log_dir" ]]; then
        echo "error: could not determine log-dir for run $run (stdout missing '[log-dir]' marker)" >&2
        exit 2
    fi
    RUN_LOG_DIRS+=("$log_dir")

    # Pick the *last* attempt's server.log (bench_two_process retries on
    # handshake race; only the successful attempt has the real numbers).
    last_attempt_dir=$(ls -d "$log_dir"/attempt_* 2>/dev/null | sort -V | tail -1)
    if [[ -z "$last_attempt_dir" ]]; then
        echo "  WARN: run $run produced no attempt_* directory under $log_dir"
        continue
    fi
    server_log="$last_attempt_dir/server.log"

    if [[ ! -s "$server_log" ]]; then
        echo "  WARN: run $run server.log empty or missing ($server_log)"
        RUN_AVGS+=("")
        RUN_PEAKS+=("")
        RUN_GOOD+=("0")
        continue
    fi

    run_avgs_local=()
    run_peaks_local=()
    run_good_local=0

    # Parse FINAL lines. Format:
    #   (#0) FINAL: 15000000119 bytes in 8.183 s -- avg 1.83 gb/s (peak 3.85 gb/s)
    while IFS= read -r line; do
        # awk extracts: bytes, avg, peak
        read -r bytes avg peak <<<"$(awk '{
            for (i = 1; i <= NF; i++) {
                if ($i == "FINAL:") b = $(i+1)
                if ($i == "avg")     a = $(i+1)
                if ($i == "(peak")   p = $(i+1)
            }
            print b, a, p
        }' <<<"$line")"

        if [[ -z "$bytes" || -z "$avg" || -z "$peak" ]]; then
            continue
        fi

        ALL_AVGS+=("$avg")
        ALL_PEAKS+=("$peak")
        run_avgs_local+=("$avg")
        run_peaks_local+=("$peak")

        # Truncated conns (idle timeout / bilateral failure) DO emit a FINAL
        # line; we exclude them from the "good" count via the bytes floor.
        if (( $(awk -v b="$bytes" -v f="$GOOD_CONN_MIN_BYTES" 'BEGIN{print (b+0 >= f+0) ? 1 : 0}') )); then
            GOOD_CONNS=$((GOOD_CONNS + 1))
            run_good_local=$((run_good_local + 1))
            verdict="GOOD"
        else
            verdict="TRUNC"
        fi
        echo "  $verdict  bytes=$bytes  avg=${avg} gb/s  peak=${peak} gb/s"
    done < <(grep -E '^\(#[0-9]+\) FINAL:' "$server_log" || true)

    RUN_AVGS+=("${run_avgs_local[*]:-}")
    RUN_PEAKS+=("${run_peaks_local[*]:-}")
    RUN_GOOD+=("$run_good_local")
done

echo
echo "=========================================================================="
echo " aggregate"
echo "=========================================================================="

if [[ ${#ALL_AVGS[@]} -eq 0 ]]; then
    echo "ERROR: no FINAL lines parsed across $N_RUNS run(s). Bench harness is broken." >&2
    exit 2
fi

# awk for mean/max so we don't depend on bc/python on the runner.
mean_avg=$(printf '%s\n' "${ALL_AVGS[@]}" | awk '{s+=$1; n++} END{printf "%.3f", s/n}')
mean_peak=$(printf '%s\n' "${ALL_PEAKS[@]}" | awk '{s+=$1; n++} END{printf "%.3f", s/n}')
max_peak=$(printf '%s\n' "${ALL_PEAKS[@]}" | awk 'BEGIN{m=0} {if($1>m) m=$1} END{printf "%.3f", m}')
min_avg=$(printf '%s\n' "${ALL_AVGS[@]}" | awk 'BEGIN{m=1e9} {if($1<m) m=$1} END{printf "%.3f", m}')

printf "  conn-trials:        %d (good %d, missing/trunc %d)\n" \
    "$TOTAL_CONNS" "$GOOD_CONNS" "$((TOTAL_CONNS - GOOD_CONNS))"
printf "  mean avg gb/s:      %s    (floor %s)\n" "$mean_avg" "$FLOOR_MEAN_AVG_GBPS"
printf "  min  avg gb/s:      %s\n" "$min_avg"
printf "  mean peak gb/s:     %s\n" "$mean_peak"
printf "  max  peak gb/s:     %s    (floor %s)\n" "$max_peak" "$FLOOR_MAX_PEAK_GBPS"

# --- gate -----------------------------------------------------------------
fails=()

if (( $(awk -v v="$mean_avg" -v f="$FLOOR_MEAN_AVG_GBPS" 'BEGIN{print (v+0 < f+0) ? 1 : 0}') )); then
    fails+=("mean avg gb/s ${mean_avg} < floor ${FLOOR_MEAN_AVG_GBPS}")
fi
if (( $(awk -v v="$max_peak" -v f="$FLOOR_MAX_PEAK_GBPS" 'BEGIN{print (v+0 < f+0) ? 1 : 0}') )); then
    fails+=("max peak gb/s ${max_peak} < floor ${FLOOR_MAX_PEAK_GBPS}")
fi
if (( GOOD_CONNS < FLOOR_GOOD_CONNS )); then
    fails+=("good conns ${GOOD_CONNS} < floor ${FLOOR_GOOD_CONNS}")
fi

if [[ ${#fails[@]} -eq 0 ]]; then
    verdict_overall="PASS"
    verdict_emoji=":white_check_mark:"
else
    verdict_overall="FAIL"
    verdict_emoji=":x:"
fi

# --- markdown summary (PR comment + step summary) -------------------------
# Path is overridable so the workflow can pin it to a known location.
SUMMARY_MD="${BLUEFIN_BENCH_SUMMARY_MD:-bench_logs/ci_summary.md}"
mkdir -p "$(dirname "$SUMMARY_MD")"
{
    echo "## Bluefin throughput bench $verdict_emoji $verdict_overall"
    echo
    echo "**Config:** $N_RUNS run(s) × $N_CONNS conn(s) on \`$(uname -s) $(uname -m)\`"
    echo
    echo "### Aggregate"
    echo
    echo "| metric | observed | floor | result |"
    echo "| --- | ---: | ---: | :---: |"
    if (( $(awk -v v="$mean_avg" -v f="$FLOOR_MEAN_AVG_GBPS" 'BEGIN{print (v+0 < f+0) ? 1 : 0}') )); then
        echo "| mean avg gb/s | $mean_avg | $FLOOR_MEAN_AVG_GBPS | :x: |"
    else
        echo "| mean avg gb/s | $mean_avg | $FLOOR_MEAN_AVG_GBPS | :white_check_mark: |"
    fi
    echo "| min  avg gb/s | $min_avg | — | — |"
    echo "| mean peak gb/s | $mean_peak | — | — |"
    if (( $(awk -v v="$max_peak" -v f="$FLOOR_MAX_PEAK_GBPS" 'BEGIN{print (v+0 < f+0) ? 1 : 0}') )); then
        echo "| max  peak gb/s | $max_peak | $FLOOR_MAX_PEAK_GBPS | :x: |"
    else
        echo "| max  peak gb/s | $max_peak | $FLOOR_MAX_PEAK_GBPS | :white_check_mark: |"
    fi
    if (( GOOD_CONNS < FLOOR_GOOD_CONNS )); then
        echo "| good conns | $GOOD_CONNS / $TOTAL_CONNS | $FLOOR_GOOD_CONNS | :x: |"
    else
        echo "| good conns | $GOOD_CONNS / $TOTAL_CONNS | $FLOOR_GOOD_CONNS | :white_check_mark: |"
    fi
    echo
    echo "### Per-run"
    echo
    echo "| run | good/N | mean avg gb/s | max peak gb/s |"
    echo "| ---: | :---: | ---: | ---: |"
    for ((i = 0; i < N_RUNS; i++)); do
        avgs="${RUN_AVGS[$i]}"
        peaks="${RUN_PEAKS[$i]}"
        good_n="${RUN_GOOD[$i]}"
        if [[ -z "$avgs" ]]; then
            echo "| $((i + 1)) | 0/$N_CONNS | — | — |"
            continue
        fi
        run_mean_avg=$(printf '%s\n' $avgs | awk '{s+=$1;n++} END{printf "%.2f", s/n}')
        run_max_peak=$(printf '%s\n' $peaks | awk 'BEGIN{m=0} {if($1>m) m=$1} END{printf "%.2f", m}')
        echo "| $((i + 1)) | $good_n/$N_CONNS | $run_mean_avg | $run_max_peak |"
    done
    echo
    if [[ ${#fails[@]} -gt 0 ]]; then
        echo "### Failures"
        echo
        for f in "${fails[@]}"; do
            echo "- $f"
        done
        echo
    fi
    echo "<sub>A \"good conn\" delivered ≥ $((GOOD_CONN_MIN_BYTES / 1000000000)) GB before idle timeout. "
    echo "Floors are env-var overridable: \`BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS\`, "
    echo "\`BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS\`, \`BLUEFIN_BENCH_FLOOR_GOOD_CONNS\`. "
    echo "Ratchet up after stable runs land. Script: \`bench_ci.sh\`.</sub>"
} > "$SUMMARY_MD"

echo
echo "[ci-bench] markdown summary written to $SUMMARY_MD"

# --- final stdout verdict + exit ------------------------------------------
echo
if [[ "$verdict_overall" == "PASS" ]]; then
    echo "VERDICT: PASS"
    exit 0
else
    echo "VERDICT: FAIL"
    for f in "${fails[@]}"; do
        echo "  - $f"
    done
    exit 1
fi
