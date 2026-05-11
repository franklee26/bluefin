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
| `BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS` | `0.05` (script) / `0.91` (workflow) | Minimum mean of per-conn `avg gb/s` across all conn-trials. Below this → FAIL. |
| `BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS` | `0.10` (script) / `1.90` (workflow) | Minimum of the *maximum* observed `peak gb/s`. Below this → FAIL. |
| `BLUEFIN_BENCH_FLOOR_GOOD_CONNS`    | `6` (script) / `5` (workflow) | Minimum count of "good conns" out of `N_RUNS * N_CONNS` total trials. Below this → FAIL. |
| `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` | `14000000000` (14 GB) | **Threshold for what counts as a "good conn".** A conn that emits a FINAL line is GOOD if it delivered ≥ this many bytes, else TRUNC. Used for the good-conns count above. |
| `BLUEFIN_BENCH_SUMMARY_MD` | `bench_logs/ci_summary.md` | Path the script writes the Markdown summary to. The workflow pins it so the comment step can find it. |

### `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES` in detail

Each client sends exactly **15 000 000 119 bytes** (10 M × 1500 B + handshake bytes). A connection is "good" if it delivered the full payload before the server's 2 s recv-idle timeout fired; otherwise it's "truncated" — the bilateral-failure pattern that surfaces under sustained CPU contention.

The gate distinguishes these two cases by checking each FINAL line's byte count against `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES`. The default of **14 GB ≈ 93 % of the 15 GB target** filters out clear truncations while allowing tiny rounding under-shoots (a TRUNC conn typically delivers 5–8 GB, well below the threshold). Both GOOD and TRUNC conns contribute their `avg`/`peak` numbers to the aggregate stats — the threshold only gates the good-conn count.

**When to tune it**:

- **Lower** (e.g. `12000000000`) if hosted runners reliably truncate at 12 GB due to scheduler hiccups but otherwise hit reasonable throughput. This is the first lever in the fallback ladder before lowering the gb/s floors.
- **Raise** to be stricter — e.g. `14900000000` to require ≥ 99.3 % of payload delivered. Useful if you're investigating partial-delivery regressions.

Do **not** lower it below ~10 GB — at that point you're letting through truly broken runs and the gate stops being meaningful.

## Current floors and where they came from

The workflow ships these floors:

| Floor | Value | Origin |
|-------|-------|--------|
| `mean avg gb/s` | **0.91** | 50 % of local 20-run sweep median (1.82 GB/s) |
| `max peak gb/s` | **1.90** | 50 % of local 20-run sweep max-peak (3.85 GB/s) |
| `good conns`    | **5** of 10 | 50 % of `N_RUNS × N_CONNS = 5 × 2 = 10` trials |

Local sample (5-run sweep, post-O+F1+F2+G3+F3+P, 2026-05-11): mean avg 1.892 GB/s, max peak 3.940 GB/s, 10/10 good — every floor cleared with 2× headroom.

The 50 % rule is the **ratchet contract**: floors are always pegged at ~50 % of the most recent stable measurement, never higher. This is deliberately loose so the gate only fires on serious regressions and tolerates hosted-runner noise without flaking. Tighten by **lowering the divisor** (50 % → 60 % → 75 %) only after a sustained green streak that justifies the risk.

## Ratcheting up after a perf gain

After a confirmed gain (typically: a successful round of optimisations recorded in `bluefin-performance`'s round table, with a fresh local sweep), tighten the floors:

1. Run the 20-run local sweep that the perf SKILL describes (or the 5-run quick-look in this doc's "current floors" row).
2. Read the **median** mean-avg and the **median** (not max) peak. Median is more stable than max under runner noise.
3. Multiply each by the **current ratchet factor (0.50)** and round to 2 decimal places. Update the workflow env block.
4. Set `BLUEFIN_BENCH_FLOOR_GOOD_CONNS` to `ceil(0.50 * N_RUNS * N_CONNS)`. With the current `5×2 = 10`, that's **5**.
5. Open a PR. The bench-macos job will run against its own new floors — if the PR is purely a floor bump, it should PASS comfortably.

Do **not** ratchet on max-peak — peak is high-variance. Use the median peak and let max-peak sit well above the floor as natural headroom.

## When CI fails: the fallback ladder

Hosted `macos-latest` runners are slower and noisier than typical Apple-silicon dev hardware. If the gate flakes after a floor bump, work down this ladder *before* disabling the gate:

1. **Widen `BLUEFIN_BENCH_GOOD_CONN_MIN_BYTES`** (default 14e9 → try 12e9). Counts more partial deliveries as good. Usually the right answer when the runner is delivering reasonable throughput but truncating one or two conns under load.
2. **Drop the gb/s floors in 0.10 steps**. Stay at 50 % of the new observed median.
3. **Lower `BLUEFIN_BENCH_FLOOR_GOOD_CONNS`** from 5 → 4 (out of 10). Last resort — implies the runner is so noisy that bilateral reliability is genuinely <50 %.

If you reach step 3 and still flake, the runner class is wrong for this gate. Move it to a self-hosted runner or accept that the CI bench is best-effort signal.

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
| mean avg gb/s  |   1.892  |  0.91 | :white_check_mark: |
| min  avg gb/s  |   1.820  |    —  | —                  |
| mean peak gb/s |   3.728  |    —  | —                  |
| max  peak gb/s |   3.940  |  1.90 | :white_check_mark: |
| good conns     | 10 / 10  |    5  | :white_check_mark: |

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
| **Reproduce the workflow exactly** | Same as above plus `BLUEFIN_BENCH_FLOOR_MEAN_AVG_GBPS=0.91 BLUEFIN_BENCH_FLOOR_MAX_PEAK_GBPS=1.90 BLUEFIN_BENCH_FLOOR_GOOD_CONNS=5` and `BLUEFIN_BENCH_SUMMARY_MD=/tmp/x.md`. |
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
