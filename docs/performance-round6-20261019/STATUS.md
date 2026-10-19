# MacEvery Round 6 Status

Updated: 2026-10-19 (Asia/Shanghai). Local implementation, controlled verification, App installation, native acceptance and resource sampling complete.

## Task capsule

- Project/host: MacEvery, this Mac, existing main checkout at 26a7246; installed App source d701658, label 0.1.3-gd701658.
- Goal: lower index loading CPU and backend startup time, retain round-5 memory reduction, preserve short-query speed and all ordered result/update semantics.
- Authorized: focused source changes, isolated verification and benchmarks, verified App replacement, commit/push and exact remote readback.
- Forbidden: live index rebuild; roots/excludes/mode/shortcut/permission changes; unrelated services; real index or full result publication; version bump/tag.
- Raw evidence: ignored .build/performance-round6-20261019 and /private/tmp/macevery-round6-20261019. Previous evidence unchanged; rollback .build/MacEvery.rollback-20261019-before-round6.app.

## Plan

1. Freeze source/App and tested round-5 artifacts; verify remote main and unchanged 7,226,486-row snapshot SHA.
2. Remove duplicate string layout/prefix lookup and per-row decoding work; let watcher read roots/excludes without counting all files. Keep query algorithms and stored layout unchanged.
3. Full Debug/Release result/update tests, targeted boundary tests, format/strict Clippy and GUI checks.
4. Sequential original-nine, short-29 and status-to-ready startup A/B/A, HTTP delta/fallback/recovery and FSEvents. Preserve all runs and regressions.
5. Fresh live read-only backup, source commit/push/readback, clean App build and payload/signature identity; native search and create/modify/rename/delete acceptance, final resource observations, documentation commit/push/readback.

## Preset gates

- Zero complete ordered-result digest mismatches; full-field/score/order and index-update regressions pass.
- Short-suite and code paired CPU medians no more than +5%; investigate any common term with repeated two-block regression over +10% and +5ms. Add complete A/B/A blocks only to resolve an identified uncertainty, preserving earlier samples.
- Retained packed capacity no more than +1% of round-5 1,537,288,452 bytes. Same-input loaded RSS and lifetime RSS high-water must preserve the round-5 memory benefit; investigate increases over 5% before adoption.
- Combined database-open/index-load CPU and strict backend-ready startup must improve. Status CLI, service readiness, first correct query and actual App window acceptance are distinct evidence; a bound port or /health response alone is insufficient.
- No installation before controlled acceptance. Native acceptance and point-in-time CPU/RSS remain separate from controlled performance results.

## Ownership

- search_memory_audit: src/compact_index.rs and private focused tests, no query algorithm changes.
- watch_cpu_audit: src/model.rs, sqlite.rs, db.rs, watch.rs and focused tests.
- gui_cpu_audit: isolated tools/check_startup_round6.py and measurement audit.
- root: integration review, verification, sequential measurement, App/Git and records.

## Current evidence

- Baseline main26a7246 / installed d701658 frozen, hashes and snapshot verified. Exact capacity remains1,537,288,452B.
- Debug/Release123 unit tests each plus420-transaction/prefix-update integrations, strict Clippy/fmt/Python/GUI pass; independent source, mechanism, math and report reviews found no blocker.
- Original-nine and short29 two-block ABA complete, all full ordered digests match. Loading CPU median -9.76%/-11.88%; short group -2.39%, hot code -4.11%. Fresh code ordinary +1.54%, nine queries +2.46%, nohit and first-service-load +2.2% retained in REPORT r2.
- Startup initial6run wall mixed +1.765%/-5.344%; one whole follow-up ABA appended, no sample selection. All9 status/strictready/code300/count/hash/cleanup pass. Ready19.764->18.820s, CPU5.532->5.117s; paired wall -4.201% with positive block retained. Scope excludes GUI/window/watch.
- HTTP normal waves full0/delta1/2/3, busy40=2correct200+38busy503, all fallback/readfailure/recovery pass. 300k FSEvents ABA nine lifecycle checks and final states match.
- Source2f32302c640228dad9b3a2e4c111b3fff823450c pushed/read back exact main and clean. App0.1.3-g2f32302 clean build/signatures/payload equality, original-path replacement complete; rollback and original App kept. No source changes after controlled acceptance.
- Fresh source-mode=ro online backup7,310,058rows/quick_checkok/SHA dd4db6f36f1b059d71a57ad10752f399de722fd92721fa19e301d892a6c2ced8. Roots/excludes/database_id unchanged; no manual live rebuild/direct index edit.
- Old exact roles16389/16393/16394 exited. New55324/55336/55337 parentage/stability verified. Native18->35->beta-only->0, code300 PASS; fixture and query cleaned toNoQuery, Fastest/Command-Shift-Space/Limited preserved.
- Native baseline20.073s CPU0.21598%/RSSsum1.734GiB/revision+5; final20.067s CPU0.01830%/RSSsum1.754GiB/revision0. Point observations only, RSSsum+1.13%; retained alongside controlled results, no idle/long-term claim.
- Final readonly index7,311,144rows, database_id preserved,16,735 journal events<17,408 bound, fixture rows absent. Owned benchmark processes none. REPORT r2/evidence complete; final docs push receipt in ignored final-git-receipt.json.

## Next

本轮本机目标已完成；最终文档提交/推送的精确SHA、远端回读、ahead/behind和干净状态在忽略目录回执保存。App源码固定2f32302，不随文档提交重新构建。远端CI、正式QA、长时间后台和受保护目录扩展覆盖未执行。
