# MacEvery Round 3 Status

Updated: 2026-10-18 (Asia/Shanghai). Implementation frozen; final checks and sequential benchmarks in progress.

## Task capsule

- Project: MacEvery; current main ec58990, clean at start.
- Goal: lower query CPU and avoid full packed reloads for bounded file changes while preserving complete results, ordering and index freshness.
- Lanes: code, correctness, sequential benchmark, local App acceptance.
- Allowed: Rust search/packed storage, DB change journal, server refresh; focused GUI/tool/report changes; replace only the verified local App after validation; commit and normal push authorized.
- Forbidden: rebuild live index, change roots/excludes/mode/shortcut, other apps/services, release tags or version bumps, publish real index copies/full search results.
- Host: this Mac only. Preserve round 1/2 evidence and current App rollback.

## Plan

1. Freeze exact baseline source/artifacts and existing real-data snapshot.
2. Independent designs for prepared query scoring, bounded DB change capture and transactional incremental service refresh; coordinate API before implementation.
3. Full-result differential and update/recovery/retention/compaction/concurrency regressions, Debug/Release/lint and GUI checks as applicable.
4. Sequential same-input A/B/A query CPU/RSS and HTTP update workload; preserve all samples and failures.
5. Commit/push verified source, clean build, preserve App/DB rollback, replace only verified App processes, native create/modify/rename/delete and resource sample, final evidence commit/push/readback.

## Ownership

- search_memory_audit: src/search.rs and src/compact_index.rs.
- watch_cpu_audit: src/db.rs and src/sqlite.rs.
- gui_cpu_audit: src/server.rs and tools/check_service_round3.py if needed.
- root: coordination, baseline/final builds, other tools, App, benchmarks, documentation and commit/push.

## Evidence

- Baseline archive: .build/performance-round3-20261018/baseline-src.
- Temporary work: /private/tmp/macevery-round3-20261018.
- Comparable snapshot: .build/performance-round1-20261017/snapshot.sqlite, 7,226,486 rows (frozen and retained).
- Runtime identities must be rechecked before replacement; previous PIDs are not authority.

## Next action

Finalize the kind-field payload guard identified in independent review, rerun final checks, then sequential same-input query/HTTP/watcher measurements. No live App replacement yet.

## First implementation checkpoint

- Baseline ec58990 confirmed equals origin/main (0 ahead/0 behind), new offline release binary/library built from frozen archive.
- Current App rollback preserved at .build/MacEvery.rollback-20261018-before-round3.app; original signatures/hashes match. Baseline 20.13s three-process observation: .122% of one CPU core, RSS sum 2.66 GiB. Ordinary user/background editing retained; not a controlled workload comparison.
- DB bounded journal implemented: 16,384-event retention plus at most 1,023 tail events, fixed scratch for OR REPLACE conflict IDs, same-transaction ordered IDs/final rows, cap/gap/identity fallbacks, canonical ID full-load order. Independent db/sqlite 20 checks PASS.
- Packed/query and server implementation still integrating; full crate checks and formal benchmarks NOT_RUN. No live schema migration or App replacement.
- Query benchmark must compare compact versus compact (both represent round2/round3 default memory service storage), never legacy Vec memory against packed.

## Correctness checkpoint

- Debug/Release: 99 library tests plus the fixed-seed 420-transaction integration property each PASS. All nine query results and complete FileRecords matched a fresh index from the same DB read snapshot after each transaction.
- Swift GUI regression checks, strict Clippy all-targets, format/diff and Python syntax checks PASS. Preliminary 89/90 fixture failure was corrected by reopening an external writer after a deliberate schema migration; raw failure retained.
- Independent cross-reviews found no query ranking, glob, incremental publication or journal correctness blocker. Review identified a kind-field temporary lowercase allocation outside the first payload accounting; add that field to the pre-copy budget before final artifact freeze.
- First A/B/A query set (pre-final kind guard) matched all ordered full-result digests; nine-query CPU median 5.723s -> 2.632s, -54.0%; peak RSS unchanged around 2.64 GiB. Preserve this run as preliminary, rerun against the exact final artifact.

## Final artifact checkpoint

- Kind parsing temporary allocation is now charged before row decoding. Final Debug/Release each 100 library tests plus 420-transaction property PASS; strict Clippy/format/Python PASS; GUI unchanged and full GUI checks PASS.
- Final library SHA256 2519fb96accc4a7050bd69c25572ba7c2472bd896db1544592c77f190a66f109; CLI SHA256 ebb04b77e54752b1b3ed49827face8434f08248a5d1583bb3337294b89e93a4b. Exact source hashes recorded with artifacts.
- Final same-input packed/packed A/B/A PASS, all complete ordered digests equal. Nine-query CPU 5.666 -> 2.650s (-53.2%; paired -53.30%); HWM RSS 2.638 -> 2.638 GiB (+0.01%). Short code query +13.9%, fuzzy roughly unchanged; do not claim universal speedup.
- HTTP update/fault and FSEvents final checks still pending. Native old App is accessible and remains running. No live migration/replace/round3 commit/push yet.

## Controlled acceptance checkpoint

- Same-input final A/B/A query PASS, full ordered fields and scores equal, all raw evidence preserved.
- 7,226,488-row prepared-clone HTTP A/B PASS. Three ordinary waves use incremental 1/2/3 with zero full reloads; wave1/2 CPU 4.570/4.578s -> .0207/.0237s. Explicit reload/event cap/gap/missing journal/read 503 recovery all PASS. HTTP40 each version 2 correct 200 + 38 busy503; all 200 complete JSON equal, controls200, cleanup PASS.
- Lifecycle HWM old2.611/new2.639 GiB (+1.1%); no claimed extra memory reduction. Fixed query HWM approximately unchanged.
- 300k FSEvents A/B/A nine lifecycle/ignore/recovery checks and final state PASS, watcher CPU .0204/.0227s vs .0221s. No material watcher CPU change claimed.
- Latest live read-only online backup in progress. Controlled validation complete; next source commit/push, clean candidate App build, signature/payload checks, replacement/native window acceptance and idle observation.
