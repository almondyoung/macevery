# MacEvery Round 6 Status

Updated: 2026-10-19 (Asia/Shanghai). Controlled implementation and verification complete; installation/native acceptance pending.

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

- main and origin/main match 26a72463c4cf876cafb6e8db1b843aae723eb5d1 (ahead=behind=0), clean before edits.
- Round-5 release CLI/library reused after source and artifact hash verification; current installed App copied to named rollback.
- Frozen snapshot copied to this round's /private/tmp location; SHA 68bd22081af1eb5ec9164037407cb6c9c13349a7f8e460fa9aa682b3286b5b29 verified.
- Debug/Release 123 unit tests each plus 420-transaction and prefix-update integrations pass; format, strict all-target Clippy and GUI regression pass. Independent cross-review of compact and decoding/watch changes found no blocker.
- Exact eight-part retained capacity is unchanged at 1,537,288,452 bytes; string usage, prefix entries/capacity, metadata escapes and 56B records agree.
- Native old-App settings and No Query verified; 20.073s baseline CPU0.216% one-core sum, RSSsum1.734GiB, revision+5 (point observation, not idle benchmark).
- HTTP correctness/update/recovery, sequential performance, startup and FSEvents complete. No direct live index edit/rebuild or App replacement so far.

## Measurement follow-up

- Initial startup two-block A/B/A passed correctness and CPU; bootstrap-ready wall paired changes +1.765% and -5.344% differ in direction. One additional complete startup A/B/A block is scheduled to quantify this variability. Keep all first six runs; combine all nine without selecting samples. Other workloads do not require repetition.

## Controlled completion

- All prescribed checks, original-nine/short29 two-block ABA, strict startup nine-run aggregate, HTTP recovery and 300k FSEvents passed; performance-gates.json all_pass=true.
- Loading CPU median -9.76%/-11.88%; capacity unchanged, RSS effectively unchanged. Short29 CPU -2.39%, hot code -4.11%; fresh code ordinary +1.54% retained (paired -1.13%).
- Startup ready 19.764->18.820s, cumulative CPU5.532->5.117s; paired wall -4.201% with one +1.765% block retained. REPORT r1 explicitly limits startup evidence to status->serve and keeps first-service-load +2.2% CPU regression.
- Fresh live read-only backup complete; source commit/push, clean App and native acceptance follow.

- Fresh live source mode=ro online backup complete:7,310,058rows,quick_check=ok,SHA dd4db6f36f1b059d71a57ad10752f399de722fd92721fa19e301d892a6c2ced8; database_id/roots/excludes preserved. No live rebuild/direct SQL write.
