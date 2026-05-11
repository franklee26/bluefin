---
name: bluefin-ci
description: How CI/CD works for the Bluefin codebase. Covers the GitHub Actions workflow ([`.github/workflows/bluefin.yml`](../../.github/workflows/bluefin.yml)), the throughput-regression `bench (macos-latest)` job (driver script [`bench_ci.sh`](../../bench_ci.sh), floors, sticky PR comments, log artifacts), the env-var knobs that govern the gate, and the ratchet protocol for tightening floors as performance improves. Load whenever a task touches the workflow file, the bench gate, the comment formatting, the floor thresholds, or asks "why did CI fail?" / "how do I bump CI to require more throughput?". Pair with `bluefin-performance` for the *measurement* side (what the numbers mean, the canonical baseline).
---

# Bluefin CI

End-to-end documentation of the Bluefin GitHub Actions pipeline, with primary focus on the throughput regression gate.

## Workflow overview

[`.github/workflows/bluefin.yml`](../../.github/workflows/bluefin.yml) defines four jobs, all triggered on push-to-main and PR-to-main:

| Job | Runs on | Purpose |
|-----|---------|---------|
| `build` | ubuntu-latest, macos-latest | `cargo build` + `cargo test`, plus a second pass with `--features macos-fast` on macOS only |
| `coverage` | ubuntu-latest | `cargo llvm-cov` → upload to Codecov |
| `kani` | ubuntu-latest | Model-checking via `model-checking/kani-github-action@v1.1` |
| `bench (macos-latest)` | macos-latest | **Throughput regression gate. The rest of this doc.** |

## The `bench (macos-latest)` job

The `bench (macos-latest)` job runs [`bench_ci.sh`](../../bench_ci.sh), which drives [`bench_two_process.sh`](../../bench_two_process.sh) `N_RUNS` times at `N_CONNS` connections each, parses the per-conn `(#X) FINAL: …` lines from each successful attempt's `server.log`, aggregates the stats, compares them against env-var floors, and produces:

1. **stdout** with per-conn verdicts and an aggregate block — visible in the Actions log;
2. **a Markdown summary** at `$BLUEFIN_BENCH_SUMMARY_MD` (default `bench_logs/ci_summary.md`) — used downstream;
3. **`bench_logs/`** preserved as an upload artifact (14-day retention).

The workflow then:

- Appends the Markdown to `$GITHUB_STEP_SUMMARY` so it shows on the run page.
- On `pull_request` events, posts/updates a sticky PR comment via [`marocchino/sticky-pull-request-comment@v2`](https://github.com/marocchino/sticky-pull-request-comment), keyed by `header: bluefin-bench`, so subsequent pushes **edit** the same comment instead of spamming the PR.
- Uploads `bench_logs/` as the `bench-logs` artifact (always, even on failure).
- Re-exits with the captured `bench_ci.sh` rc as the final step, so the job fails on regression *after* the comment has been posted.

The script runs under `set +e` for the bench step itself; the rc is captured into `steps.bench.outputs.rc` and re-exported in the trailing `Fail job on regression` step. This ordering is deliberate — without it, a regression would skip the comment and the artifact upload.

## Env-var knobs

All knobs are read from the environment by [`bench_ci.sh`](../../bench_ci.sh); set them in the workflow `env:` block, on the command line, or in your shell.

| Var | Default | What it does |
|-----|---------|--------------|
| `BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS` | `0.05` (script) / `0.05` (workflow) | Minimum mean of per-conn `avg gb/s` across **GOOD conns only** (filtered — see "Drain artifacts" below). Below this → FAIL. |
| `BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS` | `0.10` (script) / `1.00` (workflow) | Minimum of the *maximum* observed `peak gb/s` across **GOOD conns only** (filtered). Below this → FAIL. |
| `BLUEFIN_BENCH_FLOOR_GOOD_CONNS`    | `6` (script) / `5` (workflow) | Minimum count of "good conns" out of `N_RUNS * N_CONNS` total trials. Below this → FAIL. The threshold for "good" is `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES`. |
| `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` | `14000000000` (14 GB) script / `700000000` (700 MB) workflow | **Threshold for what counts as a "good conn".** A conn that emits a FINAL line is GOOD if it delivered ≥ this many bytes, else TRUNC. Used for the good-conns count above. **Rebaselined for CI** — see below. |
| `BLUEFIN_NUM_READER_WORKERS` | `3` (server bin default) / `2` (workflow) | Number of `ReaderTxChannel` workers bound to the server's listening UDP socket. Reader workers demux datagrams to the right `ConnectionBuffer` and are only on the hot path during handshake; steady-state data flows through the per-conn socket. Default 3 suits dev boxes; CI lowers to 2 because hosted macos-latest has only 3 vCPUs (shared by server + client + tokio worker threads), and 3 reader workers oversubscribed the runner enough to amplify the bimodal-allocation pattern. 2 keeps a touch of demux parallelism for the bench's 2 concurrent handshakes without piling threads over the vCPU count. Production unaffected unless explicitly set. |
| `BLUEFIN_BENCH_SUMMARY_MD` | `bench_logs/ci_summary.md` | Path the script writes the Markdown summary to. The workflow pins it so the comment step can find it. |
| `BLUEFIN_SOCKET_RCVBUF` | unset (production) / `8388608` (8 MiB, workflow) | Per-socket `SO_RCVBUF` override read by [`BluefinSocket::new`](../../bluefin-io/src/socket/udp_socket.rs). When unset, falls back to the hardcoded **512 KiB** default — see "Socket buffer sizes" below. |
| `BLUEFIN_SOCKET_SNDBUF` | unset (production) / `8388608` (8 MiB, workflow) | Per-socket `SO_SNDBUF` override, same semantics as RCVBUF. Set in CI to keep send/recv symmetric on the loopback bench. |
| `BLUEFIN_NUM_SENDS` | unset (production = `10_000_000`) / `500000` (workflow) | Per-conn payload-loop iteration count read by [`bluefin/src/bin/client.rs`](../../bluefin/src/bin/client.rs). Each iteration sends a 1500 B `Bytes`, so this controls total per-conn payload size: `NUM_SENDS × 1500 B`. CI ships 750 MB instead of the 15 GB dev default — see "Payload size and idle-timeout" below. |
| `BLUEFIN_RECV_IDLE_TIMEOUT_SECS` | unset (production = `2`) / `20` (workflow) | Server's per-conn recv-idle deadline in seconds, read once at startup by [`bluefin/src/bin/server.rs`](../../bluefin/src/bin/server.rs). Bluefin has no protocol FIN; this is how the bench server decides a peer is gone. CI bumps to 20 s so slow-but-progressing CI conns can finish the (shrunk) payload before the deadline TRUNCs them. The server's FINAL `avg gb/s` subtracts this tail from the divisor so the reported number is bytes / actual-transfer-time. |
| `BLUEFIN_BENCH_RUN_RETRIES_ON_ZERO` | `0` (script) / `2` (workflow) | Maximum extra attempts per run when the run yields **0** GOOD conns. Catastrophic 0-good runs on hosted CI are almost always runner-allocation noise (CPU starvation, noisy neighbour); 2 retries make the post-retry catastrophic rate well under 0.1 %. Only the final attempt's data is committed; earlier attempts' logs stay on disk under their `attempt_*` directories but are NOT parsed. |
| `BLUEFIN_BENCH_RUN_RETRIES_ON_PARTIAL` | `0` (script) / `0` (workflow) | Maximum extra attempts per run when the run yields **partial** GOOD conns (1 ≤ good < N). Default 0 because the conns that succeeded are real signal worth keeping; retrying would discard the diagnostic value of seeing GOOD-alongside-TRUNC. Bump only if you specifically want to suppress partial outcomes. |
| `BLUEFIN_BENCH_RUN_RETRIES` | `0` | **Legacy single-knob fallback.** Used as the default for both ON_ZERO and ON_PARTIAL when neither is set explicitly. Kept for shell-override convenience; new code should set the asymmetric knobs directly. |
| `BLUEFIN_BENCH_RUN_RETRY_BACKOFF_SECS` | `5` | Sleep between attempts when retrying. 5 s is enough to let the runner exit any short scheduling-burst window without dragging out the wall-clock on actually-degraded code. |

### `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` in detail

Each client sends exactly **`NUM_SENDS × 1500 B + handshake bytes`**. With the production default `NUM_SENDS = 10 000 000`, that's ~15 GB per conn (~10 s on local Apple-silicon, never completes on hosted runners). With the CI workflow override `BLUEFIN_NUM_SENDS=500000`, that's ~750 MB per conn — sized so hosted runners can actually finish it inside the 10 s recv-idle window.

The gate distinguishes "made real progress" from "starved out" by checking each FINAL line's byte count against `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES`. Both GOOD and TRUNC conns contribute their `avg`/`peak` numbers to the aggregate stats — the threshold only gates the good-conn count.

**Two operating points**:

- **Local (script default 14 GB)**: ≈ 93 % of the 15 GB local target. Filters out clear truncations while allowing tiny rounding under-shoots. A TRUNC conn locally typically delivers 5–8 GB.
- **Hosted CI (workflow override 700 MB)**: ≈ 93 % of the 750 MB CI target. Same semantic as local — "good" means "delivered ≈ the full payload". Conns that stall mid-flight (typical hosted-runner contention) get TRUNCed and excluded from `good_conns`.

**When to tune the local default (14 GB)**:

- **Lower** (e.g. `12000000000`) only if dev-box truncations become the norm and you want to ratchet the local run-quality bar down.
- **Raise** (e.g. `14900000000`) to require ≥ 99.3 % of payload delivered if you're investigating partial-delivery regressions.

**When to tune the CI override (700 MB)**:

- **Lower** if a runner-image change makes 700 MB unreachable for the typical conn (check the per-run table; if conn 0 routinely delivers <700 MB, the floor is the wrong shape for the new runner). Always tune in lock-step with `BLUEFIN_NUM_SENDS` so the threshold stays at ~93 % of the payload.
- **Raise** if hosted runners get faster and you bump `BLUEFIN_NUM_SENDS` to widen the dynamic range.

Do **not** lower the local default below ~10 GB — at that point you're letting through truly broken runs.

### Payload size and idle-timeout (`BLUEFIN_NUM_SENDS` / `BLUEFIN_RECV_IDLE_TIMEOUT_SECS`)

The two CI-only knobs ship together because they fix a shared structural mismatch: **the production-default 15 GB payload + 2 s recv-idle is incompatible with hosted-macos-latest throughput**. Active conns on hosted runners deliver ~50–300 MB/s sustained — so 15 GB needs 50–300 s, but the server gives up after 2 s of stalled recv. Every CI conn TRUNCated, mean_avg got diluted by the idle tail, and `good_conns` degenerated into a smoke test for "did *any* conn pump bytes for >100 MB before the timeout".

Option C (this stack) shrinks both axes proportionally so CI behaves like dev:

| Knob | Dev default | CI override | Why |
|------|-------------|-------------|-----|
| `BLUEFIN_NUM_SENDS` | `10_000_000` | `500_000` | 15 GB → 750 MB. Active conns on hosted runners finish 750 MB in 2.5–15 s of real transfer time. |
| `BLUEFIN_RECV_IDLE_TIMEOUT_SECS` | `2` | `20` | 2 s → 20 s. Tolerates multi-second scheduling stalls observed on the worst hosted-runner allocations (5-10 s mid-stream stalls observed in practice). The timeout only fires when nothing arrives, so healthy runs are unaffected. |

With Option C, `good_conns` returns to its original semantic ("delivered the full payload") and the floor of 5/10 becomes a **real liveness gate** rather than a smoke test.

**Server FINAL avg correction**: `bluefin/src/bin/server.rs` subtracts `recv_idle` from `now.elapsed()` when computing the FINAL avg, so a conn that delivered 750 MB in 2.5 s of real transfer + 10 s idle reports `avg = 750 MB / 2.5 s = 300 MB/s`, not `750 MB / 12.5 s = 60 MB/s`. Without this correction, bumping the timeout would have dragged mean_avg into the noise floor (see commit history for the Option C rollout).

**When to re-baseline**:

- **Lower** `BLUEFIN_NUM_SENDS` if hosted-runner throughput drops further (e.g. macOS image rev). Shrink `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` proportionally (~93 % of new target).
- **Raise** `BLUEFIN_NUM_SENDS` to widen the burst window if hosted runners get faster (more iterations → more opportunities for the server's print-cadence to catch a peak).
- **Raise** `BLUEFIN_RECV_IDLE_TIMEOUT_SECS` if `bench (macos-latest)` starts showing a wave of TRUNC conns at 20 s but no actual perf regression. Keep `bench_two_process.sh`'s server-grace in sync (it auto-derives `recv_idle + 3` seconds).
- Don't unset either env var in CI — falling back to dev defaults will TRUNC every conn and FAIL the gate.

### Socket buffer sizes (`BLUEFIN_SOCKET_RCVBUF` / `BLUEFIN_SOCKET_SNDBUF`)

[`BluefinSocket::new`](../../bluefin-io/src/socket/udp_socket.rs) reads these env vars at socket-creation time and passes the parsed value to `setsockopt(SO_{RCV,SND}BUF)`. **Production traffic is unaffected unless the env vars are explicitly set** — the code falls back to the historical 512 KiB hardcoded default.

**Why CI overrides this**: at 1.5 GB/s burst rate, a 512 KiB buffer drains in ~340 µs. On a contended 3-vCPU hosted runner, tokio scheduling gaps routinely exceed 1–10 ms. The kernel UDP buffer overflows, packets drop, bluefin's reliability layer can't catch up before the server's 2 s recv-idle timeout fires — hence the bilateral-failure pattern documented above. **8 MiB buys ~5 ms of preemption tolerance**, enough to absorb typical hosted-runner contention.

**Platform caps** (the kernel silently clamps requests above the cap):

| Platform | Cap | Sysctl | Realistic ceiling without root |
|----------|-----|--------|-------------------------------|
| macOS | `kern.ipc.maxsockbuf` | default ~8 MiB on hosted runners | **8 MiB** |
| Linux | `/proc/sys/net/core/{rmem,wmem}_max` | default ~212 KiB on most distros | ~212 KiB (use `SO_*BUFFORCE` + `CAP_NET_ADMIN` to bypass; not viable in stock CI) |

This is why the Linux `build` job doesn't run the throughput bench — without sysctl tweaks, Linux runners can't get a buffer big enough to make the gate meaningful.

**Validation**: invalid values (negative, zero, non-numeric, empty, > `i32::MAX`) silently fall back to the 512 KiB default. The helper is a one-liner in [`udp_socket.rs`](../../bluefin-io/src/socket/udp_socket.rs); if you need to debug what the kernel actually granted, call `getsockopt(SO_RCVBUF)` after construction (Linux returns 2× the requested size; macOS returns the requested size verbatim).

**When to tune**:

- **Lower** if a future macOS runner generation reduces `kern.ipc.maxsockbuf` (the kernel will silently clamp anyway, but explicit is better than implicit). Check the runner OS image release notes.
- **Raise** only after verifying via sysctl that the kernel will honour it. 16 MiB might be unlocked on some runner images; 32+ MiB needs a sysctl bump.
- **Unset** to fall back to 512 KiB — useful for reproducing a production-fidelity run locally.

## Current floors and where they came from

The workflow ships these floors:

| Floor | Value | Origin |
|-------|-------|--------|
| `mean avg gb/s` (good-conns only) | **0.40** | ~50 % of the first observed CI median (0.92 GB/s @ 5 runs × 2 conns, 2026-05-11). Detects \~50 % throughput regressions on the GOOD-conn population. |
| `max peak gb/s` (good-conns only) | **2.00** | ~50 % of the first observed CI median (4.15 GB/s @ 5 runs × 2 conns, 2026-05-11). **Regression signal for burst rate on conns that actually transferred data.** |
| `good conns` (≥ 700 MB each) | **6** of 10 | Liveness gate. Hosted-runner allocation is bimodal-with-tail: a "normal" allocation pumps 8-10/10 conns, a "bad" allocation can starve all 10. The bench script auto-retries any run that yields 0 GOOD conns (see `BLUEFIN_BENCH_RUN_RETRIES=1` in the workflow), which absorbs the catastrophic-allocation tail. Floor of 6/10 is the lower bound observed across post-retry runs. |

Floors 1 and 2 are pegged at ~50 % of the LOWER BOUND observed across multiple CI runs. Floor 3 is the real-regression detector held below its current empirical lower bound for noise tolerance. **Real perf signal still lives in local sweeps**, but the CI gate is now meaningful enough to catch a sub-50 % throughput regression or a complete liveness failure.

## Runner reality and "drain artifacts"

**Hosted `macos-latest` runners are dramatically slower and burstier than typical Apple-silicon dev hardware.** Two pathologies dominate the measurement, both exclusive to STARVED conns:

### Pathology 1: bilateral failure dominates

Per 2026-05-11 CI runs:

| Signal | Local (M-series) | Hosted (macos-latest) |
|--------|------------------|-----------------------|
| good conns @ 100 MB | 10 / 10 | 1–5 / 10 (per-runner variance) |
| active-conn avg gb/s | 1.5–2.0 | 0.10–0.30 |
| starved-conn avg gb/s | n/a | 0.003–0.030 (tail-diluted; see below) |
| starved-conn bytes | n/a | 6–60 MB (out of 15 GB target) |

The runner has 3 vCPUs but the server runtime spins 3 reader workers + 2 conn processors + accept = 6+ tasks. Under tokio's cooperative scheduling, the second conn often starves at handshake-burst time and never recovers; the kernel UDP recv buffer overflows, packets drop, bluefin's retransmit can't keep up, and the 2 s recv-idle timeout fires.

### Pathology 2: drain artifacts inflate `peak`

When the server task is preempted for tens of ms while the client pumps at 1+ GB/s, the kernel UDP socket buffer fills with multi-MB of pending data. When the task resumes, it drains the buffer at memcpy speed:

```
recv_bytes returns instantly (data pre-buffered)
  -> 3500 iterations complete in microseconds
    -> bytes_since_last_print / time_since_last_print = MBs / µs
      -> peak recorded as 100+ GB/s
```

That's kernel-to-userspace memcpy rate, **not network rate**. Empirical example: a starved conn with 60 MB total delivered showing peak 148 GB/s.

Drain artifacts ALWAYS come from starved conns by construction (the active conn never builds up a multi-MB backlog because it drains the socket as fast as the wire delivers). So `bench_ci.sh` filters BOTH `mean_avg` AND `max_peak` to GOOD conns only (delivered ≥ `GOOD_CONN_MIN_BYTES`) for the gate. The unfiltered `raw_*` numbers are still reported in the PR comment for visibility into runner contention — a large gap between filtered and raw is a runner-health signal, not a code signal.

### Pathology 3: starved-conn `avg` is structurally tail-diluted

The server's FINAL line reports `bytes / elapsed`, where `elapsed` is computed as `now.elapsed() - recv_idle` (clamped at 1 ms). Pre-Option-C, the subtraction wasn't there: `elapsed` included the full 2 s tail and the displayed avg was diluted accordingly (60 MB delivered in 0.6 s active recv → reported `60 / 2.6 = 23 MB/s`, not the 100 MB/s sustained rate). The fix landed when CI bumped `BLUEFIN_RECV_IDLE_TIMEOUT_SECS` from 2 s upward (currently 20 s) — without it, the dilution would have been 10×+ worse and dragged mean_avg into the noise floor.

**Why the fix is safe for production**: the subtraction is approximate (the actual gap between last byte and idle-fire is `recv_idle ± IDLE_RESET_EVERY-iter-time`), but the error is bounded by tens of microseconds at hot-loop rates — dwarfed by the 2 s timeout it's correcting for. Production logs go from "slightly under-reports avg" to "reports avg accurately".

Filtering to GOOD conns only (≥ `GOOD_CONN_MIN_BYTES` delivered) is still the right gate strategy because TRUNC conns deliver too few bytes for *any* avg to be meaningful, regardless of whether the divisor is corrected.

## Ratcheting up after a perf gain

After a confirmed gain (typically: a successful round of optimisations recorded in `bluefin-performance`'s round table, with a fresh local sweep), tighten the floors:

1. Run the 5-run CI bench (push a no-op PR) and read the GREEN `bench (macos-latest)` summary. Look at the `(good only)` columns, NOT the `raw` columns.
2. Compute the median of `max peak gb/s (good only)` across the 5 runs.
3. Multiply by the **current ratchet factor (0.50)** and round to 2 decimal places. Update `BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS`.
4. **Do NOT** ratchet `mean_avg` based on local numbers — hosted runners can't reach them. Bump only if the CI median of `mean avg gb/s (good only)` has demonstrably risen across multiple PRs.
5. For `good_conns`: take the LOWEST observation of `good conns` across the recent runs and drop one for headroom. Don't use the mean — runner allocation variance is too high; the lowest is the real lower bound.
6. Open a PR with the floor bump. The `bench (macos-latest)` job will run against its own new floors — if the PR is purely a floor bump, it should PASS comfortably.

Do **not** ratchet `max_peak` using the *max*, only the *median* across runs. Peak is high-variance.

## When CI fails: the fallback ladder

Hosted `macos-latest` runners are slower and noisier than typical Apple-silicon dev hardware. If the gate flakes, work down this ladder *before* disabling the gate:

1. **Check the `raw` columns.** If `raw max peak gb/s` is huge (>10 GB/s) but `max peak gb/s (good only)` is at floor, that's a runner-contention signal, not a code regression. The drain-artifact filter is doing its job; this iteration of the runner just allocated badly.
2. **Re-run the workflow.** Hosted runners have high allocation variance; a single bad run is not a regression. If 2 of 3 re-runs PASS, this was runner luck.
3. **Investigate `max_peak` failures.** If `max peak gb/s (good only)` < 1.00 across multiple consecutive runs, that's a real burst-rate regression. Look at the per-conn FINAL lines.
4. **Investigate `mean_avg` failures.** If `mean avg gb/s (good only)` < 0.05, your active conn is delivering bytes but slowly. Combined with peak still passing, this means the active conn started fast then collapsed — look for a long-tail issue (allocator pressure, GC stalls, congestion-control bug).
5. **Investigate `good_conns` failures.** Floor is already 1 (smoke test). If you're tripping it, NO conn anywhere delivered 100 MB across 10 trials. That's catastrophic — either real regression or runner outage. Don't lower below 1.

If you reach step 5 with no obvious regression cause, the runner class may have changed (GitHub occasionally migrates runner image generations). Compare against the SHA of the previous green run; if the runner image changed, rebaseline floors.

## Anatomy of `bench_ci.sh`

The driver script is a single bash file at the repo root. Key sections, with line refs into [`bench_ci.sh`](../../bench_ci.sh):

- **arg parsing**: `-r/--runs`, `-n/--num-conns`, `--skip-build`, `--retry`, `-t/--timeout`. CLI args override env-var floors? No — they don't; env vars are the only way to set floors. CLI is for shape (runs/conns/timeout) only.
- **per-run loop**: invokes `./bench_two_process.sh -n $N_CONNS -t $CLIENT_TIMEOUT --retry $RETRY --skip-build`, captures stdout, greps the `[log-dir]` marker line that `bench_two_process.sh` prints, picks the *last* `attempt_*/server.log` (handshake-race retries produce earlier `attempt_*` dirs that are not the truth).
- **FINAL line parsing**: regex `^\(#[0-9]+\) FINAL:` against `server.log`; per-line `awk` extracts `bytes`, `avg`, `peak`. Conns with bytes ≥ floor are counted as good; both contribute to `ALL_AVGS` / `ALL_PEAKS`.
- **aggregate**: `awk` for mean/min/max — no `bc`/`python` dependency on the runner.
- **gate**: three pass/fail comparisons against the floors; collects failure messages.
- **Markdown emission**: writes header + config line + aggregate table + per-run table + (conditional) failures section + footnote, to `$BLUEFIN_BENCH_SUMMARY_MD`.
- **exit**: 0 on PASS, 1 on FAIL, 2 on harness error (no FINAL lines parsed, log-dir missing).

### The `[log-dir]` contract

[`bench_two_process.sh`](../../bench_two_process.sh) prints exactly one line of the form:

```
[log-dir] bench_logs/<timestamp>
```

`bench_ci.sh` greps this with `grep -m1 '^\[log-dir\] '`. **Do not break this format** — it's the only stable way to find each run's log dir without racing `ls -t bench_logs/`. If you change `bench_two_process.sh`'s log-dir layout, update the parser in `bench_ci.sh` accordingly.

## The PR comment

Sticky-comment integration uses [`marocchino/sticky-pull-request-comment@v2`](https://github.com/marocchino/sticky-pull-request-comment), with `header: bluefin-bench` as the dedup key. The action uses the built-in `GITHUB_TOKEN` (no secrets) but requires `pull-requests: write` in the job's `permissions:` block, which the workflow declares.

The Markdown body the script emits looks like:

```markdown
## Bluefin throughput bench :white_check_mark: PASS

**Config:** 5 run(s) × 2 conn(s) on `Darwin arm64`

### Aggregate

| metric         | observed | floor | result             |
| ---            |    ---:  |  ---: | :---:              |
| mean avg gb/s (good only) | 1.905 | 0.05 | :white_check_mark: |
| min  avg gb/s  |   1.840  |    —  | —                  |
| mean peak gb/s |   3.947  |    —  | —                  |
| max  peak gb/s (good only) | 3.970 | 1.00 | :white_check_mark: |
| good conns     | 4 / 4    |    1  | :white_check_mark: |

### Per-run
…

### Failures   (only present on FAIL)
…
```

Each gated metric gets its own `:white_check_mark:` / `:x:` cell, so reviewers can tell *which* floor was missed at a glance. The Failures section appears only on FAIL.

## Common tasks

| Task | What to do |
|------|------------|
| **Run the gate locally** | `./bench_ci.sh -r 5 -n 2 --skip-build` (after a `cargo build --release --bin server --bin client`). Takes ~1 min. |
| **Reproduce the workflow exactly** | Same as above plus `BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS=0.40 BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS=2.00 BLUEFIN_BENCH_FLOOR_GOOD_CONNS=5 BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES=700000000 BLUEFIN_SOCKET_RCVBUF=8388608 BLUEFIN_SOCKET_SNDBUF=8388608 BLUEFIN_NUM_SENDS=500000 BLUEFIN_RECV_IDLE_TIMEOUT_SECS=20 BLUEFIN_NUM_READER_WORKERS=2 BLUEFIN_BENCH_RUN_RETRIES_ON_ZERO=2 BLUEFIN_BENCH_RUN_RETRIES_ON_PARTIAL=0 BLUEFIN_BENCH_RUN_RETRY_BACKOFF_SECS=5 BLUEFIN_BENCH_SUMMARY_MD=/tmp/x.md ./bench_ci.sh -r 5 -n 2 --skip-build`. |
| **Force a FAIL to test the comment** | `BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS=10.0 ./bench_ci.sh -r 2 -n 2 --skip-build`. Useful when changing the Markdown emitter. |
| **Bump floors after a perf gain** | See "Ratcheting up" above. Edit only `.github/workflows/bluefin.yml`'s `env:` block. |
| **Inspect a CI failure** | Open the run → `bench (macos-latest)` job → expand `Run CI bench`. The Markdown also lands in the run's Summary tab and on the PR. The full per-run logs are in the `bench-logs` artifact. |
| **Loosen the gate temporarily** | Set the env var in the workflow to a lower value, with a comment pointing back to the issue tracking the regression. Don't disable the job. |

## What this gate does *not* catch

- **Latency regressions.** The bench is purely throughput. Adding a handshake or per-send latency assertion is a future skill section.
- **Allocation regressions.** A version with `Bytes::clone()` everywhere can still hit the gb/s floors if the runner happens to schedule favourably. Use the local flamegraph workflow in `bluefin-performance` for alloc-shape changes.
- **Linux performance regressions.** The gate is macOS-only because that's where the local dev signal lives. A Linux gate would need its own baseline measurement.
- **N>2 contention regressions.** The known live bottlenecks #6/#11 surface only at N≥3; running the gate at N=2 deliberately stays inside the well-behaved regime.

If you need any of these, run the relevant workflow locally and post the result on the PR by hand for now.
