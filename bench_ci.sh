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
# Per-run retry knobs (env vars):
#   BLUEFIN_BENCH_RUN_RETRIES            default 0     # attempts per run = 1 + this
#   BLUEFIN_BENCH_RUN_RETRY_BACKOFF_SECS default 5     # sleep between attempts
# A run is retried only if it produced 0 GOOD conns (catastrophic
# allocation on hosted CI runners, where contention starves both peers).
# Only the final attempt's data feeds the aggregate stats.
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

# Retry a run that produced 0 GOOD conns up to this many extra times.
# Default 0 = current behaviour (no retry). CI sets this to 1 so
# catastrophic runner-allocation events (one in ~10 hosted-macos jobs by
# observation) get a second chance instead of flaking the gate. Only the
# final attempt's data is committed to the aggregate stats; earlier
# attempts' logs are still on disk under the run's `attempt_*` dirs but
# are NOT parsed.
RUN_RETRIES="${BLUEFIN_BENCH_RUN_RETRIES:-0}"
RUN_RETRY_BACKOFF_SECS="${BLUEFIN_BENCH_RUN_RETRY_BACKOFF_SECS:-5}"

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
ALL_AVGS=()       # gb/s, every conn that emitted a FINAL line
ALL_PEAKS=()      # gb/s, ditto (kept for mean_peak reporting)
GOOD_AVGS=()      # gb/s, avgs from conns that cleared the bytes floor only.
GOOD_PEAKS=()     # gb/s, peaks from conns that cleared the bytes floor only.
                  # Both used by the gate. See "drain artifacts" note below.
RUN_LOG_DIRS=()
RUN_RC=()
# Parallel arrays keyed by run index (0-based). Each slot is a space-joined
# string for that run; empty if no FINAL lines were parsed.
RUN_AVGS=()       # avg gb/s per conn, space-joined (all conns; kept for reference / debugging)
RUN_PEAKS=()      # peak gb/s per conn, space-joined (all conns, for reporting)
RUN_GOOD_AVGS=()  # avg gb/s per GOOD conn only (per-run mean_avg column)
RUN_GOOD_PEAKS=() # peak gb/s per GOOD conn only (per-run max_peak column)
RUN_GOOD=()       # good-conn count for this run
RUN_ATTEMPTS=()   # number of attempts taken (1 = no retry; 2 = one retry, etc.)

# Note on "drain artifacts" and starved-conn dilution:
#   On contended hosted runners the server task gets preempted for tens of
#   ms while the client keeps pumping at 1+ GB/s. The kernel UDP socket
#   buffer fills, then the server resumes and drains it at memcpy speed.
#   The recv loop's inst-throughput print sees `multi-MB / microseconds`
#   = 100+ GB/s and records it as `peak`. That's a measurement artifact,
#   not a real network rate. Separately, every conn's `avg` includes the
#   2 s recv-idle tail (`bytes/elapsed` where `elapsed` keeps running
#   during the idle wait); on starved conns that delivered <60 MB total,
#   the avg collapses to <30 MB/s and dilutes any aggregate that includes
#   it. Both pathologies are exclusive to STARVED conns. So we filter
#   BOTH max_peak and mean_avg to GOOD conns only (= conns that delivered
#   >= GOOD_CONN_MIN_BYTES) for the gate; the unfiltered numbers are kept
#   in the comment for visibility into runner contention.

echo
echo "=========================================================================="
echo " bluefin CI bench: $N_RUNS run(s) x $N_CONNS conn(s) = $TOTAL_CONNS conn-trials"
echo "=========================================================================="
echo " floors:"
echo "   mean avg gb/s : >= $FLOOR_MEAN_AVG_GBPS"
echo "   max peak gb/s : >= $FLOOR_MAX_PEAK_GBPS"
echo "   good conns    : >= $FLOOR_GOOD_CONNS / $TOTAL_CONNS"
echo "   good-conn floor bytes: $GOOD_CONN_MIN_BYTES"
echo "   per-run retries on 0 good: $RUN_RETRIES (backoff ${RUN_RETRY_BACKOFF_SECS}s)"
echo "=========================================================================="

# --- run loop -------------------------------------------------------------
for ((run = 1; run <= N_RUNS; run++)); do
    echo
    echo "--- run $run/$N_RUNS ---"

    # State for this run. We may retry on 0 good conns; only the final
    # attempt's data is committed to the global accumulators below.
    final_log_dir=""
    final_rc=0
    final_attempt=0   # 1-based; tracks which attempt produced the committed data
    run_avgs_local=()
    run_peaks_local=()
    run_good_avgs_local=()
    run_good_peaks_local=()
    run_good_local=0

    max_attempts=$(( RUN_RETRIES + 1 ))
    for ((attempt = 1; attempt <= max_attempts; attempt++)); do
        if (( attempt > 1 )); then
            echo "  [retry] run $run produced 0 good conns; sleeping ${RUN_RETRY_BACKOFF_SECS}s before attempt $attempt/$max_attempts..."
            sleep "$RUN_RETRY_BACKOFF_SECS"
        fi

        # Reset the per-attempt locals (a previous attempt's TRUNC entries
        # would otherwise leak into the committed totals).
        run_avgs_local=()
        run_peaks_local=()
        run_good_avgs_local=()
        run_good_peaks_local=()
        run_good_local=0

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
        final_rc="$rc"

        # Pull the log dir the bench script printed.
        log_dir=$(grep -m1 '^\[log-dir\] ' "$run_stdout" | awk '{print $2}')
        rm -f "$run_stdout"

        if [[ -z "$log_dir" || ! -d "$log_dir" ]]; then
            echo "error: could not determine log-dir for run $run (stdout missing '[log-dir]' marker)" >&2
            exit 2
        fi
        final_log_dir="$log_dir"

        # Pick the *last* attempt's server.log (bench_two_process retries on
        # handshake race; only the successful attempt has the real numbers).
        last_attempt_dir=$(ls -d "$log_dir"/attempt_* 2>/dev/null | sort -V | tail -1)
        if [[ -z "$last_attempt_dir" ]]; then
            echo "  WARN: run $run attempt $attempt produced no attempt_* directory under $log_dir"
            continue
        fi
        server_log="$last_attempt_dir/server.log"

        if [[ ! -s "$server_log" ]]; then
            echo "  WARN: run $run attempt $attempt server.log empty or missing ($server_log)"
            continue
        fi

        # Parse FINAL lines. Format (units are picked dynamically by the server,
        # so a truncated conn shows mb/s or even kb/s):
        #   (#0) FINAL: 15000000119 bytes in 8.183 s -- avg 1.83 gb/s (peak 3.85 gb/s)
        #   (#1) FINAL: 18169619    bytes in 2.002 s -- avg 9.1  mb/s (peak 0.0  kb/s)
        # We extract the value AND the unit for both avg and peak, then normalise
        # to gb/s so the aggregate stats are comparable across healthy and
        # truncated conns. Without normalisation, mixing 9.1 mb/s and 1.83 gb/s
        # as if both were gb/s produces a meaningless mean.
        while IFS= read -r line; do
            read -r bytes avg avg_unit peak peak_unit <<<"$(awk '{
                for (i = 1; i <= NF; i++) {
                    if ($i == "FINAL:") { b = $(i+1) }
                    if ($i == "avg")    { a = $(i+1); au = $(i+2) }
                    if ($i == "(peak")  { p = $(i+1); pu = $(i+2) }
                }
                print b, a, au, p, pu
            }' <<<"$line")"

            if [[ -z "$bytes" || -z "$avg" || -z "$peak" ]]; then
                continue
            fi

            # Normalise to gb/s. Server emits lowercase units with `/s` suffix:
            # "kb/s", "mb/s", "gb/s". The peak field is the last token on the
            # line so it carries a trailing `)` (e.g. "mb/s)"). Strip the `)`
            # FIRST, then the `/s`, before scaling.
            avg=$(awk -v v="$avg" -v u="$avg_unit" 'BEGIN{
                sub("\\)$", "", u);
                sub("/s$", "", u);
                if (u == "kb")      printf "%.4f", v / 1000000;
                else if (u == "mb") printf "%.4f", v / 1000;
                else if (u == "gb") printf "%.4f", v;
                else                printf "%.4f", v;   # unknown -> assume gb/s
            }')
            peak=$(awk -v v="$peak" -v u="$peak_unit" 'BEGIN{
                sub("\\)$", "", u);
                sub("/s$", "", u);
                if (u == "kb")      printf "%.4f", v / 1000000;
                else if (u == "mb") printf "%.4f", v / 1000;
                else if (u == "gb") printf "%.4f", v;
                else                printf "%.4f", v;
            }')

            run_avgs_local+=("$avg")
            run_peaks_local+=("$peak")

            # Truncated conns (idle timeout / bilateral failure) DO emit a FINAL
            # line; we exclude them from the "good" count via the bytes floor.
            if (( $(awk -v b="$bytes" -v f="$GOOD_CONN_MIN_BYTES" 'BEGIN{print (b+0 >= f+0) ? 1 : 0}') )); then
                run_good_avgs_local+=("$avg")
                run_good_peaks_local+=("$peak")
                run_good_local=$((run_good_local + 1))
                verdict="GOOD"
            else
                verdict="TRUNC"
            fi
            echo "  $verdict  bytes=$bytes  avg=${avg} gb/s  peak=${peak} gb/s"
        done < <(grep -E '^\(#[0-9]+\) FINAL:' "$server_log" || true)

        if (( run_good_local > 0 )); then
            final_attempt=$attempt
            break  # at least one good conn -- accept this attempt
        fi
        final_attempt=$attempt   # may be overwritten by next iteration; survives the loop
        if (( attempt < max_attempts )); then
            echo "  [retry] run $run attempt $attempt yielded 0 good conns; will retry"
        fi
    done

    # Commit the (possibly retried) attempt's results to the global aggregates.
    # Note: ALL_AVGS/ALL_PEAKS/GOOD_AVGS/GOOD_PEAKS/GOOD_CONNS used to be pushed
    # inline during parsing; we now stage to locals so we can discard a failed
    # attempt's data on retry.
    RUN_LOG_DIRS+=("$final_log_dir")
    RUN_RC+=("$final_rc")
    RUN_AVGS+=("${run_avgs_local[*]:-}")
    RUN_PEAKS+=("${run_peaks_local[*]:-}")
    RUN_GOOD_AVGS+=("${run_good_avgs_local[*]:-}")
    RUN_GOOD_PEAKS+=("${run_good_peaks_local[*]:-}")
    RUN_GOOD+=("$run_good_local")
    RUN_ATTEMPTS+=("$final_attempt")
    GOOD_CONNS=$(( GOOD_CONNS + run_good_local ))
    for v in "${run_avgs_local[@]+"${run_avgs_local[@]}"}"; do ALL_AVGS+=("$v"); done
    for v in "${run_peaks_local[@]+"${run_peaks_local[@]}"}"; do ALL_PEAKS+=("$v"); done
    for v in "${run_good_avgs_local[@]+"${run_good_avgs_local[@]}"}"; do GOOD_AVGS+=("$v"); done
    for v in "${run_good_peaks_local[@]+"${run_good_peaks_local[@]}"}"; do GOOD_PEAKS+=("$v"); done
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
# raw_* are reported in the comment for visibility into runner contention;
# mean_avg / max_peak (filtered to GOOD conns) are what the gate compares
# against. See "drain artifacts" / starved-conn-dilution note above.
raw_mean_avg=$(printf '%s\n' "${ALL_AVGS[@]}" | awk '{s+=$1; n++} END{printf "%.3f", s/n}')
mean_peak=$(printf '%s\n' "${ALL_PEAKS[@]}" | awk '{s+=$1; n++} END{printf "%.3f", s/n}')
min_avg=$(printf '%s\n' "${ALL_AVGS[@]}" | awk 'BEGIN{m=1e9} {if($1<m) m=$1} END{printf "%.3f", m}')
raw_max_peak=$(printf '%s\n' "${ALL_PEAKS[@]}" | awk 'BEGIN{m=0} {if($1>m) m=$1} END{printf "%.3f", m}')
if [[ ${#GOOD_AVGS[@]} -eq 0 ]]; then
    mean_avg="0.000"
    max_peak="0.000"
else
    mean_avg=$(printf '%s\n' "${GOOD_AVGS[@]}" | awk '{s+=$1; n++} END{printf "%.3f", s/n}')
    max_peak=$(printf '%s\n' "${GOOD_PEAKS[@]}" | awk 'BEGIN{m=0} {if($1>m) m=$1} END{printf "%.3f", m}')
fi

printf "  conn-trials:        %d (good %d, missing/trunc %d)\n" \
    "$TOTAL_CONNS" "$GOOD_CONNS" "$((TOTAL_CONNS - GOOD_CONNS))"
printf "  mean avg gb/s:      %s    (floor %s, good-conns only)\n" "$mean_avg" "$FLOOR_MEAN_AVG_GBPS"
printf "  raw mean avg gb/s:  %s    (incl. starved conns; idle-tail diluted)\n" "$raw_mean_avg"
printf "  min  avg gb/s:      %s\n" "$min_avg"
printf "  mean peak gb/s:     %s\n" "$mean_peak"
printf "  max  peak gb/s:     %s    (floor %s, good-conns only)\n" "$max_peak" "$FLOOR_MAX_PEAK_GBPS"
printf "  raw max peak gb/s:  %s    (incl. drain artifacts from starved conns)\n" "$raw_max_peak"

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
# Translate kernel name into the human OS label CI users expect to see.
# `uname -s` returns "Darwin" on macOS hosted runners, which is technically
# accurate but unhelpful in a PR comment titled for human readers.
uname_s="$(uname -s)"
case "$uname_s" in
    Darwin) os_label="macOS" ;;
    Linux)  os_label="Linux" ;;
    *)      os_label="$uname_s" ;;
esac
{
    echo "## Bluefin throughput bench ($os_label) $verdict_emoji $verdict_overall"
    echo
    echo "**Config:** $N_RUNS run(s) × $N_CONNS conn(s) on \`$uname_s $(uname -m)\`"
    # Surface retry summary so a green PR comment still reveals if the
    # gate had to absorb a catastrophic-allocation event. Counts runs that
    # took more than one attempt; the per-run table below shows which.
    retried_runs=0
    total_extra_attempts=0
    for a in "${RUN_ATTEMPTS[@]+"${RUN_ATTEMPTS[@]}"}"; do
        if (( a > 1 )); then
            retried_runs=$((retried_runs + 1))
            total_extra_attempts=$((total_extra_attempts + a - 1))
        fi
    done
    if (( retried_runs > 0 )); then
        echo
        echo "**Retries:** $retried_runs of $N_RUNS run(s) needed retry (+$total_extra_attempts extra attempt(s)). See per-run table."
    fi
    echo
    echo "### Aggregate"
    echo
    echo "| metric | observed | floor | result |"
    echo "| --- | ---: | ---: | :---: |"
    if (( $(awk -v v="$mean_avg" -v f="$FLOOR_MEAN_AVG_GBPS" 'BEGIN{print (v+0 < f+0) ? 1 : 0}') )); then
        echo "| mean avg gb/s (good only) | $mean_avg | $FLOOR_MEAN_AVG_GBPS | :x: |"
    else
        echo "| mean avg gb/s (good only) | $mean_avg | $FLOOR_MEAN_AVG_GBPS | :white_check_mark: |"
    fi
    if (( $(awk -v g="$mean_avg" -v r="$raw_mean_avg" 'BEGIN{print (r+0 < g+0 - 0.001) ? 1 : 0}') )); then
        echo "| raw mean avg gb/s | $raw_mean_avg | — | — |"
    fi
    echo "| min  avg gb/s | $min_avg | — | — |"
    echo "| mean peak gb/s | $mean_peak | — | — |"
    if (( $(awk -v v="$max_peak" -v f="$FLOOR_MAX_PEAK_GBPS" 'BEGIN{print (v+0 < f+0) ? 1 : 0}') )); then
        echo "| max  peak gb/s (good only) | $max_peak | $FLOOR_MAX_PEAK_GBPS | :x: |"
    else
        echo "| max  peak gb/s (good only) | $max_peak | $FLOOR_MAX_PEAK_GBPS | :white_check_mark: |"
    fi
    if (( $(awk -v g="$max_peak" -v r="$raw_max_peak" 'BEGIN{print (r+0 > g+0 + 0.001) ? 1 : 0}') )); then
        echo "| raw max peak gb/s | $raw_max_peak | — | — |"
    fi
    if (( GOOD_CONNS < FLOOR_GOOD_CONNS )); then
        echo "| good conns | $GOOD_CONNS / $TOTAL_CONNS | $FLOOR_GOOD_CONNS | :x: |"
    else
        echo "| good conns | $GOOD_CONNS / $TOTAL_CONNS | $FLOOR_GOOD_CONNS | :white_check_mark: |"
    fi
    echo
    echo "### Per-run"
    echo
    echo "| run | attempts | good/N | mean avg gb/s (good) | max peak gb/s (good) | raw max peak gb/s |"
    echo "| ---: | :---: | :---: | ---: | ---: | ---: |"
    for ((i = 0; i < N_RUNS; i++)); do
        avgs="${RUN_AVGS[$i]}"
        peaks="${RUN_PEAKS[$i]}"
        good_avgs="${RUN_GOOD_AVGS[$i]}"
        good_peaks="${RUN_GOOD_PEAKS[$i]}"
        good_n="${RUN_GOOD[$i]}"
        attempts_n="${RUN_ATTEMPTS[$i]:-1}"
        # Bold the attempts cell when retried so the PR comment reader can
        # spot retried runs at a glance without scanning numbers.
        if (( attempts_n > 1 )); then
            attempts_cell="**$attempts_n**"
        else
            attempts_cell="$attempts_n"
        fi
        if [[ -z "$avgs" ]]; then
            echo "| $((i + 1)) | $attempts_cell | 0/$N_CONNS | — | — | — |"
            continue
        fi
        # Per-run `mean avg gb/s` is filtered to GOOD conns only, mirroring
        # the aggregate. Without this filter, a TRUNC conn whose elapsed
        # collapses to <100 ms after recv-idle subtraction reports a wildly
        # inflated avg (observed: 14 GB/s with 0 good conns), which is the
        # same drain-artifact pathology as max_peak just expressed via the
        # divisor instead of the inst-throughput print.
        if [[ -z "$good_avgs" ]]; then
            run_mean_avg="—"
        else
            run_mean_avg=$(printf '%s\n' $good_avgs | awk '{s+=$1;n++} END{printf "%.2f", s/n}')
        fi
        run_raw_peak=$(printf '%s\n' $peaks | awk 'BEGIN{m=0} {if($1>m) m=$1} END{printf "%.2f", m}')
        if [[ -z "$good_peaks" ]]; then
            run_good_peak="—"
        else
            run_good_peak=$(printf '%s\n' $good_peaks | awk 'BEGIN{m=0} {if($1>m) m=$1} END{printf "%.2f", m}')
        fi
        echo "| $((i + 1)) | $attempts_cell | $good_n/$N_CONNS | $run_mean_avg | $run_good_peak | $run_raw_peak |"
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
    # Footnote: explain the bytes threshold in human-readable units (MB or GB).
    if (( GOOD_CONN_MIN_BYTES >= 1000000000 )); then
        good_threshold_human="$((GOOD_CONN_MIN_BYTES / 1000000000)) GB"
    else
        good_threshold_human="$((GOOD_CONN_MIN_BYTES / 1000000)) MB"
    fi
    echo "<sub>A \"good conn\" delivered ≥ ${good_threshold_human} before idle timeout. "
    echo "\"max peak (good only)\" filters out kernel-buffer-drain artifacts "
    echo "that appear as huge inst-throughput readings on starved conns under "
    echo "runner CPU contention. Floors are env-var overridable: "
    echo "\`BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS\`, \`BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS\`, "
    echo "\`BLUEFIN_BENCH_FLOOR_GOOD_CONNS\`, \`BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES\`. "
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
