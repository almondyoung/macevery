# MacEvery Round 7 Status

Updated: 2026-10-19 (Asia/Shanghai). Controlled and native functional acceptance PASS; installed source8096e0a. Final docs/Git receipt is stored locally.

## Task capsule

- Project/host: MacEvery on this Mac; baseline main a613164, initially installed source2f32302 /0.1.3-g2f32302; current installed source8096e0a.
- Goal: shorter startup and lower retained memory; preserve short-query CPU, full result fields/score/order and index updates.
- Authorized: focused source changes, isolated verification/measurement, verified App replacement, commit/push and exact readback.
- Forbidden: manual live index rebuild/direct SQL edits; roots/excludes/mode/shortcut/permission changes; unrelated services; real index/full result publication; version bump/tag.
- Evidence: ignored .build/performance-round7-20261019 and /private/tmp/macevery-round7-20261019; previous rounds unchanged. Rollback .build/MacEvery.rollback-20261019-before-round7.app.

## Plan

1. Freeze source, tested baseline artifacts, App identities/rollback and unchanged 7,226,486-row snapshot.
2. Reduce hot records56B->48B by deriving basename starts from existing suffix-layout parents; keep complete lengths and contiguous search strings.
3. Add lightweight transactional bootstrap; launch service/watch first, defer exact full status until strict readiness (bounded failure fallback). Preserve four empty/root cases and current query/child lifecycle rules.
4. Full Debug/Release, directed layout/bootstrap/GUI regressions, strict lint/format; sequential original9/short29/startup ABA, HTTP recovery and FSEvents.
5. Fresh live read-only backup, source commit/push/readback, clean App/signature/payload identity, original-path replacement, native search/lifecycle and final resources, docs commit/push/readback.

## Preset gates

- Zero complete ordered-result digest mismatches and passing full-field/score/order/update regressions.
- Short suite and fresh/hot code paired CPU medians <=+5%; investigate any term repeating >+10% and >+5ms in both blocks. Preserve every sample; add whole ABA blocks only to resolve identified variability.
- Packed capacity must decrease from1,537,288,452B. Same-input loaded and lifetime peak RSS must preserve/improve memory benefit; investigate increases >1% before adoption.
- State-read-to-strict-ready startup wall must improve; disclose every included process, critical path and full-count completion. Parallel counting is moved work, not eliminated work. CPU aggregate includes bootstrap/full status/service, not just serve.
- Strictready requires ready=true, stale=false and first correct ordered search, not bind/health alone. Keep GUI/native observations separate from isolated backend metrics.
- No App install before controlled acceptance. Preserve actual settings, index identity, rollback and all regressions.

## Ownership

- search_memory_audit: compact_index.rs and private layout tests.
- watch_cpu_audit: db/model/cli lightweight bootstrap and Rust tests.
- gui_cpu_audit: Swift bootstrap/deferred counts, relevant GUI guards/tests; no query fallback redesign this round.
- root: integration review, startup driver, sequential verification/measurement, App/Git/report.

## Current evidence

- Baseline main/origin match a613164 (ahead=behind=0); initial tree clean. Round6 release artifacts reused after complete source/binary hash checks.
- Initially installed Appsource2f32302, exact GUI/serve/watch identified and rollback copied. Snapshot SHA68bd22081af1eb5ec9164037407cb6c9c13349a7f8e460fa9aa682b3286b5b29 verified.
- Debug/Release each129 unit tests plus3 bootstrap CLI and differential/prefix integration tests PASS; strict all-target Clippy, fmt, Release build PASS. GUI old/new regressions PASS, including metadata snapshots returning after a rebuild completes. No residual GUI regression/fake-CLI/compiler processes.
- 48B records: exact capacity1,537,288,452B ->1,479,092,484B (-58,195,968B); records7,226,486 and other capacities unchanged.
- All source/artifact hashes frozen. Sequential HTTP update/recovery, original9/short29/final startup ABA and FSEvents completed PASS; no compilation/heavy profile in parallel.
- Installed source8096e0a after controlled gates. No manual rebuild/direct SQL edits or system permission changes. Native functional acceptance PASS.

## Preserved first-attempt outcome

- Original9 and short29 ordered digests PASS; fresh/hot code paired+3.502%/+1.629%, group+0.330%; loaded RSS about-4.5%, HWM about-3.3%. HTTP and FSEvents PASS. Rust/artifacts unchanged.
- Startup initial2ABA plus1 whole confirm block (all9 kept): ready wall paired+4.374%/-10.397%/+0.866%, median+0.866% FAIL. No installation/commit performed.
- Adjust only GUI/driver count scheduling: strict ready first, then background full status. Full status completion and complete CPU remain measured; no repeated sampling of the rejected design. Final GUI regressions and separate deferred-count startup acceptance PASS, as recorded below.

## Final controlled acceptance

- Final GUI deferred-count compiler/full regressions PASS; fixed60s deadline, strict ready/stale, explicit-refresh single consumption, old-instance callback rejection and bounded failure fallback verified. All GUI test processes exited; independent source review PASS.
- Rust sources/binaries unchanged after count-scheduling revision; prior query/HTTP/FSEvents/capacity evidence remains applicable. Final source/artifact manifest matches.
- Final deferred-count startup two ABA (6 runs), separate from rejected9: all exact counts/ordered first-code digests PASS; ready wall blocks-22.378%/-27.716%, paired median-25.047%. Ordinary median18.529s->13.662s. Full-work completion18.755s->14.978s; complete lifecycleCPU5.703s->5.628s (-1.314%, paired-2.995%). Full-count completion now14.978s versus2.091s, explicitly delayed work.
- All preset performance gates PASS. Fresh read-only live backup PASS:7,312,631 rows, quick_check=ok, SHA8e4b2d3b2cd09052f67be1ab69684cf4390a4342614492ba028712a2cc1194e8. Source8096e0a pushed/read back; clean App0.1.3-g8096e0a installed, native functional acceptance and final20s resources complete.

## Installed and native acceptance

- Source8096e0a9c61cacd3402cd3017e95fc60ddb2c0f3 pushed to origin/main and exact readback/ancestry/clean build PASS. Candidate/installed deep-strict signatures and normalized tested CLI payload PASS. Original App/initial rollback preserved; old exact GUI/serve/watch all exited before replacement.
- CurrentGUI/serve/watch97195/97199/97200 have correct parentage and stable identities. Native18B create ->35B modify ->beta-only rename ->0 delete pluscode300 PASS, each fixture step independently checked through exact read-only SQL. Fixture fully removed; query cleared/No Query. Mode/shortcut/config/DB ID retained.
- Permission indicator actually Limited ->Unknown, stillUnknown afterCheck; detector source unchanged, no permission grant/change performed. Protected-folder coverage remains unverified.
- Final readonly index7,315,500 rows; journal16,926<17,408; roots/excludes/DBID match backup; no owned test processes remain. All source and installed binary hashes unchanged.
- Two20s points: oldCPU0.451237%, RSS1.740GiB with+580 revisions; newCPU0.017421%, RSS1.703GiB with0 revisions. RSSsum-2.127%; background/history/live data differ, no stable-idle/long-leak claim. Controlled RSS-4.5%/HWM-3.3% remains primary.
- Report r2 and sanitized evidence complete. Documentation-only final commit must preserve installed source8096e0a; exact final remote/clean/ancestor receipt saved under ignored final-git-receipt.json. No App rebuild for documentation. RemoteCI/formalQA/long soak/protected expansion NOT_RUN.
