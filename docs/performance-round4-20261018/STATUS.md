# MacEvery Round 4 Status

Updated: 2026-10-18 (Asia/Shanghai). Discovery and implementation.

## Task capsule

- Project: MacEvery; baseline main4c17a03 / installed source d93f3f6, clean at start, equals origin/main.
- Goal: fix short literal queries such as code first, then reduce index memory; preserve full records/scores/order and transactional incremental freshness.
- Lanes: code, correctness, sequential benchmark, local App acceptance, report.
- Allowed: search/compact storage and focused validation tooling; source commit/normal push and verified local App replacement explicitly authorized.
- Forbidden: rebuild live index, roots/excludes/mode/shortcut/permission changes, other apps/services, release tags/patch bump, publication of real index/full query results.
- Host: this Mac only. Prior evidence and App/DB rollback retained.

## Plan

1. Freeze baseline source, App and existing 7,226,486-row frozen snapshot.
2. Isolate short-query overhead and memory composition; choose minimal compatible changes.
3. Full-field/ranking/Unicode/null/extreme/delta/recovery regression, Debug/Release, Clippy and GUI checks.
4. Serial same-input packed/packed A/B/A: original nine queries plus a general short-word suite; CPU, load/end/peak RSS and complete result digests. HTTP incremental/fallback and FSEvents checks.
5. Preserve latest live DB backup, commit/push tested source, clean App build/signature/payload verification, replace exact old processes; native create/modify/rename/delete, idle observation, final docs commit/push/readback.

## Ownership

- search_memory_audit: src/search.rs (short matcher/scorer).
- watch_cpu_audit: src/compact_index.rs (memory layout and incremental compatibility).
- gui_cpu_audit: focused tools only, independent query/memory verification design.
- root: orchestration, baseline/final artifacts, benchmarks, remaining files, App, reports and Git.

## Evidence

- Raw ignored: .build/performance-round4-20261018; temp /private/tmp/macevery-round4-20261018.
- Baseline archive: raw/baseline-src; release build in progress.
- App rollback: .build/MacEvery.rollback-20261018-before-round4.app, exact payload hashes recorded in baseline.json.
- Same frozen snapshot as rounds1–3: .build/performance-round1-20261017/snapshot.sqlite, SHA68bd22081af1eb5ec9164037407cb6c9c13349a7f8e460fa9aa682b3286b5b29.
- Rechecked old App0.1.3-gd93f3f6, GUI/service/watch76429/76436/76437. Recheck again before mutation; these PIDs are not enduring authority.

## Next

Search-only candidate frozen; same-layout code A/B/A in progress. Adaptive numeric metadata layout implementing separately. No live DB migration or App replacement.

## Design checkpoint

- Short matcher: one ASCII byte uses standard char search; 2..7 UTF-8 bytes cache first/last anchors and use safe word filtering with full validation; long BMH/KMP unchanged. Search20/20 and strict targeted Clippy PASS, exact source SHA cd46a2352198204be84b0c88755562eebec5c81f0887bee25efa7479a3fbcbe4. Separate baseline-compact/short-only source and artifact isolate CPU effect before combining memory changes.
- Memory candidate: hot56B record plus six per-block numeric columns; narrow u32 differences/checked i64 escapes, automatically Wide when compressed metadata reaches raw-size budget. All values, null bits and stable ID order retained; no global ID/value HashMap. Reuse escaped slots; one-way Wide prevents unbounded repeated-update growth. Metadata-only updates never touch string arenas; exceptional numeric updates may allocate bounded cold metadata.
- Current live vmmap: MALLOC_LARGE2.4G resident/dirty, approximately1% fragmentation, no swap. First text-query RSS jump is not yet attributed; do not call it a leak or pure query allocation. CPU/RSS comparison uses fixed complete workloads.
- Baseline20.08s point observation CPU .108% onecore/RSSsum2.609GiB; ordinary user background retained, not strict idle A/B.

## Candidate checkpoint

- Code-only real7.226m two-block A/B/A PASS: all full ordered records equal, CPU median378.88 ->190.03ms (-49.8%, paired-52.6%); one478ms baseline sample retained. This isolates matcher change with unchanged layout.
- Combined candidate Debug/Release107/107 +420-transaction integration property +GUI +strictClippy/format/Python PASS. Separate cross-review in progress.
- Real candidate-only layout diagnostics: all111 blocks narrow,0wide,157914 live escapes; hot407371776B +metadata176194432B +strings1744830464B +other188446B =2328585118B total capacity. Baseline roughly2501551kB; formal paired CPU/RSS still pending.
- New short29-query wrapper synthetic AB, load-only and reverse-selection smoke PASS, all result digests equal; compatibility calls public API shared by both versions. Same-input formal short/group measurements NOT_RUN.
- Service validation tool now detects incremental baseline diagnostics, so round3/round4 both assert delta publication, stale observation and recovery; small synthetic smoke in progress.

## Final nine-query checkpoint

- Full combined two-block real packed/packed A/B/A PASS, every full ordered result digest equal. code383.59->196.61ms (-48.7%), group2.611->1.513s (-42.0%; paired-42.12%).
- Load RSS1.825->1.689GiB (-7.4%); lifetime HWM2.638->2.502GiB (-5.1%). Load CPU4.624->4.663s (+.8%).
- Retain regressions: nohit+4.5%, filters+8.8%, mtime+33.6%; cold decoding trades memory for some metadata-only CPU. Default short29 suite now running sequentially; native App unchanged.

## Short-suite checkpoint

- Final29-query true-data two-block A/B/A PASS, all full ordered result hashes equal. GroupCPU10.557->5.959s (-43.6%; paired-43.52%). ASCII2..7 all18terms improve32.0..60.3%; Unicode6terms improve27.9..45.8%. Single a/e +1.6/+3.0%, z-43.2%; warm broad/nohit+10/+7%; no universal speedup claim.
- Same HWM~2.638->2.502GiB (-5.1%) and loadRSS1.825->1.689GiB (-7.4%). Packed capacity2.502GB->2.329GB (-6.9% decimal), distinct from RSS.
- RealHTTP A/B running, FSEvents next. Mac is locked; native acceptance requires user unlock, async question sent while independent work continues. App remains original.

## Controlled verification complete

- RealHTTP both incremental versions PASS: full result JSON equal; normal wavesfull=0/delta1,2,3, all fallback/recovery checks. HTTP40 each2 correct200/38busy503; controls200 and cleanup PASS.
- Pure metadata first query CPU20.61/21.72ms ->25.93/25.41ms (+4..5ms); do not claim further delta CPU acceleration. Lifecycle HWM2.639->2.503GiB (-5.1%).
- 300k FSEvents A/B/A nine checks/finalstate PASS; CPU baseline.0198/.0187s candidate.0205s. No watcher CPU gain claimed.
- Raw evidence archived and sanitized summaries/links checked; frozen source hashes still match. Latest live DB backup and fetch in progress; source commit/push then clean App candidate build. Native acceptance requires pending manual unlock; do not mark it PASS before visible test.
