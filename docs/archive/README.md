# Archived performance docs

These markdown files were merged into [`skills/bluefin-performance/SKILL.md`](../../skills/bluefin-performance/SKILL.md) on 2026-05-09. They are kept here for archaeology — every still-relevant claim has been verified against the current code and folded into the skill, but the original phrasing, rationale, and benchmark numbers may be useful when reviewing the history of a change.

The forward-looking backlog is in [`THROUGHPUT_ANALYSIS_2026.md`](../../THROUGHPUT_ANALYSIS_2026.md) at the workspace root.

## What's in here

| File | What it is | Status |
|------|-----------|--------|
| `OPTIMIZATIONS.md` | Commit-by-commit timeline 2.5 → 3.04 GB/s | Folded into skill "Historical timeline" |
| `PERFORMANCE_OPTIMIZATIONS.md` | Older HFT-focused write-up of optimizations 1–8 (sleep removal, AtomicU64, waker, MAX_BUFFER_SIZE, integer keys, pre-alloc, zero-copy, bytes infra) | All implementations verified; folded into skill "What's already implemented" |
| `THROUGHPUT_OPTIMIZATIONS.md` | Dec 2025 round (clone removal, socket buffers, carry-over) + future-work proposals | Implemented items folded; future items survive in `THROUGHPUT_ANALYSIS_2026.md` |
| `OPTIMIZATION_SUMMARY.md` | Near-duplicate of `THROUGHPUT_OPTIMIZATIONS.md` summary | Superseded |
| `ADDITIONAL_OPTIMIZATIONS.md` | Round 2 (split_off, extend+drain unsafe copy) + future opportunities (parking_lot, ArrayVec) | Implemented items folded; future items in `THROUGHPUT_ANALYSIS_2026.md` |
| `TIER_3_OPTIMIZATIONS.md` | VecDeque pre-alloc, lock scope tightening, deps added | Folded |
| `TIER_B_OPTIMIZATIONS.md` | Catalogue of attempted-but-regressed changes; "stop optimizing" verdict | Folded into skill "What NOT to retry" |
| `BINARY_RACE_CONDITIONS.md` | FIFO ordering, accept-before-spawn, client connection delay | All fixes are in code; folded into skill "Architecture" |
| `RACE_CONDITION_FIXES.md` | Shorter version of the above | Superseded |
