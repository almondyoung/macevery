# MacEvery Round 5 Status

Updated: 2026-10-19 (Asia/Shanghai). Implementation and verification in progress.

## Task capsule

- Project/host: MacEvery, this Mac, current checkout `main` at `eeac95e` (App source `d62aa1d`).
- Goal: reduce retained index memory while preserving short-query CPU, all result fields/scores/order and incremental freshness.
- Authorized: focused search/storage changes, isolated tests/benchmarks, verified App replacement, source and acceptance commit/push.
- Forbidden: rebuild live index; change roots/excludes/mode/shortcut/system permissions; affect other apps/services; publish real index/full search results; version bump/tag.
- Evidence: ignored `.build/performance-round5-20261019/`; previous rounds unchanged. Rollback `.build/MacEvery.rollback-20261019-before-round5.app`.

## Plan

1. Freeze source/App, verify remote/main, reuse frozen 7,226,486-row performance snapshot on isolated copies.
2. Share cold original-path directory prefixes; preserve contiguous lowercase search fields and allocation-free original-byte TopK ordering. Bound prefix pool and raw fallback; account actual capacities.
3. Full-field differential tests, directed prefix/delta tests, Debug/Release, format/strict Clippy/GUI checks.
4. Sequential original-nine and 29-short A/B/A, HTTP incremental/fallback/recovery, FSEvents; only accepted candidate proceeds.
5. Fresh live read-only backup, commit/push tested source, clean candidate App/signature/payload identity; native create/modify/rename/delete plus code search and final resource observation; commit/push/read back evidence.

## Gates

- Zero complete ordered-result digest mismatches; all record/score/order/update regressions pass.
- Preset performance check: short-suite and code paired CPU medians no more than +5%; investigate any common ASCII/Unicode term with repeated two-block regression over +10% and +5ms. Preserve all samples and failed attempts; only add a complete A/B/A block when unresolved variability warrants it.
- Same-input loaded RSS, lifetime RSS high-water and complete packed capacity are primary memory measures. Native point observation, physical footprint and capacity remain separate.
- No installation before source and controlled acceptance; native acceptance remains pending until actual visible interaction.

## Ownership

- search_memory_audit: `src/compact_index.rs`, `src/search.rs`, focused private unit tests.
- watch_cpu_audit: new `tests/round5_prefix_updates.rs`, independent public API directed delta oracle.
- gui_cpu_audit: read-only verification audit; later evidence cross-review.
- root: baseline, integration review, tools/report, sequential benchmarks, App and Git.

## Current evidence

- Controlled gates complete: Debug/Release114 unit +420-transaction +directed prefix integration; strict Clippy/format/Python/GUI; real original9 and short29 two-block A/B/A; mixed-case HTTP recovery/concurrency; 300k FSEvents A/B/A PASS.
- Full ordered real-result digests equal. Capacity2,328,585,118->1,537,288,452B (-34.0%); peakRSS2.502->1.537GiB (-38.6%). Coldcode194.12->175.51ms; shortgroup essentially unchanged; hotcode+2.1%, z+4.4%; loadingCPU+9..13%, HTTP lifecycleCPU+8% retained.
- Full report and sanitized evidence saved. Raw archived in ignored round directory. No production-source edits after frozen candidate hashes.
- Fresh read-only online backup7,303,844rows / quick_checkok / SHA3fb99b611f3a4217efb45b7ed3788c7b3c1bfa8bf17c1bf8e83a25e2a2014cb7. DBidentity c11e38f8128ce976ccb2acec808a7622; roots/excludes preserved for final comparison.
- Native oldApp settings verified gd62aa1d/Fastest/Command-Shift-Space/Limited; query cleared. Baseline20.081s: CPU0.09793% onecore/RSSsum2,707,488,768B. All exact three roles stable, no heavy benchmark during sample. Point observation, not controlled idle comparison.
- OldApp still running at original path; source commit/push, clean candidate build/install/native lifecycle/final observation pending.
- Setup/format/automatic review timeouts and sampling-helper adaptations retained in raw logs; valid checks/samples distinguish them from product failures.

## Next

Commit/push reviewed tested source and read back exact origin/main; build clean candidate, verify signature and normalized payload. Then replace only exact old processes, native18->35->beta-only->0 +code300, clear fixture/query, final20s roles-stable observation and docs commit/push/readback. No roots/excludes/mode/shortcut/permissions/index rebuild changes.
