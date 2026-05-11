---
name: bluefin-ci
description: How CI/CD works for the Bluefin codebase. Covers the GitHub Actions workflow ([`.github/workflows/bluefin.yml`](../../.github/workflows/bluefin.yml)), the throughput-regression `bench-macos` job (driver script [`bench_ci.sh`](../../bench_ci.sh), floors, sticky PR comments, log artifacts), the env-var knobs that govern the gate, and the ratchet protocol for tightening floors as performance improves. Load whenever a task touches the workflow file, the bench gate, the comment formatting, the floor thresholds, or asks "why did CI fail?" / "how do I bump CI to require more throughput?". Pair with `bluefin-performance` for the *measurement* side (what the numbers mean, the canonical baseline).
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
| `bench-macos` | macos-latest | **Throughput regression gate. The rest of this doc.** |

## The `bench-macos` job

The bench-macos job runs [`bench_ci.sh`](../../bench_ci.sh), which drives [`bench_two_process.sh`](../../bench_two_process.sh) `N_RUNS` times at `N_CONNS` connections each, parses the per-conn `(#X) FINAL: …` lines from each successful attempt's `server.log`, aggregates the stats, compares them against env-var floors, and produces:

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
| `BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS` | `0.05` (script) / `0.05` (workflow) | Minimum mean of per-conn `avg gb/s` across all conn-trials. Below this → FAIL. On hosted runners, this is a noise floor (just non-zero). |
| `BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS` | `0.10` (script) / `1.00` (workflow) | Minimum of the *maximum* observed `peak gb/s`. Below this → FAIL. **This is the primary regression signal on hosted runners** — see "Runner reality" below. |
| `BLUEFIN_BENCH_FLOOR_GOOD_CONNS`    | `6` (script) / `4` (workflow) | Minimum count of "good conns" out of `N_RUNS * N_CONNS` total trials. Below this → FAIL. The threshold for "good" is `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES`. |
| `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` | `14000000000` (14 GB) script / `100000000` (100 MB) workflow | **Threshold for what counts as a "good conn".** A conn that emits a FINAL line is GOOD if it delivered ≥ this many bytes, else TRUNC. Used for the good-conns count above. **Rebaselined for CI** — see below. |
| `BLUEFIN_BENCH_SUMMARY_MD` | `bench_logs/ci_summary.md` | Path the script writes the Markdown summary to. The workflow pins it so the comment step can find it. |

### `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` in detail

Each client sends exactly **15 000 000 119 bytes** (10 M × 1500 B + handshake bytes). On local Apple-silicon this completes in ~10 s; on hosted runners it does not.

The gate distinguishes "made real progress" from "starved out" by checking each FINAL line's byte count against `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES`. Both GOOD and TRUNC conns contribute their `avg`/`peak` numbers to the aggregate stats — the threshold only gates the good-conn count.

**Two operating points**:

- **Local (script default 14 GB)**: ≈ 93 % of the 15 GB target. Filters out clear truncations while allowing tiny rounding under-shoots. A TRUNC conn locally typically delivers 5–8 GB.
- **Hosted CI (workflow override 100 MB)**: cleanly separates the bilateral-failure populations on hosted runners. Empirically (2026-05-11): the "active" conn delivers 300–800 MB per run; the "starved" conn delivers 6–60 MB. 100 MB sits in the gap. This makes `good_conns` mean "at least N runs had at least one peer making real progress", which is the right CI question.

**When to tune the local default (14 GB)**:

- **Lower** (e.g. `12000000000`) only if dev-box truncations become the norm and you want to ratchet the local run-quality bar down.
- **Raise** (e.g. `14900000000`) to require ≥ 99.3 % of payload delivered if you're investigating partial-delivery regressions.

**When to tune the CI override (100 MB)**:

- **Lower** if a runner-image change makes 100 MB unreachable for the active conn (check the per-run table; if conn 0 routinely delivers <100 MB, the floor is the wrong shape for the new runner).
- **Raise** if hosted runners get faster and the active conn reliably clears 500 MB — that lets you tighten the meaningful-progress bar.

Do **not** lower the local default below ~10 GB — at that point you're letting through truly broken runs.

## Current floors and where they came from

The workflow ships these floors:

| Floor | Value | Origin |
|-------|-------|--------|
| `mean avg gb/s` | **0.05** | Noise floor on hosted runners (observed CI median ~0.10). Just guards against catastrophic regressions. |
| `max peak gb/s` | **1.00** | ~50 % of CI-observed median peak (~2.40 GB/s on 2026-05-11 run). **Primary regression signal.** |
| `good conns`    | **4** of 10 (≥ 100 MB each) | 1-trial headroom over the 5/10 observed on 2026-05-11. "Good" is rebaselined to 100 MB delivered (vs 14 GB locally) so the gate distinguishes "made real progress" from "starved out" on hosted runners. |

## Runner reality

**Hosted `macos-latest` runners are dramatically slower and burstier than typical Apple-silicon dev hardware.** Empirical data from the first CI run (2026-05-11):

| Signal | Local (M-series) | Hosted (macos-latest) | Ratio | Useful as gate? |
|--------|------------------|-----------------------|-------|-----------------|
| `max peak gb/s` | 3.85 | 3.65 | ~95 % | **YES** — burst rate is comparable, code regressions surface here |
| `mean avg gb/s` | 1.82 | 0.094 | ~5 % | NO — swamped by VM scheduling jitter |
| good conns @ 14 GB | 9 / 10 | 0 / 10 | — | NO — the runner can't sustain long enough to deliver 14 GB |
| good conns @ 100 MB | 10 / 10 | 5 / 10 | — | **YES** — distinguishes "active" conn (300–800 MB) from "starved" conn (6–60 MB) |
| peak / mean ratio | ~2× | ~25× | — | runner is extremely bursty |

**Bilateral failure is the steady-state on hosted runners**: per the 2026-05-11 CI data, conn 0 delivers 300–800 MB per run while conn 1 stalls at 6–60 MB. This is the same contention pattern documented in `bluefin-performance` for local under-provisioned runs, just always-on for the hosted runner class. The 100 MB threshold sits in the empirical gap between the two populations, so `good_conns` answers a real question: "did at least one peer per run move serious bytes, or is the code so broken that nothing is moving?"

The takeaway: **`max_peak` is the strongest signal, `good_conns` (rebaselined) is a meaningful liveness gate, `mean_avg` is just a noise floor**.

## Ratcheting up after a perf gain

After a confirmed gain (typically: a successful round of optimisations recorded in `bluefin-performance`'s round table, with a fresh local sweep), tighten the floors:

1. Run the 5-run CI bench (push a no-op PR) and read the GREEN `bench-macos` summary.
2. Read the **median** `max peak gb/s` across the 5 runs.
3. Multiply by the **current ratchet factor (0.50)** and round to 2 decimal places. Update `BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS`.
4. **Do NOT** ratchet `mean_avg` based on local numbers — hosted runners can't reach them. Bump only if the CI median has demonstrably risen.
5. For `good_conns`: count how many conn-trials cleared `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` across the recent runs. If consistently ≥ `N_RUNS + k` for `k ≥ 1`, raise the floor to `N_RUNS + (k - 1)` (always leave 1-trial headroom). If hosted runners get faster and the active conn reliably clears a higher byte count, also raise `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` (e.g. 100 MB → 500 MB).
6. Open a PR with the floor bump. The bench-macos job will run against its own new floors — if the PR is purely a floor bump, it should PASS comfortably.

Do **not** ratchet `max_peak` using the *max*, only the *median* across runs. Peak is high-variance.

## When CI fails: the fallback ladder

Hosted `macos-latest` runners are slower and noisier than typical Apple-silicon dev hardware. If the gate flakes after a floor bump, work down this ladder *before* disabling the gate:

1. **Lower `BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS` in 0.20 steps**. Stay at 50 % of the new observed CI median.
2. **Investigate `mean_avg` failures.** If mean_avg trips at 0.05, that's not noise — it's a real regression. Look at the per-conn FINAL lines in the artifact to see whether bytes were delivered at all.
3. **Investigate `good_conns` failures.** If `good_conns` trips at 4, look at the per-run table. Three patterns:
   - **Both conns starved across multiple runs** (all bytes < 100 MB): real regression. Don't lower the floor.
   - **Active conn delivers <100 MB**: the runner generation has changed. Lower `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` to bracket the new active-conn population (look at conn 0 bytes across runs, set the threshold half-way down).
   - **Active conn fine, fewer runs hitting it**: lower `BLUEFIN_BENCH_FLOOR_GOOD_CONNS` from 4 → 3 with a comment.

If you reach step 3 with no obvious regression cause, the runner class may have changed (GitHub occasionally migrates runner image generations). Compare against the SHA of the previous green run; if the runner image changed, rebaseline both `BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS` and `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` against the new median.

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
| mean avg gb/s  |   1.892  |  0.05 | :white_check_mark: |
| min  avg gb/s  |   1.820  |    —  | —                  |
| mean peak gb/s |   3.728  |    —  | —                  |
| max  peak gb/s |   3.940  |  1.00 | :white_check_mark: |
| good conns     | 10 / 10  |    4  | :white_check_mark: |

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
| **Reproduce the workflow exactly** | Same as above plus `BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS=0.05 BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS=1.00 BLUEFIN_BENCH_FLOOR_GOOD_CONNS=4 BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES=100000000` and `BLUEFIN_BENCH_SUMMARY_MD=/tmp/x.md`. |
| **Force a FAIL to test the comment** | `BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS=10.0 ./bench_ci.sh -r 2 -n 2 --skip-build`. Useful when changing the Markdown emitter. |
| **Bump floors after a perf gain** | See "Ratcheting up" above. Edit only `.github/workflows/bluefin.yml`'s `env:` block. |
| **Inspect a CI failure** | Open the run → `bench-macos` job → expand `Run CI bench`. The Markdown also lands in the run's Summary tab and on the PR. The full per-run logs are in the `bench-logs` artifact. |
| **Loosen the gate temporarily** | Set the env var in the workflow to a lower value, with a comment pointing back to the issue tracking the regression. Don't disable the job. |

## What this gate does *not* catch

- **Latency regressions.** The bench is purely throughput. Adding a handshake or per-send latency assertion is a future skill section.
- **Allocation regressions.** A version with `Bytes::clone()` everywhere can still hit the gb/s floors if the runner happens to schedule favourably. Use the local flamegraph workflow in `bluefin-performance` for alloc-shape changes.
- **Linux performance regressions.** The gate is macOS-only because that's where the local dev signal lives. A Linux gate would need its own baseline measurement.
- **N>2 contention regressions.** The known live bottlenecks #6/#11 surface only at N≥3; running the gate at N=2 deliberately stays inside the well-behaved regime.

If you need any of these, run the relevant workflow locally and post the result on the PR by hand for now.
